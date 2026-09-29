use crate::{
    model::{
        AnalysisReport, AnalyzeOptions, DatasetAnalysisReport, DatasetProbeReport, ProbeReport,
    },
    rules,
};

/// Analyze every active data file in a resolved dataset snapshot.
pub fn analyze_dataset(probe: DatasetProbeReport) -> DatasetAnalysisReport {
    analyze_dataset_with_options(probe, AnalyzeOptions::default())
}

pub fn analyze_dataset_with_options(
    probe: DatasetProbeReport,
    options: AnalyzeOptions,
) -> DatasetAnalysisReport {
    DatasetAnalysisReport {
        schema_version: probe.schema_version,
        source: probe.source,
        branch: probe.branch,
        manifest_version: probe.manifest_version,
        fragment_count: probe.fragment_count,
        physical_rows: probe.physical_rows,
        files: probe
            .files
            .into_iter()
            .map(|file| analyze_with_options(file, options))
            .collect(),
    }
}

/// Analyze a probe without doing I/O or mutating the Lance file.
pub fn analyze(probe: ProbeReport) -> AnalysisReport {
    analyze_with_options(probe, AnalyzeOptions::default())
}

pub fn analyze_with_options(probe: ProbeReport, options: AnalyzeOptions) -> AnalysisReport {
    let mut suggestions = rules::file_version::check(&probe);
    suggestions.extend(rules::compression::check(&probe, options));
    AnalysisReport { probe, suggestions }
}
