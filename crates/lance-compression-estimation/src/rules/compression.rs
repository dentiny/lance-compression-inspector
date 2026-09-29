use std::cmp::Ordering;

use crate::model::{
    Action, AnalyzeOptions, CandidateScore, ColumnProfile, Confidence, EncodingFileVersion,
    EncodingMeasurement, EncodingPlan, EncodingTag, EstimateBasis, GeneralCompression, Location,
    ProbeReport, SavingsEstimate, Severity, StructuralEncoding, Suggestion, ValueEncoding,
};

/// Evaluate measured structural, value, and general encoding plans for every
/// top-level logical column in a probed Lance data file.
pub(crate) fn check(probe: &ProbeReport, options: AnalyzeOptions) -> Vec<Suggestion> {
    probe
        .columns
        .iter()
        .filter_map(|column| check_column(probe, column, options))
        .collect()
}

/// Compare one column's bounded re-encoding measurements and return a
/// recommendation only when a non-baseline plan wins.
fn check_column(
    probe: &ProbeReport,
    column: &ColumnProfile,
    options: AnalyzeOptions,
) -> Option<Suggestion> {
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

    if column.encoding_measurements.is_empty() {
        return Some(Suggestion {
            rule: "encoding-probe-unavailable".into(),
            severity: Severity::Info,
            location: column_location(column),
            message: "No bounded re-encoding measurements are available for this column.".into(),
            action: Action::ProbeEncodingPlans { plans: vec![] },
            estimate: None,
            candidate_scores: vec![],
            evidence: vec!["run the probe with --sample-rows greater than zero".into()],
        });
    }

    let current_measurement = column
        .encoding_measurements
        .iter()
        .find(|measurement| is_baseline(measurement.plan))?;

    let mut candidate_scores = column
        .encoding_measurements
        .iter()
        .map(|measurement| {
            score_candidate(probe, column, current_measurement, measurement, options)
        })
        .collect::<Vec<_>>();
    candidate_scores.sort_by(|left, right| {
        left.effective_score
            .partial_cmp(&right.effective_score)
            .unwrap_or(Ordering::Equal)
    });
    let winner = candidate_scores[0].plan;
    if current_measurement.plan == winner {
        return None;
    }

    let winner_score = candidate_scores[0].clone();
    let estimate = {
        let saved = probe
            .file_size_bytes
            .saturating_sub(winner_score.projected_file_bytes);
        let percent = saved
            .saturating_mul(100)
            .checked_div(probe.file_size_bytes)
            .unwrap_or(0)
            .min(100) as u8;
        Some(SavingsEstimate {
            basis: EstimateBasis::MeasuredProbe,
            confidence: Confidence::Medium,
            lower_bytes: saved,
            upper_bytes: saved,
            lower_percent: percent,
            upper_percent: percent,
            caveat: format!(
                "Projected from {} sampled rows using the current encoding as baseline; distributions and fixed overhead may differ across the remaining rows.",
                winner_score.sample_rows
            ),
        })
    };
    let ranking_mode = if options.consider_decoding_penalty {
        "projected column size plus decoding penalty"
    } else {
        "projected column size"
    };
    let plans = candidate_scores.iter().map(|score| score.plan).collect();

    Some(Suggestion {
        rule: "encoding-plan-opportunity".into(),
        severity: Severity::Suggestion,
        location: column_location(column),
        message: format!(
            "{} wins over the metadata-preserving baseline by {ranking_mode}.",
            plan_name(winner)
        ),
        action: Action::ProbeEncodingPlans { plans },
        estimate,
        candidate_scores,
        evidence: vec![
            format!("{} current on-disk bytes", column.on_disk_bytes),
            format!("{} current file bytes", probe.file_size_bytes),
            format!(
                "{} rows were independently re-encoded for each candidate",
                winner_score.sample_rows
            ),
        ],
    })
}

/// Project one candidate from its sampled size to full column and file sizes.
/// The current encoding's sample-to-full ratio is the baseline; optional
/// policy factors affect ranking only and remain separately visible.
fn score_candidate(
    probe: &ProbeReport,
    column: &ColumnProfile,
    current: &EncodingMeasurement,
    measurement: &EncodingMeasurement,
    options: AnalyzeOptions,
) -> CandidateScore {
    let projected_column_bytes = project_bytes(
        column.on_disk_bytes,
        measurement.encoded_bytes,
        current.encoded_bytes,
    );
    let projected_file_bytes = probe
        .file_size_bytes
        .saturating_sub(column.on_disk_bytes)
        .saturating_add(projected_column_bytes);
    let consideration_factor = consideration_factor(column, measurement.plan);
    let decoding_penalty = if options.consider_decoding_penalty {
        decode_penalty(measurement.plan)
    } else {
        1.0
    };
    CandidateScore {
        plan: measurement.plan,
        sample_rows: measurement.sample_rows,
        encoded_bytes: measurement.encoded_bytes,
        projected_column_bytes,
        projected_file_bytes,
        consideration_factor,
        decoding_penalty,
        effective_score: projected_column_bytes as f64 * consideration_factor * decoding_penalty,
    }
}

fn project_bytes(full_bytes: u64, candidate_sample_bytes: u64, current_sample_bytes: u64) -> u64 {
    if current_sample_bytes == 0 {
        return full_bytes;
    }
    let projected = u128::from(full_bytes)
        .saturating_mul(u128::from(candidate_sample_bytes))
        .checked_div(u128::from(current_sample_bytes))
        .unwrap_or(u128::from(full_bytes));
    projected.min(u128::from(u64::MAX)) as u64
}

fn is_baseline(plan: EncodingPlan) -> bool {
    plan.structural == StructuralEncoding::Auto
        && plan.value == ValueEncoding::Auto
        && plan.general == GeneralCompression::Baseline
}

fn consideration_factor(column: &ColumnProfile, plan: EncodingPlan) -> f64 {
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
    if compact
        && !matches!(
            plan.general,
            GeneralCompression::Baseline | GeneralCompression::None
        )
    {
        1.2
    } else {
        1.0
    }
}

fn decode_penalty(plan: EncodingPlan) -> f64 {
    match plan.general {
        GeneralCompression::Baseline | GeneralCompression::None | GeneralCompression::Lz4 => 1.0,
        GeneralCompression::Zstd { level } if level <= 1 => 1.15,
        GeneralCompression::Zstd { level } if level <= 3 => 1.25,
        GeneralCompression::Zstd { level } if level <= 6 => 1.45,
        GeneralCompression::Zstd { level } if level <= 9 => 1.75,
        GeneralCompression::Zstd { .. } => 2.0,
    }
}

fn plan_name(plan: EncodingPlan) -> String {
    let general = match plan.general {
        GeneralCompression::Baseline => "baseline".into(),
        GeneralCompression::None => "none".into(),
        GeneralCompression::Lz4 => "lz4".into(),
        GeneralCompression::Zstd { level } => format!("zstd:{level}"),
    };
    let version = match plan.file_version {
        EncodingFileVersion::V2_2 => "2.2",
        EncodingFileVersion::V2_3 => "2.3",
    };
    format!(
        "structural={}, value={}, general={general}, format={version}",
        format!("{:?}", plan.structural).to_lowercase(),
        format!("{:?}", plan.value).to_lowercase()
    )
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
    fn report(baseline_bytes: u64) -> ProbeReport {
        let baseline = EncodingPlan::baseline(EncodingFileVersion::V2_3);
        let plans = [
            baseline,
            EncodingPlan {
                general: GeneralCompression::Lz4,
                ..baseline
            },
            EncodingPlan {
                general: GeneralCompression::Zstd { level: 1 },
                ..baseline
            },
            EncodingPlan {
                general: GeneralCompression::Zstd { level: 3 },
                ..baseline
            },
            EncodingPlan {
                general: GeneralCompression::Zstd { level: 6 },
                ..baseline
            },
            EncodingPlan {
                general: GeneralCompression::Zstd { level: 9 },
                ..baseline
            },
            EncodingPlan {
                general: GeneralCompression::Zstd { level: 12 },
                ..baseline
            },
        ];
        let measurements = plans
            .into_iter()
            .enumerate()
            .map(|(index, plan)| EncodingMeasurement {
                plan,
                sample_rows: 1_024,
                encoded_bytes: [baseline_bytes, 80, 70, 60, 55, 52, 50][index],
            })
            .collect();
        ProbeReport {
            source: "test.lance".into(),
            file_version: "2.3".into(),
            file_size_bytes: 100,
            data_bytes: 100,
            rows: 1_024,
            unsupported_nested_targets: vec![],
            columns: vec![ColumnProfile {
                index: 0,
                path: "value".into(),
                data_type: "Utf8".into(),
                pages: 1,
                on_disk_bytes: 100,
                field_metadata: BTreeMap::new(),
                encoding_tags: BTreeSet::from([EncodingTag::GeneralUncompressed]),
                encoding_measurements: measurements,
                raw_page_encodings: vec![],
            }],
        }
    }

    #[test]
    fn measured_bytes_select_high_compression_without_penalty() {
        let suggestions = check(&report(100), AnalyzeOptions::default());
        assert_eq!(
            suggestions[0].candidate_scores[0].plan.general,
            GeneralCompression::Zstd { level: 12 }
        );
        assert_eq!(
            suggestions[0].estimate.as_ref().unwrap().basis,
            EstimateBasis::MeasuredProbe
        );
    }

    #[test]
    fn decoding_penalty_changes_the_winning_level() {
        let suggestions = check(
            &report(100),
            AnalyzeOptions {
                consider_decoding_penalty: true,
            },
        );
        assert_eq!(
            suggestions[0].candidate_scores[0].plan.general,
            GeneralCompression::Zstd { level: 3 }
        );
    }

    #[test]
    fn projection_uses_current_sample_ratio() {
        let probe = report(50);
        let column = &probe.columns[0];
        let baseline = &column.encoding_measurements[0];
        let candidate = &column.encoding_measurements[1];
        let score = score_candidate(
            &probe,
            column,
            baseline,
            candidate,
            AnalyzeOptions::default(),
        );
        assert_eq!(score.projected_column_bytes, 160);
        assert_eq!(score.projected_file_bytes, 160);
    }

    #[test]
    fn current_winner_emits_no_suggestion() {
        let suggestions = check(&report(40), AnalyzeOptions::default());
        assert!(suggestions.is_empty());
    }

    #[test]
    fn score_identifies_full_plan() {
        let mut probe = report(100);
        probe.columns[0].encoding_measurements[1].plan.structural = StructuralEncoding::FullZip;
        let suggestions = check(&probe, AnalyzeOptions::default());
        assert!(
            suggestions[0]
                .candidate_scores
                .iter()
                .any(|score| score.plan.structural == StructuralEncoding::FullZip)
        );
    }
}
