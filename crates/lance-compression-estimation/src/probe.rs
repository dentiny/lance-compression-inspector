//! Footer-only probing with Lance's native encoding descriptions.

use std::path::Path as FsPath;

use anyhow::{Context, Result};
use lance_file::reader::{FileReader, describe_encoding};
use lance_file::version::ConcreteFileVersion;
use lance_io::{
    object_store::ObjectStore,
    scheduler::{ScanScheduler, SchedulerConfig},
    utils::CachedFileSize,
};

use crate::{ColumnProfile, EncodingFileVersion, ProbeReport};

/// Maximum buffered I/O per scan scheduler (256 MiB).
const SCAN_IO_BUFFER_SIZE_BYTES: u64 = 256 * 1024 * 1024;

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
    let scheduler = ScanScheduler::new(store, SchedulerConfig::new(SCAN_IO_BUFFER_SIZE_BYTES));
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
                has_blob: field_contains_blob(field),
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
