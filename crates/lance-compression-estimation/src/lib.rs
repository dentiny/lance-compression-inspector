//! Probe Lance file metadata and estimate encoding-aware optimizations.

mod analysis;
mod model;
pub mod probe;
mod rules;

pub use analysis::{analyze, analyze_dataset, analyze_dataset_with_options, analyze_with_options};
pub use model::*;
