//! Session facade over the SQLite storage — port of upstream `Session`
//! (`packages/agent/src/harness/session/session.ts`) backed by
//! `SqliteSessionStorage`.
//!
//! The method surface deliberately mirrors `pi_agent::session::Session`
//! (which is hardwired to the JSONL/in-memory storages) so the shared
//! conformance case bodies compile unchanged against the SQLite backend.

use std::sync::Arc;

use pi_agent::session::state::{BranchBounds, EntryQuery, LogOptions, RecordQuery};
use pi_agent::session::types::{
    session_error, Entry, EntryNoStats, LanePointer, LaneRecord, LogItem, NewRecord, SessionError,
    SessionErrorKind, SessionMetadata, SessionStats,
};
use pi_agent::types::AgentMessage;

use crate::repo::SqliteSessionStorage;

/// SQLite-backed session (mirror of `Session<SqliteSessionMetadata>`).
#[derive(Clone)]
pub struct SqliteSession {
    storage: Arc<SqliteSessionStorage>,
}

pub(crate) fn new_id() -> String {
    crate::new_id()
}

impl SqliteSession {
    pub(crate) fn new(storage: Arc<SqliteSessionStorage>) -> Self {
        Self { storage }
    }

    pub fn storage(&self) -> &Arc<SqliteSessionStorage> {
        &self.storage
    }

    pub fn session_id(&self) -> String {
        self.storage.session_id()
    }

    // ---------------------------------------------------------------------
    // Metadata / lanes
    // ---------------------------------------------------------------------

    pub async fn get_metadata(&self) -> Result<SessionMetadata, SessionError> {
        let metadata = self.storage.get_metadata().await?;
        Ok(metadata.to_core())
    }

    /// Returns the full SQLite metadata (including the `name` projection and
    /// the application-owned `metadata` object).
    pub async fn get_sqlite_metadata(&self) -> Result<crate::types::SqliteSessionMetadata, SessionError> {
        self.storage.get_metadata().await
    }

    pub async fn get_lanes(&self) -> Result<Vec<LanePointer>, SessionError> {
        self.storage.get_lanes().await
    }

    pub async fn create_lane(&mut self, lane: &str, at: Option<&str>) -> Result<(), SessionError> {
        self.storage.create_lane(lane, at).await
    }

    pub async fn move_lane(&mut self, lane: &str, to: Option<&str>) -> Result<(), SessionError> {
        self.storage.move_lane(lane, to).await
    }

    /// `getLeafId()` for the default ("main") lane.
    pub async fn get_leaf_id(&self) -> Result<Option<String>, SessionError> {
        let lanes = self.get_lanes().await?;
        lanes
            .iter()
            .find(|pointer| pointer.lane == "main")
            .map(|pointer| pointer.leaf_id.clone())
            .ok_or_else(|| session_error(SessionErrorKind::InvalidLane, "Lane not found: main"))
    }

    async fn get_leaf_id_for_lane(&self, lane: &str) -> Result<Option<String>, SessionError> {
        let lanes = self.get_lanes().await?;
        lanes
            .iter()
            .find(|pointer| pointer.lane == lane)
            .map(|pointer| pointer.leaf_id.clone())
            .ok_or_else(|| session_error(SessionErrorKind::InvalidLane, format!("Lane not found: {lane}")))
    }

    // ---------------------------------------------------------------------
    // Entries / records
    // ---------------------------------------------------------------------

    pub async fn append_entry(&mut self, entry: EntryNoStats, lane: &str) -> Result<Entry, SessionError> {
        self.storage.append_entry(entry, lane).await
    }

    /// `appendMessage(message) → id`.
    pub async fn append_message(&mut self, message: AgentMessage) -> Result<String, SessionError> {
        let id = new_id();
        let entry = self.append_entry(EntryNoStats::Message { id, message, terminate: None }, "main").await?;
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
            .append_entry(EntryNoStats::Custom { id, custom_type: custom_type.to_string(), data }, "main")
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
            .append_entry(EntryNoStats::Message { id, message, terminate: None }, lane)
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
            .append_entry(EntryNoStats::Custom { id, custom_type: custom_type.to_string(), data }, lane)
            .await?;
        Ok(entry.id().to_string())
    }

    pub async fn append_record(&mut self, record: NewRecord) -> Result<LaneRecord, SessionError> {
        self.storage.append_record(record).await
    }

    pub async fn get_entry(&self, id: &str) -> Option<Entry> {
        self.storage.get_entry(id).await.expect("sqlite entry read")
    }

    // ---------------------------------------------------------------------
    // Queries (facade-level validation mirrors upstream session.ts)
    // ---------------------------------------------------------------------

    fn assert_valid_limit(limit: Option<usize>) -> Result<(), SessionError> {
        if let Some(limit) = limit {
            if limit == 0 {
                return Err(session_error(SessionErrorKind::InvalidQuery, "limit must be a positive integer"));
            }
        }
        Ok(())
    }

    fn assert_valid_cursor(after_seq: Option<u64>) -> Result<(), SessionError> {
        let _ = after_seq;
        Ok(())
    }

    pub async fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, SessionError> {
        Self::assert_valid_limit(query.limit)?;
        Self::assert_valid_cursor(query.cursor.map(|c| c.after_seq))?;
        self.storage.find_entries(query).await
    }

    /// `findEntry(query)` — first match of `queryEntries(query, 1)`.
    pub async fn find_entry(&self, query: &EntryQuery) -> Result<Option<Entry>, SessionError> {
        let narrowed = match query.limit {
            Some(_) => query.clone(),
            None => EntryQuery { limit: Some(1), ..query.clone() },
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
        self.query_branch_entries("main", query, start, bounds).await
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
            None => EntryQuery { limit: Some(1), ..query.clone() },
        };
        let result = self.query_branch_entries("main", &narrowed, start, bounds).await?;
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
            Some(start) => self.storage.find_entries_on_branch(query, bounds, &start).await,
            None => Ok(Vec::new()),
        }
    }

    /// `findRecords` with the operationKind guard.
    pub async fn find_records(&self, query: &RecordQuery) -> Result<Vec<LaneRecord>, SessionError> {
        Self::assert_valid_limit(query.limit)?;
        Self::assert_valid_cursor(query.after_seq)?;
        if query.operation_kind.is_some() && query.record_type.as_deref() != Some("operation_started") {
            return Err(session_error(
                SessionErrorKind::InvalidQuery,
                "operationKind requires type \"operation_started\"",
            ));
        }
        self.storage.find_records(query).await
    }

    pub async fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> Result<Vec<LaneRecord>, SessionError> {
        Self::assert_valid_limit(limit)?;
        self.storage.find_open_operations(lane, limit).await
    }

    /// `getLog(options)` — insertion order with optional afterSeq/limit.
    pub async fn get_log(&self, options: &LogOptions) -> Result<Vec<LogItem>, SessionError> {
        Self::assert_valid_limit(options.limit)?;
        Self::assert_valid_cursor(options.after_seq)?;
        self.storage.get_log(options).await
    }

    // ---------------------------------------------------------------------
    // Stats / facts
    // ---------------------------------------------------------------------

    pub async fn get_stats(&self) -> SessionStats {
        self.storage.get_stats().await.expect("sqlite stats read")
    }

    pub async fn get_name(&self) -> Option<String> {
        self.storage.get_name().await.expect("sqlite name read")
    }

    pub async fn set_name(&mut self, name: Option<&str>) -> Result<(), SessionError> {
        self.storage.set_name(name).await
    }

    pub async fn get_label(&self, id: &str) -> Option<String> {
        self.storage.get_label(id).await.expect("sqlite label read")
    }

    pub async fn set_label(&mut self, id: &str, label: Option<&str>) -> Result<(), SessionError> {
        self.storage.set_label(id, label).await
    }

    // ---------------------------------------------------------------------
    // Lane views
    // ---------------------------------------------------------------------

    /// `view(lane)` — a lane-bound SessionTree (mirror of upstream `Session.view`).
    pub fn view<'a>(&'a mut self, lane: &'a str) -> SqliteSessionView<'a> {
        SqliteSessionView { session: self, lane }
    }
}

/// Lane-bound view (mirror of `SessionTree` returned by `Session.view`).
#[derive(Debug)]
pub struct SqliteSessionView<'a> {
    session: &'a mut SqliteSession,
    lane: &'a str,
}

impl SqliteSessionView<'_> {
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

    pub async fn set_label(&mut self, target_id: &str, label: Option<&str>) -> Result<(), SessionError> {
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
        self.session.query_branch_entries(self.lane, query, None, bounds).await
    }

    pub async fn find_entry_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> Result<Option<Entry>, SessionError> {
        let result = self.session.query_branch_entries(self.lane, query, None, bounds).await?;
        Ok(result.first().cloned())
    }

    pub async fn append_message(&mut self, message: AgentMessage) -> Result<String, SessionError> {
        self.session.append_message_to_lane(self.lane, message).await
    }

    pub async fn append_custom_entry(
        &mut self,
        custom_type: &str,
        data: Option<serde_json::Value>,
    ) -> Result<String, SessionError> {
        self.session.append_custom_entry_to_lane(self.lane, custom_type, data).await
    }
}

impl std::fmt::Debug for SqliteSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteSession").field("session_id", &self.session_id()).finish()
    }
}
