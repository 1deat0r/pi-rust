//! SQLite FTS5 session search — port of
//! `packages/session-backends/sqlite-node/src/sqlite/search-backend.ts`.

use pi_agent::session::types::{SessionError, SessionErrorKind};
use rusqlite::Connection;

use crate::migrations::apply_migrations;
use crate::sql::SqlQuery;
use crate::storage::sessions::decode_session_metadata;
use crate::types::SqliteSessionMetadata;

pub struct SqliteSessionSearchOptions {
    pub database_path: String,
}

/// Search hit returned by [`SqliteSessionSearch`].
#[derive(Debug, Clone)]
pub struct SqliteSessionSearchHit {
    pub session_id: String,
    pub metadata: SqliteSessionMetadata,
    pub entry_id: String,
    pub timestamp: u64,
    pub score: f64,
}

fn table_exists(db: &Connection, name: &str) -> rusqlite::Result<bool> {
    Ok(SqlQuery::new(
        "SELECT 1 AS found FROM sqlite_master WHERE type = 'table' AND name = ? LIMIT 1",
    )
    .bind(name)
    .get_row(db, |_row| Ok(()))?
    .is_some())
}

fn rebuild_search_index(db: &Connection) -> rusqlite::Result<()> {
    SqlQuery::new("INSERT INTO session_search_fts(session_search_fts) VALUES('rebuild')")
        .run(db)?;
    Ok(())
}

fn ensure_search_schema(db: &mut Connection) -> rusqlite::Result<()> {
    let fts_exists = table_exists(db, "session_search_fts")?;
    let entries_exist = table_exists(db, "entries")?;
    db.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS session_search_fts USING fts5(
  payload,
  content = 'entries',
  content_rowid = 'rowid',
  tokenize = 'trigram remove_diacritics 1'
);
CREATE TRIGGER IF NOT EXISTS session_search_fts_ai AFTER INSERT ON entries BEGIN
  INSERT INTO session_search_fts(rowid, payload) VALUES (new.rowid, new.payload);
END;
CREATE TRIGGER IF NOT EXISTS session_search_fts_ad AFTER DELETE ON entries BEGIN
  INSERT INTO session_search_fts(session_search_fts, rowid, payload) VALUES('delete', old.rowid, old.payload);
END;
CREATE TRIGGER IF NOT EXISTS session_search_fts_au AFTER UPDATE OF payload ON entries BEGIN
  INSERT INTO session_search_fts(session_search_fts, rowid, payload) VALUES('delete', old.rowid, old.payload);
  INSERT INTO session_search_fts(rowid, payload) VALUES (new.rowid, new.payload);
END;",
    )?;
    if !fts_exists && entries_exist {
        rebuild_search_index(db)?;
    }
    Ok(())
}

fn get_parent_path(path: &str) -> String {
    let normalized = path.trim_end_matches(['/', '\\']);
    let last_slash = normalized.rfind(['/', '\\']);
    match last_slash {
        None => ".".to_string(),
        Some(0) => normalized[..1].to_string(),
        Some(index) => normalized[..index].to_string(),
    }
}

fn configure_sqlite_database(db: &Connection) -> rusqlite::Result<()> {
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.pragma_update(None, "synchronous", "FULL")?;
    db.pragma_update(None, "busy_timeout", 5000i64)?;
    Ok(())
}

/// SQLite FTS search over a co-located canonical session database (port of
/// `SqliteSessionSearch`).
pub struct SqliteSessionSearch {
    database_path: String,
}

impl SqliteSessionSearch {
    pub fn new(options: SqliteSessionSearchOptions) -> Self {
        Self {
            database_path: options.database_path,
        }
    }

    fn open_database(&self) -> Result<Connection, SessionError> {
        let path = crate::repo::absolute_path(&self.database_path);
        let directory = get_parent_path(&path);
        std::fs::create_dir_all(&directory).map_err(|error| {
            SessionError::new(
                SessionErrorKind::Storage,
                format!("Failed to create SQLite search directory {directory}: {error}"),
            )
        })?;
        let mut db = Connection::open(&path).map_err(|error| {
            SessionError::new(
                SessionErrorKind::Storage,
                format!("Failed to open SQLite search database {path}: {error}"),
            )
        })?;
        let setup = (|| -> rusqlite::Result<()> {
            configure_sqlite_database(&db)?;
            apply_migrations(&mut db)?;
            ensure_search_schema(&mut db)?;
            Ok(())
        })();
        if let Err(error) = setup {
            drop(db);
            return Err(SessionError::new(
                SessionErrorKind::Storage,
                format!("Failed to initialize SQLite search database {path}: {error}"),
            ));
        }
        Ok(db)
    }

    /// Runs a search, returning all hits (port of the async iterator).
    pub fn search(
        &self,
        text: &str,
        options: &SearchOptions,
    ) -> Result<Vec<SqliteSessionSearchHit>, SessionError> {
        let has_entry_types = options
            .entry_types
            .as_ref()
            .map(|types| !types.is_empty())
            .unwrap_or(true);
        let query_text = text.trim();
        if query_text.is_empty() || options.limit == Some(0) || !has_entry_types {
            return Ok(Vec::new());
        }
        let db = self.open_database()?;
        let result = self.search_impl(&db, query_text, options);
        drop(db);
        result
    }

    fn search_impl(
        &self,
        db: &Connection,
        query_text: &str,
        options: &SearchOptions,
    ) -> Result<Vec<SqliteSessionSearchHit>, SessionError> {
        // Quote the query so user text cannot expose FTS grammar.
        let quoted = format!("\"{}\"", query_text.replace('"', "\"\""));
        let mut predicates = vec!["session_search_fts MATCH ?".to_string()];
        let mut params: Vec<rusqlite::types::Value> = vec![quoted.into()];
        if let Some(entry_types) = &options.entry_types {
            let placeholders = entry_types
                .iter()
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(", ");
            predicates.push(format!("se.type IN ({placeholders})"));
            params.extend(
                entry_types
                    .iter()
                    .map(|t| rusqlite::types::Value::Text(t.clone())),
            );
        }

        let predicates = predicates.join(" AND ");
        let sql = format!(
            "SELECT s.id, s.created_at, s.metadata, s.cwd, s.parent_session_id,
                name_fact.seq IS NOT NULL AS has_session_name,
                name_fact.value AS session_name,
                se.id AS entry_id, se.timestamp, bm25(session_search_fts) AS score
            FROM session_search_fts
            JOIN entries AS se ON se.rowid = session_search_fts.rowid
            JOIN sessions AS s ON s.id = se.session_id
            LEFT JOIN facts AS name_fact
                ON name_fact.session_id = s.id
                AND name_fact.kind = 'name'
                AND name_fact.key IS NULL
                AND name_fact.seq = (
                    SELECT MAX(f.seq)
                    FROM facts AS f
                    WHERE f.session_id = s.id AND f.kind = 'name' AND f.key IS NULL
                )
            WHERE {predicates} ORDER BY score
            LIMIT ?"
        );
        let limit = options.limit.map(|l| l as i64).unwrap_or(-1);
        let mut stmt = db.prepare(&sql).map_err(|error| {
            SessionError::new(
                SessionErrorKind::Storage,
                format!("Failed to prepare search: {error}"),
            )
        })?;
        let rows = stmt
            .query_map(
                rusqlite::params_from_iter(params.into_iter().chain(std::iter::once(limit.into()))),
                |row| {
                    Ok(SearchRow {
                        id: row.get(0)?,
                        created_at: row.get(1)?,
                        metadata: row.get(2)?,
                        cwd: row.get(3)?,
                        parent_session_id: row.get(4)?,
                        has_session_name: row.get::<_, i64>(5)? != 0,
                        session_name: row.get(6)?,
                        entry_id: row.get(7)?,
                        timestamp: row.get(8)?,
                        score: row.get(9)?,
                    })
                },
            )
            .map_err(|error| {
                SessionError::new(SessionErrorKind::Storage, format!("Search failed: {error}"))
            })?;

        let mut hits = Vec::new();
        for row in rows {
            let row = row.map_err(|error| {
                SessionError::new(
                    SessionErrorKind::Storage,
                    format!("Search row failed: {error}"),
                )
            })?;
            let core_row = crate::storage::sessions::SessionRow {
                id: row.id,
                created_at: row.created_at,
                metadata: row.metadata,
                cwd: row.cwd,
                parent_session_id: row.parent_session_id,
                has_session_name: row.has_session_name,
                session_name: row.session_name,
            };
            let metadata = decode_session_metadata(&core_row, &self.database_path)
                .map_err(|e| SessionError::new(e.kind, e.message))?;
            hits.push(SqliteSessionSearchHit {
                session_id: metadata.id.clone(),
                metadata,
                entry_id: row.entry_id,
                timestamp: row.timestamp as u64,
                score: row.score,
            });
        }
        Ok(hits)
    }
}

struct SearchRow {
    id: String,
    created_at: i64,
    metadata: Option<String>,
    cwd: String,
    parent_session_id: Option<String>,
    has_session_name: bool,
    session_name: Option<String>,
    entry_id: String,
    timestamp: i64,
    score: f64,
}

/// Search options for the SQLite FTS search (port of `SessionSearchOptions`).
#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    /// Restrict hits to given canonical entry types.
    pub entry_types: Option<Vec<String>>,
    /// Maximum number of hits to return.
    pub limit: Option<usize>,
}

/// Convenience factory (port of `createSqliteSessionSearch`).
pub fn create_sqlite_session_search(database_path: impl Into<String>) -> SqliteSessionSearch {
    SqliteSessionSearch::new(SqliteSessionSearchOptions {
        database_path: database_path.into(),
    })
}
