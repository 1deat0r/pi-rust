//! Compaction — port of `packages/agent/src/harness/compaction/compaction.ts`.
//!
//! Cut-point selection, token estimation, context-token calculation, and the
//! LLM-backed summary generation all follow the upstream implementation. The
//! only structural divergence is the `SimpleModels` seam in place of the
//! full pi-ai `Models` facade (see `harness/models.rs`).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use pi_ai::model::Model;
use pi_ai::types::{
    AssistantMessage, CacheRetention, ContentBlock, JsonValue, Message, SimpleStreamOptions,
    ThinkingLevel, Usage, UserContentBody,
};
use pi_ai::utils::{retry_assistant_call, RetryCallbacks, RetryPolicy};

use crate::messages::{
    convert_to_llm, create_branch_summary_message, create_compaction_summary_message,
};
use crate::session::context::{build_session_context, SessionContextBuildOptions};
use crate::session::new_id;
use crate::session::types::Entry;
use crate::types::AgentMessage;

use super::utils::{
    compute_file_lists, create_file_ops, extract_file_ops_from_message, format_file_operations,
    serialize_conversation, FileOperations,
};
use crate::harness::{CompactionError, SimpleModels};

/// File-operation details stored on generated compaction entries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionDetails {
    /// Files read in the compacted history.
    pub read_files: Vec<String>,
    /// Files modified in the compacted history.
    pub modified_files: Vec<String>,
}

/// Compaction thresholds and retention settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionSettings {
    /// Enable automatic compaction decisions.
    pub enabled: bool,
    /// Tokens reserved for summary prompt and output.
    pub reserve_tokens: u64,
    /// Approximate recent-context tokens to keep after compaction.
    pub keep_recent_tokens: u64,
}

/// Default compaction settings used by the harness.
pub const DEFAULT_COMPACTION_SETTINGS: CompactionSettings = CompactionSettings {
    enabled: true,
    reserve_tokens: 16384,
    keep_recent_tokens: 20000,
};

/// Calculate total context tokens from provider usage.
pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    let tokens = if usage.total_tokens != 0 {
        usage.total_tokens
    } else {
        usage.input + usage.output + usage.cache_read + usage.cache_write
    };
    // Negative usage is a ledger adjustment and cannot represent live context
    // size. Keep the derived estimate suitable for window arithmetic.
    tokens.max(0) as u64
}

fn get_assistant_usage(msg: &AgentMessage) -> Option<&Usage> {
    let AgentMessage::Core(Message::Assistant(assistant)) = msg else {
        return None;
    };
    if matches!(
        assistant.stop_reason(),
        Some(pi_ai::types::StopReason::Aborted) | Some(pi_ai::types::StopReason::Error)
    ) {
        return None;
    }
    let usage = assistant.usage()?;
    if calculate_context_tokens(usage) > 0 {
        Some(usage)
    } else {
        None
    }
}

/// Return usage from the last valid assistant message in session entries.
pub fn get_last_assistant_usage(entries: &[Entry]) -> Option<Usage> {
    for entry in entries.iter().rev() {
        if let Entry::Message { message, .. } = entry {
            if let Some(usage) = get_assistant_usage(message) {
                return Some(usage.clone());
            }
        }
    }
    None
}

/// Estimated context-token usage for a message list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextUsageEstimate {
    /// Estimated total context tokens.
    pub tokens: u64,
    /// Tokens reported by the most recent assistant usage block.
    pub usage_tokens: u64,
    /// Estimated tokens after the most recent assistant usage block.
    pub trailing_tokens: u64,
    /// Index of the message that provided usage, or `None` when none exists.
    pub last_usage_index: Option<usize>,
}

fn get_last_assistant_usage_info(messages: &[AgentMessage]) -> Option<(Usage, usize)> {
    for (index, msg) in messages.iter().enumerate().rev() {
        if let Some(usage) = get_assistant_usage(msg) {
            return Some((usage.clone(), index));
        }
    }
    None
}

/// Estimate context tokens for messages using provider usage when available.
pub fn estimate_context_tokens(messages: &[AgentMessage]) -> ContextUsageEstimate {
    let usage_info = get_last_assistant_usage_info(messages);

    let Some((usage, usage_index)) = usage_info else {
        let mut estimated = 0u64;
        for message in messages {
            estimated = estimated.saturating_add(estimate_tokens(message));
        }
        return ContextUsageEstimate {
            tokens: estimated,
            usage_tokens: 0,
            trailing_tokens: estimated,
            last_usage_index: None,
        };
    };

    let usage_tokens = calculate_context_tokens(&usage);
    let mut trailing_tokens = 0u64;
    for message in &messages[usage_index + 1..] {
        trailing_tokens = trailing_tokens.saturating_add(estimate_tokens(message));
    }

    ContextUsageEstimate {
        tokens: usage_tokens.saturating_add(trailing_tokens),
        usage_tokens,
        trailing_tokens,
        last_usage_index: Some(usage_index),
    }
}

/// Return whether context usage exceeds the configured compaction threshold.
pub fn should_compact(
    context_tokens: u64,
    context_window: u64,
    settings: &CompactionSettings,
) -> bool {
    if !settings.enabled {
        return false;
    }
    context_tokens > context_window.saturating_sub(settings.reserve_tokens)
}

const ESTIMATED_IMAGE_CHARS: u64 = 4800;

fn estimate_text_and_image_content_chars(content: &UserContentBody) -> u64 {
    match content {
        UserContentBody::String(s) => s.len() as u64,
        UserContentBody::Blocks(blocks) => {
            let mut chars = 0u64;
            for block in blocks {
                match block {
                    ContentBlock::Text { text, .. } => chars += text.len() as u64,
                    ContentBlock::Image { .. } => chars += ESTIMATED_IMAGE_CHARS,
                    _ => {}
                }
            }
            chars
        }
    }
}

fn thinking_level_from_str(level: &str) -> Option<ThinkingLevel> {
    match level {
        "minimal" => Some(ThinkingLevel::Minimal),
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        "xhigh" => Some(ThinkingLevel::Xhigh),
        "max" => Some(ThinkingLevel::Max),
        _ => None,
    }
}

fn safe_json_stringify(value: &JsonValue) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "[unserializable]".to_string())
}

/// Estimate token count for one message using a conservative character
/// heuristic.
pub fn estimate_tokens(message: &AgentMessage) -> u64 {
    let chars = match message {
        AgentMessage::Core(Message::User(user)) => {
            estimate_text_and_image_content_chars(user.content())
        }
        AgentMessage::Core(Message::Assistant(assistant)) => {
            let mut chars = 0u64;
            for block in assistant.content() {
                match block {
                    ContentBlock::Text { text, .. } => {
                        chars += text.len() as u64;
                    }
                    ContentBlock::Thinking { thinking, .. } => {
                        chars += thinking.len() as u64;
                    }
                    ContentBlock::ToolCall {
                        name, arguments, ..
                    } => {
                        chars += name.len() as u64;
                        chars += safe_json_stringify(arguments).len() as u64;
                    }
                    _ => {}
                }
            }
            chars
        }
        AgentMessage::Core(Message::ToolResult(result)) => estimate_text_and_image_content_chars(
            &UserContentBody::Blocks(result.content().to_vec()),
        ),
        AgentMessage::Custom(custom) => match custom {
            crate::types::CustomAgentMessage::BashExecution {
                command, output, ..
            } => (command.len() + output.len()) as u64,
            crate::types::CustomAgentMessage::Custom { content, .. } => match content {
                crate::types::CustomContent::String(s) => s.len() as u64,
                crate::types::CustomContent::Blocks(blocks) => blocks
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text { text, .. } => text.len() as u64,
                        _ => 0,
                    })
                    .sum(),
            },
            crate::types::CustomAgentMessage::BranchSummary { summary, .. }
            | crate::types::CustomAgentMessage::CompactionSummary { summary, .. } => {
                summary.len() as u64
            }
        },
    };
    chars.div_ceil(4)
}

fn find_valid_cut_points(entries: &[Entry], start: usize, end: usize) -> Vec<usize> {
    let mut cut_points: Vec<usize> = Vec::new();
    for (index, entry) in entries.iter().enumerate().take(end).skip(start) {
        match entry {
            Entry::Message { message, .. } => match message.role() {
                "bashExecution" | "custom" | "branchSummary" | "compactionSummary" | "user"
                | "assistant" => {
                    cut_points.push(index);
                }
                _ => {}
            },
            Entry::BranchSummary { .. } => cut_points.push(index),
            Entry::ThinkingLevel { .. }
            | Entry::ModelChange { .. }
            | Entry::ActiveTools { .. }
            | Entry::Compaction { .. }
            | Entry::Custom { .. } => {}
        }
    }
    cut_points
}

/// Find the user-visible message that starts the turn containing an entry.
pub fn find_turn_start_index(entries: &[Entry], entry_index: usize, start_index: usize) -> isize {
    for index in (start_index..=entry_index).rev() {
        let entry = &entries[index];
        if matches!(entry, Entry::BranchSummary { .. }) {
            return index as isize;
        }
        if let Entry::Message { message, .. } = entry {
            let role = message.role();
            if role == "user" || role == "bashExecution" {
                return index as isize;
            }
        }
    }
    -1
}

/// Cut point selected for compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CutPointResult {
    /// Index of the first entry retained after compaction.
    pub first_kept_entry_index: usize,
    /// Index of the turn-start entry when the cut splits a turn, otherwise -1.
    pub turn_start_index: isize,
    /// Whether the selected cut point splits an in-progress turn.
    pub is_split_turn: bool,
}

/// Find the compaction cut point that keeps approximately the requested
/// recent-token budget.
pub fn find_cut_point(
    entries: &[Entry],
    start_index: usize,
    end_index: usize,
    keep_recent_tokens: u64,
) -> CutPointResult {
    let cut_points = find_valid_cut_points(entries, start_index, end_index);

    if cut_points.is_empty() {
        return CutPointResult {
            first_kept_entry_index: start_index,
            turn_start_index: -1,
            is_split_turn: false,
        };
    }
    let mut accumulated_tokens = 0u64;
    let mut cut_index = cut_points[0];

    let mut cut_point_cursor = 0usize;
    for index in (start_index..end_index).rev() {
        let entry = &entries[index];
        if !matches!(entry, Entry::Message { .. }) {
            continue;
        }
        let Entry::Message { message, .. } = entry else {
            unreachable!()
        };
        accumulated_tokens += estimate_tokens(message);
        if accumulated_tokens >= keep_recent_tokens {
            for (c, cut) in cut_points.iter().enumerate() {
                if *cut >= index {
                    cut_index = *cut;
                    cut_point_cursor = c;
                    break;
                }
            }
            break;
        }
    }
    // (unused cursor guard: the loop above mirrors upstream's inner scan;
    //  keep the variable to avoid re-scanning precomputed cut points)
    let _ = cut_point_cursor;

    while cut_index > start_index {
        let prev_entry = &entries[cut_index - 1];
        if matches!(prev_entry, Entry::Compaction { .. })
            || matches!(prev_entry, Entry::Message { .. })
        {
            break;
        }
        cut_index -= 1;
    }
    let cut_entry = &entries[cut_index];
    let is_user_message =
        matches!(cut_entry, Entry::Message { message, .. } if message.role() == "user");
    let turn_start_index = if is_user_message {
        -1
    } else {
        find_turn_start_index(entries, cut_index, start_index)
    };

    CutPointResult {
        first_kept_entry_index: cut_index,
        turn_start_index,
        is_split_turn: !is_user_message && turn_start_index != -1,
    }
}

pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

const SUMMARIZATION_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or \"(none)\" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or \"(none)\" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages.";

const UPDATE_SUMMARIZATION_PROMPT: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.

Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed
- UPDATE \"Next Steps\" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages.";

/// Options used by the retry/summarization completion loop. Mirrors the
/// `SimpleStreamOptions` subset upstream compaction passes through.
#[derive(Debug, Clone, Default)]
pub struct SummarizationOptions<'a> {
    pub max_tokens: Option<u64>,
    pub signal: Option<&'a Arc<AtomicBool>>,
    /// Thinking level forwarded to the provider when the model reasons.
    pub reasoning: Option<ThinkingLevel>,
}

/// `completeSimpleWithRetries` — summaries are standalone requests, so
/// routing is isolated and cache writes that cannot be reused are avoided.
#[allow(clippy::too_many_arguments)]
pub async fn complete_simple_with_retries(
    models: &SimpleModels,
    model: &Model,
    context: &pi_ai::types::Context,
    options: SummarizationOptions<'_>,
    retry: Option<&RetryPolicy>,
    callbacks: Option<&RetryCallbacks<'_>>,
) -> AssistantMessage {
    let request_options = SimpleStreamOptions {
        base: pi_ai::types::StreamOptions {
            max_tokens: options.max_tokens,
            cache_retention: Some(CacheRetention::from("none")),
            session_id: Some(new_id()),
            ..Default::default()
        },
        reasoning: options.reasoning,
        ..Default::default()
    };
    let signal = options.signal;
    retry_assistant_call(
        || async {
            models
                .complete_simple(model, context, &request_options)
                .await
        },
        retry,
        signal,
        callbacks,
    )
    .await
}

fn combine_usage(first: &Usage, second: &Usage) -> Usage {
    let cache_write_1h = match (first.cache_write_1h, second.cache_write_1h) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    };
    let reasoning = match (first.reasoning, second.reasoning) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    };
    Usage {
        input: first.input + second.input,
        output: first.output + second.output,
        cache_read: first.cache_read + second.cache_read,
        cache_write: first.cache_write + second.cache_write,
        cache_write_1h,
        reasoning,
        total_tokens: first.total_tokens + second.total_tokens,
        cost: pi_ai::types::Cost {
            input: first.cost.input + second.cost.input,
            output: first.cost.output + second.cost.output,
            cache_read: first.cost.cache_read + second.cost.cache_read,
            cache_write: first.cost.cache_write + second.cost.cache_write,
            total: first.cost.total + second.cost.total,
        },
    }
}

fn extract_file_operations(
    messages: &[AgentMessage],
    entries: &[Entry],
    prev_compaction_index: Option<usize>,
) -> FileOperations {
    let mut file_ops = create_file_ops();
    if let Some(idx) = prev_compaction_index {
        if let Some(Entry::Compaction {
            details: Some(details),
            ..
        }) = entries.get(idx)
        {
            if let Some(read) = details.get("readFiles").and_then(JsonValue::as_array) {
                for f in read {
                    if let Some(s) = f.as_str() {
                        file_ops.read.insert(s.to_string());
                    }
                }
            }
            if let Some(modified) = details.get("modifiedFiles").and_then(JsonValue::as_array) {
                for f in modified {
                    if let Some(s) = f.as_str() {
                        file_ops.edited.insert(s.to_string());
                    }
                }
            }
        }
    }
    for msg in messages {
        extract_file_ops_from_message(msg, &mut file_ops);
    }
    file_ops
}

fn get_message_from_entry(entry: &Entry) -> Option<AgentMessage> {
    match entry {
        Entry::Message { message, .. } => Some(message.clone()),
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

fn get_message_from_entry_for_compaction(entry: &Entry) -> Option<AgentMessage> {
    if matches!(entry, Entry::Compaction { .. }) {
        return None;
    }
    get_message_from_entry(entry)
}

/// Generated compaction data ready to be persisted as a compaction entry.
#[derive(Debug, Clone)]
pub struct CompactResult {
    /// Summary text that replaces compacted history in future context.
    pub summary: String,
    /// Estimated context tokens before compaction.
    pub tokens_before: u64,
    /// Usage from the LLM call(s) that generated this summary, if available.
    pub usage: Option<Usage>,
    /// Retained recent messages stored directly on the compaction entry.
    pub retained_tail: Vec<AgentMessage>,
    /// Optional implementation-specific details stored with the compaction entry.
    pub details: Option<CompactionDetails>,
}

/// Generate or update a conversation summary and return its provider usage.
// Argument count mirrors the upstream signature exactly.
#[allow(clippy::too_many_arguments)]
pub async fn generate_summary_with_usage(
    current_messages: &[AgentMessage],
    models: &SimpleModels,
    model: &Model,
    reserve_tokens: u64,
    signal: Option<&Arc<AtomicBool>>,
    custom_instructions: Option<&str>,
    previous_summary: Option<&str>,
    thinking_level: Option<&str>,
    retry: Option<&RetryPolicy>,
    callbacks: Option<&RetryCallbacks<'_>>,
) -> Result<(String, Usage), CompactionError> {
    let max_tokens = std::cmp::min(
        (reserve_tokens as f64 * 0.8).floor() as u64,
        if model.max_tokens > 0 {
            model.max_tokens
        } else {
            u64::MAX
        },
    );
    let mut base_prompt = if previous_summary.is_some() {
        UPDATE_SUMMARIZATION_PROMPT.to_string()
    } else {
        SUMMARIZATION_PROMPT.to_string()
    };
    if let Some(custom_instructions) = custom_instructions {
        base_prompt = format!("{base_prompt}\n\nAdditional focus: {custom_instructions}");
    }
    let llm_messages = convert_to_llm(current_messages);
    let conversation_text = serialize_conversation(&llm_messages);
    let mut prompt_text = format!("<conversation>\n{conversation_text}\n</conversation>\n\n");
    if let Some(previous_summary) = previous_summary {
        prompt_text += &format!("<previous-summary>\n{previous_summary}\n</previous-summary>\n\n");
    }
    prompt_text += &base_prompt;

    let summarization_messages = vec![Message::User(pi_ai::types::UserContent::blocks(
        vec![ContentBlock::text(prompt_text)],
        pi_ai::types::now_ms(),
    ))];

    let uses_reasoning = model.reasoning && thinking_level.is_some_and(|t| t != "off");
    let context = pi_ai::types::Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        messages: summarization_messages.clone(),
        tools: vec![],
    };
    let option = SummarizationOptions {
        max_tokens: Some(max_tokens),
        signal,
        reasoning: if uses_reasoning {
            thinking_level.and_then(thinking_level_from_str)
        } else {
            None
        },
    };
    let response =
        complete_simple_with_retries(models, model, &context, option, retry, callbacks).await;
    if response.stop_reason() == Some(pi_ai::types::StopReason::Aborted) {
        return Err(CompactionError::new(
            "aborted",
            response.error_message().unwrap_or("Summarization aborted"),
        ));
    }
    if response.stop_reason() == Some(pi_ai::types::StopReason::Error) {
        return Err(CompactionError::new(
            "summarization_failed",
            format!(
                "Summarization failed: {}",
                response.error_message().unwrap_or("Unknown error")
            ),
        ));
    }

    let text_content: String = response
        .content()
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();

    Ok((text_content, response.usage().cloned().unwrap_or_default()))
}

/// Generate or update a conversation summary for compaction.
#[allow(clippy::too_many_arguments)]
pub async fn generate_summary(
    current_messages: &[AgentMessage],
    models: &SimpleModels,
    model: &Model,
    reserve_tokens: u64,
    signal: Option<&Arc<AtomicBool>>,
    custom_instructions: Option<&str>,
    previous_summary: Option<&str>,
    thinking_level: Option<&str>,
    retry: Option<&RetryPolicy>,
    callbacks: Option<&RetryCallbacks<'_>>,
) -> Result<String, CompactionError> {
    let result = generate_summary_with_usage(
        current_messages,
        models,
        model,
        reserve_tokens,
        signal,
        custom_instructions,
        previous_summary,
        thinking_level,
        retry,
        callbacks,
    )
    .await?;
    Ok(result.0)
}

/// Prepared inputs for a compaction run.
#[derive(Debug, Clone)]
pub struct CompactionPreparation {
    /// Messages summarized into the history summary.
    pub messages_to_summarize: Vec<AgentMessage>,
    /// Prefix messages summarized separately when compaction splits a turn.
    pub turn_prefix_messages: Vec<AgentMessage>,
    /// Recent messages retained after compaction and stored on the compaction entry.
    pub retained_tail: Vec<AgentMessage>,
    /// Whether compaction splits a turn.
    pub is_split_turn: bool,
    /// Estimated context tokens before compaction.
    pub tokens_before: u64,
    /// Previous compaction summary used for iterative updates.
    pub previous_summary: Option<String>,
    /// File operations extracted from summarized history.
    pub file_ops: FileOperations,
    /// Settings used to prepare compaction.
    pub settings: CompactionSettings,
}

/// Prepare session entries for compaction, or return `None` when compaction
/// is not applicable.
pub fn prepare_compaction(
    path_entries: &[Entry],
    settings: &CompactionSettings,
) -> Result<Option<CompactionPreparation>, CompactionError> {
    if path_entries.is_empty() || matches!(path_entries.last(), Some(Entry::Compaction { .. })) {
        return Ok(None);
    }

    let mut prev_compaction_index: isize = -1;
    for (index, entry) in path_entries.iter().enumerate().rev() {
        if matches!(entry, Entry::Compaction { .. }) {
            prev_compaction_index = index as isize;
            break;
        }
    }

    let mut previous_summary: Option<String> = None;
    let compactable_entries: Vec<Entry>;
    if prev_compaction_index >= 0 {
        let prev_index = prev_compaction_index as usize;
        if let Entry::Compaction {
            summary,
            retained_tail,
            seq,
            id,
            ..
        } = &path_entries[prev_index]
        {
            previous_summary = Some(summary.clone());
            let mut virtual_entries: Vec<Entry> = Vec::new();
            let mut prev_id = id.clone();
            for (index, message) in retained_tail.iter().enumerate() {
                let vid = format!("{id}:retained:{index}");
                let entry = Entry::Message {
                    id: vid.clone(),
                    seq: *seq,
                    parent_id: Some(prev_id),
                    timestamp: message.timestamp(),
                    message: message.clone(),
                    terminate: None,
                };
                prev_id = vid;
                virtual_entries.push(entry);
            }
            compactable_entries = virtual_entries
                .into_iter()
                .chain(path_entries[prev_index + 1..].iter().cloned())
                .collect();
        } else {
            unreachable!("prev_compaction_index points at a compaction entry");
        }
    } else {
        compactable_entries = path_entries.to_vec();
    }
    let boundary_end = compactable_entries.len();

    let context_messages =
        build_session_context(path_entries, &SessionContextBuildOptions::default()).messages;
    let tokens_before = estimate_context_tokens(&context_messages).tokens;

    let cut_point = find_cut_point(
        &compactable_entries,
        0,
        boundary_end,
        settings.keep_recent_tokens,
    );
    let history_end = if cut_point.is_split_turn {
        cut_point.turn_start_index as usize
    } else {
        cut_point.first_kept_entry_index
    };

    let mut messages_to_summarize: Vec<AgentMessage> = Vec::new();
    for entry in compactable_entries.iter().take(history_end) {
        if let Some(msg) = get_message_from_entry_for_compaction(entry) {
            messages_to_summarize.push(msg);
        }
    }
    let mut turn_prefix_messages: Vec<AgentMessage> = Vec::new();
    if cut_point.is_split_turn {
        for entry in &compactable_entries
            [cut_point.turn_start_index as usize..cut_point.first_kept_entry_index]
        {
            if let Some(msg) = get_message_from_entry_for_compaction(entry) {
                turn_prefix_messages.push(msg);
            }
        }
    }
    let mut retained_tail: Vec<AgentMessage> = Vec::new();
    for entry in &compactable_entries[cut_point.first_kept_entry_index..boundary_end] {
        if let Some(msg) = get_message_from_entry_for_compaction(entry) {
            retained_tail.push(msg);
        }
    }
    let prev_index = if prev_compaction_index >= 0 {
        Some(prev_compaction_index as usize)
    } else {
        None
    };
    let mut file_ops = extract_file_operations(&messages_to_summarize, path_entries, prev_index);
    if cut_point.is_split_turn {
        for msg in &turn_prefix_messages {
            extract_file_ops_from_message(msg, &mut file_ops);
        }
    }

    Ok(Some(CompactionPreparation {
        messages_to_summarize,
        turn_prefix_messages,
        retained_tail,
        is_split_turn: cut_point.is_split_turn,
        tokens_before,
        previous_summary,
        file_ops,
        settings: settings.clone(),
    }))
}

const TURN_PREFIX_SUMMARIZATION_PROMPT: &str =
    "This is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.

Summarize the prefix to provide context for the retained suffix:

## Original Request
[What did the user ask for in this turn?]

## Early Progress
- [Key decisions and work done in the prefix]

## Context for Suffix
- [Information needed to understand the retained recent work]

Be concise. Focus on what's needed to understand the kept suffix.";

#[allow(clippy::too_many_arguments)]
async fn generate_turn_prefix_summary(
    messages: &[AgentMessage],
    models: &SimpleModels,
    model: &Model,
    reserve_tokens: u64,
    signal: Option<&Arc<AtomicBool>>,
    thinking_level: Option<&str>,
    retry: Option<&RetryPolicy>,
    callbacks: Option<&RetryCallbacks<'_>>,
) -> Result<(String, Usage), CompactionError> {
    let max_tokens = std::cmp::min(
        (reserve_tokens as f64 * 0.5).floor() as u64,
        if model.max_tokens > 0 {
            model.max_tokens
        } else {
            u64::MAX
        },
    );
    let llm_messages = convert_to_llm(messages);
    let conversation_text = serialize_conversation(&llm_messages);
    let prompt_text = format!("<conversation>\n{conversation_text}\n</conversation>\n\n{TURN_PREFIX_SUMMARIZATION_PROMPT}");
    let summarization_messages = vec![Message::User(pi_ai::types::UserContent::blocks(
        vec![ContentBlock::text(prompt_text)],
        pi_ai::types::now_ms(),
    ))];
    let uses_reasoning = model.reasoning && thinking_level.is_some_and(|t| t != "off");
    let context = pi_ai::types::Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        messages: summarization_messages.clone(),
        tools: vec![],
    };
    let option = SummarizationOptions {
        max_tokens: Some(max_tokens),
        signal,
        reasoning: if uses_reasoning {
            thinking_level.and_then(thinking_level_from_str)
        } else {
            None
        },
    };
    let response =
        complete_simple_with_retries(models, model, &context, option, retry, callbacks).await;
    if response.stop_reason() == Some(pi_ai::types::StopReason::Aborted) {
        return Err(CompactionError::new(
            "aborted",
            response
                .error_message()
                .unwrap_or("Turn prefix summarization aborted"),
        ));
    }
    if response.stop_reason() == Some(pi_ai::types::StopReason::Error) {
        return Err(CompactionError::new(
            "summarization_failed",
            format!(
                "Turn prefix summarization failed: {}",
                response.error_message().unwrap_or("Unknown error")
            ),
        ));
    }
    let text_content: String = response
        .content()
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    Ok((text_content, response.usage().cloned().unwrap_or_default()))
}

/// Generate compaction summary data from prepared session history.
#[allow(clippy::too_many_arguments)]
pub async fn compact(
    preparation: &CompactionPreparation,
    models: &SimpleModels,
    model: &Model,
    custom_instructions: Option<&str>,
    signal: Option<&Arc<AtomicBool>>,
    thinking_level: Option<&str>,
    retry: Option<&RetryPolicy>,
    callbacks: Option<&RetryCallbacks<'_>>,
) -> Result<CompactResult, CompactionError> {
    let mut summary: String;
    let summary_usage: Usage;

    if preparation.is_split_turn && !preparation.turn_prefix_messages.is_empty() {
        let mut history_text = "No prior history.".to_string();
        let mut history_usage: Option<Usage> = None;
        if !preparation.messages_to_summarize.is_empty() {
            let history_result = generate_summary_with_usage(
                &preparation.messages_to_summarize,
                models,
                model,
                preparation.settings.reserve_tokens,
                signal,
                custom_instructions,
                preparation.previous_summary.as_deref(),
                thinking_level,
                retry,
                callbacks,
            )
            .await?;
            history_text = history_result.0;
            history_usage = Some(history_result.1);
        }
        let turn_prefix_result = generate_turn_prefix_summary(
            &preparation.turn_prefix_messages,
            models,
            model,
            preparation.settings.reserve_tokens,
            signal,
            thinking_level,
            retry,
            callbacks,
        )
        .await?;
        summary = format!(
            "{history_text}\n\n---\n\n**Turn Context (split turn):**\n\n{}",
            turn_prefix_result.0
        );
        summary_usage = match history_usage {
            Some(history_usage) => combine_usage(&history_usage, &turn_prefix_result.1),
            None => turn_prefix_result.1,
        };
    } else {
        let summary_result = generate_summary_with_usage(
            &preparation.messages_to_summarize,
            models,
            model,
            preparation.settings.reserve_tokens,
            signal,
            custom_instructions,
            preparation.previous_summary.as_deref(),
            thinking_level,
            retry,
            callbacks,
        )
        .await?;
        summary = summary_result.0;
        summary_usage = summary_result.1;
    }

    let (read_files, modified_files) = compute_file_lists(&preparation.file_ops);
    summary += &format_file_operations(&read_files, &modified_files);

    Ok(CompactResult {
        summary,
        tokens_before: preparation.tokens_before,
        usage: Some(summary_usage),
        retained_tail: preparation.retained_tail.clone(),
        details: Some(CompactionDetails {
            read_files,
            modified_files,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::CustomAgentMessage;
    use pi_ai::providers::{faux_assistant_message, FauxAssistantOptions};

    fn msg_entry(message: AgentMessage, index: usize) -> Entry {
        Entry::Message {
            id: format!("e{index}"),
            seq: index as u64 + 1,
            parent_id: None,
            timestamp: index as u64,
            message,
            terminate: None,
        }
    }

    fn user_text(text: &str, index: usize) -> Entry {
        msg_entry(
            AgentMessage::Core(Message::User(pi_ai::types::UserContent::string(
                text,
                index as u64,
            ))),
            index,
        )
    }

    fn assistant_text(text: &str, index: usize) -> Entry {
        msg_entry(
            AgentMessage::Core(Message::Assistant(
                faux_assistant_message(
                    vec![ContentBlock::text(text)],
                    FauxAssistantOptions::default(),
                )
                .with_timestamp(index as u64),
            )),
            index,
        )
    }

    fn default_usage(input: i64, output: i64) -> Usage {
        Usage {
            input,
            output,
            cache_read: 0,
            cache_write: 0,
            cache_write_1h: None,
            reasoning: None,
            total_tokens: input + output,
            cost: pi_ai::types::Cost::default(),
        }
    }

    #[test]
    fn calculate_context_tokens_falls_back_to_component_sum() {
        let usage = Usage {
            total_tokens: 0,
            input: 10,
            output: 4,
            cache_read: 3,
            cache_write: 2,
            cache_write_1h: None,
            reasoning: None,
            cost: pi_ai::types::Cost::default(),
        };
        assert_eq!(calculate_context_tokens(&usage), 19);
        let usage = default_usage(10, 4);
        assert_eq!(calculate_context_tokens(&usage), 14);
    }

    #[test]
    fn estimate_tokens_rounds_up_per_4_chars() {
        let msg = AgentMessage::Core(Message::User(pi_ai::types::UserContent::string(
            "abcdefgh", 1,
        )));
        assert_eq!(estimate_tokens(&msg), 2);
        let msg = AgentMessage::Core(Message::User(pi_ai::types::UserContent::string(
            "abcdefghi",
            1,
        )));
        assert_eq!(estimate_tokens(&msg), 3);
    }

    #[test]
    fn estimate_tokens_counts_assistant_thinking_and_tool_calls() {
        let assistant = AgentMessage::Core(Message::Assistant(faux_assistant_message(
            vec![
                ContentBlock::thinking("think"),
                ContentBlock::text("hello"),
                ContentBlock::tool_call("c1", "read", serde_json::json!({"path": "a"})),
            ],
            FauxAssistantOptions::default(),
        )));
        // thinking 5 + text 5 + name 4 + args json len ("{...}") 14 => 28 => 7 tokens
        assert_eq!(estimate_tokens(&assistant), 7);
    }

    #[test]
    fn estimate_tokens_bash_execution() {
        let bash = AgentMessage::Custom(CustomAgentMessage::BashExecution {
            command: "ls".into(),
            output: "a\nb\n".into(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            timestamp: 1,
            exclude_from_context: None,
        });
        assert_eq!(estimate_tokens(&bash), 2); // 2 + 5 = 7 chars => 2 tokens
    }

    #[test]
    fn find_cut_point_keeps_approx_recent_token_budget() {
        // Each message is 20 chars => 5 tokens. Budget 10 keeps ~2 recent
        // messages; the cut lands on a user boundary (index 6).
        let entries: Vec<Entry> = (0..8)
            .map(|i| user_text(&format!("message number {i} padded"), i))
            .collect();
        let cut = find_cut_point(&entries, 0, entries.len(), 10);
        assert_eq!(cut.first_kept_entry_index, 6);
        assert!(!cut.is_split_turn);
        // A large budget compacts everything (fall back to the first cut point).
        let cut = find_cut_point(&entries, 0, entries.len(), 10_000);
        assert_eq!(cut.first_kept_entry_index, 0);
    }

    #[test]
    fn find_cut_point_empty_returns_start() {
        let cut = find_cut_point(&[], 0, 0, 10);
        assert_eq!(cut.first_kept_entry_index, 0);
        assert_eq!(cut.turn_start_index, -1);
        assert!(!cut.is_split_turn);
    }

    #[test]
    fn find_cut_point_does_not_split_between_user_and_assistant() {
        let entries = vec![
            user_text("u1", 0),
            assistant_text("a1", 1),
            user_text("u2", 2),
        ];
        let cut = find_cut_point(&entries, 0, 3, 1);
        // Small budget: cut after first user+assistant pair, on the turn start (u2)
        assert_eq!(cut.first_kept_entry_index, 2);
        assert!(!cut.is_split_turn);
    }

    #[test]
    fn prepare_compaction_none_on_empty_or_trailing_compaction() {
        assert!(prepare_compaction(&[], &DEFAULT_COMPACTION_SETTINGS)
            .unwrap()
            .is_none());
        let entries = vec![
            user_text("u", 0),
            Entry::Compaction {
                id: "c".into(),
                seq: 1,
                parent_id: None,
                timestamp: 1,
                summary: "s".into(),
                retained_tail: vec![],
                tokens_before: 10,
                details: None,
                usage: None,
            },
        ];
        assert!(prepare_compaction(&entries, &DEFAULT_COMPACTION_SETTINGS)
            .unwrap()
            .is_none());
    }

    #[test]
    fn prepare_compaction_reuses_previous_summary_and_virtual_tail() {
        let previous = Entry::Compaction {
            id: "c0".into(),
            seq: 1,
            parent_id: None,
            timestamp: 1,
            summary: "prev summary".into(),
            retained_tail: vec![AgentMessage::Core(Message::User(
                pi_ai::types::UserContent::string("kept tail message", 2),
            ))],
            tokens_before: 100,
            details: Some(serde_json::json!({"readFiles": ["r.txt"], "modifiedFiles": ["m.txt"]})),
            usage: Some(default_usage(5, 5)),
        };
        let mut entries = vec![previous, user_text("new user", 3)];
        entries.push(assistant_text("new assistant", 4));
        let prep = prepare_compaction(
            &entries,
            &CompactionSettings {
                enabled: true,
                reserve_tokens: 16384,
                keep_recent_tokens: 1,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(prep.previous_summary.as_deref(), Some("prev summary"));
        // The virtual retained tail contributes file ops from details.
        assert!(prep.file_ops.read.contains("r.txt"));
        assert!(prep.file_ops.edited.contains("m.txt"));
    }
}
