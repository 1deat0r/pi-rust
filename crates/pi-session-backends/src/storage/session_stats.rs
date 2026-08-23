//! `session_stats` table helpers — port of
//! `packages/session-backends/sqlite-node/src/sqlite/storage/session-stats.ts`.

use pi_agent::session::types::{SessionError, SessionErrorKind, SessionStats};
use pi_ai::types::Usage;
use rusqlite::Connection;

use crate::sql::SqlQuery;

pub fn create_stats(db: &Connection, session_id: &str, message_count: i64) -> rusqlite::Result<()> {
    SqlQuery::new(
        "INSERT INTO session_stats
            (session_id, message_count, cached_tokens, uncached_tokens, total_tokens, cost_total)
            VALUES (?, ?, 0, 0, 0, 0)",
    )
    .bind(session_id)
    .bind(message_count)
    .run(db)?;
    Ok(())
}

pub fn read_stats(db: &Connection, session_id: &str) -> Result<SessionStats, SessionError> {
    struct StatsRow {
        message_count: i64,
        cached_tokens: f64,
        uncached_tokens: f64,
        total_tokens: f64,
        cost_total: f64,
    }
    let row = SqlQuery::new(
        "SELECT session_id, message_count, cached_tokens, uncached_tokens, total_tokens, cost_total
        FROM session_stats
        WHERE session_id = ?",
    )
    .bind(session_id)
    .get_row(db, |row| {
        Ok(StatsRow {
            message_count: row.get(1)?,
            cached_tokens: row.get(2)?,
            uncached_tokens: row.get(3)?,
            total_tokens: row.get(4)?,
            cost_total: row.get(5)?,
        })
    })
    .map_err(|error| {
        SessionError::new(
            SessionErrorKind::Storage,
            format!("Failed to read stats: {error}"),
        )
    })?;
    let row = row.ok_or_else(|| {
        SessionError::new(
            SessionErrorKind::Storage,
            format!("Missing stats row for session {session_id}"),
        )
    })?;
    Ok(SessionStats {
        message_count: row.message_count as u64,
        cached_tokens: row.cached_tokens as i64,
        uncached_tokens: row.uncached_tokens as i64,
        total_tokens: row.total_tokens as i64,
        cost_total: row.cost_total,
    })
}

fn require_changes(
    result: rusqlite::Result<u64>,
    session_id: &str,
    what: &str,
) -> Result<(), SessionError> {
    match result {
        Ok(1) => Ok(()),
        Ok(_) => Err(SessionError::new(
            SessionErrorKind::Storage,
            format!("Missing stats row for session {session_id} ({what})"),
        )),
        Err(error) => Err(SessionError::new(
            SessionErrorKind::Storage,
            format!("Failed to update stats for session {session_id} ({what}): {error}"),
        )),
    }
}

pub fn increment_message_count(db: &Connection, session_id: &str) -> Result<(), SessionError> {
    let result = SqlQuery::new(
        "UPDATE session_stats SET message_count = message_count + 1 WHERE session_id = ?",
    )
    .bind(session_id)
    .run(db)
    .map(|r| r.changes);
    require_changes(result, session_id, "increment message count")
}

pub fn add_usage_to_stats(
    db: &Connection,
    session_id: &str,
    usage: &Usage,
) -> Result<(), SessionError> {
    let uncached: f64 = (usage.input + usage.cache_write) as f64;
    let result = SqlQuery::new(
        "UPDATE session_stats
        SET cached_tokens = cached_tokens + ?,
            uncached_tokens = uncached_tokens + ?,
            total_tokens = total_tokens + ?,
            cost_total = cost_total + ?
        WHERE session_id = ?",
    )
    .bind(usage.cache_read as f64)
    .bind(uncached)
    .bind(usage.total_tokens as f64)
    .bind(usage.cost.total)
    .bind(session_id)
    .run(db)
    .map(|r| r.changes);
    require_changes(result, session_id, "add usage")
}

pub fn delete_stats(db: &Connection, session_id: &str) -> rusqlite::Result<()> {
    SqlQuery::new("DELETE FROM session_stats WHERE session_id = ?")
        .bind(session_id)
        .run(db)?;
    Ok(())
}
