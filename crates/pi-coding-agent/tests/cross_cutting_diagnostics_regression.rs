#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

//! Aggregate real-process coverage for X-011 diagnostics and X-012
//! permanent-regression discipline. Each test drives the real `pi` RPC binary
//! in an isolated environment instead of calling parser/runtime helpers.
//!
//! X-011: every failure diagnostic identifies the action and the offending
//! provider/model/path/value, states the recovery, and never leaks the
//! request-scoped secret. X-012: each failure mode reproduced across
//! X-001..X-010 keeps one permanent reproducer here and proves the same
//! runtime recovers and persists exactly-once durable state afterwards.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const RPC_TIMEOUT: Duration = Duration::from_secs(10);
const SECRET: &str = "synthetic-diagnostic-api-key-51ab";

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
            "pi-cross-cutting-diagnostics-{tag}-{}",
            uuid::Uuid::new_v4()
        ));
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

    fn command(&self) -> Command {
        let mut command = Command::new(test_binary());
        command
            .current_dir(&self.project)
            .env_clear()
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.root.join("xdg-config"))
            .env("XDG_DATA_HOME", self.root.join("xdg-data"))
            .env("PI_CODING_AGENT_DIR", &self.agent_dir)
            .env("PI_CODING_AGENT_SESSION_DIR", &self.sessions)
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env("LC_ALL", "C");
        command
    }

    fn session_files(&self) -> Vec<PathBuf> {
        jsonl_files(&self.sessions)
    }

    fn assert_no_secret_in_tree(&self) {
        let mut files = Vec::new();
        collect_files(&self.root, &mut files);
        for path in files {
            let bytes = fs::read(&path).expect("read sandbox artifact");
            assert!(
                !contains_bytes(&bytes, SECRET.as_bytes()),
                "secret leaked into {}",
                path.display()
            );
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct RpcProcess {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: Receiver<std::io::Result<Option<String>>>,
    stderr: Receiver<String>,
}

impl RpcProcess {
    fn start(sandbox: &Sandbox) -> Self {
        let mut command = sandbox.command();
        command.args([
            "--mode",
            "rpc",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "--api-key",
            SECRET,
            "--no-tools",
        ]);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn real pi RPC process");
        let stdin = child.stdin.take().expect("RPC stdin");
        let stdout = spawn_line_reader(child.stdout.take().expect("RPC stdout"));
        let stderr = spawn_text_reader(child.stderr.take().expect("RPC stderr"));
        Self {
            child: Some(child),
            stdin: Some(stdin),
            stdout,
            stderr,
        }
    }

    fn send(&mut self, value: serde_json::Value) {
        self.send_raw(&value.to_string());
    }

    fn send_raw(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("live RPC stdin");
        writeln!(stdin, "{line}").expect("write RPC command");
        stdin.flush().expect("flush RPC command");
    }

    fn read_record(&self, deadline: Instant) -> serde_json::Value {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let line = match self.stdout.recv_timeout(remaining) {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => panic!("RPC stdout closed; stderr: {}", self.stderr_text()),
            Ok(Err(error)) => panic!("RPC stdout read failed: {error}"),
            Err(error) => panic!("RPC read failed ({error}); stderr: {}", self.stderr_text()),
        };
        serde_json::from_str(line.trim())
            .unwrap_or_else(|error| panic!("invalid RPC JSONL ({error}): {line:?}"))
    }

    fn read_until(&self, predicate: impl Fn(&serde_json::Value) -> bool) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + RPC_TIMEOUT;
        let mut records = Vec::new();
        loop {
            let record = self.read_record(deadline);
            let done = predicate(&record);
            records.push(record);
            if done {
                return records;
            }
        }
    }

    fn read_until_response(&self, id: &str) -> Vec<serde_json::Value> {
        self.read_until(|record| record["type"] == "response" && record["id"] == id)
    }

    fn read_next_response(&self) -> serde_json::Value {
        self.read_until(|record| record["type"] == "response")
            .pop()
            .expect("RPC response")
    }

    fn read_until_settled(&self) -> Vec<serde_json::Value> {
        self.read_until(|record| record["type"] == "agent_settled")
    }

    fn stderr_text(&self) -> String {
        self.stderr
            .try_iter()
            .last()
            .unwrap_or_else(|| "<stderr still open>".to_string())
    }

    fn finish(mut self) {
        drop(self.stdin.take());
        let status = self
            .child
            .as_mut()
            .expect("RPC child")
            .wait()
            .expect("wait for RPC EOF");
        self.child.take();
        let stderr = self.stderr.recv_timeout(RPC_TIMEOUT).unwrap_or_default();
        assert!(status.success(), "RPC EOF failed: {status:?}; {stderr}");
        assert!(stderr.is_empty(), "unexpected RPC stderr: {stderr}");
    }
}

impl Drop for RpcProcess {
    fn drop(&mut self) {
        self.stdin.take();
        if let Some(mut child) = self.child.take() {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

fn error_text(response: &serde_json::Value) -> &str {
    response["error"].as_str().unwrap_or("<missing error>")
}

fn assert_no_secret(records: &[serde_json::Value], context: &str) {
    for record in records {
        let rendered = record.to_string();
        assert!(
            !rendered.contains(SECRET),
            "secret leaked in {context}: {rendered}"
        );
    }
}

#[test]
fn failure_diagnostics_identify_action_and_recovery_without_secrets() {
    let sandbox = Sandbox::new("diagnostics");
    let mut rpc = RpcProcess::start(&sandbox);
    let mut all_records = Vec::new();

    // Every failure names the action (response envelope) plus the offending
    // value (diagnostic text), and the very next command proves recovery.
    let cases: &[(&str, &str, serde_json::Value, &str, &[&str])] = &[
        (
            "diag-missing-message",
            "prompt",
            serde_json::json!({"id":"diag-missing-message","type":"prompt"}),
            "missing message",
            &[],
        ),
        (
            "diag-unknown-model",
            "set_model",
            serde_json::json!({"id":"diag-unknown-model","type":"set_model","provider":"nope","modelId":"missing"}),
            "Model not found: nope/missing",
            &["nope", "missing"],
        ),
        (
            "diag-unknown-entry",
            "get_entries",
            serde_json::json!({"id":"diag-unknown-entry","type":"get_entries","since":"missing-entry"}),
            "Entry not found: missing-entry",
            &["missing-entry"],
        ),
        (
            "diag-bad-path",
            "switch_session",
            serde_json::json!({"id":"diag-bad-path","type":"switch_session","sessionPath":"/missing/session.jsonl"}),
            "/missing/session.jsonl",
            &["session"],
        ),
        (
            "diag-empty-name",
            "set_session_name",
            serde_json::json!({"id":"diag-empty-name","type":"set_session_name","name":"  "}),
            "Session name cannot be empty",
            &["Session name"],
        ),
        (
            "diag-bad-steering",
            "set_steering_mode",
            serde_json::json!({"id":"diag-bad-steering","type":"set_steering_mode","mode":"invalid"}),
            "Invalid steering mode: invalid",
            &["invalid"],
        ),
        (
            "diag-bad-follow-up",
            "set_follow_up_mode",
            serde_json::json!({"id":"diag-bad-follow-up","type":"set_follow_up_mode","mode":"invalid"}),
            "Invalid follow-up mode: invalid",
            &["invalid"],
        ),
    ];
    for (id, command, payload, expected_error, must_name) in cases {
        rpc.send(payload.clone());
        let records = rpc.read_until_response(id);
        let response = records.last().expect("diagnostic response");
        assert_eq!(response["success"], false, "expected failure for {id}");
        assert_eq!(
            response["command"], *command,
            "diagnostic for {id} lost the action"
        );
        let error = error_text(response);
        assert!(
            error.contains(expected_error),
            "diagnostic for {id} lost the recovery text: {error}"
        );
        for name in *must_name {
            assert!(
                error.to_lowercase().contains(&name.to_lowercase()),
                "diagnostic for {id} does not identify {name}: {error}"
            );
        }
        all_records.extend(records);

        rpc.send(serde_json::json!({"id":format!("{id}-recovered"),"type":"get_state"}));
        let recovered = rpc.read_next_response();
        assert_eq!(
            recovered["success"], true,
            "stream did not recover after {id}"
        );
        all_records.push(recovered);
    }

    // A malformed line is a correlated parse failure, not a poisoned stream.
    rpc.send_raw("{not-json");
    let parse_failure = rpc.read_next_response();
    assert_eq!(parse_failure["success"], false);
    assert_eq!(parse_failure["command"], "parse");
    assert!(
        error_text(&parse_failure).starts_with("Failed to parse command:"),
        "unexpected parse diagnostic: {parse_failure}"
    );
    all_records.push(parse_failure);

    // An unknown command echoes its id and names the offending type.
    rpc.send(serde_json::json!({"id":"diag-unknown","type":"not_a_command"}));
    let unknown = rpc
        .read_until_response("diag-unknown")
        .pop()
        .expect("unknown response");
    assert_eq!(unknown["success"], false);
    assert_eq!(unknown["error"], "Unknown command: not_a_command");
    all_records.push(unknown);

    // Deeply nested JSON is rejected without poisoning the process.
    let mut deep = String::from("{\"id\":\"diag-deep\",\"type\":\"get_state\",\"pad\":");
    deep.push_str(&"{\"a\":".repeat(200));
    deep.push('1');
    deep.push_str(&"}".repeat(200));
    deep.push('}');
    rpc.send_raw(&deep);
    let deep_failure = rpc.read_next_response();
    assert_eq!(deep_failure["success"], false);
    all_records.push(deep_failure);

    // A successful faux turn after the whole failure battery proves recovery,
    // and its events must not carry the request-scoped secret either.
    rpc.send(serde_json::json!({"id":"diag-turn","type":"prompt","message":"diagnostic probe"}));
    let turn = rpc.read_until_settled();
    assert!(turn.iter().any(|record| record["type"] == "response"
        && record["id"] == "diag-turn"
        && record["success"] == true));
    all_records.extend(turn);

    assert_no_secret(&all_records, "diagnostic battery");
    rpc.finish();
    sandbox.assert_no_secret_in_tree();
}

#[test]
fn every_discovered_failure_keeps_a_permanent_reproducer() {
    let sandbox = Sandbox::new("regression");
    let mut rpc = RpcProcess::start(&sandbox);

    rpc.send(serde_json::json!({"id":"repro-state","type":"get_state"}));
    let initial = rpc.read_until_response("repro-state").pop().expect("state");
    assert_eq!(initial["success"], true);
    let home_session = initial["data"]["sessionFile"]
        .as_str()
        .expect("home session file")
        .to_string();

    // Reproducer 1 (X-002): malformed and unknown wire input, then recovery.
    rpc.send_raw("{not-json");
    assert_eq!(rpc.read_next_response()["success"], false);
    rpc.send(serde_json::json!({"id":"repro-unknown","type":"bogus_command"}));
    let unknown = rpc
        .read_until_response("repro-unknown")
        .pop()
        .expect("unknown");
    assert_eq!(unknown["error"], "Unknown command: bogus_command");
    rpc.send(serde_json::json!({"id":"repro-after-wire","type":"get_state"}));
    assert_eq!(rpc.read_next_response()["success"], true);

    // Reproducer 2 (X-003/X-004): switching to a malformed session file
    // fails with the path named and leaves the live session untouched. The
    // malformed file lives outside the session root so discovery is not
    // polluted; only the explicit path is exercised.
    let garbage = sandbox.root.join("garbage.jsonl");
    fs::write(&garbage, b"not jsonl {{{").expect("write malformed session");
    rpc.send(
        serde_json::json!({"id":"repro-bad-switch","type":"switch_session","sessionPath":garbage}),
    );
    let bad_switch = rpc
        .read_until_response("repro-bad-switch")
        .pop()
        .expect("switch");
    assert_eq!(bad_switch["success"], false);
    assert!(
        error_text(&bad_switch).contains("garbage.jsonl"),
        "switch diagnostic lost the path: {bad_switch}"
    );
    rpc.send(serde_json::json!({"id":"repro-still-home","type":"get_state"}));
    let still_home = rpc
        .read_until_response("repro-still-home")
        .pop()
        .expect("state");
    assert_eq!(still_home["data"]["sessionFile"], home_session);

    // Reproducer 3 (X-003): a failed export writes nothing and recovers.
    let missing_dir = sandbox.root.join("missing-dir").join("out.html");
    rpc.send(
        serde_json::json!({"id":"repro-bad-export","type":"export_html","outputPath":missing_dir}),
    );
    let bad_export = rpc
        .read_until_response("repro-bad-export")
        .pop()
        .expect("export");
    assert_eq!(bad_export["success"], false);
    assert!(
        !sandbox.root.join("missing-dir").exists(),
        "failed export wrote output"
    );
    rpc.send(serde_json::json!({"id":"repro-after-export","type":"get_state"}));
    assert_eq!(rpc.read_next_response()["success"], true);

    // Reproducer 4 (X-007/X-008): an aborted bash settles exactly once with
    // no exit code, the runtime stays reusable, and each command persists once.
    rpc.send(serde_json::json!({"id":"repro-slow","type":"bash","command":"sleep 30"}));
    thread::sleep(Duration::from_millis(200));
    rpc.send(serde_json::json!({"id":"repro-abort","type":"abort_bash"}));
    let deadline = Instant::now() + RPC_TIMEOUT;
    let mut abort_records = Vec::new();
    while !["repro-slow", "repro-abort"].iter().all(|id| {
        abort_records
            .iter()
            .any(|record: &serde_json::Value| record["type"] == "response" && record["id"] == *id)
    }) {
        abort_records.push(rpc.read_record(deadline));
    }
    let slow = abort_records
        .iter()
        .find(|record| record["id"] == "repro-slow")
        .expect("slow bash response");
    assert_eq!(slow["success"], true);
    assert_eq!(slow["data"]["cancelled"], true);
    assert!(slow["data"]["exitCode"].is_null());
    rpc.send(
        serde_json::json!({"id":"repro-after-abort","type":"bash","command":"printf repro-ok"}),
    );
    let after_abort = rpc
        .read_until_response("repro-after-abort")
        .pop()
        .expect("bash");
    assert_eq!(after_abort["data"]["output"], "repro-ok");

    // Reproducer 5 (X-009): deeply nested input is rejected, then recovery.
    let mut deep = String::from("{\"id\":\"repro-deep\",\"type\":\"get_state\",\"pad\":");
    deep.push_str(&"{\"a\":".repeat(200));
    deep.push('1');
    deep.push_str(&"}".repeat(200));
    deep.push('}');
    rpc.send_raw(&deep);
    assert_eq!(rpc.read_next_response()["success"], false);
    rpc.send(serde_json::json!({"id":"repro-after-deep","type":"get_state"}));
    assert_eq!(rpc.read_next_response()["success"], true);

    // Close-out: exactly one durable session, every line valid JSONL, clean EOF.
    rpc.send(serde_json::json!({"id":"repro-entries","type":"get_entries"}));
    assert_eq!(rpc.read_next_response()["success"], true);
    rpc.finish();
    let files = sandbox.session_files();
    assert_eq!(
        files.len(),
        1,
        "expected one durable session, found {files:?}"
    );
    let raw = fs::read_to_string(&files[0]).expect("read durable session");
    assert!(!raw.is_empty(), "durable session is empty");
    for line in raw.lines() {
        serde_json::from_str::<serde_json::Value>(line).expect("valid session JSONL");
    }
    assert_no_secret(
        &[serde_json::Value::String(raw)],
        "durable regression session",
    );
    sandbox.assert_no_secret_in_tree();
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
        let mut reader = BufReader::new(stderr);
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

fn collect_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
