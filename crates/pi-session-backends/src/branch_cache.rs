//! Derived branch cache maintenance — port of
//! `packages/session-backends/sqlite-node/src/sqlite/branch-cache.ts`.

use pi_agent::session::types::{SessionError, SessionErrorKind};
use rusqlite::Connection;

use crate::sql::SqlQuery;
use crate::storage::branch_entries::{
    copy_branch_entries_through_seq, delete_branch_entries, insert_branch_entries_for_path,
    insert_branch_entry, read_branch_containing_entry,
};
use crate::storage::branch_tips::{
    delete_branch_tips, insert_branch_tip, read_branch_tip_branch_id, update_branch_tip,
};

pub fn delete_branch_cache(db: &Connection, session_id: &str) -> rusqlite::Result<()> {
    delete_branch_tips(db, session_id)?;
    delete_branch_entries(db, session_id)
}

pub fn rebuild_branch_cache(db: &Connection, session_id: &str) -> Result<(), SessionError> {
    let tips = SqlQuery::new(
        "SELECT leaf.id
        FROM entries AS leaf
        WHERE leaf.session_id = ?
            AND NOT EXISTS (
                SELECT 1 FROM entries AS child WHERE child.session_id = leaf.session_id AND child.parent_id = leaf.id
            )
        ORDER BY leaf.seq",
    )
    .bind(session_id)
    .all_rows(db, |row| row.get::<_, String>(0))
    .map_err(|error| SessionError::new(SessionErrorKind::Storage, format!("Failed to list tips: {error}")))?;
    delete_branch_cache(db, session_id).map_err(|error| {
        SessionError::new(
            SessionErrorKind::Storage,
            format!("Failed to clear branch cache: {error}"),
        )
    })?;
    for tip in tips {
        build_cached_branch(db, session_id, &tip)?;
    }
    Ok(())
}

pub fn build_cached_branch(
    db: &Connection,
    session_id: &str,
    leaf_id: &str,
) -> Result<(), SessionError> {
    db.execute_batch("SAVEPOINT build_branch_cache")
        .map_err(|error| {
            SessionError::new(
                SessionErrorKind::Storage,
                format!("Failed to begin branch cache build: {error}"),
            )
        })?;
    let result = (|| -> Result<(), SessionError> {
        let branch_id = crate::new_id();
        insert_branch_entries_for_path(db, session_id, &branch_id, leaf_id)?;
        insert_branch_tip(db, session_id, leaf_id, &branch_id).map_err(|error| {
            SessionError::new(
                SessionErrorKind::Storage,
                format!("Failed to insert branch tip: {error}"),
            )
        })?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            db.execute_batch("RELEASE SAVEPOINT build_branch_cache")
                .map_err(|error| {
                    SessionError::new(
                        SessionErrorKind::Storage,
                        format!("Failed to release branch cache savepoint: {error}"),
                    )
                })?;
            Ok(())
        }
        Err(error) => {
            // Roll back to the savepoint and rethrow the original failure.
            let _ = db.execute_batch("ROLLBACK TO SAVEPOINT build_branch_cache");
            let _ = db.execute_batch("RELEASE SAVEPOINT build_branch_cache");
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn extend_branch(
    db: &Connection,
    session_id: &str,
    branch_id: &str,
    parent_id: &str,
    entry_id: &str,
    entry_seq: i64,
    entry_type: &str,
    custom_type: Option<&str>,
) -> Result<(), SessionError> {
    insert_branch_entry(
        db,
        session_id,
        branch_id,
        entry_id,
        entry_seq,
        entry_type,
        custom_type,
    )
    .map_err(|error| {
        SessionError::new(
            SessionErrorKind::Storage,
            format!("Failed to extend branch: {error}"),
        )
    })?;
    if !update_branch_tip(db, session_id, branch_id, parent_id, entry_id).map_err(|error| {
        SessionError::new(
            SessionErrorKind::Storage,
            format!("Failed to update branch tip: {error}"),
        )
    })? {
        return Err(SessionError::new(
            SessionErrorKind::InvalidEntry,
            format!("Branch tip {parent_id} changed during append"),
        ));
    }
    Ok(())
}

/// Appends an entry to the derived per-branch cache (or starts a new branch).
pub fn append_entry_to_branch_cache(
    db: &Connection,
    session_id: &str,
    entry_id: &str,
    entry_seq: i64,
    entry_type: &str,
    custom_type: Option<&str>,
    parent_id: Option<&str>,
) -> Result<(), SessionError> {
    let Some(parent_id) = parent_id else {
        let branch_id = crate::new_id();
        insert_branch_entry(
            db,
            session_id,
            &branch_id,
            entry_id,
            entry_seq,
            entry_type,
            custom_type,
        )
        .map_err(|error| {
            SessionError::new(
                SessionErrorKind::Storage,
                format!("Failed to insert branch entry: {error}"),
            )
        })?;
        insert_branch_tip(db, session_id, entry_id, &branch_id).map_err(|error| {
            SessionError::new(
                SessionErrorKind::Storage,
                format!("Failed to insert branch tip: {error}"),
            )
        })?;
        return Ok(());
    };

    let tip_branch_id = read_branch_tip_branch_id(db, session_id, parent_id).map_err(|error| {
        SessionError::new(
            SessionErrorKind::Storage,
            format!("Failed to read branch tip: {error}"),
        )
    })?;
    if let Some(tip_branch_id) = tip_branch_id {
        return extend_branch(
            db,
            session_id,
            &tip_branch_id,
            parent_id,
            entry_id,
            entry_seq,
            entry_type,
            custom_type,
        );
    }

    let source = read_branch_containing_entry(db, session_id, parent_id).map_err(|error| {
        SessionError::new(
            SessionErrorKind::Storage,
            format!("Failed to read branch membership: {error}"),
        )
    })?;
    let Some(source) = source else {
        return Err(SessionError::new(
            SessionErrorKind::InvalidEntry,
            format!("Branch cache has no branch containing parent entry {parent_id}"),
        ));
    };

    let branch_id = crate::new_id();
    copy_branch_entries_through_seq(
        db,
        session_id,
        &branch_id,
        &source.branch_id,
        source.leaf_seq,
    )
    .map_err(|error| {
        SessionError::new(
            SessionErrorKind::Storage,
            format!("Failed to copy branch entries: {error}"),
        )
    })?;
    insert_branch_entry(
        db,
        session_id,
        &branch_id,
        entry_id,
        entry_seq,
        entry_type,
        custom_type,
    )
    .map_err(|error| {
        SessionError::new(
            SessionErrorKind::Storage,
            format!("Failed to insert branch entry: {error}"),
        )
    })?;
    insert_branch_tip(db, session_id, entry_id, &branch_id).map_err(|error| {
        SessionError::new(
            SessionErrorKind::Storage,
            format!("Failed to insert branch tip: {error}"),
        )
    })?;
    Ok(())
}
