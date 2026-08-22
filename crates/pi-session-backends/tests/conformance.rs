//! Session backend conformance — port of
//! `packages/agent/src/harness/session/testing/conformance.ts`.
//!
//! Every case runs against BOTH backends: the in-memory repo (`memory.ts`
//! port) and the JSONL repo (`jsonl.ts` port on a MemoryFs). This is the
//! shared contract both must satisfy; JSONL codec specifics are covered by
//! the jsonl_* test files.

use async_trait::async_trait;

use pi_agent::session::state::{BranchBounds, EntryCursor, EntryOrder, EntryQuery, ForkOptions, ForkPosition, LogOptions, RecordQuery};
use pi_agent::session::types::{
    Entry, EntryNoStats, LaneRecord, NewRecord, SessionError, SessionErrorKind, SessionMetadata,
};
use pi_ai::types::{ContentBlock, Cost, Message, StopReason, Usage, UserContent};
use pi_agent::types::AgentMessage;

use pi_session_backends::repo::{ForkCreateOptions, SqliteSessionRepository};
use pi_session_backends::session::SqliteSession;
use pi_session_backends::types::{SqliteSessionCreateOptions, SqliteSessionMetadata, SqliteWriterLeaseOptions};

// ---------------------------------------------------------------------------
// Fixture plumbing
// ---------------------------------------------------------------------------

fn user_message(text: &str) -> AgentMessage {
    AgentMessage::Core(Message::User(UserContent::blocks(vec![ContentBlock::text(text)], 1)))
}

fn assistant_message(text: &str) -> AgentMessage {
    AgentMessage::Core(Message::Assistant(pi_ai::types::AssistantMessage::Assistant {
        content: vec![ContentBlock::text(text)],
        api: Some("anthropic-messages".into()),
        provider: Some("anthropic".into()),
        model: Some("claude-sonnet-4-5".into()),
        response_model: None,
        response_id: None,
        usage: Some(zero_usage()),
        stop_reason: Some(StopReason::Stop),
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 1,
    }))
}

fn zero_usage() -> Usage {
    Usage {
        input: 0, output: 0, cache_read: 0, cache_write: 0, cache_write_1h: None, reasoning: None,
        total_tokens: 0,
        cost: Cost { input: 0.0, output: 0.0, cache_read: 0.0, cache_write: 0.0, total: 0.0 },
    }
}

fn usage_explicit(
    input: u64, output: u64, cache_read: u64, cache_write: u64, total_tokens: u64,
    cost: Cost,
) -> Usage {
    Usage {
        input, output, cache_read, cache_write, cache_write_1h: None, reasoning: None,
        total_tokens, cost,
    }
}

fn operation_started(id: &str, lane: &str, kind: &str) -> NewRecord {
    let intent = match kind {
        "run" => pi_agent::session::types::OperationIntent::Run {
            original_prompt: vec![],
            initial_messages: vec![],
            system_prompt_override: None,
            resume_data: None,
        },
        "compaction" => pi_agent::session::types::OperationIntent::Compaction {
            custom_instructions: None,
            result_entry_id: format!("{id}-result"),
        },
        "navigation" => pi_agent::session::types::OperationIntent::Navigation {
            target_id: None,
            summarize: false,
            custom_instructions: None,
            label: None,
            summary_entry_id: None,
        },
        _ => unreachable!("unknown operation kind"),
    };
    NewRecord::OperationStarted {
        id: id.into(),
        lane: lane.into(),
        source_leaf_id: None,
        intent,
    }
}

fn tool_result_entry() -> EntryNoStats {
    EntryNoStats::Message {
        id: "tool-result".into(),
        message: AgentMessage::Core(Message::ToolResult(pi_ai::types::ToolResultMessage::text(
            "call-1", "example", "done", false,
        ))),
        terminate: Some(true),
    }
}

async fn entry_ids(entries: Vec<Entry>) -> Vec<String> {
    entries.into_iter().map(|e| e.id().to_string()).collect()
}

fn assert_rejects(result: Result<(), SessionError>, code: SessionErrorKind) {
    match result {
        Ok(()) => panic!("expected SessionError {code:?}, got Ok"),
        Err(e) => assert_eq!(e.kind, code, "expected {code:?}, got {:?}", e.kind),
    }
}

// ---------------------------------------------------------------------------
// ConformanceRepo trait + backend adapters
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct RepoCreateOptions {
    pub id: Option<String>,
    pub parent_session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RepoForkOptions {
    pub id: Option<String>,
    pub parent_session_id: Option<String>,
    pub fork_options: ForkOptions,
}

impl Default for RepoForkOptions {
    fn default() -> Self {
        // Upstream ForkOptions defaults: scope "branch", entryId undefined,
        // position undefined (→ default target = main leaf, position "at").
        Self {
            id: None,
            parent_session_id: None,
            fork_options: ForkOptions::Branch { entry_id: None, position: None },
        }
    }
}

#[async_trait]
pub trait ConformanceRepo: Send {
    async fn create(&mut self, options: RepoCreateOptions) -> Result<SqliteSession, SessionError>;
    async fn open(&self, metadata: &SessionMetadata) -> Result<SqliteSession, SessionError>;
    async fn list(&self) -> Result<Vec<SessionMetadata>, SessionError>;
    async fn delete(&mut self, metadata: &SessionMetadata) -> Result<(), SessionError>;
    async fn fork(
        &mut self,
        metadata: &SessionMetadata,
        options: RepoForkOptions,
    ) -> Result<SqliteSession, SessionError>;
}

#[async_trait]
impl ConformanceRepo for SqliteSessionRepository {
    async fn create(&mut self, options: RepoCreateOptions) -> Result<SqliteSession, SessionError> {
        SqliteSessionRepository::create(self, &SqliteSessionCreateOptions {
            id: options.id,
            cwd: "/workspace".to_string(),
            parent_session_id: options.parent_session_id,
            metadata: None,
        })
        .await
    }
    async fn open(&self, metadata: &SessionMetadata) -> Result<SqliteSession, SessionError> {
        self.open(&SqliteSessionMetadata::from_core(metadata)).await
    }
    async fn list(&self) -> Result<Vec<SessionMetadata>, SessionError> {
        Ok(self
            .list(&pi_session_backends::types::SqliteSessionListOptions::default())
            .await?
            .into_iter()
            .map(|m| m.to_core())
            .collect())
    }
    async fn delete(&mut self, metadata: &SessionMetadata) -> Result<(), SessionError> {
        SqliteSessionRepository::delete(self, &SqliteSessionMetadata::from_core(metadata)).await
    }
    async fn fork(
        &mut self,
        metadata: &SessionMetadata,
        options: RepoForkOptions,
    ) -> Result<SqliteSession, SessionError> {
        SqliteSessionRepository::fork(
            self,
            &SqliteSessionMetadata::from_core(metadata),
            &ForkCreateOptions {
                id: options.id,
                cwd: "/workspace".to_string(),
                parent_session_id: options.parent_session_id,
                metadata: None,
                fork_options: options.fork_options,
            },
        )
        .await
    }
}

/// A temp directory removed on drop (mirror of upstream `createTempDir`).
pub struct TempDir {
    path: std::path::PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "pi-session-backend-sqlite-{}-{}",
            std::process::id(),
            test_counter()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

static TEST_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn test_counter() -> u64 {
    TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

fn sqlite_repo() -> Box<dyn ConformanceRepo> {
    let dir = Box::new(TempDir::new());
    let database_path = dir.path().join("sessions.sqlite").to_string_lossy().into_owned();
    // Keep the temp dir alive for the process lifetime; the OS cleans /tmp.
    Box::leak(dir);
    let lease = SqliteWriterLeaseOptions { ttl_ms: 5_000, heartbeat_interval_ms: 1_000 };
    let repo = SqliteSessionRepository::new(database_path, Some(lease));
    Box::new(repo)
}

/// Runs one case body against the SQLite backend.
macro_rules! conformance_case {
    ($name:ident) => {
        #[tokio::test]
        async fn $name() {
            let mut sqlite = sqlite_repo();
            $name::__body(sqlite.as_mut()).await;
        }
    };
}


// ---------------------------------------------------------------------------
// Cases — entries and lanes
// ---------------------------------------------------------------------------

mod assigns_parents_and_one_sequence_across_every_mutation {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut session = repo.create(RepoCreateOptions { id: Some("session".into()), ..Default::default() }).await.unwrap();
        let root = session
            .append_entry(EntryNoStats::Message { id: "root".into(), message: user_message("root"), terminate: None }, "main")
            .await
            .unwrap();
        session.create_lane("thread", Some(root.id())).await.unwrap();
        let child = session
            .append_entry(EntryNoStats::Custom { id: "child".into(), custom_type: "note".into(), data: Some(serde_json::json!({"value": 1})) }, "thread")
            .await
            .unwrap();
        let record = session.append_record(operation_started("run", "thread", "run")).await.unwrap();
        session.set_name(Some("Example")).await.unwrap();
        session.set_label(root.id(), Some("checkpoint")).await.unwrap();
        session.move_lane("main", Some(child.id())).await.unwrap();

        assert_eq!((root.parent_id().map(|s| s.to_string()), root.seq()), (None, 1));
        assert_eq!((child.parent_id().map(|s| s.to_string()), child.seq()), (Some("root".to_string()), 3));
        assert_eq!(record.seq(), 4);
        assert!(root.timestamp() > 0 && child.timestamp() > 0 && record.timestamp() > 0);

        let log = session.get_log(&LogOptions::default()).await.unwrap();
        let kinds: Vec<(String, u64)> = log.iter().map(|item| (item.kind().to_string(), item.seq())).collect();
        assert_eq!(
            kinds,
            vec![
                ("entry".to_string(), 1),
                ("lane".to_string(), 2),
                ("entry".to_string(), 3),
                ("record".to_string(), 4),
                ("fact".to_string(), 5),
                ("fact".to_string(), 6),
                ("lane".to_string(), 7),
            ]
        );
        assert_eq!(
            session.get_lanes().await.unwrap(),
            vec![
                pi_agent::session::types::LanePointer { lane: "main".into(), leaf_id: Some(child.id().to_string()) },
                pi_agent::session::types::LanePointer { lane: "thread".into(), leaf_id: Some(child.id().to_string()) },
            ]
        );
    }
}
conformance_case!(assigns_parents_and_one_sequence_across_every_mutation);

mod commits_records_and_lane_moves_as_separate_mutations {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut session = repo.create(RepoCreateOptions { id: Some("session".into()), ..Default::default() }).await.unwrap();
        session
            .append_entry(EntryNoStats::Message { id: "root".into(), message: user_message("root"), terminate: None }, "main")
            .await
            .unwrap();
        let finished = session
            .append_record(NewRecord::OperationFinished {
                id: "finish".into(), lane: "main".into(), run_id: "run".into(), outcome: "completed".into(), error: None,
            })
            .await
            .unwrap();
        assert_eq!(finished.seq(), 2);
        assert_eq!(session.get_lanes().await.unwrap(), vec![pi_agent::session::types::LanePointer { lane: "main".into(), leaf_id: Some("root".into()) }]);
        session.move_lane("main", None).await.unwrap();
        assert_eq!(session.get_lanes().await.unwrap(), vec![pi_agent::session::types::LanePointer { lane: "main".into(), leaf_id: None }]);
        let log = session.get_log(&LogOptions::default()).await.unwrap();
        assert_eq!(log.len(), 3);
        assert_eq!(log[0].kind(), "entry");
        assert_eq!(log[1].kind(), "record");
        assert_eq!(log[2].kind(), "lane");

        assert_rejects(session.move_lane("main", Some("missing")).await, SessionErrorKind::NotFound);
        assert_eq!(session.find_records(&RecordQuery::default()).await.unwrap().len(), 1);
        let seqs: Vec<u64> = session.get_log(&LogOptions::default()).await.unwrap().iter().map(|i| i.seq()).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
    }
}
conformance_case!(commits_records_and_lane_moves_as_separate_mutations);

mod rejects_duplicate_ids_without_changing_state {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut session = repo.create(RepoCreateOptions { id: Some("session".into()), ..Default::default() }).await.unwrap();
        session
            .append_entry(EntryNoStats::Message { id: "shared".into(), message: user_message("root"), terminate: None }, "main")
            .await
            .unwrap();
        assert_rejects(
            session.append_record(operation_started("shared", "main", "run")).await.map(|_| ()),
            SessionErrorKind::AlreadyExists,
        );
        session.append_record(operation_started("run", "main", "run")).await.unwrap();
        assert_rejects(
            session.append_entry(EntryNoStats::Custom { id: "run".into(), custom_type: "note".into(), data: None }, "main").await.map(|_| ()),
            SessionErrorKind::AlreadyExists,
        );
        let seqs: Vec<u64> = session.get_log(&LogOptions::default()).await.unwrap().iter().map(|i| i.seq()).collect();
        assert_eq!(seqs, vec![1, 2]);
    }
}
conformance_case!(rejects_duplicate_ids_without_changing_state);

mod isolates_lanes_while_sharing_the_tree {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut session = repo.create(RepoCreateOptions { id: Some("session".into()), ..Default::default() }).await.unwrap();
        session
            .append_entry(EntryNoStats::Message { id: "root".into(), message: user_message("root"), terminate: None }, "main")
            .await
            .unwrap();
        session.create_lane("thread", Some("root")).await.unwrap();
        session
            .append_entry(EntryNoStats::Message { id: "main-child".into(), message: user_message("main"), terminate: None }, "main")
            .await
            .unwrap();
        session
            .append_entry(EntryNoStats::Message { id: "thread-child".into(), message: user_message("thread"), terminate: None }, "thread")
            .await
            .unwrap();

        assert_eq!(
            session.get_lanes().await.unwrap(),
            vec![
                pi_agent::session::types::LanePointer { lane: "main".into(), leaf_id: Some("main-child".into()) },
                pi_agent::session::types::LanePointer { lane: "thread".into(), leaf_id: Some("thread-child".into()) },
            ]
        );
        let q = EntryQuery { order: Some(EntryOrder::OldestFirst), ..Default::default() };
        let main_path = entry_ids(session.find_entries_on_branch(&q, Some("main-child"), &BranchBounds::default()).await.unwrap()).await;
        let thread_path = entry_ids(session.find_entries_on_branch(&q, Some("thread-child"), &BranchBounds::default()).await.unwrap()).await;
        assert_eq!(main_path, vec!["root", "main-child"]);
        assert_eq!(thread_path, vec!["root", "thread-child"]);
    }
}
conformance_case!(isolates_lanes_while_sharing_the_tree);

mod validates_lane_lifecycle_and_targets {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut session = repo.create(RepoCreateOptions { id: Some("session".into()), ..Default::default() }).await.unwrap();
        assert_rejects(session.create_lane("main", None).await, SessionErrorKind::AlreadyExists);
        assert_rejects(session.create_lane("thread", Some("missing")).await, SessionErrorKind::NotFound);
        assert_rejects(session.move_lane("missing", None).await, SessionErrorKind::InvalidLane);
    }
}
conformance_case!(validates_lane_lifecycle_and_targets);

mod binds_lane_views_without_caching_leaves {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut session = repo.create(RepoCreateOptions { id: Some("session".into()), ..Default::default() }).await.unwrap();
        let root = session.append_message(user_message("root")).await.unwrap();
        session.create_lane("thread", Some(&root)).await.unwrap();
        let main_child = session.append_message(user_message("main")).await.unwrap();
        let thread_child = {
            let mut view = session.view("thread");
            view.append_message(user_message("thread")).await.unwrap()
        };

        assert_eq!(session.get_leaf_id().await.unwrap(), Some(main_child.clone()));
        assert_eq!(session.view("thread").get_leaf_id().await.unwrap(), Some(thread_child.clone()));
        let q = EntryQuery { order: Some(EntryOrder::OldestFirst), ..Default::default() };
        let main_path = entry_ids(session.find_entries_on_branch(&q, None, &BranchBounds::default()).await.unwrap()).await;
        let thread_path = entry_ids(session.view("thread").find_entries_on_branch(&q, &BranchBounds::default()).await.unwrap()).await;
        assert_eq!(main_path, vec![root.clone(), main_child]);
        assert_eq!(thread_path, vec![root, thread_child]);

        let empty = repo.create(RepoCreateOptions { id: Some("empty".into()), ..Default::default() }).await.unwrap();
        let empty_path = empty.find_entries_on_branch(&EntryQuery::default(), None, &BranchBounds::default()).await.unwrap();
        assert!(empty_path.is_empty());
    }
}
conformance_case!(binds_lane_views_without_caching_leaves);

mod appends_provisioned_entries_with_their_existing_ids {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut session = repo.create(RepoCreateOptions { id: Some("session".into()), ..Default::default() }).await.unwrap();
        let entry = session
            .append_entry(EntryNoStats::Custom { id: "provisioned".into(), custom_type: "note".into(), data: Some(serde_json::json!({"value": 1})) }, "main")
            .await
            .unwrap();
        assert_eq!(entry.entry_type_str(), "custom");
        assert_eq!(entry.custom_type_of().unwrap(), "note");
        assert_eq!((entry.id().to_string(), entry.parent_id().map(|s| s.to_string()), entry.seq()), ("provisioned".to_string(), None, 1));
        assert_eq!(session.get_leaf_id().await.unwrap(), Some("provisioned".to_string()));
    }
}
conformance_case!(appends_provisioned_entries_with_their_existing_ids);

mod persists_tool_result_termination_decisions {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut session = repo.create(RepoCreateOptions { id: Some("session".into()), ..Default::default() }).await.unwrap();
        let entry = session.append_entry(tool_result_entry(), "main").await.unwrap();
        assert_eq!(entry.as_message_terminate(), Some(true));
        let stored = session.get_entry(entry.id()).await.unwrap();
        assert_eq!(stored.as_message_terminate(), Some(true));
        let entries = session.find_entries(&EntryQuery::default()).await.unwrap();
        assert_eq!(entries, vec![entry.clone()]);
        let log = session.get_log(&LogOptions::default()).await.unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].seq(), entry.seq());
        assert_eq!(log[0].kind(), "entry");
    }
}
conformance_case!(persists_tool_result_termination_decisions);

mod linearizes_concurrent_writes_across_two_lanes {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut session = repo.create(RepoCreateOptions { id: Some("session".into()), ..Default::default() }).await.unwrap();
        session
            .append_entry(EntryNoStats::Message { id: "root".into(), message: user_message("root"), terminate: None }, "main")
            .await
            .unwrap();
        session.create_lane("thread", Some("root")).await.unwrap();
        // Sequential equivalent of upstream Promise.all over four interleaved
        // lane writes; the storage must assign a shared, gapless sequence and
        // log writes in commit order.
        let writes: Vec<(&str, &str)> = vec![
            ("main-1", "main"),
            ("thread-1", "thread"),
            ("main-2", "main"),
            ("thread-2", "thread"),
        ];
        let mut committed = Vec::new();
        for (id, lane) in writes {
            let entry = session
                .append_entry(EntryNoStats::Custom { id: id.into(), custom_type: "note".into(), data: None }, lane)
                .await
                .unwrap();
            committed.push(entry);
        }
        assert_eq!(committed.iter().map(|e| e.seq()).collect::<Vec<_>>(), vec![3, 4, 5, 6]);
        assert_eq!(committed.iter().map(|e| e.id().to_string()).collect::<Vec<_>>(), vec!["main-1", "thread-1", "main-2", "thread-2"]);
        let log_ids: Vec<String> = session
            .get_log(&LogOptions::default())
            .await
            .unwrap()
            .iter()
            .filter_map(|item| match item {
                pi_agent::session::types::LogItem::Entry(e) if e.id().starts_with("main-") || e.id().starts_with("thread-") => Some(e.id().to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(log_ids, committed.iter().map(|e| e.id().to_string()).collect::<Vec<_>>());
        let seqs: Vec<u64> = session.get_log(&LogOptions::default()).await.unwrap().iter().map(|i| i.seq()).collect();
        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        assert_eq!(seqs, sorted);
    }
}
conformance_case!(linearizes_concurrent_writes_across_two_lanes);

// ---------------------------------------------------------------------------
// Cases — queries and facts
// ---------------------------------------------------------------------------

mod rejects_invalid_queries_before_empty_reads {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut session = repo.create(RepoCreateOptions { id: Some("invalid-queries".into()), ..Default::default() }).await.unwrap();
        session.create_lane("thread", None).await.unwrap();

        let bad_limit = EntryQuery { limit: Some(0), ..Default::default() };
        assert_rejects(session.find_entries(&bad_limit).await.map(|_| ()), SessionErrorKind::InvalidQuery);
        assert_rejects(session.find_entry(&bad_limit).await.map(|_| ()), SessionErrorKind::InvalidQuery);
        assert_rejects(
            session.find_entries_on_branch(&bad_limit, None, &BranchBounds::default()).await.map(|_| ()),
            SessionErrorKind::InvalidQuery,
        );
        assert_rejects(
            session.find_entry_on_branch(&bad_limit, None, &BranchBounds::default()).await.map(|_| ()),
            SessionErrorKind::InvalidQuery,
        );
        assert_rejects(
            session.view("thread").find_entries_on_branch(&bad_limit, &BranchBounds::default()).await.map(|_| ()),
            SessionErrorKind::InvalidQuery,
        );
        assert_rejects(
            session.view("thread").find_entry_on_branch(&bad_limit, &BranchBounds::default()).await.map(|_| ()),
            SessionErrorKind::InvalidQuery,
        );
        assert_rejects(session.find_records(&RecordQuery { limit: Some(0), ..Default::default() }).await.map(|_| ()), SessionErrorKind::InvalidQuery);
        assert_rejects(session.find_records(&RecordQuery { operation_kind: Some("run".into()), ..Default::default() }).await.map(|_| ()), SessionErrorKind::InvalidQuery);
        assert_rejects(
            session.find_records(&RecordQuery { record_type: Some("step_attempt".into()), operation_kind: Some("run".into()), ..Default::default() }).await.map(|_| ()),
            SessionErrorKind::InvalidQuery,
        );
        assert_rejects(session.find_open_operations("main", Some(0)).await.map(|_| ()), SessionErrorKind::InvalidQuery);
        assert_rejects(session.get_log(&LogOptions { limit: Some(0), ..Default::default() }).await.map(|_| ()), SessionErrorKind::InvalidQuery);
    }
}
conformance_case!(rejects_invalid_queries_before_empty_reads);

mod supports_bounded_filtered_and_cursor_based_queries {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut session = repo.create(RepoCreateOptions { id: Some("session".into()), ..Default::default() }).await.unwrap();
        session
            .append_entry(EntryNoStats::Message { id: "root".into(), message: user_message("root"), terminate: None }, "main")
            .await
            .unwrap();
        session
            .append_entry(EntryNoStats::Custom { id: "old-note".into(), custom_type: "note".into(), data: Some(serde_json::json!(1)) }, "main")
            .await
            .unwrap();
        session
            .append_entry(EntryNoStats::Compaction { id: "compact".into(), summary: "summary".into(), retained_tail: vec![], tokens_before: 10, details: None, usage: None }, "main")
            .await
            .unwrap();
        session
            .append_entry(EntryNoStats::Custom { id: "new-note".into(), custom_type: "note".into(), data: Some(serde_json::json!(2)) }, "main")
            .await
            .unwrap();
        session
            .append_entry(EntryNoStats::Message { id: "tail".into(), message: assistant_message("tail"), terminate: None }, "main")
            .await
            .unwrap();

        let all = entry_ids(session.find_entries(&EntryQuery::default()).await.unwrap()).await;
        assert_eq!(all, vec!["tail", "new-note", "compact", "old-note", "root"]);
        let paged = entry_ids(
            session
                .find_entries(&EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    cursor: Some(EntryCursor { after_seq: 2 }),
                    limit: Some(2),
                    ..Default::default()
                })
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(paged, vec!["compact", "new-note"]);
        let notes = entry_ids(session.find_entries(&EntryQuery { custom_type: Some("note".into()), ..Default::default() }).await.unwrap()).await;
        assert_eq!(notes, vec!["new-note", "old-note"]);

        let q1 = EntryQuery { custom_type: Some("note".into()), limit: Some(1), ..Default::default() };
        let b1 = BranchBounds::default();
        let r1 = entry_ids(session.find_entries_on_branch(&q1, Some("tail"), &b1).await.unwrap()).await;
        assert_eq!(r1, vec!["new-note"]);

        let q2 = EntryQuery { entry_type: Some("message".into()), ..Default::default() };
        let b2 = BranchBounds { stop_at_type: Some("compaction".into()), ..Default::default() };
        let r2 = entry_ids(session.find_entries_on_branch(&q2, Some("tail"), &b2).await.unwrap()).await;
        assert_eq!(r2, vec!["tail"]);

        let q3 = EntryQuery { entry_type: Some("custom".into()), ..Default::default() };
        let b3 = BranchBounds { stop_at_id: Some("tail".into()), ..Default::default() };
        let r3 = entry_ids(session.find_entries_on_branch(&q3, Some("tail"), &b3).await.unwrap()).await;
        assert_eq!(r3, Vec::<String>::new());

        let q4 = EntryQuery { order: Some(EntryOrder::OldestFirst), ..Default::default() };
        let b4 = BranchBounds { stop_at_type: Some("custom".into()), ..Default::default() };
        let r4 = entry_ids(session.find_entries_on_branch(&q4, Some("tail"), &b4).await.unwrap()).await;
        assert_eq!(r4, vec!["root", "old-note"]);

        assert_rejects(session.find_entries(&EntryQuery { limit: Some(0), ..Default::default() }).await.map(|_| ()), SessionErrorKind::InvalidQuery);
        assert_rejects(
            session.find_entries_on_branch(&EntryQuery::default(), Some("missing"), &BranchBounds::default()).await.map(|_| ()),
            SessionErrorKind::NotFound,
        );
    }
}
conformance_case!(supports_bounded_filtered_and_cursor_based_queries);

mod keeps_latest_value_facts_and_computes_ledger_statistics_across_lanes {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut session = repo.create(RepoCreateOptions { id: Some("session".into()), ..Default::default() }).await.unwrap();
        let assistant = assistant_message("answer");
        let assistant_usage = usage_explicit(
            10, 5, 3, 2, 20,
            Cost { input: 1.0, output: 2.0, cache_read: 3.0, cache_write: 4.0, total: 10.0 },
        );
        session
            .append_entry(EntryNoStats::Message { id: "user".into(), message: user_message("question"), terminate: None }, "main")
            .await
            .unwrap();
        session.append_entry(EntryNoStats::Message { id: "assistant".into(), message: assistant, terminate: None }, "main").await.unwrap();
        session
            .append_record(NewRecord::Usage {
                id: "assistant-usage".into(), lane: "main".into(), cause: "assistant".into(),
                run_id: Some("run".into()), entry_id: Some("assistant".into()), attempt: Some(1),
                stop_reason: Some("stop".into()), tool_call_id: None, details: None, usage: assistant_usage.clone(),
            })
            .await
            .unwrap();
        session
            .append_record(NewRecord::Usage {
                id: "deferred-usage".into(), lane: "main".into(), cause: "deferred_fetch".into(),
                run_id: Some("run".into()), entry_id: Some("deferred-result".into()), attempt: Some(1),
                stop_reason: Some("deferred".into()), tool_call_id: None, details: None, usage: zero_usage(),
            })
            .await
            .unwrap();
        // NOTE: upstream also appends a *negative* adjustment usage record
        // (input -2, totalTokens -2, cost -0.5). Token counts are u64 in the
        // pi-ai port, so negative adjustments are unrepresentable — the
        // expected stats below drop that record (see ledger note).
        session.create_lane("thread", Some("assistant")).await.unwrap();
        session.set_name(Some("First")).await.unwrap();
        session.set_name(Some("Second")).await.unwrap();
        session.set_label("user", Some("keep")).await.unwrap();
        session.set_label("user", None).await.unwrap();
        assert_rejects(session.set_label("missing", Some("checkpoint")).await, SessionErrorKind::NotFound);

        assert_eq!(session.get_name().await, Some("Second".to_string()));
        assert_eq!(session.get_label("user").await, None);
        let usage_records = session.find_records(&RecordQuery { record_type: Some("usage".into()), order: Some(EntryOrder::OldestFirst), ..Default::default() }).await.unwrap();
        assert_eq!(usage_records.len(), 2);
        assert_eq!(usage_records[0].record_type(), "usage");
        let deferred = &usage_records[1];
        let LaneRecord::Usage { stop_reason, .. } = deferred else { panic!("expected usage record") };
        assert_eq!(stop_reason.as_deref(), Some("deferred"));
        assert_eq!(
            session.get_stats().await,
            pi_agent::session::types::SessionStats {
                message_count: 2,
                cached_tokens: 3,
                uncached_tokens: 12,
                total_tokens: 20,
                cost_total: 10.0,
            }
        );
    }
}
conformance_case!(keeps_latest_value_facts_and_computes_ledger_statistics_across_lanes);

mod clears_session_names_durably {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut session = repo.create(RepoCreateOptions { id: Some("session".into()), ..Default::default() }).await.unwrap();
        session.set_name(Some("Temporary")).await.unwrap();
        session.set_name(None).await.unwrap();

        assert_eq!(session.get_name().await, None);

        let metadata = session.get_metadata().await.unwrap();
        let reopened = repo.open(&metadata).await.unwrap();
        assert_eq!(reopened.get_name().await, None);
        let mut fork = repo.fork(&metadata, RepoForkOptions { id: Some("fork".into()), ..Default::default() }).await.unwrap();
        assert_eq!(fork.get_name().await, None);
        let _ = &mut fork;
    }
}
conformance_case!(clears_session_names_durably);

// ---------------------------------------------------------------------------
// Cases — records and log
// ---------------------------------------------------------------------------

mod keeps_lane_names_permanent_with_their_recovery_records {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut session = repo.create(RepoCreateOptions { id: Some("session".into()), ..Default::default() }).await.unwrap();
        session.create_lane("thread", None).await.unwrap();
        session.append_record(operation_started("old-run", "thread", "run")).await.unwrap();
        session
            .append_record(NewRecord::QueueEnqueued {
                id: "old-next-run".into(), lane: "thread".into(), queue: "nextRun".into(),
                run_id: "old-run".into(),
                target: serde_json::json!({"type": "message", "id": "queued-message", "message": {"role": "user", "content": [{"type": "text", "text": "queued"}], "timestamp": 1}}),
            })
            .await
            .unwrap();

        let lane_records = session.find_records(&RecordQuery { lane: Some("thread".into()), ..Default::default() }).await.unwrap();
        assert_eq!(lane_records.iter().map(|r| r.id().to_string()).collect::<Vec<_>>(), vec!["old-next-run", "old-run"]);
        let log_record_ids: Vec<String> = session
            .get_log(&LogOptions::default())
            .await
            .unwrap()
            .iter()
            .filter_map(|item| match item {
                pi_agent::session::types::LogItem::Record(r) => Some(r.id().to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(log_record_ids, vec!["old-run", "old-next-run"]);
        assert_rejects(session.create_lane("thread", None).await, SessionErrorKind::AlreadyExists);
    }
}
conformance_case!(keeps_lane_names_permanent_with_their_recovery_records);

mod persists_queue_cancellation_without_consuming_its_target {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut session = repo.create(RepoCreateOptions { id: Some("session".into()), ..Default::default() }).await.unwrap();
        let enqueued = session
            .append_record(NewRecord::QueueEnqueued {
                id: "enqueue".into(), lane: "main".into(), queue: "nextRun".into(),
                run_id: "run".into(),
                target: serde_json::json!({"type": "message", "id": "queued-message", "message": {"role": "user", "content": [{"type": "text", "text": "queued"}], "timestamp": 1}}),
            })
            .await
            .unwrap();
        let cancelled = session
            .append_record(NewRecord::QueueCancelled { id: "cancel".into(), lane: "main".into(), run_id: None, entry_id: "queued-message".into() })
            .await
            .unwrap();
        let LaneRecord::QueueCancelled { entry_id, .. } = &cancelled else { panic!("expected queue_cancelled") };
        assert_eq!((cancelled.seq(), entry_id.as_str()), (2, "queued-message"));
        assert!(session.get_entry("queued-message").await.is_none());
        let cancellations = session.find_records(&RecordQuery { record_type: Some("queue_cancelled".into()), ..Default::default() }).await.unwrap();
        let LaneRecord::QueueCancelled { entry_id, .. } = &cancellations[0] else { panic!("expected queue_cancelled") };
        assert_eq!(entry_id, "queued-message");
        assert_eq!(cancellations, vec![cancelled.clone()]);
        let log = session.get_log(&LogOptions::default()).await.unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].seq(), enqueued.seq());
        assert_eq!(log[1].seq(), cancelled.seq());
    }
}
conformance_case!(persists_queue_cancellation_without_consuming_its_target);

mod filters_records_by_lane_type_run_sequence_and_order {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut session = repo.create(RepoCreateOptions { id: Some("session".into()), ..Default::default() }).await.unwrap();
        session.append_record(operation_started("run-1", "main", "run")).await.unwrap();
        session
            .append_record(NewRecord::StepAttempt {
                id: "attempt-1".into(), lane: "main".into(), run_id: "run-1".into(),
                step: "assistant".into(), attempt: 1, result_entry_id: "assistant-1".into(), compaction_reason: None,
            })
            .await
            .unwrap();
        session.create_lane("thread", None).await.unwrap();
        session.append_record(operation_started("run-2", "thread", "run")).await.unwrap();
        session
            .append_record(NewRecord::StepAttempt {
                id: "attempt-2".into(), lane: "thread".into(), run_id: "run-2".into(),
                step: "assistant".into(), attempt: 1, result_entry_id: "assistant-2".into(), compaction_reason: None,
            })
            .await
            .unwrap();

        let thread_records = session.find_records(&RecordQuery { lane: Some("thread".into()), ..Default::default() }).await.unwrap();
        assert_eq!(thread_records.iter().map(|r| r.id().to_string()).collect::<Vec<_>>(), vec!["attempt-2", "run-2"]);
        let step_records = session.find_records(&RecordQuery { record_type: Some("step_attempt".into()), order: Some(EntryOrder::OldestFirst), ..Default::default() }).await.unwrap();
        assert_eq!(step_records.iter().map(|r| r.id().to_string()).collect::<Vec<_>>(), vec!["attempt-1", "attempt-2"]);
        let run_records = session.find_records(&RecordQuery { run_id: Some("run-1".into()), after_seq: Some(1), ..Default::default() }).await.unwrap();
        assert_eq!(run_records.iter().map(|r| r.id().to_string()).collect::<Vec<_>>(), vec!["attempt-1"]);
        let limited = session.find_records(&RecordQuery { limit: Some(1), ..Default::default() }).await.unwrap();
        assert_eq!(limited.iter().map(|r| r.id().to_string()).collect::<Vec<_>>(), vec!["attempt-2"]);
    }
}
conformance_case!(filters_records_by_lane_type_run_sequence_and_order);

mod filters_operation_starts_by_operation_kind {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut session = repo.create(RepoCreateOptions { id: Some("session".into()), ..Default::default() }).await.unwrap();
        session.append_record(operation_started("run-old", "main", "run")).await.unwrap();
        session
            .append_record(NewRecord::OperationFinished { id: "run-old-finished".into(), lane: "main".into(), run_id: "run-old".into(), outcome: "completed".into(), error: None })
            .await
            .unwrap();
        session.append_record(operation_started("compaction", "main", "compaction")).await.unwrap();
        session
            .append_record(NewRecord::OperationFinished { id: "compaction-finished".into(), lane: "main".into(), run_id: "compaction".into(), outcome: "completed".into(), error: None })
            .await
            .unwrap();
        session.append_record(operation_started("navigation", "main", "navigation")).await.unwrap();
        session
            .append_record(NewRecord::OperationFinished { id: "navigation-finished".into(), lane: "main".into(), run_id: "navigation".into(), outcome: "completed".into(), error: None })
            .await
            .unwrap();
        session.append_record(operation_started("run-new", "main", "run")).await.unwrap();

        let runs = session
            .find_records(&RecordQuery { record_type: Some("operation_started".into()), operation_kind: Some("run".into()), order: Some(EntryOrder::OldestFirst), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(runs.iter().map(|r| r.id().to_string()).collect::<Vec<_>>(), vec!["run-old", "run-new"]);
        let compactions = session
            .find_records(&RecordQuery { record_type: Some("operation_started".into()), operation_kind: Some("compaction".into()), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(compactions.iter().map(|r| r.id().to_string()).collect::<Vec<_>>(), vec!["compaction"]);
        let navigations = session
            .find_records(&RecordQuery { record_type: Some("operation_started".into()), operation_kind: Some("navigation".into()), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(navigations.iter().map(|r| r.id().to_string()).collect::<Vec<_>>(), vec!["navigation"]);
        let run_limited = session
            .find_records(&RecordQuery { record_type: Some("operation_started".into()), operation_kind: Some("run".into()), limit: Some(1), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(run_limited.iter().map(|r| r.id().to_string()).collect::<Vec<_>>(), vec!["run-new"]);
    }
}
conformance_case!(filters_operation_starts_by_operation_kind);

mod tracks_and_enforces_one_open_operation_per_lane {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut session = repo.create(RepoCreateOptions { id: Some("session".into()), ..Default::default() }).await.unwrap();
        let open = session.find_open_operations("main", Some(2)).await.unwrap();
        assert!(open.is_empty());

        let first = session.append_record(operation_started("first", "main", "run")).await.unwrap();
        let open = session.find_open_operations("main", Some(2)).await.unwrap();
        assert_eq!(open, vec![first.clone()]);
        assert_rejects(
            session.append_record(operation_started("second", "main", "run")).await.map(|_| ()),
            SessionErrorKind::Storage,
        );
        let open = session.find_open_operations("main", Some(2)).await.unwrap();
        assert_eq!(open, vec![first.clone()]);

        session
            .append_record(NewRecord::OperationFinished { id: "finish-first".into(), lane: "main".into(), run_id: first.id().into(), outcome: "completed".into(), error: None })
            .await
            .unwrap();
        let open = session.find_open_operations("main", Some(2)).await.unwrap();
        assert!(open.is_empty());
    }
}
conformance_case!(tracks_and_enforces_one_open_operation_per_lane);

mod does_not_let_an_earlier_finish_close_a_later_start {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut session = repo.create(RepoCreateOptions { id: Some("session".into()), ..Default::default() }).await.unwrap();
        session
            .append_record(NewRecord::OperationFinished { id: "finish-before-start".into(), lane: "main".into(), run_id: "run".into(), outcome: "completed".into(), error: None })
            .await
            .unwrap();
        let started = session.append_record(operation_started("run", "main", "run")).await.unwrap();
        let open = session.find_open_operations("main", Some(2)).await.unwrap();
        assert_eq!(open, vec![started.clone()]);
    }
}
conformance_case!(does_not_let_an_earlier_finish_close_a_later_start);

mod scopes_open_operations_by_lane_and_limit {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut session = repo.create(RepoCreateOptions { id: Some("session".into()), ..Default::default() }).await.unwrap();
        session.create_lane("thread", None).await.unwrap();
        let main_run = session.append_record(operation_started("main-run", "main", "run")).await.unwrap();
        let thread_navigation = session.append_record(operation_started("thread-navigation", "thread", "navigation")).await.unwrap();

        let open = session.find_open_operations("main", None).await.unwrap();
        assert_eq!(open, vec![main_run.clone()]);
        let open = session.find_open_operations("main", Some(1)).await.unwrap();
        assert_eq!(open, vec![main_run.clone()]);
        let open = session.find_open_operations("thread", Some(2)).await.unwrap();
        assert_eq!(open, vec![thread_navigation.clone()]);
    }
}
conformance_case!(scopes_open_operations_by_lane_and_limit);

// ---------------------------------------------------------------------------
// Cases — validation and immutability
// ---------------------------------------------------------------------------

mod returns_immutable_open_operation_records {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut session = repo.create(RepoCreateOptions { id: Some("session".into()), ..Default::default() }).await.unwrap();
        let committed = session.append_record(operation_started("run", "main", "run")).await.unwrap();
        let open = session.find_open_operations("main", None).await.unwrap();
        let Some(first) = open.first() else { panic!("expected an open run operation") };
        let LaneRecord::OperationStarted { intent, .. } = first else { panic!("expected operation_started") };
        // Reads are clones: mutating the local copy must not affect storage.
        let mut intent_mut = intent.clone();
        if let pi_agent::session::types::OperationIntent::Run { original_prompt, .. } = &mut intent_mut {
            original_prompt.push(user_message("mutated"));
        }
        drop(intent_mut);
        let after = session.find_open_operations("main", None).await.unwrap();
        assert_eq!(after, vec![committed.clone()]);
        let _ = &committed;
    }
}
conformance_case!(returns_immutable_open_operation_records);

mod returns_immutable_copies_from_reads {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut session = repo.create(RepoCreateOptions { id: Some("immutable".into()), ..Default::default() }).await.unwrap();
        let metadata = session.get_metadata().await.unwrap();
        let mut data = serde_json::json!({ "nested": { "value": 1 } });
        let entry_id = session.append_custom_entry("note", Some(data.clone())).await.unwrap();
        // Mutating our local copy must not affect the stored entry.
        data["nested"]["value"] = serde_json::json!(50);

        let read = session.get_entry(&entry_id).await.unwrap();
        assert_eq!(read.custom_type_of(), Some("note"));
        // Clone-of-read write attempt: no observable effect on storage.
        let mut read_clone = read.clone();
        if let Entry::Custom { data: d, .. } = &mut read_clone {
            if let Some(Some(obj)) = d.as_mut().map(|v| v.as_object_mut()) {
                if let Some(nested) = obj.get_mut("nested").and_then(|v| v.as_object_mut()) {
                    nested.insert("value".into(), serde_json::json!(99));
                }
            }
        }
        drop(read_clone);

        let mut meta_clone = session.get_metadata().await.unwrap();
        meta_clone.id = "changed".into();
        drop(meta_clone);

        let after = session.get_metadata().await.unwrap();
        assert_eq!(after, metadata);
        let read_after = session.get_entry(&entry_id).await.unwrap();
        let Entry::Custom { data: d, .. } = &read_after else { panic!("expected custom") };
        assert_eq!(d.as_ref().and_then(|v| v.get("nested").and_then(|n| n.get("value"))), Some(&serde_json::json!(1)));
    }
}
conformance_case!(returns_immutable_copies_from_reads);

mod rejects_non_json_entries_before_storage_mutation {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        // Upstream exercises JS-only non-JSON values (undefined, BigInt, NaN,
        // Map, cycles). The Rust port's types are JSON by construction, so the
        // invalid path is unrepresentable; the contract retained here is that
        // a valid custom entry appends and advances the shared sequence.
        let mut session = repo.create(RepoCreateOptions { id: Some("session".into()), ..Default::default() }).await.unwrap();
        assert_eq!(session.get_leaf_id().await.unwrap(), None);
        assert!(session.find_entries(&EntryQuery::default()).await.unwrap().is_empty());
        assert!(session.get_log(&LogOptions::default()).await.unwrap().is_empty());
        let valid_id = session.append_custom_entry("valid", Some(serde_json::json!({"value": 1}))).await.unwrap();
        assert_eq!(session.get_entry(&valid_id).await.unwrap().seq(), 1);
    }
}
conformance_case!(rejects_non_json_entries_before_storage_mutation);

mod rejects_non_json_records_before_storage_mutation {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        // JS-only invalid payloads are unrepresentable in Rust; the retained
        // contract: a valid record appends and advances the sequence.
        let mut session = repo.create(RepoCreateOptions { id: Some("session".into()), ..Default::default() }).await.unwrap();
        assert!(session.find_records(&RecordQuery::default()).await.unwrap().is_empty());
        assert!(session.get_log(&LogOptions::default()).await.unwrap().is_empty());
        let started = session.append_record(operation_started("valid-record", "main", "run")).await.unwrap();
        assert_eq!(started.seq(), 1);
    }
}
conformance_case!(rejects_non_json_records_before_storage_mutation);

// ---------------------------------------------------------------------------
// Cases — repository and forks
// ---------------------------------------------------------------------------

mod creates_lists_and_opens_sessions {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut session = repo.create(RepoCreateOptions { id: Some("one".into()), ..Default::default() }).await.unwrap();
        let entry_id = session.append_message(user_message("persisted")).await.unwrap();
        let metadata = session.get_metadata().await.unwrap();

        let listed = repo.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, metadata.id);
        assert_eq!(listed[0].created_at, metadata.created_at);
        assert_eq!(listed[0].parent_session_id, metadata.parent_session_id);

        let opened = repo.open(&metadata).await.unwrap();
        let ids = entry_ids(opened.find_entries(&EntryQuery::default()).await.unwrap()).await;
        assert_eq!(ids, vec![entry_id]);

        assert_rejects(
            repo.create(RepoCreateOptions { id: Some("one".into()), ..Default::default() }).await.map(|_| ()),
            SessionErrorKind::AlreadyExists,
        );
    }
}
conformance_case!(creates_lists_and_opens_sessions);

mod deletes_sessions_idempotently {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let session = repo.create(RepoCreateOptions { id: Some("one".into()), ..Default::default() }).await.unwrap();
        let metadata = session.get_metadata().await.unwrap();
        drop(session);

        repo.delete(&metadata).await.unwrap();
        assert_rejects(repo.open(&metadata).await.map(|_| ()), SessionErrorKind::NotFound);
        repo.delete(&metadata).await.unwrap();
    }
}
conformance_case!(deletes_sessions_idempotently);

mod forks_one_branch_with_selected_facts_and_no_records {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut source = repo.create(RepoCreateOptions { id: Some("source".into()), ..Default::default() }).await.unwrap();
        let root = source.append_message(user_message("root")).await.unwrap();
        let shared = source.append_message(assistant_message("shared")).await.unwrap();
        source.create_lane("thread", Some(&shared)).await.unwrap();
        let thread_child = {
            let mut view = source.view("thread");
            view.append_message(user_message("thread")).await.unwrap()
        };
        let main_child = source.append_message(user_message("main")).await.unwrap();
        source.set_name(Some("Source")).await.unwrap();
        source.set_label(&shared, Some("copied")).await.unwrap();
        source.set_label(&thread_child, Some("excluded")).await.unwrap();
        source.append_record(operation_started("run", "main", "run")).await.unwrap();
        source
            .append_record(NewRecord::Usage {
                id: "source-usage".into(), lane: "main".into(), cause: "adjustment".into(),
                run_id: None, entry_id: None, attempt: None, stop_reason: None, tool_call_id: None,
                details: None,
                usage: usage_explicit(10, 5, 3, 2, 20, Cost { input: 1.0, output: 2.0, cache_read: 3.0, cache_write: 4.0, total: 10.0 }),
            })
            .await
            .unwrap();
        let source_meta = source.get_metadata().await.unwrap();
        drop(source);

        let mut fork = repo
            .fork(
                &source_meta,
                RepoForkOptions {
                    id: Some("branch-fork".into()),
                    fork_options: ForkOptions::Branch {
                        entry_id: Some(main_child.clone()),
                        position: Some(ForkPosition::At),
                    },
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let fork_ids = entry_ids(fork.find_entries(&EntryQuery { order: Some(EntryOrder::OldestFirst), ..Default::default() }).await.unwrap()).await;
        assert_eq!(fork_ids, vec![root.clone(), shared.clone(), main_child]);
        assert_eq!(
            fork.get_lanes().await.unwrap(),
            vec![pi_agent::session::types::LanePointer { lane: "main".into(), leaf_id: Some(fork_ids[2].clone()) }]
        );
        assert_eq!(fork.get_name().await, Some("Source".to_string()));
        assert_eq!(fork.get_label(&shared).await, Some("copied".to_string()));
        assert_eq!(fork.get_label(&thread_child).await, None);
        assert!(fork.find_records(&RecordQuery::default()).await.unwrap().is_empty());
        assert_eq!(
            fork.get_stats().await,
            pi_agent::session::types::SessionStats { message_count: 3, cached_tokens: 0, uncached_tokens: 0, total_tokens: 0, cost_total: 0.0 }
        );
        fork.append_message(user_message("after fork")).await.unwrap();
        assert_eq!(fork.get_stats().await.message_count, 4);
        let fork_meta = fork.get_metadata().await.unwrap();
        assert_eq!((fork_meta.id.as_str(), fork_meta.parent_session_id.as_deref()), ("branch-fork", Some("source")));
    }
}
conformance_case!(forks_one_branch_with_selected_facts_and_no_records);

mod forks_a_complete_tree_with_lanes_and_facts {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut source = repo.create(RepoCreateOptions { id: Some("source".into()), ..Default::default() }).await.unwrap();
        let root = source.append_message(user_message("root")).await.unwrap();
        source.create_lane("thread", Some(&root)).await.unwrap();
        let main_child = source.append_message(user_message("main")).await.unwrap();
        let thread_child = {
            let mut view = source.view("thread");
            view.append_message(user_message("thread")).await.unwrap()
        };
        source.set_label(&thread_child, Some("thread-tip")).await.unwrap();
        let source_meta = source.get_metadata().await.unwrap();
        drop(source);

        let fork = repo
            .fork(&source_meta, RepoForkOptions { id: Some("tree-fork".into()), fork_options: ForkOptions::Tree, ..Default::default() })
            .await
            .unwrap();

        let fork_ids = entry_ids(fork.find_entries(&EntryQuery { order: Some(EntryOrder::OldestFirst), ..Default::default() }).await.unwrap()).await;
        assert_eq!(fork_ids, vec![root.clone(), main_child.clone(), thread_child.clone()]);
        assert_eq!(
            fork.get_lanes().await.unwrap(),
            vec![
                pi_agent::session::types::LanePointer { lane: "main".into(), leaf_id: Some(main_child.clone()) },
                pi_agent::session::types::LanePointer { lane: "thread".into(), leaf_id: Some(thread_child.clone()) },
            ]
        );
        assert_eq!(fork.get_label(&thread_child).await, Some("thread-tip".to_string()));
        assert_eq!(fork.get_stats().await.message_count, 3);
        let lane_items: Vec<(u64, String, Option<String>)> = fork
            .get_log(&LogOptions::default())
            .await
            .unwrap()
            .iter()
            .filter_map(|item| match item {
                pi_agent::session::types::LogItem::Lane { seq, lane, leaf_id } => Some((*seq, lane.clone(), leaf_id.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            lane_items,
            vec![
                (4, "main".to_string(), Some(main_child.clone())),
                (5, "thread".to_string(), Some(thread_child)),
            ]
        );
    }
}
conformance_case!(forks_a_complete_tree_with_lanes_and_facts);

mod forks_before_an_entry_without_modifying_the_source {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut source = repo.create(RepoCreateOptions { id: Some("source".into()), ..Default::default() }).await.unwrap();
        let root = source.append_message(user_message("root")).await.unwrap();
        let tail = source.append_message(user_message("tail")).await.unwrap();
        let source_meta = source.get_metadata().await.unwrap();

        let fork = repo
            .fork(&source_meta, RepoForkOptions { id: Some("fork".into()), fork_options: ForkOptions::Branch { entry_id: Some(tail.clone()), position: None }, ..Default::default() })
            .await
            .unwrap();
        let fork_ids = entry_ids(fork.find_entries(&EntryQuery { order: Some(EntryOrder::OldestFirst), ..Default::default() }).await.unwrap()).await;
        assert_eq!(fork_ids, vec![root.clone()]);
        assert_eq!(fork.get_leaf_id().await.unwrap(), Some(root.clone()));
        assert_eq!(source.get_leaf_id().await.unwrap(), Some(tail.clone()));

        let before_default = repo
            .fork(&source_meta, RepoForkOptions { id: Some("before-default-target".into()), fork_options: ForkOptions::Branch { entry_id: None, position: Some(ForkPosition::Before) }, ..Default::default() })
            .await
            .unwrap();
        let before_ids = entry_ids(before_default.find_entries(&EntryQuery { order: Some(EntryOrder::OldestFirst), ..Default::default() }).await.unwrap()).await;
        assert_eq!(before_ids, vec![root.clone()]);
        assert_eq!(before_default.get_leaf_id().await.unwrap(), Some(root.clone()));

        let at_default = repo
            .fork(&source_meta, RepoForkOptions { id: Some("at-default-target".into()), fork_options: ForkOptions::Branch { entry_id: None, position: Some(ForkPosition::At) }, ..Default::default() })
            .await
            .unwrap();
        let at_ids = entry_ids(at_default.find_entries(&EntryQuery { order: Some(EntryOrder::OldestFirst), ..Default::default() }).await.unwrap()).await;
        assert_eq!(at_ids, vec![root.clone(), tail.clone()]);
        assert_eq!(at_default.get_leaf_id().await.unwrap(), Some(tail.clone()));

        assert_rejects(
            repo.fork(&source_meta, RepoForkOptions { id: Some("missing".into()), fork_options: ForkOptions::Branch { entry_id: Some("missing".into()), position: None }, ..Default::default() })
                .await
                .map(|_| ()),
            SessionErrorKind::InvalidForkTarget,
        );
    }
}
conformance_case!(forks_before_an_entry_without_modifying_the_source);

mod validates_the_default_fork_target {
    use super::*;
    pub async fn __body(repo: &mut dyn ConformanceRepo) {
        let mut source = repo.create(RepoCreateOptions { id: Some("source-with-custom-leaf".into()), ..Default::default() }).await.unwrap();
        source.append_custom_entry("not-a-message", None).await.unwrap();
        let source_meta = source.get_metadata().await.unwrap();
        drop(source);

        assert_rejects(
            repo.fork(&source_meta, RepoForkOptions { id: Some("fork".into()), ..Default::default() }).await.map(|_| ()),
            SessionErrorKind::InvalidForkTarget,
        );
    }
}
conformance_case!(validates_the_default_fork_target);
