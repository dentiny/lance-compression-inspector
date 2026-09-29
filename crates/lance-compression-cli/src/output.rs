use lance_compression_estimation::{
    Action, AnalysisReport, DatasetAnalysisReport, EncodingCandidate, Location,
};

pub fn print_human(report: &DatasetAnalysisReport) {
    println!(
        "{}: branch {}, manifest version {}, {} fragments, {} physical rows, {} active data files",
        report.source,
        report.branch,
        report.manifest_version,
        report.fragment_count,
        report.physical_rows,
        report.files.len()
    );

    for file in &report.files {
        print_file(file);
    }
}

fn print_file(report: &AnalysisReport) {
    println!(
        "\nfile {}: {} rows, {} columns, {} bytes, format {}",
        report.probe.source,
        report.probe.rows,
        report.probe.columns.len(),
        report.probe.file_size_bytes,
        report.probe.file_version
    );
    if report.suggestions.is_empty() {
        println!("  No compression opportunities met the current thresholds.");
        return;
    }

    for suggestion in &report.suggestions {
        let location = match &suggestion.location {
            Location::File => "<file>",
            Location::Column { path, .. } => path,
        };
        println!(
            "\n  [{:?}] {}: {}",
            suggestion.severity, location, suggestion.message
        );
        println!("    rule: {}", suggestion.rule);
        match &suggestion.action {
            Action::ProbeEncodings { candidates } => println!(
                "    probe: {}",
                candidates
                    .iter()
                    .map(|candidate| candidate_name(*candidate))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Action::RewriteFileVersion {
                target,
                capabilities,
            } => {
                println!("    rewrite target: {target}");
                println!("    capabilities: {}", capabilities.join(", "));
            }
            Action::InspectUnknownEncoding => {}
        }
        for score in &suggestion.candidate_scores {
            println!(
                "    score {}: {} bytes / {} sampled rows, consideration {:.2}, decode {:.2}, effective {:.0}",
                candidate_name(score.candidate),
                score.encoded_bytes,
                score.sample_rows,
                score.consideration_factor,
                score.decoding_penalty,
                score.effective_score
            );
        }
        if let Some(estimate) = &suggestion.estimate {
            println!(
                "    winner savings: {}–{} bytes ({}–{}%, {:?} confidence)",
                estimate.lower_bytes,
                estimate.upper_bytes,
                estimate.lower_percent,
                estimate.upper_percent,
                estimate.confidence
            );
            println!("    caveat: {}", estimate.caveat);
        }
        for evidence in &suggestion.evidence {
            println!("    evidence: {evidence}");
        }
    }
}

fn candidate_name(candidate: EncodingCandidate) -> String {
    match candidate.level {
        Some(level) => format!("{:?}:{level}", candidate.algorithm).to_lowercase(),
        None => format!("{:?}", candidate.algorithm).to_lowercase(),
    }
}
