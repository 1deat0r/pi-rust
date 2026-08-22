//! `branch_entries` (derived branch cache) helpers — port of
//! `packages/session-backends/sqlite-node/src/sqlite/storage/branch-entries.ts`.

use pi_agent::session::state::{EntryCursor, EntryOrder};
use pi_agent::session::types::{SessionError, SessionErrorKind};
use rusqlite::Connection;

use crate::sql::{join_sql_fragments, SqlQuery};

/// Derived root-to-tip branch cache membership. Canonical parent links remain
/// in `entries`.
#[derive(Debug, Clone)]
pub struct CachedBranch {
    pub branch_id: String,
    pub leaf_seq: i64,
}

#[derive(Debug, Clone)]
pub struct CachedBranchEntryRow {
    pub session_id: String,
    pub id: String,
    pub entry_seq: i64,
    pub parent_id: Option<String>,
    pub entry_type: String,
    pub timestamp: i64,
    pub payload: String,
}

pub fn map_cached_branch_entry_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CachedBranchEntryRow> {
    Ok(CachedBranchEntryRow {
        session_id: row.get(0)?,
        id: row.get(1)?,
        entry_seq: row.get(2)?,
        parent_id: row.get(3)?,
        entry_type: row.get(4)?,
        timestamp: row.get(5)?,
        payload: row.get(6)?,
    })
}

#[derive(Debug, Clone, Default)]
pub struct CachedBranchQuery {
    pub entry_type: Option<String>,
    pub custom_type: Option<String>,
    pub stop_at_type: Option<String>,
    pub stop_at_id: Option<String>,
    pub cursor: Option<EntryCursor>,
    pub order: Option<EntryOrder>,
    pub limit: Option<i64>,
}

pub fn read_cached_branch(
    db: &Connection,
    session_id: &str,
    leaf_id: &str,
) -> rusqlite::Result<Option<CachedBranch>> {
    let membership = SqlQuery::new(
        "SELECT branch_id, entry_seq
        FROM branch_entries
        WHERE session_id = ? AND entry_id = ?
        ORDER BY branch_id
        LIMIT 1",
    )
    .bind(session_id)
    .bind(leaf_id)
    .get_row(db, |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?;
    Ok(membership.map(|(branch_id, entry_seq)| CachedBranch { branch_id, leaf_seq: entry_seq }))
}

pub fn query_cached_branch_rows(
    db: &Connection,
    session_id: &str,
    branch: &CachedBranch,
    query: &CachedBranchQuery,
) -> rusqlite::Result<Vec<CachedBranchEntryRow>> {
    let oldest_first = matches!(query.order, Some(EntryOrder::OldestFirst));

    // stop predicates: `stop.entry_type = ?` and/or `stop.entry_id = ?`.
    let mut stop_predicates: Vec<SqlQuery> = Vec::new();
    if let Some(stop_at_type) = &query.stop_at_type {
        stop_predicates.push(SqlQuery::new("stop.entry_type = ?").bind(stop_at_type));
    }
    if let Some(stop_at_id) = &query.stop_at_id {
        stop_predicates.push(SqlQuery::new("stop.entry_id = ?").bind(stop_at_id));
    }

    // `SELECT MIN|MAX(stop.entry_seq) ...` subquery rendered inline.
    let boundary = if stop_predicates.is_empty() {
        None
    } else {
        let aggregate = if oldest_first { "MIN" } else { "MAX" };
        Some(
            SqlQuery::new(format!(
                "SELECT {aggregate}(stop.entry_seq)
                FROM branch_entries AS stop
                WHERE stop.session_id = ?
                    AND stop.branch_id = ?
                    AND stop.entry_seq <= ?
                    AND ("
            ))
            .bind(session_id)
            .bind(&branch.branch_id)
            .bind(branch.leaf_seq)
            .inline(&join_sql_fragments(&stop_predicates, " OR "))
            .inline(&SqlQuery::new(")")),
        )
    };

    let boundary_comparison = if oldest_first { "<=" } else { ">=" };
    let cursor_comparison = if oldest_first { ">" } else { "<" };
    let direction = if oldest_first { "ASC" } else { "DESC" };

    let mut predicates: Vec<SqlQuery> = vec![
        SqlQuery::new("b.session_id = ?").bind(session_id),
        SqlQuery::new("b.branch_id = ?").bind(&branch.branch_id),
        SqlQuery::new("b.entry_seq <= ?").bind(branch.leaf_seq),
    ];
    if let Some(boundary) = &boundary {
        let fallback = if oldest_first { branch.leaf_seq } else { 0 };
        // `b.entry_seq {<=|>=} COALESCE((<subquery>), {fallback})`
        predicates.push(
            SqlQuery::new(format!("b.entry_seq {boundary_comparison} COALESCE(("))
                .inline(boundary)
                .inline(&SqlQuery::new(format!("), {fallback})"))),
        );
    }
    if let Some(cursor) = query.cursor {
        predicates.push(SqlQuery::new(format!("b.entry_seq {cursor_comparison} ?")).bind(cursor.after_seq as i64));
    }
    if let Some(entry_type) = &query.entry_type {
        predicates.push(SqlQuery::new("b.entry_type = ?").bind(entry_type));
    }
    if let Some(custom_type) = &query.custom_type {
        predicates.push(SqlQuery::new("b.custom_type = ?").bind(custom_type));
    }

    let mut full = SqlQuery::new(
        "SELECT e.session_id, e.id, e.seq AS entry_seq, e.parent_id, e.type, e.timestamp, e.payload
        FROM branch_entries AS b
        JOIN entries AS e ON e.session_id = b.session_id AND e.id = b.entry_id
        WHERE ",
    )
    .inline(&join_sql_fragments(&predicates, " AND "));
    full = full.inline(&SqlQuery::new(format!(" ORDER BY b.entry_seq {direction}")));
    if let Some(limit) = query.limit {
        full = full.inline(&SqlQuery::new(" LIMIT ?").bind(limit));
    }
    full.all_rows(db, map_cached_branch_entry_row)
}

pub fn delete_branch_entries(db: &Connection, session_id: &str) -> rusqlite::Result<()> {
    SqlQuery::new("DELETE FROM branch_entries WHERE session_id = ?").bind(session_id).run(db)?;
    Ok(())
}

pub fn insert_branch_entry(
    db: &Connection,
    session_id: &str,
    branch_id: &str,
    entry_id: &str,
    entry_seq: i64,
    entry_type: &str,
    custom_type: Option<&str>,
) -> rusqlite::Result<()> {
    SqlQuery::new(
        "INSERT INTO branch_entries
            (session_id, branch_id, entry_id, entry_seq, entry_type, custom_type)
            VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(session_id)
    .bind(branch_id)
    .bind(entry_id)
    .bind(entry_seq)
    .bind(entry_type)
    .bind(custom_type)
    .run(db)?;
    Ok(())
}

#[derive(Debug, Clone)]
struct BranchPathEntryRow {
    id: String,
    seq: i64,
    parent_id: Option<String>,
    entry_type: String,
    payload: String,
}

fn custom_type_from_payload(row: &BranchPathEntryRow) -> Result<Option<String>, SessionError> {
    if row.entry_type != "custom" {
        return Ok(None);
    }
    let parsed: serde_json::Value = serde_json::from_str(&row.payload).map_err(|error| {
        SessionError::new(
            SessionErrorKind::InvalidEntry,
            format!("Invalid SQLite session entry {}: failed to decode entry {}: {error}", row.id, row.id),
        )
    })?;
    let custom_type = parsed.get("customType").and_then(|v| v.as_str());
    match custom_type {
        Some(custom_type) => Ok(Some(custom_type.to_string())),
        None => Err(SessionError::new(
            SessionErrorKind::InvalidEntry,
            format!("Invalid SQLite session entry {}: failed to decode entry {}", row.id, row.id),
        )),
    }
}

pub fn insert_branch_entries_for_path(
    db: &Connection,
    session_id: &str,
    branch_id: &str,
    leaf_id: &str,
) -> Result<(), SessionError> {
    let mut path: Vec<BranchPathEntryRow> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut entry_id: Option<String> = Some(leaf_id.to_string());

    while let Some(current) = entry_id {
        if seen.contains(&current) {
            return Err(SessionError::new(
                SessionErrorKind::InvalidEntry,
                format!("Entry parent cycle at {current}"),
            ));
        }
        seen.insert(current.clone());
        let row = SqlQuery::new(
            "SELECT id, seq, parent_id, type, payload
            FROM entries
            WHERE session_id = ? AND id = ?",
        )
        .bind(session_id)
        .bind(&current)
        .get_row(db, |row| {
            Ok(BranchPathEntryRow {
                id: row.get(0)?,
                seq: row.get(1)?,
                parent_id: row.get(2)?,
                entry_type: row.get(3)?,
                payload: row.get(4)?,
            })
        })
        .map_err(|error| SessionError::new(SessionErrorKind::Storage, format!("Failed to read entry: {error}")))?;
        let row = row.ok_or_else(|| SessionError::new(SessionErrorKind::InvalidEntry, format!("Entry {current} not found")))?;
        entry_id = row.parent_id.clone();
        path.push(row);
    }

    for row in path.into_iter().rev() {
        let custom_type = custom_type_from_payload(&row)?;
        insert_branch_entry(
            db,
            session_id,
            branch_id,
            &row.id,
            row.seq,
            &row.entry_type,
            custom_type.as_deref(),
        )
        .map_err(|error| SessionError::new(SessionErrorKind::Storage, format!("Failed to insert branch entry: {error}")))?;
    }
    Ok(())
}

pub fn read_branch_containing_entry(
    db: &Connection,
    session_id: &str,
    entry_id: &str,
) -> rusqlite::Result<Option<CachedBranch>> {
    let row = SqlQuery::new(
        "SELECT b.branch_id, b.entry_seq
        FROM branch_entries AS b
        WHERE b.session_id = ? AND b.entry_id = ?
        ORDER BY b.branch_id
        LIMIT 1",
    )
    .bind(session_id)
    .bind(entry_id)
    .get_row(db, |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?;
    Ok(row.map(|(branch_id, entry_seq)| CachedBranch { branch_id, leaf_seq: entry_seq }))
}

pub fn copy_branch_entries_through_seq(
    db: &Connection,
    session_id: &str,
    target_branch_id: &str,
    source_branch_id: &str,
    through_seq: i64,
) -> rusqlite::Result<()> {
    SqlQuery::new(
        "INSERT INTO branch_entries (session_id, branch_id, entry_id, entry_seq, entry_type, custom_type)
        SELECT session_id, ?, entry_id, entry_seq, entry_type, custom_type
        FROM branch_entries
        WHERE session_id = ? AND branch_id = ? AND entry_seq <= ?",
    )
    .bind(target_branch_id)
    .bind(session_id)
    .bind(source_branch_id)
    .bind(through_seq)
    .run(db)?;
    Ok(())
}
