//! SQLite branch cache — port of `test/branch-cache.test.ts`.

use pi_agent::session::state::{EntryOrder, EntryQuery, ForkPosition};
use pi_agent::session::types::SessionErrorKind;
use pi_session_backends::repo::{ForkCreateOptions, SqliteSessionRepository};
use pi_session_backends::sql::SqlQuery;
use pi_session_backends::types::{SqliteSessionCreateOptions, SqliteWriterLeaseOptions};

mod test_utils;
use test_utils::{
    append_sqlite_compaction, assistant_message, create_temp_dir, get_sqlite_branch,
    move_sqlite_main_lane, user_message,
};

fn repo_for(root: &std::path::Path) -> (SqliteSessionRepository, std::path::PathBuf) {
    let database_path = root.join("sessions.sqlite");
    (
        SqliteSessionRepository::new(
            database_path.to_string_lossy().into_owned(),
            Some(SqliteWriterLeaseOptions {
                ttl_ms: 5000,
                heartbeat_interval_ms: 1000,
            }),
        ),
        database_path,
    )
}

#[tokio::test]
async fn collects_complete_root_paths_for_branches_created_after_compaction() {
    let root = create_temp_dir();
    let (repo, database_path) = repo_for(&root);
    let mut session = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    let root_id = session.append_message(user_message("root")).await.unwrap();
    let kept_id = session.append_message(user_message("kept")).await.unwrap();
    let compaction_id = append_sqlite_compaction(&mut session, "summary", 100, None, None, vec![])
        .await
        .unwrap();
    session
        .append_message(assistant_message("first child"))
        .await
        .unwrap();
    move_sqlite_main_lane(&mut session, Some(&compaction_id), None)
        .await
        .unwrap();
    let branched_id = session
        .append_message(assistant_message("branched child"))
        .await
        .unwrap();

    let db = rusqlite::Connection::open(&database_path).unwrap();
    let row =
        SqlQuery::new("SELECT branch_id FROM branch_entries WHERE session_id = ? AND entry_id = ?")
            .bind("session-1")
            .bind(&branched_id)
            .get_row(&db, |row| row.get::<_, String>(0))
            .unwrap()
            .expect("cached branch row");
    let entries = SqlQuery::new("SELECT entry_id FROM branch_entries WHERE session_id = ? AND branch_id = ? ORDER BY entry_seq")
        .bind("session-1")
        .bind(&row)
        .all_rows(&db, |r| r.get::<_, String>(0))
        .unwrap();
    assert_eq!(entries, vec![root_id, kept_id, compaction_id, branched_id]);
}

#[tokio::test]
async fn reads_only_the_compacted_branch_window_from_the_complete_cache() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite");
    let repo = SqliteSessionRepository::new(
        database_path.to_string_lossy().into_owned(),
        Some(SqliteWriterLeaseOptions {
            ttl_ms: 5000,
            heartbeat_interval_ms: 1000,
        }),
    );
    let mut session = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    let old_id = session.append_message(user_message("old")).await.unwrap();
    session.append_message(user_message("kept")).await.unwrap();
    let compaction_id = append_sqlite_compaction(&mut session, "summary", 100, None, None, vec![])
        .await
        .unwrap();
    let leaf_id = session
        .append_message(assistant_message("new"))
        .await
        .unwrap();

    {
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute(
            "UPDATE entries SET payload = ? WHERE session_id = ? AND id = ?",
            rusqlite::params!["not json", "session-1", old_id],
        )
        .unwrap();
    }

    let branch = get_sqlite_branch(&session).await.unwrap();
    assert_eq!(
        branch
            .iter()
            .map(|e| e.id().to_string())
            .collect::<Vec<_>>(),
        vec![compaction_id.clone(), leaf_id.clone()]
    );
}

#[tokio::test]
async fn preserves_nested_compaction_boundaries_when_reading_the_cache() {
    let root = create_temp_dir();
    let (repo, _database_path) = repo_for(&root);
    let mut session = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    session.append_message(user_message("root")).await.unwrap();
    append_sqlite_compaction(&mut session, "first summary", 100, None, None, vec![])
        .await
        .unwrap();
    session
        .append_message(user_message("middle"))
        .await
        .unwrap();
    let second_compaction_id =
        append_sqlite_compaction(&mut session, "second summary", 200, None, None, vec![])
            .await
            .unwrap();
    let leaf_id = session
        .append_message(assistant_message("new"))
        .await
        .unwrap();

    let branch = get_sqlite_branch(&session).await.unwrap();
    assert_eq!(
        branch
            .iter()
            .map(|e| e.id().to_string())
            .collect::<Vec<_>>(),
        vec![second_compaction_id, leaf_id]
    );
}

#[tokio::test]
async fn rejects_reads_and_writes_without_repairing_a_missing_private_branch_cache() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite");
    let repo = SqliteSessionRepository::new(
        database_path.to_string_lossy().into_owned(),
        Some(SqliteWriterLeaseOptions {
            ttl_ms: 5000,
            heartbeat_interval_ms: 1000,
        }),
    );
    let mut session = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    session.append_message(user_message("root")).await.unwrap();
    session
        .append_message(assistant_message("child"))
        .await
        .unwrap();

    {
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute_batch("DELETE FROM branch_tips WHERE session_id = 'session-1'")
            .unwrap();
        db.execute_batch("DELETE FROM branch_entries WHERE session_id = 'session-1'")
            .unwrap();
    }

    let err = get_sqlite_branch(&session).await.unwrap_err();
    assert_eq!(err.kind, SessionErrorKind::InvalidEntry, "got: {err}");
    let err = session
        .append_message(assistant_message("later"))
        .await
        .unwrap_err();
    assert_eq!(err.kind, SessionErrorKind::InvalidEntry);
    assert!(
        err.message
            .contains("has no branch containing parent entry"),
        "got: {err}"
    );

    {
        let inspection = rusqlite::Connection::open(&database_path).unwrap();
        let rows = SqlQuery::new("SELECT entry_id FROM branch_entries WHERE session_id = ?")
            .bind("session-1")
            .all_rows(&inspection, |row| row.get::<_, String>(0))
            .unwrap();
        assert!(rows.is_empty());
    }
}

#[tokio::test]
async fn repairs_the_private_branch_cache_explicitly() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite");
    let repo = SqliteSessionRepository::new(
        database_path.to_string_lossy().into_owned(),
        Some(SqliteWriterLeaseOptions {
            ttl_ms: 5000,
            heartbeat_interval_ms: 1000,
        }),
    );
    let mut session = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    let root_id = session.append_message(user_message("root")).await.unwrap();
    let child_id = session
        .append_message(assistant_message("child"))
        .await
        .unwrap();
    let metadata = session.get_sqlite_metadata().await.unwrap();

    {
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute_batch("DELETE FROM branch_tips WHERE session_id = 'session-1'")
            .unwrap();
        db.execute_batch("DELETE FROM branch_entries WHERE session_id = 'session-1'")
            .unwrap();
    }

    let err = get_sqlite_branch(&session).await.unwrap_err();
    assert_eq!(err.kind, SessionErrorKind::InvalidEntry, "got: {err}");

    repo.repair_branch_cache(&metadata).await.unwrap();

    let branch = get_sqlite_branch(&session).await.unwrap();
    assert_eq!(
        branch
            .iter()
            .map(|e| e.id().to_string())
            .collect::<Vec<_>>(),
        vec![root_id, child_id]
    );
}

#[tokio::test]
async fn fails_when_forking_from_a_source_with_a_missing_branch_cache() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite");
    let repo = SqliteSessionRepository::new(
        database_path.to_string_lossy().into_owned(),
        Some(SqliteWriterLeaseOptions {
            ttl_ms: 5000,
            heartbeat_interval_ms: 1000,
        }),
    );
    let mut source = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("source".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    let root_id = source.append_message(user_message("root")).await.unwrap();
    let child_id = source
        .append_message(assistant_message("child"))
        .await
        .unwrap();

    {
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute_batch("DELETE FROM branch_tips WHERE session_id = 'source'")
            .unwrap();
        db.execute_batch("DELETE FROM branch_entries WHERE session_id = 'source'")
            .unwrap();
    }

    assert_ne!(root_id, child_id);
    let metadata = source.get_sqlite_metadata().await.unwrap();
    let err = repo
        .fork(
            &metadata,
            &ForkCreateOptions {
                id: Some("fork".into()),
                cwd: root.to_string_lossy().into_owned(),
                fork_options: pi_agent::session::state::ForkOptions::Branch {
                    entry_id: Some(child_id.clone()),
                    position: Some(ForkPosition::At),
                },
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind, SessionErrorKind::InvalidForkTarget, "got: {err}");
}

#[tokio::test]
async fn fails_when_the_private_branch_cache_is_stale() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite");
    let repo = SqliteSessionRepository::new(
        database_path.to_string_lossy().into_owned(),
        Some(SqliteWriterLeaseOptions {
            ttl_ms: 5000,
            heartbeat_interval_ms: 1000,
        }),
    );
    let mut session = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    let root_id = session.append_message(user_message("root")).await.unwrap();
    let stale_id = session
        .append_message(assistant_message("stale"))
        .await
        .unwrap();
    let leaf_id = session.append_message(user_message("leaf")).await.unwrap();

    {
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute(
            "UPDATE entries SET parent_id = ? WHERE session_id = ? AND id = ?",
            rusqlite::params![root_id, "session-1", leaf_id],
        )
        .unwrap();
    }

    assert_ne!(stale_id, leaf_id);
    let err = session
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            Some(&leaf_id),
            &Default::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind, SessionErrorKind::InvalidEntry, "got: {err}");
}

#[tokio::test]
async fn deletes_branch_entries_and_tips_with_the_session() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite");
    let repo = SqliteSessionRepository::new(
        database_path.to_string_lossy().into_owned(),
        Some(SqliteWriterLeaseOptions {
            ttl_ms: 5000,
            heartbeat_interval_ms: 1000,
        }),
    );
    let session = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    let metadata = session.get_sqlite_metadata().await.unwrap();

    repo.delete(&metadata).await.unwrap();

    let db = rusqlite::Connection::open(&database_path).unwrap();
    let entries = SqlQuery::new("SELECT entry_id FROM branch_entries WHERE session_id = ?")
        .bind("session-1")
        .all_rows(&db, |row| row.get::<_, String>(0))
        .unwrap();
    assert!(entries.is_empty());
    let tips = SqlQuery::new("SELECT tip_id FROM branch_tips WHERE session_id = ?")
        .bind("session-1")
        .all_rows(&db, |row| row.get::<_, String>(0))
        .unwrap();
    assert!(tips.is_empty());
}
