//! Compact display of Lance's native encoding descriptions; never used for ranking.

use lance_compression_estimation::{
    EncodingPlan, GeneralCompression, StructuralEncoding, ValueEncoding,
};

/// Keep distinct page summaries separate instead of merging their encodings.
pub fn summarize(encodings: &[String], plan: Option<EncodingPlan>) -> String {
    let mut summaries = Vec::new();
    for encoding in encodings {
        let summary =
            summarize_page(encoding, plan).unwrap_or_else(|| "details (--verbose)".into());
        if !summaries.contains(&summary) {
            summaries.push(summary);
        }
    }
    if summaries.is_empty() {
        "unavailable".into()
    } else {
        summaries.join("; ")
    }
}

// These names come from the pinned Lance version's Debug descriptions. Unknown
// layouts or constructors fall back to the full description in --verbose.
fn summarize_page(encoding: &str, plan: Option<EncodingPlan>) -> Option<String> {
    let lines = encoding.lines().map(str::trim).collect::<Vec<_>>();
    let mut layout = None;
    let mut values = Vec::new();
    let mut codecs = Vec::new();
    let mut general = false;
    for (index, line) in lines.iter().enumerate() {
        if let Some(name) = line.strip_suffix(" {") {
            let value = match name {
                "PageLayout" | "CompressiveEncoding" | "BufferCompression" => None,
                "MiniBlockLayout" => {
                    layout = Some("miniblock");
                    None
                }
                "FullZipLayout" => {
                    layout = Some("fullzip");
                    None
                }
                "SparseLayout" => {
                    layout = Some("sparse");
                    None
                }
                "ConstantLayout" => {
                    layout = Some("constant");
                    Some("constant")
                }
                "General" => {
                    general = true;
                    None
                }
                "Flat" => Some("flat"),
                "Variable" => Some("variable"),
                "Constant" => Some("constant"),
                "InlineBitpacking" => Some("inline-bitpacking"),
                "OutOfLineBitpacking" => Some("bitpacking"),
                "Fsst" => Some("fsst"),
                "Dictionary" => Some("dictionary"),
                "Rle" => Some("rle"),
                "FixedSizeList" => Some("fixed-size-list"),
                "PackedStruct" => Some("packed-struct"),
                "VariablePackedStruct" => Some("variable-packed-struct"),
                "ByteStreamSplit" => Some("bss"),
                _ => return None,
            };
            if let Some(value) = value {
                if !values.contains(&value) {
                    values.push(value);
                }
            }
        }
        if *line == "dictionary: Some(" && !values.contains(&"dictionary") {
            values.push("dictionary");
        }
        if let Some(scheme) = line.strip_prefix("scheme: ") {
            let codec = match scheme.trim_end_matches(',') {
                "CompressionAlgorithmLz4" => "lz4".into(),
                "CompressionAlgorithmZstd" => {
                    let level = lines[index + 1..]
                        .iter()
                        .take_while(|line| **line != "},")
                        .position(|line| *line == "level: Some(")
                        .and_then(|offset| lines.get(index + offset + 2))
                        .and_then(|line| line.trim_end_matches(',').parse::<i32>().ok());
                    level.map_or_else(
                        || "zstd (level not recorded)".into(),
                        |level| format!("zstd:{level}"),
                    )
                }
                _ => return None,
            };
            if !codecs.contains(&codec) {
                codecs.push(codec);
            }
        }
    }
    if values.is_empty() || (general && codecs.is_empty()) {
        return None;
    }
    let mut layout = layout?.to_string();
    let mut value = values.join("+");
    let mut codec = if codecs.is_empty() {
        "none".into()
    } else {
        codecs.join("+")
    };
    if let Some(plan) = plan {
        if plan.structural == StructuralEncoding::Auto {
            layout = format!("auto({layout})");
        }
        if plan.value == ValueEncoding::Auto {
            value = format!("auto({value})");
        }
        if plan.general == GeneralCompression::Baseline {
            codec = format!("baseline({codec})");
        }
    }
    Some(format!("{layout} / {value} / {codec}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_observed_codec_and_keeps_mixed_pages_separate() {
        let plain = "PageLayout {\nMiniBlockLayout {\nVariable {\nFlat {\n";
        let compressed = format!(
            "{plain}General {{\nBufferCompression {{\nscheme: CompressionAlgorithmZstd,\nlevel: Some(\n6,\n),\n}},\n"
        );
        assert_eq!(
            summarize(
                &[plain.into(), compressed.clone(), compressed.clone()],
                None
            ),
            "miniblock / variable+flat / none; miniblock / variable+flat / zstd:6"
        );
        let mut plan =
            EncodingPlan::baseline(lance_compression_estimation::EncodingFileVersion::V2_2);
        plan.general = GeneralCompression::Zstd { level: 12 };
        assert_eq!(
            summarize(&[compressed.clone()], Some(plan)),
            "auto(miniblock) / auto(variable+flat) / zstd:6"
        );
        let no_level = compressed.replace("level: Some(\n6,\n),", "level: None,");
        assert_eq!(
            summarize(&[no_level], Some(plan)),
            "auto(miniblock) / auto(variable+flat) / zstd (level not recorded)"
        );
        assert_eq!(
            summarize(&[format!("{plain}FutureEncoding {{\n")], None),
            "details (--verbose)"
        );
    }
}
