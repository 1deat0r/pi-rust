//! SQLite branch queries — port of `test/branch-query.test.ts`.

use pi_agent::session::state::{BranchBounds, EntryOrder, EntryQuery};
use pi_session_backends::repo::SqliteSessionRepository;
use pi_session_backends::types::{SqliteSessionCreateOptions, SqliteSessionMetadata, SqliteWriterLeaseOptions};

mod test_utils;
use test_utils::{assistant_message, create_temp_dir, user_message};

fn repo_for(root: &std::path::Path) -> SqliteSessionRepository {
    let database_path = root.join("sessions.sqlite").to_string_lossy().into_owned();
    SqliteSessionRepository::new(database_path, Some(SqliteWriterLeaseOptions { ttl_ms: 5000, heartbeat_interval_ms: 1000 }))
}

#[tokio::test]
async fn does_not_decode_entries_outside_bounded_branch_queries() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite");
    let repo = repo_for(&root);
    let mut session = repo.create(&SqliteSessionCreateOptions { id: Some("session-1".into()), cwd: root.to_string_lossy().into_owned(), ..Default::default() }).await.unwrap();
    let root_id = session.append_message(user_message("root")).await.unwrap();
    let middle_id = session.append_message(assistant_message("middle")).await.unwrap();
    let leaf_id = session.append_message(user_message("leaf")).await.unwrap();

    {
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute(
            "UPDATE entries SET payload = ? WHERE session_id = ? AND id = ?",
            rusqlite::params!["not json", "session-1", middle_id],
        )
        .unwrap();
        let branch = pi_session_backends::sql::SqlQuery::new("SELECT branch_id FROM branch_entries WHERE session_id = ? AND entry_id = ?")
            .bind("session-1")
            .bind(&leaf_id)
            .get_row(&db, |row| row.get::<_, String>(0))
            .unwrap()
            .expect("branch cache row");
        db.execute(
            "DELETE FROM branch_entries WHERE session_id = ? AND branch_id = ? AND entry_id = ?",
            rusqlite::params!["session-1", branch, middle_id],
        )
        .unwrap();
    }

    let ids = session
        .find_entries_on_branch(
            &EntryQuery::default(),
            Some(&leaf_id),
            &BranchBounds { stop_at_id: Some(leaf_id.clone()), ..Default::default() },
        )
        .await
        .unwrap();
    assert_eq!(ids.iter().map(|e| e.id().to_string()).collect::<Vec<_>>(), vec![leaf_id.clone()]);

    let ids = session
        .find_entries_on_branch(
            &EntryQuery { order: Some(EntryOrder::OldestFirst), limit: Some(1), ..Default::default() },
            Some(&leaf_id),
            &BranchBounds { stop_at_id: Some(root_id.clone()), ..Default::default() },
        )
        .await
        .unwrap();
    assert_eq!(ids.iter().map(|e| e.id().to_string()).collect::<Vec<_>>(), vec![root_id]);

    let err = session
        .find_entries_on_branch(
            &EntryQuery { limit: Some(2), ..Default::default() },
            Some(&leaf_id),
            &BranchBounds::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind, pi_agent::session::types::SessionErrorKind::InvalidEntry);
    assert!(err.message.contains(&format!("Entry {middle_id} not found")), "got: {err}");
}

#[tokio::test]
async fn does_not_decode_entries_excluded_by_branch_query_filters_and_limits() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite");
    let repo = repo_for(&root);
    let mut session = repo.create(&SqliteSessionCreateOptions { id: Some("session-1".into()), cwd: root.to_string_lossy().into_owned(), ..Default::default() }).await.unwrap();
    session.append_message(user_message("root")).await.unwrap();
    let custom_id = session.append_custom_entry("note", Some(serde_json::json!({ "value": 1 }))).await.unwrap();
    let leaf_id = session.append_message(assistant_message("leaf")).await.unwrap();

    {
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute(
            "UPDATE entries SET payload = ? WHERE session_id = ? AND id = ?",
            rusqlite::params!["{}", "session-1", custom_id],
        )
        .unwrap();
    }
    let ids = session
        .find_entries_on_branch(
            &EntryQuery { entry_type: Some("message".into()), limit: Some(1), ..Default::default() },
            Some(&leaf_id),
            &BranchBounds::default(),
        )
        .await
        .unwrap();
    assert_eq!(ids.iter().map(|e| e.id().to_string()).collect::<Vec<_>>(), vec![leaf_id.clone()]);

    {
        let invalid_json_db = rusqlite::Connection::open(&database_path).unwrap();
        invalid_json_db
            .execute(
                "UPDATE entries SET payload = ? WHERE session_id = ? AND id = ?",
                rusqlite::params!["not json", "session-1", custom_id],
            )
            .unwrap();
    }
    let ids = session
        .find_entries_on_branch(
            &EntryQuery { custom_type: Some("other".into()), ..Default::default() },
            Some(&leaf_id),
            &BranchBounds::default(),
        )
        .await
        .unwrap();
    assert!(ids.is_empty());
}

#[tokio::test]
async fn does_not_validate_ancestors_beyond_newest_first_stop_bounds() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite");
    let repo = repo_for(&root);
    let mut session = repo.create(&SqliteSessionCreateOptions { id: Some("session-1".into()), cwd: root.to_string_lossy().into_owned(), ..Default::default() }).await.unwrap();
    let root_id = session.append_message(user_message("root")).await.unwrap();
    let child_id = session.append_message(assistant_message("child")).await.unwrap();

    {
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute(
            "UPDATE entries SET parent_id = ? WHERE session_id = ? AND id = ?",
            rusqlite::params!["missing-parent", "session-1", child_id],
        )
        .unwrap();
    }
    let ids = session
        .find_entries_on_branch(
            &EntryQuery::default(),
            Some(&child_id),
            &BranchBounds { stop_at_id: Some(child_id.clone()), ..Default::default() },
        )
        .await
        .unwrap();
    assert_eq!(ids.iter().map(|e| e.id().to_string()).collect::<Vec<_>>(), vec![child_id.clone()]);
    let ids = session
        .find_entries_on_branch(
            &EntryQuery::default(),
            Some(&child_id),
            &BranchBounds { stop_at_type: Some("message".into()), ..Default::default() },
        )
        .await
        .unwrap();
    assert_eq!(ids.iter().map(|e| e.id().to_string()).collect::<Vec<_>>(), vec![child_id.clone()]);
    let err = session
        .find_entries_on_branch(&EntryQuery::default(), Some(&child_id), &BranchBounds::default())
        .await
        .unwrap_err();
    assert_eq!(err.kind, pi_agent::session::types::SessionErrorKind::InvalidEntry);
    assert!(err.message.contains("Entry missing-parent not found"), "got: {err}");

    {
        let cycle_db = rusqlite::Connection::open(&database_path).unwrap();
        cycle_db
            .execute(
                "UPDATE entries SET parent_id = ? WHERE session_id = ? AND id = ?",
                rusqlite::params![root_id, "session-1", child_id],
            )
            .unwrap();
        cycle_db
            .execute(
                "UPDATE entries SET parent_id = ? WHERE session_id = ? AND id = ?",
                rusqlite::params![child_id, "session-1", root_id],
            )
            .unwrap();
    }
    let ids = session
        .find_entries_on_branch(
            &EntryQuery::default(),
            Some(&child_id),
            &BranchBounds { stop_at_id: Some(child_id.clone()), ..Default::default() },
        )
        .await
        .unwrap();
    assert_eq!(ids.iter().map(|e| e.id().to_string()).collect::<Vec<_>>(), vec![child_id.clone()]);
    let ids = session
        .find_entries_on_branch(
            &EntryQuery::default(),
            Some(&child_id),
            &BranchBounds { stop_at_type: Some("message".into()), ..Default::default() },
        )
        .await
        .unwrap();
    assert_eq!(ids.iter().map(|e| e.id().to_string()).collect::<Vec<_>>(), vec![child_id.clone()]);
    let err = session
        .find_entries_on_branch(&EntryQuery::default(), Some(&child_id), &BranchBounds::default())
        .await
        .unwrap_err();
    assert_eq!(err.kind, pi_agent::session::types::SessionErrorKind::InvalidEntry);
    assert!(err.message.contains(&format!("Entry {child_id} not found")), "got: {err}");
}
