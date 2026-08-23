//! Agent harness — port of `packages/agent/src/harness/`.
//!
//! Landed: compaction + branch-summarization (LLM-backed summary generation
//! over session paths), the minimal `SimpleModels` seam used by the
//! summarization calls, and the harness error types.
//!
//! Remaining harness surfaces (tracked in `crates/pi-agent/TODO.md`):
//! `events` landed this session; reducer, prompt templates, system prompt,
//! skills, env, agent-loop integration, and telemetry wiring remain.

pub mod agent_harness;
pub mod compaction;
pub mod env;
pub mod events;
pub mod frontmatter;
mod models;
pub mod prompt_templates;
pub mod reducer;
pub mod result;
pub mod shell_output;
pub mod skills;
pub mod system_prompt;
pub mod telemetry;
pub mod tools;

pub use models::{BoxFuture, CompleteSimpleFn, SimpleModels};
pub use prompt_templates::{load_prompt_templates, PromptTemplateDiagnostic};
pub use reducer::{
    reduce_lane_state, validate_record_log, LaneReductionInput, LaneReductionResult, LaneState,
};
pub use skills::{load_skills, SkillDiagnostic};

/// Stable error codes returned by compaction helpers
/// (upstream `CompactionErrorCode`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionError {
    pub code: &'static str,
    pub message: String,
}

impl CompactionError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CompactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "compaction error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for CompactionError {}

/// Stable error codes returned by branch-summarization helpers
/// (upstream `BranchSummaryErrorCode`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchSummaryError {
    pub code: &'static str,
    pub message: String,
}

impl BranchSummaryError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for BranchSummaryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "branch summary error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for BranchSummaryError {}
