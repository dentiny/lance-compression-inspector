//! General compression candidates and their field metadata.

use crate::{EncodingFileVersion, EncodingPlan, GeneralCompression};
use arrow_schema::Field as ArrowField;

/// Generate the baseline and general compression plans for a target format.
pub(super) fn compression_candidate_plans(file_version: EncodingFileVersion) -> Vec<EncodingPlan> {
    let baseline = EncodingPlan::baseline(file_version);
    let mut plans = vec![baseline];
    plans.extend(
        generate_compression_candicates().map(|general| EncodingPlan {
            general,
            ..baseline
        }),
    );
    plans.sort();
    plans.dedup();
    plans
}

fn generate_compression_candicates() -> [GeneralCompression; 7] {
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

pub(super) fn with_compression_field(
    field: &ArrowField,
    general: GeneralCompression,
) -> ArrowField {
    let mut metadata = field.metadata().clone();
    match general {
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
