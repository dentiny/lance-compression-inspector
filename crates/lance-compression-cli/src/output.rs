use lance_compression_estimation::{
    Action, AnalysisReport, DatasetAnalysisReport, Location, Suggestion,
};

pub fn print_human(report: &DatasetAnalysisReport, verbose: bool) {
    println!(
        "{}: branch {}, version {}, {} rows, {} data files",
        report.source,
        report.branch,
        report.manifest_version,
        report.physical_rows,
        report.files.len()
    );

    for file in &report.files {
        print_file(file, verbose);
    }
}

fn print_file(report: &AnalysisReport, verbose: bool) {
    println!(
        "\n{}: {}, format {}, {} columns",
        report.probe.source,
        human_bytes(report.probe.file_size_bytes),
        report.probe.file_version,
        report.probe.columns.len()
    );
    if verbose && !report.probe.unsupported_nested_targets.is_empty() {
        println!(
            "  nested targets not measured independently: {}",
            report.probe.unsupported_nested_targets.join(", ")
        );
    }
    if verbose {
        for column in &report.probe.columns {
            println!("    source encodings for {}:", column.path);
            print_encodings(&column.raw_page_encodings);
        }
    }
    let visible = report
        .suggestions
        .iter()
        .filter(|suggestion| {
            verbose || visible_by_default(suggestion, report.probe.file_size_bytes)
        })
        .collect::<Vec<_>>();

    if visible.is_empty() {
        println!("  No recommended changes.");
        return;
    }
    print_format_table(&visible);
    for suggestion in visible.iter().copied().filter(|suggestion| {
        suggestion.candidate_scores.is_empty()
            && !matches!(suggestion.action, Action::RewriteFileVersion { .. })
    }) {
        print_non_plan_conclusion(suggestion);
    }
    print_plan_table(report, &visible);
    if verbose {
        for suggestion in visible {
            print_details(suggestion);
        }
    }
}

fn visible_by_default(suggestion: &Suggestion, file_bytes: u64) -> bool {
    suggestion.candidate_scores.first().map_or_else(
        || {
            !suggestion
                .estimate
                .as_ref()
                .is_some_and(|estimate| estimate.lower_bytes == 0)
        },
        |winner| winner.projected_file_bytes < file_bytes,
    )
}

fn print_non_plan_conclusion(suggestion: &Suggestion) {
    let location = match &suggestion.location {
        Location::File => "<file>",
        Location::Column { path, .. } => path,
    };
    let severity = format!("{:?}", suggestion.severity).to_uppercase();

    println!("  [{severity}] {location}: {}", suggestion.message);
}

fn print_format_table(suggestions: &[&Suggestion]) {
    let headers = [
        "TARGET",
        "STATUS",
        "PROJECTED FILE",
        "GAIN / COST",
        "CAPABILITIES",
    ];
    let rows = suggestions
        .iter()
        .filter_map(|suggestion| {
            let Action::RewriteFileVersion {
                target,
                capabilities,
                projected_file_bytes,
                size_delta_bytes,
            } = &suggestion.action
            else {
                return None;
            };
            let delta = size_delta_bytes.map_or_else(
                || "unavailable".into(),
                |delta| {
                    if delta >= 0 {
                        format!("gain {}", human_bytes(delta.unsigned_abs()))
                    } else {
                        format!("cost {}", human_bytes(delta.unsigned_abs()))
                    }
                },
            );
            Some(vec![
                target.clone(),
                format!("{:?}", suggestion.severity).to_uppercase(),
                projected_file_bytes.map_or_else(|| "-".into(), human_bytes),
                delta,
                capabilities.join(", "),
            ])
        })
        .collect::<Vec<_>>();
    print_table("FORMAT CHANGES", &headers, &rows);
}

fn print_plan_table(report: &AnalysisReport, suggestions: &[&Suggestion]) {
    let headers = [
        "COLUMN",
        "CURRENT ENCODING",
        "SUGGESTED ENCODING",
        "FORMAT",
        "PROJECTED FILE",
        "SAVINGS",
    ];
    let rows = suggestions
        .iter()
        .filter_map(|suggestion| {
            let winner = suggestion.candidate_scores.first()?;
            let Location::Column { path, .. } = &suggestion.location else {
                return None;
            };
            let column = report
                .probe
                .columns
                .iter()
                .find(|column| column.path == *path)?;
            Some(vec![
                path.clone(),
                crate::encoding_display::summarize(&column.raw_page_encodings, None),
                crate::encoding_display::summarize(
                    &winner.resolved_page_encodings,
                    Some(winner.plan),
                ),
                format!(
                    "{} → {}",
                    report.probe.file_version, winner.plan.file_version
                ),
                human_bytes(winner.projected_file_bytes),
                file_savings(report.probe.file_size_bytes, winner.projected_file_bytes),
            ])
        })
        .collect::<Vec<_>>();
    print_table("ENCODING CHANGES", &headers, &rows);
}

// Compare with the original file, including the format migration cost.
fn file_savings(current: u64, projected: u64) -> String {
    let delta = current.abs_diff(projected);
    let percent = u128::from(delta) * 100 / u128::from(current.max(1));
    let percent = if delta > 0 && percent == 0 {
        "<1".into()
    } else {
        percent.to_string()
    };
    let sign = if projected > current { "-" } else { "" };
    format!("{sign}{} ({sign}{percent}%)", human_bytes(delta))
}

fn print_encodings(encodings: &[String]) {
    if encodings.is_empty() {
        println!("      unavailable");
    }
    for (index, encoding) in encodings.iter().enumerate() {
        println!("      encoding {}:", index + 1);
        for line in encoding.lines() {
            println!("        {line}");
        }
    }
}

fn print_table(title: &str, headers: &[&str], rows: &[Vec<String>]) {
    if rows.is_empty() {
        return;
    }
    let widths = headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            rows.iter()
                .filter_map(|row| row.get(index))
                .map(|value| value.chars().count())
                .max()
                .unwrap_or(0)
                .max(header.chars().count())
        })
        .collect::<Vec<_>>();
    let separator = widths
        .iter()
        .map(|width| "-".repeat(*width))
        .collect::<Vec<_>>()
        .join("-+-");
    let print_row = |row: &[&str]| {
        println!(
            "  {}",
            row.iter()
                .zip(&widths)
                .map(|(value, width)| format!("{value:<width$}"))
                .collect::<Vec<_>>()
                .join(" | ")
        )
    };
    println!("\n  {title}\n");
    print_row(headers);
    println!("  {separator}");
    for row in rows {
        print_row(&row.iter().map(String::as_str).collect::<Vec<_>>());
    }
}

fn print_details(suggestion: &Suggestion) {
    println!("    rule: {}", suggestion.rule);
    if let Action::ProbeEncodingPlans { plans } = &suggestion.action {
        println!(
            "    plans: {}",
            plans
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    for score in &suggestion.candidate_scores {
        println!(
            "    score {}: sample={}B/{} rows, column={}, file={}, decode={:.2}, effective={:.0}",
            score.plan,
            score.encoded_bytes,
            score.sample_rows,
            human_bytes(score.projected_column_bytes),
            human_bytes(score.projected_file_bytes),
            score.decoding_penalty,
            score.effective_score
        );
        print_encodings(&score.resolved_page_encodings);
    }
    if let Some(estimate) = &suggestion.estimate {
        println!("    caveat: {}", estimate.caveat);
    }
    for evidence in &suggestion.evidence {
        println!("    evidence: {evidence}");
    }
}

fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes / KIB)
    } else {
        format!("{bytes:.0} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_sub_percent_suggestions_are_visible_without_verbose() {
        let suggestion: Suggestion = serde_json::from_value(serde_json::json!({
            "rule": "encoding-plan-opportunity", "severity": "suggestion",
            "location": {"kind": "column", "index": 0, "path": "value"},
            "message": "small improvement", "action": {"kind": "probe-encoding-plans", "plans": []},
            "estimate": {"basis": "measured-probe", "lower_bytes": 1, "upper_bytes": 1,
                "lower_percent": 0, "upper_percent": 0, "caveat": "sample"},
            "candidate_scores": [{"plan": {"structural": "auto", "value": "auto",
                "general": {"kind": "lz4"}, "file_version": "2.2"}, "sample_rows": 100,
                "encoded_bytes": 99, "resolved_page_encodings": [], "projected_column_bytes": 99,
                "projected_file_bytes": 99999, "decoding_penalty": 1.0, "effective_score": 99.0}],
            "evidence": []
        }))
        .unwrap();
        assert!(visible_by_default(&suggestion, 100000));
        assert_eq!(file_savings(100000, 99999), "1 B (<1%)");
        assert!(!visible_by_default(&suggestion, 99999));
        assert!(!visible_by_default(&suggestion, 99998));
    }
}
