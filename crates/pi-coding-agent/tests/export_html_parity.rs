#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Export-html integration coverage for the Rust-only static renderer.
//!
//! The old JavaScript oracle/golden comparison intentionally no longer
//! applies: the 100%-Rust distribution renders a self-contained document and
//! does not ship or execute a browser runtime.

use std::path::PathBuf;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

#[test]
fn export_renders_static_dark_html() {
    let session = fixture_dir().join("export_session.jsonl");
    let out_dir = std::env::temp_dir().join(format!(
        "pi-export-parity-{}-{}",
        std::process::id(),
        line!()
    ));
    std::fs::create_dir_all(&out_dir).unwrap();
    let out = out_dir.join("dark.html");
    let path = pi_coding_agent::core::export_html::export_session_file(
        session.to_str().unwrap(),
        Some(out.to_str().unwrap()),
        Some("dark"),
    )
    .unwrap();
    let ours = std::fs::read_to_string(&path).unwrap();
    assert_static_export(&ours, "#8abeb7");
    let _ = std::fs::remove_dir_all(&out_dir);
}

#[test]
fn export_renders_static_light_html() {
    let session = fixture_dir().join("export_session.jsonl");
    let out_dir = std::env::temp_dir().join(format!(
        "pi-export-parity-{}-{}",
        std::process::id(),
        line!()
    ));
    std::fs::create_dir_all(&out_dir).unwrap();
    let out = out_dir.join("light.html");
    let path = pi_coding_agent::core::export_html::export_session_file(
        session.to_str().unwrap(),
        Some(out.to_str().unwrap()),
        Some("light"),
    )
    .unwrap();
    let ours = std::fs::read_to_string(&path).unwrap();
    assert_static_export(&ours, "#5a8080");
    let _ = std::fs::remove_dir_all(&out_dir);
}

fn assert_static_export(html: &str, accent: &str) {
    assert!(html.starts_with("<!DOCTYPE html>"));
    assert!(html.contains("Static zero-JavaScript export"));
    assert!(html.contains(&format!("--accent: {accent};")));
    assert!(html.contains("hello"));
    assert!(html.contains("hi there"));
    assert!(html.contains("Let me check."));
    assert!(html.contains("Reasoning step..."));
    assert!(html.contains("Summarized earlier turns."));
    assert!(html.contains("Branch summarized."));
    assert!(
        !html.contains("<script"),
        "static export must not contain scripts"
    );
    assert!(
        !html.contains("onclick="),
        "static export must not contain handlers"
    );
}

fn entry_type(v: &serde_json::Value) -> Option<&str> {
    v.get("type").and_then(|t| t.as_str())
}

fn msg_has_content_block(v: &serde_json::Value, block_type: &str) -> bool {
    v.get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
        .is_some_and(|c| {
            c.iter()
                .any(|b| b.get("type").and_then(|t| t.as_str()) == Some(block_type))
        })
}

#[test]
fn fixture_covers_tool_compaction_and_branch_summary() {
    // The fixture exercises tool calls, compaction, and branch summaries so
    // the static Rust renderer covers each specialized entry row.
    let session = fixture_dir().join("export_session.jsonl");
    let loaded =
        pi_coding_agent::core::export_html::load_session_file(session.to_str().unwrap()).unwrap();
    let entries = &loaded.entries;
    assert_eq!(
        entries.len(),
        8,
        "expected the 8 expanded non-session entries"
    );
    assert_eq!(loaded.leaf_id.as_deref(), Some("msg_6"));

    assert!(
        entries
            .iter()
            .any(|e| entry_type(e) == Some("message") && msg_has_content_block(e, "toolCall")),
        "fixture must include an assistant tool-call block"
    );
    assert!(
        entries
            .iter()
            .any(|e| entry_type(e) == Some("message") && msg_has_content_block(e, "thinking")),
        "fixture must include a thinking block"
    );
    assert!(
        entries.iter().any(|e| entry_type(e) == Some("compaction")),
        "fixture must include a compaction entry"
    );
    assert!(
        entries
            .iter()
            .any(|e| entry_type(e) == Some("branch_summary")),
        "fixture must include a branch_summary entry"
    );
}
