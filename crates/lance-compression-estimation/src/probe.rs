//! Footer-only probing for standalone Lance data files.
//!
//! Every raw page encoding description is retained so a newer Lance encoding is
//! never silently discarded. Normalized tags are additive and power the
//! estimator's current capability matrix.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path as FsPath,
    sync::Arc,
};

use anyhow::{Context, Result};
use arrow_array::{RecordBatch, RecordBatchIterator};
use arrow_schema::{Field as ArrowField, Schema as ArrowSchema};
use futures::{TryStreamExt, future::try_join_all};
use lance::{Dataset, dataset::builder::DatasetBuilder};
use lance_file::reader::{FileReader, describe_encoding};
use lance_io::{
    object_store::ObjectStore,
    scheduler::{ScanScheduler, SchedulerConfig},
    utils::CachedFileSize,
};

use crate::{
    ColumnProfile, CompressionAlgorithm, CompressionMeasurement, DEFAULT_ENCODING_CANDIDATES,
    DatasetProbeReport, EncodingCandidate, EncodingTag, ProbeReport,
};

/// Resolve a local dataset snapshot from its manifest and probe every active
/// Lance data file. `version=None` selects the latest version on `branch`.
pub async fn probe_local_dataset(
    source: impl AsRef<FsPath>,
    branch: &str,
    version: Option<u64>,
    sample_rows: usize,
) -> Result<DatasetProbeReport> {
    let source = source.as_ref();
    let canonical = source
        .canonicalize()
        .with_context(|| format!("cannot resolve dataset {}", source.display()))?;
    let uri = canonical
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("dataset path is not valid UTF-8"))?;
    let dataset = DatasetBuilder::from_uri(uri)
        .with_branch(branch, version)
        .load()
        .await
        .with_context(|| {
            let version = version.map_or_else(|| "latest".into(), |value| value.to_string());
            format!(
                "cannot open Lance dataset {} at branch {branch}, version {version}",
                canonical.display()
            )
        })?;

    let fragments = dataset.iter_fragments().collect::<Vec<_>>();
    let physical_rows = fragments
        .iter()
        .filter_map(|fragment| fragment.physical_rows)
        .map(|rows| rows as u64)
        .sum();
    let mut relative_paths = BTreeSet::new();
    for fragment in &fragments {
        for data_file in fragment.referenced_lance_files() {
            if data_file.base_id.is_some() {
                anyhow::bail!(
                    "external data file base paths are not supported yet: {}",
                    data_file.path
                );
            }
            relative_paths.insert(data_file.path.clone());
        }
    }

    let mut files = Vec::with_capacity(relative_paths.len());
    for relative_path in relative_paths {
        let root_relative = canonical.join(&relative_path);
        let data_path = if root_relative.exists() {
            root_relative
        } else {
            canonical.join("data").join(relative_path)
        };
        files.push(probe_local_file(data_path).await?);
    }

    if sample_rows > 0 {
        attach_sample_measurements(&dataset, sample_rows, &mut files).await?;
    }

    Ok(DatasetProbeReport {
        source: canonical.display().to_string(),
        branch: dataset
            .manifest
            .branch
            .clone()
            .unwrap_or_else(|| "main".into()),
        manifest_version: dataset.manifest.version,
        fragment_count: fragments.len(),
        physical_rows,
        files,
    })
}

async fn attach_sample_measurements(
    dataset: &Dataset,
    sample_rows: usize,
    files: &mut [ProbeReport],
) -> Result<()> {
    let mut candidates = DEFAULT_ENCODING_CANDIDATES.to_vec();
    for candidate in files
        .iter()
        .flat_map(|file| &file.columns)
        .flat_map(|column| &column.observed_compressions)
    {
        if !candidates.contains(candidate) {
            candidates.push(*candidate);
        }
    }

    // Sample each fragment independently so one fragment's distribution is
    // never used to recommend settings for another fragment's physical files.
    for fragment in dataset.iter_fragments() {
        let mut scanner = dataset.scan();
        scanner.with_fragments(vec![fragment.clone()]);
        scanner.limit(Some(sample_rows as i64), None)?;
        let batches = scanner
            .try_into_stream()
            .await?
            .try_collect::<Vec<_>>()
            .await?;
        if batches.is_empty() {
            continue;
        }
        let measured_rows = batches.iter().map(RecordBatch::num_rows).sum::<usize>() as u64;

        // Each candidate writes its own temporary Lance dataset. Running these
        // futures together parallelizes encoding of the same fragment sample.
        let measurements = try_join_all(
            candidates
                .iter()
                .copied()
                .map(|candidate| measure_candidate(batches.clone(), candidate, measured_rows)),
        )
        .await?;
        let fragment_paths = fragment
            .referenced_lance_files()
            .map(|data_file| data_file.path.as_str())
            .collect::<Vec<_>>();

        for file in files.iter_mut().filter(|file| {
            fragment_paths
                .iter()
                .any(|path| file.source.ends_with(path))
        }) {
            for column in &mut file.columns {
                column.compression_measurements = measurements
                    .iter()
                    .filter_map(|measurement| {
                        measurement
                            .column_bytes
                            .get(&column.path)
                            .map(|encoded_bytes| CompressionMeasurement {
                                candidate: measurement.candidate,
                                sample_rows: measurement.sample_rows,
                                encoded_bytes: *encoded_bytes,
                            })
                    })
                    .collect();
            }
        }
    }
    Ok(())
}

struct CandidateMeasurement {
    candidate: EncodingCandidate,
    sample_rows: u64,
    column_bytes: BTreeMap<String, u64>,
}

async fn measure_candidate(
    batches: Vec<RecordBatch>,
    candidate: EncodingCandidate,
    sample_rows: u64,
) -> Result<CandidateMeasurement> {
    let (schema, batches) = with_candidate_schema(batches, candidate)?;
    let reader = RecordBatchIterator::new(batches.into_iter().map(Ok), schema);
    let temp_dir = tempfile::tempdir()?;
    let dataset_path = temp_dir.path().join("sample.lance");
    let uri = dataset_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("temporary sample path is not valid UTF-8"))?;
    let dataset = Dataset::write(reader, uri, None).await?;

    let data_file = dataset
        .iter_fragments()
        .flat_map(|fragment| fragment.referenced_lance_files())
        .next()
        .ok_or_else(|| anyhow::anyhow!("sample rewrite produced no data file"))?;
    let root_relative = dataset_path.join(&data_file.path);
    let data_path = if root_relative.exists() {
        root_relative
    } else {
        dataset_path.join("data").join(&data_file.path)
    };
    let report = probe_local_file(data_path).await?;
    Ok(CandidateMeasurement {
        candidate,
        sample_rows,
        column_bytes: report
            .columns
            .into_iter()
            .map(|column| (column.path, column.on_disk_bytes))
            .collect(),
    })
}

fn with_candidate_schema(
    batches: Vec<RecordBatch>,
    candidate: EncodingCandidate,
) -> Result<(Arc<ArrowSchema>, Vec<RecordBatch>)> {
    let source_schema = batches[0].schema();
    let fields = source_schema
        .fields()
        .iter()
        .map(|field| Arc::new(with_candidate_field(field, candidate)))
        .collect::<Vec<_>>();
    let schema = Arc::new(ArrowSchema::new_with_metadata(
        fields,
        source_schema.metadata().clone(),
    ));
    let batches = batches
        .into_iter()
        .map(|batch| {
            RecordBatch::try_new(schema.clone(), batch.columns().to_vec())
                .map_err(anyhow::Error::from)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((schema, batches))
}

fn with_candidate_field(field: &ArrowField, candidate: EncodingCandidate) -> ArrowField {
    let mut metadata = field.metadata().clone();
    let scheme = match candidate.algorithm {
        CompressionAlgorithm::Uncompressed => "none",
        CompressionAlgorithm::Lz4 => "lz4",
        CompressionAlgorithm::Zstd => "zstd",
    };
    metadata.insert("lance-encoding:compression".into(), scheme.into());
    if let Some(level) = candidate.level {
        metadata.insert("lance-encoding:compression-level".into(), level.to_string());
    } else {
        metadata.remove("lance-encoding:compression-level");
    }
    field.clone().with_metadata(metadata)
}

/// Probe a local standalone Lance data file without decoding column values.
pub async fn probe_local_file(source: impl AsRef<FsPath>) -> Result<ProbeReport> {
    let source = source.as_ref();
    let canonical = source
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", source.display()))?;
    let uri = canonical
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("file path is not valid UTF-8"))?;
    let (store, object_path) = ObjectStore::from_uri(uri).await?;
    let scheduler = ScanScheduler::new(store, SchedulerConfig::new(256 * 1024 * 1024));
    let file_scheduler = scheduler
        .open_file(&object_path, &CachedFileSize::unknown())
        .await
        .with_context(|| format!("cannot open Lance file {}", canonical.display()))?;
    let metadata = FileReader::read_all_metadata(&file_scheduler)
        .await
        .with_context(|| format!("cannot read Lance metadata from {}", canonical.display()))?;

    let leaf_fields = leaf_fields_with_paths(&metadata.file_schema);
    let columns = metadata
        .column_metadatas
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let field = leaf_fields.get(index);
            let mut raw_page_encodings = column
                .pages
                .iter()
                .map(describe_encoding)
                .collect::<Vec<_>>();
            raw_page_encodings.sort();
            raw_page_encodings.dedup();

            let mut encoding_tags = raw_page_encodings
                .iter()
                .flat_map(|description| classify_encoding(description))
                .collect::<BTreeSet<_>>();
            if encoding_tags.is_empty() {
                encoding_tags.insert(EncodingTag::Unknown);
            }
            let observed_compressions = raw_page_encodings
                .iter()
                .flat_map(|description| classify_general_compressions(description))
                .collect();

            ColumnProfile {
                index,
                path: field
                    .map(|(path, _)| path.clone())
                    .unwrap_or_else(|| format!("physical_column_{index}")),
                data_type: field
                    .map(|(_, field)| field.data_type().to_string())
                    .unwrap_or_else(|| "unknown".into()),
                pages: column.pages.len(),
                on_disk_bytes: column
                    .pages
                    .iter()
                    .flat_map(|page| page.buffer_sizes.iter())
                    .sum(),
                field_metadata: field
                    .map(|(_, field)| field.metadata.clone().into_iter().collect())
                    .unwrap_or_else(BTreeMap::new),
                encoding_tags,
                observed_compressions,
                compression_measurements: vec![],
                raw_page_encodings,
            }
        })
        .collect();

    Ok(ProbeReport {
        source: canonical.display().to_string(),
        file_version: metadata.version.to_string(),
        file_size_bytes: metadata.file_size_bytes,
        data_bytes: metadata.num_data_bytes,
        rows: metadata.num_rows,
        columns,
    })
}

fn leaf_fields_with_paths(
    schema: &lance_core::datatypes::Schema,
) -> Vec<(String, &lance_core::datatypes::Field)> {
    fn visit<'a>(
        field: &'a lance_core::datatypes::Field,
        parent: Option<&str>,
        output: &mut Vec<(String, &'a lance_core::datatypes::Field)>,
    ) {
        let path = parent.map_or_else(
            || field.name.clone(),
            |parent| format!("{parent}.{}", field.name),
        );
        if field.children.is_empty() {
            output.push((path, field));
        } else {
            for child in &field.children {
                visit(child, Some(&path), output);
            }
        }
    }

    let mut output = Vec::new();
    for field in &schema.fields {
        visit(field, None, &mut output);
    }
    output
}

/// Classify all currently known Lance encoding families while keeping the raw
/// description in the report for forward compatibility.
pub fn classify_encoding(description: &str) -> BTreeSet<EncodingTag> {
    let lower = description.to_ascii_lowercase();
    let mut tags = BTreeSet::new();

    let mappings = [
        ("miniblock", EncodingTag::StructuralMiniBlock),
        ("fullzip", EncodingTag::StructuralFullZip),
        ("sparselayout", EncodingTag::StructuralSparse),
        ("bloblayout", EncodingTag::StructuralBlob),
        ("nullable", EncodingTag::Nullable),
        ("fixedsizelist", EncodingTag::FixedSizeList),
        ("fixed_size_list", EncodingTag::FixedSizeList),
        ("fixedsizebinary", EncodingTag::FixedSizeBinary),
        ("fixed_size_binary", EncodingTag::FixedSizeBinary),
        ("packedstruct", EncodingTag::PackedStruct),
        ("packed_struct", EncodingTag::PackedStruct),
        ("byte_stream_split", EncodingTag::ByteStreamSplit),
        ("bytestreamsplit", EncodingTag::ByteStreamSplit),
        ("bitpack", EncodingTag::BitPacked),
        ("flat", EncodingTag::Flat),
        ("rle", EncodingTag::Rle),
        ("runlength", EncodingTag::Rle),
        ("delta", EncodingTag::Delta),
        ("fsst", EncodingTag::Fsst),
        ("constant", EncodingTag::Constant),
        ("variable", EncodingTag::VariableWidth),
        ("indirectencoding", EncodingTag::Indirect),
    ];
    for (needle, tag) in mappings {
        if lower.contains(needle) {
            tags.insert(tag);
        }
    }

    // Avoid matching metadata field names such as `dictionary: None` and
    // `value_compression`; only tag actual debug enum variants.
    if variant_present(&lower, "dictionary") {
        tags.insert(EncodingTag::Dictionary);
    }
    if variant_present(&lower, "binary")
        && !variant_present(&lower, "fixedsizebinary")
        && !variant_present(&lower, "fixed_size_binary")
    {
        tags.insert(EncodingTag::Binary);
    }
    if variant_present(&lower, "list")
        && !variant_present(&lower, "fixedsizelist")
        && !variant_present(&lower, "fixed_size_list")
    {
        tags.insert(EncodingTag::List);
    }
    if variant_present(&lower, "simplestruct") {
        tags.insert(EncodingTag::Struct);
    }
    if variant_present(&lower, "block") && !lower.contains("miniblock") {
        tags.insert(EncodingTag::Block);
    }

    if lower.contains("zstd") {
        tags.insert(EncodingTag::GeneralZstd);
    }
    if lower.contains("lz4") {
        tags.insert(EncodingTag::GeneralLz4);
    }
    if lower.contains("compression: none")
        || lower.contains("compression: \"none\"")
        || lower.contains("compression_algorithm_unspecified")
        || lower.contains("noencodingdescription")
    {
        tags.insert(EncodingTag::GeneralUncompressed);
    }

    if tags == BTreeSet::from([EncodingTag::Indirect])
        || lower.contains("unrecognized(type_url=")
        || lower.contains("unsupported(decode_err=")
        || lower.contains("missing")
    {
        tags.insert(EncodingTag::Unknown);
    }
    if !tags.contains(&EncodingTag::GeneralZstd)
        && !tags.contains(&EncodingTag::GeneralLz4)
        && !tags.contains(&EncodingTag::Unknown)
    {
        // Lance's only general codecs are ZSTD and LZ4. Their absence means the
        // default (general compression disabled), even when protobuf omits the
        // optional compression field.
        tags.insert(EncodingTag::GeneralUncompressed);
    }
    tags
}

/// Extract the concrete general-compression scheme and level from page
/// encoding debug descriptions emitted by Lance.
pub fn classify_general_compressions(description: &str) -> BTreeSet<EncodingCandidate> {
    description
        .to_ascii_lowercase()
        .split("scheme:")
        .skip(1)
        .filter_map(|suffix| {
            let details = &suffix[..suffix.len().min(192)];
            let algorithm = if details.contains("compressionalgorithmzstd")
                || details.trim_start().starts_with("\"zstd\"")
                || details.trim_start().starts_with("zstd")
            {
                CompressionAlgorithm::Zstd
            } else if details.contains("compressionalgorithmlz4")
                || details.trim_start().starts_with("\"lz4\"")
                || details.trim_start().starts_with("lz4")
            {
                CompressionAlgorithm::Lz4
            } else {
                return None;
            };
            Some(EncodingCandidate {
                algorithm,
                level: parse_debug_level(details),
            })
        })
        .collect()
}

fn parse_debug_level(description: &str) -> Option<i32> {
    let value = description.split_once("level: some(")?.1;
    let value = value.split_once(')')?.0.trim();
    value.parse().ok()
}

fn variant_present(description: &str, name: &str) -> bool {
    description.contains(&format!("{name}(")) || description.contains(&format!("{name} {{"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_multiple_layers_without_losing_composition() {
        let tags = classify_encoding(
            "MiniBlockLayout(Binary { values: RLE, compression: Zstd, bss: ByteStreamSplit })",
        );
        assert!(tags.contains(&EncodingTag::StructuralMiniBlock));
        assert!(tags.contains(&EncodingTag::Binary));
        assert!(tags.contains(&EncodingTag::Rle));
        assert!(tags.contains(&EncodingTag::GeneralZstd));
        assert!(tags.contains(&EncodingTag::ByteStreamSplit));
    }

    #[test]
    fn marks_future_encoding_as_unknown() {
        let tags = classify_encoding("Unrecognized(type_url=/lance.encodings.Future)");
        assert_eq!(tags, BTreeSet::from([EncodingTag::Unknown]));
    }

    #[test]
    fn recognizes_default_general_compression_as_disabled() {
        let tags = classify_encoding("Flat { compression: None }");
        assert!(tags.contains(&EncodingTag::Flat));
        assert!(tags.contains(&EncodingTag::GeneralUncompressed));
    }

    #[test]
    fn does_not_treat_absent_dictionary_field_as_dictionary_encoding() {
        let tags = classify_encoding(
            "MiniBlockLayout { value_compression: Some(Variable { values: None }), dictionary: None }",
        );
        assert!(!tags.contains(&EncodingTag::Dictionary));
        assert!(!tags.contains(&EncodingTag::Block));
        assert!(!tags.contains(&EncodingTag::Value));
    }

    #[test]
    fn extracts_general_compression_levels() {
        let encodings = classify_general_compressions(
            "General { compression: BufferCompression { scheme: CompressionAlgorithmZstd, level: Some(6) } } \
             General { compression: Compression { scheme: \"lz4\", level: None } }",
        );
        assert_eq!(
            encodings,
            BTreeSet::from([
                EncodingCandidate {
                    algorithm: CompressionAlgorithm::Lz4,
                    level: None,
                },
                EncodingCandidate {
                    algorithm: CompressionAlgorithm::Zstd,
                    level: Some(6),
                },
            ])
        );
    }
}
