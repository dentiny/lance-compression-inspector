//! Sample once and measure one column at a time, sequentially.

use super::{
    encoding_candidates::{
        candidate_plans_for_type, combined_candidate_plans, with_candidate_field,
    },
    probe_local_file,
};
use crate::{EncodingFileVersion, EncodingMeasurement, EncodingPlan, ProbeReport};
use anyhow::{Context, Result};
use arrow_array::{RecordBatch, RecordBatchIterator, RecordBatchOptions};
use arrow_schema::{Field as ArrowField, Schema as ArrowSchema};
use lance::{Dataset, dataset::WriteParams};
use lance_core::datatypes::Schema;
use lance_file::version::LanceFileVersion;
use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

// All files share the same sampled values. Include the physical field (including
// nested metadata) and source version so unlike baselines never share results.
pub(super) type MeasurementCache =
    HashMap<(i32, EncodingFileVersion, ArrowField), Vec<EncodingMeasurement>>;

pub(super) async fn attach_data_file_measurements(
    sample: &RecordBatch,
    current_schema: &Schema,
    file_schema: &Schema,
    report: &mut ProbeReport,
    cache: &mut MeasurementCache,
) -> Result<()> {
    if sample.num_rows() == 0 {
        return Ok(());
    }
    let source_version = report.file_version;
    let plan_version = source_version.max(EncodingFileVersion::V2_2);
    for column in &mut report.columns {
        if column.has_blob {
            continue;
        }
        let source_field = &file_schema.fields[column.index];
        let sample_index = current_schema
            .fields
            .iter()
            .position(|f| f.id == source_field.id)
            .context("sample is missing an active field")?;
        let field = ArrowField::from(source_field).with_name(column.path.clone());
        // A changed nested shape cannot reproduce the physical baseline. Leave
        // it unmeasured rather than comparing different sets of child values.
        if !same_field_ids(source_field, &current_schema.fields[sample_index])
            || !field
                .data_type()
                .equals_datatype(sample.column(sample_index).data_type())
        {
            eprintln!(
                "Skipping changed nested schema for {}: physical baseline cannot be reproduced",
                column.path
            );
            continue;
        }
        let key = (source_field.id, source_version, field.clone());
        if let Some(measurements) = cache.get(&key) {
            column.encoding_measurements = measurements.clone();
            continue;
        }
        let batch = RecordBatch::try_new_with_options(
            Arc::new(ArrowSchema::new(vec![field])),
            vec![sample.column(sample_index).clone()],
            &RecordBatchOptions::new().with_match_field_names(false),
        )?;
        let mut plans = candidate_plans_for_type(batch.schema().field(0).data_type(), plan_version);
        for version in [source_version, EncodingFileVersion::V2_3] {
            let baseline = EncodingPlan::baseline(version);
            if !plans.contains(&baseline) {
                plans.push(baseline);
            }
        }
        let mut measurements = Vec::new();
        for plan in plans {
            if let Some(measurement) =
                measure_supported_candidate(&batch, plan, source_version).await?
            {
                measurements.push(measurement);
            }
        }
        for plan in combined_candidate_plans(&measurements) {
            if !measurements.iter().any(|m| m.plan == plan)
                && let Some(measurement) =
                    measure_supported_candidate(&batch, plan, source_version).await?
            {
                measurements.push(measurement);
            }
        }
        cache.insert(key, measurements.clone());
        column.encoding_measurements = measurements;
    }
    Ok(())
}

async fn measure_supported_candidate(
    batch: &RecordBatch,
    plan: EncodingPlan,
    source: EncodingFileVersion,
) -> Result<Option<EncodingMeasurement>> {
    match measure_candidate(batch, plan).await {
        Ok(measurement) => Ok(Some(measurement)),
        Err(error)
            if plan != EncodingPlan::baseline(source)
                && error.downcast_ref::<lance_core::Error>().is_some_and(|e| {
                    matches!(
                        e,
                        lance_core::Error::InvalidInput { .. }
                            | lance_core::Error::NotSupported { .. }
                    )
                }) =>
        {
            eprintln!(
                "Skipping unsupported candidate {plan:?} for {}: {error}",
                batch.schema().field(0).name()
            );
            Ok(None)
        }
        Err(error) => Err(error)
            .with_context(|| format!("measuring {plan:?} for {}", batch.schema().field(0).name())),
    }
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

async fn measure_candidate(batch: &RecordBatch, plan: EncodingPlan) -> Result<EncodingMeasurement> {
    let field = with_candidate_field(batch.schema().field(0), plan);
    let schema = Arc::new(ArrowSchema::new(vec![field]));
    let candidate = RecordBatch::try_new_with_options(
        schema.clone(),
        batch.columns().to_vec(),
        &RecordBatchOptions::new().with_match_field_names(false),
    )?;
    let reader = RecordBatchIterator::new([Ok(candidate)], schema);
    let temp_dir = tempfile::tempdir()?;
    let dataset_path = temp_dir.path().join("sample.lance");
    let uri = dataset_path
        .to_str()
        .context("temporary sample path is not valid UTF-8")?;
    let params = WriteParams {
        data_storage_version: Some(file_version_for_plan(plan)),
        ..Default::default()
    };
    let dataset = Dataset::write(reader, uri, Some(params)).await?;
    let paths = dataset
        .iter_fragments()
        .flat_map(|f| f.referenced_lance_files())
        .map(|file| file.path.clone())
        .collect::<BTreeSet<_>>();
    anyhow::ensure!(!paths.is_empty(), "sample rewrite produced no data file");
    let mut encoded_bytes = 0;
    let mut encodings = BTreeSet::new();
    for path in paths {
        let root_relative = dataset_path.join(&path);
        let path = if root_relative.exists() {
            root_relative
        } else {
            dataset_path.join("data").join(path)
        };
        let report = probe_local_file(path).await?;
        let column = report
            .columns
            .first()
            .context("sample rewrite omitted target column")?;
        encoded_bytes += column.on_disk_bytes;
        encodings.extend(column.raw_page_encodings.iter().cloned());
    }
    Ok(EncodingMeasurement {
        plan,
        sample_rows: batch.num_rows() as u64,
        encoded_bytes,
        resolved_page_encodings: encodings.into_iter().collect(),
    })
}

// Child order and identity must agree even when names or metadata changed.
fn same_field_ids(
    left: &lance_core::datatypes::Field,
    right: &lance_core::datatypes::Field,
) -> bool {
    left.id == right.id
        && left.children.len() == right.children.len()
        && left
            .children
            .iter()
            .zip(&right.children)
            .all(|(a, b)| same_field_ids(a, b))
}

#[cfg(test)]
#[path = "sampling_tests.rs"]
mod tests;
