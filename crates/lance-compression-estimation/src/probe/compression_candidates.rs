//! General compression candidates and their field metadata.

use crate::{EncodingFileVersion, EncodingPlan, GeneralCompression};
use std::collections::HashMap;

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

pub(super) fn apply_compression(
    metadata: &mut HashMap<String, String>,
    general: GeneralCompression,
) {
    let codec = match general {
        GeneralCompression::Baseline => return,
        GeneralCompression::None => "none",
        GeneralCompression::Lz4 => "lz4",
        GeneralCompression::Zstd { .. } => "zstd",
    };
    metadata.insert("lance-encoding:compression".into(), codec.into());
    if let GeneralCompression::Zstd { level } = general {
        metadata.insert("lance-encoding:compression-level".into(), level.to_string());
    } else {
        metadata.remove("lance-encoding:compression-level");
    }
}
