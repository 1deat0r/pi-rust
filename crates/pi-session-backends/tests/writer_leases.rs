//! SQLite session writer leases — port of `test/writer-leases.test.ts`.

use pi_session_backends::repo::SqliteSessionRepository;
use pi_session_backends::sql::SqlQuery;
use pi_session_backends::types::{
    SqliteSessionCreateOptions, SqliteSessionListOptions, SqliteSessionMetadata,
    SqliteWriterLeaseOptions,
};

mod test_utils;
use test_utils::{create_temp_dir, user_message};

fn create_repository(root: &std::path::Path) -> SqliteSessionRepository {
    let database_path = root.join("sessions.sqlite").to_string_lossy().into_owned();
    SqliteSessionRepository::new(database_path, None)
}

#[tokio::test]
async fn shares_one_write_queue_across_repeated_opens_in_one_repository() {
    let root = create_temp_dir();
    let repo = create_repository(&root);
    let mut session = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    let metadata = session.get_metadata().await.unwrap();
    let mut reopened = repo
        .open(&SqliteSessionMetadata::from_core(&metadata))
        .await
        .unwrap();

    let (first, second) = tokio::join!(
        session.append_message(user_message("first")),
        reopened.append_message(user_message("second")),
    );
    let first = first.unwrap();
    let second = second.unwrap();

    let entries = session
        .find_entries(&pi_agent::session::state::EntryQuery {
            order: Some(pi_agent::session::state::EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        entries
            .iter()
            .map(|e| e.id().to_string())
            .collect::<Vec<_>>(),
        vec![first, second]
    );
}

#[test]
#[should_panic(expected = "writerLease.ttlMs must be positive")]
fn rejects_zero_ttl() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite").to_string_lossy().into_owned();
    SqliteSessionRepository::new(
        database_path,
        Some(SqliteWriterLeaseOptions {
            ttl_ms: 0,
            heartbeat_interval_ms: 1,
        }),
    );
}

#[test]
#[should_panic(expected = "writerLease.heartbeatIntervalMs must be positive and less than ttlMs")]
fn rejects_non_positive_heartbeat() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite").to_string_lossy().into_owned();
    SqliteSessionRepository::new(
        database_path,
        Some(SqliteWriterLeaseOptions {
            ttl_ms: 100,
            heartbeat_interval_ms: 100,
        }),
    );
}

#[tokio::test]
async fn lists_complete_metadata_without_acquiring_active_sessions_writer_leases() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite").to_string_lossy().into_owned();
    let writer_repo = SqliteSessionRepository::new(database_path.clone(), None);
    let reader_repo = SqliteSessionRepository::new(database_path.clone(), None);

    let mut first = writer_repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            metadata: Some(serde_json::json!({ "profile": "reviewer" })),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut second = writer_repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-2".into()),
            cwd: root.to_string_lossy().into_owned(),
            parent_session_id: Some("session-1".into()),
            metadata: Some(
                serde_json::json!({ "profile": "writer", "name": "application-owned name" }),
            ),
        })
        .await
        .unwrap();
    first.set_name(Some("Review session")).await.unwrap();
    second.set_name(Some("Write session")).await.unwrap();
    let (first_metadata, second_metadata) =
        tokio::join!(first.get_sqlite_metadata(), second.get_sqlite_metadata());
    let first_metadata = first_metadata.unwrap();
    let second_metadata = second_metadata.unwrap();
    assert_eq!(first_metadata.name, Some("Review session".to_string()));
    assert_eq!(
        first_metadata.metadata,
        Some(serde_json::json!({ "profile": "reviewer" }))
    );
    assert_eq!(second_metadata.name, Some("Write session".to_string()));
    assert_eq!(
        second_metadata.metadata,
        Some(serde_json::json!({ "profile": "writer", "name": "application-owned name" }))
    );

    let inspection = rusqlite::Connection::open(&database_path).unwrap();
    let leases_before = SqlQuery::new(
        "SELECT session_id, owner_id, fence, expires_at_ms FROM writer_leases ORDER BY session_id",
    )
    .all_rows(&inspection, |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })
    .unwrap();

    let mut listed = reader_repo
        .list(&SqliteSessionListOptions {
            cwd: Some(root.to_string_lossy().into_owned()),
        })
        .await
        .unwrap();
    listed.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(listed.len(), 2);

    let after_list = rusqlite::Connection::open(&database_path).unwrap();
    let leases_after = SqlQuery::new(
        "SELECT session_id, owner_id, fence, expires_at_ms FROM writer_leases ORDER BY session_id",
    )
    .all_rows(&after_list, |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })
    .unwrap();
    assert_eq!(leases_after, leases_before);

    let err = reader_repo.open(&first_metadata).await.unwrap_err();
    assert_eq!(
        err.kind,
        pi_agent::session::types::SessionErrorKind::Storage
    );
    assert!(
        err.message.contains("already has an active writer"),
        "got: {err}"
    );
}

#[tokio::test]
async fn rejects_a_second_writer_until_the_first_session_releases_its_claim() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite").to_string_lossy().into_owned();
    let first_repo = SqliteSessionRepository::new(database_path.clone(), None);
    let second_repo = SqliteSessionRepository::new(database_path.clone(), None);

    let first = first_repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    let metadata = first.get_metadata().await.unwrap();

    let err = second_repo
        .open(&SqliteSessionMetadata::from_core(&metadata))
        .await
        .unwrap_err();
    assert!(
        err.message.contains("already has an active writer"),
        "got: {err}"
    );

    first_repo.close().await;

    let mut second = second_repo
        .open(&SqliteSessionMetadata::from_core(&metadata))
        .await
        .unwrap();
    let id = second
        .append_message(user_message("new owner"))
        .await
        .unwrap();
    assert!(!id.is_empty());
}

#[tokio::test]
async fn fences_a_stale_owner_after_an_expired_lease_is_acquired_by_another_writer() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite").to_string_lossy().into_owned();
    let lease = SqliteWriterLeaseOptions {
        ttl_ms: 120_000,
        heartbeat_interval_ms: 60_000,
    };
    let first_repo = SqliteSessionRepository::new(database_path.clone(), Some(lease));
    let second_repo = SqliteSessionRepository::new(database_path.clone(), Some(lease));

    let mut first = first_repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    let metadata = first.get_metadata().await.unwrap();

    {
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute(
            "UPDATE writer_leases SET expires_at_ms = 0 WHERE session_id = ?",
            [metadata.id.clone()],
        )
        .unwrap();
    }

    let mut second = second_repo
        .open(&SqliteSessionMetadata::from_core(&metadata))
        .await
        .unwrap();

    let err = first
        .append_message(user_message("stale owner"))
        .await
        .unwrap_err();
    assert_eq!(
        err.kind,
        pi_agent::session::types::SessionErrorKind::Storage
    );
    assert!(err.message.contains("writer lease was lost"), "got: {err}");
    assert!(second
        .find_entries(&Default::default())
        .await
        .unwrap()
        .is_empty());

    let inspection = rusqlite::Connection::open(&database_path).unwrap();
    let current_lease =
        SqlQuery::new("SELECT owner_id, fence FROM writer_leases WHERE session_id = ?")
            .bind(metadata.id.as_str())
            .get_row(&inspection, |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .unwrap();
    assert_eq!(current_lease.as_ref().unwrap().1, 2);

    first_repo.close().await;

    let after_stale_close = rusqlite::Connection::open(&database_path).unwrap();
    let lease_after =
        SqlQuery::new("SELECT owner_id, fence FROM writer_leases WHERE session_id = ?")
            .bind(metadata.id.as_str())
            .get_row(&after_stale_close, |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .unwrap();
    assert_eq!(lease_after.as_ref().unwrap().1, 2);

    let id = second
        .append_message(user_message("current owner"))
        .await
        .unwrap();
    assert!(!id.is_empty());
}

#[tokio::test]
async fn serializes_lease_checked_writes_for_sessions_sharing_one_database_connection() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite").to_string_lossy().into_owned();
    let repo = SqliteSessionRepository::new(database_path, None);
    let mut first = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut second = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-2".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();

    let (a, b) = tokio::join!(
        first.append_message(user_message("first")),
        second.append_message(user_message("second")),
    );
    a.unwrap();
    b.unwrap();
}

#[tokio::test]
async fn renews_an_idle_writer_lease_with_a_heartbeat() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite").to_string_lossy().into_owned();
    let lease = SqliteWriterLeaseOptions {
        ttl_ms: 500,
        heartbeat_interval_ms: 100,
    };
    let repo = SqliteSessionRepository::new(database_path.clone(), Some(lease));
    let session = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    let metadata = session.get_metadata().await.unwrap();

    let read_expiry = |db_path: &str, session_id: &str| -> Option<i64> {
        let db = rusqlite::Connection::open(db_path).unwrap();
        SqlQuery::new("SELECT expires_at_ms FROM writer_leases WHERE session_id = ?")
            .bind(session_id)
            .get_row(&db, |row| row.get::<_, i64>(0))
            .unwrap()
    };

    let initial = read_expiry(&database_path, &metadata.id).unwrap();

    // Wait for at least two heartbeat ticks (100ms each).
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    let after = read_expiry(&database_path, &metadata.id).unwrap();
    assert!(
        after > initial,
        "expected expiry to renew: initial {initial}, after {after}"
    );

    repo.close().await;
}
