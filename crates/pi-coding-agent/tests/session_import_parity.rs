#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! SES-012 invalid-import refusal pins (offline).
//!
//! `SessionManager::prepare_import` must refuse a missing file before
//! touching the session dir, and `session_from_import` must refuse a
//! file whose first line is not a valid session header — without
//! copying anything into the session dir.
//!
//! Slice AG also pins the positive side of `importFromJsonl`
//! (agent-session-runtime.ts:361): a valid **v3** session header is
//! accepted on import and open (oracle test
//! agent-session-runtime.test.ts:229-245 imports a v3 header-only file).
//!
//! Slice AH closes the pure-sdk follow-up: oracle
//! `parseSessionHeaderCandidate` validates type+id only, so a v3
//! header with a malformed `timestamp` or a missing `cwd` still
//! opens (oracle `_loadEntries` → `migrateToCurrentVersion` never
//! rewrites version ≥ 3; `buildSessionInfo` NaN→mtime, session-cwd
//! falsy guard). The CLI `--session` path already accepts both via
//! `migrate_legacy_session_file` (slice AF pins); the pure-sdk path
//! must too, before the strict storage load.

use pi_coding_agent::core::sdk::SessionManager;

fn sandbox(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pi-import-parity-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn prepare_import_refuses_a_missing_file_before_creating_the_session_dir() {
    let root = sandbox("missing");
    let session_dir = root.join("sessions");
    assert!(!session_dir.exists());
    let manager = SessionManager::new(
        root.to_string_lossy().as_ref(),
        session_dir.to_string_lossy().as_ref(),
    );

    let missing = root.join("no-such-session.jsonl");
    let err = manager
        .prepare_import(&missing)
        .expect_err("missing import source must refuse");
    assert!(
        err.contains("File not found"),
        "diagnostic names the refusal, got: {err}"
    );
    assert!(
        !session_dir.exists(),
        "refusal precedes session-dir creation"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn session_from_import_refuses_an_invalid_header() {
    let root = sandbox("bad-header");
    let session_dir = root.join("sessions");
    let manager = SessionManager::new(
        root.to_string_lossy().as_ref(),
        session_dir.to_string_lossy().as_ref(),
    );

    let bad = root.join("bad.jsonl");
    std::fs::write(&bad, "not-json\n{\"type\":\"message\"}\n").unwrap();
    // Oracle `importFromJsonl` copies before `SessionManager.open`
    // validates, so the refusal surfaces from open — the session is
    // never adopted. Assert the diagnostic, not the staging copy.
    let err = manager
        .session_from_import(&bad, None)
        .await
        .expect_err("invalid header must refuse");
    assert!(
        err.contains("invalid session header"),
        "diagnostic names the refusal, got: {err}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Oracle fixture: agent-session-runtime.test.ts:229-235 — a v3
/// `type:"session"` header (version 3, ISO timestamp, string cwd) is
/// a valid `importFromJsonl` source.
fn v3_header_only(id: &str, cwd: &str) -> String {
    format!(
        r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-01-02T03:04:05.000Z","cwd":{cwd:?}}}"#
    )
}

#[tokio::test]
async fn session_from_import_accepts_a_v3_header() {
    let root = sandbox("v3-import");
    let session_dir = root.join("sessions");
    let manager = SessionManager::new(
        root.to_string_lossy().as_ref(),
        session_dir.to_string_lossy().as_ref(),
    );

    let source = root.join("legacy-v3.jsonl");
    std::fs::write(
        &source,
        format!(
            "{}\n",
            v3_header_only("imported-v3", &root.to_string_lossy())
        ),
    )
    .unwrap();

    let session = manager
        .session_from_import(&source, None)
        .await
        .expect("v3 header must import");
    let metadata = session.get_metadata().await;
    assert_eq!(metadata.id, "imported-v3", "import adopts the v3 id");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn open_session_accepts_a_v3_header() {
    let root = sandbox("v3-open");
    let session_dir = root.join("sessions");
    let manager = SessionManager::new(
        root.to_string_lossy().as_ref(),
        session_dir.to_string_lossy().as_ref(),
    );

    let source = root.join("legacy-open-v3.jsonl");
    std::fs::write(
        &source,
        format!("{}\n", v3_header_only("opened-v3", &root.to_string_lossy())),
    )
    .unwrap();

    let session = manager
        .open_session(&source)
        .await
        .expect("v3 header must open");
    let metadata = session.get_metadata().await;
    assert_eq!(metadata.id, "opened-v3", "open adopts the v3 id");
    let _ = std::fs::remove_dir_all(&root);
}

/// Oracle type+id-only validation: bad `timestamp` does not refuse
/// (session-manager.ts:568; `buildSessionInfo` NaN→mtime :743).
/// CLI pins this via migration (session_file_invalid
/// `session_flag_v3_header_with_bad_timestamp_still_opens`); the
/// pure-sdk path must accept it too.
#[tokio::test]
async fn session_from_import_accepts_a_v3_header_with_bad_timestamp() {
    let root = sandbox("v3-import-badts");
    let session_dir = root.join("sessions");
    let manager = SessionManager::new(
        root.to_string_lossy().as_ref(),
        session_dir.to_string_lossy().as_ref(),
    );

    let source = root.join("bad-ts.jsonl");
    std::fs::write(
        &source,
        format!(
            r#"{{"type":"session","version":3,"id":"badts-sdk","timestamp":"not-a-date","cwd":{}}}"#,
            serde_json::to_string(&root.to_string_lossy()).unwrap()
        )
        .replace('\n', "")
        + "\n",
    )
    .unwrap();

    let session = manager
        .session_from_import(&source, None)
        .await
        .expect("v3 bad-timestamp header must import");
    let metadata = session.get_metadata().await;
    assert_eq!(metadata.id, "badts-sdk", "import adopts the type+id header");
    let _ = std::fs::remove_dir_all(&root);
}

/// Same oracle acceptance for a header missing `cwd`
/// (`getSessionHeaderCwd` non-string → undefined → process.cwd;
/// session-cwd falsy empty guard).
#[tokio::test]
async fn open_session_accepts_a_v3_header_without_cwd() {
    let root = sandbox("v3-open-nocwd");
    let session_dir = root.join("sessions");
    let manager = SessionManager::new(
        root.to_string_lossy().as_ref(),
        session_dir.to_string_lossy().as_ref(),
    );

    let source = root.join("no-cwd.jsonl");
    std::fs::write(
        &source,
        r#"{"type":"session","version":3,"id":"nocwd-sdk","timestamp":"2025-01-01T00:00:00Z"}"#
            .to_string()
            + "\n",
    )
    .unwrap();

    let session = manager
        .open_session(&source)
        .await
        .expect("v3 no-cwd header must open");
    let metadata = session.get_metadata().await;
    assert_eq!(metadata.id, "nocwd-sdk", "open adopts the type+id header");
    let _ = std::fs::remove_dir_all(&root);
}
