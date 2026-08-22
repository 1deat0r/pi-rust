//! `sessions` table helpers — port of
//! `packages/session-backends/sqlite-node/src/sqlite/storage/sessions.ts`.

use pi_agent::session::types::{SessionError, SessionErrorKind};
use serde_json::Value;
use rusqlite::Connection;

use crate::sql::SqlQuery;
use crate::types::SqliteSessionMetadata;

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub created_at: i64,
    pub metadata: Option<String>,
    pub cwd: String,
    pub parent_session_id: Option<String>,
    pub has_session_name: bool,
    pub session_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewSessionRow {
    pub id: String,
    pub created_at: i64,
    pub cwd: String,
    pub parent_session_id: Option<String>,
    pub metadata: Option<String>,
}

fn map_session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: row.get(0)?,
        created_at: row.get(1)?,
        metadata: row.get(2)?,
        cwd: row.get(3)?,
        parent_session_id: row.get(4)?,
        has_session_name: row.get::<_, i64>(5)? != 0,
        session_name: row.get(6)?,
    })
}

fn parse_metadata(metadata: Option<String>, session_id: &str) -> Result<Option<Value>, SessionError> {
    let Some(metadata) = metadata else { return Ok(None) };
    let parsed: Value = serde_json::from_str(&metadata).map_err(|error| {
        SessionError::new(
            SessionErrorKind::Storage,
            format!("Invalid SQLite session {session_id}: metadata is not valid JSON: {error}"),
        )
    })?;
    if !parsed.is_object() {
        return Err(SessionError::new(
            SessionErrorKind::Storage,
            format!("Invalid SQLite session {session_id}: metadata must be an object"),
        ));
    }
    Ok(Some(parsed))
}

fn parse_session_name(value: Option<String>, session_id: &str) -> Result<Option<String>, SessionError> {
    let Some(value) = value else { return Ok(None) };
    let parsed: Value = serde_json::from_str(&value).map_err(|error| {
        SessionError::new(
            SessionErrorKind::Storage,
            format!("Invalid SQLite session {session_id}: name is not valid JSON: {error}"),
        )
    })?;
    match parsed {
        Value::String(s) => Ok(Some(s)),
        _ => Err(SessionError::new(
            SessionErrorKind::Storage,
            format!("Invalid SQLite session {session_id}: name must be a string"),
        )),
    }
}

pub fn session_exists(db: &Connection, session_id: &str) -> rusqlite::Result<bool> {
    Ok(SqlQuery::new("SELECT 1 AS found FROM sessions WHERE id = ?")
        .bind(session_id)
        .get_row(db, |_row| Ok(()))?
        .is_some())
}

pub fn insert_session_row(db: &Connection, session: &NewSessionRow) -> rusqlite::Result<()> {
    SqlQuery::new(
        "INSERT INTO sessions (id, created_at, metadata, cwd, parent_session_id)
        VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&session.id)
    .bind(session.created_at)
    .bind(session.metadata.clone())
    .bind(&session.cwd)
    .bind(session.parent_session_id.clone())
    .run(db)?;
    Ok(())
}

const SESSION_ROW_SELECT: &str = "SELECT s.id, s.created_at, s.metadata, s.cwd, s.parent_session_id,
        name_fact.seq IS NOT NULL AS has_session_name,
        name_fact.value AS session_name
    FROM sessions AS s
    LEFT JOIN facts AS name_fact
        ON name_fact.session_id = s.id
        AND name_fact.kind = 'name'
        AND name_fact.key IS NULL
        AND name_fact.seq = (
            SELECT MAX(f.seq)
            FROM facts AS f
            WHERE f.session_id = s.id AND f.kind = 'name' AND f.key IS NULL
        )";

pub fn read_session_row(db: &Connection, session_id: &str) -> rusqlite::Result<Option<SessionRow>> {
    SqlQuery::new(format!("{SESSION_ROW_SELECT} WHERE s.id = ?"))
        .bind(session_id)
        .get_row(db, map_session_row)
}

pub fn read_session_rows(db: &Connection, cwd: Option<&str>) -> rusqlite::Result<Vec<SessionRow>> {
    let mut query = SqlQuery::new(SESSION_ROW_SELECT.to_string());
    if let Some(cwd) = cwd {
        query = query.inline(&SqlQuery::new(" WHERE s.cwd = ?").bind(cwd));
    }
    query = query.inline(&SqlQuery::new(" ORDER BY s.created_at DESC"));
    query.all_rows(db, map_session_row)
}

pub fn delete_session_row(db: &Connection, session_id: &str) -> rusqlite::Result<()> {
    SqlQuery::new("DELETE FROM sessions WHERE id = ?").bind(session_id).run(db)?;
    Ok(())
}

pub fn decode_session_metadata(row: &SessionRow, path: &str) -> Result<SqliteSessionMetadata, SessionError> {
    let metadata = parse_metadata(row.metadata.clone(), &row.id)?;
    let name = if row.has_session_name {
        parse_session_name(row.session_name.clone(), &row.id)?
    } else {
        None
    };
    Ok(SqliteSessionMetadata {
        id: row.id.clone(),
        created_at: row.created_at as u64,
        cwd: row.cwd.clone(),
        path: path.to_string(),
        parent_session_id: row.parent_session_id.clone(),
        name,
        metadata,
    })
}
