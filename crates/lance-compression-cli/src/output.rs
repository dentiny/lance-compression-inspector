use lance_compression_estimation::{
    Action, AnalysisReport, DatasetAnalysisReport, EncodingFileVersion, EncodingPlan, EncodingTag,
    GeneralCompression, Location, StructuralEncoding, Suggestion, ValueEncoding,
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
    if report.suggestions.is_empty() {
        println!("  No recommended changes.");
        return;
    }

    let visible = report
        .suggestions
        .iter()
        .filter(|suggestion| {
            verbose
                || !suggestion
                    .estimate
                    .as_ref()
                    .is_some_and(|estimate| estimate.lower_percent == 0)
        })
        .collect::<Vec<_>>();

    let format_suggestions = visible
        .iter()
        .copied()
        .filter(|suggestion| matches!(suggestion.action, Action::RewriteFileVersion { .. }))
        .collect::<Vec<_>>();
    print_format_table(&format_suggestions);
    for suggestion in visible.iter().copied().filter(|suggestion| {
        suggestion.candidate_scores.is_empty()
            && !matches!(suggestion.action, Action::RewriteFileVersion { .. })
    }) {
        print_non_plan_conclusion(suggestion);
    }
    print_plan_table(
        &visible
            .iter()
            .copied()
            .filter(|suggestion| !suggestion.candidate_scores.is_empty())
            .collect::<Vec<_>>(),
    );
    if verbose {
        for suggestion in visible {
            print_details(suggestion);
        }
    }
}

fn print_non_plan_conclusion(suggestion: &Suggestion) {
    let location = match &suggestion.location {
        Location::File => "<file>",
        Location::Column { path, .. } => path,
    };
    let severity = format!("{:?}", suggestion.severity).to_uppercase();

    match &suggestion.action {
        Action::RewriteFileVersion { .. } => {}
        Action::ProbeEncodingPlans { .. } | Action::InspectUnknownEncoding => {
            println!("  [{severity}] {location}: {}", suggestion.message);
        }
    }
}

fn print_format_table(suggestions: &[&Suggestion]) {
    if suggestions.is_empty() {
        return;
    }
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
    println!("\n  FORMAT CHANGES");
    print_table(&headers, &rows);
}

fn print_plan_table(suggestions: &[&Suggestion]) {
    if suggestions.is_empty() {
        return;
    }
    let headers = [
        "COLUMN",
        "BASELINE ENCODING",
        "SUGGESTED ENCODING",
        "PROJECTED FILE",
        "SAVINGS",
    ];
    let rows = suggestions
        .iter()
        .filter_map(|suggestion| {
            let winner = suggestion.candidate_scores.first()?;
            let column = match &suggestion.location {
                Location::Column { path, .. } => path.clone(),
                Location::File => "<file>".into(),
            };
            let baseline = suggestion.candidate_scores.iter().find(|score| {
                score.plan.file_version == winner.plan.file_version
                    && score.plan.structural == StructuralEncoding::Auto
                    && score.plan.value == ValueEncoding::Auto
                    && score.plan.general == GeneralCompression::Baseline
            })?;
            let current_encoding = resolved_encoding(
                &baseline.resolved_encoding_tags,
                resolved_general(baseline.plan, &baseline.resolved_encoding_tags),
            );
            let suggested_encoding = resolved_encoding(
                &winner.resolved_encoding_tags,
                resolved_general(winner.plan, &winner.resolved_encoding_tags),
            );
            let savings = suggestion
                .estimate
                .as_ref()
                .map(|estimate| {
                    format!(
                        "{} ({}%)",
                        human_bytes(estimate.lower_bytes),
                        estimate.lower_percent
                    )
                })
                .unwrap_or_else(|| "-".into());
            Some(vec![
                column,
                current_encoding,
                suggested_encoding,
                human_bytes(winner.projected_file_bytes),
                savings,
            ])
        })
        .collect::<Vec<_>>();
    let target_format = suggestions
        .first()
        .and_then(|suggestion| suggestion.candidate_scores.first())
        .map(|winner| plan_parts(winner.plan).3)
        .unwrap_or("unknown");
    println!("\n  ENCODING CHANGES (evaluated within format {target_format})");
    print_table(&headers, &rows);
}

fn resolved_encoding(tags: &std::collections::BTreeSet<EncodingTag>, general: String) -> String {
    format!(
        "{} / {} / {general}",
        resolved_structural(tags),
        resolved_value(tags)
    )
}

fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let widths = headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            rows.iter()
                .filter_map(|row| row.get(index))
                .map(String::len)
                .max()
                .unwrap_or(0)
                .max(header.len())
        })
        .collect::<Vec<_>>();
    let separator = widths
        .iter()
        .map(|width| "-".repeat(*width))
        .collect::<Vec<_>>()
        .join("-+-");
    println!();
    println!(
        "  {}",
        headers
            .iter()
            .enumerate()
            .map(|(index, value)| format!("{value:<width$}", width = widths[index]))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    println!("  {separator}");
    for row in rows {
        println!(
            "  {}",
            row.iter()
                .enumerate()
                .map(|(index, value)| format!("{value:<width$}", width = widths[index]))
                .collect::<Vec<_>>()
                .join(" | ")
        );
    }
}

fn print_details(suggestion: &Suggestion) {
    println!("    rule: {}", suggestion.rule);
    if let Action::ProbeEncodingPlans { plans } = &suggestion.action {
        println!(
            "    plans: {}",
            plans
                .iter()
                .map(|plan| plan_name(*plan))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    for score in &suggestion.candidate_scores {
        println!(
            "    score {}: sample={}B/{} rows, column={}, file={}, factor={:.2}, decode={:.2}, effective={:.0}",
            plan_name(score.plan),
            score.encoded_bytes,
            score.sample_rows,
            human_bytes(score.projected_column_bytes),
            human_bytes(score.projected_file_bytes),
            score.consideration_factor,
            score.decoding_penalty,
            score.effective_score
        );
    }
    if let Some(estimate) = &suggestion.estimate {
        println!("    caveat: {}", estimate.caveat);
    }
    for evidence in &suggestion.evidence {
        println!("    evidence: {evidence}");
    }
}

fn plan_name(plan: EncodingPlan) -> String {
    let (structural, value, general, version) = plan_parts(plan);
    format!("{structural}/{value}/{general}/v{version}")
}

fn plan_parts(plan: EncodingPlan) -> (&'static str, &'static str, String, &'static str) {
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
        ValueEncoding::ByteStreamSplit => "bss",
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
        EncodingFileVersion::V1 => "0.1",
        EncodingFileVersion::V2_0 => "2.0",
        EncodingFileVersion::V2_1 => "2.1",
        EncodingFileVersion::V2_2 => "2.2",
        EncodingFileVersion::V2_3 => "2.3",
    };
    (structural, value, general, version)
}

fn resolved_structural(tags: &std::collections::BTreeSet<EncodingTag>) -> String {
    if tags.contains(&EncodingTag::StructuralSparse) {
        "sparse"
    } else if tags.contains(&EncodingTag::StructuralMiniBlock) {
        "miniblock"
    } else if tags.contains(&EncodingTag::StructuralFullZip) {
        "fullzip"
    } else if tags.contains(&EncodingTag::StructuralBlob) {
        "blob"
    } else {
        "unknown"
    }
    .into()
}

fn resolved_value(tags: &std::collections::BTreeSet<EncodingTag>) -> String {
    let ordered = [
        (EncodingTag::Dictionary, "dictionary"),
        (EncodingTag::Rle, "rle"),
        (EncodingTag::ByteStreamSplit, "bss"),
        (EncodingTag::BitPacked, "bitpacked"),
        (EncodingTag::Fsst, "fsst"),
        (EncodingTag::PackedStruct, "packed-struct"),
        (EncodingTag::Constant, "constant"),
        (EncodingTag::FixedSizeList, "fixed-size-list"),
        (EncodingTag::VariableWidth, "variable"),
        (EncodingTag::Binary, "binary"),
        (EncodingTag::Flat, "flat"),
    ];
    let values = ordered
        .iter()
        .filter(|(tag, _)| tags.contains(tag))
        .map(|(_, name)| *name)
        .collect::<Vec<_>>();
    if values.is_empty() {
        "unknown".into()
    } else {
        values.join("+")
    }
}

fn resolved_general(plan: EncodingPlan, tags: &std::collections::BTreeSet<EncodingTag>) -> String {
    if tags.contains(&EncodingTag::GeneralZstd) {
        match plan.general {
            GeneralCompression::Zstd { level } => format!("zstd:{level}"),
            _ => "zstd".into(),
        }
    } else if tags.contains(&EncodingTag::GeneralLz4) {
        "lz4".into()
    } else {
        "none".into()
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
