//! Binary-level tests for `--mode json` (JSON event stream over stdout).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    agent_dir: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("pi-json-mode-{tag}-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let agent_dir = home.join(".pi").join("agent");
        fs::create_dir_all(&agent_dir).unwrap();
        Self { root, home, agent_dir }
    }

    fn pi(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_pi"))
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("PI_CODING_AGENT_DIR", &self.agent_dir)
            .env_remove("PI_PROVIDER")
            .env_remove("PI_MODEL")
            .env_remove("PI_KEY")
            .env_remove("PI_SESSION_ID")
            .args(args)
            .output()
            .expect("spawn pi")
    }

    fn stdout(&self, output: &std::process::Output) -> String {
        String::from_utf8_lossy(&output.stdout).to_string()
    }
    fn stderr(&self, output: &std::process::Output) -> String {
        String::from_utf8_lossy(&output.stderr).to_string()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn json_mode_emits_event_lines() {
    let sandbox = Sandbox::new("events");
    let out = sandbox.pi(
        &sandbox.root,
        &["--mode", "json", "--provider", "faux", "--model", "faux-1", "hello"],
    );
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    let stdout = sandbox.stdout(&out);
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(!lines.is_empty(), "expected JSON event lines");
    // Every line is valid JSON with a type field.
    for line in &lines {
        let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON line");
        assert!(v.get("type").is_some(), "event must carry a type: {line}");
    }
    // The stream includes message_update events and the final text.
    let has_update = lines.iter().any(|l| {
        serde_json::from_str::<serde_json::Value>(l)
            .map(|v| v["type"] == "message_update")
            .unwrap_or(false)
    });
    assert!(has_update, "expected message_update events: {stdout}");
    let all = stdout.clone();
    assert!(all.contains("faux response to: hello"), "expected faux reply: {all}");
}

#[test]
fn json_mode_streams_terminal_error_as_event_and_exits_zero() {
    let sandbox = Sandbox::new("error");
    // A provider with no key terminates the stream in an error. Upstream
    // `runPrintMode` in *json* mode delivers the error as a JSON event line
    // on stdout and exits 0 (only text mode turns Error/Aborted into a
    // nonzero exit).
    let out = sandbox.pi(
        &sandbox.root,
        &["--mode", "json", "--provider", "openai", "--model", "gpt-5.4", "hi"],
    );
    assert!(out.status.success(), "json mode must exit 0, stderr: {}", sandbox.stderr(&out));
    let stdout = sandbox.stdout(&out);
    // Every line is valid JSON carrying a type.
    let mut seen_update = false;
    for line in stdout.lines() {
        let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON line");
        assert!(v.get("type").is_some(), "event must carry a type: {line}");
        if v["type"] == "message_update" {
            seen_update = true;
        }
    }
    // A terminal error still reaches the stream as a message_update event
    // (the falsey text is not required; the event envelope is the contract).
    assert!(!stdout.trim().is_empty(), "expected JSON event lines on stdout");
    assert!(seen_update, "expected a message_update event on the wire: {stdout}");
}