//! Session search — port of `packages/agent/src/search/` (scanning.ts +
//! index.ts): substring, case-insensitive search across sessions.
//!
//! The upstream `createScanningSessionSearch` supports two source forms:
//! an array of readables and a lazy source function (async iterable). This
//! port implements the array form; the lazy JSONL source function is
//! deferred until async-iteration infrastructure lands (the disk case is
//! covered directly through `JsonlSessionRepo` in tests). Abort is exposed
//! as a synchronous `abort_requested` flag — there is no mid-stream
//! cancellation point in a collected result.

use std::collections::HashSet;

use crate::fs::FileSystem;
use crate::session::state::{EntryCursor, EntryOrder, EntryQuery};
use crate::session::{Entry, Session};

/// A search hit: owning session, matching entry, timestamp, and the full
/// projected candidate text used for matching (upstream ScanningSessionSearchHit).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSearchHit {
    pub session_id: String,
    pub entry_id: String,
    pub timestamp: u64,
    pub snippet: String,
}

#[derive(Debug, Clone, Default)]
pub struct SessionSearchOptions {
    /// Restrict results to canonical entry types (e.g. "message").
    pub entry_types: Option<Vec<String>>,
    /// Maximum number of hits to return.
    pub limit: Option<usize>,
    /// When true the search is aborted (upstream `AbortSignal`).
    pub abort_requested: bool,
}

/// Page size for the entry scan (upstream `pageSize ?? 100`).
const DEFAULT_PAGE_SIZE: usize = 100;

/// Canonical entry type name ("message", "custom", ...).
fn entry_type_name(entry: &Entry) -> &'static str {
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

/// Default text projector: JSON.stringify(entry) plus the label when present.
fn default_search_text(entry: &Entry, label: Option<&str>) -> String {
    let json = serde_json::to_string(entry).unwrap_or_default();
    match label {
        Some(label) => format!("{json} {label}"),
        None => json,
    }
}

/// Default matcher: case-insensitive substring on the projected text.
fn default_match(query_text: &str, candidate_text: &str) -> bool {
    candidate_text.to_lowercase().contains(query_text)
}

/// Search across an owned set of sessions (the array-source form of
/// `createScanningSessionSearch`).
pub struct ScanningSessionSearch<F: FileSystem> {
    sessions: Vec<Session<F>>,
}

impl<F: FileSystem> ScanningSessionSearch<F> {
    pub fn new(sessions: Vec<Session<F>>) -> Self {
        Self { sessions }
    }

    pub async fn search(
        &self,
        text: &str,
        options: &SessionSearchOptions,
    ) -> Result<Vec<SessionSearchHit>, String> {
        let normalized_text = text.trim().to_lowercase();
        if normalized_text.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(limit) = options.limit {
            if limit == 0 {
                return Ok(Vec::new());
            }
        }
        if let Some(entry_types) = &options.entry_types {
            if entry_types.is_empty() {
                return Ok(Vec::new());
            }
        }

        let mut hit_count = 0usize;
        let mut seen_session_ids: HashSet<String> = HashSet::new();
        let mut hits: Vec<SessionSearchHit> = Vec::new();

        for session in &self.sessions {
            if options.abort_requested {
                return Err("The operation was aborted".to_string());
            }
            let metadata = session.get_metadata().await;
            if !seen_session_ids.insert(metadata.id.clone()) {
                return Err(format!("Duplicate sessionId: {}", metadata.id));
            }

            let mut after_seq = 0u64;
            loop {
                if options.abort_requested {
                    return Err("The operation was aborted".to_string());
                }
                let entry_type = match &options.entry_types {
                    Some(types) if types.len() == 1 => Some(types[0].clone()),
                    _ => None,
                };
                let query = EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    entry_type,
                    limit: Some(DEFAULT_PAGE_SIZE),
                    cursor: Some(EntryCursor { after_seq }),
                    ..Default::default()
                };
                let entries = session
                    .find_entries(&query)
                    .await
                    .map_err(|e| format!("find entries: {e}"))?;
                if entries.is_empty() {
                    break;
                }
                for entry in &entries {
                    if let Some(types) = &options.entry_types {
                        if !types.iter().any(|t| t == entry_type_name(entry)) {
                            continue;
                        }
                    }
                    let label = session.get_label(entry.id()).await;
                    let candidate_text = default_search_text(entry, label.as_deref());
                    if !default_match(&normalized_text, &candidate_text) {
                        continue;
                    }
                    hits.push(SessionSearchHit {
                        session_id: metadata.id.clone(),
                        entry_id: entry.id().to_string(),
                        timestamp: entry.timestamp(),
                        snippet: candidate_text,
                    });
                    hit_count += 1;
                    if let Some(limit) = options.limit {
                        if hit_count >= limit {
                            return Ok(hits);
                        }
                    }
                }
                after_seq = entries.iter().last().map(|e| e.seq()).unwrap_or(after_seq);
                if entries.len() < DEFAULT_PAGE_SIZE {
                    break;
                }
            }
        }
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::MemoryFs;
    use crate::session::jsonl::storage::JsonlSessionStorage;
    use crate::session::types::{EntryNoStats, JsonlV4Header};
    use crate::session::{jsonl_session_directory_name, CreateOptions, JsonlSessionRepo};
    use pi_ai::types::{ContentBlock, Message, UserContent};

    fn user_message(text: &str) -> crate::types::AgentMessage {
        crate::types::AgentMessage::Core(Message::User(UserContent::blocks(
            vec![ContentBlock::text(text)],
            1,
        )))
    }

    fn header(id: &str, cwd: &str) -> JsonlV4Header {
        JsonlV4Header {
            kind: "header".into(),
            version: 4,
            id: id.into(),
            created_at: 1_700_000_000_000,
            cwd: cwd.into(),
            parent_session_id: None,
            legacy_parent_session_path: None,
            metadata: None,
        }
    }

    /// Memory-backed session with the given id/cwd.
    async fn memory_session(id: &str, cwd: &str) -> Session<MemoryFs> {
        let fs = MemoryFs::new();
        let path = format!("/sessions/{id}.jsonl");
        let storage = JsonlSessionStorage::create(fs, &path, header(id, cwd))
            .await
            .unwrap();
        Session::new(storage)
    }

    fn notes_entry(text: &str) -> EntryNoStats {
        let msg = user_message(text);
        EntryNoStats::Message {
            id: format!("note-{}", text.len()),
            message: msg,
            terminate: None,
        }
    }

    fn custom_entry(custom_type: &str, data: serde_json::Value) -> EntryNoStats {
        EntryNoStats::Custom {
            id: format!("custom-{custom_type}"),
            custom_type: custom_type.to_string(),
            data: Some(data),
        }
    }

    async fn hit_search(
        sessions: Vec<Session<MemoryFs>>,
        text: &str,
        options: &SessionSearchOptions,
    ) -> Result<Vec<SessionSearchHit>, String> {
        ScanningSessionSearch::new(sessions)
            .search(text, options)
            .await
    }

    fn default_options() -> SessionSearchOptions {
        SessionSearchOptions::default()
    }

    // ---- oracle-ported tests (packages/agent/test/harness/session/search.test.ts) ----

    #[tokio::test]
    async fn scans_in_memory_projected_source() {
        let mut root = memory_session("root", "/repo").await;
        root.append_entry(notes_entry("fix auth flow"), "main")
            .await
            .unwrap();
        let mut other = memory_session("other", "/other").await;
        other
            .append_entry(notes_entry("auth in another workspace"), "main")
            .await
            .unwrap();

        let root_hits = hit_search(vec![root], "auth", &default_options())
            .await
            .unwrap();
        assert_eq!(root_hits.len(), 1);
        assert_eq!(root_hits[0].session_id, "root");

        let other_hits = hit_search(vec![other], "auth", &default_options())
            .await
            .unwrap();
        assert_eq!(other_hits[0].session_id, "other");

        let root = memory_session("root", "/repo").await;
        assert!(hit_search(vec![root], "missing", &default_options())
            .await
            .unwrap()
            .is_empty());
        assert!(hit_search(vec![], "auth", &default_options())
            .await
            .unwrap()
            .is_empty());
        // Trims and is case-insensitive.
        let mut root = memory_session("root", "/repo").await;
        root.append_entry(notes_entry("Fix Auth Flow"), "main")
            .await
            .unwrap();
        let hits = hit_search(vec![root], "  auth  ", &default_options())
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[tokio::test]
    async fn includes_labels_in_projection() {
        let mut session = memory_session("session", "/repo").await;
        let entry = session
            .append_entry(notes_entry("plain body"), "main")
            .await
            .unwrap();
        session
            .set_label(entry.id(), Some("important label"))
            .await
            .unwrap();

        let hits = hit_search(vec![session], "important", &default_options())
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "session");
        assert_eq!(hits[0].entry_id, entry.id());
        assert!(
            hits[0].snippet.contains("important label"),
            "got: {:?}",
            hits[0].snippet
        );
    }

    #[tokio::test]
    async fn honors_entry_type_filters_and_abort() {
        let mut session = memory_session("session", "/repo").await;
        session
            .append_entry(notes_entry("auth message"), "main")
            .await
            .unwrap();
        session
            .append_entry(
                custom_entry("note", serde_json::json!({ "text": "auth custom" })),
                "main",
            )
            .await
            .unwrap();

        let options = SessionSearchOptions {
            entry_types: Some(vec!["message".to_string()]),
            ..Default::default()
        };
        let hits = hit_search(vec![session], "auth", &options).await.unwrap();
        assert_eq!(hits.len(), 1, "got: {hits:?}");
        assert!(
            hits[0].snippet.contains("auth message"),
            "got: {:?}",
            hits[0].snippet
        );

        // Abort requested -> error surfaced immediately.
        let mut session = memory_session("session", "/repo").await;
        session
            .append_entry(notes_entry("auth message"), "main")
            .await
            .unwrap();
        let abort = SessionSearchOptions {
            abort_requested: true,
            ..Default::default()
        };
        let err = hit_search(vec![session], "auth", &abort).await.unwrap_err();
        assert!(err.to_lowercase().contains("abort"), "got: {err}");
    }

    #[tokio::test]
    async fn duplicate_session_ids_are_rejected() {
        // Two distinct storage backends sharing one session id -> the
        // duplicate guard fires while scanning the second readable.
        let mut a = memory_session("dup", "/a").await;
        a.append_entry(notes_entry("auth x"), "main").await.unwrap();
        let mut b = memory_session("dup", "/b").await;
        b.append_entry(notes_entry("auth y"), "main").await.unwrap();

        let err = hit_search(vec![a, b], "auth", &default_options())
            .await
            .unwrap_err();
        assert!(err.contains("Duplicate sessionId: dup"), "got: {err}");
    }

    #[tokio::test]
    async fn scans_jsonl_sessions_from_disk() {
        let fs = MemoryFs::new();
        let _root = format!("/{}/", jsonl_session_directory_name("/work"));
        let repo = JsonlSessionRepo::new(fs, "/sessions".to_string());
        let mut repo = repo;

        let mut session = repo
            .create(CreateOptions::new("/work").with_id("jsonl"))
            .await
            .unwrap();
        let entry = session
            .append_entry(notes_entry("jsonl backed auth entry"), "main")
            .await
            .unwrap();
        session
            .set_label(entry.id(), Some("disk label"))
            .await
            .unwrap();
        drop(session);

        let mut other = repo
            .create(CreateOptions::new("/other").with_id("other"))
            .await
            .unwrap();
        other
            .append_entry(
                notes_entry("jsonl backed auth entry in another cwd"),
                "main",
            )
            .await
            .unwrap();
        drop(other);

        let metadata_list = repo.list(None).await.unwrap();
        assert_eq!(metadata_list.len(), 2, "repo should discover both sessions");
        let mut sessions = Vec::new();
        for metadata in &metadata_list {
            sessions.push(repo.open(metadata).await.unwrap());
        }

        let hits = hit_search(sessions, "auth", &default_options())
            .await
            .unwrap();
        assert_eq!(hits.len(), 2, "got: {hits:?}");
        let ids: Vec<&str> = hits.iter().map(|h| h.session_id.as_str()).collect();
        assert!(ids.contains(&"jsonl"), "got: {ids:?}");
        assert!(ids.contains(&"other"), "got: {ids:?}");

        let sessions = {
            let metadata_list = repo.list(None).await.unwrap();
            let mut v = Vec::new();
            for metadata in &metadata_list {
                v.push(repo.open(metadata).await.unwrap());
            }
            v
        };
        let disk_hits = hit_search(sessions, "disk", &default_options())
            .await
            .unwrap();
        assert_eq!(disk_hits.len(), 1, "got: {disk_hits:?}");
        assert_eq!(disk_hits[0].session_id, "jsonl");
    }
}
