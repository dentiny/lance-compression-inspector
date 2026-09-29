//! Probe dataset snapshots and standalone Lance file metadata.
//!
//! Encoding descriptions come directly from Lance and are preserved for
//! reporting. Sampling and scoring do not depend on a custom encoding taxonomy.

mod compression_candidates;
mod encoding_candidates;
mod metadata;
mod sampling;
mod storage;
mod type_utils;

pub use encoding_candidates::candidate_plans_for_type;
pub use sampling::sample_dataset;
use sampling::{MeasurementCache, attach_data_file_measurements};

use std::{collections::BTreeSet, path::Path as FsPath, sync::Arc};

use anyhow::{Context, Result};
use lance_file::reader::{FileReader, describe_encoding};
use lance_file::version::ConcreteFileVersion;
use lance_io::{
    object_store::ObjectStore,
    scheduler::{ScanScheduler, SchedulerConfig},
    utils::CachedFileSize,
};
use object_store::ObjectStoreExt;

use crate::{ColumnProfile, DatasetProbeReport, EncodingFileVersion, ProbeReport};

/// Maximum buffered I/O per scan scheduler (256 MiB).
const SCAN_IO_BUFFER_SIZE_BYTES: u64 = 256 * 1024 * 1024;

/// Resolve a local dataset snapshot from its manifest and probe every active
/// Lance data file. `version=None` selects the latest version on `branch`.
pub async fn probe_local_dataset(
    source: impl AsRef<FsPath>,
    branch: &str,
    version: Option<u64>,
    sample_rows: usize,
) -> Result<DatasetProbeReport> {
    probe_dataset(
        source
            .as_ref()
            .to_str()
            .context("dataset path is not valid UTF-8")?,
        branch,
        version,
        sample_rows,
    )
    .await
}

/// Probe a local path or remote dataset URI using environment configuration.
/// Manifest reads, sampling, and file metadata share the same object store.
pub async fn probe_dataset(
    source: &str,
    branch: &str,
    version: Option<u64>,
    sample_rows: usize,
) -> Result<DatasetProbeReport> {
    let (source, builder) = storage::dataset_builder(source)?;
    let dataset = builder
        .with_branch(branch, version)
        .load()
        .await
        .with_context(|| {
            format!("cannot open Lance dataset {source} at branch {branch}, version {version:?}")
        })?;
    let store = dataset.object_store(None).await?;
    let data_dir = dataset.data_dir();
    let root = data_dir.parent().context("dataset has no root directory")?;

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

    let sample = if sample_rows > 0 {
        Some(sample_dataset(&dataset, sample_rows).await?)
    } else {
        None
    };
    let mut files = Vec::with_capacity(relative_paths.len());
    let mut measurements = MeasurementCache::default();
    for relative_path in relative_paths {
        let root_relative = object_store::path::Path::parse(format!("{root}/{relative_path}"))?;
        let (data_path, relative_source) = match store.inner.head(&root_relative).await {
            Ok(_) => (root_relative, relative_path.clone()),
            Err(object_store::Error::NotFound { .. }) => (
                object_store::path::Path::parse(format!("{data_dir}/{relative_path}"))?,
                format!("data/{relative_path}"),
            ),
            Err(error) => return Err(error.into()),
        };
        let file_source = if source.contains("://") {
            let mut uri = url::Url::parse(&source)?;
            uri.set_path(data_path.as_ref());
            uri.to_string()
        } else {
            format!("{source}/{relative_source}")
        };
        let (mut report, file_schema) = probe_file(store.clone(), data_path, file_source).await?;
        // Match stable IDs, not names: deleted fields remain in old physical files.
        // Their bytes stay in the fixed overhead of the projection.
        report.columns.retain_mut(|column| {
            let source_field = &file_schema.fields[column.index];
            let Some(current) = dataset
                .schema()
                .fields
                .iter()
                .find(|f| f.id == source_field.id)
            else {
                return false;
            };
            column.path = current.name.clone();
            true
        });
        if let Some(sample) = &sample {
            attach_data_file_measurements(
                sample,
                dataset.schema(),
                &file_schema,
                &mut report,
                &mut measurements,
            )
            .await?;
        }
        files.push(report);
    }

    Ok(DatasetProbeReport {
        source,
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
    let uri = canonical
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("file path is not valid UTF-8"))?;
    let (store, object_path) = ObjectStore::from_uri(uri).await?;
    Ok(
        probe_file(store, object_path, canonical.display().to_string())
            .await?
            .0,
    )
}

async fn probe_file(
    store: Arc<ObjectStore>,
    object_path: object_store::path::Path,
    source: String,
) -> Result<(ProbeReport, lance_core::datatypes::Schema)> {
    let scheduler = ScanScheduler::new(store, SchedulerConfig::new(SCAN_IO_BUFFER_SIZE_BYTES));
    let file_scheduler = scheduler
        .open_file(&object_path, &CachedFileSize::unknown())
        .await
        .with_context(|| format!("cannot open Lance file {source}"))?;
    let metadata = match FileReader::read_all_metadata(&file_scheduler).await {
        Err(lance_core::Error::VersionConflict {
            major_version,
            minor_version,
            ..
        }) if matches!(
            ConcreteFileVersion::from_footer_numbers(major_version, minor_version),
            Ok(ConcreteFileVersion::V1)
        ) =>
        {
            anyhow::bail!(
                "legacy Lance v1 is not supported; supported file formats are 2.0–2.3: {source}"
            );
        }
        result => result.with_context(|| format!("cannot read Lance metadata from {source}"))?,
    };

    let file_schema = metadata::restore_blob_children(&metadata.file_schema);
    let mut physical_index: usize = 0;
    let columns = file_schema
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let physical_count =
                lance_file::versions::physical_column_count(metadata.version, field);
            anyhow::ensure!(physical_index + physical_count <= metadata.column_metadatas.len(),
                "invalid physical column mapping: field {}, version {:?}, index {}, count {}, columns {}",
                field.name, metadata.version, physical_index, physical_count, metadata.column_metadatas.len());
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
            Ok(ColumnProfile {
                index,
                path: field.name.clone(),
                data_type: field.data_type().to_string(),
                pages: physical_columns
                    .iter()
                    .map(|column| column.pages.len())
                    .sum(),
                on_disk_bytes: page_bytes.saturating_add(column_buffer_bytes),
                field_metadata: field.metadata.clone().into_iter().collect(),
                has_blob: field_contains_blob(field),
                encoding_measurements: vec![],
                raw_page_encodings,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok((
        ProbeReport {
            source,
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
            unsupported_nested_targets: nested_field_paths(&file_schema),
            columns,
        },
        file_schema,
    ))
}

// Use Lance's schema predicate rather than classifying every page encoding.
fn field_contains_blob(field: &lance_core::datatypes::Field) -> bool {
    field.is_blob() || field.children.iter().any(field_contains_blob)
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
mod blob_tests {
    use arrow_schema::{DataType, Field};
    use std::{collections::HashMap, sync::Arc};

    use super::field_contains_blob;

    #[test]
    fn detects_legacy_and_v2_blobs_in_nested_fields() {
        for (key, value) in [
            ("lance-encoding:blob", "true"),
            ("ARROW:extension:name", "lance.blob.v2"),
        ] {
            let blob = Field::new("payload", DataType::LargeBinary, true)
                .with_metadata(HashMap::from([(key.into(), value.into())]));
            let blob_field = lance_core::datatypes::Field::try_from(&blob).unwrap();
            assert!(field_contains_blob(&blob_field));
            let parent = Field::new(
                "parent",
                DataType::Struct(vec![Arc::new(blob)].into()),
                true,
            );
            let parent = lance_core::datatypes::Field::try_from(&parent).unwrap();
            assert!(field_contains_blob(&parent));
        }
    }

    #[test]
    fn ordinary_binary_fields_are_not_blobs() {
        let field = Field::new("blob", DataType::LargeBinary, true);
        let field = lance_core::datatypes::Field::try_from(&field).unwrap();
        assert!(!field_contains_blob(&field));
    }
}
