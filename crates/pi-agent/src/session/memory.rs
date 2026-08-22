//! In-memory session storage and repository — port of
//! `packages/agent/src/harness/session/memory.ts`.

use std::sync::{Arc, Mutex};

use super::state::{BranchBounds, EntryQuery, ForkOptions, LogOptions, RecordQuery, SessionState};
use super::types::{
    session_error, Entry, EntryNoStats, LanePointer, LaneRecord, LogItem, Mutation, NewRecord,
    SessionError, SessionErrorKind, SessionMetadata, SessionStats,
};
use crate::fs::MemoryFs;

/// `InMemorySessionStorage` — a `SessionStorage` over an in-memory
/// `SessionState` (upstream `memory.ts`).
#[derive(Debug)]
pub struct InMemorySessionStorage {
    metadata: SessionMetadata,
    state: SessionState,
}

impl InMemorySessionStorage {
    pub fn new(metadata: SessionMetadata) -> Self {
        Self { metadata, state: SessionState::default() }
    }

    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// `fork(metadata, options)` — new storage replaying fork mutations.
    pub fn fork(&self, metadata: SessionMetadata, options: &ForkOptions) -> Result<Self, SessionError> {
        let mut storage = InMemorySessionStorage::new(metadata);
        let mutations = self.state.create_fork_mutations(options)?;
        for mutation in &mutations {
            storage.state.apply_mutation(mutation)?;
        }
        Ok(storage)
    }

    pub async fn get_metadata(&self) -> SessionMetadata {
        self.metadata.clone()
    }

    pub fn metadata(&self) -> &SessionMetadata {
        &self.metadata
    }

    pub fn get_lanes(&self) -> Vec<LanePointer> {
        self.state.get_lanes()
    }

    pub fn create_lane(&mut self, lane: &str, at: Option<&str>) -> Result<(), SessionError> {
        self.state.validate_new_lane(lane)?;
        self.state.validate_target(at)?;
        self.state.apply_mutation(&Mutation::Lane {
            seq: self.state.next_sequence(),
            lane: lane.to_string(),
            leaf_id: at.map(|s| s.to_string()),
        })
    }

    pub fn move_lane(&mut self, lane: &str, to: Option<&str>) -> Result<(), SessionError> {
        self.state.require_lane(lane)?;
        self.state.validate_target(to)?;
        self.state.apply_mutation(&Mutation::Lane {
            seq: self.state.next_sequence(),
            lane: lane.to_string(),
            leaf_id: to.map(|s| s.to_string()),
        })
    }

    pub fn append_entry(&mut self, entry: EntryNoStats, lane: &str) -> Result<Entry, SessionError> {
        let parent_id = self.state.require_lane(lane)?;
        self.state.validate_unused_id(entry.id())?;
        let entry = super::types::Entry::from_provisioned(entry, parent_id, self.state.next_sequence(), now_ms());
        self.state.apply_mutation(&Mutation::Entry { lane: Some(lane.to_string()), entry: entry.clone() })?;
        Ok(entry)
    }

    pub fn append_record(&mut self, new_record: NewRecord) -> Result<LaneRecord, SessionError> {
        self.state.require_lane(new_record.lane())?;
        self.state.validate_unused_id(new_record.id())?;
        let current_open_operation_id = self.state.open_operation_ids(new_record.lane()).first().cloned();
        if new_record.record_type() == "operation_started" && current_open_operation_id.is_some() {
            return Err(session_error(
                SessionErrorKind::Storage,
                format!(
                    "Lane {} already has an open operation {}",
                    new_record.lane(),
                    current_open_operation_id.as_deref().unwrap_or_default()
                ),
            ));
        }
        let record = complete_record(new_record, self.state.next_sequence(), now_ms());
        self.state.apply_mutation(&Mutation::Record { record: record.clone() })?;
        Ok(record)
    }

    pub fn get_entry(&self, id: &str) -> Option<Entry> {
        self.state.get_entry(id).cloned()
    }

    pub fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, SessionError> {
        self.state.find_entries(query)
    }

    pub fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        start: &str,
        bounds: &BranchBounds,
    ) -> Result<Vec<Entry>, SessionError> {
        self.state.find_entries_on_branch(query, start, bounds)
    }

    pub fn find_records(&self, query: &RecordQuery) -> Result<Vec<LaneRecord>, SessionError> {
        self.state.find_records(query)
    }

    pub fn find_open_operations(&self, lane: &str, limit: Option<usize>) -> Result<Vec<LaneRecord>, SessionError> {
        self.state.find_open_operations(lane, limit)
    }

    pub fn get_log(&self, options: &LogOptions) -> Result<Vec<LogItem>, SessionError> {
        self.state.get_log(options)
    }

    pub fn get_name(&self) -> Option<String> {
        self.state.get_name().map(|s| s.to_string())
    }

    pub fn set_name(&mut self, name: Option<&str>) -> Result<(), SessionError> {
        self.state.apply_mutation(&Mutation::Fact(super::types::Fact::Name {
            seq: self.state.next_sequence(),
            name: name.map(|s| s.to_string()),
        }))
    }

    pub fn get_label(&self, id: &str) -> Option<String> {
        self.state.get_label(id).map(|s| s.to_string())
    }

    pub fn set_label(&mut self, id: &str, label: Option<&str>) -> Result<(), SessionError> {
        self.state.validate_target(Some(id))?;
        self.state.apply_mutation(&Mutation::Fact(super::types::Fact::Label {
            seq: self.state.next_sequence(),
            target_id: id.to_string(),
            label: label.map(|s| s.to_string()),
        }))
    }

    pub fn get_stats(&self) -> SessionStats {
        self.state.get_stats().clone()
    }
}

fn complete_record(new_record: NewRecord, seq: u64, timestamp: u64) -> LaneRecord {
    match new_record {
        NewRecord::OperationStarted { id, lane, source_leaf_id, intent } => LaneRecord::OperationStarted {
            id, seq, lane, timestamp, source_leaf_id, intent,
        },
        NewRecord::AbortRequested { id, lane, run_id } => LaneRecord::AbortRequested {
            id, seq, lane, timestamp, run_id,
        },
        NewRecord::OperationFinished { id, lane, run_id, outcome, error } => {
            LaneRecord::OperationFinished { id, seq, lane, timestamp, run_id, outcome, error }
        }
        NewRecord::StepAttempt { id, lane, run_id, step, attempt, result_entry_id, compaction_reason } => {
            LaneRecord::StepAttempt {
                id, seq, lane, timestamp, run_id, step, attempt, result_entry_id, compaction_reason,
            }
        }
        NewRecord::ToolStarted {
            id, lane, run_id, assistant_entry_id, tool_index, tool_call_id, tool_name, effective_args,
            result_entry_id, replay,
        } => LaneRecord::ToolStarted {
            id, seq, lane, timestamp, run_id, assistant_entry_id, tool_index, tool_call_id, tool_name,
            effective_args, result_entry_id, replay,
        },
        NewRecord::QueueEnqueued { id, lane, queue, run_id, target } => LaneRecord::QueueEnqueued {
            id, seq, lane, timestamp, queue, run_id, target,
        },
        NewRecord::QueueCancelled { id, lane, entry_id } => {
            LaneRecord::QueueCancelled { id, seq, lane, timestamp, entry_id }
        }
        NewRecord::WriteDeferred { id, lane, run_id, target } => LaneRecord::WriteDeferred {
            id, seq, lane, timestamp, run_id, target,
        },
        NewRecord::Usage {
            id, lane, cause, run_id, entry_id, attempt, stop_reason, tool_call_id, details, usage,
        } => LaneRecord::Usage {
            id, seq, lane, timestamp, cause,
            run_id: run_id.unwrap_or_default(),
            entry_id: entry_id.unwrap_or_default(),
            attempt: attempt.unwrap_or(0),
            stop_reason,
            tool_call_id,
            details,
            usage,
        },
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn in_memory_metadata(id: impl Into<String>, parent_session_id: Option<String>) -> SessionMetadata {
    SessionMetadata {
        id: id.into(),
        created_at: now_ms(),
        cwd: String::new(),
        path: String::new(),
        modified_at: 0,
        source_format: 4,
        parent_session_id,
        legacy_parent_session_path: None,
        metadata: None,
    }
}

/// `InMemorySessionRepo` — port of upstream `memory.ts` repository. Sessions
/// and their repos share storage through `Arc<Mutex<..>>` so that an opened
/// session observes (and participates in) the repo's live state, exactly like
/// the upstream shared-reference model.
#[derive(Debug, Default)]
pub struct InMemorySessionRepo {
    sessions: std::collections::HashMap<String, Arc<Mutex<InMemorySessionStorage>>>,
}

impl InMemorySessionRepo {
    pub fn new() -> Self {
        Self { sessions: std::collections::HashMap::new() }
    }

    pub fn create(
        &mut self,
        id: Option<&str>,
        parent_session_id: Option<&str>,
    ) -> Result<super::session::Session<MemoryFs>, SessionError> {
        let id = id.map(|s| s.to_string()).unwrap_or_else(super::session::new_id);
        if self.sessions.contains_key(&id) {
            return Err(session_error(SessionErrorKind::AlreadyExists, format!("Session already exists: {id}")));
        }
        let storage = Arc::new(Mutex::new(InMemorySessionStorage::new(in_memory_metadata(
            id.clone(),
            parent_session_id.map(|s| s.to_string()),
        ))));
        self.sessions.insert(id, storage.clone());
        Ok(super::session::Session::from_in_memory(storage))
    }

    pub fn open(
        &self,
        metadata: &SessionMetadata,
    ) -> Result<super::session::Session<MemoryFs>, SessionError> {
        let storage = self.require_storage(&metadata.id)?;
        Ok(super::session::Session::from_in_memory(storage.clone()))
    }

    pub fn list(&self) -> Vec<SessionMetadata> {
        let mut metas: Vec<SessionMetadata> = Vec::new();
        for storage in self.sessions.values() {
            metas.push(storage.lock().unwrap().metadata().clone());
        }
        metas
    }

    pub fn delete(&mut self, metadata: &SessionMetadata) {
        self.sessions.remove(&metadata.id);
    }

    pub fn fork(
        &mut self,
        source: &SessionMetadata,
        id: Option<&str>,
        parent_session_id: Option<&str>,
        options: &ForkOptions,
    ) -> Result<super::session::Session<MemoryFs>, SessionError> {
        let source_storage = self.require_storage(&source.id)?;
        let id = id.map(|s| s.to_string()).unwrap_or_else(super::session::new_id);
        if self.sessions.contains_key(&id) {
            return Err(session_error(SessionErrorKind::AlreadyExists, format!("Session already exists: {id}")));
        }
        let parent = parent_session_id.map(|s| s.to_string()).or_else(|| Some(source.id.clone()));
        let metadata = in_memory_metadata(id.clone(), parent);
        let storage = Arc::new(Mutex::new(source_storage.lock().unwrap().fork(metadata, options)?));
        self.sessions.insert(id, storage.clone());
        Ok(super::session::Session::from_in_memory(storage))
    }

    fn require_storage(&self, id: &str) -> Result<&Arc<Mutex<InMemorySessionStorage>>, SessionError> {
        self.sessions
            .get(id)
            .ok_or_else(|| session_error(SessionErrorKind::NotFound, format!("Session not found: {id}")))
    }
}
