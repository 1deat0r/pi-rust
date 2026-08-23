//! `branch_tips` table helpers — port of
//! `packages/session-backends/sqlite-node/src/sqlite/storage/branch-tips.ts`.

use rusqlite::Connection;

use crate::sql::SqlQuery;

pub fn read_branch_tip_ids(db: &Connection, session_id: &str) -> rusqlite::Result<Vec<String>> {
    SqlQuery::new("SELECT tip_id FROM branch_tips WHERE session_id = ? ORDER BY tip_id")
        .bind(session_id)
        .all_rows(db, |row| row.get::<_, String>(0))
}

pub fn read_branch_tip_branch_id(
    db: &Connection,
    session_id: &str,
    tip_id: &str,
) -> rusqlite::Result<Option<String>> {
    let tip =
        SqlQuery::new("SELECT branch_id FROM branch_tips WHERE session_id = ? AND tip_id = ?")
            .bind(session_id)
            .bind(tip_id)
            .get_row(db, |row| row.get::<_, String>(0))?;
    Ok(tip)
}

pub fn insert_branch_tip(
    db: &Connection,
    session_id: &str,
    tip_id: &str,
    branch_id: &str,
) -> rusqlite::Result<()> {
    SqlQuery::new("INSERT INTO branch_tips (session_id, tip_id, branch_id) VALUES (?, ?, ?)")
        .bind(session_id)
        .bind(tip_id)
        .bind(branch_id)
        .run(db)?;
    Ok(())
}

pub fn update_branch_tip(
    db: &Connection,
    session_id: &str,
    branch_id: &str,
    old_tip_id: &str,
    new_tip_id: &str,
) -> rusqlite::Result<bool> {
    let result = SqlQuery::new(
        "UPDATE branch_tips SET tip_id = ?
        WHERE session_id = ? AND branch_id = ? AND tip_id = ?",
    )
    .bind(new_tip_id)
    .bind(session_id)
    .bind(branch_id)
    .bind(old_tip_id)
    .run(db)?;
    Ok(result.changes == 1)
}

pub fn delete_branch_tips(db: &Connection, session_id: &str) -> rusqlite::Result<()> {
    SqlQuery::new("DELETE FROM branch_tips WHERE session_id = ?")
        .bind(session_id)
        .run(db)?;
    Ok(())
}
