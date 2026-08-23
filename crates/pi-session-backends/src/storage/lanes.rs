//! `lanes` / `lane_moves` table helpers — port of
//! `packages/session-backends/sqlite-node/src/sqlite/storage/lanes.ts`.

use pi_agent::session::types::{SessionError, SessionErrorKind};
use rusqlite::Connection;

use crate::sql::{empty, SqlQuery};

#[derive(Debug, Clone)]
pub struct LaneRow {
    pub session_id: String,
    pub lane: String,
    pub leaf_id: Option<String>,
    pub open_operation_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LaneMoveRow {
    pub session_id: String,
    pub seq: i64,
    pub lane: String,
    pub leaf_id: Option<String>,
}

pub fn map_lane_move_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LaneMoveRow> {
    Ok(LaneMoveRow {
        session_id: row.get(0)?,
        seq: row.get(1)?,
        lane: row.get(2)?,
        leaf_id: row.get(3)?,
    })
}

pub fn create_initial_lane(
    db: &Connection,
    session_id: &str,
    lane: &str,
    leaf_id: Option<&str>,
) -> rusqlite::Result<()> {
    SqlQuery::new(
        "INSERT INTO lanes (session_id, lane, leaf_id, open_operation_id)
        VALUES (?, ?, ?, NULL)",
    )
    .bind(session_id)
    .bind(lane)
    .bind(leaf_id)
    .run(db)?;
    Ok(())
}

pub fn read_lanes(db: &Connection, session_id: &str) -> Result<Vec<LaneRow>, SessionError> {
    let rows = SqlQuery::new(
        "SELECT
            l.session_id,
            l.lane,
            l.leaf_id,
            l.open_operation_id,
            (l.leaf_id IS NULL OR EXISTS (
                SELECT 1 FROM entries AS e WHERE e.session_id = l.session_id AND e.id = l.leaf_id
            )) AS leaf_exists
        FROM lanes AS l
        WHERE l.session_id = ?
        ORDER BY l.lane",
    )
    .bind(session_id)
    .all_rows(db, |row| {
        Ok((
            LaneRow {
                session_id: row.get(0)?,
                lane: row.get(1)?,
                leaf_id: row.get(2)?,
                open_operation_id: row.get(3)?,
            },
            row.get::<_, i64>(4)?,
        ))
    })
    .map_err(|error| {
        SessionError::new(
            SessionErrorKind::Storage,
            format!("Failed to read lanes: {error}"),
        )
    })?;
    let mut out = Vec::with_capacity(rows.len());
    for (row, leaf_exists) in rows {
        if leaf_exists == 0 {
            return Err(SessionError::new(
                SessionErrorKind::Storage,
                format!(
                    "Lane {} points at missing entry {}",
                    row.lane,
                    row.leaf_id.as_deref().unwrap_or("null")
                ),
            ));
        }
        out.push(row);
    }
    Ok(out)
}

pub fn read_lane(
    db: &Connection,
    session_id: &str,
    lane: &str,
) -> rusqlite::Result<Option<LaneRow>> {
    SqlQuery::new(
        "SELECT session_id, lane, leaf_id, open_operation_id
        FROM lanes
        WHERE session_id = ? AND lane = ?",
    )
    .bind(session_id)
    .bind(lane)
    .get_row(db, |row| {
        Ok(LaneRow {
            session_id: row.get(0)?,
            lane: row.get(1)?,
            leaf_id: row.get(2)?,
            open_operation_id: row.get(3)?,
        })
    })
}

/// `readLaneHead`: reads a lane's leaf, validating that a non-null leaf still
/// exists.
pub fn read_lane_head(
    db: &Connection,
    session_id: &str,
    lane: &str,
) -> Result<Option<String>, SessionError> {
    let row = SqlQuery::new(
        "SELECT
            l.leaf_id,
            (l.leaf_id IS NULL OR EXISTS (
                SELECT 1 FROM entries AS e WHERE e.session_id = l.session_id AND e.id = l.leaf_id
            )) AS leaf_exists
        FROM lanes AS l
        WHERE l.session_id = ? AND l.lane = ?",
    )
    .bind(session_id)
    .bind(lane)
    .get_row(db, |row| {
        Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?))
    })
    .map_err(|error| {
        SessionError::new(
            SessionErrorKind::Storage,
            format!("Failed to read lane head: {error}"),
        )
    })?;
    match row {
        None => Err(SessionError::new(
            SessionErrorKind::InvalidLane,
            format!("Lane not found: {lane}"),
        )),
        Some((leaf_id, leaf_exists)) => {
            if leaf_exists == 0 {
                Err(SessionError::new(
                    SessionErrorKind::Storage,
                    format!("Entry {} not found", leaf_id.as_deref().unwrap_or("null")),
                ))
            } else {
                Ok(leaf_id)
            }
        }
    }
}

pub fn create_lane(
    db: &Connection,
    session_id: &str,
    seq: i64,
    lane: &str,
    leaf_id: Option<&str>,
) -> rusqlite::Result<()> {
    SqlQuery::new(
        "INSERT INTO lanes (session_id, lane, leaf_id, open_operation_id)
        VALUES (?, ?, ?, NULL)",
    )
    .bind(session_id)
    .bind(lane)
    .bind(leaf_id)
    .run(db)?;
    append_lane_move(db, session_id, seq, lane, leaf_id)
}

/// `moveLane`: updates the lane and records a lane move at `seq`.
pub fn move_lane(
    db: &Connection,
    session_id: &str,
    seq: i64,
    lane: &str,
    leaf_id: Option<&str>,
) -> Result<(), SessionError> {
    let result = SqlQuery::new("UPDATE lanes SET leaf_id = ? WHERE session_id = ? AND lane = ?")
        .bind(leaf_id)
        .bind(session_id)
        .bind(lane)
        .run(db)
        .map(|r| r.changes)
        .map_err(|error| {
            SessionError::new(
                SessionErrorKind::Storage,
                format!("Failed to move lane: {error}"),
            )
        })?;
    if result != 1 {
        return Err(SessionError::new(
            SessionErrorKind::InvalidLane,
            format!("Lane not found: {lane}"),
        ));
    }
    append_lane_move(db, session_id, seq, lane, leaf_id).map_err(|error| {
        SessionError::new(
            SessionErrorKind::Storage,
            format!("Failed to record lane move: {error}"),
        )
    })
}

/// `setLaneLeaf`: updates only the lane row (used by entry appends). Does not
/// record a lane move.
pub fn set_lane_leaf(
    db: &Connection,
    session_id: &str,
    lane: &str,
    leaf_id: &str,
) -> Result<(), SessionError> {
    let result = SqlQuery::new("UPDATE lanes SET leaf_id = ? WHERE session_id = ? AND lane = ?")
        .bind(leaf_id)
        .bind(session_id)
        .bind(lane)
        .run(db)
        .map(|r| r.changes)
        .map_err(|error| {
            SessionError::new(
                SessionErrorKind::Storage,
                format!("Failed to set lane leaf: {error}"),
            )
        })?;
    if result != 1 {
        return Err(SessionError::new(
            SessionErrorKind::InvalidLane,
            format!("Lane not found: {lane}"),
        ));
    }
    Ok(())
}

pub fn start_lane_operation(
    db: &Connection,
    session_id: &str,
    lane: &str,
    run_id: &str,
) -> Result<(), SessionError> {
    let result = SqlQuery::new(
        "UPDATE lanes SET open_operation_id = ?
        WHERE session_id = ? AND lane = ? AND open_operation_id IS NULL",
    )
    .bind(run_id)
    .bind(session_id)
    .bind(lane)
    .run(db)
    .map(|r| r.changes)
    .map_err(|error| {
        SessionError::new(
            SessionErrorKind::Storage,
            format!("Failed to start lane operation: {error}"),
        )
    })?;
    if result == 1 {
        return Ok(());
    }
    let current = read_lane(db, session_id, lane).map_err(|error| {
        SessionError::new(
            SessionErrorKind::Storage,
            format!("Failed to read lane: {error}"),
        )
    })?;
    match current {
        None => Err(SessionError::new(
            SessionErrorKind::InvalidLane,
            format!("Lane not found: {lane}"),
        )),
        Some(current) => Err(SessionError::new(
            SessionErrorKind::Storage,
            format!(
                "Lane {lane} already has an open operation {}",
                current.open_operation_id.as_deref().unwrap_or("null")
            ),
        )),
    }
}

pub fn finish_lane_operation(
    db: &Connection,
    session_id: &str,
    lane: &str,
    run_id: Option<&str>,
) -> rusqlite::Result<()> {
    SqlQuery::new(
        "UPDATE lanes SET open_operation_id = NULL
        WHERE session_id = ? AND lane = ? AND open_operation_id = ?",
    )
    .bind(session_id)
    .bind(lane)
    .bind(run_id)
    .run(db)?;
    Ok(())
}

pub fn read_lane_move_rows(
    db: &Connection,
    session_id: &str,
    after_seq: Option<i64>,
    limit: Option<i64>,
) -> rusqlite::Result<Vec<LaneMoveRow>> {
    let mut query = SqlQuery::new(
        "SELECT session_id, seq, lane, leaf_id
        FROM lane_moves
        WHERE session_id = ?",
    )
    .bind(session_id);
    if let Some(after) = after_seq {
        query = query.inline(&SqlQuery::new(" AND seq > ?").bind(after));
    }
    query = query.inline(&SqlQuery::new(" ORDER BY seq"));
    if let Some(limit) = limit {
        query = query.inline(&SqlQuery::new(" LIMIT ?").bind(limit));
    }
    query.all_rows(db, map_lane_move_row)
}

pub fn delete_lane_rows(db: &Connection, session_id: &str) -> rusqlite::Result<()> {
    SqlQuery::new("DELETE FROM lane_moves WHERE session_id = ?")
        .bind(session_id)
        .run(db)?;
    SqlQuery::new("DELETE FROM lanes WHERE session_id = ?")
        .bind(session_id)
        .run(db)?;
    Ok(())
}

fn append_lane_move(
    db: &Connection,
    session_id: &str,
    seq: i64,
    lane: &str,
    leaf_id: Option<&str>,
) -> rusqlite::Result<()> {
    SqlQuery::new("INSERT INTO lane_moves (session_id, seq, lane, leaf_id) VALUES (?, ?, ?, ?)")
        .bind(session_id)
        .bind(seq)
        .bind(lane)
        .bind(leaf_id)
        .run(db)?;
    Ok(())
}

/// Empty fragment used to keep the port's SQL composition shape.
pub fn _empty_fragment() -> SqlQuery {
    empty()
}
