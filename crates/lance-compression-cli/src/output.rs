use lance_compression_estimation::{
    Action, AnalysisReport, DatasetAnalysisReport, EncodingFileVersion, EncodingPlan,
    GeneralCompression, Location, StructuralEncoding, ValueEncoding,
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
    if !report.probe.unsupported_nested_targets.is_empty() {
        println!(
            "  Nested child targets are not measured independently: {}",
            report.probe.unsupported_nested_targets.join(", ")
        );
    }
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
            Action::ProbeEncodingPlans { plans } => println!(
                "    plans: {}",
                plans
                    .iter()
                    .map(|plan| plan_name(*plan))
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
                "    score {}: sample {} bytes / {} rows, projected column {} bytes, projected file {} bytes, consideration {:.2}, decode {:.2}, effective {:.0}",
                plan_name(score.plan),
                score.encoded_bytes,
                score.sample_rows,
                score.projected_column_bytes,
                score.projected_file_bytes,
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

fn plan_name(plan: EncodingPlan) -> String {
    let structural = match plan.structural {
        StructuralEncoding::Auto => "auto",
        StructuralEncoding::MiniBlock => "miniblock",
        StructuralEncoding::FullZip => "fullzip",
        StructuralEncoding::Sparse => "sparse",
    };
    let value = match plan.value {
        ValueEncoding::Auto => "auto",
        ValueEncoding::Rle => "rle",
        ValueEncoding::Fsst => "fsst",
        ValueEncoding::ByteStreamSplit => "byte-stream-split",
        ValueEncoding::Dictionary => "dictionary",
        ValueEncoding::PackedStruct => "packed-struct",
    };
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
    format!("structural={structural}/value={value}/general={general}/format={version}")
}
