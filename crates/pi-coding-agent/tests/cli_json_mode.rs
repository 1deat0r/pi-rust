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
fn json_mode_surfaces_model_error() {
    let sandbox = Sandbox::new("error");
    // A provider with no key errors; the error must surface on stderr.
    let out = sandbox.pi(
        &sandbox.root,
        &["--mode", "json", "--provider", "openai", "--model", "gpt-5.4", "hi"],
    );
    assert!(!out.status.success(), "expected failure");
    assert!(
        sandbox.stderr(&out).contains("Error"),
        "stderr: {}",
        sandbox.stderr(&out)
    );
}