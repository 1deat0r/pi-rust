#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Clean-room black-box acceptance coverage for the `pi` executable.
//!
//! Every case starts a real child process with an empty environment apart from
//! the minimum isolated runtime variables.  Set `PI_RUST_TEST_BINARY` to run
//! the same matrix against an already-built release executable; otherwise the
//! Cargo-provided integration-test executable is used.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const RPC_TIMEOUT: Duration = Duration::from_secs(10);

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    agent_dir: PathBuf,
    sessions: PathBuf,
    project: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("pi-clean-home-cli-{tag}-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let agent_dir = root.join("agent");
        let sessions = root.join("sessions");
        let project = root.join("project");
        for path in [&home, &agent_dir, &sessions, &project] {
            fs::create_dir_all(path).expect("create clean-room directory");
        }
        Self {
            root,
            home,
            agent_dir,
            sessions,
            project,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(test_binary());
        command
            .current_dir(&self.project)
            // A clean environment catches accidental dependence on the
            // operator's credentials, provider, model, or shell settings.
            .env_clear()
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.root.join("xdg-config"))
            .env("XDG_DATA_HOME", self.root.join("xdg-data"))
            .env("PI_CODING_AGENT_DIR", &self.agent_dir)
            .env("PI_CODING_AGENT_SESSION_DIR", &self.sessions)
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env("RUST_BACKTRACE", "1")
            .env("LC_ALL", "C");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command()
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("spawn pi {}: {error}", test_binary().display()))
    }

    fn run_with_stdin(&self, args: &[&str], input: &[u8]) -> Output {
        let mut child = self
            .command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("spawn pi {}: {error}", test_binary().display()));
        child
            .stdin
            .take()
            .expect("child stdin")
            .write_all(input)
            .expect("write child stdin");
        child.wait_with_output().expect("wait for pi stdin process")
    }

    fn session_files(&self) -> Vec<PathBuf> {
        jsonl_files(&self.sessions)
    }

    fn session_tree_snapshot(&self) -> Vec<(PathBuf, Option<Vec<u8>>)> {
        tree_snapshot(&self.sessions)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct RpcProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: Receiver<std::io::Result<Option<String>>>,
    stderr: Receiver<String>,
}

impl RpcProcess {
    fn start(sandbox: &Sandbox, no_session: bool) -> Self {
        let mut command = sandbox.command();
        let mut args = vec![
            "--mode",
            "rpc",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "--no-tools",
        ];
        if no_session {
            args.push("--no-session");
        }
        let mut child = command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("spawn pi RPC process: {error}"));

        let stdout = spawn_line_reader(child.stdout.take().expect("RPC stdout"));
        let stderr = spawn_text_reader(child.stderr.take().expect("RPC stderr"));
        let stdin = child.stdin.take().expect("RPC stdin");
        Self {
            child,
            stdin,
            stdout,
            stderr,
        }
    }

    fn send(&mut self, value: serde_json::Value) {
        writeln!(self.stdin, "{value}").expect("write RPC command");
        self.stdin.flush().expect("flush RPC command");
    }

    fn read_record(&self, deadline: Instant) -> serde_json::Value {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let line = match self.stdout.recv_timeout(remaining) {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => panic!("RPC stdout closed; stderr: {}", self.stderr_text()),
            Ok(Err(error)) => panic!("RPC stdout read failed: {error}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("RPC timed out; stderr: {}", self.stderr_text())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!(
                    "RPC stdout reader disconnected; stderr: {}",
                    self.stderr_text()
                )
            }
        };
        serde_json::from_str(line.trim())
            .unwrap_or_else(|error| panic!("invalid RPC JSONL ({error}): {line:?}"))
    }

    fn read_until_settled(&self) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + RPC_TIMEOUT;
        let mut records = Vec::new();
        loop {
            let record = self.read_record(deadline);
            let settled = record["type"] == "agent_settled";
            records.push(record);
            if settled {
                return records;
            }
        }
    }

    fn read_until_response(&self, id: &str) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + RPC_TIMEOUT;
        let mut records = Vec::new();
        loop {
            let record = self.read_record(deadline);
            let response = record["type"] == "response" && record["id"] == id;
            records.push(record);
            if response {
                return records;
            }
        }
    }

    fn stderr_text(&self) -> String {
        self.stderr
            .try_iter()
            .last()
            .unwrap_or_else(|| "<stderr still open>".to_string())
    }

    fn finish(mut self) -> Output {
        drop(self.stdin);
        let status = self.child.wait().expect("wait for RPC EOF");
        let mut stdout = Vec::new();
        while let Ok(Ok(Some(line))) = self.stdout.try_recv() {
            stdout.extend_from_slice(line.as_bytes());
        }
        let stderr = self
            .stderr
            .try_iter()
            .last()
            .unwrap_or_default()
            .into_bytes();
        Output {
            status,
            stdout,
            stderr,
        }
    }
}

fn test_binary() -> PathBuf {
    std::env::var_os("PI_RUST_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pi")))
}

fn spawn_line_reader(stdout: ChildStdout) -> Receiver<std::io::Result<Option<String>>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = sender.send(Ok(None));
                    break;
                }
                Ok(_) => {
                    if sender.send(Ok(Some(line))).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error));
                    break;
                }
            }
        }
    });
    receiver
}

fn spawn_text_reader(stderr: impl Read + Send + 'static) -> Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stderr);
        let mut text = String::new();
        let _ = reader.read_to_string(&mut text);
        let _ = sender.send(text);
    });
    receiver
}

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
    files.sort();
    files
}

fn tree_snapshot(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    fn visit(root: &Path, current: &Path, snapshot: &mut Vec<(PathBuf, Option<Vec<u8>>)>) {
        let entries = fs::read_dir(current).expect("read session directory");
        for entry in entries {
            let entry = entry.expect("read session directory entry");
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .expect("session entry is below session root")
                .to_path_buf();
            let file_type = entry.file_type().expect("read session entry type");
            if file_type.is_dir() {
                snapshot.push((relative, None));
                visit(root, &path, snapshot);
            } else if file_type.is_file() {
                snapshot.push((relative, Some(fs::read(&path).expect("read session entry"))));
            }
        }
    }

    let mut snapshot = Vec::new();
    visit(root, root, &mut snapshot);
    snapshot.sort_by(|left, right| left.0.cmp(&right.0));
    snapshot
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn clean_home_help_and_version_are_process_stable() {
    let sandbox = Sandbox::new("metadata");

    let help = sandbox.run(&["--help"]);
    assert!(help.status.success(), "--help stderr: {}", stderr(&help));
    assert!(stdout(&help).contains("Usage:"));
    assert!(stdout(&help).contains("--mode <mode>"));
    assert!(
        stderr(&help).is_empty(),
        "unexpected --help stderr: {}",
        stderr(&help)
    );

    let version = sandbox.run(&["--version"]);
    assert!(
        version.status.success(),
        "--version stderr: {}",
        stderr(&version)
    );
    assert_eq!(stdout(&version), "pi 0.84.2\n");
    assert!(
        stderr(&version).is_empty(),
        "unexpected --version stderr: {}",
        stderr(&version)
    );
    assert!(
        sandbox.session_files().is_empty(),
        "metadata commands created sessions"
    );
}

#[test]
fn text_mode_is_a_real_process_and_no_session_is_ephemeral() {
    let sandbox = Sandbox::new("text");
    let output = sandbox.run(&[
        "--mode",
        "text",
        "--provider",
        "faux",
        "--model",
        "faux-1",
        "--no-tools",
        "--no-session",
        "text prompt",
    ]);
    assert!(output.status.success(), "text stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "faux response to: text prompt\n");
    assert!(
        sandbox.session_files().is_empty(),
        "--no-session wrote JSONL"
    );
}

#[test]
fn clean_home_without_credentials_reports_no_models_not_provider_failure() {
    let sandbox = Sandbox::new("no-credentials");
    let output = sandbox.run(&["--mode", "text", "--no-tools", "clean startup probe"]);

    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    assert!(
        stdout(&output).is_empty(),
        "unexpected stdout: {}",
        stdout(&output)
    );
    let diagnostic = stderr(&output);
    assert!(
        diagnostic.starts_with("No models available."),
        "{diagnostic}"
    );
    assert!(
        !diagnostic.contains("Provider is not configured: google"),
        "clean startup selected an unauthenticated default: {diagnostic}"
    );
    assert!(
        sandbox.session_files().is_empty(),
        "failed startup wrote JSONL"
    );
}

#[test]
fn json_mode_emits_valid_events_and_persists_a_clean_home_session() {
    let sandbox = Sandbox::new("json");
    let output = sandbox.run(&[
        "--mode",
        "json",
        "--provider",
        "faux",
        "--model",
        "faux-1",
        "--no-tools",
        "json prompt",
    ]);
    assert!(output.status.success(), "json stderr: {}", stderr(&output));
    let records = stdout(&output)
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("valid JSON event"))
        .collect::<Vec<_>>();
    assert!(!records.is_empty(), "JSON mode emitted no events");
    assert_eq!(records[0]["type"], "session");
    assert_eq!(records[0]["version"], 3);
    assert!(records[0]["timestamp"]
        .as_str()
        .is_some_and(|value| value.ends_with('Z')));
    assert!(records
        .iter()
        .filter(|record| record["type"] != "session")
        .all(|record| record["type"].is_string()));
    assert!(records.iter().any(|record| {
        record["type"] == "message_end" && record["message"]["role"] == "assistant"
    }));
    assert_eq!(
        sandbox.session_files().len(),
        1,
        "JSON mode session missing"
    );
}

#[test]
fn rpc_mode_accepts_a_real_prompt_and_cleanly_handles_stdin_eof() {
    let sandbox = Sandbox::new("rpc");
    let mut rpc = RpcProcess::start(&sandbox, false);
    rpc.send(serde_json::json!({
        "id": "clean-room-turn",
        "type": "prompt",
        "message": "rpc prompt"
    }));
    let records = rpc.read_until_settled();
    assert!(records.iter().any(|record| {
        record["type"] == "response"
            && record["id"] == "clean-room-turn"
            && record["success"] == true
    }));
    assert!(records.iter().any(|record| {
        record["type"] == "message_end"
            && record["message"]["role"] == "assistant"
            && record["message"]["content"]
                .as_array()
                .is_some_and(|blocks| {
                    blocks.iter().any(|block| {
                        block["text"]
                            .as_str()
                            .is_some_and(|text| text.contains("faux response to: rpc prompt"))
                    })
                })
    }));
    let output = rpc.finish();
    assert!(
        output.status.success(),
        "RPC EOF stderr: {}",
        stderr(&output)
    );
    assert_eq!(sandbox.session_files().len(), 1, "RPC session missing");
}

#[test]
fn piped_stdin_is_consumed_before_eof_and_empty_eof_does_not_hang() {
    let piped = Sandbox::new("stdin-content");
    let output = piped.run_with_stdin(
        &[
            "--mode",
            "text",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "--no-tools",
        ],
        b"stdin prompt\n",
    );
    assert!(output.status.success(), "stdin stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "faux response to: stdin prompt\n");

    let empty = Sandbox::new("stdin-eof");
    let output = empty.run_with_stdin(
        &[
            "--mode",
            "text",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "--no-tools",
        ],
        b"",
    );
    assert!(
        output.status.success(),
        "empty-EOF stderr: {}",
        stderr(&output)
    );
    // With no positional prompt, EOF is a clean no-op. The text-mode wrapper
    // still prints its final empty line, and the process must terminate.
    assert_eq!(stdout(&output), "\n");
}

#[test]
fn malformed_flags_fail_at_the_process_boundary() {
    let unknown = Sandbox::new("unknown-flag");
    let output = unknown.run(&["--definitely-not-a-pi-flag"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("Unknown flag: --definitely-not-a-pi-flag"));

    let missing = Sandbox::new("missing-value");
    let output = missing.run(&["--provider"]);
    // Keep the process contract aligned with the pinned Pi parser: value
    // flags other than --name are silently ignored when no value follows.
    // With no initial prompt this is therefore a clean no-op. Text mode
    // emits its normal final newline.
    assert!(output.status.success());
    assert_eq!(stdout(&output), "\n");
    assert!(stderr(&output).is_empty());

    let invalid_mode = Sandbox::new("invalid-mode");
    let output = invalid_mode.run(&["--mode", "not-a-mode"]);
    // The pinned Pi parser consumes an invalid mode value and leaves the
    // mode unset. In a non-TTY clean-room process that resolves to the
    // no-prompt print path, so it must terminate cleanly without inventing a
    // different diagnostic contract. Text mode emits its normal final newline.
    assert!(output.status.success());
    assert_eq!(stdout(&output), "\n");
    assert!(stderr(&output).is_empty());
}

#[test]
fn json_mode_honors_no_session() {
    let json = Sandbox::new("json-no-session");
    let output = json.run(&[
        "--mode",
        "json",
        "--provider",
        "faux",
        "--model",
        "faux-1",
        "--no-tools",
        "--no-session",
        "json ephemeral",
    ]);
    assert!(output.status.success(), "JSON stderr: {}", stderr(&output));
    let records: Vec<serde_json::Value> = stdout(&output)
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSON event line"))
        .collect();
    assert_eq!(
        records.first().map(|record| record["type"].as_str()),
        Some(Some("session"))
    );
    assert_eq!(
        records.first().map(|record| record["version"].as_u64()),
        Some(Some(3))
    );
    assert_eq!(
        records.first().map(|record| record["cwd"].as_str()),
        Some(Some(json.project.to_str().expect("project path")))
    );
    assert!(
        json.session_files().is_empty(),
        "JSON --no-session wrote JSONL"
    );
}

#[test]
fn rpc_no_session_startup_leaves_directory_untouched_and_omits_session_file() {
    let rpc = Sandbox::new("rpc-no-session-state");
    fs::write(
        rpc.sessions.join("pre-existing-marker.txt"),
        b"must remain unchanged",
    )
    .expect("write session-directory marker");
    let before = rpc.session_tree_snapshot();
    let mut process = RpcProcess::start(&rpc, true);

    assert_eq!(
        rpc.session_tree_snapshot(),
        before,
        "--no-session startup changed the configured session directory"
    );

    process.send(serde_json::json!({
        "id": "state-before",
        "type": "get_state"
    }));
    let records = process.read_until_response("state-before");
    let response = records
        .iter()
        .find(|record| record["type"] == "response" && record["id"] == "state-before")
        .expect("get_state response");
    assert_eq!(response["success"], true);
    assert!(
        response["data"].get("sessionFile").is_none(),
        "in-memory get_state must omit sessionFile: {response}"
    );
    assert_eq!(response["data"]["messageCount"], 0);
    assert_eq!(rpc.session_tree_snapshot(), before);

    let output = process.finish();
    assert!(
        output.status.success(),
        "RPC --no-session stderr: {}",
        stderr(&output)
    );
    assert_eq!(rpc.session_tree_snapshot(), before);
    assert!(rpc.session_files().is_empty());
}

#[test]
fn rpc_no_session_prompt_and_session_commands_remain_in_memory() {
    let rpc = Sandbox::new("rpc-no-session-lifecycle");
    fs::write(
        rpc.sessions.join("pre-existing-marker.txt"),
        b"must remain unchanged",
    )
    .expect("write session-directory marker");
    let before = rpc.session_tree_snapshot();
    let mut process = RpcProcess::start(&rpc, true);

    process.send(serde_json::json!({
        "id": "ephemeral",
        "type": "prompt",
        "message": "rpc ephemeral"
    }));
    let records = process.read_until_settled();
    assert!(records.iter().any(|record| {
        record["type"] == "response" && record["id"] == "ephemeral" && record["success"] == true
    }));
    assert!(records.iter().any(|record| {
        record["type"] == "message_end"
            && record["message"]["role"] == "assistant"
            && record["message"]["content"]
                .as_array()
                .is_some_and(|blocks| {
                    blocks.iter().any(|block| {
                        block["text"]
                            .as_str()
                            .is_some_and(|text| text.contains("faux response to: rpc ephemeral"))
                    })
                })
    }));

    process.send(serde_json::json!({
        "id": "messages",
        "type": "get_messages"
    }));
    let records = process.read_until_response("messages");
    let messages = records
        .iter()
        .find(|record| record["type"] == "response" && record["id"] == "messages")
        .expect("get_messages response");
    assert_eq!(messages["success"], true);
    let messages = messages["data"]["messages"]
        .as_array()
        .expect("in-memory messages array");
    assert!(messages.iter().any(|message| {
        message["role"] == "user"
            && (message["content"].as_str() == Some("rpc ephemeral")
                || message["content"].as_array().is_some_and(|blocks| {
                    blocks
                        .iter()
                        .any(|block| block["text"].as_str() == Some("rpc ephemeral"))
                }))
    }));
    assert!(messages.iter().any(|message| {
        message["role"] == "assistant"
            && message["content"].as_array().is_some_and(|blocks| {
                blocks.iter().any(|block| {
                    block["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("faux response to: rpc ephemeral"))
                })
            })
    }));

    process.send(serde_json::json!({
        "id": "entries",
        "type": "get_entries"
    }));
    let records = process.read_until_response("entries");
    let entries = records
        .iter()
        .find(|record| record["type"] == "response" && record["id"] == "entries")
        .expect("get_entries response");
    assert_eq!(entries["success"], true);
    let entries = entries["data"]["entries"]
        .as_array()
        .expect("in-memory entries array");
    assert!(entries
        .iter()
        .any(|entry| entry["message"]["role"] == "user"));
    assert!(entries
        .iter()
        .any(|entry| entry["message"]["role"] == "assistant"));
    assert_eq!(rpc.session_tree_snapshot(), before);

    process.send(serde_json::json!({
        "id": "name",
        "type": "set_session_name",
        "name": "ephemeral name"
    }));
    let records = process.read_until_response("name");
    assert!(records.iter().any(|record| {
        record["type"] == "session_info_changed" && record["name"] == "ephemeral name"
    }));
    assert!(records.iter().any(|record| {
        record["type"] == "response" && record["id"] == "name" && record["success"] == true
    }));
    assert_eq!(rpc.session_tree_snapshot(), before);

    process.send(serde_json::json!({
        "id": "new",
        "type": "new_session"
    }));
    let records = process.read_until_response("new");
    let response = records
        .iter()
        .find(|record| record["type"] == "response" && record["id"] == "new")
        .expect("new_session response");
    assert_eq!(response["success"], true);
    assert_eq!(response["data"]["cancelled"], false);
    assert_eq!(rpc.session_tree_snapshot(), before);
    assert!(rpc.session_files().is_empty());

    let output = process.finish();
    assert!(
        output.status.success(),
        "RPC --no-session stderr: {}",
        stderr(&output)
    );
    assert_eq!(rpc.session_tree_snapshot(), before);
    assert!(rpc.session_files().is_empty());
}

#[test]
fn pi_rust_test_binary_override_is_accepted_for_this_matrix() {
    let Some(path) = std::env::var_os("PI_RUST_TEST_BINARY") else {
        return;
    };
    let path = PathBuf::from(path);
    assert!(
        path.is_file(),
        "PI_RUST_TEST_BINARY is not a file: {}",
        path.display()
    );
    let sandbox = Sandbox::new("override");
    let output = sandbox.run(&["--version"]);
    assert!(
        output.status.success(),
        "override stderr: {}",
        stderr(&output)
    );
    assert_eq!(stdout(&output), "pi 0.84.2\n");
}
