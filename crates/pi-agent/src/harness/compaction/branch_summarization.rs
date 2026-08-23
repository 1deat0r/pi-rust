//! Branch summarization — port of
//! `packages/agent/src/harness/compaction/branch-summarization.ts`.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use pi_ai::model::Model;
use pi_ai::types::{ContentBlock, Message, Usage};

use crate::fs::FileSystem;
use crate::messages::{
    convert_to_llm, create_branch_summary_message, create_compaction_summary_message,
};
use crate::session::session::Session;
use crate::session::state::{BranchBounds, EntryQuery};
use crate::session::types::{session_error, Entry, SessionErrorKind};
use crate::types::AgentMessage;
use pi_ai::utils::{RetryCallbacks, RetryPolicy};

use super::compaction::{
    complete_simple_with_retries, estimate_tokens, SummarizationOptions,
    SUMMARIZATION_SYSTEM_PROMPT,
};
use super::utils::{
    compute_file_lists, create_file_ops, extract_file_ops_from_message, format_file_operations,
    serialize_conversation, FileOperations,
};
use crate::harness::{BranchSummaryError, SimpleModels};

/// Generated branch summary data ready to be persisted as a branch-summary
/// entry.
#[derive(Debug, Clone, PartialEq)]
pub struct BranchSummaryResult {
    pub summary: String,
    pub usage: Option<Usage>,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// File-operation details stored on generated branch summary entries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BranchSummaryDetails {
    /// Files read while exploring the summarized branch.
    pub read_files: Vec<String>,
    /// Files modified while exploring the summarized branch.
    pub modified_files: Vec<String>,
}

/// Prepared branch content for summarization.
#[derive(Debug, Clone)]
pub struct BranchPreparation {
    /// Messages selected for the branch summary.
    pub messages: Vec<AgentMessage>,
    /// File operations extracted from the branch.
    pub file_ops: FileOperations,
    /// Estimated token count for selected messages.
    pub total_tokens: u64,
}

/// Entries selected for branch summarization.
#[derive(Debug, Clone, Default)]
pub struct CollectEntriesResult {
    /// Entries to summarize in chronological order.
    pub entries: Vec<Entry>,
    /// Deepest common ancestor between the previous leaf and target entry.
    pub common_ancestor_id: Option<String>,
}

/// Collect entries that should be summarized before navigating to a
/// different session tree entry.
pub async fn collect_entries_for_branch_summary<F: FileSystem>(
    session: &Session<F>,
    old_leaf_id: Option<&str>,
    target_id: &str,
) -> Result<CollectEntriesResult, crate::session::types::SessionError> {
    let Some(old_leaf_id) = old_leaf_id else {
        return Ok(CollectEntriesResult::default());
    };
    // Upstream `findEntriesOnBranch({ start })` defaults to newest-first
    // (walk from the start entry back toward the root), so a plain default
    // query keeps the common-ancestor scan identical to upstream.
    let old_path: std::collections::HashSet<String> = session
        .find_entries_on_branch(
            &EntryQuery::default(),
            Some(old_leaf_id),
            &BranchBounds::default(),
        )
        .await?
        .into_iter()
        .map(|entry| entry.id().to_string())
        .collect();
    let target_path = session
        .find_entries_on_branch(
            &EntryQuery::default(),
            Some(target_id),
            &BranchBounds::default(),
        )
        .await?;
    let mut common_ancestor_id: Option<String> = None;
    for entry in &target_path {
        if old_path.contains(entry.id()) {
            common_ancestor_id = Some(entry.id().to_string());
            break;
        }
    }
    let mut entries: Vec<Entry> = Vec::new();
    let mut current: Option<String> = Some(old_leaf_id.to_string());
    while let Some(id) = current {
        if common_ancestor_id.as_deref() == Some(id.as_str()) {
            break;
        }
        let entry = session.get_entry(&id).await.ok_or_else(|| {
            session_error(
                SessionErrorKind::InvalidEntry,
                format!("Entry {id} not found"),
            )
        })?;
        let parent_id = entry.parent_id().map(|s| s.to_string());
        entries.push(entry);
        current = parent_id;
    }
    entries.reverse();

    Ok(CollectEntriesResult {
        entries,
        common_ancestor_id,
    })
}

fn get_message_from_entry(entry: &Entry) -> Option<AgentMessage> {
    match entry {
        Entry::Message { message, .. } => {
            if message.role() == "toolResult" {
                return None;
            }
            Some(message.clone())
        }
        Entry::BranchSummary {
            summary,
            from_id,
            timestamp,
            ..
        } => Some(create_branch_summary_message(
            summary.clone(),
            from_id.clone(),
            *timestamp,
        )),
        Entry::Compaction {
            summary,
            tokens_before,
            timestamp,
            ..
        } => Some(create_compaction_summary_message(
            summary.clone(),
            *tokens_before,
            *timestamp,
        )),
        Entry::ThinkingLevel { .. }
        | Entry::ModelChange { .. }
        | Entry::ActiveTools { .. }
        | Entry::Custom { .. } => None,
    }
}

/// Prepare branch entries for summarization within an optional token budget.
pub fn prepare_branch_entries(entries: &[Entry], token_budget: u64) -> BranchPreparation {
    let mut messages: Vec<AgentMessage> = Vec::new();
    let mut file_ops = create_file_ops();
    let mut total_tokens = 0u64;

    for entry in entries {
        if let Entry::BranchSummary {
            details: Some(details),
            ..
        } = entry
        {
            if let Some(read) = details
                .get("readFiles")
                .and_then(pi_ai::types::JsonValue::as_array)
            {
                for f in read {
                    if let Some(s) = f.as_str() {
                        file_ops.read.insert(s.to_string());
                    }
                }
            }
            if let Some(modified) = details
                .get("modifiedFiles")
                .and_then(pi_ai::types::JsonValue::as_array)
            {
                for f in modified {
                    if let Some(s) = f.as_str() {
                        file_ops.edited.insert(s.to_string());
                    }
                }
            }
        }
    }
    for index in (0..entries.len()).rev() {
        let entry = &entries[index];
        let Some(message) = get_message_from_entry(entry) else {
            continue;
        };
        extract_file_ops_from_message(&message, &mut file_ops);

        let tokens = estimate_tokens(&message);
        if token_budget > 0 && total_tokens + tokens > token_budget {
            if matches!(
                entry,
                Entry::Compaction { .. } | Entry::BranchSummary { .. }
            ) && total_tokens < (token_budget as f64 * 0.9).floor() as u64
            {
                messages.insert(0, message);
                total_tokens += tokens;
            }
            break;
        }

        messages.insert(0, message);
        total_tokens += tokens;
    }

    BranchPreparation {
        messages,
        file_ops,
        total_tokens,
    }
}

const BRANCH_SUMMARY_PREAMBLE: &str =
    "The user explored a different conversation branch before returning here.
Summary of that exploration:

";

const BRANCH_SUMMARY_PROMPT: &str =
    "Create a structured summary of this conversation branch for context when returning later.

Use this EXACT format:

## Goal
[What was the user trying to accomplish in this branch?]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned]
- [Or \"(none)\" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Work that was started but not finished]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [What should happen next to continue this work]

Keep each section concise. Preserve exact file paths, function names, and error messages.";

/// Options for generating a branch summary.
#[derive(Default)]
pub struct GenerateBranchSummaryOptions<'a> {
    pub signal: Option<&'a Arc<AtomicBool>>,
    /// Optional instructions appended to or replacing the default prompt.
    pub custom_instructions: Option<&'a str>,
    /// Replace the default prompt with custom instructions instead of
    /// appending them.
    pub replace_instructions: bool,
    /// Tokens reserved for prompt and model output. Defaults to 16384.
    pub reserve_tokens: Option<u64>,
    /// Optional retry policy for transient summarization errors.
    pub retry: Option<&'a RetryPolicy>,
    /// Optional callbacks for retry reporting.
    pub callbacks: Option<&'a RetryCallbacks<'a>>,
}

/// Generate a summary for abandoned branch entries.
pub async fn generate_branch_summary(
    entries: &[Entry],
    models: &SimpleModels,
    model: &Model,
    options: &GenerateBranchSummaryOptions<'_>,
) -> Result<BranchSummaryResult, BranchSummaryError> {
    let reserve_tokens = options.reserve_tokens.unwrap_or(16_384);
    let context_window = if model.context_window != 0 {
        model.context_window
    } else {
        128_000
    };
    let token_budget = context_window.saturating_sub(reserve_tokens);

    let preparation = prepare_branch_entries(entries, token_budget);
    let BranchPreparation {
        messages, file_ops, ..
    } = preparation;

    if messages.is_empty() {
        return Ok(BranchSummaryResult {
            summary: "No content to summarize".to_string(),
            usage: None,
            read_files: vec![],
            modified_files: vec![],
        });
    }
    let llm_messages = convert_to_llm(&messages);
    let conversation_text = serialize_conversation(&llm_messages);
    let instructions: String = match (options.replace_instructions, options.custom_instructions) {
        (true, Some(custom_instructions)) => custom_instructions.to_string(),
        (_, Some(custom_instructions)) => {
            format!("{BRANCH_SUMMARY_PROMPT}\n\nAdditional focus: {custom_instructions}")
        }
        _ => BRANCH_SUMMARY_PROMPT.to_string(),
    };
    let prompt_text =
        format!("<conversation>\n{conversation_text}\n</conversation>\n\n{instructions}");

    let summarization_messages = vec![Message::User(pi_ai::types::UserContent::blocks(
        vec![ContentBlock::text(prompt_text)],
        pi_ai::types::now_ms(),
    ))];
    let context = pi_ai::types::Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        messages: summarization_messages.clone(),
        tools: vec![],
    };
    let completion_options = SummarizationOptions {
        max_tokens: Some(2048),
        signal: options.signal,
        reasoning: None,
    };
    let response = complete_simple_with_retries(
        models,
        model,
        &context,
        completion_options,
        options.retry,
        options.callbacks,
    )
    .await;
    if response.stop_reason() == Some(pi_ai::types::StopReason::Aborted) {
        return Err(BranchSummaryError::new(
            "aborted",
            response.error_message().unwrap_or("Branch summary aborted"),
        ));
    }
    if response.stop_reason() == Some(pi_ai::types::StopReason::Error) {
        return Err(BranchSummaryError::new(
            "summarization_failed",
            format!(
                "Branch summary failed: {}",
                response.error_message().unwrap_or("Unknown error")
            ),
        ));
    }

    let content: String = response
        .content()
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let mut summary = format!("{BRANCH_SUMMARY_PREAMBLE}{content}");
    let (read_files, modified_files) = compute_file_lists(&file_ops);
    summary += &format_file_operations(&read_files, &modified_files);

    Ok(BranchSummaryResult {
        summary: if summary.is_empty() {
            "No summary generated".to_string()
        } else {
            summary
        },
        usage: response.usage().cloned(),
        read_files,
        modified_files,
    })
}
