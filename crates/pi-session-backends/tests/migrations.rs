//! SQLite schema migrations — port of `test/migrations.test.ts`.

use pi_session_backends::migrations::apply_migrations;
use pi_session_backends::sql::SqlQuery;
use rusqlite::Connection;

mod test_utils;
use test_utils::create_temp_dir;

#[test]
fn applies_the_current_schema_once_and_records_its_migration() {
    let dir = create_temp_dir();
    let database_path = dir.join("sessions.sqlite");
    let mut db = Connection::open(&database_path).unwrap();
    apply_migrations(&mut db).unwrap();
    apply_migrations(&mut db).unwrap();

    let rows = SqlQuery::new("SELECT id FROM migrations ORDER BY id")
        .all_rows(&db, |row| row.get::<_, String>(0))
        .unwrap();
    assert_eq!(rows, vec!["001_initial.sql".to_string()]);

    let tables = SqlQuery::new("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .all_rows(&db, |row| row.get::<_, String>(0))
        .unwrap();
    for expected in [
        "migrations", "sessions", "entries", "session_sequences", "session_stats", "branch_entries",
        "branch_tips", "lanes", "records", "lane_moves", "facts", "writer_leases",
    ] {
        assert!(tables.iter().any(|t| t == expected), "missing table {expected}: {tables:?}");
    }

    let session_columns = db
        .prepare("PRAGMA table_info(sessions)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap();
    assert!(!session_columns.contains(&"leaf_id".to_string()));

    let session_indexes = db
        .prepare("PRAGMA index_list(sessions)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap();
    assert!(session_indexes.contains(&"idx_sessions_cwd_created_at".to_string()));
    assert!(!session_indexes.contains(&"idx_sessions_parent".to_string()));

    let lane_columns = db
        .prepare("PRAGMA table_info(lanes)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap();
    assert!(lane_columns.contains(&"open_operation_id".to_string()));

    let entry_indexes = db
        .prepare("PRAGMA index_list(entries)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap();
    assert!(!entry_indexes.contains(&"idx_entries_session_seq".to_string()));

    let branch_entry_indexes = db
        .prepare("PRAGMA index_list(branch_entries)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap();
    assert!(branch_entry_indexes.contains(&"idx_branch_entries_session_entry".to_string()));

    let record_indexes = db
        .prepare("PRAGMA index_list(records)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap();
    for expected in [
        "idx_records_session_lane_seq",
        "idx_records_session_type_seq",
        "idx_records_session_type_op_kind_seq",
    ] {
        assert!(record_indexes.contains(&expected.to_string()), "missing index {expected}: {record_indexes:?}");
    }
    assert!(!record_indexes.contains(&"idx_records_session_seq".to_string()));

    let lane_move_indexes = db
        .prepare("PRAGMA index_list(lane_moves)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap();
    assert!(!lane_move_indexes.contains(&"idx_lane_moves_session_lane_seq".to_string()));

    drop(db);
}
