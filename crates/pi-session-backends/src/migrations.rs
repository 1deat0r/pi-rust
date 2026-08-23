//! SQLite schema migrations — port of
//! `packages/session-backends/sqlite-node/src/sqlite/migrations.ts`.

use rusqlite::Connection;

use crate::sql::SqlQuery;

pub struct SqliteMigration {
    pub id: &'static str,
    pub order: u32,
    pub sql: &'static str,
}

/// The initial schema. This is the exact upstream
/// `migrations/001_initial.sql`; the text must stay byte-identical so the
/// observable `sqlite_master` structure matches upstream.
pub const MIGRATION_001_INITIAL_SQL: &str = include_str!("migrations/001_initial.sql");

pub fn load_migrations() -> Vec<SqliteMigration> {
    vec![SqliteMigration {
        id: "001_initial.sql",
        order: 1,
        sql: MIGRATION_001_INITIAL_SQL,
    }]
}

fn ensure_migrations_table(db: &Connection) -> rusqlite::Result<()> {
    SqlQuery::new(
        "CREATE TABLE IF NOT EXISTS migrations (\n\tid TEXT PRIMARY KEY,\n\tapplied_at TEXT NOT NULL\n);",
    )
    .exec(db)
}

/// Applies un-applied migrations, recording each in `migrations`.
pub fn apply_migrations(db: &mut Connection) -> rusqlite::Result<()> {
    ensure_migrations_table(db)?;
    let migrations = load_migrations();
    let applied_rows = SqlQuery::new("SELECT id FROM migrations ORDER BY applied_at, id")
        .all_rows(db, |row| row.get::<_, String>(0))?;
    let mut applied: std::collections::HashSet<String> = applied_rows.into_iter().collect();

    for migration in migrations {
        if applied.contains(migration.id) {
            continue;
        }
        transaction(db, |tx| -> rusqlite::Result<()> {
            tx.execute_batch(migration.sql)?;
            SqlQuery::new("INSERT INTO migrations (id, applied_at) VALUES (?, ?)")
                .bind(migration.id)
                .bind(iso_now())
                .run(tx)?;
            Ok(())
        })?;
        applied.insert(migration.id.to_string());
    }
    Ok(())
}

/// Runs a synchronous write transaction with `BEGIN IMMEDIATE` semantics
/// (mirror of `SqliteDatabase.transaction`). The closure may fail with any
/// error that converts from `rusqlite::Error` (the sqlite backend uses
/// [`SessionError`](pi_agent::session::types::SessionError)).
pub fn transaction<F, T, E>(db: &mut Connection, f: F) -> Result<T, E>
where
    F: FnOnce(&Connection) -> Result<T, E>,
    E: SqliteErrorExt,
{
    let tx = db
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(E::from_sqlite)?;
    match f(&tx) {
        Ok(value) => {
            tx.commit().map_err(E::from_sqlite)?;
            Ok(value)
        }
        Err(error) => {
            // Dropping `tx` rolls back; preserve the original error.
            drop(tx);
            Err(error)
        }
    }
}

/// Local conversion trait for [`transaction`] (see `repo.rs`).
pub trait SqliteErrorExt: Sized {
    fn from_sqlite(error: rusqlite::Error) -> Self;
}

impl SqliteErrorExt for rusqlite::Error {
    fn from_sqlite(error: rusqlite::Error) -> Self {
        error
    }
}

impl SqliteErrorExt for pi_agent::session::types::SessionError {
    fn from_sqlite(error: rusqlite::Error) -> Self {
        pi_agent::session::types::SessionError::new(
            pi_agent::session::types::SessionErrorKind::Storage,
            error.to_string(),
        )
    }
}

fn iso_now() -> String {
    // Container-friendly UTC ISO timestamp with milliseconds, matching
    // `new Date().toISOString()`.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let millis = now.subsec_millis();
    let days = secs.div_euclid(86400);
    let time_secs = secs.rem_euclid(86400);
    let (y, mo, d) = civil_from_days(days);
    let (h, mi, s) = (time_secs / 3600, (time_secs / 60) % 60, time_secs % 60);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{millis:03}Z")
}

/// Convert days since 1970-01-01 to (year, month, day). Howard Hinnant's
/// civil_from_days algorithm.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (365 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
