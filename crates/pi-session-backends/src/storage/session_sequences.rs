//! `session_sequences` table helpers — port of
//! `packages/session-backends/sqlite-node/src/sqlite/storage/session-sequences.ts`.

use pi_agent::session::types::{SessionError, SessionErrorKind};
use rusqlite::Connection;

use crate::sql::SqlQuery;

pub fn create_sequence(db: &Connection, session_id: &str, next_seq: i64) -> rusqlite::Result<()> {
    SqlQuery::new("INSERT INTO session_sequences (session_id, next_seq) VALUES (?, ?)")
        .bind(session_id)
        .bind(next_seq)
        .run(db)?;
    Ok(())
}

pub fn get_next_sequence(db: &Connection, session_id: &str) -> Result<i64, SessionError> {
    let row = SqlQuery::new("SELECT next_seq FROM session_sequences WHERE session_id = ?")
        .bind(session_id)
        .get_row(db, |row| row.get::<_, i64>(0))
        .map_err(|error| {
            SessionError::new(SessionErrorKind::Storage, format!("Failed to read next sequence: {error}"))
        })?;
    row.ok_or_else(|| {
        SessionError::new(SessionErrorKind::Storage, format!("Missing sequence row for session {session_id}"))
    })
}

pub fn set_next_sequence(db: &Connection, session_id: &str, next_seq: i64) -> rusqlite::Result<()> {
    SqlQuery::new("UPDATE session_sequences SET next_seq = ? WHERE session_id = ?")
        .bind(next_seq)
        .bind(session_id)
        .run(db)?;
    Ok(())
}

pub fn advance_sequence(db: &Connection, session_id: &str, seq: i64) -> rusqlite::Result<()> {
    set_next_sequence(db, session_id, seq + 1)
}

pub fn delete_sequence(db: &Connection, session_id: &str) -> rusqlite::Result<()> {
    SqlQuery::new("DELETE FROM session_sequences WHERE session_id = ?").bind(session_id).run(db)?;
    Ok(())
}
