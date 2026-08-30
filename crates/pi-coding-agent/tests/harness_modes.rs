#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! C1 harness/mode contract tests.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn jsonl_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(jsonl_files(&path));
        } else if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            files.push(path);
        }
    }
    files
}

#[test]
fn json_mode_emits_complete_lifecycle_and_persists_jsonl() {
    let root = std::env::temp_dir().join(format!("pi-c1-json-{}", uuid::Uuid::new_v4()));
    let home = root.join("home");
    let agent_dir = home.join(".pi").join("agent");
    let sessions = root.join("sessions");
    fs::create_dir_all(&agent_dir).unwrap();
    fs::create_dir_all(&sessions).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_pi"))
        .current_dir(&root)
        .env("HOME", &home)
        .env("PI_CODING_AGENT_DIR", &agent_dir)
        .env("PI_CODING_AGENT_SESSION_DIR", &sessions)
        .args([
            "--mode",
            "json",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "hello",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "json mode failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSONL event"))
        .collect();
    assert_eq!(
        lines.first().and_then(|event| event["type"].as_str()),
        Some("session")
    );
    assert_eq!(
        lines.first().and_then(|event| event["version"].as_u64()),
        Some(3)
    );
    let events: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|event| event["type"] != "session")
        .collect();
    let event_types: Vec<&str> = events
        .iter()
        .map(|event| event["type"].as_str().expect("event type"))
        .collect();
    assert_eq!(event_types.first(), Some(&"agent_start"));
    assert_eq!(event_types.get(1), Some(&"turn_start"));
    assert_eq!(event_types.last(), Some(&"agent_settled"));
    let assistant_start = events
        .iter()
        .position(|event| {
            event["type"] == "message_start" && event["message"]["role"] == "assistant"
        })
        .expect("assistant message_start");
    let message_update = event_types
        .iter()
        .position(|event_type| *event_type == "message_update")
        .expect("assistant message_update");
    let assistant_end = events
        .iter()
        .position(|event| event["type"] == "message_end" && event["message"]["role"] == "assistant")
        .expect("assistant message_end");
    let turn_end = event_types
        .iter()
        .position(|event_type| *event_type == "turn_end")
        .expect("turn_end");
    assert!(assistant_start < message_update);
    assert!(message_update < assistant_end);
    assert!(assistant_end < turn_end);
    for event_type in [
        "agent_start",
        "turn_start",
        "message_start",
        "message_update",
        "message_end",
        "turn_end",
        "agent_end",
    ] {
        assert!(
            events.iter().any(|event| event["type"] == event_type),
            "missing {event_type}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    let persisted = jsonl_files(&sessions);
    assert!(
        !persisted.is_empty(),
        "JSON mode must create a durable JSONL session"
    );
    let session_text = persisted
        .iter()
        .map(|path| fs::read_to_string(path).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(session_text.contains("\"type\":\"message\""));
    assert!(session_text.contains("\"role\":\"assistant\""));

    let _ = fs::remove_dir_all(root);
}
