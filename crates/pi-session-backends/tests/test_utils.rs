#![allow(dead_code)]
//! Shared test fixtures — port of upstream `test/test-utils.ts`.

use pi_agent::session::types::Entry;
use pi_agent::types::AgentMessage;
use pi_ai::types::{ContentBlock, Cost, Message, StopReason, Usage, UserContent};

/// Creates a temp dir for one test.
pub fn create_temp_dir() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "pi-session-backend-sqlite-{}-{}",
        std::process::id(),
        counter()
    ));
    std::fs::create_dir_all(&path).expect("create temp dir");
    path
}

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn counter() -> u64 {
    COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

pub fn user_message(text: &str) -> AgentMessage {
    AgentMessage::Core(Message::User(UserContent::blocks(
        vec![ContentBlock::text(text)],
        1,
    )))
}

pub fn assistant_message(text: &str) -> AgentMessage {
    AgentMessage::Core(Message::Assistant(
        pi_ai::types::AssistantMessage::Assistant {
            content: vec![ContentBlock::text(text)],
            api: Some("anthropic-messages".into()),
            provider: Some("anthropic".into()),
            model: Some("claude-sonnet-4-5".into()),
            response_model: None,
            response_id: None,
            usage: Some(zero_usage()),
            stop_reason: Some(StopReason::Stop),
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: now_ms(),
        },
    ))
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn zero_usage() -> Usage {
    Usage {
        input: 0,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 0,
        cost: Cost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            total: 0.0,
        },
    }
}

/// Appends a compaction entry; returns its id.
pub async fn append_sqlite_compaction(
    session: &mut pi_session_backends::session::SqliteSession,
    summary: &str,
    tokens_before: u64,
    details: Option<serde_json::Value>,
    usage: Option<Usage>,
    retained_tail: Vec<AgentMessage>,
) -> Result<String, pi_agent::session::types::SessionError> {
    session
        .append_entry(
            pi_agent::session::types::EntryNoStats::Compaction {
                id: pi_session_backends::new_id(),
                summary: summary.to_string(),
                retained_tail,
                tokens_before,
                details,
                usage,
            },
            "main",
        )
        .await
        .map(|entry| entry.id().to_string())
}

/// Moves the main lane to `entry_id` and optionally appends a branch-summary.
pub async fn move_sqlite_main_lane(
    session: &mut pi_session_backends::session::SqliteSession,
    entry_id: Option<&str>,
    summary: Option<(String, Option<serde_json::Value>, Option<Usage>)>,
) -> Result<Option<String>, pi_agent::session::types::SessionError> {
    session.move_lane("main", entry_id).await?;
    let Some((summary, details, usage)) = summary else {
        return Ok(None);
    };
    let entry = session
        .append_entry(
            pi_agent::session::types::EntryNoStats::BranchSummary {
                id: pi_session_backends::new_id(),
                from_id: entry_id.unwrap_or("root").to_string(),
                summary,
                details,
                usage,
            },
            "main",
        )
        .await?;
    Ok(Some(entry.id().to_string()))
}

/// `getSqliteBranch`: newest window capped by the nearest compaction.
pub async fn get_sqlite_branch(
    session: &pi_session_backends::session::SqliteSession,
) -> Result<Vec<Entry>, pi_agent::session::types::SessionError> {
    let start = match session.get_leaf_id().await? {
        Some(start) => start,
        None => return Ok(Vec::new()),
    };
    let bounds = pi_agent::session::state::BranchBounds {
        stop_at_type: Some("compaction".into()),
        stop_at_id: None,
    };
    let query = pi_agent::session::state::EntryQuery::default();
    let mut newest = session
        .find_entries_on_branch(&query, Some(&start), &bounds)
        .await?;
    newest.reverse();
    Ok(newest)
}

pub async fn get_sqlite_entries(
    session: &pi_session_backends::session::SqliteSession,
    after_entry_seq: Option<u64>,
    limit: Option<usize>,
) -> Result<Vec<Entry>, pi_agent::session::types::SessionError> {
    let query = pi_agent::session::state::EntryQuery {
        order: Some(pi_agent::session::state::EntryOrder::OldestFirst),
        limit,
        cursor: after_entry_seq
            .map(|after_seq| pi_agent::session::state::EntryCursor { after_seq }),
        ..Default::default()
    };
    session.find_entries(&query).await
}
