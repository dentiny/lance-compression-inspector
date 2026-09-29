use crate::model::{
    Action, EncodingFileVersion, EncodingPlan, EncodingTag, Location, ProbeReport, Severity,
    Suggestion,
};

/// Generate file-format upgrade suggestions from the probed file version and
/// previously measured rewrite sizes.
///
/// Version 2.3 needs no upgrade, version 2.2 only gets the experimental 2.3
/// option, and older versions are evaluated against both stable 2.2 and
/// experimental 2.3.
pub(crate) fn check(probe: &ProbeReport) -> Vec<Suggestion> {
    let Some(version) = encoding_file_version(&probe.file_version) else {
        return vec![];
    };
    if version == EncodingFileVersion::V2_3 {
        return vec![];
    }
    if version == EncodingFileVersion::V2_2 {
        return vec![experimental_v2_3_suggestion(probe)];
    }
    vec![
        stable_v2_2_suggestion(probe),
        experimental_v2_3_suggestion(probe),
    ]
}

fn stable_v2_2_suggestion(probe: &ProbeReport) -> Suggestion {
    let capabilities = vec![
        "constant-layout".into(),
        "larger-miniblocks".into(),
        "variable-packed-struct".into(),
    ];
    let projection = projected_file_size(probe, EncodingFileVersion::V2_2);
    let delta = projection.map(|projected| signed_delta(probe, projected));
    Suggestion {
        rule: "file-version-upgrade".into(),
        severity: if delta.is_some_and(|delta| delta > 0) {
            Severity::Suggestion
        } else {
            Severity::Info
        },
        location: Location::File,
        message: format!(
            "File version {} predates Lance 2.2 encoding improvements; benchmark a rewrite to stable format 2.2.",
            probe.file_version
        ),
        action: Action::RewriteFileVersion {
            target: "2.2".into(),
            capabilities: capabilities.clone(),
            projected_file_bytes: projection,
            size_delta_bytes: delta,
        },
        estimate: None,
        candidate_scores: vec![],
        evidence: vec![
            format!("observed file version: {}", probe.file_version),
            format!("new stable capabilities: {}", capabilities.join(", ")),
            "2.2 is the stable/default target format".into(),
        ],
    }
}

fn experimental_v2_3_suggestion(probe: &ProbeReport) -> Suggestion {
    let projection = projected_file_size(probe, EncodingFileVersion::V2_3);
    let structural_candidates = probe
        .columns
        .iter()
        .filter(|column| {
            [
                EncodingTag::Nullable,
                EncodingTag::List,
                EncodingTag::FixedSizeList,
                EncodingTag::Struct,
            ]
            .iter()
            .any(|tag| column.encoding_tags.contains(tag))
        })
        .map(|column| column.path.as_str())
        .collect::<Vec<_>>();
    let mut evidence = vec![
        format!("observed file version: {}", probe.file_version),
        "2.3 adds sparse structural layout for sparse flat or nested pages".into(),
        "2.3 is the next/unstable target format".into(),
        "footer metadata does not expose null/empty-list density, so no savings estimate is assigned".into(),
    ];
    if !structural_candidates.is_empty() {
        evidence.push(format!(
            "structural candidate columns: {}",
            structural_candidates.join(", ")
        ));
    }

    Suggestion {
        rule: "experimental-file-version-upgrade".into(),
        severity: Severity::Info,
        location: Location::File,
        message: format!(
            "Also benchmark an experimental rewrite from {} to Lance 2.3; sparse layout may help null-heavy or nested data.",
            probe.file_version
        ),
        action: Action::RewriteFileVersion {
            target: "2.3 (next/unstable)".into(),
            capabilities: vec!["sparse-structural-layout".into()],
            projected_file_bytes: projection,
            size_delta_bytes: projection.map(|projected| signed_delta(probe, projected)),
        },
        estimate: None,
        candidate_scores: vec![],
        evidence,
    }
}

fn projected_file_size(probe: &ProbeReport, target: EncodingFileVersion) -> Option<u64> {
    let source = encoding_file_version(&probe.file_version)?;
    let current_column_bytes = probe
        .columns
        .iter()
        .map(|column| column.on_disk_bytes)
        .sum::<u64>();
    let fixed_overhead = probe.file_size_bytes.saturating_sub(current_column_bytes);
    probe
        .columns
        .iter()
        .try_fold(fixed_overhead, |total, column| {
            let current = column
                .encoding_measurements
                .iter()
                .find(|measurement| measurement.plan == EncodingPlan::baseline(source))?;
            let target = column
                .encoding_measurements
                .iter()
                .find(|measurement| measurement.plan == EncodingPlan::baseline(target))?;
            let projected = project_bytes(
                column.on_disk_bytes,
                target.encoded_bytes,
                current.encoded_bytes,
            );
            Some(total.saturating_add(projected))
        })
}

fn project_bytes(full_bytes: u64, target_sample_bytes: u64, current_sample_bytes: u64) -> u64 {
    if current_sample_bytes == 0 {
        return full_bytes;
    }
    let projected = u128::from(full_bytes)
        .saturating_mul(u128::from(target_sample_bytes))
        .checked_div(u128::from(current_sample_bytes))
        .unwrap_or(u128::from(full_bytes));
    projected.min(u128::from(u64::MAX)) as u64
}

fn signed_delta(probe: &ProbeReport, projected: u64) -> i64 {
    let delta = i128::from(probe.file_size_bytes) - i128::from(projected);
    delta.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

fn encoding_file_version(version: &str) -> Option<EncodingFileVersion> {
    match version {
        "0.1" | "V1" => Some(EncodingFileVersion::V1),
        "2.0" | "V2_0" => Some(EncodingFileVersion::V2_0),
        "2.1" | "V2_1" => Some(EncodingFileVersion::V2_1),
        "2.2" | "V2_2" => Some(EncodingFileVersion::V2_2),
        "2.3" | "V2_3" => Some(EncodingFileVersion::V2_3),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::model::ColumnProfile;

    fn report(version: &str, tags: &[EncodingTag]) -> ProbeReport {
        ProbeReport {
            source: "test.lance".into(),
            file_version: version.into(),
            file_size_bytes: 1024,
            data_bytes: 1024,
            rows: 100,
            unsupported_nested_targets: vec![],
            columns: vec![ColumnProfile {
                index: 0,
                path: "nested".into(),
                data_type: "FixedSizeList(64 x Float32)".into(),
                pages: 1,
                on_disk_bytes: 1024,
                field_metadata: BTreeMap::new(),
                encoding_tags: tags.iter().copied().collect::<BTreeSet<_>>(),
                encoding_measurements: vec![],
                raw_page_encodings: vec![],
            }],
        }
    }

    #[test]
    fn old_version_gets_stable_and_experimental_paths() {
        let suggestions = check(&report("2.1", &[]));
        assert_eq!(suggestions.len(), 2);
        assert_eq!(suggestions[0].rule, "file-version-upgrade");
        assert_eq!(suggestions[1].rule, "experimental-file-version-upgrade");
    }

    #[test]
    fn stable_version_still_considers_sparse_next_format() {
        let suggestions = check(&report("2.2", &[EncodingTag::FixedSizeList]));
        assert_eq!(suggestions.len(), 1);
        assert!(
            suggestions[0]
                .evidence
                .iter()
                .any(|item| item.contains("structural candidate columns: nested"))
        );
    }

    #[test]
    fn v2_3_needs_no_version_suggestion() {
        assert!(check(&report("2.3", &[])).is_empty());
    }
}
