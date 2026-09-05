#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Port of repository-level cases from
//! `packages/agent/test/harness/session/jsonl.test.ts` (metadata contract,
//! listing, id validation, sequence restore, forks, validation on open).

use pi_agent::fs::{FileSystem, MemoryFs};
use pi_agent::session::jsonl::repo::jsonl_session_directory_name;
use pi_agent::session::state::{EntryOrder, EntryQuery, ForkOptions};
use pi_agent::session::types::{Entry, EntryNoStats, SessionErrorKind};
use pi_agent::session::{CreateOptions, JsonlSessionRepo};
use pi_ai::types::{ContentBlock, Message, UserContent};

fn user_message(text: &str) -> pi_agent::types::AgentMessage {
    pi_agent::types::AgentMessage::Core(Message::User(UserContent::blocks(
        vec![ContentBlock::text(text)],
        1,
    )))
}

fn repo(fs: MemoryFs) -> JsonlSessionRepo<MemoryFs> {
    JsonlSessionRepo::new(fs.clone(), "/sessions")
}

#[test]
fn cwd_directory_encoding_strips_only_one_leading_separator() {
    assert_eq!(
        jsonl_session_directory_name("/tmp/project"),
        "--tmp-project--"
    );
    assert_eq!(
        jsonl_session_directory_name("//tmp/project"),
        "---tmp-project--"
    );
    assert_eq!(
        jsonl_session_directory_name("\\\\tmp\\project"),
        "---tmp-project--"
    );
}

#[test]
fn exposes_complete_metadata_contract_with_cwd_layout() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let fs = MemoryFs::new();
        let mut r = repo(fs.clone());
        let cwd = "/work/workspace/project".to_string();
        let session = r
            .create(CreateOptions {
                id: Some("metadata".into()),
                cwd: cwd.clone(),
                parent_session_id: Some("parent".into()),
                metadata: Some(serde_json::json!({"owner": "agent", "nested": {"enabled": true}})),
                fork_options: ForkOptions::Tree,
            })
            .await
            .unwrap();
        let meta = session.get_metadata().await;
        assert_eq!(meta.id, "metadata");
        assert_eq!(meta.cwd, "/work/workspace/project");
        assert_eq!(meta.parent_session_id.as_deref(), Some("parent"));
        assert_eq!(meta.source_format, 4);
        // Path follows the --<cwd-with-dashes>-- layout and the timestamped file name.
        let dir = jsonl_session_directory_name(&cwd);
        assert!(meta.path.contains(&dir));
        assert!(meta.path.ends_with(&format!("_{}.jsonl", "metadata")));
        assert_eq!(meta.created_at, parse_ts_from_path(&meta.path));

        // listing by cwd finds it; other cwd doesn't.
        let listed = r.list(Some(&cwd)).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "metadata");
        assert!(r.list(Some("/work/other")).await.unwrap().is_empty());
    });
}

#[test]
fn explicitly_selected_v3_create_and_source_format_fork_reopen() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let fs = MemoryFs::new();
        let mut r = repo(fs.clone());
        let mut source = r
            .create_v3(CreateOptions::new("/work".to_string()).with_id("v3-source"))
            .await
            .unwrap();
        source
            .append_entry(
                EntryNoStats::Message {
                    id: "m1".into(),
                    message: user_message("hello"),
                    terminate: None,
                },
                "main",
            )
            .await
            .unwrap();
        let source_meta = source.get_metadata().await;
        assert_eq!(source_meta.source_format, 3);
        assert!(fs
            .content(&source_meta.path)
            .unwrap()
            .starts_with("{\"type\":\"session\",\"version\":3"));

        let fork = r
            .fork(
                &source_meta,
                CreateOptions::new("/work".to_string()).with_id("v3-fork"),
            )
            .await
            .unwrap();
        let fork_meta = fork.get_metadata().await;
        assert_eq!(fork_meta.source_format, 3);
        assert!(fs
            .content(&fork_meta.path)
            .unwrap()
            .starts_with("{\"type\":\"session\",\"version\":3"));
        assert_eq!(
            fork.find_entries(&EntryQuery::default())
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            r.open(&fork_meta)
                .await
                .unwrap()
                .get_metadata()
                .await
                .source_format,
            3
        );
    });
}

fn parse_ts_from_path(path: &str) -> u64 {
    // session files are <iso-ts>_<id>.jsonl; reconstruct the ms timestamp.
    let name = path.rsplit('/').next().unwrap();
    let ts_part = name.split('_').next().unwrap();
    // parse format YYYY-MM-DDTHH-MM-SS-mmmZ
    let y: u64 = ts_part[0..4].parse().unwrap();
    let mo: u64 = ts_part[5..7].parse().unwrap();
    let d: u64 = ts_part[8..10].parse().unwrap();
    let h: u64 = ts_part[11..13].parse().unwrap();
    let mi: u64 = ts_part[14..16].parse().unwrap();
    let s: u64 = ts_part[17..19].parse().unwrap();
    let ms: u64 = ts_part[20..23].parse().unwrap();
    let days = days_from_civil(y as i64, mo as i64, d as i64);
    (((days * 24 + h as i64) * 60 + mi as i64) * 60 + s as i64) as u64 * 1000 + ms
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[test]
fn rejects_malformed_header_on_open_and_skips_when_listing() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let fs = MemoryFs::new();
        let mut r = repo(fs.clone());
        r.create(CreateOptions::new("/work".to_string()).with_id("valid"))
            .await
            .unwrap();
        let session = r
            .create(CreateOptions::new("/work".to_string()).with_id("malformed-header"))
            .await
            .unwrap();
        let meta = session.get_metadata().await;
        fs.write_file(&meta.path, "not json\n").unwrap();

        let opened = r.open(&meta).await;
        assert!(opened.is_err());
        let listed = r.list(Some("/work")).await.unwrap();
        assert_eq!(
            listed.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["valid"]
        );
        assert_eq!(fs.content(&meta.path).unwrap(), "not json\n");
    });
}

#[test]
fn rejects_non_object_header_metadata_on_open_and_skips_when_listing() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let fs = MemoryFs::new();
        let mut r = repo(fs.clone());
        r.create(CreateOptions::new("/work".to_string()).with_id("valid2")).await.unwrap();
        let session = r.create(CreateOptions::new("/work".to_string()).with_id("bad-meta")).await.unwrap();
        let meta = session.get_metadata().await;
        let malformed = format!(
            "{}\n",
            serde_json::json!({"kind": "header", "version": 4, "id": meta.id, "createdAt": meta.created_at, "cwd": meta.cwd, "metadata": "invalid"})
        );
        fs.write_file(&meta.path, &malformed).unwrap();
        assert!(r.open(&meta).await.is_err());
        let listed = r.list(Some("/work")).await.unwrap();
        assert_eq!(listed.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["valid2"]);
    });
}

#[test]
fn rejects_session_ids_that_cannot_be_used_in_filenames() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let fs = MemoryFs::new();
        let mut r = repo(fs.clone());
        let err = r
            .create(CreateOptions {
                id: Some("../escape".into()),
                cwd: "/work".into(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(err.kind, SessionErrorKind::InvalidPayload);
    });
}

#[test]
fn allows_same_explicit_id_in_different_cwds() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let fs = MemoryFs::new();
        let mut r = repo(fs.clone());
        let a = r
            .create(CreateOptions {
                id: Some("shared".into()),
                cwd: "/work/one".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let b = r
            .create(CreateOptions {
                id: Some("shared".into()),
                cwd: "/work/two".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(a.get_metadata().await.cwd, "/work/one");
        assert_eq!(b.get_metadata().await.cwd, "/work/two");
        // both listed (different dirs, same filename)
        let listed = r.list(None).await.unwrap();
        assert_eq!(listed.iter().filter(|m| m.id == "shared").count(), 2);
    });
}

#[test]
fn rejects_concurrent_create_for_same_destination() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let fs = MemoryFs::new();
        let mut r = repo(fs.clone());
        let cwd = "/work".to_string();
        let options = CreateOptions {
            id: Some("same".into()),
            cwd,
            ..Default::default()
        };
        let first = r.create(options.clone()).await.unwrap();
        let err = r.create(options.clone()).await.unwrap_err();
        assert_eq!(err.kind, SessionErrorKind::AlreadyExists);
        let listed = r.list(Some("/work")).await.unwrap();
        assert_eq!(listed.iter().filter(|m| m.id == "same").count(), 1);
        let _ = first;
    });
}

#[test]
fn sorts_listed_sessions_by_mtime_descending() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let fs = MemoryFs::new();
        let mut r = repo(fs.clone());
        let newest = r
            .create(CreateOptions {
                id: Some("newest".into()),
                cwd: "/work/n".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let newest_meta = newest.get_metadata().await;
        let oldest = r
            .create(CreateOptions {
                id: Some("oldest".into()),
                cwd: "/work/o".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let oldest_meta = oldest.get_metadata().await;
        fs.set_mtime(&newest_meta.path, 1_700_000_002_000);
        fs.set_mtime(&oldest_meta.path, 1_700_000_001_000);

        let listed = r.list(None).await.unwrap();
        assert_eq!(
            listed.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["newest", "oldest"]
        );
        let filtered = r.list(Some("/work/n")).await.unwrap();
        assert_eq!(
            filtered.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["newest"]
        );
    });
}

#[test]
fn writes_one_line_per_mutation_and_restores_shared_sequence() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let fs = MemoryFs::new();
        let mut r = repo(fs.clone());
        let mut session = r
            .create(CreateOptions::new("/work".to_string()).with_id("session"))
            .await
            .unwrap();
        session
            .append_entry(
                EntryNoStats::Message {
                    id: "m1".into(),
                    message: user_message("a"),
                    terminate: None,
                },
                "main",
            )
            .await
            .unwrap();
        session
            .append_entry(
                EntryNoStats::Message {
                    id: "m2".into(),
                    message: user_message("b"),
                    terminate: None,
                },
                "main",
            )
            .await
            .unwrap();
        let m3 = session
            .append_entry(
                EntryNoStats::Message {
                    id: "m3".into(),
                    message: user_message("c"),
                    terminate: None,
                },
                "main",
            )
            .await
            .unwrap();
        assert_eq!(m3.seq(), 3);

        let meta = session.get_metadata().await;
        let reopened = r.open(&meta).await.unwrap();
        let entries = reopened
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|e| e.id().to_string())
                .collect::<Vec<_>>(),
            vec!["m1", "m2", "m3"]
        );
        assert_eq!(
            entries.iter().map(|e| e.seq()).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );

        // Reopened session continues the shared sequence (record append -> seq 4).
        let mut reopened = reopened;
        let rec = reopened
            .append_record(pi_agent::session::types::NewRecord::AbortRequested {
                id: "abort".into(),
                lane: "main".into(),
                run_id: "r".into(),
            })
            .await
            .unwrap();
        assert_eq!(rec.seq(), 4);
    });
}

#[test]
fn reopens_tree_fork_with_lanes_and_facts() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let fs = MemoryFs::new();
        let mut r = repo(fs.clone());
        let mut source = r
            .create(CreateOptions::new("/work".to_string()).with_id("source"))
            .await
            .unwrap();
        source
            .append_entry(
                EntryNoStats::Message {
                    id: "m1".into(),
                    message: user_message("a"),
                    terminate: None,
                },
                "main",
            )
            .await
            .unwrap();
        source
            .append_entry(
                EntryNoStats::Custom {
                    id: "c1".into(),
                    custom_type: "note".into(),
                    data: None,
                },
                "main",
            )
            .await
            .unwrap();
        source.create_lane("branch", Some("m1")).await.unwrap();
        source.set_name(Some("A named session")).await.unwrap();
        source.set_label("m1", Some("checkpoint")).await.unwrap();
        let source_meta = source.get_metadata().await;

        let fork = r
            .fork(
                &source_meta,
                CreateOptions {
                    id: Some("forked".into()),
                    cwd: "/work".into(),
                    fork_options: ForkOptions::Tree,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let fork_meta = fork.get_metadata().await;
        assert_eq!(fork_meta.parent_session_id.as_deref(), Some("source"));
        let entries = fork
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(entries.len(), 2);
        // Fork renumbers seq from 1.
        assert_eq!(entries[0].seq(), 1);
        assert_eq!(entries[1].seq(), 2);
        let lanes = fork.get_lanes().await;
        assert_eq!(lanes.len(), 2);
        assert!(lanes
            .iter()
            .any(|l| l.lane == "branch" && l.leaf_id.as_deref() == Some("m1")));
        assert_eq!(fork.get_name().await.as_deref(), Some("A named session"));
        assert_eq!(fork.get_label("m1").await.as_deref(), Some("checkpoint"));

        // Full reopen after reload preserves lanes/facts.
        let reopened = r.open(&fork_meta).await.unwrap();
        assert_eq!(
            reopened.get_name().await.as_deref(),
            Some("A named session")
        );
        assert_eq!(
            reopened.get_label("m1").await.as_deref(),
            Some("checkpoint")
        );
        assert_eq!(
            reopened
                .find_entries(&EntryQuery::default())
                .await
                .unwrap()
                .len(),
            2
        );
    });
}

#[test]
fn recomputes_fork_message_counts_when_reopening() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let fs = MemoryFs::new();
        let mut r = repo(fs.clone());
        let mut source = r
            .create(CreateOptions::new("/work".to_string()).with_id("source2"))
            .await
            .unwrap();
        source
            .append_entry(
                EntryNoStats::Message {
                    id: "m1".into(),
                    message: user_message("a"),
                    terminate: None,
                },
                "main",
            )
            .await
            .unwrap();
        source
            .append_entry(
                EntryNoStats::Custom {
                    id: "c1".into(),
                    custom_type: "note".into(),
                    data: None,
                },
                "main",
            )
            .await
            .unwrap();
        // branch fork at m1 (position at) => copies only m1; stats recount = 1 message
        let source_meta = source.get_metadata().await;
        let fork = r
            .fork(
                &source_meta,
                CreateOptions {
                    id: Some("branch-fork".into()),
                    cwd: "/work".into(),
                    fork_options: ForkOptions::Branch {
                        entry_id: Some("m1".into()),
                        position: Some(pi_agent::session::ForkPosition::At),
                    },
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        // In-memory stats already recomputed from applied mutations.
        assert_eq!(fork.get_stats().await.message_count, 1);
        let fork_meta = fork.get_metadata().await;
        let reopened = r.open(&fork_meta).await.unwrap();
        assert_eq!(reopened.get_stats().await.message_count, 1);
    });
}

#[test]
fn rejects_imported_entry_referencing_missing_parent() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let fs = MemoryFs::new();
        #[allow(clippy::redundant_clone)]
        let mut r = repo(fs.clone());
        r.create(CreateOptions::new("/work".to_string()).with_id("anchored")).await.unwrap();
        // Write a session file with a lane-bound entry that chains to a
        // missing parent (parent id not present).
        let path = "/sessions/--work--/broken.jsonl";
        fs.ensure_dir("/sessions/--work--");
        let header = serde_json::json!({"kind":"header","version":4,"id":"broken","createdAt":1,"cwd":"/work"});
        let line = serde_json::json!({
            "kind": "entry", "lane": "main", "type": "custom", "id": "x", "seq": 1,
            "parentId": "missing", "timestamp": 1, "customType": "note",
        });
        fs.write_file(path, &format!("{header}\n{line}\n")).unwrap();
        // open via repo requires list metadata; use storage directly for load semantics.
        let err = pi_agent::session::jsonl::storage::JsonlSessionStorage::<MemoryFs>::load(fs.clone(), path).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid"), "expected invalid entry error, got {msg}");
    });
}

#[test]
fn rejects_non_consecutive_sequence_during_replay() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let fs = MemoryFs::new();
        let path = "/sessions/--work--/gap.jsonl";
        fs.ensure_dir("/sessions/--work--");
        let header =
            serde_json::json!({"kind":"header","version":4,"id":"gap","createdAt":1,"cwd":"/work"});
        let line = serde_json::json!({
            "kind": "entry", "lane": "main", "type": "custom", "id": "x", "seq": 2,
            "parentId": null, "timestamp": 1, "customType": "note",
        });
        fs.write_file(path, &format!("{header}\n{line}\n")).unwrap();

        let error =
            pi_agent::session::jsonl::storage::JsonlSessionStorage::<MemoryFs>::load(fs, path)
                .await
                .expect_err("a sequence gap corrupts the session log");
        let message = error.to_string();
        assert!(
            message.contains("non-consecutive seq 2"),
            "expected sequence-integrity diagnostic, got {message}"
        );
    });
}

#[test]
fn replay_accepts_unknown_entry_fields_without_losing_tree_semantics() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let fs = MemoryFs::new();
        let path = "/sessions/--work--/forward-compatible.jsonl";
        fs.ensure_dir("/sessions/--work--");
        let header = serde_json::json!({
            "kind": "header", "version": 4, "id": "forward-compatible",
            "createdAt": 1, "cwd": "/work"
        });
        let message = serde_json::json!({
            "kind": "entry", "lane": "main", "type": "message", "id": "m1",
            "seq": 1, "parentId": null, "timestamp": 1, "terminate": true,
            "futureEntry": {"owner": "extension"},
            "message": {
                "role": "user", "content": [{"type": "text", "text": "hello", "futureBlock": 1}],
                "timestamp": 1, "futureMessage": "raw"
            }
        });
        let custom = serde_json::json!({
            "kind": "entry", "lane": "main", "type": "custom", "id": "c1",
            "seq": 2, "parentId": "m1", "timestamp": 2, "customType": "note",
            "data": {"text": "world"}, "futureEntry": 42
        });
        fs.write_file(path, &format!("{header}\n{message}\n{custom}\n"))
            .unwrap();

        let storage = pi_agent::session::jsonl::storage::JsonlSessionStorage::<MemoryFs>::load(
            fs.clone(),
            path,
        )
        .await
        .expect("unknown fields must not prevent replay");
        let entries = storage
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..EntryQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert!(matches!(
            &entries[0],
            Entry::Message {
                id,
                parent_id: None,
                terminate: Some(true),
                ..
            } if id == "m1"
        ));
        assert!(matches!(
            &entries[1],
            Entry::Custom {
                id,
                parent_id: Some(parent),
                custom_type,
                ..
            } if id == "c1" && parent == "m1" && custom_type == "note"
        ));
        let raw = fs.content(path).unwrap();
        assert!(raw.contains("futureEntry"));
        assert!(raw.contains("futureMessage"));
    });
}
