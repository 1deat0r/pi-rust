#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Port of `packages/coding-agent/test/session-id-readonly.test.ts`
//! (case 1 — metadata commands stay read-only for `--session-id`).
//!
//! Cases 2–3 of the oracle file are already covered elsewhere:
//! create/reopen-without-warning by
//! `cli_session_restart_parity::explicit_session_id_reopens_the_same_session_across_processes`
//! (second run asserts empty stderr), and the fork-existing-target
//! rejection by the in-process `run::tests` fork-conflict pin — the
//! oracle itself tests that case in-process too.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    agent_dir: PathBuf,
    sessions: PathBuf,
    project: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "pi-session-id-readonly-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        let agent_dir = root.join("agent");
        let sessions = root.join("sessions");
        let project = root.join("project");
        for path in [&home, &agent_dir, &sessions, &project] {
            fs::create_dir_all(path).expect("create isolated test directory");
        }
        Self {
            root,
            home,
            agent_dir,
            sessions,
            project,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(test_binary())
            .current_dir(&self.project)
            .env_clear()
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.root.join("xdg-config"))
            .env("XDG_DATA_HOME", self.root.join("xdg-data"))
            .env("PI_CODING_AGENT_DIR", &self.agent_dir)
            .env("PI_CODING_AGENT_SESSION_DIR", &self.sessions)
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env("LC_ALL", "C")
            .args(args)
            .output()
            .expect("spawn real pi process")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn test_binary() -> PathBuf {
    std::env::var_os("PI_RUST_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pi")))
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Recursively collect `.jsonl` files under `root`.
fn jsonl_files(root: &std::path::Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(jsonl_files(&path));
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            found.push(path);
        }
    }
    found.sort();
    found
}

/// Oracle: `--session-id read-only-help --help` exits 0 and persists no
/// session — metadata commands must never materialize a durable file for
/// the requested id.
#[test]
fn session_id_with_help_does_not_persist_any_session() {
    let sandbox = Sandbox::new("help");
    let output = sandbox.run(&["--session-id", "read-only-help", "--help"]);

    assert_eq!(output.status.code(), Some(0), "exit status: {output:?}");
    assert!(
        stdout(&output).contains("Usage:"),
        "help output missing: {}",
        stdout(&output)
    );
    assert!(
        stderr(&output).is_empty(),
        "unexpected stderr: {}",
        stderr(&output)
    );
    assert!(
        jsonl_files(&sandbox.sessions).is_empty(),
        "help must not persist a session, found: {:?}",
        jsonl_files(&sandbox.sessions)
    );
    assert!(
        !any_session_header_has_id(&sandbox.agent_dir, "read-only-help"),
        "no header may carry the requested id after --help"
    );
}

/// Recursively scan first lines of `.jsonl` files for a header id.
fn any_session_header_has_id(root: &std::path::Path, id: &str) -> bool {
    jsonl_files(root).iter().any(|path| {
        fs::read_to_string(path)
            .ok()
            .and_then(|content| content.lines().next().map(str::to_string))
            .and_then(|line| serde_json::from_str::<serde_json::Value>(&line).ok())
            .and_then(|header| {
                header
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .is_some_and(|header_id| header_id == id)
    })
}
