use std::cmp::Ordering;

use crate::model::{
    Action, AnalyzeOptions, CandidateScore, ColumnProfile, CompressionAlgorithm,
    CompressionMeasurement, Confidence, DEFAULT_ENCODING_CANDIDATES, EncodingCandidate,
    EncodingTag, EstimateBasis, Location, ProbeReport, SavingsEstimate, Severity, Suggestion,
};

/// Evaluate measured general-compression candidates for every physical column
/// in a probed Lance data file.
pub(crate) fn check(probe: &ProbeReport, options: AnalyzeOptions) -> Vec<Suggestion> {
    probe
        .columns
        .iter()
        .filter_map(|column| check_column(column, options))
        .collect()
}

/// Compare one column's bounded re-encoding measurements and return a
/// recommendation only when another (algorithm, level) candidate wins.
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
    // Blob pages only contain (position, size) descriptors; their external
    // payload bytes are not re-encoded by Lance's general-compression setting.
    if column.encoding_tags.contains(&EncodingTag::StructuralBlob) {
        return None;
    }

    if column.compression_measurements.is_empty() {
        return Some(Suggestion {
            rule: "compression-probe-unavailable".into(),
            severity: Severity::Info,
            location: column_location(column),
            message: "No bounded re-encoding measurements are available for this column.".into(),
            action: Action::ProbeEncodings {
                candidates: DEFAULT_ENCODING_CANDIDATES.to_vec(),
            },
            estimate: None,
            candidate_scores: vec![],
            evidence: vec!["run the probe with --sample-rows greater than zero".into()],
        });
    }

    let mut candidate_scores = column
        .compression_measurements
        .iter()
        .map(|measurement| score_candidate(column, measurement, options))
        .collect::<Vec<_>>();
    candidate_scores.sort_by(|left, right| {
        left.effective_score
            .partial_cmp(&right.effective_score)
            .unwrap_or(Ordering::Equal)
    });
    let winner = candidate_scores[0].candidate;
    let current = current_candidates(column);
    if current.len() == 1 && current.contains(&winner) {
        return None;
    }

    let baseline = column
        .compression_measurements
        .iter()
        .find(|measurement| measurement.candidate.algorithm == CompressionAlgorithm::Uncompressed);
    let winner_measurement = column
        .compression_measurements
        .iter()
        .find(|measurement| measurement.candidate == winner)?;
    let estimate = baseline.map(|baseline| {
        let saved = baseline
            .encoded_bytes
            .saturating_sub(winner_measurement.encoded_bytes);
        let percent = saved
            .saturating_mul(100)
            .checked_div(baseline.encoded_bytes)
            .unwrap_or(0)
            .min(100) as u8;
        SavingsEstimate {
            basis: EstimateBasis::MeasuredProbe,
            confidence: Confidence::Medium,
            lower_bytes: saved,
            upper_bytes: saved,
            lower_percent: percent,
            upper_percent: percent,
            caveat: format!(
                "Measured on {} sampled rows; distributions may differ across the remaining rows.",
                winner_measurement.sample_rows
            ),
        }
    });
    let ranking_mode = if options.consider_decoding_penalty {
        "measured sample size plus decoding penalty"
    } else {
        "measured sample size"
    };
    let current_label = if current.is_empty() {
        "uncompressed".into()
    } else {
        current
            .iter()
            .map(|candidate| candidate_name(*candidate))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let current_is_uncompressed =
        current.len() == 1 && current[0].algorithm == CompressionAlgorithm::Uncompressed;
    let candidates = candidate_scores
        .iter()
        .map(|score| score.candidate)
        .collect();

    Some(Suggestion {
        rule: if current.is_empty() || current_is_uncompressed {
            "general-compression-opportunity"
        } else {
            "general-compression-tuning"
        }
        .into(),
        severity: Severity::Suggestion,
        location: column_location(column),
        message: format!(
            "Current compression is {current_label}; {} wins by {ranking_mode}.",
            candidate_name(winner)
        ),
        action: Action::ProbeEncodings { candidates },
        estimate,
        candidate_scores,
        evidence: vec![
            format!("{} current on-disk bytes", column.on_disk_bytes),
            format!(
                "{} rows were independently re-encoded for each candidate",
                winner_measurement.sample_rows
            ),
        ],
    })
}

/// Convert one measured sample size into a comparable score. The raw encoded
/// bytes remain unchanged; optional policy factors only affect candidate
/// ranking and are reported separately.
fn score_candidate(
    column: &ColumnProfile,
    measurement: &CompressionMeasurement,
    options: AnalyzeOptions,
) -> CandidateScore {
    let consideration_factor = consideration_factor(column, measurement.candidate);
    let decoding_penalty = if options.consider_decoding_penalty {
        decode_penalty(measurement.candidate)
    } else {
        1.0
    };
    CandidateScore {
        candidate: measurement.candidate,
        sample_rows: measurement.sample_rows,
        encoded_bytes: measurement.encoded_bytes,
        consideration_factor,
        decoding_penalty,
        effective_score: measurement.encoded_bytes as f64 * consideration_factor * decoding_penalty,
    }
}

fn current_candidates(column: &ColumnProfile) -> Vec<EncodingCandidate> {
    if !column.observed_compressions.is_empty() {
        return column.observed_compressions.iter().copied().collect();
    }
    if column
        .encoding_tags
        .contains(&EncodingTag::GeneralUncompressed)
    {
        return vec![EncodingCandidate {
            algorithm: CompressionAlgorithm::Uncompressed,
            level: None,
        }];
    }
    vec![]
}

fn consideration_factor(column: &ColumnProfile, candidate: EncodingCandidate) -> f64 {
    let compact = [
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
    .any(|tag| column.encoding_tags.contains(tag));
    if compact && candidate.algorithm != CompressionAlgorithm::Uncompressed {
        1.2
    } else {
        1.0
    }
}

fn decode_penalty(candidate: EncodingCandidate) -> f64 {
    match (candidate.algorithm, candidate.level) {
        (CompressionAlgorithm::Uncompressed | CompressionAlgorithm::Lz4, _) => 1.0,
        (CompressionAlgorithm::Zstd, Some(level)) if level <= 1 => 1.15,
        (CompressionAlgorithm::Zstd, Some(level)) if level <= 3 => 1.25,
        (CompressionAlgorithm::Zstd, Some(level)) if level <= 6 => 1.45,
        (CompressionAlgorithm::Zstd, Some(level)) if level <= 9 => 1.75,
        (CompressionAlgorithm::Zstd, _) => 2.0,
    }
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    fn report(current: EncodingCandidate) -> ProbeReport {
        let measurements = DEFAULT_ENCODING_CANDIDATES
            .iter()
            .enumerate()
            .map(|(index, candidate)| CompressionMeasurement {
                candidate: *candidate,
                sample_rows: 1_024,
                encoded_bytes: [100, 80, 70, 60, 55, 52, 50][index],
            })
            .collect();
        ProbeReport {
            source: "test.lance".into(),
            file_version: "2.3".into(),
            file_size_bytes: 100,
            data_bytes: 100,
            rows: 1_024,
            columns: vec![ColumnProfile {
                index: 0,
                path: "value".into(),
                data_type: "Utf8".into(),
                pages: 1,
                on_disk_bytes: 100,
                field_metadata: BTreeMap::new(),
                encoding_tags: BTreeSet::new(),
                observed_compressions: if current.algorithm == CompressionAlgorithm::Uncompressed {
                    BTreeSet::new()
                } else {
                    BTreeSet::from([current])
                },
                compression_measurements: measurements,
                raw_page_encodings: vec![],
            }],
        }
    }

    #[test]
    fn measured_bytes_select_high_compression_without_penalty() {
        let current = DEFAULT_ENCODING_CANDIDATES[0];
        let suggestions = check(&report(current), AnalyzeOptions::default());
        assert_eq!(
            suggestions[0].candidate_scores[0].candidate,
            DEFAULT_ENCODING_CANDIDATES[6]
        );
        assert_eq!(
            suggestions[0].estimate.as_ref().unwrap().basis,
            EstimateBasis::MeasuredProbe
        );
    }

    #[test]
    fn decoding_penalty_changes_the_winning_level() {
        let suggestions = check(
            &report(DEFAULT_ENCODING_CANDIDATES[0]),
            AnalyzeOptions {
                consider_decoding_penalty: true,
            },
        );
        assert_eq!(
            suggestions[0].candidate_scores[0].candidate,
            DEFAULT_ENCODING_CANDIDATES[3]
        );
    }

    #[test]
    fn existing_codec_is_still_compared() {
        let suggestions = check(
            &report(DEFAULT_ENCODING_CANDIDATES[3]),
            AnalyzeOptions::default(),
        );
        assert_eq!(suggestions[0].rule, "general-compression-tuning");
    }

    #[test]
    fn current_winner_emits_no_suggestion() {
        let suggestions = check(
            &report(DEFAULT_ENCODING_CANDIDATES[6]),
            AnalyzeOptions::default(),
        );
        assert!(suggestions.is_empty());
    }
}
