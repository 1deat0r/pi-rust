//! `writer_leases` table helpers — port of
//! `packages/session-backends/sqlite-node/src/sqlite/storage/writer-leases.ts`.

use rusqlite::Connection;

use crate::sql::SqlQuery;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterLease {
    pub owner_id: String,
    pub fence: i64,
    pub expires_at_ms: i64,
}

/// Tries to claim (or take over an expired) writer lease for a session.
/// Returns `None` when an unexpired lease is held by another owner.
pub fn acquire_writer_lease(
    db: &Connection,
    session_id: &str,
    owner_id: &str,
    now: i64,
    expires_at_ms: i64,
) -> rusqlite::Result<Option<WriterLease>> {
    let row = SqlQuery::new(
        "INSERT INTO writer_leases (session_id, owner_id, fence, expires_at_ms)
        VALUES (?, ?, 1, ?)
        ON CONFLICT(session_id) DO UPDATE SET
            owner_id = excluded.owner_id,
            fence = writer_leases.fence + 1,
            expires_at_ms = excluded.expires_at_ms
        WHERE writer_leases.expires_at_ms <= ?
        RETURNING owner_id, fence, expires_at_ms",
    )
    .bind(session_id)
    .bind(owner_id)
    .bind(expires_at_ms)
    .bind(now)
    .get_row(db, |row| {
        Ok(WriterLease {
            owner_id: row.get(0)?,
            fence: row.get(1)?,
            expires_at_ms: row.get(2)?,
        })
    })?;
    Ok(row)
}

pub fn renew_writer_lease(
    db: &Connection,
    session_id: &str,
    lease: &WriterLease,
    now: i64,
    expires_at_ms: i64,
) -> rusqlite::Result<bool> {
    let result = SqlQuery::new(
        "UPDATE writer_leases
        SET expires_at_ms = ?
        WHERE session_id = ?
            AND owner_id = ?
            AND fence = ?
            AND expires_at_ms > ?",
    )
    .bind(expires_at_ms)
    .bind(session_id)
    .bind(&lease.owner_id)
    .bind(lease.fence)
    .bind(now)
    .run(db)?;
    Ok(result.changes == 1)
}

pub fn release_writer_lease(
    db: &Connection,
    session_id: &str,
    lease: &WriterLease,
) -> rusqlite::Result<()> {
    SqlQuery::new(
        "DELETE FROM writer_leases
        WHERE session_id = ? AND owner_id = ? AND fence = ?",
    )
    .bind(session_id)
    .bind(&lease.owner_id)
    .bind(lease.fence)
    .run(db)?;
    Ok(())
}

pub fn delete_writer_lease(db: &Connection, session_id: &str) -> rusqlite::Result<()> {
    SqlQuery::new("DELETE FROM writer_leases WHERE session_id = ?")
        .bind(session_id)
        .run(db)?;
    Ok(())
}
