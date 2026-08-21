//! Session facade — port of the subset of
//! `packages/agent/src/harness/session/session.ts` that storage consumers
//! use (create/open/append/query/fork delegation and id generation).

use super::jsonl::storage::JsonlSessionStorage;
use super::state::{EntryOrder, EntryQuery, RecordQuery};
use super::types::{
    Entry, EntryNoStats, LanePointer, LaneRecord, LogItem, NewRecord, SessionError, SessionMetadata,
    SessionStats,
};
use crate::fs::FileSystem;

/// Generates a session/entry id (upstream `uuidv7`).
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[derive(Debug)]
pub struct Session<F: FileSystem> {
    storage: JsonlSessionStorage<F>,
}

impl<F: FileSystem> Session<F> {
    pub fn new(storage: JsonlSessionStorage<F>) -> Self {
        Self { storage }
    }

    pub fn storage(&self) -> &JsonlSessionStorage<F> {
        &self.storage
    }

    pub fn storage_mut(&mut self) -> &mut JsonlSessionStorage<F> {
        &mut self.storage
    }

    pub async fn get_metadata(&self) -> SessionMetadata {
        self.storage.get_metadata().await
    }

    pub async fn append_entry(&mut self, entry: EntryNoStats, lane: &str) -> Result<Entry, SessionError> {
        self.storage.append_entry(entry, lane).await
    }

    pub async fn append_custom_entry(
        &mut self,
        custom_type: &str,
        data: Option<serde_json::Value>,
    ) -> Result<Entry, SessionError> {
        let id = new_id();
        self.storage
            .append_entry(
                EntryNoStats::Custom {
                    id,
                    custom_type: custom_type.to_string(),
                    data,
                },
                "main",
            )
            .await
    }

    pub async fn append_record(&mut self, record: NewRecord) -> Result<LaneRecord, SessionError> {
        self.storage.append_record(record).await
    }

    pub async fn get_entry(&self, id: &str) -> Option<Entry> {
        self.storage.get_entry(id).await
    }

    pub async fn find_entries(&self, query: &EntryQuery) -> Vec<Entry> {
        self.storage.find_entries(query).await
    }

    pub async fn find_entries_on_branch(&self, start: &str, stop_at_type: Option<&str>) -> Vec<Entry> {
        self.storage.find_entries_on_branch(start, stop_at_type).await
    }

    pub async fn find_records(&self, query: &RecordQuery) -> Vec<LaneRecord> {
        self.storage.find_records(query).await
    }

    pub async fn get_log(&self, order: EntryOrder) -> Vec<LogItem> {
        self.storage.get_log(order).await
    }

    pub async fn get_stats(&self) -> SessionStats {
        self.storage.get_stats().await
    }

    pub async fn get_lanes(&self) -> Vec<LanePointer> {
        self.storage.get_lanes().await
    }

    pub async fn create_lane(&mut self, lane: &str, at: Option<&str>) -> Result<(), SessionError> {
        self.storage.create_lane(lane, at).await
    }

    pub async fn move_lane(&mut self, lane: &str, to: Option<&str>) -> Result<(), SessionError> {
        self.storage.move_lane(lane, to).await
    }

    pub async fn get_name(&self) -> Option<String> {
        self.storage.get_name().await
    }

    pub async fn set_name(&mut self, name: Option<&str>) -> Result<(), SessionError> {
        self.storage.set_name(name).await
    }

    pub async fn get_label(&self, id: &str) -> Option<String> {
        self.storage.get_label(id).await
    }

    pub async fn set_label(&mut self, id: &str, label: Option<&str>) -> Result<(), SessionError> {
        self.storage.set_label(id, label).await
    }
}
