//! Session facade — port of `packages/agent/src/harness/session/session.ts`
//! (`Session` + the `SessionTree` view surface), over either the JSONL
//! storage or the in-memory storage.

use std::sync::{Arc, Mutex};

use super::jsonl::storage::JsonlSessionStorage;
use super::memory::InMemorySessionStorage;
use super::state::{BranchBounds, EntryQuery, LogOptions, RecordQuery};
use super::types::{
    session_error, Entry, EntryNoStats, LanePointer, LaneRecord, LogItem, NewRecord, SessionError,
    SessionErrorKind, SessionMetadata, SessionStats,
};
use crate::fs::FileSystem;
use crate::types::AgentMessage;

/// Generates a session/entry id (upstream `uuidv7`).
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Storage kind behind a `Session`.
///
/// `large_enum_variant` is intentional: the JSONL variant is the common case
/// and boxing it would add an indirection to every access.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum SessionStorageKind<F: FileSystem> {
    Jsonl(JsonlSessionStorage<F>),
    InMemory(Arc<Mutex<InMemorySessionStorage>>),
}

#[derive(Debug)]
pub struct Session<F: FileSystem> {
    inner: SessionStorageKind<F>,
}

// The in-memory storage locks are held only across synchronous sub-calls;
// the inner async fns never yield, so `await_holding_lock` is a false positive.
#[allow(clippy::await_holding_lock)]
impl<F: FileSystem> Session<F> {
    pub fn new(storage: JsonlSessionStorage<F>) -> Self {
        Self {
            inner: SessionStorageKind::Jsonl(storage),
        }
    }

    pub fn from_in_memory(storage: Arc<Mutex<InMemorySessionStorage>>) -> Self {
        Self {
            inner: SessionStorageKind::InMemory(storage),
        }
    }

    pub fn kind(&self) -> &SessionStorageKind<F> {
        &self.inner
    }

    // ---------------------------------------------------------------------
    // Metadata / lanes
    // ---------------------------------------------------------------------

    pub async fn get_metadata(&self) -> SessionMetadata {
        match &self.inner {
            SessionStorageKind::Jsonl(s) => s.get_metadata().await,
            SessionStorageKind::InMemory(s) => s.lock().unwrap().get_metadata().await,
        }
    }

    pub async fn get_lanes(&self) -> Vec<LanePointer> {
        match &self.inner {
            SessionStorageKind::Jsonl(s) => s.get_lanes().await,
            SessionStorageKind::InMemory(s) => s.lock().unwrap().get_lanes(),
        }
    }

    pub async fn create_lane(&mut self, lane: &str, at: Option<&str>) -> Result<(), SessionError> {
        match &mut self.inner {
            SessionStorageKind::Jsonl(s) => s.create_lane(lane, at).await,
            SessionStorageKind::InMemory(s) => s.lock().unwrap().create_lane(lane, at),
        }
    }

    pub async fn move_lane(&mut self, lane: &str, to: Option<&str>) -> Result<(), SessionError> {
        match &mut self.inner {
            SessionStorageKind::Jsonl(s) => s.move_lane(lane, to).await,
            SessionStorageKind::InMemory(s) => s.lock().unwrap().move_lane(lane, to),
        }
    }

    /// `getLeafId()` for the default ("main") lane.
    pub async fn get_leaf_id(&self) -> Result<Option<String>, SessionError> {
        let lanes = self.get_lanes().await;
        lanes
            .iter()
            .find(|pointer| pointer.lane == "main")
            .map(|pointer| pointer.leaf_id.clone())
            .ok_or_else(|| session_error(SessionErrorKind::InvalidLane, "Lane not found: main"))
    }

    async fn get_leaf_id_for_lane(&self, lane: &str) -> Result<Option<String>, SessionError> {
        let lanes = self.get_lanes().await;
        lanes
            .iter()
            .find(|pointer| pointer.lane == lane)
            .map(|pointer| pointer.leaf_id.clone())
            .ok_or_else(|| {
                session_error(
                    SessionErrorKind::InvalidLane,
                    format!("Lane not found: {lane}"),
                )
            })
    }

    // ---------------------------------------------------------------------
    // Entries / records
    // ---------------------------------------------------------------------

    pub async fn append_entry(
        &mut self,
        entry: EntryNoStats,
        lane: &str,
    ) -> Result<Entry, SessionError> {
        match &mut self.inner {
            SessionStorageKind::Jsonl(s) => s.append_entry(entry, lane).await,
            SessionStorageKind::InMemory(s) => s.lock().unwrap().append_entry(entry, lane),
        }
    }

    /// `appendMessage(message) → id`.
    pub async fn append_message(&mut self, message: AgentMessage) -> Result<String, SessionError> {
        let id = new_id();
        let entry = self
            .append_entry(
                EntryNoStats::Message {
                    id,
                    message,
                    terminate: None,
                },
                "main",
            )
            .await?;
        Ok(entry.id().to_string())
    }

    /// `appendCustomEntry(customType, data?) → id`.
    pub async fn append_custom_entry(
        &mut self,
        custom_type: &str,
        data: Option<serde_json::Value>,
    ) -> Result<String, SessionError> {
        let id = new_id();
        let entry = self
            .append_entry(
                EntryNoStats::Custom {
                    id,
                    custom_type: custom_type.to_string(),
                    data,
                },
                "main",
            )
            .await?;
        Ok(entry.id().to_string())
    }

    /// `appendMessageToLane(lane, message) → id`.
    pub async fn append_message_to_lane(
        &mut self,
        lane: &str,
        message: AgentMessage,
    ) -> Result<String, SessionError> {
        let id = new_id();
        let entry = self
            .append_entry(
                EntryNoStats::Message {
                    id,
                    message,
                    terminate: None,
                },
                lane,
            )
            .await?;
        Ok(entry.id().to_string())
    }

    /// `appendCustomEntryToLane(lane, customType, data?) → id`.
    pub async fn append_custom_entry_to_lane(
        &mut self,
        lane: &str,
        custom_type: &str,
        data: Option<serde_json::Value>,
    ) -> Result<String, SessionError> {
        let id = new_id();
        let entry = self
            .append_entry(
                EntryNoStats::Custom {
                    id,
                    custom_type: custom_type.to_string(),
                    data,
                },
                lane,
            )
            .await?;
        Ok(entry.id().to_string())
    }

    pub async fn append_record(&mut self, record: NewRecord) -> Result<LaneRecord, SessionError> {
        match &mut self.inner {
            SessionStorageKind::Jsonl(s) => s.append_record(record).await,
            SessionStorageKind::InMemory(s) => s.lock().unwrap().append_record(record),
        }
    }

    pub async fn get_entry(&self, id: &str) -> Option<Entry> {
        match &self.inner {
            SessionStorageKind::Jsonl(s) => s.get_entry(id).await,
            SessionStorageKind::InMemory(s) => s.lock().unwrap().get_entry(id),
        }
    }

    // ---------------------------------------------------------------------
    // Queries (facade-level validation mirrors upstream session.ts)
    // ---------------------------------------------------------------------

    fn assert_valid_limit(limit: Option<usize>) -> Result<(), SessionError> {
        if let Some(limit) = limit {
            if limit == 0 {
                return Err(session_error(
                    SessionErrorKind::InvalidQuery,
                    "limit must be a positive integer",
                ));
            }
        }
        Ok(())
    }

    fn assert_valid_cursor(after_seq: Option<u64>) -> Result<(), SessionError> {
        let _ = after_seq;
        Ok(())
    }

    /// `findEntries` (upstream `queryEntries`), with validation.
    pub async fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, SessionError> {
        Self::assert_valid_limit(query.limit)?;
        Self::assert_valid_cursor(query.cursor.map(|c| c.after_seq))?;
        match &self.inner {
            SessionStorageKind::Jsonl(s) => s.find_entries(query).await,
            SessionStorageKind::InMemory(s) => s.lock().unwrap().find_entries(query),
        }
    }

    /// `findEntry(query)` — first match of `queryEntries(query, 1)`.
    pub async fn find_entry(&self, query: &EntryQuery) -> Result<Option<Entry>, SessionError> {
        let narrowed = match query.limit {
            Some(_) => query.clone(),
            None => EntryQuery {
                limit: Some(1),
                ..query.clone()
            },
        };
        let result = self.find_entries(&narrowed).await?;
        Ok(result.first().cloned())
    }

    /// `findEntriesOnBranch` — default start is the main lane leaf.
    pub async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        start: Option<&str>,
        bounds: &BranchBounds,
    ) -> Result<Vec<Entry>, SessionError> {
        self.query_branch_entries("main", query, start, bounds)
            .await
    }

    /// `findEntryOnBranch`.
    pub async fn find_entry_on_branch(
        &self,
        query: &EntryQuery,
        start: Option<&str>,
        bounds: &BranchBounds,
    ) -> Result<Option<Entry>, SessionError> {
        let narrowed = match query.limit {
            Some(_) => query.clone(),
            None => EntryQuery {
                limit: Some(1),
                ..query.clone()
            },
        };
        let result = self
            .query_branch_entries("main", &narrowed, start, bounds)
            .await?;
        Ok(result.first().cloned())
    }

    async fn query_branch_entries(
        &self,
        default_lane: &str,
        query: &EntryQuery,
        start: Option<&str>,
        bounds: &BranchBounds,
    ) -> Result<Vec<Entry>, SessionError> {
        Self::assert_valid_limit(query.limit)?;
        Self::assert_valid_cursor(query.cursor.map(|c| c.after_seq))?;
        let start = match start {
            Some(start) => Some(start.to_string()),
            None => self.get_leaf_id_for_lane(default_lane).await?,
        };
        match start {
            Some(start) => match &self.inner {
                SessionStorageKind::Jsonl(s) => {
                    s.find_entries_on_branch(query, &start, bounds).await
                }
                SessionStorageKind::InMemory(s) => s
                    .lock()
                    .unwrap()
                    .find_entries_on_branch(query, &start, bounds),
            },
            None => Ok(Vec::new()),
        }
    }

    /// `findRecords` with the operationKind guard.
    pub async fn find_records(&self, query: &RecordQuery) -> Result<Vec<LaneRecord>, SessionError> {
        Self::assert_valid_limit(query.limit)?;
        Self::assert_valid_cursor(query.after_seq)?;
        if query.operation_kind.is_some()
            && query.record_type.as_deref() != Some("operation_started")
        {
            return Err(session_error(
                SessionErrorKind::InvalidQuery,
                "operationKind requires type \"operation_started\"",
            ));
        }
        match &self.inner {
            SessionStorageKind::Jsonl(s) => s.find_records(query).await,
            SessionStorageKind::InMemory(s) => s.lock().unwrap().find_records(query),
        }
    }

    pub async fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> Result<Vec<LaneRecord>, SessionError> {
        Self::assert_valid_limit(limit)?;
        match &self.inner {
            SessionStorageKind::Jsonl(s) => s.find_open_operations(lane, limit).await,
            SessionStorageKind::InMemory(s) => s.lock().unwrap().find_open_operations(lane, limit),
        }
    }

    /// `getLog(options)` — insertion order with optional afterSeq/limit.
    pub async fn get_log(&self, options: &LogOptions) -> Result<Vec<LogItem>, SessionError> {
        Self::assert_valid_limit(options.limit)?;
        Self::assert_valid_cursor(options.after_seq)?;
        match &self.inner {
            SessionStorageKind::Jsonl(s) => s.get_log(options).await,
            SessionStorageKind::InMemory(s) => s.lock().unwrap().get_log(options),
        }
    }

    // ---------------------------------------------------------------------
    // Stats / facts
    // ---------------------------------------------------------------------

    pub async fn get_stats(&self) -> SessionStats {
        match &self.inner {
            SessionStorageKind::Jsonl(s) => s.get_stats().await,
            SessionStorageKind::InMemory(s) => s.lock().unwrap().get_stats(),
        }
    }

    pub async fn get_name(&self) -> Option<String> {
        match &self.inner {
            SessionStorageKind::Jsonl(s) => s.get_name().await,
            SessionStorageKind::InMemory(s) => s.lock().unwrap().get_name(),
        }
    }

    pub async fn set_name(&mut self, name: Option<&str>) -> Result<(), SessionError> {
        match &mut self.inner {
            SessionStorageKind::Jsonl(s) => s.set_name(name).await,
            SessionStorageKind::InMemory(s) => s.lock().unwrap().set_name(name),
        }
    }

    pub async fn get_label(&self, id: &str) -> Option<String> {
        match &self.inner {
            SessionStorageKind::Jsonl(s) => s.get_label(id).await,
            SessionStorageKind::InMemory(s) => s.lock().unwrap().get_label(id),
        }
    }

    pub async fn set_label(&mut self, id: &str, label: Option<&str>) -> Result<(), SessionError> {
        match &mut self.inner {
            SessionStorageKind::Jsonl(s) => s.set_label(id, label).await,
            SessionStorageKind::InMemory(s) => s.lock().unwrap().set_label(id, label),
        }
    }

    // ---------------------------------------------------------------------
    // Lane views
    // ---------------------------------------------------------------------

    /// `view(lane)` — a lane-bound SessionTree (borrows this session mutably
    /// so views can append to their lane, mirroring upstream `Session.view`).
    pub fn view<'a>(&'a mut self, lane: &'a str) -> SessionView<'a, F> {
        SessionView {
            session: self,
            lane,
        }
    }
}

/// Lane-bound view (upstream `SessionTree` returned by `Session.view`).
/// Holds `&mut Session` so views can append to their lane.
#[derive(Debug)]
pub struct SessionView<'a, F: FileSystem> {
    session: &'a mut Session<F>,
    lane: &'a str,
}

impl<F: FileSystem> SessionView<'_, F> {
    pub async fn get_leaf_id(&self) -> Result<Option<String>, SessionError> {
        self.session.get_leaf_id_for_lane(self.lane).await
    }

    pub async fn get_entry(&self, id: &str) -> Option<Entry> {
        self.session.get_entry(id).await
    }

    pub async fn get_stats(&self) -> SessionStats {
        self.session.get_stats().await
    }

    pub async fn get_name(&self) -> Option<String> {
        self.session.get_name().await
    }

    pub async fn set_name(&mut self, name: Option<&str>) -> Result<(), SessionError> {
        self.session.set_name(name).await
    }

    pub async fn get_label(&self, target_id: &str) -> Option<String> {
        self.session.get_label(target_id).await
    }

    pub async fn set_label(
        &mut self,
        target_id: &str,
        label: Option<&str>,
    ) -> Result<(), SessionError> {
        self.session.set_label(target_id, label).await
    }

    pub async fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, SessionError> {
        self.session.find_entries(query).await
    }

    pub async fn find_entry(&self, query: &EntryQuery) -> Result<Option<Entry>, SessionError> {
        self.session.find_entry(query).await
    }

    pub async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> Result<Vec<Entry>, SessionError> {
        self.session
            .query_branch_entries(self.lane, query, None, bounds)
            .await
    }

    pub async fn find_entry_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> Result<Option<Entry>, SessionError> {
        let result = self
            .session
            .query_branch_entries(self.lane, query, None, bounds)
            .await?;
        Ok(result.first().cloned())
    }

    pub async fn append_message(&mut self, message: AgentMessage) -> Result<String, SessionError> {
        self.session
            .append_message_to_lane(self.lane, message)
            .await
    }

    pub async fn append_custom_entry(
        &mut self,
        custom_type: &str,
        data: Option<serde_json::Value>,
    ) -> Result<String, SessionError> {
        self.session
            .append_custom_entry_to_lane(self.lane, custom_type, data)
            .await
    }
}
