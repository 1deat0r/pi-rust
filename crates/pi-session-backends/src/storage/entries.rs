//! `entries` table helpers — port of
//! `packages/session-backends/sqlite-node/src/sqlite/storage/entries.ts`.

use pi_agent::session::state::{EntryCursor, EntryOrder};
use pi_agent::session::types::Entry;
use rusqlite::Connection;

use crate::sql::SqlQuery;

#[derive(Debug, Clone)]
pub struct EntryRow {
    pub session_id: String,
    pub seq: i64,
    pub id: String,
    pub parent_id: Option<String>,
    pub entry_type: String,
    pub timestamp: i64,
    pub payload: String,
}

pub fn map_entry_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EntryRow> {
    Ok(EntryRow {
        session_id: row.get(0)?,
        seq: row.get(1)?,
        id: row.get(2)?,
        parent_id: row.get(3)?,
        entry_type: row.get(4)?,
        timestamp: row.get(5)?,
        payload: row.get(6)?,
    })
}

#[derive(Debug, Clone)]
pub struct NewEntryRow {
    pub seq: i64,
    pub id: String,
    pub parent_id: Option<String>,
    pub entry_type: String,
    pub timestamp: i64,
    pub payload: String,
}

/// The non-storage-assigned fields of an entry, serialized into `payload`.
/// Mirrors upstream `entryPayload` (type/id/seq/parentId/timestamp stripped).
pub fn entry_payload(entry: &Entry) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    match entry {
        Entry::Message { message, terminate, .. } => {
            obj.insert("message".into(), serde_json::to_value(message).unwrap_or_default());
            if let Some(terminate) = terminate {
                obj.insert("terminate".into(), serde_json::json!(terminate));
            }
        }
        Entry::ModelChange { provider, model_id, .. } => {
            obj.insert("provider".into(), serde_json::json!(provider));
            obj.insert("modelId".into(), serde_json::json!(model_id));
        }
        Entry::ThinkingLevel { thinking_level, .. } => {
            obj.insert("thinkingLevel".into(), serde_json::json!(thinking_level));
        }
        Entry::ActiveTools { active_tool_names, .. } => {
            obj.insert("activeToolNames".into(), serde_json::json!(active_tool_names));
        }
        Entry::Compaction { summary, retained_tail, tokens_before, details, usage, .. } => {
            obj.insert("summary".into(), serde_json::json!(summary));
            obj.insert("retainedTail".into(), serde_json::json!(retained_tail));
            obj.insert("tokensBefore".into(), serde_json::json!(tokens_before));
            if let Some(details) = details {
                obj.insert("details".into(), details.clone());
            }
            if let Some(usage) = usage {
                obj.insert("usage".into(), serde_json::to_value(usage).unwrap_or_default());
            }
        }
        Entry::BranchSummary { from_id, summary, details, usage, .. } => {
            obj.insert("fromId".into(), serde_json::json!(from_id));
            obj.insert("summary".into(), serde_json::json!(summary));
            if let Some(details) = details {
                obj.insert("details".into(), details.clone());
            }
            if let Some(usage) = usage {
                obj.insert("usage".into(), serde_json::to_value(usage).unwrap_or_default());
            }
        }
        Entry::Custom { custom_type, data, .. } => {
            obj.insert("customType".into(), serde_json::json!(custom_type));
            if let Some(data) = data {
                obj.insert("data".into(), data.clone());
            }
        }
    }
    serde_json::Value::Object(obj)
}

/// Serializes an entry payload to the JSON string stored in `entries.payload`.
/// The payload mirrors the entry minus `type/id/seq/parentId/timestamp`; the
/// entry `type` is stored in its own column.
pub fn serialize_payload(entry: &Entry) -> String {
    serde_json::to_string(&entry_payload(entry)).expect("entry payload serializes")
}

pub fn insert_entry_row(db: &Connection, session_id: &str, entry: &NewEntryRow) -> rusqlite::Result<()> {
    SqlQuery::new(
        "INSERT INTO entries (session_id, id, seq, parent_id, type, timestamp, payload)
        VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(session_id)
    .bind(&entry.id)
    .bind(entry.seq)
    .bind(entry.parent_id.clone())
    .bind(&entry.entry_type)
    .bind(entry.timestamp)
    .bind(&entry.payload)
    .run(db)?;
    Ok(())
}

pub fn read_entry_row(db: &Connection, session_id: &str, entry_id: &str) -> rusqlite::Result<Option<EntryRow>> {
    SqlQuery::new(
        "SELECT session_id, seq, id, parent_id, type, timestamp, payload
        FROM entries
        WHERE session_id = ? AND id = ?",
    )
    .bind(session_id)
    .bind(entry_id)
    .get_row(db, map_entry_row)
}

pub struct ReadEntryRowsOptions<'a> {
    pub after_seq: Option<i64>,
    pub cursor: Option<EntryCursor>,
    pub entry_type: Option<&'a str>,
    pub order: Option<EntryOrder>,
    pub limit: Option<i64>,
}

pub fn read_entry_rows(db: &Connection, session_id: &str, options: ReadEntryRowsOptions<'_>) -> rusqlite::Result<Vec<EntryRow>> {
    let oldest_first = matches!(options.order, Some(EntryOrder::OldestFirst));
    let mut query = SqlQuery::new(
        "SELECT session_id, seq, id, parent_id, type, timestamp, payload
        FROM entries
        WHERE session_id = ?",
    )
    .bind(session_id);
    if let Some(after_seq) = options.after_seq {
        query = query.inline(&SqlQuery::new(" AND seq > ?").bind(after_seq));
    }
    if let Some(cursor) = options.cursor {
        if oldest_first {
            query = query.inline(&SqlQuery::new(" AND seq > ?").bind(cursor.after_seq as i64));
        } else {
            query = query.inline(&SqlQuery::new(" AND seq < ?").bind(cursor.after_seq as i64));
        }
    }
    if let Some(entry_type) = options.entry_type {
        query = query.inline(&SqlQuery::new(" AND type = ?").bind(entry_type));
    }
    query = query.inline(&SqlQuery::new(if oldest_first { " ORDER BY seq ASC" } else { " ORDER BY seq DESC" }));
    if let Some(limit) = options.limit {
        query = query.inline(&SqlQuery::new(" LIMIT ?").bind(limit));
    }
    query.all_rows(db, map_entry_row)
}

pub fn id_exists_in_entries(db: &Connection, session_id: &str, id: &str) -> rusqlite::Result<bool> {
    Ok(SqlQuery::new("SELECT 1 AS found FROM entries WHERE session_id = ? AND id = ? LIMIT 1")
        .bind(session_id)
        .bind(id)
        .get_row(db, |_row| Ok(()))?
        .is_some())
}

pub fn delete_entry_rows(db: &Connection, session_id: &str) -> rusqlite::Result<()> {
    SqlQuery::new("DELETE FROM entries WHERE session_id = ?").bind(session_id).run(db)?;
    Ok(())
}
