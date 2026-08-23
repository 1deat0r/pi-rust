//! Session context builder — port of
//! `packages/agent/src/harness/session/context.ts`.
//!
//! Turns a path of session entries into the LLM-facing context: derived
//! session state (thinking level, model, active tools) plus the message list
//! with compaction/branch summaries materialized.

use std::collections::HashMap;

use crate::messages::{create_branch_summary_message, create_compaction_summary_message};
use crate::types::AgentMessage;

use super::types::Entry;

/// `SessionContext` from context.ts.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionContext {
    pub messages: Vec<AgentMessage>,
    pub thinking_level: String,
    pub model: Option<(String, String)>, // (provider, modelId)
    pub active_tool_names: Option<Vec<String>>,
}

/// `ContextEntryTransform` — transforms a path of entries into another path.
pub type ContextEntryTransform = Box<dyn Fn(&[Entry]) -> Vec<Entry>>;

/// `CustomEntryContextMessageProjector` — projects a custom entry into
/// messages (None = no projection).
pub type CustomEntryContextMessageProjector =
    Box<dyn Fn(&Entry, usize, &[Entry]) -> Option<Vec<AgentMessage>>>;

/// `SessionContextBuildOptions`.
#[derive(Default)]
pub struct SessionContextBuildOptions {
    pub entry_transforms: Vec<ContextEntryTransform>,
    pub entry_projectors: HashMap<String, CustomEntryContextMessageProjector>,
}

/// `deriveSessionContextState` — walks path entries accumulating state.
fn derive_session_context_state(path_entries: &[Entry]) -> SessionContextState {
    let mut thinking_level = String::from("off");
    let mut model: Option<(String, String)> = None;
    let mut active_tool_names: Option<Vec<String>> = None;

    for entry in path_entries {
        match entry {
            Entry::ThinkingLevel {
                thinking_level: level,
                ..
            } => {
                thinking_level = level.clone();
            }
            Entry::ModelChange {
                provider, model_id, ..
            } => {
                model = Some((provider.to_string(), model_id.to_string()));
            }
            Entry::Message {
                message: AgentMessage::Core(pi_ai::types::Message::Assistant(a)),
                ..
            } => {
                if let (Some(provider), Some(model_id)) = (a.provider(), a.model()) {
                    model = Some((provider.to_string(), model_id.to_string()));
                }
            }
            Entry::ActiveTools {
                active_tool_names: names,
                ..
            } => {
                active_tool_names = Some(names.clone());
            }
            _ => {}
        }
    }

    SessionContextState {
        thinking_level,
        model,
        active_tool_names,
    }
}

#[derive(Debug, Clone, PartialEq)]
struct SessionContextState {
    thinking_level: String,
    model: Option<(String, String)>,
    active_tool_names: Option<Vec<String>>,
}

/// `defaultContextEntryTransform` — keeps only the last compaction entry (and
/// everything after it).
pub fn default_context_entry_transform(path_entries: &[Entry]) -> Vec<Entry> {
    let mut compaction_index: Option<usize> = None;
    for (index, entry) in path_entries.iter().enumerate().rev() {
        if matches!(entry, Entry::Compaction { .. }) {
            compaction_index = Some(index);
            break;
        }
    }
    match compaction_index {
        None => path_entries.to_vec(),
        Some(index) => {
            let mut out = Vec::with_capacity(path_entries.len() - index);
            out.push(path_entries[index].clone());
            out.extend_from_slice(&path_entries[index + 1..]);
            out
        }
    }
}

/// `buildContextEntries` — applies the default compaction transform plus any
/// additional transforms in order.
pub fn build_context_entries(
    path_entries: &[Entry],
    options: &SessionContextBuildOptions,
) -> Vec<Entry> {
    let mut entries = default_context_entry_transform(path_entries);
    for transform in &options.entry_transforms {
        entries = transform(&entries);
    }
    entries
}

/// `sessionEntryToContextMessages` — materializes a single context entry.
pub fn session_entry_to_context_messages(
    entry: &Entry,
    index: usize,
    entries: &[Entry],
    options: &SessionContextBuildOptions,
) -> Vec<AgentMessage> {
    match entry {
        Entry::Message { message, .. } => {
            if let AgentMessage::Core(pi_ai::types::Message::Assistant(a)) = message {
                if a.stop_reason() == Some(pi_ai::types::StopReason::Deferred) {
                    return Vec::new();
                }
            }
            vec![message.clone()]
        }
        Entry::Compaction {
            summary,
            tokens_before,
            timestamp,
            retained_tail,
            ..
        } => {
            let mut out = Vec::with_capacity(1 + retained_tail.len());
            out.push(create_compaction_summary_message(
                summary.clone(),
                *tokens_before,
                *timestamp,
            ));
            out.extend(retained_tail.clone());
            out
        }
        Entry::BranchSummary {
            summary,
            from_id,
            timestamp,
            ..
        } => {
            if summary.is_empty() {
                Vec::new()
            } else {
                vec![create_branch_summary_message(
                    summary.clone(),
                    from_id.clone(),
                    *timestamp,
                )]
            }
        }
        Entry::Custom { custom_type, .. } => match options.entry_projectors.get(custom_type) {
            Some(projector) => projector(entry, index, entries).unwrap_or_default(),
            None => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// `buildSessionContext` — the full context build.
pub fn build_session_context(
    path_entries: &[Entry],
    options: &SessionContextBuildOptions,
) -> SessionContext {
    let state = derive_session_context_state(path_entries);
    let context_entries = build_context_entries(path_entries, options);
    let mut messages = Vec::new();
    for (index, entry) in context_entries.iter().enumerate() {
        messages.extend(session_entry_to_context_messages(
            entry,
            index,
            &context_entries,
            options,
        ));
    }
    SessionContext {
        messages,
        thinking_level: state.thinking_level,
        model: state.model,
        active_tool_names: state.active_tool_names,
    }
}
