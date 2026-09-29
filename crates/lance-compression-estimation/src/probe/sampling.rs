//! Sample rows using Lance’s dataset API.

use anyhow::Result;
use arrow_array::RecordBatch;
use lance::Dataset;

/// Sample live rows from the dataset snapshot in row-id order.
pub async fn sample_dataset(dataset: &Dataset, sample_rows: usize) -> Result<RecordBatch> {
    Ok(dataset.sample(sample_rows, dataset.schema(), None).await?)
}
