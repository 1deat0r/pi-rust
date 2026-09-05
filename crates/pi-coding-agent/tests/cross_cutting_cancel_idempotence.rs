#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

//! Aggregate real-process coverage for RPC cancellation and repeated-command
//! settlement. The fixture intentionally uses standalone bash because it gives
//! a deterministic, locally cancellable operation without provider timing.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
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
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "pi-cross-cancel-idempotence-{}",
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
        let mut child = sandbox
            .command()
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
            .expect("spawn real RPC process");
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

    fn read_until_response(&self, id: &str) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + RPC_TIMEOUT;
        let mut records = Vec::new();
        loop {
            let record = self.read_record(deadline);
            let done = record["type"] == "response" && record["id"] == id;
            records.push(record);
            if done {
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
fn repeated_abort_settles_once_and_keeps_the_rpc_runtime_reusable() {
    let sandbox = Sandbox::new();
    let mut rpc = RpcProcess::start(&sandbox);
    rpc.send(serde_json::json!({
        "id": "slow-bash",
        "type": "bash",
        "command": "sleep 30"
    }));
    thread::sleep(Duration::from_millis(100));
    rpc.send(serde_json::json!({"id":"abort-one","type":"abort_bash"}));
    rpc.send(serde_json::json!({"id":"abort-two","type":"abort_bash"}));

    let deadline = Instant::now() + RPC_TIMEOUT;
    let mut records = Vec::new();
    while !["slow-bash", "abort-one", "abort-two"].iter().all(|id| {
        records
            .iter()
            .any(|record: &serde_json::Value| record["type"] == "response" && record["id"] == *id)
    }) {
        records.push(rpc.read_record(deadline));
    }
    for id in ["slow-bash", "abort-one", "abort-two"] {
        assert_eq!(
            records
                .iter()
                .filter(|record| record["type"] == "response" && record["id"] == id)
                .count(),
            1,
            "response {id} settled more than once: {records:#?}"
        );
    }
    let cancelled = records
        .iter()
        .find(|record| record["type"] == "response" && record["id"] == "slow-bash")
        .expect("cancelled bash response");
    assert_eq!(cancelled["success"], true);
    assert_eq!(cancelled["data"]["cancelled"], true);
    assert!(cancelled["data"]["exitCode"].is_null());

    rpc.send(serde_json::json!({
        "id":"after-cancel",
        "type":"bash",
        "command":"printf after-cancel"
    }));
    let after = rpc.read_until_response("after-cancel");
    let after_response = after.last().expect("post-cancel response");
    assert_eq!(after_response["success"], true);
    assert_eq!(after_response["data"]["output"], "after-cancel");
    assert_eq!(after_response["data"]["cancelled"], false);

    rpc.send(serde_json::json!({"id":"entries","type":"get_entries"}));
    let entries = rpc
        .read_until_response("entries")
        .pop()
        .expect("entries response");
    assert_eq!(entries["success"], true);
    assert_eq!(value_string_count(&entries, "sleep 30"), 1);
    assert_eq!(value_string_count(&entries, "printf after-cancel"), 1);
    rpc.finish();
}

fn value_string_count(value: &serde_json::Value, expected: &str) -> usize {
    match value {
        serde_json::Value::String(value) => usize::from(value == expected),
        serde_json::Value::Array(values) => values
            .iter()
            .map(|value| value_string_count(value, expected))
            .sum(),
        serde_json::Value::Object(values) => values
            .values()
            .map(|value| value_string_count(value, expected))
            .sum(),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => 0,
    }
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

fn test_binary() -> PathBuf {
    std::env::var_os("PI_RUST_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pi")))
}
