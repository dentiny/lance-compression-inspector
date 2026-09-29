use crate::model::{Action, EncodingTag, Location, ProbeReport, Severity, Suggestion};

pub(crate) fn check(probe: &ProbeReport) -> Vec<Suggestion> {
    let mut suggestions = Vec::new();

    if matches!(
        probe.file_version.as_str(),
        "V1" | "V2_0" | "V2_1" | "0.1" | "2.0" | "2.1"
    ) {
        let capabilities = vec![
            "constant-layout".into(),
            "larger-miniblocks".into(),
            "variable-packed-struct".into(),
        ];
        suggestions.push(Suggestion {
            rule: "file-version-upgrade".into(),
            severity: Severity::Suggestion,
            location: Location::File,
            message: format!(
                "File version {} predates Lance 2.2 encoding improvements; benchmark a rewrite to the current stable 2.2 format.",
                probe.file_version
            ),
            action: Action::RewriteFileVersion {
                target: "2.2".into(),
                capabilities: capabilities.clone(),
            },
            estimate: None,
            candidate_scores: vec![],
            evidence: vec![
                format!("observed file version: {}", probe.file_version),
                format!("new stable capabilities: {}", capabilities.join(", ")),
                "2.2 is the stable/default format in lance-file 12.0.0".into(),
            ],
        });
    }

    if matches!(
        probe.file_version.as_str(),
        "V1" | "V2_0" | "V2_1" | "V2_2" | "0.1" | "2.0" | "2.1" | "2.2"
    ) {
        suggestions.push(experimental_v2_3_suggestion(probe));
    }

    suggestions
}

fn experimental_v2_3_suggestion(probe: &ProbeReport) -> Suggestion {
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
        "2.3 is the next/unstable format in lance-file 12.0.0".into(),
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
        },
        estimate: None,
        candidate_scores: vec![],
        evidence,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::model::{ColumnProfile, REPORT_SCHEMA_VERSION};

    fn report(version: &str, tags: &[EncodingTag]) -> ProbeReport {
        ProbeReport {
            schema_version: REPORT_SCHEMA_VERSION,
            source: "test.lance".into(),
            file_version: version.into(),
            file_size_bytes: 1024,
            data_bytes: 1024,
            rows: 100,
            columns: vec![ColumnProfile {
                index: 0,
                path: "nested".into(),
                data_type: "FixedSizeList(64 x Float32)".into(),
                pages: 1,
                on_disk_bytes: 1024,
                field_metadata: BTreeMap::new(),
                encoding_tags: tags.iter().copied().collect::<BTreeSet<_>>(),
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
