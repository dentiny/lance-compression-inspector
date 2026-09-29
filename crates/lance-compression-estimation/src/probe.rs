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
use lance::dataset::builder::DatasetBuilder;
use lance_file::reader::{FileReader, describe_encoding};
use lance_io::{
    object_store::ObjectStore,
    scheduler::{ScanScheduler, SchedulerConfig},
    utils::CachedFileSize,
};
use object_store::path::Path;

use crate::{ColumnProfile, DatasetProbeReport, EncodingTag, ProbeReport, REPORT_SCHEMA_VERSION};

/// Resolve a local dataset snapshot from its manifest and probe every active
/// Lance data file. `version=None` selects the latest version on `branch`.
pub async fn probe_local_dataset(
    source: impl AsRef<FsPath>,
    branch: &str,
    version: Option<u64>,
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

    Ok(DatasetProbeReport {
        schema_version: REPORT_SCHEMA_VERSION,
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

/// Probe a local standalone Lance data file without decoding column values.
pub async fn probe_local_file(source: impl AsRef<FsPath>) -> Result<ProbeReport> {
    let source = source.as_ref();
    let canonical = source
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", source.display()))?;
    let object_path = Path::from_filesystem_path(&canonical)
        .map_err(|error| anyhow::anyhow!("invalid local path {}: {error}", canonical.display()))?;

    let store = Arc::new(ObjectStore::local());
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
                raw_page_encodings,
            }
        })
        .collect();

    Ok(ProbeReport {
        schema_version: REPORT_SCHEMA_VERSION,
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
}
