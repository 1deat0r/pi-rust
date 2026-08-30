//! Parameterized SQLite query builder — port of
//! `packages/session-backends/sqlite-node/src/sqlite/sql.ts`.
//!
//! The TypeScript original builds parameterized queries from tagged template
//! literals, inlining nested `SqlQuery` fragments and turning every other
//! interpolation into a positional `?` parameter. This module reproduces the
//! same composition semantics so the storage-layer SQL ports are mechanical.

use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection};

/// Result of a prepared SQLite statement execution (mirror of
/// `SqliteRunResult` in `types.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqliteRunResult {
    /// Number of rows changed by the statement.
    pub changes: u64,
    /// Inserted row id when the backend exposes one.
    pub last_insert_rowid: Option<i64>,
}

/// Converts a Rust value into a SQLite bind value. rusqlite's `Value` only
/// implements `From` for owned types; this local trait keeps the port's
/// `bind` ergonomic for the borrowed strings used throughout the SQL ports.
pub trait BindValue {
    fn into_value(self) -> Value;
}

impl BindValue for Value {
    fn into_value(self) -> Value {
        self
    }
}

impl BindValue for &str {
    fn into_value(self) -> Value {
        Value::Text(self.to_string())
    }
}

impl BindValue for String {
    fn into_value(self) -> Value {
        Value::Text(self)
    }
}

impl BindValue for &String {
    fn into_value(self) -> Value {
        Value::Text(self.clone())
    }
}

impl BindValue for i64 {
    fn into_value(self) -> Value {
        Value::Integer(self)
    }
}

impl BindValue for i32 {
    fn into_value(self) -> Value {
        Value::Integer(self as i64)
    }
}

impl BindValue for u64 {
    fn into_value(self) -> Value {
        Value::Integer(self as i64)
    }
}

impl BindValue for usize {
    fn into_value(self) -> Value {
        Value::Integer(self as i64)
    }
}

impl BindValue for f64 {
    fn into_value(self) -> Value {
        Value::Real(self)
    }
}

impl BindValue for bool {
    fn into_value(self) -> Value {
        Value::Integer(self as i64)
    }
}

impl BindValue for Option<&str> {
    fn into_value(self) -> Value {
        match self {
            Some(value) => Value::Text(value.to_string()),
            None => Value::Null,
        }
    }
}

impl BindValue for Option<String> {
    fn into_value(self) -> Value {
        match self {
            Some(value) => Value::Text(value),
            None => Value::Null,
        }
    }
}

impl BindValue for Option<&String> {
    fn into_value(self) -> Value {
        match self {
            Some(value) => Value::Text(value.clone()),
            None => Value::Null,
        }
    }
}

impl BindValue for Option<i64> {
    fn into_value(self) -> Value {
        match self {
            Some(value) => Value::Integer(value),
            None => Value::Null,
        }
    }
}

impl BindValue for Option<i32> {
    fn into_value(self) -> Value {
        match self {
            Some(value) => Value::Integer(value as i64),
            None => Value::Null,
        }
    }
}

impl BindValue for Option<f64> {
    fn into_value(self) -> Value {
        match self {
            Some(value) => Value::Real(value),
            None => Value::Null,
        }
    }
}

impl BindValue for Option<Value> {
    fn into_value(self) -> Value {
        self.unwrap_or(Value::Null)
    }
}

/// A parameterized SQLite query produced by composing fragments with
/// [`sql`] / [`join_sql_fragments`].
#[derive(Debug, Clone, Default)]
pub struct SqlQuery {
    query_text: String,
    params: Vec<Value>,
}

impl SqlQuery {
    pub fn new(query_text: impl Into<String>) -> Self {
        Self {
            query_text: query_text.into(),
            params: Vec::new(),
        }
    }

    pub fn query_text(&self) -> &str {
        &self.query_text
    }

    pub fn params(&self) -> &[Value] {
        &self.params
    }

    /// Appends a bound parameter (`?` in the query text).
    pub fn bind(mut self, value: impl BindValue) -> Self {
        self.params.push(value.into_value());
        self
    }

    /// Inlines another fragment's text and parameters at the current position.
    pub fn inline(mut self, fragment: &SqlQuery) -> Self {
        self.query_text.push_str(&fragment.query_text);
        self.params.extend(fragment.params.iter().cloned());
        self
    }

    /// Executes a parameterless DDL/PRAGMA statement.
    ///
    /// Mirrors `SqlQuery.exec`: exec queries cannot have parameters.
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    pub fn exec(&self, db: &Connection) -> rusqlite::Result<()> {
        if !self.params.is_empty() {
            panic!("SQLite exec queries cannot have parameters");
        }
        db.execute_batch(&self.query_text)
    }

    /// Runs the statement and returns the changes / last insert rowid.
    pub fn run(&self, db: &Connection) -> rusqlite::Result<SqliteRunResult> {
        let mut stmt = db.prepare(&self.query_text)?;
        let changes = stmt.execute(params_from_iter(self.params.iter()))?;
        Ok(SqliteRunResult {
            changes: changes as u64,
            last_insert_rowid: Some(db.last_insert_rowid()),
        })
    }

    /// Executes and returns the first row mapped by `map`, or `None`.
    pub fn get_row<T>(
        &self,
        db: &Connection,
        map: impl FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<Option<T>> {
        let mut stmt = db.prepare(&self.query_text)?;
        let mut rows = stmt.query(params_from_iter(self.params.iter()))?;
        match rows.next()? {
            Some(row) => Ok(Some(map(row)?)),
            None => Ok(None),
        }
    }

    /// Executes and returns all rows mapped by `map`.
    pub fn all_rows<T>(
        &self,
        db: &Connection,
        map: impl Fn(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<Vec<T>> {
        let mut stmt = db.prepare(&self.query_text)?;
        let rows = stmt.query_map(params_from_iter(self.params.iter()), map)?;
        rows.collect()
    }
}

/// Builds a parameterized query. Nested `SqlQuery` values are inlined; other
/// interpolations become `?` parameters (port of the `sql` template helper).
pub fn sql(fragment: &SqlQuery) -> SqlQuery {
    fragment.clone()
}

/// Joins trusted query fragments while preserving their parameter order.
pub fn join_sql_fragments(fragments: &[SqlQuery], separator: &str) -> SqlQuery {
    let mut out = SqlQuery::default();
    for (index, fragment) in fragments.iter().enumerate() {
        if index > 0 {
            out.query_text.push_str(separator);
        }
        out = out.inline(fragment);
    }
    out
}

/// Empty fragment helper for optional SQL parts (mirrors `sql\`\``).
pub fn empty() -> SqlQuery {
    SqlQuery::default()
}

/// Returns a column reference fragment bound to nothing (mirrors `sql`COL``).
pub fn ident(name: &str) -> SqlQuery {
    SqlQuery::new(name.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn composes_fragments_without_renumbering_parameters() {
        let db = Connection::open_in_memory().unwrap();
        sql(&SqlQuery::new("CREATE TABLE entries (id TEXT PRIMARY KEY, kind TEXT NOT NULL, active INTEGER NOT NULL)")).exec(&db).unwrap();
        sql(
            &SqlQuery::new("INSERT INTO entries (id, kind, active) VALUES (?, ?, ?)")
                .bind("one")
                .bind("message")
                .bind(1),
        )
        .run(&db)
        .unwrap();
        sql(
            &SqlQuery::new("INSERT INTO entries (id, kind, active) VALUES (?, ?, ?)")
                .bind("two")
                .bind("message")
                .bind(0),
        )
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
        sql(&SqlQuery::new(
            "CREATE TABLE values_table (id INTEGER PRIMARY KEY, value TEXT NOT NULL)",
        ))
        .exec(&db)
        .unwrap();
        sql(
            &SqlQuery::new("INSERT INTO values_table (id, value) VALUES (?, ?)")
                .bind(1)
                .bind("one"),
        )
        .run(&db)
        .unwrap();
        sql(
            &SqlQuery::new("INSERT INTO values_table (id, value) VALUES (?, ?)")
                .bind(2)
                .bind("two"),
        )
        .run(&db)
        .unwrap();

        let value = sql(&SqlQuery::new("SELECT value FROM values_table WHERE id = ?").bind(1))
            .get_row(&db, |row| row.get::<_, String>(0))
            .unwrap();
        assert_eq!(value, Some("one".to_string()));
        let all = sql(&SqlQuery::new("SELECT value FROM values_table ORDER BY id"))
            .all_rows(&db, |row| row.get::<_, String>(0))
            .unwrap();
        assert_eq!(all, vec!["one".to_string(), "two".to_string()]);
    }
}
