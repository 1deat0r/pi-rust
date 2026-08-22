//! SQLite FTS5 session search — port of `test/search.test.ts`.

use pi_session_backends::repo::SqliteSessionRepository;
use pi_session_backends::search::{create_sqlite_session_search, SearchOptions};
use pi_session_backends::sql::SqlQuery;
use pi_session_backends::types::{SqliteSessionCreateOptions, SqliteWriterLeaseOptions};

mod test_utils;
use test_utils::{create_temp_dir, get_sqlite_entries, user_message};

fn fixture(root: &std::path::Path) -> (SqliteSessionRepository, pi_session_backends::search::SqliteSessionSearch) {
    let database_path = root.join("sessions.sqlite").to_string_lossy().into_owned();
    let repo = SqliteSessionRepository::new(
        database_path.clone(),
        Some(SqliteWriterLeaseOptions { ttl_ms: 5000, heartbeat_interval_ms: 1000 }),
    );
    let search = create_sqlite_session_search(database_path);
    (repo, search)
}

fn search_all(search: &pi_session_backends::search::SqliteSessionSearch, text: &str) -> Vec<pi_session_backends::search::SqliteSessionSearchHit> {
    search.search(text, &SearchOptions::default()).unwrap()
}

#[tokio::test]
async fn matches_trigrams() {
    let root = create_temp_dir();
    let (repo, search) = fixture(&root);
    let mut included = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("included".into()),
            cwd: root.to_string_lossy().into_owned(),
            metadata: Some(serde_json::json!({ "name": "application-owned" })),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut excluded = repo
        .create(&SqliteSessionCreateOptions { id: Some("excluded".into()), cwd: root.join("other").to_string_lossy().into_owned(), ..Default::default() })
        .await
        .unwrap();
    let entry_id = included.append_message(user_message("Find the auth defect")).await.unwrap();
    included.set_name(Some("Canonical name")).await.unwrap();
    let excluded_entry_id = excluded.append_message(user_message("Find the auth defect")).await.unwrap();

    let auth_hits = search_all(&search, "auth");
    assert_eq!(auth_hits.len(), 2);
    let included_hit = auth_hits.iter().find(|h| h.session_id == "included").expect("included hit");
    assert_eq!(included_hit.entry_id, entry_id);
    assert!(included_hit.timestamp > 0);
    assert_eq!(included_hit.metadata.name.as_deref(), Some("Canonical name"));
    assert_eq!(included_hit.metadata.metadata, Some(serde_json::json!({ "name": "application-owned" })));
    let excluded_hit = auth_hits.iter().find(|h| h.session_id == "excluded").expect("excluded hit");
    assert_eq!(excluded_hit.entry_id, excluded_entry_id);

    let uth_hits = search_all(&search, "uth");
    assert!(uth_hits.iter().any(|h| h.session_id == "included"));
    assert!(uth_hits.iter().any(|h| h.session_id == "excluded"));
}

#[tokio::test]
async fn omits_a_cleared_session_name_from_search_metadata() {
    let root = create_temp_dir();
    let (repo, search) = fixture(&root);
    let mut session = repo
        .create(&SqliteSessionCreateOptions { id: Some("session-1".into()), cwd: root.to_string_lossy().into_owned(), ..Default::default() })
        .await
        .unwrap();
    let entry_id = session.append_message(user_message("Find the auth defect")).await.unwrap();
    session.set_name(Some("Temporary")).await.unwrap();
    session.set_name(None).await.unwrap();

    let hits = search_all(&search, "auth");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].session_id, "session-1");
    assert_eq!(hits[0].entry_id, entry_id);
    assert!(hits[0].metadata.name.is_none());
}

#[tokio::test]
async fn handles_quoted_search_text_without_exposing_fts_syntax() {
    let root = create_temp_dir();
    let (_repo, search) = fixture(&root);
    let hits = search_all(&search, "missing \"phrase\"");
    assert!(hits.is_empty());
}

#[tokio::test]
async fn rebuilds_existing_entries_when_fts_is_first_initialized() {
    let root = create_temp_dir();
    let (repo, search) = fixture(&root);
    let mut session = repo
        .create(&SqliteSessionCreateOptions { id: Some("session-1".into()), cwd: root.to_string_lossy().into_owned(), ..Default::default() })
        .await
        .unwrap();
    session.append_message(user_message("Find the auth defect")).await.unwrap();
    assert_eq!(search_all(&search, "auth").len(), 1);
}

#[tokio::test]
async fn indexes_and_removes_session_entries_through_triggers_after_fts_initialization() {
    let root = create_temp_dir();
    let (repo, search) = fixture(&root);
    assert!(search_all(&search, "auth").is_empty());
    let mut session = repo
        .create(&SqliteSessionCreateOptions { id: Some("session-1".into()), cwd: root.to_string_lossy().into_owned(), ..Default::default() })
        .await
        .unwrap();
    session.append_message(user_message("Find the auth defect")).await.unwrap();
    assert_eq!(search_all(&search, "auth").len(), 1);

    let metadata = session.get_sqlite_metadata().await.unwrap();
    repo.delete(&metadata).await.unwrap();
    assert!(search_all(&search, "auth").is_empty());
}

#[tokio::test]
async fn removes_deleted_entries_from_fts_through_triggers() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite");
    let (repo, search) = fixture(&root);
    let mut session = repo
        .create(&SqliteSessionCreateOptions { id: Some("session-1".into()), cwd: root.to_string_lossy().into_owned(), ..Default::default() })
        .await
        .unwrap();
    let entry_id = session.append_message(user_message("Find the auth defect")).await.unwrap();
    assert_eq!(search_all(&search, "auth").len(), 1);

    {
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute(
            "DELETE FROM entries WHERE session_id = ? AND id = ?",
            rusqlite::params!["session-1", entry_id],
        )
        .unwrap();
    }
    assert!(search_all(&search, "auth").is_empty());
}

#[tokio::test]
async fn does_not_initialize_fts_for_canonical_writes_or_blank_searches() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite");
    let (repo, search) = fixture(&root);
    assert!(search_all(&search, "  ").is_empty());
    let mut session = repo
        .create(&SqliteSessionCreateOptions { id: Some("session-1".into()), cwd: root.to_string_lossy().into_owned(), ..Default::default() })
        .await
        .unwrap();

    {
        let db = rusqlite::Connection::open(&database_path).unwrap();
        let found = SqlQuery::new("SELECT 1 AS found FROM sqlite_master WHERE type = 'table' AND name = 'session_search_fts'")
            .get_row(&db, |_row| Ok(()))
            .unwrap();
        assert!(found.is_none());
    }
    session.append_message(user_message("still writable")).await.unwrap();
}

#[tokio::test]
async fn rolls_back_canonical_appends_when_colocated_fts_trigger_writes_fail() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite");
    let (repo, search) = fixture(&root);
    search_all(&search, "initialize");
    let mut session = repo
        .create(&SqliteSessionCreateOptions { id: Some("session-1".into()), cwd: root.to_string_lossy().into_owned(), ..Default::default() })
        .await
        .unwrap();

    {
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute_batch("DROP TABLE session_search_fts").unwrap();
    }

    let err = session.append_message(user_message("must roll back")).await.unwrap_err();
    assert!(!err.message.is_empty());
    let entries = get_sqlite_entries(&session, None, None).await.unwrap();
    assert!(entries.is_empty());
}

#[tokio::test]
async fn rolls_back_canonical_deletion_when_colocated_fts_cleanup_fails() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite");
    let (repo, search) = fixture(&root);
    search_all(&search, "initialize");
    let mut session = repo
        .create(&SqliteSessionCreateOptions { id: Some("session-1".into()), cwd: root.to_string_lossy().into_owned(), ..Default::default() })
        .await
        .unwrap();
    session.append_message(user_message("must remain")).await.unwrap();
    let metadata = session.get_sqlite_metadata().await.unwrap();

    {
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute_batch("DROP TABLE session_search_fts").unwrap();
    }

    let err = repo.delete(&metadata).await.unwrap_err();
    assert!(!err.message.is_empty());
    let reopened = repo.open(&metadata).await.unwrap();
    let entries = get_sqlite_entries(&reopened, None, None).await.unwrap();
    assert_eq!(entries.len(), 1);
}

#[tokio::test]
async fn initializes_canonical_storage_when_searched_before_the_first_session_is_created() {
    let root = create_temp_dir();
    let (repo, search) = fixture(&root);
    assert!(search_all(&search, "auth").is_empty());
    let mut session = repo
        .create(&SqliteSessionCreateOptions { id: Some("session-1".into()), cwd: root.to_string_lossy().into_owned(), ..Default::default() })
        .await
        .unwrap();
    let entry_id = session.append_message(user_message("Find the auth defect")).await.unwrap();

    let hits = search_all(&search, "auth");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].session_id, "session-1");
    assert_eq!(hits[0].entry_id, entry_id);
    session.append_message(user_message("Still writable")).await.unwrap();
}
