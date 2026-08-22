//! SQLite session repository — port of
//! `packages/session-backends/sqlite-node/src/sqlite/repo.ts`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pi_agent::session::state::{EntryOrder, EntryQuery, ForkOptions, LogOptions, RecordQuery};
use pi_agent::session::types::{
    session_error, Entry, EntryNoStats, LanePointer, LaneRecord, LogItem, NewRecord, SessionError,
    SessionErrorKind, SessionStats,
};
use rusqlite::Connection;
use tokio::sync::Mutex as AsyncMutex;

use crate::branch_cache::{append_entry_to_branch_cache, build_cached_branch, delete_branch_cache, rebuild_branch_cache};
use crate::migrations::{apply_migrations, transaction};
use crate::storage::branch_entries::{
    query_cached_branch_rows, read_cached_branch, CachedBranchEntryRow, CachedBranchQuery,
};
use crate::storage::branch_tips::read_branch_tip_ids;
use crate::storage::entries::{
    delete_entry_rows, id_exists_in_entries, insert_entry_row, read_entry_row, read_entry_rows, serialize_payload,
    EntryRow, NewEntryRow, ReadEntryRowsOptions,
};
use crate::storage::facts::{append_fact, delete_fact_rows, read_fact_rows, read_latest_fact, read_latest_label_facts};
use crate::storage::lanes::{
    create_initial_lane, create_lane, delete_lane_rows, finish_lane_operation, move_lane, read_lane, read_lane_head,
    read_lane_move_rows, read_lanes, set_lane_leaf, start_lane_operation, LaneMoveRow,
};
use crate::storage::records::{
    append_record_row, delete_record_rows, id_exists_in_records, read_open_operation_rows, read_record_rows,
    NewRecordRow, ReadRecordRowsOptions, RecordRow,
};
use crate::storage::session_sequences::{
    advance_sequence, create_sequence, delete_sequence, get_next_sequence, set_next_sequence,
};
use crate::storage::session_stats::{add_usage_to_stats, create_stats, delete_stats, increment_message_count, read_stats};
use crate::storage::sessions::{
    decode_session_metadata, delete_session_row, insert_session_row, read_session_row, read_session_rows,
    session_exists, NewSessionRow, SessionRow,
};
use crate::storage::writer_leases::{
    acquire_writer_lease, delete_writer_lease, release_writer_lease, renew_writer_lease, WriterLease,
};
use crate::types::{
    SqliteSessionCreateOptions, SqliteSessionListOptions, SqliteSessionMetadata, SqliteWriterLeaseOptions,
};
use crate::new_id;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

fn active_writer_error(session_id: &str) -> SessionError {
    session_error(SessionErrorKind::Storage, format!("SQLite session {session_id} already has an active writer"))
}

fn lost_writer_error(session_id: &str) -> SessionError {
    session_error(SessionErrorKind::Storage, format!("SQLite session {session_id} writer lease was lost"))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
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

pub(crate) fn absolute_path(path: &str) -> String {
    let path_buf = PathBuf::from(path);
    if path_buf.is_absolute() {
        path.to_string()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path_buf).to_string_lossy().into_owned(),
            Err(_) => path.to_string(),
        }
    }
}

fn configure_sqlite_database(db: &Connection) -> rusqlite::Result<()> {
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.pragma_update(None, "synchronous", "FULL")?;
    db.pragma_update(None, "busy_timeout", 5000i64)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared repository state
// ---------------------------------------------------------------------------

/// Shared per-repository database state. The SQLite connection sits behind a
/// `tokio` mutex so every operation is serialized — the Rust counterpart of
/// Node's synchronous `DatabaseSync` plus the repository operation queue.
pub(crate) struct RepoState {
    database_path: String,
    db: AsyncMutex<Option<Connection>>,
    open_error: Mutex<Option<SessionError>>,
    active_storages: Mutex<Vec<Arc<SqliteSessionStorage>>>,
    lease_options: SqliteWriterLeaseOptions,
}

impl RepoState {
    pub(crate) fn new(database_path: String, lease_options: SqliteWriterLeaseOptions) -> Arc<Self> {
        Arc::new(Self {
            database_path,
            db: AsyncMutex::new(None),
            open_error: Mutex::new(None),
            active_storages: Mutex::new(Vec::new()),
            lease_options,
        })
    }

    pub(crate) fn lease_options(&self) -> SqliteWriterLeaseOptions {
        self.lease_options
    }

    /// Opens (once) and runs `f` against the shared connection.
    pub(crate) async fn with_db<T>(
        self: &Arc<Self>,
        f: impl FnOnce(&mut Connection) -> Result<T, SessionError> + Send,
    ) -> Result<T, SessionError>
    where
        T: Send,
    {
        let mut guard = self.db.lock().await;
        if guard.is_none() {
            if let Some(error) = self.open_error.lock().unwrap().clone() {
                return Err(error);
            }
            let opened = self.open_database();
            match opened {
                Ok(connection) => *guard = Some(connection),
                Err(error) => {
                    *self.open_error.lock().unwrap() = Some(error.clone());
                    return Err(error);
                }
            }
        }
        let db = guard.as_mut().expect("database opened above");
        f(db)
    }

    fn open_database(&self) -> Result<Connection, SessionError> {
        let path = absolute_path(&self.database_path);
        let parent = get_parent_path(&path);
        std::fs::create_dir_all(&parent).map_err(|error| {
            SessionError::new(
                SessionErrorKind::Storage,
                format!("Failed to create SQLite sessions directory {parent}: {error}"),
            )
        })?;
        let mut db = Connection::open(&path).map_err(|error| {
            SessionError::new(
                SessionErrorKind::Storage,
                format!("Failed to open SQLite session database {path}: {error}"),
            )
        })?;
        let setup = (|| -> rusqlite::Result<()> {
            configure_sqlite_database(&db)?;
            apply_migrations(&mut db)?;
            Ok(())
        })();
        if let Err(error) = setup {
            drop(db);
            return Err(SessionError::new(
                SessionErrorKind::Storage,
                format!("Failed to initialize SQLite session database {path}: {error}"),
            ));
        }
        Ok(db)
    }

    pub(crate) fn register_storage(&self, storage: &Arc<SqliteSessionStorage>) {
        self.active_storages.lock().unwrap().push(Arc::clone(storage));
    }

    pub(crate) fn unregister_storage(&self, storage: &Arc<SqliteSessionStorage>) {
        self.active_storages
            .lock()
            .unwrap()
            .retain(|candidate| !Arc::ptr_eq(candidate, storage));
    }

    pub(crate) fn find_active_storage(&self, session_id: &str) -> Option<Arc<SqliteSessionStorage>> {
        let storages = self.active_storages.lock().unwrap();
        for storage in storages.iter() {
            if storage.is_for_session(session_id) {
                return Some(Arc::clone(storage));
            }
        }
        None
    }

    pub(crate) async fn release_storages_for_session(&self, session_id: &str) {
        let storages: Vec<Arc<SqliteSessionStorage>> = self
            .active_storages
            .lock()
            .unwrap()
            .iter()
            .filter(|storage| storage.is_for_session(session_id))
            .cloned()
            .collect();
        for storage in storages {
            storage.release().await;
        }
    }
}

// ---------------------------------------------------------------------------
// Entry / record decoding
// ---------------------------------------------------------------------------

fn decode_entry(row: &EntryRow) -> Result<Entry, SessionError> {
    let payload: serde_json::Value = serde_json::from_str(&row.payload).map_err(|_| {
        SessionError::new(
            SessionErrorKind::InvalidEntry,
            format!("Invalid SQLite session entry {}: failed to decode entry {}", row.id, row.id),
        )
    })?;
    let mut frame = match payload {
        serde_json::Value::Object(object) => object,
        _ => {
            return Err(SessionError::new(
                SessionErrorKind::InvalidEntry,
                format!("Invalid SQLite session entry {}: failed to decode entry {}", row.id, row.id),
            ))
        }
    };
    frame.insert("type".to_string(), serde_json::json!(row.entry_type));
    frame.insert("id".to_string(), serde_json::json!(row.id));
    frame.insert("seq".to_string(), serde_json::json!(row.seq));
    frame.insert("parentId".to_string(), serde_json::json!(row.parent_id));
    frame.insert("timestamp".to_string(), serde_json::json!(row.timestamp));
    serde_json::from_value(serde_json::Value::Object(frame)).map_err(|error| {
        SessionError::new(
            SessionErrorKind::InvalidEntry,
            format!("Invalid SQLite session entry {}: failed to decode entry {}: {error}", row.id, row.id),
        )
    })
}

fn decode_record(row: &RecordRow) -> Result<LaneRecord, SessionError> {
    let new_record: NewRecord = serde_json::from_str(&row.payload).map_err(|_| {
        SessionError::new(
            SessionErrorKind::Storage,
            format!("Invalid SQLite session record at sequence {}: failed to decode payload", row.seq),
        )
    })?;
    Ok(new_record_complete(new_record, row.seq as u64, row.timestamp as u64))
}

/// Completes a provisioning-time record with storage-assigned seq/timestamp,
/// normalizing optional fields exactly like upstream's committed shape (port
/// of pi-agent's `new_record_complete`).
fn new_record_complete(new_record: NewRecord, seq: u64, timestamp: u64) -> LaneRecord {
    use pi_agent::session::types::NewRecord::*;
    match new_record {
        OperationStarted { id, lane, source_leaf_id, intent } => LaneRecord::OperationStarted {
            id, seq, lane, timestamp, source_leaf_id, intent,
        },
        AbortRequested { id, lane, run_id } => LaneRecord::AbortRequested { id, seq, lane, timestamp, run_id },
        OperationFinished { id, lane, run_id, outcome, error } => {
            LaneRecord::OperationFinished { id, seq, lane, timestamp, run_id, outcome, error }
        }
        StepAttempt { id, lane, run_id, step, attempt, result_entry_id, compaction_reason } => {
            LaneRecord::StepAttempt {
                id, seq, lane, timestamp, run_id, step, attempt, result_entry_id, compaction_reason,
            }
        }
        ToolStarted {
            id, lane, run_id, assistant_entry_id, tool_index, tool_call_id, tool_name, effective_args,
            result_entry_id, replay,
        } => LaneRecord::ToolStarted {
            id, seq, lane, timestamp, run_id, assistant_entry_id, tool_index, tool_call_id, tool_name,
            effective_args, result_entry_id, replay,
        },
        QueueEnqueued { id, lane, queue, run_id, target } => {
            LaneRecord::QueueEnqueued { id, seq, lane, timestamp, queue, run_id, target }
        }
        QueueCancelled { id, lane, entry_id } => LaneRecord::QueueCancelled { id, seq, lane, timestamp, entry_id },
        WriteDeferred { id, lane, run_id, target } => {
            LaneRecord::WriteDeferred { id, seq, lane, timestamp, run_id, target }
        }
        Usage { id, lane, cause, run_id, entry_id, attempt, stop_reason, tool_call_id, details, usage } => {
            LaneRecord::Usage {
                id, seq, lane, timestamp, cause,
                run_id: run_id.unwrap_or_default(),
                entry_id: entry_id.unwrap_or_default(),
                attempt: attempt.unwrap_or(0),
                stop_reason,
                tool_call_id,
                details,
                usage,
            }
        }
    }
}

fn record_run_id(record: &NewRecord) -> Option<String> {
    match record {
        NewRecord::OperationStarted { id, .. } => Some(id.clone()),
        NewRecord::AbortRequested { run_id, .. }
        | NewRecord::OperationFinished { run_id, .. }
        | NewRecord::StepAttempt { run_id, .. }
        | NewRecord::ToolStarted { run_id, .. }
        | NewRecord::QueueEnqueued { run_id, .. } => Some(run_id.clone()),
        // `runId` is optional on usage records (upstream `"runId" in record`).
        NewRecord::Usage { run_id, .. } => run_id.clone(),
        NewRecord::QueueCancelled { .. } | NewRecord::WriteDeferred { .. } => None,
    }
}

fn record_op_kind(record: &NewRecord) -> Option<&'static str> {
    match record {
        NewRecord::OperationStarted { intent, .. } => match intent {
            pi_agent::session::types::OperationIntent::Run { .. } => Some("run"),
            pi_agent::session::types::OperationIntent::Compaction { .. } => Some("compaction"),
            pi_agent::session::types::OperationIntent::Navigation { .. } => Some("navigation"),
        },
        _ => None,
    }
}

fn require_session_row(db: &Connection, session_id: &str) -> Result<SessionRow, SessionError> {
    read_session_row(db, session_id)
        .map_err(|error| session_error(SessionErrorKind::Storage, format!("Failed to read session: {error}")))?
        .ok_or_else(|| session_error(SessionErrorKind::NotFound, format!("Session not found: {session_id}")))
}

pub(crate) fn entry_type_of(entry: &Entry) -> &'static str {
    match entry {
        Entry::Message { .. } => "message",
        Entry::ModelChange { .. } => "model_change",
        Entry::ThinkingLevel { .. } => "thinking_level_change",
        Entry::ActiveTools { .. } => "active_tools_change",
        Entry::Compaction { .. } => "compaction",
        Entry::BranchSummary { .. } => "branch_summary",
        Entry::Custom { .. } => "custom",
    }
}

pub(crate) fn custom_type_of(entry: &Entry) -> Option<&str> {
    match entry {
        Entry::Custom { custom_type, .. } => Some(custom_type),
        _ => None,
    }
}

fn matches_entry_query(entry: &Entry, query: &EntryQuery) -> bool {
    let type_matches = match &query.entry_type {
        Some(expected) => entry_type_of(entry) == expected,
        None => true,
    };
    let custom_matches = match &query.custom_type {
        Some(expected) => entry_type_of(entry) == "custom" && custom_type_of(entry) == Some(expected.as_str()),
        None => true,
    };
    let cursor_matches = match query.cursor {
        Some(cursor) => match query.order {
            Some(EntryOrder::OldestFirst) => entry.seq() > cursor.after_seq,
            _ => entry.seq() < cursor.after_seq,
        },
        None => true,
    };
    type_matches && custom_matches && cursor_matches
}

fn assert_unused_id(db: &Connection, session_id: &str, id: &str) -> Result<(), SessionError> {
    let in_entries = id_exists_in_entries(db, session_id, id)
        .map_err(|error| session_error(SessionErrorKind::Storage, format!("Failed to check id: {error}")))?;
    let in_records = id_exists_in_records(db, session_id, id)
        .map_err(|error| session_error(SessionErrorKind::Storage, format!("Failed to check id: {error}")))?;
    if in_entries || in_records {
        return Err(session_error(SessionErrorKind::AlreadyExists, format!("ID already exists: {id}")));
    }
    Ok(())
}

/// `validateCachedBranchRows` — verifies a returned cached path is a
/// contiguous root-to-tip chain unless filters/limits make that check unsafe.
fn validate_cached_branch_rows(
    rows: &[CachedBranchEntryRow],
    query: &EntryQuery,
    bounds: &pi_agent::session::state::BranchBounds,
) -> Result<(), SessionError> {
    if rows.is_empty() || query.entry_type.is_some() || query.custom_type.is_some() {
        return Ok(());
    }
    let mut path = rows.to_vec();
    path.sort_by_key(|row| row.entry_seq);
    let should_include_root = bounds.stop_at_id.is_none()
        && bounds.stop_at_type.is_none()
        && query.cursor.is_none()
        && (query.order != Some(EntryOrder::OldestFirst) || query.limit.is_none());
    if should_include_root && path[0].parent_id.is_some() {
        return Err(session_error(
            SessionErrorKind::InvalidEntry,
            format!("Entry {} not found", path[0].parent_id.as_deref().unwrap_or_default()),
        ));
    }
    for index in 1..path.len() {
        let previous = &path[index - 1];
        let current = &path[index];
        if current.parent_id.as_deref() != Some(previous.id.as_str()) {
            return Err(session_error(
                SessionErrorKind::InvalidEntry,
                format!("Entry {} not found", current.parent_id.as_deref().unwrap_or_default()),
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Writer leases
// ---------------------------------------------------------------------------

fn claim_writer_lease(
    db: &Connection,
    session_id: &str,
    ttl_ms: u64,
) -> Result<WriterLease, SessionError> {
    let now = now_ms();
    let lease = acquire_writer_lease(db, session_id, &new_id(), now, now + ttl_ms as i64)
        .map_err(|error| session_error(SessionErrorKind::Storage, format!("Failed to claim writer lease: {error}")))?;
    lease.ok_or_else(|| active_writer_error(session_id))
}

fn claim_storage(
    repo: &Arc<RepoState>,
    db: &mut Connection,
    metadata: &SqliteSessionMetadata,
) -> Result<Arc<SqliteSessionStorage>, SessionError> {
    require_session_row(db, &metadata.id)?;
    let ttl = repo.lease_options().ttl_ms;
    let claimed = transaction(db, |tx| -> Result<(WriterLease, SessionRow), SessionError> {
        let lease = claim_writer_lease(tx, &metadata.id, ttl)?;
        let row = require_session_row(tx, &metadata.id)?;
        read_lanes(tx, &metadata.id).map_err(|e| SessionError::new(e.kind, e.message))?;
        Ok((lease, row))
    })?;
    let decoded = decode_session_metadata(&claimed.1, &absolute_path(&repo.database_path))
        .map_err(|e| SessionError::new(e.kind, e.message))?;
    Ok(SqliteSessionStorage::new(repo.clone(), decoded, claimed.0))
}

// ---------------------------------------------------------------------------
// SqliteSessionStorage
// ---------------------------------------------------------------------------

/// Storage handle for one opened session. Writes are serialized and verified
/// against a per-session writer lease (mirror of `SqliteSessionStorage`).
pub struct SqliteSessionStorage {
    repo: Arc<RepoState>,
    metadata: Mutex<SqliteSessionMetadata>,
    lease: Mutex<WriterLease>,
    lease_error: Mutex<Option<SessionError>>,
    closing: AtomicBool,
    heartbeat: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl SqliteSessionStorage {
    fn new(repo: Arc<RepoState>, metadata: SqliteSessionMetadata, lease: WriterLease) -> Arc<Self> {
        let storage = Arc::new(Self {
            repo,
            metadata: Mutex::new(metadata),
            lease: Mutex::new(lease),
            lease_error: Mutex::new(None),
            closing: AtomicBool::new(false),
            heartbeat: Mutex::new(None),
        });
        storage.schedule_heartbeat();
        storage
    }

    pub(crate) fn is_for_session(&self, session_id: &str) -> bool {
        self.metadata.lock().unwrap().id == session_id
    }

    pub(crate) fn session_id(&self) -> String {
        self.metadata.lock().unwrap().id.clone()
    }

    fn path(&self) -> String {
        absolute_path(&self.repo.database_path)
    }

    /// Releases the writer lease and stops the heartbeat. Idempotent.
    pub async fn release(self: &Arc<Self>) {
        if self.closing.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(handle) = self.heartbeat.lock().unwrap().take() {
            handle.abort();
        }
        let repo = self.repo.clone();
        let session_id = self.session_id();
        let lease = self.lease.lock().unwrap().clone();
        let _ = repo
            .with_db(move |db| {
                release_writer_lease(db, &session_id, &lease)
                    .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                Ok(())
            })
            .await;
        repo.unregister_storage(self);
    }

    fn schedule_heartbeat(self: &Arc<Self>) {
        if self.closing.load(Ordering::SeqCst) || self.lease_error.lock().unwrap().is_some() {
            return;
        }
        let storage = Arc::downgrade(self);
        let interval_ms = self.repo.lease_options().heartbeat_interval_ms;
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(interval_ms));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let Some(storage) = storage.upgrade() else { break };
                storage.heartbeat_once().await;
                if storage.is_done() {
                    break;
                }
            }
        });
        *self.heartbeat.lock().unwrap() = Some(handle);
    }

    fn is_done(&self) -> bool {
        self.closing.load(Ordering::SeqCst) || self.lease_error.lock().unwrap().is_some()
    }

    async fn heartbeat_once(self: &Arc<Self>) {
        let repo = self.repo.clone();
        let session_id = self.session_id();
        let lease = self.lease.lock().unwrap().clone();
        let this = Arc::clone(self);
        let ttl = repo.lease_options().ttl_ms as i64;
        let _ = repo
            .with_db(move |db| {
                if this.closing.load(Ordering::SeqCst) || this.lease_error.lock().unwrap().is_some() {
                    return Ok(());
                }
                let now = now_ms();
                let renewed = renew_writer_lease(db, &session_id, &lease, now, now + ttl)
                    .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                if !renewed {
                    *this.lease_error.lock().unwrap() = Some(lost_writer_error(&session_id));
                }
                Ok(())
            })
            .await;
    }

    /// Runs a write operation serially: fatal-lease errors are memoized,
    /// every write renews the lease inside the same transaction, and the
    /// transaction aborts when the lease was lost (mirror of `enqueueWrite`).
    async fn enqueue_write<T>(
        self: &Arc<Self>,
        operation: impl FnOnce(&Connection) -> Result<T, SessionError> + Send,
    ) -> Result<T, SessionError>
    where
        T: Send,
    {
        if self.closing.load(Ordering::SeqCst) {
            return Err(session_error(
                SessionErrorKind::Storage,
                format!("SQLite session {} is closed", self.session_id()),
            ));
        }
        let repo = self.repo.clone();
        let session_id = self.session_id();
        let lease = self.lease.lock().unwrap().clone();
        let this = Arc::clone(self);
        let ttl = repo.lease_options().ttl_ms as i64;
        repo.with_db(move |db| {
            if this.closing.load(Ordering::SeqCst) {
                return Err(session_error(
                    SessionErrorKind::Storage,
                    format!("SQLite session {session_id} is closed"),
                ));
            }
            if let Some(error) = this.lease_error.lock().unwrap().clone() {
                return Err(error);
            }
            transaction(db, |tx| {
                if let Some(error) = this.lease_error.lock().unwrap().clone() {
                    return Err(error);
                }
                let now = now_ms();
                let renewed = renew_writer_lease(tx, &session_id, &lease, now, now + ttl)
                    .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                if !renewed {
                    *this.lease_error.lock().unwrap() = Some(lost_writer_error(&session_id));
                    return Err(lost_writer_error(&session_id));
                }
                operation(tx)
            })
        })
        .await
    }

    pub(crate) async fn get_metadata(&self) -> Result<SqliteSessionMetadata, SessionError> {
        let session_id = self.session_id();
        let path = self.path();
        let repo = self.repo.clone();
        repo.with_db(move |db| {
            let row = require_session_row(db, &session_id)?;
            decode_session_metadata(&row, &path).map_err(|e| SessionError::new(e.kind, e.message))
        })
        .await
    }

    pub(crate) async fn get_lanes(&self) -> Result<Vec<LanePointer>, SessionError> {
        let session_id = self.session_id();
        let repo = self.repo.clone();
        repo.with_db(move |db| {
            read_lanes(db, &session_id).map(|lanes| {
                lanes
                    .into_iter()
                    .map(|row| LanePointer { lane: row.lane, leaf_id: row.leaf_id })
                    .collect()
            })
        })
        .await
    }

    pub(crate) async fn create_lane(self: &Arc<Self>, lane: &str, at: Option<&str>) -> Result<(), SessionError> {
        let session_id = self.session_id();
        let lane = lane.to_string();
        let at = at.map(|s| s.to_string());
        self.enqueue_write(move |db| {
            if read_lane(db, &session_id, &lane)
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?
                .is_some()
            {
                return Err(session_error(SessionErrorKind::AlreadyExists, format!("Lane already exists: {lane}")));
            }
            if let Some(at) = &at {
                if read_entry_row(db, &session_id, at)
                    .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?
                    .is_none()
                {
                    return Err(session_error(SessionErrorKind::NotFound, format!("Entry not found: {at}")));
                }
            }
            let seq = get_next_sequence(db, &session_id)?;
            create_lane(db, &session_id, seq, &lane, at.as_deref())
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
            advance_sequence(db, &session_id, seq)
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))
        })
        .await
    }

    pub(crate) async fn move_lane(self: &Arc<Self>, lane: &str, to: Option<&str>) -> Result<(), SessionError> {
        let session_id = self.session_id();
        let lane = lane.to_string();
        let to = to.map(|s| s.to_string());
        self.enqueue_write(move |db| {
            if read_lane(db, &session_id, &lane)
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?
                .is_none()
            {
                return Err(session_error(SessionErrorKind::InvalidLane, format!("Lane not found: {lane}")));
            }
            if let Some(to) = &to {
                if read_entry_row(db, &session_id, to)
                    .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?
                    .is_none()
                {
                    return Err(session_error(SessionErrorKind::NotFound, format!("Entry not found: {to}")));
                }
            }
            let seq = get_next_sequence(db, &session_id)?;
            move_lane(db, &session_id, seq, &lane, to.as_deref())?;
            advance_sequence(db, &session_id, seq)
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))
        })
        .await
    }

    pub(crate) async fn append_entry(self: &Arc<Self>, entry: EntryNoStats, lane: &str) -> Result<Entry, SessionError> {
        let session_id = self.session_id();
        let lane = lane.to_string();
        self.enqueue_write(move |db| {
            let parent_id = read_lane_head(db, &session_id, &lane)?.map(|leaf| leaf.to_string());
            assert_unused_id(db, &session_id, entry.id())?;
            let seq = get_next_sequence(db, &session_id)?;
            let id = entry.id().to_string();
            let entry_type = entry_type_of_no_stats(&entry).to_string();
            let custom_type = custom_type_of_no_stats(&entry).map(|s| s.to_string());
            let committed = commit_entry(db, &session_id, entry, parent_id.as_deref(), seq, now_ms())?;
            set_lane_leaf(db, &session_id, &lane, &id)?;
            append_entry_to_branch_cache(db, &session_id, &id, seq, &entry_type, custom_type.as_deref(), parent_id.as_deref())?;
            if entry_type == "message" {
                increment_message_count(db, &session_id)?;
            }
            advance_sequence(db, &session_id, seq)
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
            Ok(committed)
        })
        .await
    }

    pub(crate) async fn append_record(self: &Arc<Self>, record: NewRecord) -> Result<LaneRecord, SessionError> {
        let session_id = self.session_id();
        let record_str = serde_json::to_string(&record)
            .map_err(|error| session_error(SessionErrorKind::Storage, format!("Failed to serialize record: {error}")))?;
        let record_id = record.id().to_string();
        let lane = record.lane().to_string();
        let record_type = record.record_type().to_string();
        let run_id = record_run_id(&record);
        let op_kind = record_op_kind(&record).map(|s| s.to_string());
        let usage = match &record {
            NewRecord::Usage { usage, .. } => Some(usage.clone()),
            _ => None,
        };
        self.enqueue_write(move |db| {
            if read_lane(db, &session_id, &lane)
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?
                .is_none()
            {
                return Err(session_error(SessionErrorKind::InvalidLane, format!("Lane not found: {lane}")));
            }
            assert_unused_id(db, &session_id, &record_id)?;
            let seq = get_next_sequence(db, &session_id)?;
            let timestamp = now_ms();
            if record_type == "operation_started" {
                start_lane_operation(db, &session_id, &lane, &record_id)?;
            }
            append_record_row(
                db,
                &session_id,
                &NewRecordRow {
                    seq,
                    id: record_id.clone(),
                    lane: lane.clone(),
                    run_id: run_id.clone(),
                    record_type: record_type.clone(),
                    op_kind: op_kind.clone(),
                    timestamp,
                    payload: record_str.clone(),
                },
            )
            .map_err(|error| session_error(SessionErrorKind::Storage, format!("Failed to append record: {error}")))?;
            if record_type == "operation_finished" {
                finish_lane_operation(db, &session_id, &lane, run_id.as_deref())
                    .map_err(|error| session_error(SessionErrorKind::Storage, format!("Failed to finish operation: {error}")))?;
            }
            if record_type == "usage" {
                if let Some(usage) = &usage {
                    add_usage_to_stats(db, &session_id, usage)?;
                }
            }
            advance_sequence(db, &session_id, seq)
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
            let record: NewRecord = serde_json::from_str(&record_str)
                .map_err(|error| session_error(SessionErrorKind::Storage, format!("Failed to decode committed record: {error}")))?;
            Ok(new_record_complete(record, seq as u64, timestamp as u64))
        })
        .await
    }

    pub(crate) async fn get_entry(&self, id: &str) -> Result<Option<Entry>, SessionError> {
        let session_id = self.session_id();
        let id = id.to_string();
        let repo = self.repo.clone();
        repo.with_db(move |db| {
            let row = read_entry_row(db, &session_id, &id)
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
            row.map(|row| decode_entry(&row)).transpose()
        })
        .await
    }

    pub(crate) async fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, SessionError> {
        let session_id = self.session_id();
        let query = query.clone();
        let repo = self.repo.clone();
        repo.with_db(move |db| {
            let sql_type = query
                .entry_type
                .clone()
                .or_else(|| if query.custom_type.is_none() { None } else { Some("custom".to_string()) });
            let sql_limit = if query.custom_type.is_none() { query.limit.map(|l| l as i64) } else { None };
            let rows = read_entry_rows(
                db,
                &session_id,
                ReadEntryRowsOptions {
                    after_seq: None,
                    cursor: query.cursor,
                    entry_type: sql_type.as_deref(),
                    order: query.order,
                    limit: sql_limit,
                },
            )
            .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
            let mut entries = Vec::new();
            for row in rows {
                let entry = decode_entry(&row)?;
                if matches_entry_query(&entry, &query) {
                    entries.push(entry);
                }
            }
            if let Some(limit) = query.limit {
                entries.truncate(limit);
            }
            Ok(entries)
        })
        .await
    }

    pub(crate) async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &pi_agent::session::state::BranchBounds,
        start: &str,
    ) -> Result<Vec<Entry>, SessionError> {
        let session_id = self.session_id();
        let query = query.clone();
        let bounds = bounds.clone();
        let start = start.to_string();
        let repo = self.repo.clone();
        repo.with_db(move |db| {
            let cached = read_cached_branch(db, &session_id, &start)
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
            let cached = match cached {
                Some(cached) => cached,
                None => {
                    if read_entry_row(db, &session_id, &start)
                        .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?
                        .is_none()
                    {
                        return Err(session_error(SessionErrorKind::NotFound, format!("Entry not found: {start}")));
                    }
                    return Err(session_error(SessionErrorKind::InvalidEntry, format!("Branch cache missing entry {start}")));
                }
            };
            let rows = query_cached_branch_rows(
                db,
                &session_id,
                &cached,
                &CachedBranchQuery {
                    entry_type: query.entry_type.clone(),
                    custom_type: query.custom_type.clone(),
                    stop_at_type: bounds.stop_at_type.clone(),
                    stop_at_id: bounds.stop_at_id.clone(),
                    cursor: query.cursor,
                    order: query.order,
                    limit: query.limit.map(|l| l as i64),
                },
            )
            .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
            validate_cached_branch_rows(&rows, &query, &bounds)?;
            let mut entries = Vec::new();
            for row in rows {
                let row = entry_row_from_cached(row);
                let entry = decode_entry(&row)?;
                if matches_entry_query(&entry, &query) {
                    entries.push(entry);
                }
            }
            if let Some(limit) = query.limit {
                entries.truncate(limit);
            }
            Ok(entries)
        })
        .await
    }

    pub(crate) async fn find_records(&self, query: &RecordQuery) -> Result<Vec<LaneRecord>, SessionError> {
        let session_id = self.session_id();
        let query = query.clone();
        let repo = self.repo.clone();
        repo.with_db(move |db| {
            let rows = read_record_rows(
                db,
                &session_id,
                &ReadRecordRowsOptions {
                    lane: query.lane.as_deref(),
                    record_type: query.record_type.as_deref(),
                    run_id: query.run_id.as_deref(),
                    operation_kind: query.operation_kind.as_deref(),
                    after_seq: query.after_seq.map(|s| s as i64),
                    order: query.order,
                    limit: query.limit.map(|l| l as i64),
                },
            )
            .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
            rows.iter().map(decode_record).collect()
        })
        .await
    }

    pub(crate) async fn find_open_operations(
        &self,
        lane: &str,
        _options: Option<usize>,
    ) -> Result<Vec<LaneRecord>, SessionError> {
        let session_id = self.session_id();
        let lane = lane.to_string();
        let repo = self.repo.clone();
        repo.with_db(move |db| {
            let rows = read_open_operation_rows(db, &session_id, &lane)?;
            rows.iter()
                .map(decode_record)
                .map(|record| {
                    record.and_then(|record| {
                        if record.record_type() != "operation_started" {
                            Err(session_error(
                                SessionErrorKind::Storage,
                                "Expected operation_started record",
                            ))
                        } else {
                            Ok(record)
                        }
                    })
                })
                .collect()
        })
        .await
    }

    pub(crate) async fn get_log(&self, options: &LogOptions) -> Result<Vec<LogItem>, SessionError> {
        let session_id = self.session_id();
        let options = options.clone();
        let repo = self.repo.clone();
        repo.with_db(move |db| {
            let after_seq = options.after_seq.unwrap_or(0) as i64;
            let limit = options.limit.map(|l| l as i64);
            let entry_rows = read_entry_rows(
                db,
                &session_id,
                ReadEntryRowsOptions {
                    after_seq: Some(after_seq),
                    cursor: None,
                    entry_type: None,
                    order: Some(EntryOrder::OldestFirst),
                    limit,
                },
            )
            .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
            let record_rows = read_record_rows(
                db,
                &session_id,
                &ReadRecordRowsOptions {
                    after_seq: Some(after_seq),
                    order: Some(EntryOrder::OldestFirst),
                    limit,
                    ..Default::default()
                },
            )
            .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
            let lane_rows = read_lane_move_rows(db, &session_id, Some(after_seq), limit)
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
            let fact_rows = read_fact_rows(db, &session_id, Some(after_seq), limit)
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;

            // Selection happens BEFORE decoding (mirror of upstream): the
            // rows are merged by sequence, trimmed, and only then decoded.
            #[derive(Clone)]
            enum LogSource {
                Entry(EntryRow),
                Record(RecordRow),
                Lane(LaneMoveRow),
                Fact(crate::storage::facts::FactRow),
            }
            let mut log_rows: Vec<(i64, LogSource)> = Vec::new();
            for row in entry_rows {
                log_rows.push((row.seq, LogSource::Entry(row)));
            }
            for row in record_rows {
                log_rows.push((row.seq, LogSource::Record(row)));
            }
            for row in lane_rows {
                log_rows.push((row.seq, LogSource::Lane(row)));
            }
            for row in fact_rows {
                log_rows.push((row.seq, LogSource::Fact(row)));
            }
            log_rows.sort_by_key(|(seq, _)| *seq);
            if let Some(limit) = options.limit {
                log_rows.truncate(limit);
            }
            let mut items = Vec::with_capacity(log_rows.len());
            for (_, source) in log_rows {
                match source {
                    LogSource::Entry(row) => items.push(LogItem::Entry(decode_entry(&row)?)),
                    LogSource::Record(row) => items.push(LogItem::Record(decode_record(&row)?)),
                    LogSource::Lane(row) => {
                        items.push(LogItem::Lane { seq: row.seq as u64, lane: row.lane, leaf_id: row.leaf_id })
                    }
                    LogSource::Fact(row) => {
                        if row.kind == "name" {
                            let name = row.value.as_deref().and_then(|v| serde_json::from_str::<String>(v).ok());
                            items.push(LogItem::Fact(pi_agent::session::types::FactLogItem {
                                seq: row.seq as u64,
                                fact: "name".to_string(),
                                name,
                                target_id: None,
                                label: None,
                            }));
                        } else {
                            let label = row.value.as_deref().and_then(|v| serde_json::from_str::<String>(v).ok());
                            items.push(LogItem::Fact(pi_agent::session::types::FactLogItem {
                                seq: row.seq as u64,
                                fact: "label".to_string(),
                                name: None,
                                target_id: row.key.clone(),
                                label,
                            }));
                        }
                    }
                }
            }
            Ok(items)
        })
        .await
    }

    pub(crate) async fn get_name(&self) -> Result<Option<String>, SessionError> {
        let session_id = self.session_id();
        let repo = self.repo.clone();
        repo.with_db(move |db| {
            let row = read_latest_fact(db, &session_id, "name", None)
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
            match row.and_then(|row| row.value) {
                Some(value) => serde_json::from_str(&value)
                    .map(Some)
                    .map_err(|error| session_error(SessionErrorKind::Storage, format!("Invalid stored name: {error}"))),
                None => Ok(None),
            }
        })
        .await
    }

    pub(crate) async fn set_name(self: &Arc<Self>, name: Option<&str>) -> Result<(), SessionError> {
        let session_id = self.session_id();
        let name = name.map(|s| s.to_string());
        self.enqueue_write(move |db| {
            let seq = get_next_sequence(db, &session_id)?;
            let value = name.as_deref().map(|n| serde_json::to_string(n).expect("string serializes"));
            append_fact(db, &session_id, seq, "name", None, value.as_deref())
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
            advance_sequence(db, &session_id, seq)
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))
        })
        .await
    }

    pub(crate) async fn get_label(&self, id: &str) -> Result<Option<String>, SessionError> {
        let session_id = self.session_id();
        let id = id.to_string();
        let repo = self.repo.clone();
        repo.with_db(move |db| {
            let row = read_latest_fact(db, &session_id, "label", Some(&id))
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
            match row.and_then(|row| row.value) {
                Some(value) => serde_json::from_str(&value)
                    .map(Some)
                    .map_err(|error| session_error(SessionErrorKind::Storage, format!("Invalid stored label: {error}"))),
                None => Ok(None),
            }
        })
        .await
    }

    pub(crate) async fn set_label(self: &Arc<Self>, id: &str, label: Option<&str>) -> Result<(), SessionError> {
        let session_id = self.session_id();
        let id = id.to_string();
        let label = label.map(|s| s.to_string());
        self.enqueue_write(move |db| {
            if read_entry_row(db, &session_id, &id)
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?
                .is_none()
            {
                return Err(session_error(SessionErrorKind::NotFound, format!("Entry not found: {id}")));
            }
            let seq = get_next_sequence(db, &session_id)?;
            let value = label.as_deref().map(|l| serde_json::to_string(l).expect("string serializes"));
            append_fact(db, &session_id, seq, "label", Some(&id), value.as_deref())
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
            advance_sequence(db, &session_id, seq)
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))
        })
        .await
    }

    pub(crate) async fn get_stats(&self) -> Result<SessionStats, SessionError> {
        let session_id = self.session_id();
        let repo = self.repo.clone();
        repo.with_db(move |db| read_stats(db, &session_id)).await
    }
}

// ---------------------------------------------------------------------------
// Entry helpers
// ---------------------------------------------------------------------------

fn entry_type_of_no_stats(entry: &EntryNoStats) -> &'static str {
    match entry {
        EntryNoStats::Message { .. } => "message",
        EntryNoStats::ModelChange { .. } => "model_change",
        EntryNoStats::ThinkingLevel { .. } => "thinking_level_change",
        EntryNoStats::ActiveTools { .. } => "active_tools_change",
        EntryNoStats::Compaction { .. } => "compaction",
        EntryNoStats::BranchSummary { .. } => "branch_summary",
        EntryNoStats::Custom { .. } => "custom",
    }
}

fn custom_type_of_no_stats(entry: &EntryNoStats) -> Option<&str> {
    match entry {
        EntryNoStats::Custom { custom_type, .. } => Some(custom_type),
        _ => None,
    }
}

/// Converts a provisioned entry to its committed form (with parent/seq/
/// timestamp), serializes the payload, and persists the row. Mirrors the
/// `insertEntryRow` step of upstream `appendEntry`.
fn commit_entry(
    db: &Connection,
    session_id: &str,
    entry: EntryNoStats,
    parent_id: Option<&str>,
    seq: i64,
    timestamp: i64,
) -> Result<Entry, SessionError> {
    let committed = provisioned_to_entry(entry, parent_id, seq as u64, timestamp as u64)?;
    let entry_type = entry_type_of(&committed).to_string();
    let payload = serialize_payload(&committed);
    insert_entry_row(
        db,
        session_id,
        &NewEntryRow {
            seq,
            id: committed.id().to_string(),
            parent_id: parent_id.map(|s| s.to_string()),
            entry_type,
            timestamp,
            payload,
        },
    )
    .map_err(|error| session_error(SessionErrorKind::Storage, format!("Failed to insert entry: {error}")))?;
    Ok(committed)
}

fn provisioned_to_entry(
    entry: EntryNoStats,
    parent_id: Option<&str>,
    seq: u64,
    timestamp: u64,
) -> Result<Entry, SessionError> {
    let entry = match entry {
        EntryNoStats::Message { id, message, terminate } => Entry::Message {
            id,
            seq,
            parent_id: parent_id.map(|s| s.to_string()),
            timestamp,
            message,
            terminate,
        },
        EntryNoStats::ModelChange { id, provider, model_id } => Entry::ModelChange {
            id,
            seq,
            parent_id: parent_id.map(|s| s.to_string()),
            timestamp,
            provider,
            model_id,
        },
        EntryNoStats::ThinkingLevel { id, thinking_level } => Entry::ThinkingLevel {
            id,
            seq,
            parent_id: parent_id.map(|s| s.to_string()),
            timestamp,
            thinking_level,
        },
        EntryNoStats::ActiveTools { id, active_tool_names } => Entry::ActiveTools {
            id,
            seq,
            parent_id: parent_id.map(|s| s.to_string()),
            timestamp,
            active_tool_names,
        },
        EntryNoStats::Compaction { id, summary, retained_tail, tokens_before, details, usage } => Entry::Compaction {
            id,
            seq,
            parent_id: parent_id.map(|s| s.to_string()),
            timestamp,
            summary,
            retained_tail,
            tokens_before,
            details,
            usage,
        },
        EntryNoStats::BranchSummary { id, from_id, summary, details, usage } => Entry::BranchSummary {
            id,
            seq,
            parent_id: parent_id.map(|s| s.to_string()),
            timestamp,
            from_id,
            summary,
            details,
            usage,
        },
        EntryNoStats::Custom { id, custom_type, data } => Entry::Custom {
            id,
            seq,
            parent_id: parent_id.map(|s| s.to_string()),
            timestamp,
            custom_type,
            data,
        },
    };
    Ok(entry)
}

// ---------------------------------------------------------------------------
// SqliteSessionRepository
// ---------------------------------------------------------------------------

/// SQLite-backed session repository (port of `SqliteSessionRepository`).
pub struct SqliteSessionRepository {
    state: Arc<RepoState>,
    lease_options: SqliteWriterLeaseOptions,
}

impl SqliteSessionRepository {
    pub fn new(database_path: impl Into<String>, lease_options: Option<SqliteWriterLeaseOptions>) -> Self {
        let lease_options = lease_options.unwrap_or_default();
        if lease_options.ttl_ms == 0 {
            panic!("writerLease.ttlMs must be positive");
        }
        if lease_options.heartbeat_interval_ms == 0 || lease_options.heartbeat_interval_ms >= lease_options.ttl_ms {
            panic!("writerLease.heartbeatIntervalMs must be positive and less than ttlMs");
        }
        Self {
            state: RepoState::new(database_path.into(), lease_options),
            lease_options,
        }
    }

    pub fn lease_options(&self) -> &SqliteWriterLeaseOptions {
        &self.lease_options
    }

    pub async fn create(&self, options: &SqliteSessionCreateOptions) -> Result<crate::session::SqliteSession, SessionError> {
        let state = self.state.clone();
        let options = options.clone();
        let state_inner = Arc::clone(&state);
        let path = absolute_path(&state_inner.database_path);
        let id = options.id.clone().unwrap_or_else(new_id);
        let ttl = state.lease_options().ttl_ms;
        state.with_db(move |db| {
            if session_exists(db, &id)
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?
            {
                return Err(session_error(SessionErrorKind::AlreadyExists, format!("Session already exists: {id}")));
            }
            let created_at = now_ms();
            let metadata = options.metadata.clone();
            let serialized = serialize_metadata_option(&metadata)?;
            let lease = transaction(db, |tx| {
                insert_session_row(
                    tx,
                    &NewSessionRow {
                        id: id.clone(),
                        created_at,
                        cwd: options.cwd.clone(),
                        parent_session_id: options.parent_session_id.clone(),
                        metadata: serialized.clone(),
                    },
                )
                .map_err(|error| session_error(SessionErrorKind::Storage, format!("Failed to insert session: {error}")))?;
                create_sequence(tx, &id, 1)
                    .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                create_stats(tx, &id, 0)
                    .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                create_initial_lane(tx, &id, "main", None)
                    .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                claim_writer_lease(tx, &id, ttl)
            })?;
            let row = require_session_row(db, &id)?;
            let decoded = decode_session_metadata(&row, &path).map_err(|e| SessionError::new(e.kind, e.message))?;
            let storage = SqliteSessionStorage::new(state_inner.clone(), decoded, lease);
            state_inner.register_storage(&storage);
            Ok(crate::session::SqliteSession::new(storage))
        })
        .await
    }

    pub async fn open(&self, metadata: &SqliteSessionMetadata) -> Result<crate::session::SqliteSession, SessionError> {
        let state = self.state.clone();
        let state_inner = Arc::clone(&state);
        let metadata = metadata.clone();
        state.with_db(move |db| self_claim_session(&state_inner, db, &metadata)).await
    }

    /// Rebuilds this session's private branch-read cache from canonical entry
    /// parent links.
    pub async fn repair_branch_cache(&self, metadata: &SqliteSessionMetadata) -> Result<(), SessionError> {
        let state = self.state.clone();
        let state_inner = Arc::clone(&state);
        let metadata = metadata.clone();
        state.release_storages_for_session(&metadata.id).await;
        state
            .with_db(move |db| {
                transaction(db, |tx| {
                    let lease = claim_writer_lease(tx, &metadata.id, state_inner.lease_options().ttl_ms)?;
                    require_session_row(tx, &metadata.id)?;
                    rebuild_branch_cache(tx, &metadata.id)?;
                    release_writer_lease(tx, &metadata.id, &lease)
                        .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                    Ok(())
                })
            })
            .await
    }

    /// Reads the session catalog without acquiring or renewing per-session
    /// writer leases.
    pub async fn list(&self, options: &SqliteSessionListOptions) -> Result<Vec<SqliteSessionMetadata>, SessionError> {
        let state = self.state.clone();
        let state_inner = Arc::clone(&state);
        let options = options.clone();
        state.with_db(move |db| {
            let path = absolute_path(&state_inner.database_path);
            let rows = read_session_rows(db, options.cwd.as_deref())
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
            rows.iter()
                .map(|row| decode_session_metadata(row, &path).map_err(|e| SessionError::new(e.kind, e.message)))
                .collect()
        })
        .await
    }

    pub async fn delete(&self, metadata: &SqliteSessionMetadata) -> Result<(), SessionError> {
        let state = self.state.clone();
        let state_inner = Arc::clone(&state);
        let metadata = metadata.clone();
        state.release_storages_for_session(&metadata.id).await;
        state
            .with_db(move |db| {
                transaction(db, |tx| {
                    if !session_exists(tx, &metadata.id)
                        .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?
                    {
                        delete_writer_lease(tx, &metadata.id)
                            .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                        return Ok(());
                    }
                    let _lease = claim_writer_lease(tx, &metadata.id, state_inner.lease_options().ttl_ms)?;
                    delete_branch_cache(tx, &metadata.id)
                        .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                    delete_fact_rows(tx, &metadata.id)
                        .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                    delete_lane_rows(tx, &metadata.id)
                        .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                    delete_record_rows(tx, &metadata.id)
                        .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                    delete_entry_rows(tx, &metadata.id)
                        .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                    delete_writer_lease(tx, &metadata.id)
                        .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                    delete_stats(tx, &metadata.id)
                        .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                    delete_sequence(tx, &metadata.id)
                        .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                    delete_session_row(tx, &metadata.id)
                        .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                    Ok(())
                })
            })
            .await
    }

    pub async fn fork(
        &self,
        source: &SqliteSessionMetadata,
        options: &ForkCreateOptions,
    ) -> Result<crate::session::SqliteSession, SessionError> {
        let state = self.state.clone();
        let state_inner = Arc::clone(&state);
        let source = source.clone();
        let options = options.clone();
        state.with_db(move |db| {
            let path = absolute_path(&state_inner.database_path);
            let source_row = require_session_row(db, &source.id)?;
            let source_metadata = decode_session_metadata(&source_row, &path).map_err(|e| SessionError::new(e.kind, e.message))?;
            let id = options.id.clone().unwrap_or_else(new_id);
            if session_exists(db, &id)
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?
            {
                return Err(session_error(SessionErrorKind::AlreadyExists, format!("Session already exists: {id}")));
            }

            let mut entries: Vec<EntryRow> = Vec::new();
            let mut lanes: Vec<(String, Option<String>)> = Vec::new();
            let mut branch_tips: Vec<String> = Vec::new();
            let mut branch_fork_target_id: Option<String> = None;

            match &options.fork_options {
                ForkOptions::Tree => {
                    entries.extend(
                        read_entry_rows(
                            db,
                            &source.id,
                            ReadEntryRowsOptions {
                                after_seq: None,
                                cursor: None,
                                entry_type: None,
                                order: Some(EntryOrder::OldestFirst),
                                limit: None,
                            },
                        )
                        .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?,
                    );
                    lanes.extend(
                        read_lanes(db, &source.id)
                            .map_err(|e| SessionError::new(e.kind, e.message))?
                            .into_iter()
                            .map(|row| (row.lane, row.leaf_id)),
                    );
                    branch_tips = read_branch_tip_ids(db, &source.id)
                        .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                }
                ForkOptions::Branch { entry_id, position } => {
                    let main = read_lane(db, &source.id, "main")
                        .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                    let main = main.ok_or_else(|| session_error(SessionErrorKind::InvalidLane, "Lane not found: main"))?;
                    let selected_entry_id = entry_id.clone().or(main.leaf_id.clone());
                    if let Some(selected_entry_id) = selected_entry_id {
                        let target = read_entry_row(db, &source.id, &selected_entry_id)
                            .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                        let target = match target.as_ref() {
                            Some(target) if target.entry_type == "message" => target,
                            _ => {
                                return Err(session_error(
                                    SessionErrorKind::InvalidForkTarget,
                                    format!("Fork target is not a message entry: {selected_entry_id}"),
                                ))
                            }
                        };
                        let position = position.unwrap_or(if entry_id.is_none() {
                            pi_agent::session::state::ForkPosition::At
                        } else {
                            pi_agent::session::state::ForkPosition::Before
                        });
                        branch_fork_target_id = match position {
                            pi_agent::session::state::ForkPosition::At => Some(target.id.clone()),
                            pi_agent::session::state::ForkPosition::Before => target.parent_id.clone(),
                        };
                    }
                    lanes.push(("main".to_string(), branch_fork_target_id.clone()));
                    if let Some(target) = branch_fork_target_id.clone() {
                        let cached = read_cached_branch(db, &source.id, &target)
                            .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                        let cached = cached.ok_or_else(|| {
                            session_error(
                                SessionErrorKind::InvalidForkTarget,
                                format!("Fork target is not on a cached branch: {target}"),
                            )
                        })?;
                        let rows = query_cached_branch_rows(
                            db,
                            &source.id,
                            &cached,
                            &CachedBranchQuery {
                                order: Some(EntryOrder::OldestFirst),
                                ..Default::default()
                            },
                        )
                        .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                        // cached rows carry `entry_seq`; normalize into EntryRow
                        // before copying (upstream `entryRowFromCached`).
                        entries.extend(rows.into_iter().map(entry_row_from_cached));
                        branch_tips.push(target);
                    }
                }
            }

            let copied_ids: std::collections::HashSet<String> =
                entries.iter().map(|entry| entry.id.clone()).collect();
            let latest_name = read_latest_fact(db, &source.id, "name", None)
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
            let latest_labels = read_latest_label_facts(db, &source.id)
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
            let labels_to_copy: Vec<(String, String)> = latest_labels
                .into_iter()
                .filter(|(key, _)| matches!(options.fork_options, ForkOptions::Tree) || copied_ids.contains(key))
                .collect();
            let created_at = now_ms();
            let metadata = options.metadata.clone().or_else(|| source_metadata.metadata.clone());
            let serialized = serialize_metadata_option(&metadata)?;
            let lease = transaction(db, |tx| {
                insert_session_row(
                    tx,
                    &NewSessionRow {
                        id: id.clone(),
                        created_at,
                        cwd: options.cwd.clone(),
                        parent_session_id: options.parent_session_id.clone().or(Some(source.id.clone())),
                        metadata: serialized.clone(),
                    },
                )
                .map_err(|error| session_error(SessionErrorKind::Storage, format!("Failed to insert session: {error}")))?;
                create_sequence(tx, &id, 1)
                    .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                create_stats(
                    tx,
                    &id,
                    entries.iter().filter(|entry| entry.entry_type == "message").count() as i64,
                )
                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;

                let mut next_seq = 1i64;
                let mut allocate_seq = || {
                    let seq = next_seq;
                    next_seq += 1;
                    seq
                };
                for entry in entries.clone() {
                    insert_entry_row(
                        tx,
                        &id,
                        &NewEntryRow {
                            seq: allocate_seq(),
                            id: entry.id.clone(),
                            parent_id: entry.parent_id.clone(),
                            entry_type: entry.entry_type.clone(),
                            timestamp: entry.timestamp,
                            payload: entry.payload.clone(),
                        },
                    )
                    .map_err(|error| session_error(SessionErrorKind::Storage, format!("Failed to insert entry: {error}")))?;
                }

                match options.fork_options {
                    ForkOptions::Tree => {
                        for (lane, leaf_id) in &lanes {
                            create_lane(tx, &id, allocate_seq(), lane, leaf_id.as_deref())
                                .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                        }
                    }
                    ForkOptions::Branch { .. } => {
                        create_initial_lane(tx, &id, "main", branch_fork_target_id.as_deref())
                            .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                    }
                }

                if let Some(name_row) = &latest_name {
                    if let Some(value) = &name_row.value {
                        append_fact(tx, &id, allocate_seq(), "name", None, Some(value))
                            .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                    }
                }
                for (key, value) in &labels_to_copy {
                    append_fact(tx, &id, allocate_seq(), "label", Some(key), Some(value))
                        .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                }

                set_next_sequence(tx, &id, next_seq)
                    .map_err(|error| session_error(SessionErrorKind::Storage, error.to_string()))?;
                for tip in &branch_tips {
                    build_cached_branch(tx, &id, tip)?;
                }
                claim_writer_lease(tx, &id, state_inner.lease_options().ttl_ms)
            })
            .map_err(|error: SessionError| {
                if matches!(error.kind, SessionErrorKind::Storage | SessionErrorKind::InvalidForkTarget | SessionErrorKind::AlreadyExists | SessionErrorKind::InvalidLane) {
                    error
                } else {
                    SessionError::new(
                        SessionErrorKind::Storage,
                        format!("Failed to fork SQLite session {id}: {}", error.message),
                    )
                }
            })?;
            let row = require_session_row(db, &id)?;
            let decoded = decode_session_metadata(&row, &path).map_err(|e| SessionError::new(e.kind, e.message))?;
            let storage = SqliteSessionStorage::new(state_inner.clone(), decoded, lease);
            state_inner.register_storage(&storage);
            Ok(crate::session::SqliteSession::new(storage))
        })
        .await
    }

    /// Releases all active storages and closes the shared database
    /// connection. Idempotent.
    pub async fn close(&self) {
        let state = self.state.clone();
        let storages: Vec<Arc<SqliteSessionStorage>> = state
            .active_storages
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect();
        for storage in storages {
            storage.release().await;
        }
        let mut guard = state.db.lock().await;
        *guard = None;
    }
}

/// Fork options plus sqlite create options (mirror of the upstream
/// `fork(source, options: ForkOptions & SqliteSessionCreateOptions)`).
#[derive(Debug, Clone)]
pub struct ForkCreateOptions {
    pub id: Option<String>,
    pub cwd: String,
    pub parent_session_id: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub fork_options: ForkOptions,
}

impl Default for ForkCreateOptions {
    fn default() -> Self {
        Self {
            id: None,
            cwd: "/workspace".to_string(),
            parent_session_id: None,
            metadata: None,
            fork_options: ForkOptions::Branch { entry_id: None, position: None },
        }
    }
}

fn serialize_metadata_option(metadata: &Option<serde_json::Value>) -> Result<Option<String>, SessionError> {
    match metadata {
        None => Ok(None),
        Some(metadata) => {
            if !metadata.is_object() {
                return Err(session_error(
                    SessionErrorKind::InvalidPayload,
                    "SQLite session metadata must be an object",
                ));
            }
            Ok(Some(
                serde_json::to_string(metadata)
                    .map_err(|error| session_error(SessionErrorKind::InvalidPayload, format!("Metadata is not serializable: {error}")))?,
            ))
        }
    }
}

fn entry_row_from_cached(row: CachedBranchEntryRow) -> EntryRow {
    EntryRow {
        session_id: row.session_id,
        seq: row.entry_seq,
        id: row.id,
        parent_id: row.parent_id,
        entry_type: row.entry_type,
        timestamp: row.timestamp,
        payload: row.payload,
    }
}

fn self_claim_session(
    state: &Arc<RepoState>,
    db: &mut Connection,
    metadata: &SqliteSessionMetadata,
) -> Result<crate::session::SqliteSession, SessionError> {
    if let Some(active) = state.find_active_storage(&metadata.id) {
        read_lanes(db, &metadata.id).map_err(|e| SessionError::new(e.kind, e.message))?;
        return Ok(crate::session::SqliteSession::new(active));
    }
    let storage = claim_storage(state, db, metadata)?;
    state.register_storage(&storage);
    Ok(crate::session::SqliteSession::new(storage))
}
