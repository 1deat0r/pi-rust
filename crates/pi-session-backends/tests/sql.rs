//! SQL composition helper — port of `test/sql.test.ts`.

use pi_session_backends::sql::{join_sql_fragments, SqlQuery};
use rusqlite::Connection;

#[test]
fn composes_queries_without_renumbering_parameters() {
    let db = Connection::open_in_memory().unwrap();
    SqlQuery::new(
        "CREATE TABLE entries (id TEXT PRIMARY KEY, kind TEXT NOT NULL, active INTEGER NOT NULL)",
    )
    .exec(&db)
    .unwrap();
    SqlQuery::new("INSERT INTO entries (id, kind, active) VALUES (?, ?, ?)")
        .bind("one")
        .bind("message")
        .bind(1)
        .run(&db)
        .unwrap();
    SqlQuery::new("INSERT INTO entries (id, kind, active) VALUES (?, ?, ?)")
        .bind("two")
        .bind("message")
        .bind(0)
        .run(&db)
        .unwrap();
    let filters = join_sql_fragments(
        &[
            SqlQuery::new("kind = ?").bind("message"),
            SqlQuery::new("active = ?").bind(1),
        ],
        " AND ",
    );
    let query = SqlQuery::new("SELECT id FROM entries WHERE ")
        .inline(&filters)
        .inline(&SqlQuery::new(" LIMIT ?").bind(10));
    let rows = query.all_rows(&db, |row| row.get::<_, String>(0)).unwrap();
    assert_eq!(rows, vec!["one".to_string()]);
}

#[test]
fn executes_parameterized_queries() {
    let db = Connection::open_in_memory().unwrap();
    SqlQuery::new("CREATE TABLE values_table (id INTEGER PRIMARY KEY, value TEXT NOT NULL)")
        .exec(&db)
        .unwrap();
    SqlQuery::new("INSERT INTO values_table (id, value) VALUES (?, ?)")
        .bind(1)
        .bind("one")
        .run(&db)
        .unwrap();
    SqlQuery::new("INSERT INTO values_table (id, value) VALUES (?, ?)")
        .bind(2)
        .bind("two")
        .run(&db)
        .unwrap();

    let value = SqlQuery::new("SELECT value FROM values_table WHERE id = ?")
        .bind(1)
        .get_row(&db, |row| row.get(0))
        .unwrap();
    assert_eq!(value, Some("one".to_string()));
    let all: Vec<String> = SqlQuery::new("SELECT value FROM values_table ORDER BY id")
        .all_rows(&db, |row| row.get(0))
        .unwrap();
    assert_eq!(all, vec!["one".to_string(), "two".to_string()]);
}

/// node:sqlite adapter transactional semantics — port of `test/adapter.test.ts`.
#[test]
fn commits_a_synchronous_transaction_and_returns_its_result() {
    let mut db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE values_table (value INTEGER NOT NULL)")
        .unwrap();
    let result = pi_session_backends::migrations::transaction(&mut db, |tx| {
        tx.execute("INSERT INTO values_table (value) VALUES (?)", [42])
            .unwrap();
        Ok::<&str, rusqlite::Error>("committed")
    })
    .unwrap();
    assert_eq!(result, "committed");
    let value = db
        .query_row("SELECT value FROM values_table", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
    assert_eq!(value, 42);
}

/// A transaction aborts (rolls back) when the closure errors.
#[test]
fn rolls_back_a_failed_transaction() {
    let mut db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE values_table (value INTEGER NOT NULL)")
        .unwrap();
    let result: Result<(), rusqlite::Error> =
        pi_session_backends::migrations::transaction(&mut db, |tx| {
            tx.execute("INSERT INTO values_table (value) VALUES (?)", [42])
                .unwrap();
            Err(rusqlite::Error::InvalidQuery)
        });
    assert!(result.is_err());
    let count: i64 = db
        .query_row("SELECT COUNT(*) FROM values_table", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
}
