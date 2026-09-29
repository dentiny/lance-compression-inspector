//! Sample dataset rows and measure candidate rewrites sequentially.

use std::{collections::BTreeSet, sync::Arc};

use anyhow::{Context, Result};
use arrow_array::{RecordBatch, RecordBatchIterator};
use arrow_schema::Schema as ArrowSchema;
use lance::{Dataset, dataset::WriteParams};
use lance_file::version::LanceFileVersion;

use super::{
    encoding_candidates::{
        candidate_plans_for_type, combined_candidate_plans, with_candidate_field,
    },
    probe_local_file,
};
use crate::{EncodingFileVersion, EncodingMeasurement, EncodingPlan, ProbeReport};

pub(super) async fn attach_data_file_measurements(
    sample: &RecordBatch,
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
    if sample.num_rows() == 0 {
        return Ok(());
    }
    // Blob columns are not candidates. Their sampled descriptors also refer to
    // source storage and cannot be written as values in a temporary dataset.
    let indices = sample
        .schema()
        .fields()
        .iter()
        .enumerate()
        .filter(|(_, field)| {
            report
                .columns
                .iter()
                .any(|column| column.path == field.name().as_str() && !column.has_blob)
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let sample = sample.project(&indices)?;
    let batches = vec![sample.clone()];
    let measured_rows = sample.num_rows() as u64;
    let schema = sample.schema();

    // The bounded policy runs one axis sweep, keeps the best structural and
    // value plan, and then combines only that small beam with representative
    // general compressors. Each rewrite finishes before the next one starts.
    for field in schema.fields() {
        let path = field.name().to_string();
        if !report.columns.iter().any(|column| column.path == path) {
            continue;
        }
        let mut stage_one = candidate_plans_for_type(field.data_type(), plan_version);
        for baseline_version in [source_plan_version, EncodingFileVersion::V2_3] {
            let baseline = EncodingPlan::baseline(baseline_version);
            if !stage_one.contains(&baseline) {
                stage_one.push(baseline);
            }
        }
        let mut measurements = Vec::with_capacity(stage_one.len());
        for plan in stage_one {
            let measurement = measure_candidate(
                batches.clone(),
                path.clone(),
                plan,
                measured_rows,
                file_version_for_plan(plan),
            )
            .await?;
            measurements.push(measurement);
        }

        let stage_two = combined_candidate_plans(&measurements);
        let measured_plans = measurements
            .iter()
            .map(|measurement| measurement.plan)
            .collect::<BTreeSet<_>>();
        for plan in stage_two {
            if measured_plans.contains(&plan) {
                continue;
            }
            let measurement = measure_candidate(
                batches.clone(),
                path.clone(),
                plan,
                measured_rows,
                file_version_for_plan(plan),
            )
            .await?;
            measurements.push(measurement);
        }
        if let Some(column) = report.columns.iter_mut().find(|column| column.path == path) {
            column.encoding_measurements = measurements;
        }
    }
    Ok(())
}

/// Sample live rows from the dataset snapshot in row-id order.
pub async fn sample_dataset(dataset: &Dataset, sample_rows: usize) -> Result<RecordBatch> {
    Ok(dataset.sample(sample_rows, dataset.schema(), None).await?)
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
        resolved_page_encodings: column.raw_page_encodings.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GeneralCompression, StructuralEncoding, ValueEncoding};
    use arrow_schema::{DataType, Field as ArrowField};
    use std::collections::HashMap;

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
