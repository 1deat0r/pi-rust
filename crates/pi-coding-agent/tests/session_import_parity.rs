#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! SES-012 invalid-import refusal pins (offline).
//!
//! `SessionManager::prepare_import` must refuse a missing file before
//! touching the session dir, and `session_from_import` must refuse a
//! file whose first line is not a valid session header — without
//! copying anything into the session dir.

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
