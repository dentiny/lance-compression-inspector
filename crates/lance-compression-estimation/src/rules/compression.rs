use std::cmp::Ordering;

use crate::model::{
    Action, AnalyzeOptions, CandidateScore, ColumnProfile, CompressionAlgorithm, Confidence,
    EncodingCandidate, EncodingTag, EstimateBasis, Location, ProbeReport, SavingsEstimate,
    Severity, Suggestion,
};

const MIN_GENERAL_COMPRESSION_BYTES: u64 = 8 * 1024 * 1024;
const CANDIDATES: [EncodingCandidate; 4] = [
    EncodingCandidate {
        algorithm: CompressionAlgorithm::Lz4,
        level: None,
    },
    EncodingCandidate {
        algorithm: CompressionAlgorithm::Zstd,
        level: Some(1),
    },
    EncodingCandidate {
        algorithm: CompressionAlgorithm::Zstd,
        level: Some(3),
    },
    EncodingCandidate {
        algorithm: CompressionAlgorithm::Zstd,
        level: Some(9),
    },
];

pub(crate) fn check(probe: &ProbeReport, options: AnalyzeOptions) -> Vec<Suggestion> {
    probe
        .columns
        .iter()
        .filter_map(|column| check_column(column, options))
        .collect()
}

fn check_column(column: &ColumnProfile, options: AnalyzeOptions) -> Option<Suggestion> {
    if column.encoding_tags.contains(&EncodingTag::Unknown) {
        return Some(Suggestion {
            rule: "unknown-encoding".into(),
            severity: Severity::Info,
            location: column_location(column),
            message: "A page uses an encoding this estimator does not classify yet; its raw description is preserved.".into(),
            action: Action::InspectUnknownEncoding,
            estimate: None,
            candidate_scores: vec![],
            evidence: vec!["unknown encoding tag".into()],
        });
    }
    if column.encoding_tags.contains(&EncodingTag::StructuralBlob) {
        return None;
    }

    let has_codec = column.encoding_tags.contains(&EncodingTag::GeneralLz4)
        || column.encoding_tags.contains(&EncodingTag::GeneralZstd);
    if has_codec || column.on_disk_bytes < MIN_GENERAL_COMPRESSION_BYTES {
        return None;
    }

    let mut candidate_scores = CANDIDATES
        .into_iter()
        .map(|candidate| score_candidate(column, candidate, options))
        .collect::<Vec<_>>();
    candidate_scores.sort_by(|left, right| {
        left.effective_score
            .partial_cmp(&right.effective_score)
            .unwrap_or(Ordering::Equal)
    });
    let winner = &candidate_scores[0];
    let (lower_percent, upper_percent) = heuristic_savings_range(column, winner.candidate);
    let candidates = candidate_scores
        .iter()
        .map(|score| score.candidate)
        .collect::<Vec<_>>();
    let ranking_mode = if options.consider_decoding_penalty {
        "estimated size plus decoding penalty"
    } else {
        "estimated size only"
    };

    Some(Suggestion {
        rule: "general-compression-opportunity".into(),
        severity: Severity::Suggestion,
        location: column_location(column),
        message: format!(
            "General compression is not observed. Probe each (algorithm, level) candidate; current winner by {ranking_mode} is {}.",
            candidate_name(winner.candidate)
        ),
        action: Action::ProbeEncodings { candidates },
        estimate: Some(SavingsEstimate {
            basis: EstimateBasis::EncodingHeuristic,
            confidence: Confidence::Low,
            lower_bytes: percent_of(column.on_disk_bytes, lower_percent),
            upper_bytes: percent_of(column.on_disk_bytes, upper_percent),
            lower_percent,
            upper_percent,
            caveat: "Ranges and decode penalties are heuristics; validate every candidate by bounded re-encoding and decode benchmarks.".into(),
        }),
        candidate_scores,
        evidence: vec![
            format!("{} on-disk bytes", column.on_disk_bytes),
            format!(
                "existing encodings: {}",
                existing_encoding_summary(column)
            ),
            format!("ranking mode: {ranking_mode}"),
        ],
    })
}

fn score_candidate(
    column: &ColumnProfile,
    candidate: EncodingCandidate,
    options: AnalyzeOptions,
) -> CandidateScore {
    let (savings_lower, savings_upper) = heuristic_savings_range(column, candidate);
    let estimated_bytes_lower = remaining_bytes(column.on_disk_bytes, savings_upper);
    let estimated_bytes_upper = remaining_bytes(column.on_disk_bytes, savings_lower);
    let consideration_factor = if is_already_compact(column) { 1.2 } else { 1.0 };
    let decoding_penalty = if options.consider_decoding_penalty {
        decode_penalty(candidate)
    } else {
        1.0
    };
    let midpoint = (estimated_bytes_lower as f64 + estimated_bytes_upper as f64) / 2.0;

    CandidateScore {
        candidate,
        estimated_bytes_lower,
        estimated_bytes_upper,
        consideration_factor,
        decoding_penalty,
        effective_score: midpoint * consideration_factor * decoding_penalty,
    }
}

fn heuristic_savings_range(column: &ColumnProfile, candidate: EncodingCandidate) -> (u8, u8) {
    let text_like = is_text_like(&column.data_type);
    let compact = is_already_compact(column);
    match (candidate.algorithm, candidate.level, text_like, compact) {
        (CompressionAlgorithm::Lz4, _, _, false) => (5, 30),
        (CompressionAlgorithm::Lz4, _, _, true) => (2, 15),
        (CompressionAlgorithm::Zstd, Some(1), true, false) => (20, 55),
        (CompressionAlgorithm::Zstd, Some(3), true, false) => (25, 65),
        (CompressionAlgorithm::Zstd, Some(9), true, false) => (30, 72),
        (CompressionAlgorithm::Zstd, Some(1), false, false) => (8, 35),
        (CompressionAlgorithm::Zstd, Some(3), false, false) => (10, 45),
        (CompressionAlgorithm::Zstd, Some(9), false, false) => (15, 52),
        (CompressionAlgorithm::Zstd, Some(1), _, true) => (2, 18),
        (CompressionAlgorithm::Zstd, Some(3), _, true) => (3, 25),
        (CompressionAlgorithm::Zstd, Some(9), _, true) => (5, 30),
        (CompressionAlgorithm::Zstd, None, _, _) => (3, 25),
        (CompressionAlgorithm::Zstd, Some(_), _, _) => (3, 25),
    }
}

fn decode_penalty(candidate: EncodingCandidate) -> f64 {
    match (candidate.algorithm, candidate.level) {
        (CompressionAlgorithm::Lz4, _) => 1.0,
        (CompressionAlgorithm::Zstd, Some(level)) if level <= 1 => 1.15,
        (CompressionAlgorithm::Zstd, Some(level)) if level <= 3 => 1.25,
        (CompressionAlgorithm::Zstd, _) => 1.75,
    }
}

fn is_already_compact(column: &ColumnProfile) -> bool {
    [
        EncodingTag::Dictionary,
        EncodingTag::Rle,
        EncodingTag::BitPacked,
        EncodingTag::Fsst,
        EncodingTag::Constant,
        EncodingTag::Delta,
        EncodingTag::ByteStreamSplit,
        EncodingTag::PackedStruct,
    ]
    .iter()
    .any(|tag| column.encoding_tags.contains(tag))
}

fn is_text_like(data_type: &str) -> bool {
    let data_type = data_type.to_ascii_lowercase();
    ["utf8", "string", "binary", "largeutf8", "largebinary"]
        .iter()
        .any(|needle| data_type.contains(needle))
}

fn candidate_name(candidate: EncodingCandidate) -> String {
    match candidate.level {
        Some(level) => format!("{:?}:{level}", candidate.algorithm).to_lowercase(),
        None => format!("{:?}", candidate.algorithm).to_lowercase(),
    }
}

fn column_location(column: &ColumnProfile) -> Location {
    Location::Column {
        index: column.index,
        path: column.path.clone(),
    }
}

fn percent_of(bytes: u64, percent: u8) -> u64 {
    bytes.saturating_mul(u64::from(percent)) / 100
}

fn remaining_bytes(bytes: u64, savings_percent: u8) -> u64 {
    bytes.saturating_sub(percent_of(bytes, savings_percent))
}

fn existing_encoding_summary(column: &ColumnProfile) -> String {
    column
        .encoding_tags
        .iter()
        .filter(|tag| **tag != EncodingTag::Unknown)
        .map(|tag| format!("{tag:?}").to_lowercase())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::model::REPORT_SCHEMA_VERSION;

    fn report(data_type: &str) -> ProbeReport {
        let bytes = 100 * 1024 * 1024;
        ProbeReport {
            schema_version: REPORT_SCHEMA_VERSION,
            source: "test.lance".into(),
            file_version: "2.3".into(),
            file_size_bytes: bytes,
            data_bytes: bytes,
            rows: 100,
            columns: vec![ColumnProfile {
                index: 0,
                path: "value".into(),
                data_type: data_type.into(),
                pages: 1,
                on_disk_bytes: bytes,
                field_metadata: BTreeMap::new(),
                encoding_tags: BTreeSet::from([
                    EncodingTag::GeneralUncompressed,
                    EncodingTag::Flat,
                ]),
                raw_page_encodings: vec![],
            }],
        }
    }

    #[test]
    fn each_algorithm_level_is_a_distinct_candidate() {
        let suggestions = check(&report("Utf8"), AnalyzeOptions::default());
        let candidates = &suggestions[0].candidate_scores;
        assert_eq!(candidates.len(), 4);
        assert!(candidates.iter().any(|score| {
            score.candidate.algorithm == CompressionAlgorithm::Zstd
                && score.candidate.level == Some(9)
        }));
        assert_eq!(candidates[0].candidate.level, Some(9));
    }

    #[test]
    fn decoding_penalty_changes_fixed_width_winner_to_lz4() {
        let suggestions = check(
            &report("Float32"),
            AnalyzeOptions {
                consider_decoding_penalty: true,
            },
        );
        assert_eq!(
            suggestions[0].candidate_scores[0].candidate.algorithm,
            CompressionAlgorithm::Lz4
        );
        assert_eq!(suggestions[0].candidate_scores[0].decoding_penalty, 1.0);
    }

    #[test]
    fn penalties_are_disabled_by_default() {
        let suggestions = check(&report("Utf8"), AnalyzeOptions::default());
        assert!(
            suggestions[0]
                .candidate_scores
                .iter()
                .all(|score| score.decoding_penalty == 1.0)
        );
    }

    #[test]
    fn compact_base_encoding_gets_duckdb_style_consideration_factor() {
        let mut probe = report("Utf8");
        probe.columns[0]
            .encoding_tags
            .insert(EncodingTag::Dictionary);
        let suggestions = check(&probe, AnalyzeOptions::default());
        assert!(
            suggestions[0]
                .candidate_scores
                .iter()
                .all(|score| score.consideration_factor == 1.2)
        );
    }
}
