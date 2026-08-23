//! SQLite log queries — port of `test/log-query.test.ts`.

use pi_agent::session::state::LogOptions;
use pi_session_backends::repo::SqliteSessionRepository;
use pi_session_backends::types::{SqliteSessionCreateOptions, SqliteWriterLeaseOptions};

mod test_utils;
use test_utils::{create_temp_dir, user_message};

#[tokio::test]
async fn does_not_decode_rows_beyond_the_requested_log_limit() {
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
    session.set_name(Some("name")).await.unwrap();
    let tail_id = session.append_message(user_message("tail")).await.unwrap();

    {
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute(
            "UPDATE entries SET payload = ? WHERE session_id = ? AND id = ?",
            rusqlite::params!["not json", "session-1", tail_id],
        )
        .unwrap();
    }

    let log = session
        .get_log(&LogOptions {
            limit: Some(1),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(log.len(), 1);
    match &log[0] {
        pi_agent::session::types::LogItem::Entry(e) if e.id() == root_id => {}
        other => panic!("expected root entry, got {other:?}"),
    }

    let log = session
        .get_log(&LogOptions {
            after_seq: Some(1),
            limit: Some(1),
        })
        .await
        .unwrap();
    assert_eq!(log.len(), 1);
    match &log[0] {
        pi_agent::session::types::LogItem::Fact(f)
            if f.fact == "name" && f.name.as_deref() == Some("name") => {}
        other => panic!("expected name fact, got {other:?}"),
    }
}
