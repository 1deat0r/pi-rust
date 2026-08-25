//! Interactive TUI mode — port of `packages/coding-agent/src/modes/interactive/
//! interactive-mode.ts` using the ported pi-tui component surface.
//!
//! Drives the Editor (multi-line, history, undo, autocomplete), the Markdown
//! transcript, slash-command dispatch with model/thinking/theme/settings
//! selectors, a footer, and the agent turn loop.

use std::sync::{Arc, Mutex};

use pi_agent::harness::{AgentHarness, AgentHarnessOptions, HarnessTool};
use pi_agent::session::jsonl::repo::CreateOptions;
use pi_agent::session::session::Session as JsonlSession;
use pi_agent::session::state::ForkOptions;
use pi_agent::session::types::EntryNoStats;
use pi_agent::session::JsonlSessionRepo;
use pi_ai::model::Model;
use pi_ai::types::{AssistantMessageEvent, Message};
use serde_json::{json, Value};

use crate::args::Args;
use crate::config;
use crate::core::extensions::{install_tools, load_for_mode, LoadedExtensions};
use crate::core::settings::SettingsManager;
use crate::interactive as it;
use crate::interactive::footer::{self, FooterData};
use crate::interactive::selectors::ListSelector;
use crate::interactive::settings_panel::SettingsPanel;
use crate::interactive::slash::SlashKind;
use crate::interactive::{Modal, SubmitAction};

use pi_tui::components::{Editor, Markdown, Text};
use pi_tui::keys::{parse_key, TuiKey};

use pi_tui::terminal::TerminalBackend;
use pi_tui::tui::{Component, SharedComponent, Tree};

/// Interactive session runtime (reuses the run/RPC wiring).
struct InteractiveRuntime {
    cwd: String,
    models: pi_ai::models::Models,
    /// Shared faux core for deterministic mode tests and the local provider;
    /// registering it through Models keeps deferred hooks available to the
    /// interactive runtime instead of bypassing the provider facade.
    faux_core: Option<pi_ai::providers::FauxProviderCore>,
    provider: String,
    model: Model,
    messages: Vec<pi_agent::types::AgentMessage>,
    session: JsonlSession<pi_agent::fs::StdFileSystem>,
    repo: JsonlSessionRepo<pi_agent::fs::StdFileSystem>,
    session_root: String,
    session_id: String,
    session_name: Option<String>,
    system_prompt: Option<String>,
    tools_enabled: bool,
    builtin_tools_enabled: bool,
    extensions: LoadedExtensions,
    auto_resize_images: bool,
    block_images: bool,
    /// Number of in-memory messages already persisted into the current
    /// session. Session-switch operations (resume/fork/clone) advance it so
    /// the exit persist only appends messages added after the switch.
    persisted_until: usize,
    /// Serialized session entries used to derive cache notices and cumulative
    /// footer/session usage before the deferred exit persist runs.
    cache_entries: Vec<Value>,
}

impl Drop for InteractiveRuntime {
    fn drop(&mut self) {
        self.extensions
            .runner
            .invalidate(Some("interactive mode shutdown"));
    }
}

/// Own raw/alternate-screen cleanup for every exit after the TUI activates.
/// The explicit cleanup at the normal loop boundary remains useful for prompt
/// handoff, while this guard covers startup failures, input errors, and early
/// returns without leaving the parent shell in raw mode.
struct InteractiveTerminalGuard {
    terminal: Arc<Mutex<TerminalBackend>>,
}

impl Drop for InteractiveTerminalGuard {
    fn drop(&mut self) {
        let mut terminal = match self.terminal.lock() {
            Ok(terminal) => terminal,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _ = terminal.leave_raw();
    }
}

fn should_exit_on_key(key: &TuiKey, editor_text: &str) -> bool {
    key.ctrl && key.base == "d" && editor_text.is_empty()
}

fn resumable_sessions(
    sessions: Vec<pi_agent::session::types::SessionMetadata>,
    current_id: &str,
) -> Vec<pi_agent::session::types::SessionMetadata> {
    sessions
        .into_iter()
        .filter(|session| session.id != current_id)
        .collect()
}

/// Build the tools for one interactive turn and refresh the extension host
/// catalog from the exact set that is available to that turn.
fn interactive_turn_tools(runtime: &InteractiveRuntime) -> Vec<pi_agent::tools::AgentTool> {
    let mut tools = if runtime.tools_enabled && runtime.builtin_tools_enabled {
        vec![
            pi_agent::tools::bash_tool(runtime.cwd.clone()),
            pi_agent::tools::read_tool_with_options(
                runtime.cwd.clone(),
                pi_agent::tools::image::ProcessImageOptions {
                    auto_resize_images: runtime.auto_resize_images,
                    ..Default::default()
                },
            ),
            pi_agent::tools::write_tool(runtime.cwd.clone()),
            pi_agent::tools::edit_tool(runtime.cwd.clone()),
            crate::core::tools::ls_tool(runtime.cwd.clone()),
            crate::core::tools::find_tool(runtime.cwd.clone()),
            crate::core::tools::grep_tool(runtime.cwd.clone()),
        ]
    } else {
        Vec::new()
    };
    install_tools(&runtime.extensions, &mut tools, runtime.tools_enabled);
    tools
}

/// Stream a prompt through the agent loop, observing raw events.
async fn stream_turn(
    runtime: &mut InteractiveRuntime,
    message: String,
    on_event: Arc<dyn Fn(&AssistantMessageEvent) + Send + Sync>,
) -> Result<Vec<pi_agent::types::AgentMessage>, String> {
    let prompt = pi_agent::agent::user_text_prompt(message.clone(), pi_ai::types::now_ms());
    runtime.messages.push(prompt.clone());
    let tools = interactive_turn_tools(runtime);
    let models = runtime.models.clone();
    let api_key = std::env::var(config::ENV_KEY).ok();
    let stream_options = pi_ai::types::StreamOptions {
        base: pi_ai::types::ProviderRequestOptions {
            api_key,
            ..Default::default()
        },
        ..Default::default()
    };
    let provider = runtime.provider.clone();
    let provider_uses_oauth = models
        .get_provider(&provider)
        .is_some_and(|registered| registered.auth.oauth.is_some());
    let stream_fn: crate::run::StreamFn = if provider == "faux" {
        let core = runtime.faux_core.clone().unwrap_or_else(|| {
            crate::core::model_runtime::register_faux_provider(
                &models,
                &pi_ai::providers::RegisterFauxProviderOptions::default(),
            )
        });
        core.set_responses(vec![pi_ai::providers::FauxResponseStep::Message(
            pi_ai::providers::faux_assistant_message(
                vec![pi_ai::types::ContentBlock::text(format!(
                    "faux response to: {message}"
                ))],
                pi_ai::providers::FauxAssistantOptions::default(),
            ),
        )]);
        let stream_models = models.clone();
        let faux_stream_options = stream_options.clone();
        Arc::new(move |model, ctx| stream_models.stream(model, ctx, Some(&faux_stream_options)))
    } else {
        Arc::new(move |model, ctx| models.stream(model, ctx, Some(&stream_options)))
    };
    let storage = Arc::new(Mutex::new(
        pi_agent::session::memory::InMemorySessionStorage::new(
            pi_agent::session::memory::in_memory_metadata("interactive-turn", None),
        ),
    ));
    let session = pi_agent::session::Session::<pi_agent::fs::MemoryFs>::from_in_memory(storage);
    let mut options = AgentHarnessOptions::new(session, runtime.model.clone());
    options.stream_fn = Some(stream_fn);
    options.system_prompt = runtime.system_prompt.clone();
    options.block_images = runtime.block_images;
    options.tools = Some(tools.iter().map(HarnessTool::from_agent_tool).collect());
    let (mut harness, _suspended) = AgentHarness::create(options)
        .await
        .map_err(|error| error.to_string())?;
    if harness
        .set_agent_messages(runtime.messages[..runtime.messages.len() - 1].to_vec())
        .await
        .is_err()
    {
        return Err("failed to seed interactive harness transcript".to_string());
    }
    let (mut new_messages, rich_events) = harness
        .run_prompt_with_events(vec![prompt])
        .await
        .map_err(|error| error.to_string())?;
    for event in rich_events {
        if let pi_agent::rich_agent::RichAgentEvent::MessageUpdate {
            mut assistant_message_event,
            ..
        } = event
        {
            if let AssistantMessageEvent::Error { error_message, .. } = &mut assistant_message_event
            {
                crate::core::auth_guidance::rewrite_assistant_error(
                    error_message,
                    &provider,
                    provider_uses_oauth,
                );
            }
            on_event(&assistant_message_event);
        }
    }
    for message in &mut new_messages {
        if let pi_agent::types::AgentMessage::Core(Message::Assistant(assistant)) = message {
            crate::core::auth_guidance::rewrite_assistant_error(
                assistant,
                &provider,
                provider_uses_oauth,
            );
        }
    }
    persist_messages_checked(&mut runtime.session, &new_messages).await?;
    for m in new_messages.iter().skip(1) {
        runtime.messages.push(m.clone());
    }
    runtime.persisted_until = runtime.messages.len();
    Ok(new_messages)
}

/// Run the shared interactive compaction path. Automatic compaction observes
/// the threshold; `/compact` forces the same persistence/context replacement
/// path and may provide custom summarization instructions.
async fn compact_interactive(
    runtime: &mut InteractiveRuntime,
    custom_instructions: Option<&str>,
    force: bool,
) -> Result<bool, String> {
    let operation = if force { "compact" } else { "auto-compact" };
    let settings = pi_agent::harness::compaction::DEFAULT_COMPACTION_SETTINGS;
    if !force {
        let estimate = pi_agent::harness::compaction::estimate_context_tokens(&runtime.messages);
        if !pi_agent::harness::compaction::should_compact(
            estimate.tokens,
            runtime.model.context_window,
            &settings,
        ) {
            return Ok(false);
        }
    }
    let entries = runtime
        .session
        .find_entries(&pi_agent::session::state::EntryQuery {
            order: Some(pi_agent::session::state::EntryOrder::OldestFirst),
            id: None,
            entry_type: None,
            custom_type: None,
            cursor: None,
            limit: None,
        })
        .await
        .map_err(|e| format!("{operation}: read entries: {e}"))?;
    let Some(preparation) = pi_agent::harness::compaction::prepare_compaction(&entries, &settings)
        .map_err(|e| format!("{operation}: prepare: {e}"))?
    else {
        return Ok(false);
    };
    // Summarize through the models facade (same seam as the RPC compact).
    let models = runtime.models.clone();
    let complete_simple_fn: pi_agent::harness::CompleteSimpleFn =
        Arc::new(move |model, ctx, opts| {
            let models = models.clone();
            let opts = opts.clone();
            let model = model.clone();
            let ctx = ctx.clone();
            Box::pin(async move { models.complete_simple(&model, &ctx, Some(&opts)).await })
        });
    let options = pi_agent::harness::SimpleModels { complete_simple_fn };
    let retry = pi_ai::utils::retry::RetryPolicy {
        enabled: false,
        max_retries: 0,
        base_delay_ms: 0,
    };
    let result = pi_agent::harness::compaction::compact(
        &preparation,
        &options,
        &runtime.model,
        custom_instructions,
        None,
        None,
        Some(&retry),
        None,
    )
    .await
    .map_err(|e| format!("{operation}: {e}"))?;

    // Replace the in-memory context: summary message + retained tail.
    let summary_msg = pi_agent::agent::user_text_prompt(
        format!("[Compaction summary]\n{}", result.summary),
        pi_ai::types::now_ms(),
    );
    let mut replaced = vec![summary_msg];
    replaced.extend(result.retained_tail.clone());
    runtime.messages = replaced;

    // Persist a compaction entry so the session file records the summary.
    runtime
        .session
        .append_entry(
            EntryNoStats::Compaction {
                id: format!("c-{}", pi_agent::session::new_id()),
                summary: result.summary.clone(),
                retained_tail: result.retained_tail,
                tokens_before: result.tokens_before,
                details: None,
                usage: result.usage.clone(),
            },
            "main",
        )
        .await
        .map_err(|e| format!("{operation}: persist: {e}"))?;
    // Keep a reset marker in the deferred display-entry shadow so the next
    // request cannot be mistaken for a continuation of the pre-compaction
    // prompt cache.
    runtime.cache_entries.push(json!({
        "type": "compaction",
        "timestamp": pi_ai::types::now_ms(),
        "usage": result.usage,
    }));
    runtime.persisted_until = runtime.messages.len();
    Ok(true)
}

/// Auto-compaction (upstream `core/compaction/` loop): after a turn, if the
/// estimated context tokens exceed the model's window minus the reserve,
/// summarize the history through the models facade and replace the in-memory
/// context with the summary plus the retained tail. Returns true when
/// compaction ran.
async fn maybe_auto_compact(runtime: &mut InteractiveRuntime) -> Result<bool, String> {
    compact_interactive(runtime, None, false).await
}

/// Short cwd for banners (home-relative like the footer).
fn meta_short_cwd(cwd: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = cwd.strip_prefix(&home) {
            if rest.is_empty() {
                return "~".to_string();
            }
            return format!("~{rest}");
        }
    }
    cwd.to_string()
}

/// Aggregate cumulative usage + the latest assistant turn's cache-hit rate
/// from the in-memory transcript, for the footer token totals (upstream
/// `FooterComponent.render`).
#[cfg(test)]
fn footer_usage_from_messages(
    messages: &[pi_agent::types::AgentMessage],
) -> (Option<crate::core::usage_totals::UsageTotals>, Option<f64>) {
    use crate::core::usage_totals as ut;
    let mut totals = ut::create_usage_totals();
    let mut saw_any = false;
    let mut cache_hit_rate: Option<f64> = None;
    for message in messages {
        let assistant = match message {
            pi_agent::types::AgentMessage::Core(pi_ai::types::Message::Assistant(a)) => a,
            _ => continue,
        };
        let Some(usage) = assistant.usage() else {
            continue;
        };
        saw_any = true;
        ut::add_usage_to_totals(&mut totals, usage);
        let prompt_tokens = usage.input + usage.cache_read + usage.cache_write;
        cache_hit_rate = if prompt_tokens > 0 {
            Some((usage.cache_read as f64 / prompt_tokens as f64) * 100.0)
        } else {
            None
        };
    }
    if saw_any {
        (Some(totals), cache_hit_rate)
    } else {
        (None, None)
    }
}

/// Rehydrate in-memory messages + transcript from a session's message
/// entries (oldest first), mirroring the RPC get_entries load path.
async fn rehydrate_transcript(
    runtime: &InteractiveRuntime,
    transcript_md: &Arc<Mutex<Markdown>>,
    hide_thinking: bool,
) -> (Vec<pi_agent::types::AgentMessage>, Vec<Value>) {
    let entries = runtime
        .session
        .find_entries(&pi_agent::session::state::EntryQuery {
            order: Some(pi_agent::session::state::EntryOrder::OldestFirst),
            id: None,
            entry_type: None,
            custom_type: None,
            cursor: None,
            limit: None,
        })
        .await
        .unwrap_or_default();
    let mut messages = Vec::new();
    for entry in &entries {
        if let pi_agent::session::types::Entry::Message { message, .. } = entry {
            messages.push(message.clone());
        }
    }
    let cache_entries = entries
        .iter()
        .filter_map(|entry| serde_json::to_value(entry).ok())
        .collect();
    transcript_md
        .lock()
        .unwrap()
        .set_text(it::compose_transcript(&messages, hide_thinking, ""));
    (messages, cache_entries)
}

/// Serialize one in-memory agent message into the session-entry shape used by
/// the cache and usage analyzers. Interactive turns are persisted on exit, so
/// keeping this shadow list lets the footer and `/session` stay current.
fn cache_entry_from_message(message: &pi_agent::types::AgentMessage) -> Option<Value> {
    let timestamp = match message {
        pi_agent::types::AgentMessage::Core(Message::User(user)) => user.timestamp(),
        pi_agent::types::AgentMessage::Core(Message::Assistant(assistant)) => assistant.timestamp(),
        pi_agent::types::AgentMessage::Core(Message::ToolResult(tool)) => tool.timestamp(),
        pi_agent::types::AgentMessage::Custom(custom) => custom.timestamp(),
    };
    Some(json!({
        "type": "message",
        "timestamp": timestamp,
        "message": serde_json::to_value(message).ok()?,
    }))
}

fn append_cache_entries_from_messages(
    entries: &mut Vec<Value>,
    messages: &[pi_agent::types::AgentMessage],
) {
    entries.extend(messages.iter().filter_map(cache_entry_from_message));
}

/// Format one significant cache miss using the upstream labels and thresholds.
fn format_cache_miss_notice(miss: &crate::core::cache_stats::CacheMiss) -> Option<String> {
    if miss.missed_tokens < crate::core::cache_stats::CACHE_NOTICE_MIN_TOKENS
        && miss.missed_cost < crate::core::cache_stats::CACHE_NOTICE_MIN_COST
    {
        return None;
    }
    let cost = if miss.missed_cost >= 0.01 {
        format!(" (~${:.2})", miss.missed_cost)
    } else {
        String::new()
    };
    let rebilled = format!(
        "{} tokens re-billed{}",
        it::messages::format_tokens(miss.missed_tokens),
        cost
    );
    let label = if miss.model_changed {
        "Cache miss after model switch".to_string()
    } else if miss.idle_ms >= crate::core::cache_stats::CACHE_TTL_MS {
        format!(
            "Cache miss after {}m idle",
            (miss.idle_ms as f64 / 60_000.0).round() as u64
        )
    } else {
        "Cache miss".to_string()
    };
    Some(format!("⚠ {label}: {rebilled}"))
}

/// Re-derive transcript notices from the current shadow session entries. The
/// notices are keyed by the assistant entry timestamp, not vector position,
/// so compaction can replace the in-memory context without misplacing them.
fn cache_notice_timestamps(entries: &[Value]) -> Vec<(u64, String)> {
    let misses = crate::core::cache_stats::collect_cache_misses(
        entries,
        &crate::core::cache_stats::NoPrices,
    );
    misses
        .into_iter()
        .filter_map(|(index, miss)| {
            let entry = entries.get(index)?;
            if entry.get("type").and_then(Value::as_str) != Some("message")
                || entry
                    .get("message")
                    .and_then(|message| message.get("role"))
                    .and_then(Value::as_str)
                    != Some("assistant")
            {
                return None;
            }
            let timestamp = entry.get("timestamp").and_then(Value::as_u64)?;
            Some((timestamp, format_cache_miss_notice(&miss)?))
        })
        .collect()
}

/// Aggregate cumulative usage from serialized entries, including summary and
/// tool-result usage that is not present in the post-compaction context.
fn footer_usage_from_entries(
    entries: &[Value],
) -> (Option<crate::core::usage_totals::UsageTotals>, Option<f64>) {
    use crate::core::usage_totals as ut;
    let mut totals = ut::create_usage_totals();
    let mut saw_any = false;
    let mut cache_hit_rate = None;
    for entry in entries {
        match ut::parse_session_entry(entry) {
            ut::SessionEntryUsageView::Assistant { usage, .. } => {
                if let Some(usage) = usage {
                    saw_any = true;
                    ut::add_usage_to_totals(&mut totals, &usage);
                    let prompt_tokens = usage.input + usage.cache_read + usage.cache_write;
                    cache_hit_rate = if prompt_tokens > 0 {
                        Some((usage.cache_read as f64 / prompt_tokens as f64) * 100.0)
                    } else {
                        None
                    };
                }
            }
            ut::SessionEntryUsageView::ToolResult { usage }
            | ut::SessionEntryUsageView::Summary { usage } => {
                saw_any = true;
                ut::add_usage_to_totals(&mut totals, &usage);
            }
            ut::SessionEntryUsageView::Other => {}
        }
    }
    if saw_any {
        (Some(totals), cache_hit_rate)
    } else {
        (None, None)
    }
}

fn format_cache_waste_line(waste: crate::core::cache_stats::CacheWasteTotals) -> Option<String> {
    if waste.missed_tokens == 0 {
        return None;
    }
    let miss_label = if waste.miss_count == 1 {
        "1 miss".to_string()
    } else {
        format!("{} misses", waste.miss_count)
    };
    let detail = format!("{} tokens, {}", waste.missed_tokens, miss_label);
    if waste.missed_cost >= 0.0001 {
        Some(format!(
            "Cache Re-billed: ${:.3} ({detail})",
            waste.missed_cost
        ))
    } else {
        Some(format!("Cache Re-billed: {detail}"))
    }
}

fn session_status(runtime: &InteractiveRuntime) -> String {
    let waste = crate::core::cache_stats::compute_cache_waste(
        &runtime.cache_entries,
        &crate::core::cache_stats::NoPrices,
    );
    let (usage, _) = footer_usage_from_entries(&runtime.cache_entries);
    let mut status = format!(
        "session {} — {} messages in transcript",
        runtime.session_id.get(..8).unwrap_or(&runtime.session_id),
        runtime.messages.len()
    );
    if let Some(usage) = usage {
        status.push_str(&format!(
            "\nusage: {} tokens, ${:.3}",
            usage.input + usage.output + usage.cache_read + usage.cache_write,
            usage.cost
        ));
    }
    if let Some(line) = format_cache_waste_line(waste) {
        status.push('\n');
        status.push_str(&line);
    }
    status
}

/// Append in-memory messages to a session's main lane (idempotent per call).
async fn persist_messages_checked(
    session: &mut JsonlSession<pi_agent::fs::StdFileSystem>,
    messages: &[pi_agent::types::AgentMessage],
) -> Result<(), String> {
    for message in messages {
        session
            .append_entry(
                EntryNoStats::Message {
                    id: format!("m-{}", pi_agent::session::new_id()),
                    message: message.clone(),
                    terminate: None,
                },
                "main",
            )
            .await
            .map_err(|error| format!("persist interactive turn: {error}"))?;
    }
    Ok(())
}

async fn persist_messages(
    session: &mut JsonlSession<pi_agent::fs::StdFileSystem>,
    messages: &[pi_agent::types::AgentMessage],
) {
    let _ = persist_messages_checked(session, messages).await;
}

/// Compact text for a message entry (upstream truncates in tree labels).
fn short_text(message: &pi_agent::types::AgentMessage) -> String {
    match message {
        pi_agent::types::AgentMessage::Core(pi_ai::types::Message::Assistant(a)) => {
            let mut text = String::new();
            for block in a.content() {
                if let pi_ai::types::ContentBlock::Text { text: t, .. } = block {
                    text.push_str(t);
                }
            }
            let trimmed = text.trim();
            trimmed.chars().take(40).collect()
        }
        pi_agent::types::AgentMessage::Core(pi_ai::types::Message::User(u)) => match u.content() {
            pi_ai::types::UserContentBody::String(s) => s.chars().take(40).collect(),
            pi_ai::types::UserContentBody::Blocks(blocks) => {
                let mut text = String::new();
                for block in blocks {
                    if let pi_ai::types::ContentBlock::Text { text: t, .. } = block {
                        text.push_str(t);
                    }
                }
                text.chars().take(40).collect()
            }
        },
        pi_agent::types::AgentMessage::Core(pi_ai::types::Message::ToolResult(tr)) => {
            let mut text = String::new();
            for block in tr.content() {
                if let pi_ai::types::ContentBlock::Text { text: t, .. } = block {
                    text.push_str(t);
                }
            }
            format!(
                "tool({}) {}",
                tr.tool_name(),
                text.chars().take(24).collect::<String>()
            )
        }
        _ => String::new(),
    }
}

/// Truncate a string for compact tree labels.
fn short_truncate(text: &str) -> String {
    text.chars().take(40).collect()
}

/// Label for non-message entry types in the tree view.
fn entry_type_label(entry: &pi_agent::session::types::Entry) -> &'static str {
    match entry {
        pi_agent::session::types::Entry::Message { .. } => "message",
        pi_agent::session::types::Entry::ModelChange { .. } => "model_change",
        pi_agent::session::types::Entry::ThinkingLevel { .. } => "thinking_level",
        pi_agent::session::types::Entry::ActiveTools { .. } => "active_tools",
        pi_agent::session::types::Entry::Compaction { .. } => "compaction",
        pi_agent::session::types::Entry::BranchSummary { .. } => "branch_summary",
        pi_agent::session::types::Entry::Custom { .. } => "custom",
    }
}

/// Run the upstream `/share` flow: gh auth check -> export session HTML ->
/// `gh gist create --public=false` -> viewer URL. Returns the final status
/// message or an error. All gh calls are spawn_blocking + timeout so a
/// hanging gh never blocks the UI loop.
async fn run_gh(args: Vec<String>) -> Result<std::process::Output, String> {
    let layered = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tokio::task::spawn_blocking(move || {
            let mut cmd = std::process::Command::new("gh");
            cmd.args(&args);
            cmd.output()
        }),
    )
    .await
    .map_err(|_| "gh command timed out".to_string())?;
    match layered {
        Ok(res) => res.map_err(|e| format!("gh spawn failed: {e}")),
        Err(e) => Err(format!("gh spawn failed: {e}")),
    }
}

/// Run the upstream `/share` flow: gh auth check -> export session HTML ->
/// `gh gist create --public=false` -> viewer URL. Returns the final status
/// message or an error.
async fn run_share(runtime: &InteractiveRuntime, dry_run: bool) -> Result<String, String> {
    if dry_run {
        return Ok("PI_SHARE_DRY_RUN=1: /share skipped".to_string());
    }
    let gh_auth = match run_gh(vec!["auth".to_string(), "status".to_string()]).await {
        Ok(out) => out,
        Err(_) => {
            return Err(
                "GitHub CLI (gh) is not installed. Install it from https://cli.github.com/"
                    .to_string(),
            )
        }
    };
    if !gh_auth.status.success() {
        return Err("GitHub CLI is not logged in. Run 'gh auth login' first.".to_string());
    }
    let meta = runtime.session.get_metadata().await;
    let tmp_file = std::env::temp_dir().join(format!("pi-share-{}.html", std::process::id()));
    let tmp_path = tmp_file.to_string_lossy().into_owned();
    crate::core::export_html::export_session_file(&meta.path, Some(&tmp_path), None)
        .map_err(|e| format!("failed to export session: {e}"))?;
    let gh_gist = run_gh(vec![
        "gist".to_string(),
        "create".to_string(),
        "--public=false".to_string(),
        tmp_path.clone(),
    ])
    .await?;
    let _ = std::fs::remove_file(&tmp_path);
    if !gh_gist.status.success() {
        return Err(format!(
            "failed to create gist: {}",
            String::from_utf8_lossy(&gh_gist.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&gh_gist.stdout);
    let gist_url = stdout.lines().next().unwrap_or("").trim().to_string();
    let gist_id = gist_url.rsplit('/').next().unwrap_or("").to_string();
    let viewer = std::env::var("PI_SHARE_VIEWER_URL")
        .unwrap_or_else(|_| "https://pi.dev/session/".to_string());
    Ok(format!("Share URL: {viewer}#{gist_id}\nGist: {gist_url}"))
}

/// TUI-backed auth interaction (upstream `AuthInteraction`): notifications go
/// to the status banner; prompts temporarily leave raw mode to read a line
/// from stdin, then re-enter raw mode.
struct TuiAuthInteraction {
    banner: Arc<Mutex<String>>,
    terminal: Arc<Mutex<TerminalBackend>>,
}

impl pi_ai::auth::AuthInteraction for TuiAuthInteraction {
    fn prompt(&self, prompt: &pi_ai::auth::AuthPrompt) -> Result<String, String> {
        let message = match prompt {
            pi_ai::auth::AuthPrompt::Text {
                message,
                placeholder,
            } => {
                let mut m = message.clone();
                if let Some(p) = placeholder {
                    m.push_str(&format!(" ({p})"));
                }
                m
            }
            pi_ai::auth::AuthPrompt::Secret { message, .. } => message.clone(),
            pi_ai::auth::AuthPrompt::ManualCode {
                message,
                placeholder,
            } => {
                let mut m = message.clone();
                if let Some(p) = placeholder {
                    m.push_str(&format!(" ({p})"));
                }
                m
            }
            pi_ai::auth::AuthPrompt::Select { message, options } => {
                let mut m = message.clone();
                for (i, opt) in options.iter().enumerate() {
                    m.push_str(&format!("\n  {}. {}", i + 1, opt.label));
                }
                m
            }
        };
        let mut terminal = self.terminal.lock().unwrap();
        terminal
            .leave_raw()
            .map_err(|e| format!("leave raw: {e}"))?;
        println!("\n{message}");
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| format!("read input: {e}"))?;
        terminal
            .enter_raw()
            .map_err(|e| format!("enter raw: {e}"))?;
        Ok(line.trim().to_string())
    }

    fn notify(&self, event: &pi_ai::auth::AuthEvent) {
        let msg = match event {
            pi_ai::auth::AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                ..
            } => {
                format!("Open {verification_uri} and enter code: {user_code}")
            }
            pi_ai::auth::AuthEvent::AuthUrl { url, .. } => {
                format!("Open this URL to sign in: {url}")
            }
            pi_ai::auth::AuthEvent::Progress { message } => message.clone(),
            pi_ai::auth::AuthEvent::Info { message, .. } => message.clone(),
        };
        *self.banner.lock().unwrap() = msg;
    }
}

/// Run the upstream `/login <provider>` OAuth flow: find the provider in the
/// models registry, run its OAuth login, store the credential. Returns the
/// final status message or an error.
async fn run_oauth_login(
    models: &pi_ai::models::Models,
    provider_ref: Option<&str>,
    banner: Arc<Mutex<String>>,
    terminal: Arc<Mutex<TerminalBackend>>,
) -> Result<String, String> {
    let providers: Vec<pi_ai::models::Provider> = models
        .get_providers()
        .into_iter()
        .filter(|p| p.auth.oauth.is_some())
        .filter(|p| match provider_ref {
            Some(r) => p.id == r || p.name.as_str() == r,
            None => true,
        })
        .collect();
    if providers.is_empty() {
        return Err(match provider_ref {
            Some(r) => format!("no OAuth login available for provider {r:?}"),
            None => "no OAuth-capable providers registered".to_string(),
        });
    }
    let provider = &providers[0];
    let oauth = provider.auth.oauth.as_ref().expect("filtered for oauth");
    let interaction = TuiAuthInteraction { banner, terminal };
    let credential = oauth.login(&interaction).await?;
    let auth = crate::core::auth_storage::AuthStorage::create(config::get_auth_path());
    let opts = crate::core::auth_storage::AuthOperationOptions::default();
    let cred = crate::core::auth_storage::Credential::OAuth {
        access: credential.access,
        refresh: credential.refresh,
        expires: credential.expires,
        extra: credential.extra,
    };
    let provider_id = provider.id.clone();
    auth.modify(
        &provider_id,
        move |_| {
            let cred = cred.clone();
            Box::pin(async move { Ok(Some(cred)) })
        },
        &opts,
    )
    .await?;
    Ok(format!("logged in to {provider_id} via OAuth"))
}

/// Wrap a modal in a renderable SharedComponent for the frame.
fn modal_shared(modal: &mut Modal) -> SharedComponent {
    match modal {
        Modal::Model(sel) | Modal::Thinking(sel) | Modal::Theme(sel) => {
            sel.clone() as SharedComponent
        }
        Modal::Settings(panel) => panel.clone() as SharedComponent,
        Modal::Resume(sel, _) => sel.clone() as SharedComponent,
    }
}

/// The interactive main loop. Returns Ok(()) on clean exit.
pub async fn run_interactive_mode(args: &Args, settings: SettingsManager) -> Result<(), String> {
    let mut settings = settings;
    let cwd = config::cwd();
    let models = crate::core::model_registry::builtin_models();
    let provider = crate::run::resolve_run_provider(args.provider.as_deref(), &settings);
    let model_hint = crate::run::resolve_run_model(
        args.model.as_deref(),
        &settings,
        !crate::run::has_explicit_provider(args.provider.as_deref()),
    );
    let faux_core = if provider == "faux" {
        Some(crate::core::model_runtime::register_faux_provider(
            &models,
            &pi_ai::providers::RegisterFauxProviderOptions::default(),
        ))
    } else {
        None
    };
    let model = if provider == "faux" {
        let core = faux_core.as_ref().expect("faux core registered");
        match model_hint.as_deref() {
            Some(hint) => core
                .get_model(Some(hint.rsplit('/').next().unwrap_or(hint)))
                .cloned()
                .ok_or_else(|| format!("unknown faux model {hint:?}"))?,
            None => core
                .models
                .first()
                .cloned()
                .ok_or_else(|| "no faux model".to_string())?,
        }
    } else {
        crate::core::model_runtime::resolve_run_model_for_provider(
            &models,
            &provider,
            model_hint.as_deref(),
        )?
    };

    // Session repo + initial session.
    let session_root = args
        .session_dir
        .clone()
        .map(|d| config::expand_tilde_path(&d))
        .unwrap_or_else(|| config::get_session_dir().to_string_lossy().into_owned());
    std::fs::create_dir_all(&session_root).map_err(|e| format!("create session dir: {e}"))?;
    crate::core::session_migration::migrate_legacy_sessions_in_root(std::path::Path::new(
        &session_root,
    ))
    .map_err(|e| format!("migrate legacy sessions: {e}"))?;
    let mut repo = JsonlSessionRepo::new(pi_agent::fs::StdFileSystem::new(&cwd), &session_root);
    let mut initial_status_banner = String::new();
    let source_selector = args.fork.as_deref().or(args.session.as_deref());
    let mut session = if let Some(selector) = source_selector {
        let selected_path = config::expand_tilde_path(selector);
        if std::path::Path::new(&selected_path).is_file() {
            crate::core::session_migration::migrate_legacy_session_file(std::path::Path::new(
                &selected_path,
            ))
            .map_err(|e| format!("migrate selected session: {e}"))?;
        }
        let source = crate::run::resolve_session_metadata(&repo, selector).await?;
        if args.fork.is_some() {
            let new_id = args
                .session_id
                .clone()
                .or_else(|| std::env::var(config::ENV_SESSION_ID).ok())
                .unwrap_or_else(pi_agent::session::new_id);
            let session = repo
                .fork(
                    &source,
                    CreateOptions {
                        id: Some(new_id.clone()),
                        cwd: cwd.clone(),
                        parent_session_id: None,
                        metadata: None,
                        fork_options: ForkOptions::Tree,
                    },
                )
                .await
                .map_err(|e| format!("fork session {}: {e}", source.id))?;
            initial_status_banner = format!(
                "forked session {} into {}",
                source.id.get(..8).unwrap_or(&source.id),
                new_id.get(..8).unwrap_or(&new_id)
            );
            session
        } else {
            let session = repo
                .open(&source)
                .await
                .map_err(|e| format!("open session {}: {e}", source.id))?;
            initial_status_banner = format!(
                "resumed session {}",
                source.id.get(..8).unwrap_or(&source.id)
            );
            session
        }
    } else if args.continue_session || args.resume {
        let mut sessions = repo
            .list(Some(&cwd))
            .await
            .map_err(|e| format!("list sessions: {e}"))?;
        sessions.sort_by_key(|session| std::cmp::Reverse(session.modified_at));
        let source = sessions.into_iter().next().ok_or_else(|| {
            if args.resume {
                "no sessions found to resume in this directory".to_string()
            } else {
                "no previous session found to continue in this directory".to_string()
            }
        })?;
        let session = repo
            .open(&source)
            .await
            .map_err(|e| format!("open session {}: {e}", source.id))?;
        initial_status_banner = if args.resume {
            format!(
                "resumed session {}",
                source.id.get(..8).unwrap_or(&source.id)
            )
        } else {
            format!(
                "continued session {}",
                source.id.get(..8).unwrap_or(&source.id)
            )
        };
        session
    } else {
        repo.create(CreateOptions {
            id: args
                .session_id
                .clone()
                .or_else(|| std::env::var(config::ENV_SESSION_ID).ok()),
            cwd: cwd.clone(),
            parent_session_id: None,
            metadata: None,
            fork_options: ForkOptions::Tree,
        })
        .await
        .map_err(|e| format!("create session: {e}"))?
    };
    if let Some(name) = &args.name {
        session
            .set_name(Some(name))
            .await
            .map_err(|e| format!("set session name: {e}"))?;
    }
    let session_id = session.get_metadata().await.id;
    let session_name = session.get_name().await;
    let initial_thinking_level = settings
        .get_default_thinking_level()
        .map(str::to_string)
        .unwrap_or_else(|| "off".to_string());
    let agent_dir = config::get_agent_dir().to_string_lossy().into_owned();
    let extensions = load_for_mode(
        args,
        &settings,
        &cwd,
        &agent_dir,
        "interactive",
        true,
        session_name.clone(),
        initial_thinking_level.clone(),
    );
    for error in &extensions.errors {
        tracing::warn!(path = %error.path, error = %error.error, "failed to load extension");
    }

    let mut runtime = InteractiveRuntime {
        cwd: cwd.clone(),
        models,
        faux_core,
        provider: provider.clone(),
        model: model.clone(),
        messages: Vec::new(),
        session,
        repo,
        session_root: session_root.clone(),
        session_id: session_id.clone(),
        session_name,
        system_prompt: args.system_prompt.clone(),
        tools_enabled: !args.no_tools,
        builtin_tools_enabled: !args.no_tools && !args.no_builtin_tools,
        extensions,
        auto_resize_images: settings.get_image_auto_resize(),
        block_images: settings.get_block_images(),
        persisted_until: 0,
        cache_entries: Vec::new(),
    };

    // Match the upstream non-blocking startup check: the TUI becomes usable
    // immediately and the notification is added when the request completes.
    let mut version_check = if std::env::var_os("PI_OFFLINE").is_some()
        || std::env::var_os("PI_SKIP_VERSION_CHECK").is_some()
    {
        None
    } else {
        Some(tokio::spawn(
            crate::core::version_check::check_for_new_pi_version(config::VERSION),
        ))
    };

    // The Rust port does not yet carry the upstream changelog catalogue, so
    // the persisted last-changelog version is the compatible update boundary:
    // report once on a fresh install and once when the shipped version moves.
    // The transport is backgrounded and independently honors PI_OFFLINE and
    // the PI_TELEMETRY/settings opt-out.
    let should_report_install_telemetry =
        settings.get_last_changelog_version() != Some(config::VERSION);
    if should_report_install_telemetry {
        settings.set_last_changelog_version(config::VERSION.to_string());
        let telemetry_enabled =
            crate::core::telemetry::is_install_telemetry_enabled_from_env(&settings);
        if telemetry_enabled && std::env::var_os("PI_OFFLINE").is_none() {
            tokio::spawn(crate::core::telemetry::report_install_telemetry(
                config::VERSION,
                telemetry_enabled,
            ));
        }
    }

    // Terminal + components.
    let terminal = Arc::new(Mutex::new(TerminalBackend::new()));
    terminal
        .lock()
        .unwrap()
        .enter_raw()
        .map_err(|e| format!("enter raw: {e}"))?;
    let _terminal_guard = InteractiveTerminalGuard {
        terminal: terminal.clone(),
    };

    it::tui_theme::load_theme(
        settings
            .get_theme_setting()
            .unwrap_or(crate::theme::DEFAULT_THEME),
    );
    let mut hide_thinking = settings.get_hide_thinking_block();
    let mut thinking_level = initial_thinking_level;

    let mut editor = it::create_editor(cwd.clone());
    editor.set_terminal_rows(terminal.lock().unwrap().height());
    let editor: Arc<Mutex<Editor>> = Arc::new(Mutex::new(editor));

    let transcript_md: Arc<Mutex<Markdown>> = Arc::new(Mutex::new(Markdown::new(
        String::new(),
        1,
        0,
        it::tui_theme::markdown_theme(),
        None,
        None,
    )));

    // CLI startup selectors (`--continue`, `--resume`, `--session`, and
    // `--fork`) open the target before the TUI starts. Rehydrate the visible
    // transcript and cache shadow now so the first rendered frame and the
    // first prompt observe the same history as slash-command resume/import.
    if !initial_status_banner.is_empty() {
        let (messages, cache_entries) =
            rehydrate_transcript(&runtime, &transcript_md, hide_thinking).await;
        runtime.messages = messages;
        runtime.cache_entries = cache_entries;
        runtime.persisted_until = runtime.messages.len();
    }

    let mut tree = Tree::new(terminal);

    let footer_text: Arc<Mutex<Text>> = Arc::new(Mutex::new(Text::new(String::new(), 0, 0, None)));

    let mut modal: Option<Modal> = None;
    let mut status_banner = initial_status_banner;
    let stream_buffer: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let mut streaming = false;
    let mut pending_text = String::new();

    tree.focus(editor.clone());
    tree.query_cell_size();

    let result = tokio::time::timeout(std::time::Duration::from_secs(24 * 60 * 60), async {
        loop {
            if version_check.as_ref().is_some_and(|task| task.is_finished()) {
                if let Some(task) = version_check.take() {
                    if let Ok(Some(release)) = task.await {
                        status_banner = format!(
                            "Update available: pi {} — run `pi update` (https://pi.dev/changelog)",
                            release.version
                        );
                    }
                }
            }
            // 1) Compose transcript (messages + streams + status banner).
            {
                let mut md = transcript_md.lock().unwrap();
                let stream = stream_buffer.lock().unwrap().clone();
                let cache_notices = if settings.get_show_cache_miss_notices() {
                    cache_notice_timestamps(&runtime.cache_entries)
                } else {
                    Vec::new()
                };
                let composed = it::compose_transcript_with_cache_notices(
                    &runtime.messages,
                    hide_thinking,
                    &stream,
                    &cache_notices,
                );
                let mut text = composed;
                if !status_banner.is_empty() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&status_banner);
                }
                md.set_text(text);
            }

            // 2) Footer.
            {
                let (usage, cache_hit_rate) = footer_usage_from_entries(&runtime.cache_entries);
                let fd = FooterData {
                    cwd: cwd.clone(),
                    branch: footer::git_branch(&cwd),
                    session_name: runtime.session_name.clone(),
                    model_label: Some(format!("{}/{}", runtime.provider, runtime.model.name)),
                    thinking: Some(thinking_level.clone()),
                    provider_count: runtime.models.get_providers().len(),
                    usage,
                    cache_hit_rate,
                };
                let lines = footer::render_footer(&fd, 80);
                footer_text.lock().unwrap().set_text(lines.join("\n"));
            }

            // 3) Scene.
            let modal_comp: Option<SharedComponent> = match modal.as_mut() {
                Some(m) => Some(modal_shared(m)),
                None => None,
            };
            let scene = it::build_scene(&transcript_md, &editor, &footer_text, modal_comp, &pending_text);
            tree.render(Some(&scene));

            // 4) Input.
            let term = tree.terminal_handle();
            let ev = term.lock().unwrap().next_event().map_err(|e| e.to_string())?;
            let key_str = match ev {
                pi_tui::terminal::TerminalEvent::Key(k) => k,
                pi_tui::terminal::TerminalEvent::Resize(_w, h) => {
                    tree.invalidate();
                    editor.lock().unwrap().set_terminal_rows(h as usize);
                    continue;
                }
            };
            if key_str.is_empty() {
                continue;
            }
            if tree.consume_cell_size_response(&key_str) {
                continue;
            }
            let key = parse_key(&key_str);

            if key.ctrl && key.base == "c" {
                if streaming {
                    status_banner = "Press Ctrl+C again to quit".to_string();
                    continue;
                }
                return Ok(());
            }
            let editor_text = editor.lock().unwrap().get_text();
            if should_exit_on_key(&key, &editor_text) {
                return Ok(());
            }

            // Modal input handling.
            if let Some(active_modal) = &mut modal {
                let mut close_modal = false;
                match active_modal {
                    Modal::Model(sel) => {
                        let mut guard = sel.lock().unwrap();
                        match guard.handle(&key) {
                            it::selectors::SelectorAction::Select(Some(idx)) if idx < guard.count() => {
                                if let Some(item) = guard.selected_item() {
                                    if let Some((p, id)) = it::apply_model_selection(&mut settings, &item.value) {
                                        runtime.provider = p.clone();
                                        if let Some(m) = runtime.models.get_model(&p, &id) {
                                            runtime.model = m;
                                        }
                                        status_banner = format!("Model: {}", item.label);
                                    }
                                }
                                close_modal = true;
                            }
                            it::selectors::SelectorAction::Cancel | it::selectors::SelectorAction::Select(_) => {
                                close_modal = true;
                            }
                            _ => {}
                        }
                    }
                    Modal::Thinking(sel) => {
                        let mut guard = sel.lock().unwrap();
                        match guard.handle(&key) {
                            it::selectors::SelectorAction::Select(Some(idx)) if idx < guard.count() => {
                                if let Some(item) = guard.selected_item() {
                                    settings.set_default_thinking_level(&item.value);
                                    thinking_level = item.value.clone();
                                    hide_thinking = item.value == "off";
                                    status_banner = format!("Thinking: {}", item.value);
                                }
                                close_modal = true;
                            }
                            it::selectors::SelectorAction::Cancel | it::selectors::SelectorAction::Select(_) => {
                                close_modal = true;
                            }
                            _ => {}
                        }
                    }
                    Modal::Theme(sel) => {
                        let mut guard = sel.lock().unwrap();
                        match guard.handle(&key) {
                            it::selectors::SelectorAction::Select(Some(idx)) if idx < guard.count() => {
                                if let Some(item) = guard.selected_item() {
                                    it::tui_theme::load_theme(&item.value);
                                    status_banner = format!("Theme: {}", item.value);
                                }
                                close_modal = true;
                            }
                            it::selectors::SelectorAction::Cancel | it::selectors::SelectorAction::Select(_) => {
                                close_modal = true;
                            }
                            _ => {}
                        }
                    }
                    Modal::Resume(sel, sessions) => {
                        let (close_resume, selected_session_id) = {
                            let mut guard = sel.lock().unwrap();
                            match guard.handle(&key) {
                                it::selectors::SelectorAction::Select(Some(idx))
                                    if idx < guard.count() =>
                                {
                                    (true, guard.selected_item().map(|item| item.value))
                                }
                                it::selectors::SelectorAction::Cancel
                                | it::selectors::SelectorAction::Select(_) => (true, None),
                                _ => (false, None),
                            }
                        };
                        if let Some(session_id) = selected_session_id {
                            if let Some(meta) = sessions.iter().find(|s| s.id == session_id) {
                                // Refuse to resume a session whose stored cwd
                                // no longer exists (upstream session-cwd.ts).
                                let cwd_now = std::env::current_dir()
                                    .map(|p| p.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                let issue = crate::core::session_cwd::get_missing_session_cwd_issue(
                                    Some(&meta.metadata.path),
                                    &meta.metadata.cwd,
                                    &cwd_now,
                                );
                                if let Some(issue) = issue {
                                    status_banner = crate::core::session_cwd::format_missing_session_cwd_error(&issue);
                                } else {
                                    match runtime.repo.open(&meta.metadata).await {
                                        Ok(session) => {
                                            runtime.session = session;
                                            runtime.session_id = meta.id.clone();
                                            runtime.session_name = None;
                                            let (messages, cache_entries) =
                                                rehydrate_transcript(&runtime, &transcript_md, hide_thinking).await;
                                            runtime.messages = messages;
                                            runtime.cache_entries = cache_entries;
                                            runtime.persisted_until = runtime.messages.len();
                                            status_banner = format!(
                                                "resumed session {} ({} prior messages)",
                                                meta.id.get(..8).unwrap_or(&meta.id),
                                                runtime.messages.len()
                                            );
                                        }
                                        Err(e) => {
                                            status_banner = format!("resume failed: {e}");
                                        }
                                    }
                                }
                            }
                        }
                        if close_resume {
                            close_modal = true;
                        }
                    }
                    Modal::Settings(panel) => {
                        let was_enter = key.base == "enter" && !key.ctrl && !key.alt;
                        {
                            let mut guard = panel.lock().unwrap();
                            let _ = was_enter;
                            guard.handle_input(&key);
                        }
                        let changes = { panel.lock().unwrap().drain_changes() };
                        for (id, value) in changes {
                            match id.as_str() {
                                "theme" => {
                                    settings.set_theme(value.clone());
                                    it::tui_theme::load_theme(&value);
                                }
                                "thinking" => {
                                    settings.set_default_thinking_level(&value);
                                    thinking_level = value.clone();
                                }
                                "images" => {
                                    settings.set_show_images(value == "on");
                                }
                                "cache-miss-notices" => {
                                    settings.set_show_cache_miss_notices(value == "true");
                                }
                                "install-telemetry" => {
                                    settings.set_enable_install_telemetry(value == "true");
                                }
                                _ => {}
                            }
                            status_banner = format!("/settings {id} → {value}");
                        }
                        if key.base == "esc" || key.base == "escape" {
                            close_modal = true;
                        }
                    }
                }
                if close_modal {
                    modal = None;
                    tree.focus(editor.clone());
                }
                continue;
            }

            // Editor input (skip Enter/Ctrl+C which the parent handles).
            {
                let mut e = editor.lock().unwrap();
                if key.ctrl && key.base == "c" {
                    continue;
                }
                e.handle_input(&key_str);
            }

            // Submit?
            let submitted = editor.lock().unwrap().drain_submitted();
            if let Some(submitted) = submitted {
                if submitted.trim().is_empty() || streaming {
                    continue;
                }
                let action = it::parse_submit(&submitted);
                match action {
                    SubmitAction::Prompt(prompt) => {
                        editor.lock().unwrap().add_to_history(&prompt);
                        let message_start = runtime.messages.len();
                        streaming = true;
                        pending_text = " …".to_string();
                        *stream_buffer.lock().unwrap() = String::new();
                        let on_event: Arc<dyn Fn(&AssistantMessageEvent) + Send + Sync> = {
                            let stream_buffer = stream_buffer.clone();
                            Arc::new(move |event: &AssistantMessageEvent| {
                                if let AssistantMessageEvent::TextDelta { delta, .. } = event {
                                    stream_buffer.lock().unwrap().push_str(delta);
                                }
                            })
                        };
                        let turn_result = stream_turn(&mut runtime, prompt, on_event).await;
                        if let Err(error) = turn_result {
                            status_banner = error;
                        }
                        let new_messages = runtime.messages[message_start..].to_vec();
                        append_cache_entries_from_messages(&mut runtime.cache_entries, &new_messages);
                        streaming = false;
                        pending_text = String::new();
                        *stream_buffer.lock().unwrap() = String::new();
                        // Auto-compaction: summarize history when the context
                        // approaches the model window (upstream compaction loop).
                        match maybe_auto_compact(&mut runtime).await {
                            Ok(true) => status_banner = "context compacted (auto)".to_string(),
                            Ok(false) => {}
                            Err(e) => status_banner = e,
                        }
                    }
                    SubmitAction::Command(command, arg) => {
                        match command.kind {
                            SlashKind::Model => {
                                let items = it::selectors::model_selector_items(&runtime.models, None);
                                modal = Some(Modal::Model(Arc::new(Mutex::new(ListSelector::new_slash_layout(items, 10)))));
                            }
                            SlashKind::Thinking => {
                                let items = it::selectors::thinking_selector_items();
                                modal = Some(Modal::Thinking(Arc::new(Mutex::new(ListSelector::new(items, 6)))));
                            }
                            SlashKind::Theme => {
                                let items = it::selectors::theme_selector_items();
                                modal = Some(Modal::Theme(Arc::new(Mutex::new(ListSelector::new(items, 10)))));
                            }
                            SlashKind::Settings => {
                                let entries = it::selectors::settings_selector_items(&settings);
                                modal = Some(Modal::Settings(Arc::new(Mutex::new(SettingsPanel::new(entries)))));
                            }
                            SlashKind::Session => {
                                status_banner = session_status(&runtime);
                            }
                            SlashKind::Clear => {
                                runtime.messages.clear();
                                // `/clear` starts a fresh prompt-cache segment
                                // while retaining the session's historical
                                // accounting.
                                runtime.cache_entries.push(json!({
                                    "type": "compaction",
                                    "timestamp": pi_ai::types::now_ms(),
                                }));
                                transcript_md.lock().unwrap().set_text("");
                            }
                            SlashKind::Hotkeys => {
                                status_banner = "hotkeys: enter submit · shift+enter newline · ctrl+c quit · ↑/↓ history · ctrl+w word-delete".to_string();
                            }
                            SlashKind::Help => {
                                status_banner = it::slash::help_banner();
                            }
                            SlashKind::Quit => {
                                return Ok(());
                            }
                            SlashKind::Compact => {
                                let instructions = arg
                                    .as_deref()
                                    .map(str::trim)
                                    .filter(|value| !value.is_empty());
                                match compact_interactive(&mut runtime, instructions, true).await {
                                    Ok(true) => status_banner = "context compacted".to_string(),
                                    Ok(false) => status_banner = "nothing to compact".to_string(),
                                    Err(error) => status_banner = error,
                                }
                            }
                            SlashKind::Unsupported => match command.name {
                                "export" => {
                                    let meta = runtime.session.get_metadata().await;
                                    match crate::core::export_html::export_session_file(
                                        &meta.path,
                                        arg.as_deref(),
                                        None,
                                    ) {
                                        Ok(path) => {
                                            status_banner = format!("exported session to {path}");
                                        }
                                        Err(e) => {
                                            status_banner = format!("export failed: {e}");
                                        }
                                    }
                                }
                                "new" => {
                                    let new_id = pi_agent::session::new_id();
                                    match runtime
                                        .repo
                                        .create(CreateOptions {
                                            id: Some(new_id.clone()),
                                            cwd: runtime.cwd.clone(),
                                            parent_session_id: None,
                                            metadata: None,
                                            fork_options: ForkOptions::Tree,
                                        })
                                        .await
                                    {
                                        Ok(new_session) => {
                                            runtime.session = new_session;
                                            runtime.session_id = new_id;
                                            runtime.messages.clear();
                                            runtime.cache_entries.clear();
                                            runtime.persisted_until = 0;
                                            transcript_md.lock().unwrap().set_text("");
                                            status_banner = format!(
                                                "started new session {} in {}",
                                                runtime.session_id.get(..8).unwrap_or(&runtime.session_id),
                                                meta_short_cwd(&runtime.cwd)
                                            );
                                        }
                                        Err(e) => {
                                            status_banner = format!("new session failed: {e}");
                                        }
                                    }
                                }
                                "resume" => {
                                    match crate::core::session_migration::migrate_legacy_sessions_in_root(
                                        std::path::Path::new(&runtime.session_root),
                                    ) {
                                        Ok(_) => match runtime.repo.list(Some(&runtime.cwd)).await {
                                            Ok(sessions) if !sessions.is_empty() => {
                                                // Exclude the current session so the picker offers
                                                // other sessions (newest-first default).
                                                let sessions = resumable_sessions(sessions, &runtime.session_id);
                                                if sessions.is_empty() {
                                                    status_banner =
                                                        "no sessions found to resume in this directory"
                                                            .to_string();
                                                } else {
                                                    let picker = it::session_picker_items(sessions);
                                                    let items = it::picker_select_items(&picker);
                                                    modal = Some(Modal::Resume(
                                                        Arc::new(Mutex::new(ListSelector::new(
                                                            items, 10,
                                                        ))),
                                                        picker,
                                                    ));
                                                }
                                            }
                                            Ok(_) => {
                                                status_banner =
                                                    "no sessions found to resume in this directory".to_string();
                                            }
                                            Err(e) => {
                                                status_banner = format!("list sessions failed: {e}");
                                            }
                                        },
                                        Err(e) => {
                                            status_banner = format!("migrate legacy sessions failed: {e}");
                                        }
                                    }
                                }
                                "name" => {
                                    match arg.as_deref() {
                                        Some(name) if !name.trim().is_empty() => {
                                            match runtime.session.set_name(Some(name.trim())).await {
                                                Ok(()) => {
                                                    runtime.session_name = Some(name.trim().to_string());
                                                    status_banner = format!("session name: {}", name.trim());
                                                }
                                                Err(e) => {
                                                    status_banner = format!("set name failed: {e}");
                                                }
                                            }
                                        }
                                        _ => {
                                            status_banner = "usage: /name <session-name>".to_string();
                                        }
                                    }
                                }
                                "import" => {
                                    let mut import_path: Option<String> = None;
                                    match arg.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
                                        None => {
                                            status_banner = "usage: /import <session.jsonl>".to_string();
                                        }
                                        Some(path) => {
                                            let input_path = config::expand_tilde_path(path);
                                            if !std::path::Path::new(&input_path).exists() {
                                                status_banner = format!("file not found: {path}");
                                            } else if let Ok(content) = std::fs::read_to_string(&input_path) {
                                                let header_id = content.lines().next().and_then(|line| {
                                                    serde_json::from_str::<serde_json::Value>(line)
                                                        .ok()
                                                        .and_then(|v| {
                                                            v.get("id").and_then(|i| i.as_str()).map(|s| s.to_string())
                                                        })
                                                });
                                                match header_id {
                                                    None => {
                                                        status_banner = format!("invalid session file: {path}");
                                                    }
                                                    Some(header_id) => {
                                                        // Legacy (v1-v3) files are converted to the v4
                                                        // harness JSONL format and written into the
                                                        // session dir before opening (upstream
                                                        // session-manager migration path).
                                                        let first_line = content.lines().next().unwrap_or("");
                                                        let is_v4 = serde_json::from_str::<serde_json::Value>(first_line)
                                                            .ok()
                                                            .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(|s| s.to_string()))
                                                            == Some("header".to_string());
                                                        let resolved_path: Option<String> = if is_v4 {
                                                            Some(input_path.clone())
                                                        } else {
                                                            match crate::core::session_migration::convert_legacy_to_v4(&content) {
                                                                Ok(v4_content) => {
                                                                    let _ = std::fs::create_dir_all(&runtime.session_root);
                                                                    let converted = std::path::Path::new(&runtime.session_root)
                                                                        .join(format!("imported-{header_id}.jsonl"));
                                                                    match std::fs::write(&converted, v4_content) {
                                                                        Ok(()) => Some(converted.to_string_lossy().into_owned()),
                                                                        Err(e) => {
                                                                            status_banner = format!("import failed: {e}");
                                                                            None
                                                                        }
                                                                    }
                                                                }
                                                                Err(e) => {
                                                                    status_banner = format!("import failed: {e}");
                                                                    None
                                                                }
                                                            }
                                                        };
                                                        let Some(resolved_path) = resolved_path else {
                                                            return Ok(());
                                                        };
                                                        import_path = Some(resolved_path.clone());
                                                        let metadata = match crate::run::metadata_from_session_path(
                                                            std::path::Path::new(&resolved_path),
                                                        ) {
                                                            Ok(metadata) => metadata,
                                                            Err(error) => {
                                                                status_banner = format!("import failed: {error}");
                                                                return Ok(());
                                                            }
                                                        };
                                                        match runtime.repo.open(&metadata).await {
                                                            Ok(session) => {
                                                                runtime.session = session;
                                                                runtime.session_id =
                                                                    runtime.session.get_metadata().await.id;
                                                                runtime.session_name = None;
                                                                let (messages, cache_entries) = rehydrate_transcript(
                                                                    &runtime,
                                                                    &transcript_md,
                                                                    hide_thinking,
                                                                )
                                                                .await;
                                                                runtime.messages = messages;
                                                                runtime.cache_entries = cache_entries;
                                                                runtime.persisted_until = runtime.messages.len();
                                                                status_banner = format!(
                                                                    "imported {} ({} prior messages)",
                                                                    path,
                                                                    runtime.messages.len()
                                                                );
                                                            }
                                                            Err(e) => {
                                                                status_banner = format!("import failed: {e}");
                                                            }
                                                        }
                                                    }
                                                }
                                            } else {
                                                status_banner = format!("cannot read {path}");
                                            }
                                        }
                                    }
                                    let _ = &import_path;
                                }
                                "reload" => {
                                    let theme_before = settings
                                        .get_theme_setting()
                                        .unwrap_or(crate::theme::DEFAULT_THEME)
                                        .to_string();
                                    settings.reload().await;
                                    let mut notes: Vec<String> = Vec::new();
                                    for se in settings.drain_errors() {
                                        let where_ = se.path.clone().unwrap_or_else(|| format!("{:?}", se.scope));
                                        notes.push(format!("{where_}: {}", se.error));
                                    }
                                    it::tui_theme::load_theme(
                                        settings
                                            .get_theme_setting()
                                            .unwrap_or(crate::theme::DEFAULT_THEME),
                                    );
                                    let theme_after = settings
                                        .get_theme_setting()
                                        .unwrap_or(crate::theme::DEFAULT_THEME)
                                        .to_string();
                                    if theme_after != theme_before {
                                        notes.push(format!("theme changed to {theme_after}"));
                                    }
                                    if notes.is_empty() {
                                        status_banner = "reloaded settings".to_string();
                                    } else {
                                        status_banner = format!("reloaded settings ({})", notes.join("; "));
                                    }
                                }
                                "fork" | "clone" => {
                                    // Persist the current in-memory transcript first so the
                                    // fork/clone carries it (the interactive loop only persists
                                    // on exit; we switch sessions before that happens).
                                    if !runtime.messages.is_empty() {
                                        let to_append: Vec<pi_agent::types::AgentMessage> = runtime.messages.to_vec();
                                        persist_messages(&mut runtime.session, &to_append).await;
                                    }
                                    let meta = runtime.session.get_metadata().await;
                                    let new_id = pi_agent::session::new_id();
                                    let cwd = runtime.cwd.clone();
                                    let result = if command.name == "fork" {
                                        runtime
                                            .repo
                                            .fork(
                                                &meta,
                                                CreateOptions {
                                                    id: Some(new_id.clone()),
                                                    cwd,
                                                    parent_session_id: None,
                                                    metadata: None,
                                                    fork_options: ForkOptions::Tree,
                                                },
                                            )
                                            .await
                                    } else {
                                        let mut fresh = runtime
                                            .repo
                                            .create(CreateOptions {
                                                id: Some(new_id.clone()),
                                                cwd,
                                                parent_session_id: None,
                                                metadata: None,
                                                fork_options: ForkOptions::Tree,
                                            })
                                            .await
                                            .map_err(|e| format!("clone create failed: {e}"))?;
                                        let to_append: Vec<pi_agent::types::AgentMessage> = runtime.messages.to_vec();
                                        persist_messages(&mut fresh, &to_append).await;
                                        Ok(fresh)
                                    };
                                    match result {
                                        Ok(session) => {
                                            runtime.session = session;
                                            runtime.session_id = new_id;
                                            runtime.session_name = None;
                                            // Messages are persisted in the target already; keep the
                                            // in-memory transcript for display and only persist
                                            // messages added after the switch.
                                            runtime.persisted_until = runtime.messages.len();
                                            transcript_md
                                                .lock()
                                                .unwrap()
                                                .set_text(it::compose_transcript(
                                                    &runtime.messages,
                                                    hide_thinking,
                                                    "",
                                                ));
                                            status_banner = format!(
                                                "{} session {} ({} prior messages)",
                                                command.name,
                                                runtime.session_id.get(..8).unwrap_or(&runtime.session_id),
                                                runtime.messages.len()
                                            );
                                        }
                                        Err(e) => {
                                            status_banner = format!("{} failed: {e}", command.name);
                                        }
                                    }
                                }
                                "trust" => {
                                    match arg.as_deref().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()) {
                                        Some(choice) if matches!(choice.as_str(), "allow" | "deny" | "ask") => {
                                            settings.set_default_project_trust(&choice);
                                            status_banner = format!("default project trust: {choice}");
                                        }
                                        _ => {
                                            status_banner = "usage: /trust <allow|deny|ask>".to_string();
                                        }
                                    }
                                }
                                "copy" => {
                                    // Copy the last assistant message text. Without a system
                                    // clipboard binary the text is surfaced in the banner instead.
                                    let mut text = String::new();
                                    for message in runtime.messages.iter().rev() {
                                        if let pi_agent::types::AgentMessage::Core(
                                            pi_ai::types::Message::Assistant(a),
                                        ) = message
                                        {
                                            for block in a.content() {
                                                if let pi_ai::types::ContentBlock::Text { text: t, .. } = block {
                                                    if !t.is_empty() {
                                                        text = t.clone();
                                                        break;
                                                    }
                                                }
                                            }
                                            break;
                                        }
                                    }
                                    if text.is_empty() {
                                        status_banner = "no assistant message to copy".to_string();
                                    } else {
                                        let copied = ["xclip", "wl-copy", "pbcopy"]
                                            .iter()
                                            .find_map(|bin| {
                                                let Ok(mut child) = std::process::Command::new(bin)
                                                    .stdin(std::process::Stdio::piped())
                                                    .spawn()
                                                else {
                                                    return None;
                                                };
                                                let mut stdin = child.stdin.take()?;
                                                use std::io::Write as _;
                                                let _ = stdin.write_all(text.as_bytes());
                                                drop(stdin);
                                                child.wait().ok();
                                                Some(())
                                            });
                                        if copied.is_some() {
                                            status_banner = "copied last assistant message to clipboard".to_string();
                                        } else {
                                            let preview: String = text.chars().take(90).collect();
                                            if preview != text {
                                                status_banner = format!("copied (preview): {preview}…");
                                            } else {
                                                status_banner = format!("copied: {preview}");
                                            }
                                        }
                                    }
                                }
                                "login" => {
                                    let provider_ref = arg.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty());
                                    let banner = Arc::new(Mutex::new(String::new()));
                                    let term = tree.terminal_handle();
                                    match run_oauth_login(&runtime.models, provider_ref, banner.clone(), term).await {
                                        Ok(message) => status_banner = message,
                                        Err(e) => status_banner = e,
                                    }
                                }
                                "logout" => {
                                    match arg.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
                                        Some(provider) => {
                                            let auth = crate::core::auth_storage::AuthStorage::create(config::get_auth_path());
                                            let opts = crate::core::auth_storage::AuthOperationOptions::default();
                                            match auth.delete(provider, &opts).await {
                                                Ok(()) => status_banner = format!("logged out {provider}"),
                                                Err(e) => status_banner = format!("logout failed: {e}"),
                                            }
                                        }
                                        None => {
                                            status_banner = "usage: /logout <provider>".to_string();
                                        }
                                    }
                                }
                                "tree" => {
                                    let entries = runtime
                                        .session
                                        .find_entries(&pi_agent::session::state::EntryQuery {
                                            order: Some(pi_agent::session::state::EntryOrder::OldestFirst),
                                            id: None,
                                            entry_type: None,
                                            custom_type: None,
                                            cursor: None,
                                            limit: None,
                                        })
                                        .await;
                                    match entries {
                                        Err(_) => {
                                            status_banner = "tree: failed to read session entries".to_string();
                                        }
                                        Ok(entries) => {
                                            // Parent-linked textual tree (same linkage as the RPC
                                            // get_tree surface, rendered compactly).
                                            let mut lines: Vec<String> = Vec::new();
                                            let mut depth: std::collections::HashMap<String, usize> =
                                                std::collections::HashMap::new();
                                            let mut by_id: std::collections::HashMap<String, String> =
                                                std::collections::HashMap::new();
                                            for entry in &entries {
                                                let id = entry.id().to_string();
                                                let parent = entry.parent_id().map(|s| s.to_string());
                                                let label = match entry {
                                                    pi_agent::session::types::Entry::Message { message, .. } => {
                                                        format!("{}: {}", message.role(), short_text(message))
                                                    }
                                                    pi_agent::session::types::Entry::Compaction { summary, .. } => {
                                                        format!("compaction: {}", short_truncate(summary))
                                                    }
                                                    pi_agent::session::types::Entry::BranchSummary { summary, .. } => {
                                                        format!("branch-summary: {}", short_truncate(summary))
                                                    }
                                                    pi_agent::session::types::Entry::ModelChange { model_id, .. } => {
                                                        format!("model_change: {model_id}")
                                                    }
                                                    other => entry_type_label(other).to_string(),
                                                };
                                                let d = parent
                                                    .as_deref()
                                                    .and_then(|p| depth.get(p))
                                                    .copied()
                                                    .unwrap_or(0);
                                                depth.insert(id.clone(), d);
                                                by_id.insert(
                                                    id.clone(),
                                                    format!("{}{} {}", "  ".repeat(d), id.get(..8).unwrap_or(&id), label),
                                                );
                                            }
                                            for entry in &entries {
                                                let id = entry.id().to_string();
                                                lines.push(by_id.get(&id).cloned().unwrap_or_default());
                                            }
                                            if lines.is_empty() {
                                                status_banner = "tree: empty session".to_string();
                                            } else {
                                                let total = lines.join("\n");
                                                let preview: String = total.chars().take(700).collect();
                                                status_banner = format!("session tree:\n{preview}");
                                            }
                                        }
                                    }
                                }
                                "share" => {
                                    // Persist unpersisted messages so the exported HTML
                                    // matches the current transcript.
                                    if runtime.messages.len() > runtime.persisted_until {
                                        let to_append: Vec<pi_agent::types::AgentMessage> =
                                            runtime.messages[runtime.persisted_until..].to_vec();
                                        persist_messages(&mut runtime.session, &to_append).await;
                                        runtime.persisted_until = runtime.messages.len();
                                    }
                                    let dry_run = std::env::var("PI_SHARE_DRY_RUN").as_deref() == Ok("1");
                                    match run_share(&runtime, dry_run).await {
                                        Ok(message) => status_banner = message,
                                        Err(e) => status_banner = e,
                                    }
                                }
                                _ => {
                                    status_banner = format!(
                                        "`/{}` is not wired in the interactive port yet",
                                        command.name
                                    );
                                }
                            },
                        }
                    }
                }
            }
        }
    })
    .await;

    // Persist messages that were added after the last session-switch
    // operation (resume/fork/clone advance the watermark; the rest already
    // live in the session).
    if runtime.messages.len() > runtime.persisted_until {
        let to_append: Vec<pi_agent::types::AgentMessage> =
            runtime.messages[runtime.persisted_until..].to_vec();
        persist_messages(&mut runtime.session, &to_append).await;
    }

    // Leave the alternate screen.
    tree.leave_alt_screen();
    match result {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("interactive mode timed out".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::ExtensionHostActions;
    use pi_agent::fs::StdFileSystem;
    use pi_agent::session::jsonl::repo::CreateOptions;
    use pi_agent::session::state::ForkOptions;
    use pi_agent::session::JsonlSessionRepo;

    #[test]
    fn ctrl_d_exits_only_for_an_empty_editor() {
        let ctrl_d = parse_key("\x04");
        assert!(should_exit_on_key(&ctrl_d, ""));
        assert!(!should_exit_on_key(&ctrl_d, "draft"));
        assert!(!should_exit_on_key(&parse_key("ctrl+c"), ""));
    }

    #[tokio::test]
    async fn resume_candidates_exclude_the_current_session() {
        let root =
            std::env::temp_dir().join(format!("pi-resume-empty-selector-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let runtime = test_runtime(&root).await;
        let sessions = runtime.repo.list(Some(&runtime.cwd)).await.unwrap();
        assert!(resumable_sessions(sessions, &runtime.session_id).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Serializes tests that mutate the process-global PATH /
    /// PI_SHARE_VIEWER_URL so parallel executions cannot race on the env.
    fn env_lock() -> &'static tokio::sync::Mutex<()> {
        use std::sync::OnceLock;
        static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    /// Restores PATH / PI_SHARE_VIEWER_URL on drop. `replace_path` swaps PATH
    /// entirely (hermetic: no real `gh` visible); otherwise `bin_dir` is
    /// prepended so the fake `gh` shadows the real one.
    struct EnvGuard {
        old_path: String,
        old_viewer: Option<String>,
    }

    impl EnvGuard {
        fn install(bin_dir: &std::path::Path, viewer: &str) -> Self {
            let old_path = std::env::var("PATH").unwrap_or_default();
            let old_viewer = std::env::var("PI_SHARE_VIEWER_URL").ok();
            std::env::set_var("PATH", format!("{}:{}", bin_dir.display(), old_path));
            std::env::set_var("PI_SHARE_VIEWER_URL", viewer);
            EnvGuard {
                old_path,
                old_viewer,
            }
        }

        fn install_hermetic(bin_dir: &std::path::Path, viewer: &str) -> Self {
            let old_path = std::env::var("PATH").unwrap_or_default();
            let old_viewer = std::env::var("PI_SHARE_VIEWER_URL").ok();
            std::env::set_var("PATH", bin_dir.as_os_str());
            std::env::set_var("PI_SHARE_VIEWER_URL", viewer);
            EnvGuard {
                old_path,
                old_viewer,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            std::env::set_var("PATH", &self.old_path);
            match &self.old_viewer {
                Some(v) => std::env::set_var("PI_SHARE_VIEWER_URL", v),
                None => std::env::remove_var("PI_SHARE_VIEWER_URL"),
            }
        }
    }

    /// Build an InteractiveRuntime backed by a real session file in `root`.
    async fn test_runtime(root: &std::path::Path) -> InteractiveRuntime {
        let cwd = root.to_string_lossy().into_owned();
        let session_root = root.join("sessions");
        std::fs::create_dir_all(&session_root).unwrap();
        let mut repo = JsonlSessionRepo::new(
            StdFileSystem::new(&cwd),
            session_root.to_string_lossy().into_owned(),
        );
        let session_id = pi_agent::session::new_id();
        let session = repo
            .create(CreateOptions {
                id: Some(session_id.clone()),
                cwd: cwd.clone(),
                parent_session_id: None,
                metadata: None,
                fork_options: ForkOptions::Tree,
            })
            .await
            .unwrap();
        let models =
            pi_ai::providers::builtin_models(pi_ai::models::CreateModelsOptions::default());
        let faux_core = Some(crate::core::model_runtime::register_faux_provider(
            &models,
            &pi_ai::providers::RegisterFauxProviderOptions::default(),
        ));
        let model = faux_core
            .as_ref()
            .and_then(|core| core.models.first().cloned())
            .expect("faux model");
        let extensions = load_for_mode(
            &Args {
                no_extensions: true,
                ..Default::default()
            },
            &SettingsManager::in_memory(crate::core::settings::SettingsMap::new()),
            &cwd,
            &cwd,
            "interactive",
            true,
            None,
            "off",
        );
        InteractiveRuntime {
            cwd,
            models,
            faux_core,
            provider: "faux".to_string(),
            model,
            messages: Vec::new(),
            session,
            repo,
            session_root: session_root.to_string_lossy().into_owned(),
            session_id,
            session_name: None,
            system_prompt: None,
            tools_enabled: true,
            builtin_tools_enabled: true,
            extensions,
            auto_resize_images: true,
            block_images: false,
            persisted_until: 0,
            cache_entries: Vec::new(),
        }
    }

    #[tokio::test]
    async fn interactive_stream_turn_uses_harness_transcript_and_events() {
        let root = std::env::temp_dir().join(format!(
            "pi-interactive-harness-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut runtime = test_runtime(&root).await;
        let deltas = Arc::new(Mutex::new(Vec::<String>::new()));
        let deltas_for_event = deltas.clone();
        let on_event: Arc<dyn Fn(&AssistantMessageEvent) + Send + Sync> = Arc::new(move |event| {
            if let AssistantMessageEvent::TextDelta { delta, .. } = event {
                deltas_for_event.lock().unwrap().push(delta.clone());
            }
        });

        let new_messages = stream_turn(&mut runtime, "hello".to_string(), on_event)
            .await
            .unwrap();

        assert_eq!(new_messages.len(), 2, "prompt plus assistant response");
        assert_eq!(runtime.messages.len(), 2);
        assert!(runtime.messages.iter().any(|message| {
            matches!(
                message,
                pi_agent::types::AgentMessage::Core(pi_ai::types::Message::Assistant(assistant))
                    if assistant.content().iter().any(|block| matches!(
                        block,
                        pi_ai::types::ContentBlock::Text { text, .. }
                            if text.contains("faux response to: hello")
                    ))
            )
        }));
        assert!(!deltas.lock().unwrap().is_empty());
        let entries = runtime
            .session
            .find_entries(&pi_agent::session::state::EntryQuery {
                order: Some(pi_agent::session::state::EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(entries.len(), 2, "a completed turn is durable immediately");
        assert_eq!(runtime.persisted_until, runtime.messages.len());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn interactive_turn_tools_respect_builtin_and_all_tool_flags() {
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let root = std::env::temp_dir().join(format!(
            "pi-interactive-extension-tools-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let extension = root.join("index.js");
        std::fs::write(
            &extension,
            r#"export default function (pi) {
  pi.registerTool({
    name: "interactive-tool",
    description: "interactive policy fixture",
    parameters: { type: "object", properties: {} },
    execute: async () => ({ content: [{ type: "text", text: "ok" }] }),
  });
}"#,
        )
        .unwrap();

        let args = Args {
            extensions: vec![extension.to_string_lossy().into_owned()],
            no_extensions: true,
            ..Default::default()
        };
        let loaded = load_for_mode(
            &args,
            &SettingsManager::in_memory(crate::core::settings::SettingsMap::new()),
            &root.to_string_lossy(),
            &root.to_string_lossy(),
            "interactive",
            true,
            None,
            "off",
        );
        assert!(loaded.errors.is_empty(), "load errors: {:?}", loaded.errors);

        let mut runtime = test_runtime(&root).await;
        runtime.extensions = loaded;
        runtime.builtin_tools_enabled = false;
        let tools = interactive_turn_tools(&runtime);
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["interactive-tool"]
        );
        assert_eq!(
            runtime.extensions.host.snapshot()["activeTools"],
            json!(["interactive-tool"])
        );

        runtime.tools_enabled = false;
        assert!(interactive_turn_tools(&runtime).is_empty());
        assert_eq!(runtime.extensions.host.snapshot()["activeTools"], json!([]));

        drop(runtime);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Write a fake `gh` script into `bin_dir`. `auth_status` is the exit code
    /// for `gh auth status`; `gist_url` is the stdout for `gh gist create`
    /// (None => exit 1).
    fn install_fake_gh(bin_dir: &std::path::Path, auth_status: i32, gist_url: Option<&str>) {
        std::fs::create_dir_all(bin_dir).unwrap();
        let script = match gist_url {
            Some(url) => format!(
                "#!/bin/sh\nif [ \"$1\" = \"auth\" ] && [ \"$2\" = \"status\" ]; then exit {auth_status}; fi\nif [ \"$1\" = \"gist\" ] && [ \"$2\" = \"create\" ]; then echo '{url}'; exit 0; fi\nexit 1\n"
            ),
            None => format!(
                "#!/bin/sh\nif [ \"$1\" = \"auth\" ] && [ \"$2\" = \"status\" ]; then exit {auth_status}; fi\nexit 1\n"
            ),
        };
        std::fs::write(bin_dir.join("gh"), script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(bin_dir.join("gh"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
    }

    #[tokio::test]
    async fn share_creates_secret_gist_and_prints_viewer_url() {
        let root = std::env::temp_dir().join(format!("pi-share-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let _env = env_lock().lock().await;
        let runtime = test_runtime(&root).await;
        install_fake_gh(
            &root.join("bin"),
            0,
            Some("https://gist.github.com/fakeuser/abc123"),
        );
        let _guard = EnvGuard::install(&root.join("bin"), "https://pi.dev/session/");
        let msg = run_share(&runtime, false)
            .await
            .expect("share should succeed");
        assert_eq!(
            msg,
            "Share URL: https://pi.dev/session/#abc123\nGist: https://gist.github.com/fakeuser/abc123"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn share_requires_gh_auth() {
        let root = std::env::temp_dir().join(format!("pi-share-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let _env = env_lock().lock().await;
        let runtime = test_runtime(&root).await;
        install_fake_gh(&root.join("bin"), 1, None);
        let _guard = EnvGuard::install(&root.join("bin"), "https://pi.dev/session/");
        let err = run_share(&runtime, false).await.unwrap_err();
        assert_eq!(
            err,
            "GitHub CLI is not logged in. Run 'gh auth login' first."
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn share_reports_missing_gh() {
        let root = std::env::temp_dir().join(format!("pi-share-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let _env = env_lock().lock().await;
        let runtime = test_runtime(&root).await;
        // PATH pointing at an empty dir only: no gh binary anywhere.
        let empty = root.join("empty-bin");
        std::fs::create_dir_all(&empty).unwrap();
        let _guard = EnvGuard::install_hermetic(&empty, "https://pi.dev/session/");
        let err = run_share(&runtime, false).await.unwrap_err();
        assert_eq!(
            err,
            "GitHub CLI (gh) is not installed. Install it from https://cli.github.com/"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn share_dry_run_skips_gh() {
        let root = std::env::temp_dir().join(format!("pi-share-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let _env = env_lock().lock().await;
        let runtime = test_runtime(&root).await;
        let msg = run_share(&runtime, true).await.unwrap();
        assert_eq!(msg, "PI_SHARE_DRY_RUN=1: /share skipped");
        let _ = std::fs::remove_dir_all(&root);
    }
    #[tokio::test]
    async fn auto_compact_replaces_context_when_over_threshold() {
        let _env = env_lock().lock().await;
        let root = std::env::temp_dir().join(format!("pi-compact-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut runtime = test_runtime(&root).await;
        // Register faux in the models facade so complete_simple resolves
        // (mirrors RpcRuntime::new's scripted faux registration).
        {
            use pi_ai::models::{
                create_provider, CreateProviderOptions, ProviderApiSpec, ProviderStreams,
            };
            use pi_ai::providers::{
                faux_assistant_message, FauxAssistantOptions, FauxProviderCore, FauxResponseStep,
                RegisterFauxProviderOptions,
            };
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
                vec![pi_ai::types::ContentBlock::text(
                    "Compaction summary: history retained",
                )],
                FauxAssistantOptions::default(),
            ))]);
            let stream_core = core.clone();
            let stream = Arc::new(
                move |model: &pi_ai::model::Model,
                      ctx: &pi_ai::types::Context,
                      _options: Option<&pi_ai::types::StreamOptions>| {
                    stream_core.stream(model, ctx, None)
                },
            );
            let simple_core = core.clone();
            let stream_simple = Arc::new(
                move |model: &pi_ai::model::Model,
                      ctx: &pi_ai::types::Context,
                      options: Option<&pi_ai::types::SimpleStreamOptions>| {
                    simple_core.stream(model, ctx, options)
                },
            );
            runtime
                .models
                .set_provider(create_provider(CreateProviderOptions {
                    id: "faux".to_string(),
                    name: Some("Faux".to_string()),
                    base_url: None,
                    headers: None,
                    auth: pi_ai::auth::ProviderAuth {
                        api_key: Some(pi_ai::auth::env_api_key_auth(
                            "Faux API key",
                            vec!["FAUX_API_KEY"],
                        )),
                        oauth: None,
                    },
                    models: core.models.clone(),
                    api: ProviderApiSpec::Single(ProviderStreams {
                        stream,
                        stream_simple,
                        fetch_deferred: None,
                        cancel_deferred: None,
                    }),
                    filter_models: None,
                }));
        }
        // The env-key auth resolves when FAUX_API_KEY is set.
        std::env::set_var("FAUX_API_KEY", "test");
        // Tiny context window so the threshold triggers immediately.
        runtime.model.context_window = 1000;
        // A few long messages push the estimate over window - reserve.
        for i in 0..8 {
            let text = format!("message {i}: {}", "x".repeat(400));
            runtime.messages.push(pi_agent::agent::user_text_prompt(
                text,
                pi_ai::types::now_ms(),
            ));
        }
        // prepare_compaction reads session entries, so persist the messages.
        persist_messages(&mut runtime.session, &runtime.messages).await;
        runtime.persisted_until = runtime.messages.len();
        let estimate = pi_agent::harness::compaction::estimate_context_tokens(&runtime.messages);
        assert!(
            pi_agent::harness::compaction::should_compact(
                estimate.tokens,
                runtime.model.context_window,
                &pi_agent::harness::compaction::DEFAULT_COMPACTION_SETTINGS
            ),
            "test setup: context should be over threshold (tokens={})",
            estimate.tokens
        );
        let compacted = maybe_auto_compact(&mut runtime)
            .await
            .expect("auto-compact");
        assert!(compacted, "compaction should have run");
        // The context is now the summary message + retained tail.
        assert!(!runtime.messages.is_empty(), "context replaced");
        let first = &runtime.messages[0];
        let text: String = match first {
            pi_agent::types::AgentMessage::Core(pi_ai::types::Message::User(u)) => {
                match u.content() {
                    pi_ai::types::UserContentBody::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|b| match b {
                            pi_ai::types::ContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect(),
                    pi_ai::types::UserContentBody::String(s) => s.clone(),
                }
            }
            _ => String::new(),
        };
        assert!(
            text.contains("Compaction summary"),
            "summary message: {text}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn auto_compact_skips_when_under_threshold() {
        let _env = env_lock().lock().await;
        let root = std::env::temp_dir().join(format!("pi-compact-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut runtime = test_runtime(&root).await;
        runtime.model.context_window = 1_000_000;
        runtime.messages.push(pi_agent::agent::user_text_prompt(
            "hi".to_string(),
            pi_ai::types::now_ms(),
        ));
        let compacted = maybe_auto_compact(&mut runtime)
            .await
            .expect("auto-compact");
        assert!(!compacted, "no compaction under threshold");
        assert_eq!(runtime.messages.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn manual_compact_is_a_noop_without_session_history() {
        let _env = env_lock().lock().await;
        let root = std::env::temp_dir().join(format!("pi-compact-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut runtime = test_runtime(&root).await;

        let compacted = compact_interactive(&mut runtime, Some("Focus on decisions"), true)
            .await
            .expect("manual compact");

        assert!(!compacted);
        assert!(runtime.messages.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn footer_usage_aggregates_assistant_messages_and_hit_rate() {
        use pi_ai::types::{Cost, Message, Usage};
        let usage = |input: i64, cache_read: i64, output: i64| Usage {
            input,
            output,
            cache_read,
            cache_write: 0,
            cache_write_1h: None,
            reasoning: None,
            total_tokens: input + output + cache_read,
            cost: Cost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
                total: 0.01,
            },
        };
        let with_usage = |u: Usage| -> pi_agent::types::AgentMessage {
            let mut msg = pi_ai::providers::faux_assistant_message(
                vec![pi_ai::types::ContentBlock::text("hi")],
                pi_ai::providers::FauxAssistantOptions::default(),
            );
            msg.set_usage(u);
            pi_agent::types::AgentMessage::Core(Message::Assistant(msg))
        };

        let messages = vec![
            with_usage(usage(100, 50, 30)),
            with_usage(usage(200, 50, 70)),
        ];
        let (totals, hit_rate) = footer_usage_from_messages(&messages);
        let totals = totals.expect("usage present");
        assert_eq!(totals.input, 300);
        assert_eq!(totals.output, 100);
        assert_eq!(totals.cache_read, 100);
        // Last turn: 200 prompt, 50 cached => 50 / 250 = 20%.
        assert!((hit_rate.unwrap() - 20.0).abs() < 1e-9);
    }

    #[test]
    fn footer_usage_empty_when_no_assistant_usage() {
        let messages = vec![pi_agent::agent::user_text_prompt(
            "hi".to_string(),
            pi_ai::types::now_ms(),
        )];
        let (totals, hit_rate) = footer_usage_from_messages(&messages);
        assert!(totals.is_none());
        assert!(hit_rate.is_none());
    }

    #[test]
    fn cache_notice_is_rederived_with_idle_label_and_threshold() {
        let entries = vec![
            json!({
                "type": "message",
                "timestamp": 1_000,
                "message": {
                    "role": "assistant",
                    "provider": "anthropic",
                    "model": "claude",
                    "usage": {
                        "input": 0,
                        "output": 1,
                        "cacheRead": 0,
                        "cacheWrite": 25_000,
                        "totalTokens": 25_001,
                        "cost": {
                            "input": 0.0,
                            "output": 0.01,
                            "cache_read": 0.0,
                            "cache_write": 100.0,
                            "total": 100.01
                        }
                    }
                }
            }),
            json!({
                "type": "message",
                "timestamp": 301_001,
                "message": {
                    "role": "assistant",
                    "provider": "anthropic",
                    "model": "claude",
                    "usage": {
                        "input": 24_000,
                        "output": 1,
                        "cacheRead": 1_000,
                        "cacheWrite": 0,
                        "totalTokens": 25_001,
                        "cost": {
                            "input": 72_000.0,
                            "output": 0.01,
                            "cache_read": 300.0,
                            "cache_write": 0.0,
                            "total": 72_300.01
                        }
                    }
                }
            }),
        ];

        let notices = cache_notice_timestamps(&entries);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].0, 301_001);
        assert!(notices[0].1.contains("Cache miss after 5m idle"));
        assert!(notices[0].1.contains("24k tokens re-billed"));
    }

    #[test]
    fn footer_entries_include_summary_usage_and_cache_rebilling_line() {
        let entries = vec![
            json!({
                "type": "message",
                "timestamp": 1,
                "message": {
                    "role": "assistant",
                    "provider": "anthropic",
                    "model": "claude",
                    "usage": {
                        "input": 5_000,
                        "output": 10,
                        "cacheRead": 0,
                        "cacheWrite": 5_000,
                        "totalTokens": 10_010,
                        "cost": {"input": 1.0, "output": 0.1, "cache_read": 0.0, "cache_write": 2.0, "total": 3.1}
                    }
                }
            }),
            json!({
                "type": "compaction",
                "timestamp": 2,
                "usage": {
                    "input": 100,
                    "output": 20,
                    "cacheRead": 0,
                    "cacheWrite": 0,
                    "totalTokens": 120,
                    "cost": {"input": 0.2, "output": 0.1, "cache_read": 0.0, "cache_write": 0.0, "total": 0.3}
                }
            }),
        ];
        let (usage, _) = footer_usage_from_entries(&entries);
        let usage = usage.expect("usage");
        assert_eq!(usage.input, 5_100);
        assert_eq!(usage.output, 30);
        assert!((usage.cost - 3.4).abs() < 1e-9);
        let waste = crate::core::cache_stats::compute_cache_waste(
            &entries,
            &crate::core::cache_stats::NoPrices,
        );
        assert_eq!(format_cache_waste_line(waste), None);
        assert_eq!(
            format_cache_waste_line(crate::core::cache_stats::CacheWasteTotals {
                missed_tokens: 24_000,
                missed_cost: 0.25,
                miss_count: 2,
            }),
            Some("Cache Re-billed: $0.250 (24000 tokens, 2 misses)".to_string())
        );
    }

    #[test]
    fn transcript_reinjects_cache_notice_after_matching_assistant() {
        let mut assistant = pi_ai::providers::faux_assistant_message(
            vec![pi_ai::types::ContentBlock::text("answer")],
            pi_ai::providers::FauxAssistantOptions::default(),
        );
        assistant = assistant.with_timestamp(42);
        let messages = vec![pi_agent::types::AgentMessage::Core(Message::Assistant(
            assistant,
        ))];
        let transcript = it::compose_transcript_with_cache_notices(
            &messages,
            false,
            "",
            &[(42, "⚠ Cache miss: 24k tokens re-billed".to_string())],
        );
        assert!(transcript.contains("answer"));
        assert!(transcript.contains("> ⚠ Cache miss: 24k tokens re-billed"));
    }
}
