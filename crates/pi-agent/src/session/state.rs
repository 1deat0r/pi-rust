//! In-memory session state — port of `packages/agent/src/harness/session/state.ts`
//! (the portion the JSONL storage needs: mutations, lanes, queries, log, stats).

use std::collections::{BTreeMap, HashSet};

use indexmap::IndexMap;

use super::types::{
    session_error, set_entry_seq, Entry, Fact, LanePointer, LaneRecord, LogItem, Mutation,
    SessionError, SessionErrorKind, SessionStats,
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
    /// `afterSeq`: exclusive chronological lower bound (seq > afterSeq,
    /// regardless of order).
    pub after_seq: Option<u64>,
    pub limit: Option<usize>,
}

/// `LogOptions` from session/types.ts: order, afterSeq, limit.
#[derive(Debug, Clone, Default)]
pub struct LogOptions {
    pub after_seq: Option<u64>,
    pub limit: Option<usize>,
}

/// `BranchBounds` from session/types.ts: stopAtId / stopAtType.
#[derive(Debug, Clone, Default)]
pub struct BranchBounds {
    pub stop_at_id: Option<String>,
    pub stop_at_type: Option<String>,
}

/// Fork scope options — port of `ForkOptions` from session/types.ts.
#[derive(Debug, Clone)]
pub enum ForkOptions {
    /// Copy the entire tree (all entries, lanes, name, labels).
    Tree,
    /// Copy the branch ending at the given entry (defaults to the main lane
    /// leaf), optionally positioning before it.
    Branch {
        entry_id: Option<String>,
        position: Option<ForkPosition>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkPosition {
    Before,
    At,
}

#[derive(Debug)]
pub struct SessionState {
    sequence: u64,
    used_ids: HashSet<String>,
    entries: Vec<Entry>,
    records: Vec<LaneRecord>,
    open_operations_by_lane: BTreeMap<String, Vec<String>>, // lane -> open op ids (insertion order)
    lanes: IndexMap<String, Option<String>>,                // lane -> leaf id (insertion order)
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
            lanes: IndexMap::from([("main".to_string(), None)]),
            name: None,
            labels: BTreeMap::new(),
            stats: SessionStats::default(),
            log: Vec::new(),
        }
    }
}

/// `matchesEntryQuery` — the upstream entry query predicate (type,
/// customType, order-dependent cursor).
pub fn matches_entry_query(entry: &Entry, query: &EntryQuery) -> bool {
    if let Some(id) = &query.id {
        if entry.id() != id {
            return false;
        }
    }
    if let Some(t) = &query.entry_type {
        if entry.entry_type_str() != t {
            return false;
        }
    }
    if let Some(ct) = &query.custom_type {
        if let Entry::Custom { custom_type, .. } = entry {
            if custom_type != ct {
                return false;
            }
        } else {
            return false;
        }
    }
    if let Some(cursor) = &query.cursor {
        // Upstream: oldestFirst keeps seq > afterSeq; newestFirst keeps seq < afterSeq.
        if query.order == Some(EntryOrder::OldestFirst) {
            if entry.seq() <= cursor.after_seq {
                return false;
            }
        } else if entry.seq() >= cursor.after_seq {
            return false;
        }
    }
    true
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
            // Upstream validateNewLane: already_exists (`Lane already exists`).
            Err(session_error(SessionErrorKind::AlreadyExists, format!("Lane already exists: {lane}")))
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
            // Upstream validateTarget: not_found (`Entry not found`).
            .ok_or_else(|| session_error(SessionErrorKind::NotFound, format!("Entry not found: {id}")))
    }

    pub fn validate_unused_id(&self, id: &str) -> Result<(), SessionError> {
        if self.used_ids.contains(id) {
            // Upstream validateUnusedId: already_exists (`Session id already exists`).
            Err(session_error(SessionErrorKind::AlreadyExists, format!("Session id already exists: {id}")))
        } else {
            Ok(())
        }
    }

    /// Internal open-operation ids for a lane (oldest first) — used by the
    /// one-open-operation-per-lane enforcement.
    pub fn open_operation_ids(&self, lane: &str) -> Vec<String> {
        self.open_operations_by_lane.get(lane).cloned().unwrap_or_default()
    }

    /// `findOpenOperations(lane, { limit })` — full operation-started records,
    /// newest first, validated limit.
    pub fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> Result<Vec<LaneRecord>, SessionError> {
        Self::assert_valid_limit(limit)?;
        let ids: Vec<String> = self.open_operations_by_lane.get(lane).cloned().unwrap_or_default();
        let mut ops: Vec<LaneRecord> = ids
            .iter()
            .filter_map(|id| self.records.iter().find(|r| r.id() == id).cloned())
            .collect();
        ops.reverse();
        if let Some(limit) = limit {
            ops.truncate(limit);
        }
        Ok(ops)
    }

    pub fn get_entry(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id() == id)
    }

    fn assert_valid_limit(limit: Option<usize>) -> Result<(), SessionError> {
        if let Some(limit) = limit {
            if limit == 0 {
                return Err(session_error(SessionErrorKind::InvalidQuery, "limit must be a positive integer"));
            }
        }
        Ok(())
    }

    fn assert_valid_cursor(after_seq: Option<u64>) -> Result<(), SessionError> {
        // usize/u64 cursors cannot be negative; upstream rejects negative.
        let _ = after_seq;
        Ok(())
    }

    pub fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, SessionError> {
        Self::assert_valid_limit(query.limit)?;
        Self::assert_valid_cursor(query.cursor.map(|c| c.after_seq))?;
        let mut items: Vec<&Entry> = self.entries.iter().collect();
        items.retain(|e| matches_entry_query(e, query));
        let limit = query.limit.unwrap_or(usize::MAX);
        let mut results = Vec::with_capacity(items.len().min(limit));
        if query.order == Some(EntryOrder::OldestFirst) {
            // already insertion order (ascending seq)
        } else {
            items.reverse();
        }
        for entry in items {
            results.push(entry.clone());
            if results.len() == limit {
                break;
            }
        }
        Ok(results)
    }

    /// `walkToRoot(start, bounds)` — yields entries from `start` toward the
    /// root, stopping at (and including) the first bound hit. Throws
    /// `not_found` on missing entries and `invalid_entry` on cycles.
    fn walk_to_root<'a>(
        &'a self,
        start: &str,
        bounds: Option<&BranchBounds>,
    ) -> Result<Vec<&'a Entry>, SessionError> {
        let mut visited = HashSet::new();
        let mut current: Option<&Entry> = self.entries.iter().find(|e| e.id() == start);
        let mut out = Vec::new();
        while let Some(entry) = current {
            if !visited.insert(entry.id().to_string()) {
                return Err(session_error(
                    SessionErrorKind::InvalidEntry,
                    format!("Session branch contains a cycle at {}", entry.id()),
                ));
            }
            let reached_bound = bounds
                .map(|b| {
                    Some(entry.id().to_string()) == b.stop_at_id
                        || b.stop_at_type.as_deref() == Some(entry.entry_type_str())
                })
                .unwrap_or(false);
            let parent = entry.parent_id().map(|s| s.to_string());
            out.push(entry);
            if reached_bound || parent.is_none() || parent.as_deref() == Some("") {
                break;
            }
            current = self
                .entries
                .iter()
                .find(|e| e.id() == parent.as_deref().unwrap_or_default());
            if current.is_none() {
                return Err(session_error(
                    SessionErrorKind::NotFound,
                    format!("Entry not found: {}", parent.unwrap()),
                ));
            }
        }
        if out.is_empty() {
            return Err(session_error(SessionErrorKind::NotFound, format!("Entry not found: {start}")));
        }
        Ok(out)
    }

    /// `findEntriesOnBranch` — the full upstream branch query: order, type and
    /// customType filters, cursor, limit, stopAtId/stopAtType bounds, cycle
    /// detection, and `not_found` for a missing start.
    pub fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        start: &str,
        bounds: &BranchBounds,
    ) -> Result<Vec<Entry>, SessionError> {
        Self::assert_valid_limit(query.limit)?;
        Self::assert_valid_cursor(query.cursor.map(|c| c.after_seq))?;
        let limit = query.limit.unwrap_or(usize::MAX);
        let mut results: Vec<Entry> = Vec::new();
        if query.order == Some(EntryOrder::OldestFirst) {
            let mut walked = self.walk_to_root(start, None)?;
            walked.reverse();
            for entry in walked {
                let reached_bound = Some(entry.id().to_string()) == bounds.stop_at_id
                    || bounds.stop_at_type.as_deref() == Some(entry.entry_type_str());
                if matches_entry_query(entry, query) {
                    results.push(entry.clone());
                }
                if reached_bound || results.len() == limit {
                    break;
                }
            }
        } else {
            for entry in self.walk_to_root(start, Some(bounds))? {
                if matches_entry_query(entry, query) {
                    results.push(entry.clone());
                }
                if results.len() == limit {
                    break;
                }
            }
        }
        Ok(results)
    }

    pub fn find_records(&self, query: &RecordQuery) -> Result<Vec<LaneRecord>, SessionError> {
        Self::assert_valid_limit(query.limit)?;
        Self::assert_valid_cursor(query.after_seq)?;
        let mut items: Vec<&LaneRecord> = self.records.iter().collect();
        items.retain(|r| {
            if let Some(t) = &query.record_type {
                if r.record_type() != t {
                    return false;
                }
            }
            if let Some(run_id) = &query.run_id {
                // Upstream: runId matches OperationStartedRecord.id and the
                // runId property of operation-owned records.
                let matches = match r {
                    LaneRecord::OperationStarted { id, .. } => id == run_id,
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
            if let Some(after_seq) = &query.after_seq {
                if r.seq() <= *after_seq {
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
        let mut results = Vec::with_capacity(items.len().min(limit));
        for record in items {
            results.push(record.clone());
            if results.len() == limit {
                break;
            }
        }
        Ok(results)
    }

    /// `getLog(options)` — insertion order with optional afterSeq filter and
    /// limit (upstream `state.getLog`).
    pub fn get_log(&self, options: &LogOptions) -> Result<Vec<LogItem>, SessionError> {
        Self::assert_valid_limit(options.limit)?;
        Self::assert_valid_cursor(options.after_seq)?;
        let mut results = Vec::new();
        for item in &self.log {
            if let Some(after_seq) = options.after_seq {
                if item.seq() <= after_seq {
                    continue;
                }
            }
            results.push(item.clone());
            if results.len() == options.limit.unwrap_or(usize::MAX) {
                break;
            }
        }
        Ok(results)
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

    /// Builds the mutation list for a fork (port of
    /// `SessionState.createForkMutations`): copied entries renumbered from 1,
    /// then lane lines, then the name fact, then label facts.
    pub fn create_fork_mutations(&self, options: &ForkOptions) -> Result<Vec<Mutation>, SessionError> {
        let (copied_entries, fork_lanes) = match options {
            ForkOptions::Tree => {
                let entries = self.find_entries(&EntryQuery { order: Some(EntryOrder::OldestFirst), ..Default::default() })?;
                (entries, self.get_lanes())
            }
            ForkOptions::Branch { entry_id, position } => {
                let selected = match entry_id {
                    Some(id) => Some(id.clone()),
                    None => self.require_lane("main")?,
                };
                let mut target_id: Option<String> = None;
                if let Some(selected) = selected {
                    // Upstream: `!entry || entry.type !== "message"` →
                    // invalid_fork_target with the same message either way.
                    let entry = self
                        .get_entry(&selected)
                        .cloned()
                        .ok_or_else(|| {
                            session_error(
                                SessionErrorKind::InvalidForkTarget,
                                format!("Fork target is not a message entry: {selected}"),
                            )
                        })?;
                    if entry.entry_type_str() != "message" {
                        return Err(session_error(
                            SessionErrorKind::InvalidForkTarget,
                            format!("Fork target is not a message entry: {selected}"),
                        ));
                    }
                    let position = match position {
                        Some(p) => *p,
                        None => {
                            if entry_id.is_some() {
                                ForkPosition::Before
                            } else {
                                ForkPosition::At
                            }
                        }
                    };
                    target_id = match position {
                        ForkPosition::At => Some(entry.id().to_string()),
                        ForkPosition::Before => entry.parent_id().map(|s| s.to_string()),
                    };
                }
                let entries = match &target_id {
                    Some(t) => {
                        let mut cursor = t.clone();
                        let mut out = Vec::new();
                        loop {
                            let Some(entry) = self.entries.iter().find(|e| e.id() == cursor) else {
                                break;
                            };
                            let clone = entry.clone();
                            match entry.parent_id() {
                                Some(p) if !p.is_empty() => {
                                    cursor = p.to_string();
                                    out.push(clone);
                                }
                                _ => {
                                    out.push(clone);
                                    break;
                                }
                            }
                        }
                        out.reverse();
                        out
                    }
                    None => Vec::new(),
                };
                (entries, vec![crate::session::types::LanePointer { lane: "main".into(), leaf_id: target_id }])
            }
        };

        let mut mutations: Vec<Mutation> = Vec::new();
        let mut sequence: u64 = 1;
        for source_entry in &copied_entries {
            let mut entry = source_entry.clone();
            set_entry_seq(&mut entry, sequence);
            mutations.push(Mutation::Entry { lane: None, entry });
            sequence += 1;
        }
        for pointer in &fork_lanes {
            mutations.push(Mutation::Lane { seq: sequence, lane: pointer.lane.clone(), leaf_id: pointer.leaf_id.clone() });
            sequence += 1;
        }
        if let Some(name) = &self.name {
            mutations.push(Mutation::Fact(crate::session::types::Fact::Name { seq: sequence, name: Some(name.clone()) }));
            sequence += 1;
        }
        for entry in &copied_entries {
            if let Some(label) = self.labels.get(entry.id()) {
                mutations.push(Mutation::Fact(crate::session::types::Fact::Label {
                    seq: sequence,
                    target_id: entry.id().to_string(),
                    label: Some(label.clone()),
                }));
                sequence += 1;
            }
        }
        Ok(mutations)
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
                if let Some(parent) = entry.parent_id() {
                    if !parent.is_empty() && !self.entries.iter().any(|e| e.id() == parent) {
                        return Err(session_error(
                            SessionErrorKind::InvalidEntry,
                            format!("references missing parent {parent}"),
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
                if let LaneRecord::OperationFinished { run_id, .. } = &record {
                    if let Some(ops) = self.open_operations_by_lane.get_mut(record.lane()) {
                        ops.retain(|id| id != run_id);
                    }
                }
                if let LaneRecord::Usage { usage, .. } = &record {
                    self.stats.cached_tokens += usage.cache_read;
                    self.stats.uncached_tokens += usage.input + usage.cache_write;
                    self.stats.total_tokens += usage.total_tokens;
                    self.stats.cost_total += usage.cost.total;
                }
                self.used_ids.insert(record.id().to_string());
                self.sequence = self.sequence.max(seq);
                self.records.push(record.clone());
                self.log.push(LogItem::Record(record));
                Ok(())
            }
            Mutation::Lane { seq, lane, leaf_id } => {
                // Upstream applyMutation simply sets the pointer; creation is
                // guarded by validateNewLane in the storage layer before the
                // mutation is persisted.
                if let Some(leaf) = leaf_id {
                    self.require_entry_id(leaf)?;
                }
                self.sequence = self.sequence.max(*seq);
                self.lanes.insert(lane.clone(), leaf_id.clone());
                self.log.push(crate::session::types::LogItem::Lane {
                    seq: *seq,
                    lane: lane.clone(),
                    leaf_id: leaf_id.clone(),
                });
                Ok(())
            }
            Mutation::Fact(fact) => {
                let seq = fact_seq(fact);
                self.sequence = self.sequence.max(seq);
                match fact {
                    Fact::Name { name, .. } => {
                        self.name = name.clone();
                        self.log.push(crate::session::types::LogItem::Fact(
                            crate::session::types::FactLogItem {
                                seq,
                                fact: "name".to_string(),
                                name: name.clone(),
                                target_id: None,
                                label: None,
                            },
                        ));
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
                        self.log.push(crate::session::types::LogItem::Fact(
                            crate::session::types::FactLogItem {
                                seq,
                                fact: "label".to_string(),
                                name: None,
                                target_id: Some(target_id.clone()),
                                label: label.clone(),
                            },
                        ));
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
