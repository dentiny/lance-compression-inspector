//! Footer-only probing for standalone Lance data files.
//!
//! Every raw page encoding description is retained so a newer Lance encoding is
//! never silently discarded. Normalized tags are additive and power the
//! estimator's current capability matrix.

use std::{collections::BTreeSet, path::Path as FsPath, sync::Arc};

use anyhow::{Context, Result};
use arrow_array::{RecordBatch, RecordBatchIterator, UInt32Array};
use arrow_schema::{DataType, Field as ArrowField, Schema as ArrowSchema};
use futures::{TryStreamExt, future::try_join_all};
use lance::{
    Dataset,
    dataset::{WriteParams, builder::DatasetBuilder},
};
use lance_core::cache::LanceCache;
use lance_encoding::decoder::{DecoderPlugins, FilterExpression};
use lance_file::reader::{FileReader, FileReaderOptions, describe_encoding};
use lance_file::version::{ConcreteFileVersion, LanceFileVersion};
use lance_io::{
    ReadBatchParams,
    object_store::ObjectStore,
    scheduler::{ScanScheduler, SchedulerConfig},
    utils::CachedFileSize,
};
use rand::seq::index;

use crate::{
    ColumnProfile, DatasetProbeReport, EncodingFileVersion, EncodingMeasurement, EncodingPlan,
    EncodingTag, GeneralCompression, ProbeReport, StructuralEncoding, ValueEncoding, encoding_tags,
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
        let mut report = probe_local_file(&data_path).await?;
        if sample_rows > 0 {
            attach_data_file_measurements(&data_path, sample_rows, &mut report).await?;
        }
        files.push(report);
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

async fn attach_data_file_measurements(
    data_path: &FsPath,
    sample_rows: usize,
    report: &mut ProbeReport,
) -> Result<()> {
    let source_plan_version = report.file_version;
    if source_plan_version == EncodingFileVersion::V1 {
        return Ok(());
    }
    let plan_version = match source_plan_version {
        EncodingFileVersion::V1 | EncodingFileVersion::V2_0 | EncodingFileVersion::V2_1 => {
            EncodingFileVersion::V2_2
        }
        version => version,
    };
    let batches = sample_data_file(data_path, sample_rows).await?;
    if batches.is_empty() {
        return Ok(());
    }
    let measured_rows = batches.iter().map(RecordBatch::num_rows).sum::<usize>() as u64;
    let schema = batches[0].schema();

    // The bounded policy runs one axis sweep, keeps the best structural and
    // value plan, and then combines only that small beam with representative
    // general compressors. Rewrites within each stage remain parallel.
    for field in schema.fields() {
        let path = field.name().to_string();
        let mut stage_one = candidate_plans_for_type(field.data_type(), plan_version);
        for baseline_version in [source_plan_version, EncodingFileVersion::V2_3] {
            let baseline = EncodingPlan::baseline(baseline_version);
            if !stage_one.contains(&baseline) {
                stage_one.push(baseline);
            }
        }
        let mut measurements = try_join_all(stage_one.iter().copied().map(|plan| {
            measure_candidate(
                batches.clone(),
                path.clone(),
                plan,
                measured_rows,
                file_version_for_plan(plan),
            )
        }))
        .await?;

        let stage_two = combined_candidate_plans(&measurements);
        let measured_plans = measurements
            .iter()
            .map(|measurement| measurement.plan)
            .collect::<BTreeSet<_>>();
        measurements.extend(
            try_join_all(
                stage_two
                    .into_iter()
                    .filter(|plan| !measured_plans.contains(plan))
                    .map(|plan| {
                        measure_candidate(
                            batches.clone(),
                            path.clone(),
                            plan,
                            measured_rows,
                            file_version_for_plan(plan),
                        )
                    }),
            )
            .await?,
        );
        if let Some(column) = report.columns.iter_mut().find(|column| column.path == path) {
            column.encoding_measurements = measurements;
        }
    }
    Ok(())
}

async fn sample_data_file(data_path: &FsPath, sample_rows: usize) -> Result<Vec<RecordBatch>> {
    let canonical = data_path
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", data_path.display()))?;
    let uri = canonical
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("data file path is not valid UTF-8"))?;
    let (store, object_path) = ObjectStore::from_uri(uri).await?;
    let scheduler = ScanScheduler::new(store, SchedulerConfig::new(256 * 1024 * 1024));
    let file_scheduler = scheduler
        .open_file(&object_path, &CachedFileSize::unknown())
        .await?;
    let cache = LanceCache::no_cache();
    let reader = FileReader::try_open(
        file_scheduler,
        None,
        Arc::<DecoderPlugins>::default(),
        &cache,
        FileReaderOptions::default(),
    )
    .await?;
    let row_count = usize::try_from(reader.num_rows())
        .context("data file row count exceeds platform limits")?;
    let sample_size = sample_rows.min(row_count);
    if sample_size == 0 {
        return Ok(vec![]);
    }
    if row_count > u32::MAX as usize {
        anyhow::bail!("data file has more than u32::MAX rows and cannot use indexed sampling");
    }
    let mut indices = index::sample(&mut rand::rng(), row_count, sample_size)
        .into_vec()
        .into_iter()
        .map(|index| index as u32)
        .collect::<Vec<_>>();
    indices.sort_unstable();
    reader
        .read_stream(
            ReadBatchParams::Indices(UInt32Array::from(indices)),
            1024,
            16,
            FilterExpression::no_filter(),
        )
        .await?
        .try_collect()
        .await
        .map_err(Into::into)
}

/// Generate the independent axis sweep for a top-level Arrow field.
///
/// This always includes the exact metadata-preserving baseline and all general
/// codecs. Structural and value candidates are added only where Lance exposes
/// an applicable writer control for the data type and target format.
pub fn candidate_plans_for_type(
    data_type: &DataType,
    file_version: EncodingFileVersion,
) -> Vec<EncodingPlan> {
    let baseline = EncodingPlan::baseline(file_version);
    let mut plans = vec![baseline];
    for general in general_candidates() {
        plans.push(EncodingPlan {
            general,
            ..baseline
        });
    }

    if !matches!(data_type, DataType::Struct(_)) {
        for structural in [StructuralEncoding::MiniBlock, StructuralEncoding::FullZip] {
            plans.push(EncodingPlan {
                structural,
                ..baseline
            });
        }
        plans.push(EncodingPlan {
            structural: StructuralEncoding::Sparse,
            file_version: EncodingFileVersion::V2_3,
            ..baseline
        });
    }

    let mut add_value = |value, general| {
        plans.push(EncodingPlan {
            value,
            general,
            ..baseline
        });
    };
    if fixed_bit_width(data_type).is_some_and(|width| matches!(width, 8 | 16 | 32 | 64)) {
        add_value(ValueEncoding::Rle, GeneralCompression::Baseline);
    }
    if matches!(
        data_type,
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Binary | DataType::LargeBinary
    ) {
        add_value(ValueEncoding::Fsst, GeneralCompression::Baseline);
    }
    if matches!(data_type, DataType::Float32 | DataType::Float64) {
        // BSS has no useful standalone representation; Lance layers a general
        // compressor over the split byte streams.
        add_value(
            ValueEncoding::ByteStreamSplit,
            GeneralCompression::Zstd { level: 3 },
        );
    }
    if supports_dictionary(data_type) {
        add_value(ValueEncoding::Dictionary, GeneralCompression::Baseline);
    }
    if matches!(data_type, DataType::Struct(_))
        && matches!(
            file_version,
            EncodingFileVersion::V2_2 | EncodingFileVersion::V2_3
        )
    {
        add_value(ValueEncoding::PackedStruct, GeneralCompression::Baseline);
    }
    plans.sort();
    plans.dedup();
    plans
}

fn general_candidates() -> [GeneralCompression; 7] {
    [
        GeneralCompression::None,
        GeneralCompression::Lz4,
        GeneralCompression::Zstd { level: 1 },
        GeneralCompression::Zstd { level: 3 },
        GeneralCompression::Zstd { level: 6 },
        GeneralCompression::Zstd { level: 9 },
        GeneralCompression::Zstd { level: 12 },
    ]
}

fn combined_candidate_plans(measurements: &[EncodingMeasurement]) -> Vec<EncodingPlan> {
    let best_structural = measurements
        .iter()
        .filter(|measurement| measurement.plan.structural != StructuralEncoding::Auto)
        .min_by_key(|measurement| measurement.encoded_bytes)
        .map(|measurement| measurement.plan);
    let best_value = measurements
        .iter()
        .filter(|measurement| measurement.plan.value != ValueEncoding::Auto)
        .min_by_key(|measurement| measurement.encoded_bytes)
        .map(|measurement| measurement.plan);

    let mut beam = best_structural.into_iter().collect::<Vec<_>>();
    beam.extend(best_value);
    if let (Some(structural), Some(value)) = (best_structural, best_value) {
        beam.push(EncodingPlan {
            structural: structural.structural,
            value: value.value,
            general: value.general,
            file_version: structural.file_version.max(value.file_version),
        });
    }

    let mut plans = Vec::new();
    for plan in beam {
        plans.push(plan);
        if plan.value == ValueEncoding::Fsst {
            continue;
        }
        let generals: &[GeneralCompression] = if plan.value == ValueEncoding::ByteStreamSplit {
            &[
                GeneralCompression::Lz4,
                GeneralCompression::Zstd { level: 3 },
                GeneralCompression::Zstd { level: 9 },
            ]
        } else {
            &[
                GeneralCompression::None,
                GeneralCompression::Lz4,
                GeneralCompression::Zstd { level: 3 },
                GeneralCompression::Zstd { level: 9 },
            ]
        };
        plans.extend(
            generals
                .iter()
                .copied()
                .map(|general| EncodingPlan { general, ..plan }),
        );
    }
    plans.sort();
    plans.dedup();
    plans
}

fn fixed_bit_width(data_type: &DataType) -> Option<usize> {
    match data_type {
        DataType::Int8 | DataType::UInt8 => Some(8),
        DataType::Int16 | DataType::UInt16 | DataType::Float16 => Some(16),
        DataType::Int32
        | DataType::UInt32
        | DataType::Float32
        | DataType::Date32
        | DataType::Time32(_) => Some(32),
        DataType::Int64
        | DataType::UInt64
        | DataType::Float64
        | DataType::Date64
        | DataType::Time64(_)
        | DataType::Timestamp(_, _)
        | DataType::Duration(_) => Some(64),
        DataType::Decimal128(_, _) => Some(128),
        DataType::FixedSizeBinary(bytes) if *bytes > 0 => usize::try_from(*bytes)
            .ok()
            .and_then(|bytes| bytes.checked_mul(8)),
        _ => None,
    }
}

fn supports_dictionary(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Binary | DataType::LargeBinary
    ) || fixed_bit_width(data_type).is_some_and(|width| matches!(width, 64 | 128))
}

fn file_version_for_plan(plan: EncodingPlan) -> LanceFileVersion {
    match plan.file_version {
        EncodingFileVersion::V1 => LanceFileVersion::Legacy,
        EncodingFileVersion::V2_0 => LanceFileVersion::V2_0,
        EncodingFileVersion::V2_1 => LanceFileVersion::V2_1,
        EncodingFileVersion::V2_2 => LanceFileVersion::V2_2,
        EncodingFileVersion::V2_3 => LanceFileVersion::V2_3,
    }
}

async fn measure_candidate(
    batches: Vec<RecordBatch>,
    target_path: String,
    plan: EncodingPlan,
    sample_rows: u64,
    file_version: LanceFileVersion,
) -> Result<EncodingMeasurement> {
    let (schema, batches) = with_candidate_schema(batches, &target_path, plan)?;
    let reader = RecordBatchIterator::new(batches.into_iter().map(Ok), schema);
    let temp_dir = tempfile::tempdir()?;
    let dataset_path = temp_dir.path().join("sample.lance");
    let uri = dataset_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("temporary sample path is not valid UTF-8"))?;
    let write_params = WriteParams {
        data_storage_version: Some(file_version),
        ..Default::default()
    };
    let dataset = Dataset::write(reader, uri, Some(write_params)).await?;

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
    let column = report
        .columns
        .iter()
        .find(|column| column.path == target_path)
        .with_context(|| format!("sample rewrite omitted target column {target_path}"))?;
    Ok(EncodingMeasurement {
        plan,
        sample_rows,
        encoded_bytes: column.on_disk_bytes,
        resolved_encoding_tags: column.encoding_tags.clone(),
    })
}

fn with_candidate_schema(
    batches: Vec<RecordBatch>,
    target_path: &str,
    plan: EncodingPlan,
) -> Result<(Arc<ArrowSchema>, Vec<RecordBatch>)> {
    let source_schema = batches[0].schema();
    let fields = source_schema
        .fields()
        .iter()
        .map(|field| {
            if field.name() == target_path {
                Arc::new(with_candidate_field(field, plan))
            } else {
                field.clone()
            }
        })
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

fn with_candidate_field(field: &ArrowField, plan: EncodingPlan) -> ArrowField {
    let mut metadata = field.metadata().clone();
    match plan.structural {
        StructuralEncoding::Auto => {}
        StructuralEncoding::MiniBlock => {
            metadata.insert(
                "lance-encoding:structural-encoding".into(),
                "miniblock".into(),
            );
        }
        StructuralEncoding::FullZip => {
            metadata.insert(
                "lance-encoding:structural-encoding".into(),
                "fullzip".into(),
            );
        }
        StructuralEncoding::Sparse => {
            metadata.insert("lance-encoding:structural-encoding".into(), "sparse".into());
        }
    }
    if plan.value != ValueEncoding::Auto {
        for key in [
            "lance-encoding:rle-threshold",
            "lance-encoding:bss",
            "lance-encoding:dict-divisor",
            "lance-encoding:dict-size-ratio",
            "lance-encoding:packed",
        ] {
            metadata.remove(key);
        }
    }
    match plan.value {
        ValueEncoding::Auto => {}
        ValueEncoding::Rle => {
            metadata.insert("lance-encoding:rle-threshold".into(), "1.0".into());
            metadata.insert("lance-encoding:bss".into(), "off".into());
        }
        ValueEncoding::Fsst => {
            metadata.insert("lance-encoding:compression".into(), "fsst".into());
            metadata.remove("lance-encoding:compression-level");
        }
        ValueEncoding::ByteStreamSplit => {
            metadata.insert("lance-encoding:bss".into(), "on".into());
            metadata.insert("lance-encoding:rle-threshold".into(), "0".into());
        }
        ValueEncoding::Dictionary => {
            metadata.insert("lance-encoding:dict-divisor".into(), "1".into());
            metadata.insert("lance-encoding:dict-size-ratio".into(), "1.0".into());
        }
        ValueEncoding::PackedStruct => {
            metadata.insert("lance-encoding:packed".into(), "true".into());
        }
    }
    match plan.general {
        GeneralCompression::Baseline => {}
        GeneralCompression::None => {
            metadata.insert("lance-encoding:compression".into(), "none".into());
            metadata.remove("lance-encoding:compression-level");
        }
        GeneralCompression::Lz4 => {
            metadata.insert("lance-encoding:compression".into(), "lz4".into());
            metadata.remove("lance-encoding:compression-level");
        }
        GeneralCompression::Zstd { level } => {
            metadata.insert("lance-encoding:compression".into(), "zstd".into());
            metadata.insert("lance-encoding:compression-level".into(), level.to_string());
        }
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

    let mut physical_index: usize = 0;
    let columns = metadata
        .file_schema
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let physical_count =
                lance_file::versions::physical_column_count(metadata.version, field);
            let end = physical_index.saturating_add(physical_count);
            let physical_columns = &metadata.column_metadatas[physical_index..end];
            let physical_infos = &metadata.column_infos[physical_index..end];
            physical_index = end;
            let mut raw_page_encodings = physical_columns
                .iter()
                .flat_map(|column| &column.pages)
                .map(describe_encoding)
                .collect::<Vec<_>>();
            raw_page_encodings.sort();
            raw_page_encodings.dedup();

            let mut encoding_tags = physical_infos
                .iter()
                .flat_map(|column| encoding_tags::classify_column(column))
                .collect::<BTreeSet<_>>();
            if encoding_tags.is_empty() {
                encoding_tags.insert(EncodingTag::Unknown);
            }
            let page_bytes = physical_columns
                .iter()
                .flat_map(|column| &column.pages)
                .flat_map(|page| page.buffer_sizes.iter())
                .sum::<u64>();
            // Dictionaries and other shared data may live in column-level
            // buffers instead of page buffers; both contribute to column size.
            let column_buffer_bytes = physical_infos
                .iter()
                .flat_map(|column| column.buffer_offsets_and_sizes.iter())
                .map(|(_, size)| *size)
                .sum::<u64>();
            ColumnProfile {
                index,
                path: field.name.clone(),
                data_type: field.data_type().to_string(),
                pages: physical_columns
                    .iter()
                    .map(|column| column.pages.len())
                    .sum(),
                on_disk_bytes: page_bytes.saturating_add(column_buffer_bytes),
                field_metadata: field.metadata.clone().into_iter().collect(),
                encoding_tags,
                encoding_measurements: vec![],
                raw_page_encodings,
            }
        })
        .collect();

    Ok(ProbeReport {
        source: canonical.display().to_string(),
        file_version: match metadata.version {
            ConcreteFileVersion::V1 => EncodingFileVersion::V1,
            ConcreteFileVersion::V2_0 => EncodingFileVersion::V2_0,
            ConcreteFileVersion::V2_1 => EncodingFileVersion::V2_1,
            ConcreteFileVersion::V2_2 => EncodingFileVersion::V2_2,
            ConcreteFileVersion::V2_3 => EncodingFileVersion::V2_3,
        },
        file_size_bytes: metadata.file_size_bytes,
        data_bytes: metadata.num_data_bytes,
        rows: metadata.num_rows,
        unsupported_nested_targets: nested_field_paths(&metadata.file_schema),
        columns,
    })
}

fn nested_field_paths(schema: &lance_core::datatypes::Schema) -> Vec<String> {
    fn visit(field: &lance_core::datatypes::Field, parent: &str, output: &mut Vec<String>) {
        for child in &field.children {
            let path = format!("{parent}.{}", child.name);
            output.push(path.clone());
            visit(child, &path, output);
        }
    }

    let mut output = Vec::new();
    for field in &schema.fields {
        visit(field, &field.name, &mut output);
    }
    output
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn plans_are_type_and_version_aware() {
        let floats = candidate_plans_for_type(&DataType::Float32, EncodingFileVersion::V2_2);
        assert!(floats.iter().any(|plan| {
            plan.value == ValueEncoding::ByteStreamSplit
                && plan.general == GeneralCompression::Zstd { level: 3 }
        }));
        assert!(floats.iter().any(|plan| {
            plan.structural == StructuralEncoding::Sparse
                && plan.file_version == EncodingFileVersion::V2_3
        }));

        let strings = candidate_plans_for_type(&DataType::Utf8, EncodingFileVersion::V2_2);
        assert!(strings.iter().any(|plan| plan.value == ValueEncoding::Fsst));
        assert!(
            strings
                .iter()
                .any(|plan| plan.value == ValueEncoding::Dictionary)
        );
        assert!(
            !strings
                .iter()
                .any(|plan| plan.value == ValueEncoding::ByteStreamSplit)
        );

        let int64 = candidate_plans_for_type(&DataType::Int64, EncodingFileVersion::V2_2);
        assert!(int64.iter().any(|plan| plan.value == ValueEncoding::Rle));
        assert!(
            int64
                .iter()
                .any(|plan| plan.value == ValueEncoding::Dictionary)
        );

        let int32 = candidate_plans_for_type(&DataType::Int32, EncodingFileVersion::V2_2);
        assert!(int32.iter().any(|plan| plan.value == ValueEncoding::Rle));
        assert!(
            !int32
                .iter()
                .any(|plan| plan.value == ValueEncoding::Dictionary)
        );

        let struct_type = DataType::Struct(
            vec![Arc::new(ArrowField::new("child", DataType::Int32, true))].into(),
        );
        let structs = candidate_plans_for_type(&struct_type, EncodingFileVersion::V2_2);
        assert!(
            structs
                .iter()
                .any(|plan| plan.value == ValueEncoding::PackedStruct)
        );
        assert!(
            !structs
                .iter()
                .any(|plan| plan.structural == StructuralEncoding::Sparse)
        );
    }

    #[test]
    fn only_target_field_metadata_changes() {
        let target = ArrowField::new("target", DataType::Int32, false)
            .with_metadata(HashMap::from([("custom".into(), "keep".into())]));
        let other = Arc::new(
            ArrowField::new("other", DataType::Utf8, true)
                .with_metadata(HashMap::from([("other-key".into(), "other-value".into())])),
        );
        let source = Arc::new(ArrowSchema::new(vec![Arc::new(target), other.clone()]));
        let batch = RecordBatch::new_empty(source);
        let plan = EncodingPlan {
            structural: StructuralEncoding::MiniBlock,
            value: ValueEncoding::Rle,
            general: GeneralCompression::Lz4,
            file_version: EncodingFileVersion::V2_2,
        };
        let (schema, _) = with_candidate_schema(vec![batch], "target", plan).unwrap();
        assert_eq!(schema.field(1), other.as_ref());
        assert_eq!(schema.field(0).metadata().get("custom").unwrap(), "keep");
        assert_eq!(
            schema.field(0).metadata().get("lance-encoding:compression"),
            Some(&"lz4".into())
        );
    }
}
