use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

pub const REPORT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DatasetProbeReport {
    pub schema_version: u32,
    pub source: String,
    pub branch: String,
    pub manifest_version: u64,
    pub fragment_count: usize,
    pub physical_rows: u64,
    pub files: Vec<ProbeReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DatasetAnalysisReport {
    pub schema_version: u32,
    pub source: String,
    pub branch: String,
    pub manifest_version: u64,
    pub fragment_count: usize,
    pub physical_rows: u64,
    pub files: Vec<AnalysisReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProbeReport {
    pub schema_version: u32,
    pub source: String,
    pub file_version: String,
    pub file_size_bytes: u64,
    pub data_bytes: u64,
    pub rows: u64,
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
    ProbeEncodings {
        candidates: Vec<EncodingCandidate>,
    },
    RewriteFileVersion {
        target: String,
        capabilities: Vec<String>,
    },
    InspectUnknownEncoding,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CompressionAlgorithm {
    Lz4,
    Zstd,
}

/// A compression algorithm and level form one distinct encoding candidate.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncodingCandidate {
    pub algorithm: CompressionAlgorithm,
    pub level: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CandidateScore {
    pub candidate: EncodingCandidate,
    pub estimated_bytes_lower: u64,
    pub estimated_bytes_upper: u64,
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
