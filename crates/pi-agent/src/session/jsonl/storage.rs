//! Durable JSONL session storage — official v3 files with backward-compatible
//! reads/writes for the prior native v4 format.

use super::super::state::{BranchBounds, EntryQuery, LogOptions, RecordQuery, SessionState};
use super::super::types::{
    Entry, EntryNoStats, Fact, JsonlV4Header, LanePointer, LaneRecord, LogItem, Mutation,
    NewRecord, SessionError, SessionErrorKind, SessionMetadata, SessionStats,
};
use super::errors::{file_result, JsonlDecodeError, JsonlDecodeErrorKind};
use super::v3;
use super::{encode_header, encode_mutation, metadata_from_header, parse_header, parse_mutation};
use crate::fs::FileSystem;
use crate::types::FileError;

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionFileFormat {
    V3,
    V4,
}

/// Build a complete sibling temporary file, then atomically rename it over
/// the destination. Mirrors `publishFileAtomically` (crash leaves only the
/// ignored `.tmp` file; failures clean it up best-effort).
async fn publish_file_atomically<F: FileSystem>(
    fs: &F,
    destination_path: &str,
    populate: impl FnOnce(&F, &str) -> Result<(), FileError>,
) -> Result<(), FileError> {
    let temp_path = format!("{destination_path}.tmp");
    let result = (|| {
        populate(fs, &temp_path)?;
        file_result(
            fs.rename_file(&temp_path, destination_path),
            &format!("Failed to publish staged file {destination_path}"),
        )
    })();
    if let Err(e) = result {
        let _ = fs.remove(&temp_path);
        return Err(e);
    }
    Ok(())
}

#[derive(Debug)]
pub struct JsonlSessionStorage<F: FileSystem> {
    fs: F,
    metadata: SessionMetadata,
    state: SessionState,
    format: SessionFileFormat,
}

impl<F: FileSystem> JsonlSessionStorage<F> {
    pub fn new(fs: F, metadata: SessionMetadata) -> Self {
        Self {
            fs,
            metadata,
            state: SessionState::default(),
            format: SessionFileFormat::V4,
        }
    }

    pub async fn create(fs: F, path: &str, header: JsonlV4Header) -> Result<Self, FileError> {
        Self::create_with_format(fs, path, header, SessionFileFormat::V4).await
    }

    pub async fn create_v3(fs: F, path: &str, header: JsonlV4Header) -> Result<Self, FileError> {
        Self::create_with_format(fs, path, header, SessionFileFormat::V3).await
    }

    async fn create_with_format(
        fs: F,
        path: &str,
        header: JsonlV4Header,
        format: SessionFileFormat,
    ) -> Result<Self, FileError> {
        let encoded_header = if format == SessionFileFormat::V3 {
            v3::encode_header(&header).map_err(to_fs)?
        } else {
            encode_header(&header).map_err(to_fs)?
        };
        file_result(
            fs.write_file(path, &encoded_header),
            &format!("Failed to initialize session {path}"),
        )?;
        let file_info = file_result(
            fs.file_info(path),
            &format!("Failed to read session metadata {path}"),
        )?;
        let mut metadata = metadata_from_header(&header, path, file_info.mtime_ms);
        metadata.source_format = if format == SessionFileFormat::V3 {
            3
        } else {
            4
        };
        let storage = Self {
            fs,
            metadata,
            state: SessionState::default(),
            format,
        };
        Ok(storage)
    }

    pub async fn load(fs: F, path: &str) -> Result<Self, LoadError> {
        let content = file_result(
            fs.read_text_file(path),
            &format!("Failed to read session {path}"),
        )
        .map_err(LoadError::Io)?;
        let physical_lines: Vec<&str> = content.split('\n').collect();
        let mut physical_lines: Vec<&str> = physical_lines.into_iter().collect();
        if physical_lines.last().copied() == Some("") {
            physical_lines.pop();
        }
        if physical_lines.is_empty() || physical_lines[0].is_empty() {
            return Err(LoadError::InvalidFile {
                path: path.to_string(),
                line: 1,
                kind: JsonlDecodeErrorKind::Schema,
                message: "is missing a header".to_string(),
            });
        }
        let (header, format) = match parse_header(physical_lines[0]) {
            Ok(header) => (header, SessionFileFormat::V4),
            Err(v4_error) => match v3::parse_header(physical_lines[0]) {
                Ok(header) => (header, SessionFileFormat::V3),
                Err(_) => return Err(invalid_file(path, 1, v4_error)),
            },
        };
        let file_info = file_result(
            fs.file_info(path),
            &format!("Failed to read session metadata {path}"),
        )
        .map_err(LoadError::Io)?;
        let mut metadata = metadata_from_header(&header, path, file_info.mtime_ms);
        metadata.source_format = if format == SessionFileFormat::V3 {
            3
        } else {
            4
        };
        let mut storage = Self {
            fs,
            metadata,
            state: SessionState::default(),
            format,
        };
        let mut torn_tail_repaired = false;
        let mut legacy_seq = 0u64;
        for (index, line) in physical_lines.iter().enumerate().skip(1) {
            if format == SessionFileFormat::V3 && line.trim().is_empty() {
                continue;
            }
            if format == SessionFileFormat::V3 {
                legacy_seq += 1;
            }
            let mutation = match if format == SessionFileFormat::V3 {
                v3::parse_entry(line, legacy_seq)
            } else {
                parse_mutation(line)
            } {
                Ok(m) => m,
                Err(e) => {
                    let is_torn_tail =
                        index == physical_lines.len() - 1 && e.kind == JsonlDecodeErrorKind::Syntax;
                    if is_torn_tail {
                        torn_tail_repaired = true;
                        let valid_prefix = format!("{}\n", physical_lines[..index].join("\n"));
                        publish_file_atomically(&storage.fs, path, |fs, temp| {
                            file_result(
                                fs.write_file(temp, &valid_prefix),
                                &format!("Failed to stage torn-tail repair {path}"),
                            )
                        })
                        .await
                        .map_err(LoadError::Io)?;
                        break;
                    }
                    return Err(LoadError::InvalidFile {
                        path: path.to_string(),
                        line: index + 1,
                        kind: e.kind,
                        message: e.message,
                    });
                }
            };
            storage
                .state
                .apply_mutation(&mutation)
                .map_err(|e| LoadError::InvalidMutation {
                    path: path.to_string(),
                    line: index + 1,
                    error: e,
                })?;
            if format == SessionFileFormat::V3 {
                if let Mutation::Entry { entry, .. } = &mutation {
                    storage.state.set_main_leaf_for_legacy(entry.id());
                }
            }
        }
        if !torn_tail_repaired && !content.ends_with('\n') {
            file_result(
                storage.fs.append_file(path, "\n"),
                &format!("Failed to repair unterminated session tail {path}"),
            )
            .map_err(LoadError::Io)?;
        }
        Ok(storage)
    }

    pub async fn get_metadata(&self) -> SessionMetadata {
        self.metadata.clone()
    }

    pub async fn get_lanes(&self) -> Vec<LanePointer> {
        self.state.get_lanes()
    }

    /// Forks this session into a new file. Mirrors upstream storage.fork:
    /// computes fork mutations from the current state, publishes them
    /// atomically next to the destination (temp sibling + rename), then
    /// reloads the committed file.
    pub async fn fork(
        &self,
        path: &str,
        header: JsonlV4Header,
        options: &super::super::state::ForkOptions,
    ) -> Result<Self, ForkError>
    where
        F: Clone,
    {
        self.fork_with_format(path, header, options, self.format)
            .await
    }

    /// Explicitly stage a v3 fork for coding-agent's upstream-compatible
    /// durable session path, even when the source is a native v4 session.
    pub async fn fork_v3(
        &self,
        path: &str,
        header: JsonlV4Header,
        options: &super::super::state::ForkOptions,
    ) -> Result<Self, ForkError>
    where
        F: Clone,
    {
        self.fork_with_format(path, header, options, SessionFileFormat::V3)
            .await
    }

    async fn fork_with_format(
        &self,
        path: &str,
        header: JsonlV4Header,
        options: &super::super::state::ForkOptions,
        format: SessionFileFormat,
    ) -> Result<Self, ForkError>
    where
        F: Clone,
    {
        let mutations = self
            .state
            .create_fork_mutations(options)
            .map_err(ForkError::Session)?;
        let temp_path = format!("{path}.tmp");
        let fs = self.fs.clone();
        let staged: Result<(), FileError> = async {
            let mut target = match format {
                SessionFileFormat::V3 => {
                    JsonlSessionStorage::create_v3(fs.clone(), &temp_path, header).await
                }
                SessionFileFormat::V4 => {
                    JsonlSessionStorage::create(fs.clone(), &temp_path, header).await
                }
            }
            .map_err(|e| FileError::new(format!("Failed to stage fork {path}: {e}")))?;
            for mutation in &mutations {
                target
                    .append_mutation(mutation)
                    .await
                    .map_err(|e| FileError::new(format!("Failed to stage fork {path}: {e}")))?;
                target
                    .state
                    .apply_mutation(mutation)
                    .map_err(|e| FileError::new(format!("Failed to stage fork {path}: {e}")))?;
            }
            file_result(
                fs.rename_file(&temp_path, path),
                &format!("Failed to publish staged file {path}"),
            )
        }
        .await;
        if let Err(e) = staged {
            let _ = fs.remove(&temp_path);
            return Err(ForkError::Storage(e));
        }
        JsonlSessionStorage::load(fs, path)
            .await
            .map_err(|e| ForkError::Load(e.to_string()))
    }

    pub async fn create_lane(&mut self, lane: &str, at: Option<&str>) -> Result<(), SessionError> {
        self.state.validate_new_lane(lane)?;
        self.state.validate_target(at)?;
        let mutation = Mutation::Lane {
            seq: self.state.next_sequence(),
            lane: lane.to_string(),
            leaf_id: at.map(|s| s.to_string()),
        };
        self.append_mutation(&mutation).await?;
        self.state.apply_mutation(&mutation)?;
        Ok(())
    }

    pub async fn move_lane(&mut self, lane: &str, to: Option<&str>) -> Result<(), SessionError> {
        self.state.require_lane(lane)?;
        self.state.validate_target(to)?;
        let mutation = Mutation::Lane {
            seq: self.state.next_sequence(),
            lane: lane.to_string(),
            leaf_id: to.map(|s| s.to_string()),
        };
        self.append_mutation(&mutation).await?;
        self.state.apply_mutation(&mutation)?;
        Ok(())
    }

    pub async fn append_entry(
        &mut self,
        entry: EntryNoStats,
        lane: &str,
    ) -> Result<Entry, SessionError> {
        let parent_id = self.state.require_lane(lane)?;
        self.state.validate_unused_id(entry.id())?;
        let entry = Entry::from_provisioned(entry, parent_id, self.state.next_sequence(), now_ms());
        let mutation = Mutation::Entry {
            lane: Some(lane.to_string()),
            entry: entry.clone(),
        };
        self.append_mutation(&mutation).await?;
        self.state.apply_mutation(&mutation)?;
        Ok(entry)
    }

    pub async fn append_custom_entry(
        &mut self,
        custom_type: &str,
        data: Option<serde_json::Value>,
    ) -> Result<Entry, SessionError> {
        let id = format!("custom-{}", self.state.next_sequence());
        self.append_entry(
            EntryNoStats::Custom {
                id,
                custom_type: custom_type.to_string(),
                data,
            },
            "main",
        )
        .await
    }

    pub async fn append_record(
        &mut self,
        new_record: NewRecord,
    ) -> Result<LaneRecord, SessionError> {
        self.state.require_lane(new_record.lane())?;
        self.state.validate_unused_id(new_record.id())?;
        let current_open_operation_ids = self.state.open_operation_ids(new_record.lane());
        if new_record.record_type() == "operation_started" && !current_open_operation_ids.is_empty()
        {
            return Err(SessionError::new(
                SessionErrorKind::Storage,
                format!(
                    "Lane {} already has an open operation {}",
                    new_record.lane(),
                    current_open_operation_ids[0]
                ),
            ));
        }
        let record = new_record_complete(new_record, self.state.next_sequence(), now_ms());
        let mutation = Mutation::Record {
            record: record.clone(),
        };
        self.append_mutation(&mutation).await?;
        self.state.apply_mutation(&mutation)?;
        Ok(record)
    }

    pub async fn get_entry(&self, id: &str) -> Option<Entry> {
        self.state.get_entry(id).cloned()
    }

    pub async fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, SessionError> {
        self.state.find_entries(query)
    }

    pub async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        start: &str,
        bounds: &BranchBounds,
    ) -> Result<Vec<Entry>, SessionError> {
        self.state.find_entries_on_branch(query, start, bounds)
    }

    pub async fn find_records(&self, query: &RecordQuery) -> Result<Vec<LaneRecord>, SessionError> {
        self.state.find_records(query)
    }

    pub async fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> Result<Vec<LaneRecord>, SessionError> {
        self.state.find_open_operations(lane, limit)
    }

    pub async fn get_log(&self, options: &LogOptions) -> Result<Vec<LogItem>, SessionError> {
        self.state.get_log(options)
    }

    pub async fn get_stats(&self) -> SessionStats {
        self.state.get_stats().clone()
    }

    pub async fn get_name(&self) -> Option<String> {
        self.state.get_name().map(|s| s.to_string())
    }

    pub async fn set_name(&mut self, name: Option<&str>) -> Result<(), SessionError> {
        let mutation = Mutation::Fact(Fact::Name {
            seq: self.state.next_sequence(),
            name: name.map(|s| s.to_string()),
        });
        self.append_mutation(&mutation).await?;
        self.state.apply_mutation(&mutation)?;
        Ok(())
    }

    pub async fn get_label(&self, id: &str) -> Option<String> {
        self.state.get_label(id).map(|s| s.to_string())
    }

    pub async fn set_label(&mut self, id: &str, label: Option<&str>) -> Result<(), SessionError> {
        let mutation = Mutation::Fact(Fact::Label {
            seq: self.state.next_sequence(),
            target_id: id.to_string(),
            label: label.map(|s| s.to_string()),
        });
        self.append_mutation(&mutation).await?;
        self.state.apply_mutation(&mutation)?;
        Ok(())
    }

    /// Apply a mutation without persisting (used by fork staging after the
    /// line was appended).
    pub fn apply_mutation_unchecked(&mut self, mutation: &Mutation) -> Result<(), SessionError> {
        self.state.apply_mutation(mutation)
    }

    pub fn state(&self) -> &SessionState {
        &self.state
    }

    async fn append_mutation(&mut self, mutation: &Mutation) -> Result<(), SessionError> {
        let line = if self.format == SessionFileFormat::V3 {
            v3::encode_mutation(mutation, now_ms()).map_err(|e| {
                SessionError::new(
                    SessionErrorKind::Storage,
                    format!("failed to encode session entry: {e}"),
                )
            })?
        } else {
            encode_mutation(mutation).map(Some).map_err(|e| {
                SessionError::new(
                    SessionErrorKind::Storage,
                    format!("failed to encode mutation: {e}"),
                )
            })?
        };
        let Some(line) = line else {
            return Ok(());
        };
        file_result(
            self.fs.append_file(&self.metadata.path, &line),
            &format!("Failed to append session {}", self.metadata.path),
        )
        .map_err(|e| SessionError::new(SessionErrorKind::Storage, e.message))?;
        Ok(())
    }
}

fn new_record_complete(new_record: NewRecord, seq: u64, timestamp: u64) -> LaneRecord {
    match new_record {
        NewRecord::OperationStarted {
            id,
            lane,
            source_leaf_id,
            intent,
        } => LaneRecord::OperationStarted {
            id,
            seq,
            lane,
            timestamp,
            source_leaf_id,
            intent,
        },
        NewRecord::AbortRequested { id, lane, run_id } => LaneRecord::AbortRequested {
            id,
            seq,
            lane,
            timestamp,
            run_id,
        },
        NewRecord::OperationFinished {
            id,
            lane,
            run_id,
            outcome,
            error,
        } => LaneRecord::OperationFinished {
            id,
            seq,
            lane,
            timestamp,
            run_id,
            outcome,
            error,
        },
        NewRecord::StepAttempt {
            id,
            lane,
            run_id,
            step,
            attempt,
            result_entry_id,
            compaction_reason,
        } => LaneRecord::StepAttempt {
            id,
            seq,
            lane,
            timestamp,
            run_id,
            step,
            attempt,
            result_entry_id,
            compaction_reason,
        },
        NewRecord::ToolStarted {
            id,
            lane,
            run_id,
            assistant_entry_id,
            tool_index,
            tool_call_id,
            tool_name,
            effective_args,
            result_entry_id,
            replay,
        } => LaneRecord::ToolStarted {
            id,
            seq,
            lane,
            timestamp,
            run_id,
            assistant_entry_id,
            tool_index,
            tool_call_id,
            tool_name,
            effective_args,
            result_entry_id,
            replay,
        },
        NewRecord::QueueEnqueued {
            id,
            lane,
            queue,
            run_id,
            target,
        } => LaneRecord::QueueEnqueued {
            id,
            seq,
            lane,
            timestamp,
            queue,
            run_id,
            target,
        },
        NewRecord::QueueCancelled {
            id,
            lane,
            run_id,
            entry_id,
        } => LaneRecord::QueueCancelled {
            id,
            seq,
            lane,
            timestamp,
            run_id,
            entry_id,
        },
        NewRecord::WriteDeferred {
            id,
            lane,
            run_id,
            target,
        } => LaneRecord::WriteDeferred {
            id,
            seq,
            lane,
            timestamp,
            run_id,
            target,
        },
        NewRecord::Usage {
            id,
            lane,
            cause,
            run_id,
            entry_id,
            attempt,
            stop_reason,
            tool_call_id,
            details,
            usage,
        } => LaneRecord::Usage {
            id,
            seq,
            lane,
            timestamp,
            cause,
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

fn to_fs(e: impl std::fmt::Display) -> FileError {
    FileError::new(e.to_string())
}

/// Errors raised by `fork`.
#[derive(Debug, thiserror::Error)]
pub enum ForkError {
    #[error("session error: {0}")]
    Session(SessionError),
    #[error("storage error: {0}")]
    Storage(#[from] FileError),
    #[error("load error: {0}")]
    Load(String),
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("{0}")]
    Io(#[from] FileError),
    #[error("invalid session file {path}!:{line}: {kind:?}: {message}")]
    InvalidFile {
        path: String,
        line: usize,
        kind: JsonlDecodeErrorKind,
        message: String,
    },
    #[error("invalid session mutation at {path}!:{line}: {error}")]
    InvalidMutation {
        path: String,
        line: usize,
        error: SessionError,
    },
}

fn invalid_file(path: &str, line: usize, error: JsonlDecodeError) -> LoadError {
    LoadError::InvalidFile {
        path: path.to_string(),
        line,
        kind: error.kind,
        message: error.message,
    }
}
