use super::*;
use crate::probe::probe_local_dataset;
use arrow_array::{Int32Array, StringArray};
use arrow_schema::DataType;
use lance::dataset::ColumnAlteration;

async fn write(batch: RecordBatch, uri: &str, max_rows: usize) -> Dataset {
    Dataset::write(
        RecordBatchIterator::new([Ok(batch.clone())], batch.schema()),
        uri,
        Some(WriteParams {
            data_storage_version: Some(LanceFileVersion::V2_2),
            max_rows_per_file: max_rows,
            ..Default::default()
        }),
    )
    .await
    .unwrap()
}

fn baseline(report: &ProbeReport) -> &EncodingMeasurement {
    report.columns[0]
        .encoding_measurements
        .iter()
        .find(|m| m.plan == EncodingPlan::baseline(EncodingFileVersion::V2_2))
        .unwrap()
}

#[tokio::test]
async fn physical_baseline_survives_metadata_change_rename_and_drop() {
    let temp = tempfile::tempdir().unwrap();
    let uri = temp.path().join("input.lance");
    let text = ArrowField::new("text", DataType::Utf8, false).with_metadata(HashMap::from([
        ("lance-encoding:compression".into(), "zstd".into()),
        ("lance-encoding:compression-level".into(), "12".into()),
    ]));
    let batch = RecordBatch::try_new(
        Arc::new(ArrowSchema::new(vec![
            text,
            ArrowField::new("id", DataType::Int32, false),
        ])),
        vec![
            Arc::new(StringArray::from_iter_values(
                (0..512).map(|i| format!("{}{}", "repeated payload".repeat(8), i % 12)),
            )),
            Arc::new(Int32Array::from_iter_values(0..512)),
        ],
    )
    .unwrap();
    let mut dataset = write(batch, uri.to_str().unwrap(), 1024).await;
    let before = probe_local_dataset(&uri, "main", None, 512).await.unwrap();
    let id = dataset.schema().fields[0].id as u32;
    dataset
        .replace_field_metadata([(id, HashMap::new())])
        .await
        .unwrap();
    dataset
        .alter_columns(&[ColumnAlteration::new("text".into()).rename("renamed".into())])
        .await
        .unwrap();
    dataset.drop_columns(&["id"]).await.unwrap();
    let after = probe_local_dataset(&uri, "main", None, 512).await.unwrap();
    assert_eq!(after.files[0].columns.len(), 1);
    assert_eq!(after.files[0].columns[0].path, "renamed");
    assert_eq!(baseline(&before.files[0]), baseline(&after.files[0]));
    assert_eq!(
        baseline(&after.files[0]).encoded_bytes,
        after.files[0].columns[0].on_disk_bytes
    );
}

#[tokio::test]
async fn measurement_counts_every_rewritten_file() {
    let rows = 2 * 1024 * 1024;
    let schema = Arc::new(ArrowSchema::new(vec![ArrowField::new(
        "value",
        DataType::Int32,
        false,
    )]));
    let batch = RecordBatch::try_new(
        schema,
        vec![Arc::new(Int32Array::from_iter_values(
            (0..rows).map(|i| i % 257),
        ))],
    )
    .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let uri = temp.path().join("expected.lance");
    let dataset = write(batch.clone(), uri.to_str().unwrap(), 1024 * 1024).await;
    assert_eq!(dataset.iter_fragments().count(), 2);
    let report = probe_local_dataset(&uri, "main", None, 0).await.unwrap();
    let expected: u64 = report
        .files
        .iter()
        .map(|f| f.columns[0].on_disk_bytes)
        .sum();
    let measured = measure_candidate(&batch, EncodingPlan::baseline(EncodingFileVersion::V2_2))
        .await
        .unwrap();
    assert_eq!(measured.sample_rows, rows as u64);
    assert_eq!(measured.encoded_bytes, expected);
}

#[tokio::test]
async fn identical_files_reuse_measurements_but_distinct_metadata_does_not() {
    let temp = tempfile::tempdir().unwrap();
    let uri = temp.path().join("input.lance");
    let schema = Arc::new(ArrowSchema::new(vec![ArrowField::new(
        "value",
        DataType::Int32,
        false,
    )]));
    let batch =
        RecordBatch::try_new(schema, vec![Arc::new(Int32Array::from_iter_values(0..64))]).unwrap();
    let dataset = write(batch.clone(), uri.to_str().unwrap(), 32).await;
    let (mut report, mut schema) = {
        let file = dataset
            .iter_fragments()
            .next()
            .unwrap()
            .referenced_lance_files()
            .next()
            .unwrap()
            .path
            .clone();
        let report = probe_local_file(uri.join("data").join(file)).await.unwrap();
        (report, dataset.schema().clone())
    };
    let mut cache = MeasurementCache::default();
    attach_data_file_measurements(&batch, dataset.schema(), &schema, &mut report, &mut cache)
        .await
        .unwrap();
    let first = report.columns[0].encoding_measurements.clone();
    attach_data_file_measurements(&batch, dataset.schema(), &schema, &mut report, &mut cache)
        .await
        .unwrap();
    assert_eq!(cache.len(), 1);
    assert_eq!(report.columns[0].encoding_measurements, first);
    schema.fields[0]
        .metadata
        .insert("lance-encoding:compression".into(), "lz4".into());
    attach_data_file_measurements(&batch, dataset.schema(), &schema, &mut report, &mut cache)
        .await
        .unwrap();
    assert_eq!(cache.len(), 2);
}

#[tokio::test]
async fn dictionary_and_nested_columns_do_not_try_unsupported_layouts() {
    use arrow_array::builder::StringDictionaryBuilder;
    use arrow_array::{ArrayRef, ListArray, StructArray, types::Int32Type};
    let mut dictionary = StringDictionaryBuilder::<Int32Type>::new();
    for value in ["a", "b", "a", "b"] {
        dictionary.append(value).unwrap();
    }
    let list = Arc::new(ListArray::from_iter_primitive::<Int32Type, _, _>(
        (0..4).map(|i| Some(vec![Some(i)])),
    )) as ArrayRef;
    let struct_list = Arc::new(StructArray::from(vec![(
        Arc::new(ArrowField::new("items", list.data_type().clone(), true)),
        list,
    )])) as ArrayRef;
    let inner = Arc::new(StructArray::from(vec![(
        Arc::new(ArrowField::new("n", DataType::Int32, false)),
        Arc::new(Int32Array::from(vec![1, 2, 3, 4])) as ArrayRef,
    )])) as ArrayRef;
    let nested = Arc::new(StructArray::from(vec![(
        Arc::new(ArrowField::new("inner", inner.data_type().clone(), false)),
        inner,
    )])) as ArrayRef;
    let arrays = vec![
        Arc::new(dictionary.finish()) as ArrayRef,
        struct_list,
        nested,
    ];
    let fields = arrays
        .iter()
        .enumerate()
        .map(|(i, a)| ArrowField::new(format!("c{i}"), a.data_type().clone(), true))
        .collect::<Vec<_>>();
    let batch = RecordBatch::try_new(Arc::new(ArrowSchema::new(fields)), arrays).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let uri = temp.path().join("input.lance");
    write(batch.clone(), uri.to_str().unwrap(), 1024).await;
    let report = probe_local_dataset(&uri, "main", None, 4).await.unwrap();
    assert!(
        report.files[0]
            .columns
            .iter()
            .all(|c| !c.encoding_measurements.is_empty())
    );
    let unsupported = EncodingPlan {
        structural: crate::StructuralEncoding::Sparse,
        ..EncodingPlan::baseline(EncodingFileVersion::V2_3)
    };
    assert!(
        measure_supported_candidate(
            &batch.project(&[0]).unwrap(),
            unsupported,
            EncodingFileVersion::V2_2
        )
        .await
        .unwrap()
        .is_none()
    );
}

#[tokio::test]
async fn legacy_input_has_an_explicit_support_error() {
    let batch = RecordBatch::try_new(
        Arc::new(ArrowSchema::new(vec![ArrowField::new(
            "value",
            DataType::Int32,
            false,
        )])),
        vec![Arc::new(Int32Array::from(vec![1, 2]))],
    )
    .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let uri = temp.path().join("legacy.lance");
    Dataset::write(
        RecordBatchIterator::new([Ok(batch.clone())], batch.schema()),
        uri.to_str().unwrap(),
        Some(WriteParams {
            data_storage_version: Some(LanceFileVersion::Legacy),
            ..Default::default()
        }),
    )
    .await
    .unwrap();
    let error = probe_local_dataset(uri, "main", None, 0).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("legacy Lance v1 is not supported")
    );
}
