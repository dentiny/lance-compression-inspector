use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Maximum visible rows sampled uniformly without replacement from each
/// dataset fragment. Every active physical data file in that fragment is
/// evaluated on the same random sample; this is not a per-page or per-row-group
/// limit.
pub const DEFAULT_SAMPLE_ROWS: usize = 16_384;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DatasetProbeReport {
    pub source: String,
    pub branch: String,
    pub manifest_version: u64,
    pub fragment_count: usize,
    pub physical_rows: u64,
    pub files: Vec<ProbeReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DatasetAnalysisReport {
    pub source: String,
    pub branch: String,
    pub manifest_version: u64,
    pub fragment_count: usize,
    pub physical_rows: u64,
    pub files: Vec<AnalysisReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProbeReport {
    pub source: String,
    pub file_version: String,
    pub file_size_bytes: u64,
    pub data_bytes: u64,
    pub rows: u64,
    /// Nested child paths are reported explicitly but are not independently
    /// rewritten; their top-level parent is measured as one logical column.
    pub unsupported_nested_targets: Vec<String>,
    pub columns: Vec<ColumnProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ColumnProfile {
    pub index: usize,
    pub path: String,
    pub data_type: String,
    pub pages: usize,
    pub on_disk_bytes: u64,
    pub field_metadata: BTreeMap<String, String>,
    /// Normalized tags discovered by walking every page's encoding description.
    pub encoding_tags: BTreeSet<EncodingTag>,
    /// Sizes measured by re-encoding a bounded dataset sample. Each plan is
    /// applied only to this top-level logical field.
    pub encoding_measurements: Vec<EncodingMeasurement>,
    /// Lossless fallback for new or unknown Lance encodings.
    pub raw_page_encodings: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum EncodingTag {
    GeneralUncompressed,
    GeneralLz4,
    GeneralZstd,
    StructuralMiniBlock,
    StructuralFullZip,
    StructuralSparse,
    StructuralBlob,
    Nullable,
    List,
    Struct,
    Flat,
    Value,
    Block,
    Binary,
    FixedSizeBinary,
    Dictionary,
    Rle,
    BitPacked,
    ByteStreamSplit,
    Delta,
    Fsst,
    PackedStruct,
    FixedSizeList,
    Constant,
    VariableWidth,
    Indirect,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnalysisReport {
    pub probe: ProbeReport,
    pub suggestions: Vec<Suggestion>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Suggestion {
    pub rule: String,
    pub severity: Severity,
    pub location: Location,
    pub message: String,
    pub action: Action,
    pub estimate: Option<SavingsEstimate>,
    pub candidate_scores: Vec<CandidateScore>,
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
        plans: Vec<EncodingPlan>,
    },
    RewriteFileVersion {
        target: String,
        capabilities: Vec<String>,
        projected_file_bytes: Option<u64>,
        size_delta_bytes: Option<i64>,
    },
    InspectUnknownEncoding,
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

/// A measurable, version-aware writer configuration for one top-level field.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EncodingPlan {
    pub structural: StructuralEncoding,
    pub value: ValueEncoding,
    pub general: GeneralCompression,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncodingMeasurement {
    pub plan: EncodingPlan,
    pub sample_rows: u64,
    pub encoded_bytes: u64,
    /// Encodings actually selected by the Lance writer for this sample.
    pub resolved_encoding_tags: BTreeSet<EncodingTag>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CandidateScore {
    pub plan: EncodingPlan,
    pub sample_rows: u64,
    pub encoded_bytes: u64,
    pub resolved_encoding_tags: BTreeSet<EncodingTag>,
    pub projected_column_bytes: u64,
    pub projected_file_bytes: u64,
    pub consideration_factor: f64,
    pub decoding_penalty: f64,
    pub effective_score: f64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnalyzeOptions {
    /// Include a decode-cost multiplier when ranking encoding candidates.
    pub consider_decoding_penalty: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SavingsEstimate {
    pub basis: EstimateBasis,
    pub confidence: Confidence,
    pub lower_bytes: u64,
    pub upper_bytes: u64,
    pub lower_percent: u8,
    pub upper_percent: u8,
    pub caveat: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum EstimateBasis {
    EncodingHeuristic,
    MeasuredProbe,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Low,
    Medium,
    High,
}
