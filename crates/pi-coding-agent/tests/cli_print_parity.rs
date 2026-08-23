//! Binary-level print-mode parity tests (T3 #42/#43): sequential multi-turn
//! prompting and terminal error/abort exit semantics, matching upstream
//! `modes/print-mode.ts`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    sessions: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("pi-print-parity-{tag}-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let sessions = root.join("sessions");
        fs::create_dir_all(home.join(".pi").join("agent")).unwrap();
        fs::create_dir_all(&sessions).unwrap();
        Self {
            root,
            home,
            sessions,
        }
    }

    fn pi(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_pi"))
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("PI_CODING_AGENT_SESSION_DIR", &self.sessions)
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

    /// Walk `sessions` recursively and count assistant-role message entries in
    /// the session JSONL files that the run just wrote.
    fn count_assistant_entries(&self) -> usize {
        walk_jsonl(&self.sessions)
            .into_iter()
            .map(|path| count_assistants(&path))
            .sum()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn walk_jsonl(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                out.extend(walk_jsonl(&p));
            } else if p.extension().map(|e| e == "jsonl").unwrap_or(false) {
                out.push(p);
            }
        }
    }
    out
}

/// Count `"role":"assistant"` occurrences across all message lines (a
/// line-per-${seq} scan would be over-engineered; the role string is unique
/// to assistant messages).
fn count_assistants(path: &Path) -> usize {
    let content = fs::read_to_string(path).unwrap_or_default();
    content.matches("\"role\":\"assistant\"").count()
}

#[test]
fn multiple_messages_are_prompted_as_sequential_turns() {
    let sandbox = Sandbox::new("multi-turn");
    let cwd = sandbox.root.clone();
    let out = sandbox.pi(
        &cwd,
        &[
            "-p",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "first",
            "second",
        ],
    );
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    // The visible final output is the last assistant turn.
    assert!(
        sandbox.stdout(&out).contains("faux response to: second"),
        "no final reply"
    );

    let assistant_entries = sandbox.count_assistant_entries();
    // Sequential turns ⇒ two assistant entries persisted (a batched run would
    // persist a single assistant turn).
    assert_eq!(
        assistant_entries, 2,
        "expected two assistant turns, got {assistant_entries}"
    );
}

#[test]
fn terminal_provider_error_exits_nonzero_with_raw_message() {
    let sandbox = Sandbox::new("provider-error");
    let cwd = sandbox.root.clone();
    // A real provider with no key terminates in an error; print mode must
    // surface it on stderr and exit nonzero (upstream exitCode = 1).
    let out = sandbox.pi(
        &cwd,
        &["-p", "--provider", "openai", "--model", "gpt-5.4", "hi"],
    );
    assert!(!out.status.success(), "expected nonzero exit");
    let stderr = sandbox.stderr(&out);
    assert!(
        !stderr.starts_with("Error: model error"),
        "print mode must surface the raw error, got: {stderr}"
    );
    // Upstream prints the raw error message (not wrapped).
    assert!(!stderr.is_empty(), "expected an error message on stderr");
    assert!(
        !sandbox.stdout(&out).contains("faux"),
        "no reply expected on stdout"
    );
}
