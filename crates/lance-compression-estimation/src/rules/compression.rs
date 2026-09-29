use std::cmp::Ordering;

use crate::model::{
    Action, AnalyzeOptions, CandidateScore, ColumnProfile, EncodingFileVersion,
    EncodingMeasurement, EncodingPlan, EstimateBasis, GeneralCompression, Location, ProbeReport,
    SavingsEstimate, Severity, StructuralEncoding, Suggestion, ValueEncoding,
};

/// Evaluate measured encoding plans in top-level column order.
pub(crate) fn evaluate_encoding_plans(
    probe: &ProbeReport,
    options: AnalyzeOptions,
) -> Vec<Suggestion> {
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
    // Blob pages only contain (position, size) descriptors; their external
    // payload bytes are not re-encoded by Lance's general-compression setting.
    if column.has_blob {
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

    let source_version = probe.file_version;
    let target_version = encoding_target_version(source_version);
    let source_measurement = column.encoding_measurements.iter().find(|measurement| {
        is_baseline(measurement.plan) && measurement.plan.file_version == source_version
    })?;
    let target_baseline = column.encoding_measurements.iter().find(|measurement| {
        is_baseline(measurement.plan) && measurement.plan.file_version == target_version
    })?;
    let target_column_bytes = project_bytes(
        column.on_disk_bytes,
        target_baseline.encoded_bytes,
        source_measurement.encoded_bytes,
    );
    let target_file_bytes = projected_file_for_version(probe, source_version, target_version)?;

    let mut candidate_scores = column
        .encoding_measurements
        .iter()
        .filter(|measurement| measurement.plan.file_version == target_version)
        .map(|measurement| {
            score_candidate(
                target_baseline,
                target_column_bytes,
                target_file_bytes,
                measurement,
                options,
            )
        })
        .collect::<Vec<_>>();
    candidate_scores.sort_by(|left, right| {
        left.effective_score
            .partial_cmp(&right.effective_score)
            .unwrap_or(Ordering::Equal)
    });
    let winner = candidate_scores[0].plan;
    if target_baseline.plan == winner {
        return None;
    }

    let winner_score = candidate_scores[0].clone();
    let estimate = {
        let saved = target_file_bytes.saturating_sub(winner_score.projected_file_bytes);
        let percent = saved
            .saturating_mul(100)
            .checked_div(target_file_bytes)
            .unwrap_or(0)
            .min(100) as u8;
        Some(SavingsEstimate {
            basis: EstimateBasis::MeasuredProbe,
            lower_bytes: saved,
            upper_bytes: saved,
            lower_percent: percent,
            upper_percent: percent,
            caveat: format!(
                "Encoding-only gain projected from {} sampled rows within format {}; format migration is reported separately.",
                winner_score.sample_rows,
                version_name(target_version)
            ),
        })
    };
    let ranking_mode = if options.consider_decoding_penalty {
        "projected column size plus decode-cost factor"
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
            format!("{} format-baseline column bytes", target_column_bytes),
            format!("{} format-baseline file bytes", target_file_bytes),
            format!(
                "{} rows were independently re-encoded for each candidate",
                winner_score.sample_rows
            ),
        ],
    })
}

/// Project one candidate within a fixed target format. Format migration is
/// intentionally excluded; optional policy factors affect ranking only.
fn score_candidate(
    target_baseline: &EncodingMeasurement,
    target_column_bytes: u64,
    target_file_bytes: u64,
    measurement: &EncodingMeasurement,
    options: AnalyzeOptions,
) -> CandidateScore {
    let projected_column_bytes = project_bytes(
        target_column_bytes,
        measurement.encoded_bytes,
        target_baseline.encoded_bytes,
    );
    let projected_file_bytes = target_file_bytes
        .saturating_sub(target_column_bytes)
        .saturating_add(projected_column_bytes);
    let decoding_penalty = if options.consider_decoding_penalty {
        decode_penalty(measurement.plan)
    } else {
        1.0
    };
    CandidateScore {
        plan: measurement.plan,
        sample_rows: measurement.sample_rows,
        encoded_bytes: measurement.encoded_bytes,
        resolved_page_encodings: measurement.resolved_page_encodings.clone(),
        projected_column_bytes,
        projected_file_bytes,
        decoding_penalty,
        effective_score: projected_column_bytes as f64 * decoding_penalty,
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

fn encoding_target_version(source: EncodingFileVersion) -> EncodingFileVersion {
    match source {
        EncodingFileVersion::V1
        | EncodingFileVersion::V2_0
        | EncodingFileVersion::V2_1
        | EncodingFileVersion::V2_2 => EncodingFileVersion::V2_2,
        EncodingFileVersion::V2_3 => EncodingFileVersion::V2_3,
    }
}

fn projected_file_for_version(
    probe: &ProbeReport,
    source: EncodingFileVersion,
    target: EncodingFileVersion,
) -> Option<u64> {
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
            // Blob bytes stay unchanged; Blob columns have no candidate measurements.
            if column.has_blob {
                return Some(total.saturating_add(column.on_disk_bytes));
            }
            let source_measurement = column
                .encoding_measurements
                .iter()
                .find(|measurement| measurement.plan == EncodingPlan::baseline(source))?;
            let target_measurement = column
                .encoding_measurements
                .iter()
                .find(|measurement| measurement.plan == EncodingPlan::baseline(target))?;
            Some(total.saturating_add(project_bytes(
                column.on_disk_bytes,
                target_measurement.encoded_bytes,
                source_measurement.encoded_bytes,
            )))
        })
}

fn version_name(version: EncodingFileVersion) -> &'static str {
    match version {
        EncodingFileVersion::V1 => "0.1",
        EncodingFileVersion::V2_0 => "2.0",
        EncodingFileVersion::V2_1 => "2.1",
        EncodingFileVersion::V2_2 => "2.2",
        EncodingFileVersion::V2_3 => "2.3",
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
        EncodingFileVersion::V1 => "0.1",
        EncodingFileVersion::V2_0 => "2.0",
        EncodingFileVersion::V2_1 => "2.1",
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
    use std::collections::BTreeMap;

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
                resolved_page_encodings: vec![],
            })
            .collect();
        ProbeReport {
            source: "test.lance".into(),
            file_version: EncodingFileVersion::V2_3,
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
                has_blob: false,
                encoding_measurements: measurements,
                raw_page_encodings: vec![],
            }],
        }
    }

    #[test]
    fn measured_bytes_select_high_compression_without_penalty() {
        let suggestions = evaluate_encoding_plans(&report(100), AnalyzeOptions::default());
        assert_eq!(
            suggestions[0].candidate_scores[0].plan.general,
            GeneralCompression::Zstd { level: 12 }
        );
        assert_eq!(
            suggestions[0].estimate.as_ref().unwrap().basis,
            EstimateBasis::MeasuredProbe
        );
        assert!(
            suggestions[0]
                .candidate_scores
                .iter()
                .all(|score| score.decoding_penalty == 1.0)
        );
    }

    #[test]
    fn decoding_penalty_changes_the_winning_level() {
        let suggestions = evaluate_encoding_plans(
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
    fn blob_columns_are_not_recommended() {
        let mut probe = report(100);
        probe.columns[0].has_blob = true;
        assert!(evaluate_encoding_plans(&probe, AnalyzeOptions::default()).is_empty());
    }

    #[test]
    fn unmeasured_blob_preserves_other_column_suggestions_and_file_bytes() {
        let mut probe = report(100);
        probe.file_size_bytes = 1_000;
        let mut blob = probe.columns[0].clone();
        blob.index = 1;
        blob.path = "body".into();
        blob.has_blob = true;
        blob.on_disk_bytes = 800;
        blob.encoding_measurements.clear();
        probe.columns.push(blob);

        let suggestions = evaluate_encoding_plans(&probe, AnalyzeOptions::default());
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].location, column_location(&probe.columns[0]));
        let winner = &suggestions[0].candidate_scores[0];
        assert_eq!(winner.projected_column_bytes, 50);
        // 50 candidate bytes + 800 Blob bytes + 100 bytes of file overhead.
        assert_eq!(winner.projected_file_bytes, 950);
        assert_eq!(suggestions[0].estimate.as_ref().unwrap().lower_bytes, 50);

        // Format migration also keeps the unmeasured Blob contribution.
        probe.file_version = EncodingFileVersion::V2_2;
        let mut source_baseline = probe.columns[0].encoding_measurements[0].clone();
        source_baseline.plan = EncodingPlan::baseline(EncodingFileVersion::V2_2);
        source_baseline.encoded_bytes = 200;
        probe.columns[0].encoding_measurements.push(source_baseline);
        let upgrades = crate::rules::file_version::evaluate_file_versions(&probe);
        match &upgrades[0].action {
            Action::RewriteFileVersion {
                projected_file_bytes,
                size_delta_bytes,
                ..
            } => {
                assert_eq!(*projected_file_bytes, Some(950));
                assert_eq!(*size_delta_bytes, Some(50));
            }
            action => panic!("unexpected action: {action:?}"),
        }
    }

    #[test]
    fn native_descriptions_are_preserved_without_driving_ranking() {
        let mut probe = report(100);
        let descriptions = vec!["MiniBlockLayout(...)".into(), "FutureEncoding(...)".into()];
        for measurement in &mut probe.columns[0].encoding_measurements {
            measurement.resolved_page_encodings = descriptions.clone();
        }
        let suggestions = evaluate_encoding_plans(&probe, AnalyzeOptions::default());
        let winner = &suggestions[0].candidate_scores[0];
        assert_eq!(winner.plan.general, GeneralCompression::Zstd { level: 12 });
        assert_eq!(winner.resolved_page_encodings, descriptions);
    }

    #[test]
    fn projection_uses_current_sample_ratio() {
        let probe = report(50);
        let column = &probe.columns[0];
        let baseline = &column.encoding_measurements[0];
        let candidate = &column.encoding_measurements[1];
        let score = score_candidate(
            baseline,
            column.on_disk_bytes,
            probe.file_size_bytes,
            candidate,
            AnalyzeOptions::default(),
        );
        assert_eq!(score.projected_column_bytes, 160);
        assert_eq!(score.projected_file_bytes, 160);
    }

    #[test]
    fn current_winner_emits_no_suggestion() {
        let suggestions = evaluate_encoding_plans(&report(40), AnalyzeOptions::default());
        assert!(suggestions.is_empty());
    }

    #[test]
    fn score_identifies_full_plan() {
        let mut probe = report(100);
        probe.columns[0].encoding_measurements[1].plan.structural = StructuralEncoding::FullZip;
        let suggestions = evaluate_encoding_plans(&probe, AnalyzeOptions::default());
        assert!(
            suggestions[0]
                .candidate_scores
                .iter()
                .any(|score| score.plan.structural == StructuralEncoding::FullZip)
        );
    }
}
