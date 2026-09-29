use super::projection::projected_file_size;
use crate::model::{Action, EncodingFileVersion, Location, ProbeReport, Severity, Suggestion};

const V2_2_CAPABILITIES: &[&str] = &[
    "constant-layout",
    "larger-miniblocks",
    "variable-packed-struct",
];
const V2_3_CAPABILITIES: &[&str] = &["sparse-structural-layout"];

/// Evaluate stable and experimental format upgrades for one physical file.
pub(crate) fn evaluate_file_versions(probe: &ProbeReport) -> Vec<Suggestion> {
    match probe.file_version {
        EncodingFileVersion::V1 | EncodingFileVersion::V2_3 => vec![],
        EncodingFileVersion::V2_2 => {
            vec![upgrade_suggestion(probe, EncodingFileVersion::V2_3, true)]
        }
        EncodingFileVersion::V2_0 | EncodingFileVersion::V2_1 => vec![
            upgrade_suggestion(probe, EncodingFileVersion::V2_2, false),
            upgrade_suggestion(probe, EncodingFileVersion::V2_3, true),
        ],
    }
}

fn upgrade_suggestion(
    probe: &ProbeReport,
    target: EncodingFileVersion,
    experimental: bool,
) -> Suggestion {
    let capabilities = match target {
        EncodingFileVersion::V2_2 => V2_2_CAPABILITIES,
        EncodingFileVersion::V2_3 => V2_3_CAPABILITIES,
        _ => &[],
    };
    let projection = projected_file_size(probe, target);
    let delta = projection.map(|projected| signed_delta(probe.file_size_bytes, projected));
    Suggestion {
        rule: if experimental {
            "experimental-file-version-upgrade"
        } else {
            "file-version-upgrade"
        }
        .into(),
        severity: if !experimental && delta.is_some_and(|delta| delta > 0) {
            Severity::Suggestion
        } else {
            Severity::Info
        },
        location: Location::File,
        message: format!(
            "Benchmark a rewrite from format {} to {}{}.",
            probe.file_version,
            target,
            if experimental { " (experimental)" } else { "" }
        ),
        action: Action::RewriteFileVersion {
            target: if experimental {
                format!("{target} (next/unstable)")
            } else {
                target.to_string()
            },
            capabilities: capabilities.iter().map(|value| (*value).into()).collect(),
            projected_file_bytes: projection,
            size_delta_bytes: delta,
        },
        estimate: None,
        candidate_scores: vec![],
        evidence: vec![format!(
            "format {} adds {}",
            target,
            capabilities.join(", ")
        )],
    }
}

fn signed_delta(current: u64, projected: u64) -> i64 {
    (i128::from(current) - i128::from(projected)).clamp(i128::from(i64::MIN), i128::from(i64::MAX))
        as i64
}
