//! SQLite session repository — port of `test/repository.test.ts`.

use pi_agent::session::types::{NewRecord, SessionErrorKind};
use pi_ai::types::{Cost, Usage};
use pi_session_backends::repo::{ForkCreateOptions, SqliteSessionRepository};
use pi_session_backends::types::{
    SqliteSessionCreateOptions, SqliteSessionListOptions, SqliteWriterLeaseOptions,
};

mod test_utils;
use test_utils::{
    append_sqlite_compaction, assistant_message, create_temp_dir, get_sqlite_entries,
    move_sqlite_main_lane, user_message,
};

fn repo_for(root: &std::path::Path) -> SqliteSessionRepository {
    SqliteSessionRepository::new(
        root.join("sessions.sqlite").to_string_lossy().into_owned(),
        Some(SqliteWriterLeaseOptions {
            ttl_ms: 5000,
            heartbeat_interval_ms: 1000,
        }),
    )
}

fn usage(
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    total: i64,
    cost: f64,
) -> Usage {
    Usage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: total,
        cost: Cost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            total: cost,
        },
    }
}

#[tokio::test]
async fn persists_session_metadata_through_create_list_open_and_fork() {
    let root = create_temp_dir();
    let repo = repo_for(&root);
    let source = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            metadata: Some(serde_json::json!({ "profile": "reviewer" })),
            ..Default::default()
        })
        .await
        .unwrap();
    let source_metadata = source.get_sqlite_metadata().await.unwrap();
    assert_eq!(
        source_metadata.metadata,
        Some(serde_json::json!({ "profile": "reviewer" }))
    );
    let listed = repo
        .list(&SqliteSessionListOptions::default())
        .await
        .unwrap();
    assert_eq!(
        listed[0].metadata,
        Some(serde_json::json!({ "profile": "reviewer" }))
    );

    let reopened = repo.open(&source_metadata).await.unwrap();
    assert_eq!(
        reopened.get_sqlite_metadata().await.unwrap().metadata,
        Some(serde_json::json!({ "profile": "reviewer" }))
    );

    let fork = repo
        .fork(
            &source_metadata,
            &ForkCreateOptions {
                id: Some("session-2".into()),
                cwd: root.to_string_lossy().into_owned(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        fork.get_sqlite_metadata().await.unwrap().metadata,
        Some(serde_json::json!({ "profile": "reviewer" }))
    );

    let overridden = repo
        .fork(
            &source_metadata,
            &ForkCreateOptions {
                id: Some("session-3".into()),
                cwd: root.to_string_lossy().into_owned(),
                metadata: Some(serde_json::json!({ "profile": "writer" })),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        overridden.get_sqlite_metadata().await.unwrap().metadata,
        Some(serde_json::json!({ "profile": "writer" }))
    );
}

#[tokio::test]
async fn rolls_back_the_entire_fork_when_copying_an_entry_fails() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite");
    let repo = repo_for(&root);
    let mut source = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("source".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    source.append_message(user_message("one")).await.unwrap();
    source
        .append_message(assistant_message("two"))
        .await
        .unwrap();
    let source_metadata = source.get_sqlite_metadata().await.unwrap();

    {
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute_batch(
            "
CREATE TRIGGER fail_fork_entry BEFORE INSERT ON entries
WHEN new.session_id = 'fork' AND new.seq = 2
BEGIN
  SELECT RAISE(ABORT, 'fail fork');
END;
",
        )
        .unwrap();
    }

    let err = repo
        .fork(
            &source_metadata,
            &ForkCreateOptions {
                id: Some("fork".into()),
                cwd: root.to_string_lossy().into_owned(),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind, SessionErrorKind::Storage, "got: {err}");

    {
        let inspection = rusqlite::Connection::open(&database_path).unwrap();
        let session_row =
            pi_session_backends::sql::SqlQuery::new("SELECT id FROM sessions WHERE id = ?")
                .bind("fork")
                .get_row(&inspection, |row| row.get::<_, String>(0))
                .unwrap();
        assert!(session_row.is_none());
        let entries =
            pi_session_backends::sql::SqlQuery::new("SELECT id FROM entries WHERE session_id = ?")
                .bind("fork")
                .all_rows(&inspection, |row| row.get::<_, String>(0))
                .unwrap();
        assert!(entries.is_empty());
    }
}

#[tokio::test]
async fn retains_an_opened_database_after_a_failed_operation_until_disposal() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite");
    let repo = repo_for(&root);

    // A trigger makes the first create fail after the database was opened.
    {
        // Pre-create the db + trigger by creating a scratch session first.
        let _scratch = repo
            .create(&SqliteSessionCreateOptions {
                id: Some("scratch".into()),
                cwd: root.to_string_lossy().into_owned(),
                ..Default::default()
            })
            .await
            .unwrap();
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute_batch(
            "
CREATE TRIGGER fail_session_insert BEFORE INSERT ON sessions
BEGIN
  SELECT RAISE(ABORT, 'insert failed');
END;
",
        )
        .unwrap();
    }

    let err = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(err.message.contains("insert failed"), "got: {err}");

    // The database stays available after the failed operation (no close and
    // no shared-state reset), so the repo can still list the scratch session.
    let listed = repo
        .list(&SqliteSessionListOptions::default())
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "scratch");

    {
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute_batch("DROP TRIGGER fail_session_insert")
            .unwrap();
    }
    let created = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(created.get_metadata().await.unwrap().id, "session-1");

    // Closing the repo is idempotent and completes without error.
    repo.close().await;
    repo.close().await;
}

#[tokio::test]
async fn fails_when_the_database_cannot_be_opened() {
    // A database path that is an existing directory cannot be opened as a
    // SQLite database; the failure surfaces as a storage error and is
    // memoized (mirror of `createSetupFailureSqlite` behavior).
    let root = create_temp_dir();
    let database_path = root.join("not-a-db");
    std::fs::create_dir_all(&database_path).unwrap();
    let repo = SqliteSessionRepository::new(database_path.to_string_lossy().into_owned(), None);

    let err = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind, SessionErrorKind::Storage, "got: {err}");
    let err2 = repo
        .list(&SqliteSessionListOptions::default())
        .await
        .unwrap_err();
    assert_eq!(err2.kind, SessionErrorKind::Storage, "got: {err2}");
}

#[tokio::test]
async fn closes_active_sessions_when_the_repository_is_disposed() {
    let root = create_temp_dir();
    let repo = repo_for(&root);
    let mut session = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();

    repo.close().await;

    let err = session
        .append_message(user_message("late"))
        .await
        .unwrap_err();
    assert_eq!(err.kind, SessionErrorKind::Storage);
    assert!(
        err.message.contains("SQLite session session-1 is closed"),
        "got: {err}"
    );
}

#[tokio::test]
async fn supports_repeated_session_operations_on_one_connection() {
    let root = create_temp_dir();
    let repo = repo_for(&root);
    let mut session = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    for i in 0..10 {
        session
            .append_message(user_message(&format!("message {i}")))
            .await
            .unwrap();
    }
    let entries = get_sqlite_entries(&session, None, None).await.unwrap();
    assert_eq!(entries.len(), 10);
    // Dispose is idempotent.
    repo.close().await;
    repo.close().await;
}

#[tokio::test]
async fn rejects_a_missing_lane_leaf_when_listing_lanes_and_opening() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite");
    let repo = repo_for(&root);
    let session = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    let metadata = session.get_sqlite_metadata().await.unwrap();

    {
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute(
            "UPDATE lanes SET leaf_id = ? WHERE session_id = ? AND lane = ?",
            rusqlite::params!["missing", metadata.id, "main"],
        )
        .unwrap();
    }

    let err = session.get_lanes().await.unwrap_err();
    assert_eq!(err.kind, SessionErrorKind::Storage);
    assert!(
        err.message
            .contains("Lane main points at missing entry missing"),
        "got: {err}"
    );

    let err = repo.open(&metadata).await.unwrap_err();
    assert_eq!(err.kind, SessionErrorKind::Storage);
    assert!(
        err.message
            .contains("Lane main points at missing entry missing"),
        "got: {err}"
    );
}

#[tokio::test]
async fn rejects_stored_session_metadata_containing_invalid_json() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite");
    let repo = repo_for(&root);
    repo.create(&SqliteSessionCreateOptions {
        id: Some("session-1".into()),
        cwd: root.to_string_lossy().into_owned(),
        ..Default::default()
    })
    .await
    .unwrap();

    {
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute(
            "UPDATE sessions SET metadata = ? WHERE id = ?",
            rusqlite::params!["not json", "session-1"],
        )
        .unwrap();
    }
    let err = repo
        .list(&SqliteSessionListOptions::default())
        .await
        .unwrap_err();
    assert_eq!(err.kind, SessionErrorKind::Storage);
    assert!(
        err.message.contains("metadata is not valid JSON"),
        "got: {err}"
    );
}

#[tokio::test]
async fn rejects_stored_session_metadata_containing_a_non_object_value() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite");
    let repo = repo_for(&root);
    repo.create(&SqliteSessionCreateOptions {
        id: Some("session-1".into()),
        cwd: root.to_string_lossy().into_owned(),
        ..Default::default()
    })
    .await
    .unwrap();

    {
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute(
            "UPDATE sessions SET metadata = ? WHERE id = ?",
            rusqlite::params!["[]", "session-1"],
        )
        .unwrap();
    }
    let err = repo
        .list(&SqliteSessionListOptions::default())
        .await
        .unwrap_err();
    assert_eq!(err.kind, SessionErrorKind::Storage);
    assert!(
        err.message.contains("metadata must be an object"),
        "got: {err}"
    );
}

#[tokio::test]
async fn rejects_stored_session_names_containing_invalid_json() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite");
    let repo = repo_for(&root);
    let mut session = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    session.set_name(Some("valid name")).await.unwrap();

    {
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute(
            "UPDATE facts SET value = ? WHERE session_id = ? AND kind = 'name'",
            rusqlite::params!["not json", "session-1"],
        )
        .unwrap();
    }

    let err = repo
        .list(&SqliteSessionListOptions::default())
        .await
        .unwrap_err();
    assert_eq!(err.kind, SessionErrorKind::Storage);
    assert!(err.message.contains("name is not valid JSON"), "got: {err}");

    let err = session.get_metadata().await.unwrap_err();
    assert_eq!(err.kind, SessionErrorKind::Storage);
    assert!(err.message.contains("name is not valid JSON"), "got: {err}");
}

#[tokio::test]
async fn rejects_stored_session_names_containing_a_non_string_value() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite");
    let repo = repo_for(&root);
    let mut session = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    session.set_name(Some("valid name")).await.unwrap();

    {
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute(
            "UPDATE facts SET value = ? WHERE session_id = ? AND kind = 'name'",
            rusqlite::params!["{}", "session-1"],
        )
        .unwrap();
    }

    let err = repo
        .list(&SqliteSessionListOptions::default())
        .await
        .unwrap_err();
    assert!(err.message.contains("name must be a string"), "got: {err}");

    let err = session.get_metadata().await.unwrap_err();
    assert!(err.message.contains("name must be a string"), "got: {err}");
}

#[tokio::test]
async fn omits_a_cleared_session_name_from_metadata() {
    let root = create_temp_dir();
    let repo = repo_for(&root);
    let mut session = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    session.set_name(Some("Temporary")).await.unwrap();
    assert_eq!(session.get_name().await, Some("Temporary".to_string()));

    session.set_name(None).await.unwrap();

    assert_eq!(session.get_name().await, None);
    let metadata = session.get_sqlite_metadata().await.unwrap();
    assert!(metadata.name.is_none());
    let listed = repo
        .list(&SqliteSessionListOptions::default())
        .await
        .unwrap();
    assert!(listed[0].name.is_none());
}

#[tokio::test]
async fn fails_loudly_when_a_stored_entry_is_read_and_cannot_be_decoded() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite");
    let repo = repo_for(&root);
    let mut session = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    let entry_id = session
        .append_message(user_message("message"))
        .await
        .unwrap();
    let metadata = session.get_sqlite_metadata().await.unwrap();

    {
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute(
            "UPDATE entries SET payload = ? WHERE session_id = ? AND id = ?",
            rusqlite::params!["not json", metadata.id, entry_id],
        )
        .unwrap();
    }

    let reopened = repo.open(&metadata).await.unwrap();
    let err = get_sqlite_entries(&reopened, None, None).await.unwrap_err();
    assert_eq!(err.kind, SessionErrorKind::InvalidEntry, "got: {err}");
}

#[tokio::test]
async fn fails_loudly_when_a_stored_record_cannot_be_decoded() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite");
    let repo = repo_for(&root);
    let mut session = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    session
        .append_record(NewRecord::OperationFinished {
            id: "record-1".into(),
            lane: "main".into(),
            run_id: "run-1".into(),
            outcome: "completed".into(),
            error: None,
        })
        .await
        .unwrap();

    {
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute(
            "UPDATE records SET payload = ? WHERE session_id = ? AND id = ?",
            rusqlite::params!["not json", "session-1", "record-1"],
        )
        .unwrap();
    }

    let err = session.find_records(&Default::default()).await.unwrap_err();
    assert_eq!(err.kind, SessionErrorKind::Storage);
    assert!(
        err.message.contains("failed to decode payload"),
        "got: {err}"
    );
}

#[tokio::test]
async fn does_not_publish_connection_state_when_an_append_transaction_fails() {
    let root = create_temp_dir();
    let database_path = root.join("sessions.sqlite");
    let repo = repo_for(&root);
    let mut session = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    {
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute_batch(
            "
CREATE TRIGGER fail_branch_tip_insert
BEFORE INSERT ON branch_tips
BEGIN
    SELECT RAISE(ABORT, 'branch insert failed');
END;
",
        )
        .unwrap();
        let err = session
            .append_message(user_message("root"))
            .await
            .unwrap_err();
        assert!(err.message.contains("branch insert failed"), "got: {err}");

        let lane = pi_session_backends::sql::SqlQuery::new(
            "SELECT leaf_id FROM lanes WHERE session_id = ? AND lane = ?",
        )
        .bind("session-1")
        .bind("main")
        .get_row(&db, |row| row.get::<_, Option<String>>(0))
        .unwrap()
        .flatten();
        assert_eq!(lane, None);
        let entries =
            pi_session_backends::sql::SqlQuery::new("SELECT id FROM entries WHERE session_id = ?")
                .bind("session-1")
                .all_rows(&db, |row| row.get::<_, String>(0))
                .unwrap();
        assert!(entries.is_empty());
        assert_eq!(session.get_stats().await.message_count, 0);
        db.execute_batch("DROP TRIGGER fail_branch_tip_insert")
            .unwrap();
    }

    let entry_id = session.append_message(user_message("root")).await.unwrap();
    let entries = get_sqlite_entries(&session, None, None).await.unwrap();
    assert_eq!(entries[0].id(), entry_id);
    assert_eq!(session.get_stats().await.message_count, 1);
}

#[tokio::test]
async fn accounts_for_assistant_compaction_and_branch_summary_usage() {
    let root = create_temp_dir();
    let repo = repo_for(&root);
    let mut session = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    let _user_id = session.append_message(user_message("one")).await.unwrap();

    let assistant_usage = usage(100, 25, 40, 10, 175, 0.37);
    let assistant = pi_ai::types::Message::Assistant(pi_ai::types::AssistantMessage::Assistant {
        content: vec![pi_ai::types::ContentBlock::text("two")],
        api: Some("anthropic-messages".into()),
        provider: Some("anthropic".into()),
        model: Some("claude-sonnet-4-5".into()),
        response_model: None,
        response_id: None,
        usage: Some(assistant_usage.clone()),
        stop_reason: Some(pi_ai::types::StopReason::Stop),
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: test_utils::now_ms(),
    });
    let assistant = pi_agent::types::AgentMessage::Core(assistant);
    let assistant_id = session.append_message(assistant).await.unwrap();
    session
        .append_record(NewRecord::Usage {
            id: "assistant-usage".into(),
            lane: "main".into(),
            cause: "assistant".into(),
            run_id: Some("run".into()),
            entry_id: Some(assistant_id.clone()),
            attempt: Some(1),
            stop_reason: Some("stop".into()),
            tool_call_id: None,
            details: None,
            usage: assistant_usage.clone(),
        })
        .await
        .unwrap();

    let compaction_usage = usage(1, 2, 3, 4, 10, 0.1);
    let compaction_id = append_sqlite_compaction(
        &mut session,
        "summary",
        200,
        None,
        Some(compaction_usage.clone()),
        vec![],
    )
    .await
    .unwrap();
    session
        .append_record(NewRecord::Usage {
            id: "compaction-usage".into(),
            lane: "main".into(),
            cause: "compaction".into(),
            run_id: Some("run".into()),
            entry_id: Some(compaction_id),
            attempt: Some(1),
            stop_reason: Some("stop".into()),
            tool_call_id: None,
            details: None,
            usage: compaction_usage.clone(),
        })
        .await
        .unwrap();

    let branch_usage = usage(5, 6, 7, 8, 26, 0.26);
    let branch_summary_id = move_sqlite_main_lane(
        &mut session,
        Some(&assistant_id),
        Some((
            "branch summary".to_string(),
            None,
            Some(branch_usage.clone()),
        )),
    )
    .await
    .unwrap()
    .expect("branch summary id");
    session
        .append_record(NewRecord::Usage {
            id: "branch-summary-usage".into(),
            lane: "main".into(),
            cause: "branch_summary".into(),
            run_id: Some("run".into()),
            entry_id: Some(branch_summary_id),
            attempt: Some(1),
            stop_reason: Some("stop".into()),
            tool_call_id: None,
            details: None,
            usage: branch_usage.clone(),
        })
        .await
        .unwrap();

    let stats = session.get_stats().await;
    assert_eq!(stats.message_count, 2);
    assert_eq!(stats.cached_tokens, 50);
    assert_eq!(stats.uncached_tokens, 128);
    assert_eq!(stats.total_tokens, 211);
    assert!(
        (stats.cost_total - 0.73).abs() < 1e-9,
        "cost: {}",
        stats.cost_total
    );
}

#[tokio::test]
async fn compactions_and_branch_summaries_do_not_count_as_messages() {
    let root = create_temp_dir();
    let repo = repo_for(&root);
    let mut session = repo
        .create(&SqliteSessionCreateOptions {
            id: Some("session-1".into()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    session.append_message(user_message("one")).await.unwrap();
    append_sqlite_compaction(&mut session, "summary", 100, None, None, vec![])
        .await
        .unwrap();
    session.append_message(user_message("two")).await.unwrap();
    move_sqlite_main_lane(
        &mut session,
        None,
        Some(("summary".to_string(), None, None)),
    )
    .await
    .unwrap();

    assert_eq!(session.get_stats().await.message_count, 2);
}
