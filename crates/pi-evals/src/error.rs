//! Typed errors for the eval harness.
//!
//! Every variant's `Display` output is byte-identical to the message string
//! it replaces, so downstream diagnostics and persisted eval failure text are
//! unchanged. Sources are attached with `#[source]` so the error chain stays
//! inspectable.

use std::fmt;
use std::path::PathBuf;

use thiserror::Error;

/// Assertion-failure payload from eval scenario asserts. This is scored,
/// human-readable eval feedback rather than a system error, so it is a
/// newtype instead of an `EvalError` variant.
#[derive(Debug, Clone)]
pub struct EvalFailures(pub Vec<String>);

impl fmt::Display for EvalFailures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join("; "))
    }
}

#[derive(Debug, Error)]
pub enum EvalError {
    // Input validation.
    #[error("Eval input must contain only finite numbers.")]
    NonFiniteNumber,
    #[error("evalSet must not be empty.")]
    EmptyEvalSet,
    #[error("At least one candidate harness is required.")]
    MissingCandidates,
    #[error("Harness names must be unique within an eval set.")]
    DuplicateHarnessName,
    #[error("repetitions must be a positive integer.")]
    InvalidRepetitions,
    #[error("Missing value for {flag}")]
    MissingFlagValue { flag: String },
    #[error("CLI model selection requires both --provider and --model.")]
    PartialModelSelection,
    #[error("Select a harness model explicitly or set both PI_PROVIDER and PI_MODEL as defaults.")]
    MissingModelSelection,
    #[error("Pi eval session artifact metadata is invalid.")]
    InvalidSessionArtifact,
    #[error("Invalid eval artifact name: {name}")]
    InvalidArtifactName { name: String },

    // Session-usage parsing.
    #[error("session JSONL is empty or missing a supported header")]
    MissingSessionHeader,
    #[error("session usage must be an object")]
    UsageNotObject,
    #[error("session usage cost must be an object")]
    CostNotObject,
    #[error("session usage field {field} is outside the signed range")]
    UsageFieldOutOfRange { field: String },
    #[error("session usage field {field} must be an integer")]
    UsageFieldNotInteger { field: String },
    #[error("session cost field {field} must be a number")]
    CostFieldNotNumber { field: String },
    #[error("session cost field {field} must be finite")]
    CostFieldNotFinite { field: String },
    #[error("failed to parse session JSONL line {line}: {source}")]
    ParseSessionLine {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("session JSONL line {line} must contain a JSON object")]
    SessionLineNotObject { line: usize },
    #[error("session JSONL line {line} is not a supported session header")]
    UnsupportedSessionHeader { line: usize },
    #[error("session JSONL line {line} message entry has no object message")]
    SessionMessageNotObject { line: usize },
    #[error("session JSONL line {line}: {source}")]
    SessionLineUsage {
        line: usize,
        #[source]
        source: Box<EvalError>,
    },

    // Transcript parsing.
    #[error("failed to parse transcript JSONL line {line}: {source}")]
    ParseTranscriptLine {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("transcript JSONL line {line} must contain a JSON object")]
    TranscriptLineNotObject { line: usize },

    // Harness input serialization.
    #[error("Eval input must be JSON-serializable: {source}")]
    InputNotSerializable {
        #[source]
        source: serde_json::Error,
    },

    // Process harness.
    #[error("failed to create eval session directory: {source}")]
    CreateSessionDir {
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create eval agent directory: {source}")]
    CreateAgentDir {
        #[source]
        source: std::io::Error,
    },
    #[error("failed to spawn {binary}: {source}")]
    Spawn {
        binary: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to wait: {source}")]
    Wait {
        #[source]
        source: std::io::Error,
    },
    #[error("{binary} timed out after {timeout_secs}s")]
    Timeout { binary: String, timeout_secs: u64 },
    #[error("failed to clean up eval session directory {path}: {source}")]
    CleanupSessionDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read session JSONL {path}: {source}")]
    ReadSession {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to list session directory {path}: {source}")]
    ListSessionDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to inspect session directory {path}: {source}")]
    InspectSessionDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to inspect session path {path}: {source}")]
    InspectSessionPath {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    // Artifact persistence and reporting.
    #[error("Failed to create artifact dir: {source}")]
    CreateArtifactDir {
        #[source]
        source: std::io::Error,
    },
    #[error("Failed to write artifact: {source}")]
    WriteArtifact {
        #[source]
        source: std::io::Error,
    },
    #[error("Failed to inspect artifact permissions: {source}")]
    InspectArtifactPermissions {
        #[source]
        source: std::io::Error,
    },
    #[error("Failed to restrict artifact permissions: {source}")]
    RestrictArtifactPermissions {
        #[source]
        source: std::io::Error,
    },
    #[error("Failed to serialize run record: {source}")]
    SerializeRunRecord {
        #[source]
        source: serde_json::Error,
    },
    #[error("Failed to open runs.jsonl: {source}")]
    OpenRunsJsonl {
        #[source]
        source: std::io::Error,
    },
    #[error("Failed to append run record: {source}")]
    AppendRunRecord {
        #[source]
        source: std::io::Error,
    },
}
