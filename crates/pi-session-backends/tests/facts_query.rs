//! SQLite fact queries — port of `test/facts-query.test.ts`.

use pi_session_backends::migrations::apply_migrations;
use pi_session_backends::storage::facts::{append_fact, read_latest_fact, read_latest_label_facts};
use rusqlite::Connection;

mod test_utils;
use test_utils::create_temp_dir;

#[test]
fn reads_latest_facts_and_latest_non_null_labels() {
    let database_path = create_temp_dir().join("sessions.sqlite");
    let mut db = Connection::open(&database_path).unwrap();
    apply_migrations(&mut db).unwrap();
    append_fact(&db, "session-1", 1, "label", Some("entry-1"), Some(r#""old""#)).unwrap();
    append_fact(&db, "session-1", 2, "label", Some("entry-2"), Some(r#""kept""#)).unwrap();
    append_fact(&db, "session-1", 3, "label", Some("entry-1"), Some(r#""new""#)).unwrap();
    append_fact(&db, "session-1", 4, "label", Some("entry-3"), Some(r#""removed""#)).unwrap();
    append_fact(&db, "session-1", 5, "label", Some("entry-3"), None).unwrap();
    append_fact(&db, "session-1", 6, "name", None, Some(r#""session name""#)).unwrap();
    append_fact(&db, "other-session", 1, "label", Some("entry-1"), Some(r#""other""#)).unwrap();

    assert_eq!(
        read_latest_fact(&db, "session-1", "label", Some("entry-1")).unwrap().unwrap().value,
        Some(r#""new""#.to_string())
    );
    assert_eq!(
        read_latest_fact(&db, "session-1", "name", None).unwrap().unwrap().value,
        Some(r#""session name""#.to_string())
    );
    assert_eq!(
        read_latest_label_facts(&db, "session-1").unwrap(),
        vec![
            ("entry-1".to_string(), r#""new""#.to_string()),
            ("entry-2".to_string(), r#""kept""#.to_string()),
        ]
    );
}
