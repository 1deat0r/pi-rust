//! `records` table helpers — port of
//! `packages/session-backends/sqlite-node/src/sqlite/storage/records.ts`.

use pi_agent::session::state::EntryOrder;
use pi_agent::session::types::{SessionError, SessionErrorKind};
use rusqlite::Connection;

use crate::sql::{join_sql_fragments, SqlQuery};

#[derive(Debug, Clone)]
pub struct RecordRow {
    pub session_id: String,
    pub seq: i64,
    pub id: String,
    pub lane: String,
    pub run_id: Option<String>,
    pub record_type: String,
    pub op_kind: Option<String>,
    pub timestamp: i64,
    pub payload: String,
}

pub fn map_record_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RecordRow> {
    Ok(RecordRow {
        session_id: row.get(0)?,
        seq: row.get(1)?,
        id: row.get(2)?,
        lane: row.get(3)?,
        run_id: row.get(4)?,
        record_type: row.get(5)?,
        op_kind: row.get(6)?,
        timestamp: row.get(7)?,
        payload: row.get(8)?,
    })
}

#[derive(Debug, Clone)]
pub struct NewRecordRow {
    pub seq: i64,
    pub id: String,
    pub lane: String,
    pub run_id: Option<String>,
    pub record_type: String,
    pub op_kind: Option<String>,
    pub timestamp: i64,
    pub payload: String,
}

pub fn append_record_row(db: &Connection, session_id: &str, record: &NewRecordRow) -> rusqlite::Result<()> {
    SqlQuery::new(
        "INSERT INTO records
            (session_id, seq, id, lane, run_id, type, op_kind, timestamp, payload)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(session_id)
    .bind(record.seq)
    .bind(&record.id)
    .bind(&record.lane)
    .bind(record.run_id.clone())
    .bind(&record.record_type)
    .bind(record.op_kind.clone())
    .bind(record.timestamp)
    .bind(&record.payload)
    .run(db)?;
    Ok(())
}

pub fn id_exists_in_records(db: &Connection, session_id: &str, id: &str) -> rusqlite::Result<bool> {
    Ok(SqlQuery::new("SELECT 1 AS found FROM records WHERE session_id = ? AND id = ? LIMIT 1")
        .bind(session_id)
        .bind(id)
        .get_row(db, |_row| Ok(()))?
        .is_some())
}

pub fn delete_record_rows(db: &Connection, session_id: &str) -> rusqlite::Result<()> {
    SqlQuery::new("DELETE FROM records WHERE session_id = ?").bind(session_id).run(db)?;
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct ReadRecordRowsOptions<'a> {
    pub lane: Option<&'a str>,
    pub record_type: Option<&'a str>,
    pub run_id: Option<&'a str>,
    pub operation_kind: Option<&'a str>,
    pub after_seq: Option<i64>,
    pub order: Option<EntryOrder>,
    pub limit: Option<i64>,
}

pub fn read_record_rows(
    db: &Connection,
    session_id: &str,
    query: &ReadRecordRowsOptions<'_>,
) -> rusqlite::Result<Vec<RecordRow>> {
    let mut predicates: Vec<SqlQuery> = vec![SqlQuery::new("session_id = ?").bind(session_id)];
    if let Some(lane) = query.lane {
        predicates.push(SqlQuery::new("lane = ?").bind(lane));
    }
    if let Some(record_type) = query.record_type {
        predicates.push(SqlQuery::new("type = ?").bind(record_type));
    }
    if let Some(run_id) = query.run_id {
        predicates.push(SqlQuery::new("run_id = ?").bind(run_id));
    }
    if let Some(operation_kind) = query.operation_kind {
        predicates.push(SqlQuery::new("op_kind = ?").bind(operation_kind));
    }
    if let Some(after_seq) = query.after_seq {
        predicates.push(SqlQuery::new("seq > ?").bind(after_seq));
    }
    let where_clause = join_sql_fragments(&predicates, " AND ");
    let mut full = SqlQuery::new(
        "SELECT session_id, seq, id, lane, run_id, type, op_kind, timestamp, payload
        FROM records
        WHERE ",
    )
    .inline(&where_clause);
    full = full.inline(&SqlQuery::new(if matches!(query.order, Some(EntryOrder::OldestFirst)) {
        " ORDER BY seq ASC"
    } else {
        " ORDER BY seq DESC"
    }));
    if let Some(limit) = query.limit {
        full = full.inline(&SqlQuery::new(" LIMIT ?").bind(limit));
    }
    full.all_rows(db, map_record_row)
}

pub fn read_open_operation_rows(
    db: &Connection,
    session_id: &str,
    lane: &str,
) -> Result<Vec<RecordRow>, SessionError> {
    let lane_row = SqlQuery::new("SELECT open_operation_id FROM lanes WHERE session_id = ? AND lane = ?")
        .bind(session_id)
        .bind(lane)
        .get_row(db, |row| row.get::<_, Option<String>>(0))
        .map_err(|error| SessionError::new(SessionErrorKind::Storage, format!("Failed to read lane: {error}")))?;
    let Some(open_operation_id) = lane_row.flatten() else {
        return Ok(Vec::new());
    };
    let record = SqlQuery::new(
        "SELECT session_id, seq, id, lane, run_id, type, op_kind, timestamp, payload
        FROM records
        WHERE session_id = ?
            AND id = ?",
    )
    .bind(session_id)
    .bind(&open_operation_id)
    .get_row(db, map_record_row)
    .map_err(|error| SessionError::new(SessionErrorKind::Storage, format!("Failed to read open operation: {error}")))?;
    let record = match record {
        Some(record) => record,
        None => {
            return Err(SessionError::new(
                SessionErrorKind::Storage,
                format!("Lane {lane} points at missing open operation {open_operation_id}"),
            ))
        }
    };
    if record.lane != lane || record.record_type != "operation_started" {
        return Err(SessionError::new(
            SessionErrorKind::Storage,
            format!("Lane {lane} points at invalid open operation {open_operation_id}"),
        ));
    }
    Ok(vec![record])
}
