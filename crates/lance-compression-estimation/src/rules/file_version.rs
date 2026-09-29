use crate::model::{
    Action, EncodingFileVersion, EncodingPlan, Location, ProbeReport, Severity, Suggestion,
};

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

/// Estimate file size in `target` format with unchanged writer controls.
/// Scale each column's current bytes by target/current baseline sample bytes,
/// then add the existing non-column overhead unchanged.
/// Keep Blob bytes unchanged; return None if a non-Blob column lacks a baseline.
/// A zero-byte current baseline keeps that column's size unchanged.
fn projected_file_size(probe: &ProbeReport, target: EncodingFileVersion) -> Option<u64> {
    let fixed_overhead = probe.file_size_bytes.saturating_sub(
        probe
            .columns
            .iter()
            .map(|column| column.on_disk_bytes)
            .sum(),
    );
    probe
        .columns
        .iter()
        .try_fold(fixed_overhead, |total, column| {
            // Blob bytes stay unchanged; Blob columns have no candidate measurements.
            if column.has_blob {
                return Some(total.saturating_add(column.on_disk_bytes));
            }
            let current = baseline_bytes(column, probe.file_version)?;
            let target = baseline_bytes(column, target)?;
            Some(total.saturating_add(project_bytes(column.on_disk_bytes, target, current)))
        })
}

fn baseline_bytes(
    column: &crate::model::ColumnProfile,
    version: EncodingFileVersion,
) -> Option<u64> {
    column
        .encoding_measurements
        .iter()
        .find(|measurement| measurement.plan == EncodingPlan::baseline(version))
        .map(|measurement| measurement.encoded_bytes)
}

fn project_bytes(full_bytes: u64, target_sample_bytes: u64, current_sample_bytes: u64) -> u64 {
    if current_sample_bytes == 0 {
        return full_bytes;
    }
    let projected = u128::from(full_bytes).saturating_mul(u128::from(target_sample_bytes))
        / u128::from(current_sample_bytes);
    projected.min(u128::from(u64::MAX)) as u64
}

fn signed_delta(current: u64, projected: u64) -> i64 {
    (i128::from(current) - i128::from(projected)).clamp(i128::from(i64::MIN), i128::from(i64::MAX))
        as i64
}
