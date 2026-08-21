//! In-memory session state — port of `packages/agent/src/harness/session/state.ts`
//! (the portion the JSONL storage needs: mutations, lanes, queries, log, stats).

use std::collections::{BTreeMap, HashSet};

use super::types::{
    session_error, Entry, Fact, LanePointer, LaneRecord, LogItem, Mutation, SessionError,
    SessionErrorKind, SessionStats,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryOrder {
    NewestFirst,
    OldestFirst,
}

#[derive(Debug, Clone, Default)]
pub struct EntryQuery {
    pub order: Option<EntryOrder>,
    pub id: Option<String>,
    pub entry_type: Option<String>,
    pub custom_type: Option<String>,
    pub cursor: Option<EntryCursor>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Copy)]
pub struct EntryCursor {
    pub after_seq: u64,
}

impl Default for EntryCursor {
    fn default() -> Self {
        Self { after_seq: 0 }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RecordQuery {
    pub order: Option<EntryOrder>,
    pub record_type: Option<String>,
    pub run_id: Option<String>,
    pub operation_kind: Option<String>,
    pub lane: Option<String>,
    pub tool_call_id: Option<String>,
    pub cursor: Option<EntryCursor>,
    pub limit: Option<usize>,
}

pub struct SessionState {
    sequence: u64,
    used_ids: HashSet<String>,
    entries: Vec<Entry>,
    records: Vec<LaneRecord>,
    open_operations_by_lane: BTreeMap<String, Vec<String>>, // lane -> open op ids
    lanes: BTreeMap<String, Option<String>>,                // lane -> leaf id
    name: Option<String>,
    labels: BTreeMap<String, String>,
    stats: SessionStats,
    log: Vec<LogItem>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            sequence: 0,
            used_ids: HashSet::new(),
            entries: Vec::new(),
            records: Vec::new(),
            open_operations_by_lane: BTreeMap::new(),
            lanes: BTreeMap::from([("main".to_string(), None)]),
            name: None,
            labels: BTreeMap::new(),
            stats: SessionStats::default(),
            log: Vec::new(),
        }
    }
}

impl SessionState {
    pub fn next_sequence(&self) -> u64 {
        self.sequence + 1
    }

    pub fn get_lanes(&self) -> Vec<LanePointer> {
        self.lanes
            .iter()
            .map(|(lane, leaf)| LanePointer { lane: lane.clone(), leaf_id: leaf.clone() })
            .collect()
    }

    pub fn validate_new_lane(&self, lane: &str) -> Result<(), SessionError> {
        if self.lanes.contains_key(lane) {
            Err(session_error(SessionErrorKind::InvalidLane, format!("lane {lane} already exists")))
        } else {
            Ok(())
        }
    }

    pub fn require_lane(&self, lane: &str) -> Result<Option<String>, SessionError> {
        self.lanes
            .get(lane)
            .cloned()
            .ok_or_else(|| session_error(SessionErrorKind::InvalidLane, format!("unknown lane {lane}")))
    }

    pub fn validate_target(&self, target: Option<&str>) -> Result<(), SessionError> {
        if let Some(target) = target {
            self.require_entry_id(target)?;
        }
        Ok(())
    }

    fn require_entry_id(&self, id: &str) -> Result<(), SessionError> {
        self.entries
            .iter()
            .find(|e| e.id() == id)
            .map(|_| ())
            .ok_or_else(|| session_error(SessionErrorKind::InvalidTarget, format!("unknown entry {id}")))
    }

    pub fn validate_unused_id(&self, id: &str) -> Result<(), SessionError> {
        if self.used_ids.contains(id) {
            Err(session_error(SessionErrorKind::InvalidEntry, format!("entry id already used: {id}")))
        } else {
            Ok(())
        }
    }

    /// Open-operation markers used to enforce one open operation per lane.
    pub fn find_open_operations(&self, lane: &str) -> Vec<String> {
        self.open_operations_by_lane
            .get(lane)
            .cloned()
            .unwrap_or_default()
    }

    pub fn get_entry(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id() == id)
    }

    pub fn find_entries(&self, query: &EntryQuery) -> Vec<Entry> {
        let mut items: Vec<&Entry> = self.entries.iter().collect();
        items.retain(|e| {
            if let Some(id) = &query.id {
                if e.id() != id {
                    return false;
                }
            }
            if let Some(t) = &query.entry_type {
                if e.entry_type_str() != t {
                    return false;
                }
            }
            if let Some(ct) = &query.custom_type {
                if let Entry::Custom { custom_type, .. } = e {
                    if custom_type != ct {
                        return false;
                    }
                } else {
                    return false;
                }
            }
            if let Some(cursor) = &query.cursor {
                if e.seq() <= cursor.after_seq {
                    return false;
                }
            }
            true
        });
        if query.order == Some(EntryOrder::OldestFirst) {
            // already insertion order (ascending seq)
        } else {
            items.reverse();
        }
        let limit = query.limit.unwrap_or(usize::MAX);
        items.truncate(limit);
        items.into_iter().cloned().collect()
    }

    /// Bounded branch query: walk parents from `start`, including the first
    /// entry of type `stop_at_type` if encountered, else all the way to root.
    pub fn find_entries_on_branch(&self, start: &str, stop_at_type: Option<&str>) -> Vec<Entry> {
        let mut result = Vec::new();
        let mut current = start.to_string();
        loop {
            let Some(entry) = self.entries.iter().find(|e| e.id() == current) else {
                break;
            };
            let clone = entry.clone();
            let is_stop = stop_at_type.map(|t| clone.entry_type_str() == t).unwrap_or(false);
            result.push(clone);
            if is_stop {
                break;
            }
            match entry.parent_id() {
                Some(parent) => {
                    if parent.is_empty() {
                        break;
                    }
                    current = parent.to_string();
                }
                None => break,
            }
        }
        result
    }

    pub fn find_records(&self, query: &RecordQuery) -> Vec<LaneRecord> {
        let mut items: Vec<&LaneRecord> = self.records.iter().collect();
        items.retain(|r| {
            if let Some(t) = &query.record_type {
                if r.record_type() != t {
                    return false;
                }
            }
            if let Some(run_id) = &query.run_id {
                let matches = match r {
                    LaneRecord::AbortRequested { run_id: rid, .. }
                    | LaneRecord::OperationFinished { run_id: rid, .. }
                    | LaneRecord::StepAttempt { run_id: rid, .. }
                    | LaneRecord::ToolStarted { run_id: rid, .. }
                    | LaneRecord::QueueEnqueued { run_id: rid, .. }
                    | LaneRecord::WriteDeferred { run_id: rid, .. }
                    | LaneRecord::Usage { run_id: rid, .. } => rid == run_id,
                    _ => false,
                };
                if !matches {
                    return false;
                }
            }
            if let Some(op_kind) = &query.operation_kind {
                if let LaneRecord::OperationStarted { intent, .. } = r {
                    let kind = match intent {
                        super::types::OperationIntent::Run { .. } => "run",
                        super::types::OperationIntent::Compaction { .. } => "compaction",
                        super::types::OperationIntent::Navigation { .. } => "navigation",
                    };
                    if kind != op_kind {
                        return false;
                    }
                } else {
                    return false;
                }
            }
            if let Some(lane) = &query.lane {
                if r.lane() != lane {
                    return false;
                }
            }
            if let Some(tool_call_id) = &query.tool_call_id {
                let matches = match r {
                    LaneRecord::ToolStarted { tool_call_id: id, .. } => id == tool_call_id,
                    LaneRecord::Usage { tool_call_id: id, .. } => id.as_deref() == Some(tool_call_id),
                    _ => false,
                };
                if !matches {
                    return false;
                }
            }
            if let Some(cursor) = &query.cursor {
                if r.seq() <= cursor.after_seq {
                    return false;
                }
            }
            true
        });
        if query.order == Some(EntryOrder::OldestFirst) {
            // insertion order
        } else {
            items.reverse();
        }
        let limit = query.limit.unwrap_or(usize::MAX);
        items.truncate(limit);
        items.into_iter().cloned().collect()
    }

    pub fn get_log(&self, order: EntryOrder) -> Vec<LogItem> {
        if order == EntryOrder::OldestFirst {
            self.log.clone()
        } else {
            self.log.iter().rev().cloned().collect()
        }
    }

    pub fn get_name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn get_label(&self, id: &str) -> Option<&str> {
        self.labels.get(id).map(|s| s.as_str())
    }

    pub fn get_stats(&self) -> &SessionStats {
        &self.stats
    }

    /// Applies a mutation to the in-memory state (after it has been
    /// persisted). Mirrors `SessionState.applyMutation`.
    pub fn apply_mutation(&mut self, mutation: &Mutation) -> Result<(), SessionError> {
        match mutation {
            Mutation::Entry { lane, entry } => {
                let entry = entry.clone();
                let seq = entry.seq();
                self.validate_unused_id(entry.id())?;
                if let Some(lane) = lane {
                    self.require_lane(lane)?;
                    let leaf = self.lanes.get(lane).cloned().unwrap_or(None);
                    // The stored parentId must be the lane's current leaf.
                    if entry.parent_id() != leaf.as_deref() {
                        return Err(session_error(
                            SessionErrorKind::InvalidEntry,
                            "entry parentId does not match lane leaf",
                        ));
                    }
                }
                self.used_ids.insert(entry.id().to_string());
                self.sequence = self.sequence.max(seq);
                self.entries.push(entry.clone());
                if let Some(lane) = lane {
                    self.lanes.insert(lane.clone(), Some(entry.id().to_string()));
                }
                if entry.entry_type_str() == "message" {
                    self.stats.message_count += 1;
                }
                self.log.push(LogItem::Entry(entry));
                Ok(())
            }
            Mutation::Record { record } => {
                let record = record.clone();
                let seq = record.seq();
                self.validate_unused_id(record.id())?;
                self.require_lane(record.lane())?;
                if record.record_type() == "operation_started" {
                    if self.open_operations_by_lane.get(record.lane()).is_some_and(|ops| !ops.is_empty()) {
                        return Err(session_error(
                            SessionErrorKind::Storage,
                            format!("lane {} already has an open operation", record.lane()),
                        ));
                    }
                    self.open_operations_by_lane
                        .entry(record.lane().to_string())
                        .or_default()
                        .push(record.id().to_string());
                }
                if match &record {
                    LaneRecord::OperationFinished { run_id, .. } => {
                        self.open_operations_by_lane
                            .get_mut(record.lane())
                            .map(|ops| ops.retain(|id| id != run_id));
                        false
                    }
                    _ => false,
                } {
                    unreachable!()
                }
                self.used_ids.insert(record.id().to_string());
                self.sequence = self.sequence.max(seq);
                self.records.push(record.clone());
                self.log.push(LogItem::Record(record));
                Ok(())
            }
            Mutation::Lane { seq, lane, leaf_id } => {
                if !self.lanes.contains_key(lane) {
                    return Err(session_error(SessionErrorKind::InvalidLane, format!("unknown lane {lane}")));
                }
                if let Some(leaf) = leaf_id {
                    self.require_entry_id(leaf)?;
                }
                self.sequence = self.sequence.max(*seq);
                self.lanes.insert(lane.clone(), leaf_id.clone());
                Ok(())
            }
            Mutation::Fact(fact) => {
                self.sequence = self.sequence.max(fact_seq(fact));
                match fact {
                    Fact::Name { name, .. } => {
                        self.name = name.clone();
                        Ok(())
                    }
                    Fact::Label { target_id, label, .. } => {
                        self.require_entry_id(target_id)?;
                        match label {
                            Some(label) => {
                                self.labels.insert(target_id.clone(), label.clone());
                            }
                            None => {
                                self.labels.remove(target_id);
                            }
                        }
                        Ok(())
                    }
                }
            }
        }
    }
}

fn fact_seq(fact: &Fact) -> u64 {
    match fact {
        Fact::Name { seq, .. } => *seq,
        Fact::Label { seq, .. } => *seq,
    }
}
