//! Compaction + branch summarization — port of
//! `packages/agent/src/harness/compaction/`.

pub mod branch_summarization;
#[allow(clippy::module_inception)] // mirrors the upstream compaction/ dir layout
pub mod compaction;
pub mod utils;

pub use branch_summarization::{
    collect_entries_for_branch_summary, generate_branch_summary, prepare_branch_entries,
    BranchPreparation, BranchSummaryDetails, BranchSummaryResult, CollectEntriesResult,
    GenerateBranchSummaryOptions,
};
pub use compaction::{
    calculate_context_tokens, compact, estimate_context_tokens, estimate_tokens, find_cut_point,
    find_turn_start_index, generate_summary, generate_summary_with_usage, get_last_assistant_usage,
    prepare_compaction, should_compact, CompactionDetails, CompactionPreparation, CompactionSettings,
    CompactResult, ContextUsageEstimate, CutPointResult, DEFAULT_COMPACTION_SETTINGS,
};
pub use utils::{
    compute_file_lists, create_file_ops, extract_file_ops_from_message, format_file_operations,
    serialize_conversation, FileOperations,
};
