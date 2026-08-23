//! `facts` table helpers — port of
//! `packages/session-backends/sqlite-node/src/sqlite/storage/facts.ts`.

use rusqlite::Connection;

use crate::sql::{empty, SqlQuery};

#[derive(Debug, Clone)]
pub struct FactRow {
    pub session_id: String,
    pub seq: i64,
    pub kind: String,
    pub key: Option<String>,
    pub value: Option<String>,
}

pub fn map_fact_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FactRow> {
    Ok(FactRow {
        session_id: row.get(0)?,
        seq: row.get(1)?,
        kind: row.get(2)?,
        key: row.get(3)?,
        value: row.get(4)?,
    })
}

pub fn append_fact(
    db: &Connection,
    session_id: &str,
    seq: i64,
    kind: &str,
    key: Option<&str>,
    value: Option<&str>,
) -> rusqlite::Result<()> {
    SqlQuery::new("INSERT INTO facts (session_id, seq, kind, key, value) VALUES (?, ?, ?, ?, ?)")
        .bind(session_id)
        .bind(seq)
        .bind(kind)
        .bind(key)
        .bind(value)
        .run(db)?;
    Ok(())
}

pub fn read_latest_fact(
    db: &Connection,
    session_id: &str,
    kind: &str,
    key: Option<&str>,
) -> rusqlite::Result<Option<FactRow>> {
    SqlQuery::new(
        "SELECT session_id, seq, kind, key, value
        FROM facts INDEXED BY idx_facts_session_kind_key_seq
        WHERE session_id = ? AND kind = ? AND key IS ?
        ORDER BY seq DESC
        LIMIT 1",
    )
    .bind(session_id)
    .bind(kind)
    .bind(key)
    .get_row(db, map_fact_row)
}

pub fn read_latest_label_facts(
    db: &Connection,
    session_id: &str,
) -> rusqlite::Result<Vec<(String, String)>> {
    SqlQuery::new(
        "SELECT f.key, f.value
        FROM facts AS f INDEXED BY idx_facts_session_kind_key_seq
        WHERE f.session_id = ?
            AND f.kind = 'label'
            AND f.value IS NOT NULL
            AND f.seq = (
                SELECT MAX(candidate.seq)
                FROM facts AS candidate INDEXED BY idx_facts_session_kind_key_seq
                WHERE candidate.session_id = f.session_id
                    AND candidate.kind = f.kind
                    AND candidate.key IS f.key
            )
        ORDER BY f.key",
    )
    .bind(session_id)
    .all_rows(db, |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })
}

pub fn read_fact_rows(
    db: &Connection,
    session_id: &str,
    after_seq: Option<i64>,
    limit: Option<i64>,
) -> rusqlite::Result<Vec<FactRow>> {
    let mut query =
        SqlQuery::new("SELECT session_id, seq, kind, key, value FROM facts WHERE session_id = ?")
            .bind(session_id);
    if let Some(after) = after_seq {
        query = query.inline(&SqlQuery::new(" AND seq > ?").bind(after));
    }
    query = query.inline(&SqlQuery::new(" ORDER BY seq"));
    if let Some(limit) = limit {
        query = query.inline(&SqlQuery::new(" LIMIT ?").bind(limit));
    }
    query.all_rows(db, map_fact_row)
}

pub fn delete_fact_rows(db: &Connection, session_id: &str) -> rusqlite::Result<()> {
    SqlQuery::new("DELETE FROM facts WHERE session_id = ?")
        .bind(session_id)
        .run(db)?;
    Ok(())
}

/// Empty fragment helper (mirrors `sql\`\`` usage in the upstream port).
pub fn _empty_fragment() -> SqlQuery {
    empty()
}
