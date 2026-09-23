#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Port of `packages/coding-agent/test/session-file-invalid.test.ts` plus
//! the `SessionManager.setSessionFile` / `forkFrom` open boundaries from
//! `packages/coding-agent/test/session-manager/file-operations.test.ts`.
//!
//! - `--session <non-session-file>` prints the upstream `openSessionOrExit`
//!   diagnostic (`Error: Session file is not a valid pi session: <path>`),
//!   exits 1 with no stack frames, and leaves the file byte-identical.
//! - `--session <empty-file>` initializes the file in place with a valid
//!   session header (oracle `_setSessionFile` entries-empty + size-0
//!   branch), runs the turn, and reopens with a stable id.
//! - `--fork <empty-or-invalid-source>` refuses with the upstream
//!   `Cannot fork: source session file is empty or invalid: <path>`
//!   diagnostic and never touches the source.

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
            "pi-session-file-invalid-{tag}-{}",
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

/// Read the first line of a session file as JSON and return its id.
fn header_id(path: &std::path::Path) -> String {
    let content = fs::read_to_string(path).expect("read session header");
    let first = content.lines().next().expect("session header line");
    let value: serde_json::Value = serde_json::from_str(first).expect("session header JSON");
    // Oracle writes the v3 `type: session` shape; the Rust port writes its
    // native v4 `kind: header` shape. Both must carry a non-empty id.
    assert!(
        value.get("type").and_then(serde_json::Value::as_str) == Some("session")
            || value.get("kind").and_then(serde_json::Value::as_str) == Some("header"),
        "unrecognized session header: {first}"
    );
    value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .expect("session header id")
        .to_string()
}

#[test]
fn session_flag_invalid_file_prints_friendly_error_and_preserves_content() {
    let sandbox = Sandbox::new("flag");
    let session_file = sandbox.root.join("non-session-data.log");
    let original = b"{\"type\":\"event\",\"data\":\"not a session\"}\n";
    fs::write(&session_file, original).expect("write non-session file");

    let output = sandbox.run(&["--session", session_file.to_str().unwrap(), "-p", "hi"]);

    assert_eq!(output.status.code(), Some(1), "exit status: {output:?}");
    let diagnostics = stderr(&output);
    assert!(
        diagnostics.contains(&format!(
            "Error: Session file is not a valid pi session: {}",
            session_file.display()
        )),
        "friendly diagnostic missing: {diagnostics}"
    );
    assert!(
        !diagnostics.contains("SessionManager.open"),
        "internal call leaked: {diagnostics}"
    );
    assert!(
        !diagnostics.contains("at "),
        "stack frame leaked: {diagnostics}"
    );
    assert_eq!(
        fs::read(&session_file).unwrap(),
        original,
        "non-session file must stay byte-identical"
    );
}

/// Oracle `file-operations.test.ts`: "truncates and rewrites empty file
/// with valid header" + "preserves explicit session file path" +
/// "subsequent loads of initialized empty file work correctly".
#[test]
fn session_flag_empty_file_initializes_header_and_reopens_with_stable_id() {
    let sandbox = Sandbox::new("empty");
    let session_file = sandbox.root.join("empty-session.jsonl");
    fs::write(&session_file, b"").expect("write empty session file");

    let first = sandbox.run(&[
        "--session",
        session_file.to_str().unwrap(),
        "--provider",
        "faux",
        "--model",
        "faux-1",
        "--no-tools",
        "-p",
        "first",
    ]);
    assert!(
        first.status.success(),
        "empty --session must initialize and run: {}",
        stderr(&first)
    );
    assert!(
        stdout(&first).contains("faux response to: first"),
        "faux turn missing: {}",
        stdout(&first)
    );
    let diagnostics = stderr(&first);
    assert!(
        !diagnostics.contains("is empty"),
        "empty file must not refuse: {diagnostics}"
    );
    // Header initialized in place at the explicit path.
    let first_id = header_id(&session_file);

    let second = sandbox.run(&[
        "--session",
        session_file.to_str().unwrap(),
        "--provider",
        "faux",
        "--model",
        "faux-1",
        "--no-tools",
        "-p",
        "second",
    ]);
    assert!(
        second.status.success(),
        "reopened --session must run: {}",
        stderr(&second)
    );
    assert!(
        stdout(&second).contains("faux response to: second"),
        "faux turn missing: {}",
        stdout(&second)
    );
    assert_eq!(
        header_id(&session_file),
        first_id,
        "reopen must keep the initialized header id"
    );
}

/// Oracle `forkFrom`: an empty source refuses with the upstream
/// `Cannot fork: source session file is empty or invalid` diagnostic and
/// the source stays untouched.
#[test]
fn session_fork_empty_source_fails_closed_without_touching_source() {
    let sandbox = Sandbox::new("fork-empty");
    let source = sandbox.root.join("empty-source.jsonl");
    fs::write(&source, b"").expect("write empty source");

    let output = sandbox.run(&[
        "--fork",
        source.to_str().unwrap(),
        "--provider",
        "faux",
        "--model",
        "faux-1",
        "--no-tools",
        "-p",
        "hi",
    ]);

    assert_eq!(output.status.code(), Some(1), "exit status: {output:?}");
    let diagnostics = stderr(&output);
    assert!(
        diagnostics.contains(&format!(
            "Cannot fork: source session file is empty or invalid: {}",
            source.display()
        )),
        "oracle fork diagnostic missing: {diagnostics}"
    );
    assert_eq!(
        fs::read(&source).unwrap(),
        b"",
        "fork must not initialize the source"
    );
}

/// Oracle `forkFrom`: a non-empty source that is not a pi session refuses
/// with the same `Cannot fork` family (not the open-session friendly
/// message) and the source stays byte-identical.
#[test]
fn session_fork_non_session_source_fails_closed_preserving_content() {
    let sandbox = Sandbox::new("fork-invalid");
    let source = sandbox.root.join("fork-non-session.log");
    let original = b"{\"type\":\"event\",\"data\":\"not a session\"}\n";
    fs::write(&source, original).expect("write non-session source");

    let output = sandbox.run(&[
        "--fork",
        source.to_str().unwrap(),
        "--provider",
        "faux",
        "--model",
        "faux-1",
        "--no-tools",
        "-p",
        "hi",
    ]);

    assert_eq!(output.status.code(), Some(1), "exit status: {output:?}");
    let diagnostics = stderr(&output);
    assert!(
        diagnostics.contains(&format!(
            "Cannot fork: source session file is empty or invalid: {}",
            source.display()
        )),
        "oracle fork diagnostic missing: {diagnostics}"
    );
    assert_eq!(
        fs::read(&source).unwrap(),
        original,
        "fork refusal must preserve the source"
    );
}
