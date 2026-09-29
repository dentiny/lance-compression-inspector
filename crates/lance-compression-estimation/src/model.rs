use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Maximum physical rows sampled uniformly without replacement from each
/// active Lance data file; this is not a per-fragment, per-page, or
/// per-row-group limit.
pub const DEFAULT_SAMPLE_ROWS: usize = 16_384;

/// Metadata and sample measurements for one resolved dataset snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DatasetProbeReport {
    /// Canonical local path to the dataset directory.
    pub source: String,
    /// Resolved manifest branch; manifests without a branch are reported as `main`.
    pub branch: String,
    /// Dataset snapshot version, distinct from each data file's encoding format.
    pub manifest_version: u64,
    /// Number of active fragments in the selected manifest; not a data-file count.
    pub fragment_count: usize,
    /// Sum of available physical-row counts in fragment metadata. Does not subtract
    /// logical deletions; fragments with no recorded count contribute nothing.
    pub physical_rows: u64,
    /// One probe per distinct active base or overlay data-file path referenced by
    /// the snapshot. A fragment may reference multiple files.
    pub files: Vec<ProbeReport>,
}

/// Analysis results grouped by data file for one resolved dataset snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DatasetAnalysisReport {
    /// Canonical local path to the dataset directory.
    pub source: String,
    /// Resolved manifest branch; manifests without a branch are reported as `main`.
    pub branch: String,
    /// Dataset snapshot version, distinct from each data file's encoding format.
    pub manifest_version: u64,
    /// Number of active fragments in the selected manifest; not a data-file count.
    pub fragment_count: usize,
    /// Sum of available physical-row counts in fragment metadata. Does not subtract
    /// logical deletions; fragments with no recorded count contribute nothing.
    pub physical_rows: u64,
    /// Per-file analyses for the selected snapshot; suggestions are not aggregated
    /// into a dataset-wide rewrite plan.
    pub files: Vec<AnalysisReport>,
}

/// Metadata and optional sampled measurements for one physical Lance data file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProbeReport {
    /// Canonical local path to this standalone Lance data file.
    pub source: String,
    /// Encoding format read from the file metadata, not the dataset snapshot version.
    pub file_version: EncodingFileVersion,
    /// Total physical file size in bytes, including data, metadata, and footer.
    pub file_size_bytes: u64,
    /// Data-section byte count reported by Lance metadata; distinct from total file
    /// size and from the sum of attributed logical-column buffer sizes.
    pub data_bytes: u64,
    /// Physical rows stored in this file, before dataset-level deletion filtering.
    pub rows: u64,
    /// Nested child paths retained for visibility. Rewrites and size estimates
    /// operate on their top-level parent, not on these children independently.
    pub unsupported_nested_targets: Vec<String>,
    /// Profiles in top-level schema order, aggregating each field's physical columns.
    pub columns: Vec<ColumnProfile>,
}

/// Observed properties of one top-level logical field, including its child buffers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ColumnProfile {
    /// Zero-based top-level field index in this file's schema; not a physical-column ID.
    pub index: usize,
    /// Top-level field name used to match the target of a sampled rewrite.
    pub path: String,
    /// Display representation of the logical Arrow data type.
    pub data_type: String,
    /// Total physical pages across all physical columns belonging to this field.
    pub pages: usize,
    /// Sum of page-buffer and shared column-buffer sizes attributed to this field.
    /// Excludes file-level metadata/footer overhead; blob payloads stored outside
    /// these buffers are not included.
    pub on_disk_bytes: u64,
    /// Source field metadata, including existing writer controls and custom entries.
    pub field_metadata: BTreeMap<String, String>,
    /// Whether this field or any child stores Blob payloads outside its column buffers.
    /// Such columns are excluded from compression recommendations.
    pub has_blob: bool,
    /// Measurements from this data file's bounded row sample. Each plan changes
    /// writer controls only for this top-level field; empty if no measurements
    /// were collected (for example, sampling was disabled or the file is legacy).
    pub encoding_measurements: Vec<EncodingMeasurement>,
    /// Sorted, deduplicated page-encoding descriptions retained for inspection,
    /// as returned by Lance without parsing them into a separate tag vocabulary.
    pub raw_page_encodings: Vec<String>,
}

/// Recommendations and their input measurements for one data file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnalysisReport {
    /// Original file metadata and sampled measurements used for this analysis.
    pub probe: ProbeReport,
    /// File- and column-level recommendations. An empty list means no rule emitted
    /// a recommendation; it does not prove that the file is optimally encoded.
    pub suggestions: Vec<Suggestion>,
}

/// One rule's recommendation scoped to a file or a top-level column.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Suggestion {
    /// Machine-readable identifier of the rule that emitted this recommendation.
    pub rule: String,
    /// Presentation priority; not a statistical confidence or measured savings value.
    pub severity: Severity,
    /// Scope of the recommendation within the containing file report.
    pub location: Location,
    /// Human-readable explanation of the recommendation.
    pub message: String,
    /// Proposed inspection or rewrite; analysis does not execute this action.
    pub action: Action,
    /// Optional encoding-only savings estimate. Unavailable measurements and
    /// file-version recommendations currently leave this unset.
    pub estimate: Option<SavingsEstimate>,
    /// Plans for this column in the selected target format, sorted by ascending
    /// `effective_score` (best first). Empty for non-plan recommendations.
    pub candidate_scores: Vec<CandidateScore>,
    /// Human-readable observations and assumptions supporting this recommendation.
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Location {
    File,
    Column { index: usize, path: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Suggestion,
    Warning,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Action {
    ProbeEncodingPlans {
        /// Ranked candidate plans for this column; empty when no measurements
        /// are available and the recommendation is to collect them first.
        plans: Vec<EncodingPlan>,
    },
    RewriteFileVersion {
        /// Display label of the proposed format, including an experimental
        /// qualifier when applicable.
        target: String,
        /// Format capabilities motivating a rewrite, not necessarily features
        /// that every column in this file will benefit from.
        capabilities: Vec<String>,
        /// Full-file projection using per-column baseline ratios between source
        /// and target formats, with file overhead held fixed. No column-specific
        /// encoding optimizations are applied. None if required samples are absent.
        projected_file_bytes: Option<u64>,
        /// Current file bytes minus projected bytes: positive means savings,
        /// negative means growth. None when the projection is unavailable;
        /// values outside the signed 64-bit range are clamped.
        size_delta_bytes: Option<i64>,
    },
}

/// Structural layout selection. `Auto` leaves the source field's metadata
/// untouched and therefore represents Lance's existing/default choice.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum StructuralEncoding {
    Auto,
    MiniBlock,
    FullZip,
    Sparse,
}

/// Value encodings with public writer controls. Bitpacking and constant
/// encodings deliberately remain part of `Auto`: Lance does not expose
/// controls that can honestly force them.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum ValueEncoding {
    Auto,
    Rle,
    Fsst,
    ByteStreamSplit,
    Dictionary,
    PackedStruct,
}

/// General-purpose compression layered over the structural/value encoding.
/// `Baseline` preserves source metadata; `None` explicitly disables it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum GeneralCompression {
    Baseline,
    None,
    Lz4,
    Zstd { level: i32 },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "lowercase")]
pub enum EncodingFileVersion {
    #[serde(rename = "0.1")]
    V1,
    #[serde(rename = "2.0")]
    V2_0,
    #[serde(rename = "2.1")]
    V2_1,
    #[serde(rename = "2.2")]
    V2_2,
    #[serde(rename = "2.3")]
    V2_3,
}

impl std::fmt::Display for EncodingFileVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::V1 => "0.1",
            Self::V2_0 => "2.0",
            Self::V2_1 => "2.1",
            Self::V2_2 => "2.2",
            Self::V2_3 => "2.3",
        })
    }
}

/// A measurable, version-aware writer configuration for one top-level field.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EncodingPlan {
    /// Requested structural layout for the target top-level field.
    pub structural: StructuralEncoding,
    /// Requested value-encoding control; the writer may still choose other
    /// encodings, which are recorded in the measurement's page descriptions.
    pub value: ValueEncoding,
    /// Requested general compressor. `Baseline` preserves source metadata, while
    /// `None` explicitly disables general compression.
    pub general: GeneralCompression,
    /// File format used for the temporary rewrite, which can differ from the
    /// source format. Ranking compares candidates within a fixed target format.
    pub file_version: EncodingFileVersion,
}

impl EncodingPlan {
    pub const fn baseline(file_version: EncodingFileVersion) -> Self {
        Self {
            structural: StructuralEncoding::Auto,
            value: ValueEncoding::Auto,
            general: GeneralCompression::Baseline,
            file_version,
        }
    }
}

/// Observed result of rewriting one bounded sample with a requested encoding plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncodingMeasurement {
    /// Writer configuration requested for this target field in the sampled rewrite.
    pub plan: EncodingPlan,
    /// Actual physical rows re-encoded, capped by both the per-file sampling limit
    /// and the number of rows available; not the full file's row count.
    pub sample_rows: u64,
    /// Measured target-column page and shared-buffer bytes in the temporary
    /// rewrite. Excludes other columns and file overhead; not a full-column projection.
    pub encoded_bytes: u64,
    /// Actual page-encoding descriptions returned by Lance for the sampled column.
    /// Kept separately from the requested plan; distinct encodings remain distinct.
    pub resolved_page_encodings: Vec<String>,
}

/// A column-level candidate evaluated within one target file format.
/// The file-size projection is a single-column what-if result for reporting;
/// only the projected column size and optional policy factors drive ranking.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CandidateScore {
    /// Candidate writer configuration for one top-level column in the target format.
    pub plan: EncodingPlan,
    /// Actual physical rows used for this candidate's measurement.
    pub sample_rows: u64,
    /// Measured bytes for this column in the sample rewrite, before extrapolation
    /// to the full column; excludes other columns and file overhead.
    pub encoded_bytes: u64,
    /// Actual page-encoding descriptions from the sampled rewrite.
    pub resolved_page_encodings: Vec<String>,
    /// Estimated full-column bytes: target-format baseline column bytes times
    /// the candidate-to-baseline sample byte ratio. A zero-byte sample baseline
    /// falls back to the baseline column size.
    pub projected_column_bytes: u64,
    /// Estimated file bytes if ONLY this column adopts the candidate, with all
    /// other columns kept at their target-format baselines and file overhead fixed:
    /// `baseline_file_bytes - baseline_column_bytes + projected_column_bytes`
    /// (using saturating arithmetic). Not a combined all-column optimization,
    /// and these per-candidate file estimates must not be summed.
    pub projected_file_bytes: u64,
    /// Used for decoding penalty.
    ///
    /// Heuristic decode-cost multiplier, not a measured runtime.
    /// Set to 1.0 unless `consider_decoding_penalty` is enabled.
    pub decoding_penalty: f64,
    /// `projected_column_bytes * decoding_penalty`.
    /// Lower is better when ranking candidates for this column.
    pub effective_score: f64,
}

/// Optional ranking policy, separate from measurement and size projection.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnalyzeOptions {
    /// Apply a codec/level decode-cost heuristic when ranking plans.
    /// Defaults to false. The multiplier can change the winner, but does not
    /// change any candidate's projected size.
    pub consider_decoding_penalty: bool,
}

/// Projected encoding-only savings for a single-column recommendation.
/// File-format migration is reported separately by `Action::RewriteFileVersion`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SavingsEstimate {
    /// Source of the estimate; current encoding recommendations use measured samples.
    pub basis: EstimateBasis,
    /// Encoding-only bytes saved relative to the target-format baseline file when
    /// this column changes. Negative savings are clamped to zero. Currently equal
    /// to `upper_bytes`: these are point estimates, not a statistical interval.
    pub lower_bytes: u64,
    /// Upper savings value in bytes; currently the same point estimate as
    /// `lower_bytes`, with no separately computed uncertainty bound.
    pub upper_bytes: u64,
    /// Integer percentage of target-format baseline FILE bytes saved, not of
    /// column bytes. Truncated and capped at 100; currently equals `upper_percent`.
    pub lower_percent: u8,
    /// Upper savings percentage using the same file-level denominator; currently
    /// equal to `lower_percent`, not a separate uncertainty bound.
    pub upper_percent: u8,
    /// Human-readable sampling assumptions and scope limits, including the sample
    /// row count and exclusion of format-migration gains.
    pub caveat: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum EstimateBasis {
    EncodingHeuristic,
    MeasuredProbe,
}
