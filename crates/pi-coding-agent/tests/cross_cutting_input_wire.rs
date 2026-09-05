#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

//! Aggregate real-process coverage for cross-cutting Unicode and optional RPC
//! input semantics. Each test uses an isolated environment and the real `pi`
//! executable rather than calling parser/runtime helpers directly.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
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
        let root = std::env::temp_dir().join(format!(
            "pi-cross-cutting-input-{tag}-{}",
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
    fn start(sandbox: &Sandbox, no_session: bool, session: Option<&Path>) -> Self {
        let mut command = sandbox.command();
        command.args([
            "--mode",
            "rpc",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "--no-tools",
        ]);
        if no_session {
            command.arg("--no-session");
        }
        if let Some(session) = session {
            command.arg("--session").arg(session);
        }
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
        let stdin = self.stdin.as_mut().expect("live RPC stdin");
        writeln!(stdin, "{value}").expect("write RPC command");
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

#[test]
fn unicode_and_optional_inputs_survive_the_real_rpc_wire() {
    let sandbox = Sandbox::new("unicode");
    let unicode = "界 e\u{301} 👨‍👩‍👧‍👦 🇳🇿 مرحبا";
    let expected_assistant = format!("faux response to: {unicode} (context messages: 1)");

    let mut rpc = RpcProcess::start(&sandbox, false, None);
    rpc.send(serde_json::json!({
        "id": "unicode-turn",
        "type": "prompt",
        "message": unicode,
    }));
    let records = rpc.read_until_settled();
    assert!(records.iter().all(is_valid_json_value));
    assert!(records
        .iter()
        .any(|record| value_contains_exact(record, unicode)));
    assert!(records
        .iter()
        .any(|record| value_contains_exact(record, &expected_assistant)));
    rpc.finish();

    let files = sandbox.session_files();
    assert_eq!(files.len(), 1, "expected one durable RPC session");
    let durable_records = fs::read_to_string(&files[0])
        .expect("read durable session")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("valid session JSONL"))
        .collect::<Vec<_>>();
    assert!(durable_records
        .iter()
        .any(|record| value_contains_exact(record, unicode)));
    assert!(durable_records
        .iter()
        .any(|record| value_contains_exact(record, &expected_assistant)));

    let mut reopened = RpcProcess::start(&sandbox, false, Some(&files[0]));
    reopened.send(serde_json::json!({"id":"reopen","type":"get_messages"}));
    let response = reopened
        .read_until_response("reopen")
        .pop()
        .expect("reopen response");
    assert_eq!(response["success"], true);
    assert!(value_contains_exact(&response, unicode));
    assert!(value_contains_exact(&response, &expected_assistant));
    reopened.finish();
    assert_eq!(sandbox.session_files(), files, "reopen created a session");
}

#[test]
fn optional_and_empty_fields_keep_the_upstream_rpc_contract() {
    let sandbox = Sandbox::new("optional");
    let mut rpc = RpcProcess::start(&sandbox, true, None);

    rpc.send(serde_json::json!({"type":"get_state"}));
    let omitted_id = rpc.read_next_response();
    assert_eq!(omitted_id["success"], true);
    assert!(omitted_id.get("id").is_none());

    rpc.send(serde_json::json!({"id":null,"type":"get_state"}));
    let null_id = rpc.read_next_response();
    assert_eq!(null_id["success"], true);
    assert!(null_id.get("id").is_none());

    rpc.send(serde_json::json!({"id":"","type":"get_state"}));
    let empty_id = rpc.read_next_response();
    assert_eq!(empty_id["success"], true);
    assert_eq!(empty_id["id"], "");

    for (id, command) in [
        (
            "missing-message",
            serde_json::json!({"id":"missing-message","type":"prompt"}),
        ),
        (
            "null-message",
            serde_json::json!({"id":"null-message","type":"prompt","message":null}),
        ),
    ] {
        rpc.send(command);
        let response = rpc
            .read_until_response(id)
            .pop()
            .expect("invalid prompt response");
        assert_eq!(response["success"], false);
        assert_eq!(response["error"], "missing message");
    }

    rpc.send(serde_json::json!({"id":"empty-message","type":"prompt","message":""}));
    let empty_turn = rpc.read_until_settled();
    assert!(empty_turn.iter().any(|record| {
        record["type"] == "response" && record["id"] == "empty-message" && record["success"] == true
    }));
    assert!(empty_turn.iter().any(|record| {
        record["type"] == "message_end"
            && record["message"]["role"] == "user"
            && record["message"]["content"][0]["text"] == ""
    }));
    assert!(empty_turn.iter().any(|record| {
        record["type"] == "message_end" && record["message"]["role"] == "assistant"
    }));

    rpc.send(serde_json::json!({"id":"after-errors","type":"get_state"}));
    let after = rpc
        .read_until_response("after-errors")
        .pop()
        .expect("post-error response");
    assert_eq!(after["success"], true);
    rpc.finish();
    assert!(
        sandbox.session_files().is_empty(),
        "--no-session wrote a file"
    );
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

fn value_contains_exact(value: &serde_json::Value, expected: &str) -> bool {
    match value {
        serde_json::Value::String(value) => value == expected,
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| value_contains_exact(value, expected)),
        serde_json::Value::Object(values) => values
            .values()
            .any(|value| value_contains_exact(value, expected)),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            false
        }
    }
}

fn is_valid_json_value(value: &serde_json::Value) -> bool {
    serde_json::from_str::<serde_json::Value>(&value.to_string()).is_ok()
}
