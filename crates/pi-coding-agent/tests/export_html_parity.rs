//! Export-html parity integration test: our export pipeline must produce
//! byte-identical HTML to the oracle (scripts/oracle_export_html.mjs), which
//! reproduces the upstream generateHtml string pipeline against the SAME
//! vendored template assets.
//!
//! Regenerate goldens with:
//!   node scripts/oracle_export_html.mjs \
//!     crates/pi-coding-agent/tests/fixtures/export_session.jsonl dark \
//!     crates/pi-coding-agent/tests/fixtures/export_html_golden/dark.html
//!   node scripts/oracle_export_html.mjs \
//!     crates/pi-coding-agent/tests/fixtures/export_session.jsonl light \
//!     crates/pi-coding-agent/tests/fixtures/export_html_golden/light.html

use std::path::PathBuf;

fn first_diff(a: &str, b: &str) -> Option<(usize, String, String)> {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    let n = ab.len().min(bb.len());
    for i in 0..n {
        if ab[i] != bb[i] {
            let astart = i.saturating_sub(60);
            let aend = (i + 80).min(ab.len());
            let bstart = i.saturating_sub(60);
            let bend = (i + 80).min(bb.len());
            return Some((
                i,
                String::from_utf8_lossy(&ab[astart..aend]).to_string(),
                String::from_utf8_lossy(&bb[bstart..bend]).to_string(),
            ));
        }
    }
    if ab.len() != bb.len() {
        return Some((
            n,
            format!("len {} (ours)", ab.len()),
            format!("len {} (golden)", bb.len()),
        ));
    }
    None
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

#[test]
fn export_matches_oracle_golden_dark() {
    let session = fixture_dir().join("export_session.jsonl");
    let golden =
        std::fs::read_to_string(fixture_dir().join("export_html_golden/dark.html")).unwrap();
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
    if ours != golden {
        let (pos, a, b) = first_diff(&ours, &golden).unwrap();
        panic!("dark export differs from oracle golden at byte {pos}\nOurs:   {a:?}\nGolden: {b:?}\n\nOurs tail: {:?}\nGolden tail: {:?}", &ours[ours.len().saturating_sub(60)..], &golden[golden.len().saturating_sub(60)..]);
    }
    let _ = std::fs::remove_dir_all(&out_dir);
}

#[test]
fn export_matches_oracle_golden_light() {
    let session = fixture_dir().join("export_session.jsonl");
    let golden =
        std::fs::read_to_string(fixture_dir().join("export_html_golden/light.html")).unwrap();
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
    assert_eq!(ours, golden, "light export differs from oracle golden");
    let _ = std::fs::remove_dir_all(&out_dir);
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
    // The parity fixture (regenerated from scripts/oracle_export_html.mjs)
    // exercises the entry rows #86 calls out: tool calls, compaction, and
    // branch summaries — so the byte-identical golden covers them too.
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
