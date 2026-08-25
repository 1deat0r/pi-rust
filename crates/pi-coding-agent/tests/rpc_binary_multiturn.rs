//! Binary-level coverage for the streaming RPC protocol and durable sessions.
//!
//! Set `PI_RUST_TEST_BINARY` to exercise an already-built `pi` binary;
//! otherwise Cargo supplies the binary built for this integration test.

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const READ_TIMEOUT: Duration = Duration::from_secs(10);
const FIRST_PROMPT: &str = "first RPC prompt";
const SECOND_PROMPT: &str = "second RPC prompt";

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    agent_dir: PathBuf,
    sessions: PathBuf,
    cwd: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("pi-rpc-binary-multiturn-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let agent_dir = root.join("agent");
        let sessions = root.join("sessions");
        let cwd = root.join("project");
        for directory in [&home, &agent_dir, &sessions, &cwd] {
            fs::create_dir_all(directory).expect("create isolated test directory");
        }
        Self {
            root,
            home,
            agent_dir,
            sessions,
            cwd,
        }
    }

    fn spawn_rpc(&self) -> RpcProcess {
        let binary = test_binary();
        assert!(
            binary.is_file(),
            "pi test binary does not exist: {}",
            binary.display()
        );
        let mut child = Command::new(&binary)
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("PI_CODING_AGENT_DIR", &self.agent_dir)
            .env("PI_CODING_AGENT_SESSION_DIR", &self.sessions)
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env_remove("PI_PROVIDER")
            .env_remove("PI_MODEL")
            .env_remove("PI_KEY")
            .env_remove("PI_SESSION_ID")
            .env_remove("PI_SESSION_FILE")
            .args([
                "--mode",
                "rpc",
                "--provider",
                "faux",
                "--model",
                "faux-1",
                "--no-tools",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("spawn pi RPC binary {}: {error}", binary.display()));

        let stdout = child.stdout.take().expect("capture pi stdout");
        let stderr = child.stderr.take().expect("capture pi stderr");
        let stdout_lines = spawn_stdout_reader(stdout);
        let stderr_text = Arc::new(Mutex::new(String::new()));
        let stderr_capture = Arc::clone(&stderr_text);
        thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut text = String::new();
            let _ = reader.read_to_string(&mut text);
            *stderr_capture.lock().expect("stderr capture lock") = text;
        });

        let stdin = child_stdin(&mut child);
        RpcProcess {
            child,
            stdin,
            stdout_lines,
            stderr_text,
        }
    }

    fn session_file(&self) -> PathBuf {
        let files = jsonl_files(&self.sessions);
        assert_eq!(
            files.len(),
            1,
            "expected exactly one durable session JSONL file, found {files:?}"
        );
        files.into_iter().next().expect("session file")
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
    stdout_lines: mpsc::Receiver<io::Result<Option<String>>>,
    stderr_text: Arc<Mutex<String>>,
}

impl RpcProcess {
    fn send(&mut self, command: serde_json::Value) {
        writeln!(self.stdin, "{command}").expect("write RPC command");
        self.stdin.flush().expect("flush RPC command");
    }

    fn read_record_until(&self, deadline: Instant) -> serde_json::Value {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for RPC stdout");
        let line = match self.stdout_lines.recv_timeout(remaining) {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => panic!(
                "pi closed RPC stdout before the expected record; stderr: {}",
                self.stderr()
            ),
            Ok(Err(error)) => panic!(
                "reading pi RPC stdout failed: {error}; stderr: {}",
                self.stderr()
            ),
            Err(mpsc::RecvTimeoutError::Timeout) => panic!(
                "timed out after {READ_TIMEOUT:?} waiting for RPC stdout; stderr: {}",
                self.stderr()
            ),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("RPC stdout reader disconnected; stderr: {}", self.stderr())
            }
        };
        serde_json::from_str(line.trim()).unwrap_or_else(|error| {
            panic!(
                "invalid RPC JSONL record ({error}): {line:?}; stderr: {}",
                self.stderr()
            )
        })
    }

    fn read_until_settled(&self) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + READ_TIMEOUT;
        let mut records = Vec::new();
        loop {
            let record = self.read_record_until(deadline);
            let settled = record["type"] == "agent_settled";
            records.push(record);
            if settled {
                return records;
            }
        }
    }

    fn stderr(&self) -> String {
        self.stderr_text
            .lock()
            .map(|text| text.clone())
            .unwrap_or_else(|_| "<stderr capture unavailable>".to_string())
    }
}

impl Drop for RpcProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn child_stdin(child: &mut Child) -> ChildStdin {
    child.stdin.take().expect("capture pi stdin")
}

fn spawn_stdout_reader(stdout: ChildStdout) -> mpsc::Receiver<io::Result<Option<String>>> {
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

fn test_binary() -> PathBuf {
    std::env::var_os("PI_RUST_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pi")))
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

fn persisted_messages(path: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read durable session {}: {error}", path.display()))
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap_or_else(|error| {
                panic!(
                    "invalid session JSONL in {} ({error}): {line}",
                    path.display()
                )
            })
        })
        .filter(|entry| entry["kind"] == "entry" && entry["type"] == "message")
        .collect()
}

fn message_text(message: &serde_json::Value) -> String {
    message["content"]
        .as_array()
        .unwrap_or_else(|| panic!("message has no content array: {message}"))
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect()
}

fn streamed_text(records: &[serde_json::Value]) -> String {
    records
        .iter()
        .filter(|record| record["type"] == "message_update")
        .filter(|record| record["assistantMessageEvent"]["type"] == "text_delta")
        .filter_map(|record| record["assistantMessageEvent"]["delta"].as_str())
        .collect()
}

fn assert_prompt_records(records: &[serde_json::Value], id: &str, prompt: &str) {
    assert_eq!(
        records.first().map(|record| &record["type"]),
        Some(&serde_json::json!("response"))
    );
    let response = records
        .iter()
        .find(|record| record["type"] == "response" && record["id"] == id)
        .unwrap_or_else(|| panic!("missing prompt response {id}: {records:?}"));
    assert_eq!(response["command"], "prompt");
    assert_eq!(response["success"], true);

    let expected_prefix = format!("faux response to: {prompt}");
    let stream_text = streamed_text(records);
    assert!(
        stream_text.contains(&expected_prefix),
        "streamed text did not contain {expected_prefix:?}: {stream_text:?}"
    );
    let message_end = records
        .iter()
        .find(|record| record["type"] == "message_end" && record["message"]["role"] == "assistant")
        .unwrap_or_else(|| panic!("missing assistant message_end: {records:?}"));
    let assistant_text = message_text(&message_end["message"]);
    assert!(
        assistant_text.starts_with(&expected_prefix),
        "unexpected assistant response text: {assistant_text:?}"
    );
    assert_eq!(
        records.last().map(|record| &record["type"]),
        Some(&serde_json::json!("agent_settled"))
    );
}

fn assert_session_messages(path: &Path, expected_users: &[&str], expected_assistants: &[&str]) {
    let entries = persisted_messages(path);
    let users: Vec<_> = entries
        .iter()
        .filter(|entry| entry["message"]["role"] == "user")
        .map(|entry| message_text(&entry["message"]))
        .collect();
    assert_eq!(users, expected_users);

    let assistants: Vec<_> = entries
        .iter()
        .filter(|entry| entry["message"]["role"] == "assistant")
        .map(|entry| message_text(&entry["message"]))
        .collect();
    assert_eq!(assistants.len(), expected_assistants.len());
    for (assistant, expected) in assistants.iter().zip(expected_assistants) {
        assert!(
            assistant.starts_with(expected),
            "assistant session text {assistant:?} did not start with {expected:?}"
        );
    }
}

#[test]
fn rpc_binary_streams_sequential_multiturn_prompts_and_persists_session() {
    let sandbox = Sandbox::new();
    let mut rpc = sandbox.spawn_rpc();

    rpc.send(serde_json::json!({
        "id": "state-1",
        "type": "get_state"
    }));
    let state = rpc.read_record_until(Instant::now() + READ_TIMEOUT);
    assert_eq!(state["type"], "response");
    assert_eq!(state["id"], "state-1");
    assert_eq!(state["command"], "get_state");
    assert_eq!(state["success"], true);
    assert_eq!(state["data"]["model"]["provider"], "faux");
    assert_eq!(state["data"]["model"]["id"], "faux-1");

    rpc.send(serde_json::json!({
        "id": "prompt-1",
        "type": "prompt",
        "message": FIRST_PROMPT
    }));
    let first_records = rpc.read_until_settled();
    assert_prompt_records(&first_records, "prompt-1", FIRST_PROMPT);
    let session = sandbox.session_file();
    assert_session_messages(
        &session,
        &[FIRST_PROMPT],
        &["faux response to: first RPC prompt"],
    );

    rpc.send(serde_json::json!({
        "id": "prompt-2",
        "type": "prompt",
        "message": SECOND_PROMPT
    }));
    let second_records = rpc.read_until_settled();
    assert_prompt_records(&second_records, "prompt-2", SECOND_PROMPT);
    assert_session_messages(
        &session,
        &[FIRST_PROMPT, SECOND_PROMPT],
        &[
            "faux response to: first RPC prompt",
            "faux response to: second RPC prompt",
        ],
    );
}
