#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

//! Aggregate real-process coverage for resource limits/backpressure and a
//! Linux clean-environment platform boundary.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const RPC_TIMEOUT: Duration = Duration::from_secs(15);

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
            "pi-cross-limits-platform-{tag}-{}",
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
    raw_stdout: Option<ChildStdout>,
    stdout: Option<Receiver<std::io::Result<Option<String>>>>,
    stderr: Receiver<String>,
}

impl RpcProcess {
    fn start_without_stdout_reader(sandbox: &Sandbox) -> Self {
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
        let raw_stdout = child.stdout.take().expect("RPC stdout");
        let stderr = spawn_text_reader(child.stderr.take().expect("RPC stderr"));
        Self {
            child: Some(child),
            stdin: Some(stdin),
            raw_stdout: Some(raw_stdout),
            stdout: None,
            stderr,
        }
    }

    fn start_stdout_reader(&mut self) {
        assert!(self.stdout.is_none(), "stdout reader already started");
        self.stdout = Some(spawn_line_reader(
            self.raw_stdout.take().expect("unclaimed RPC stdout"),
        ));
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
        let line = match self
            .stdout
            .as_ref()
            .expect("started stdout reader")
            .recv_timeout(remaining)
        {
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
        self.raw_stdout.take();
        if let Some(mut child) = self.child.take() {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

#[test]
fn slow_consumer_large_output_and_deep_json_remain_bounded_and_recoverable() {
    let sandbox = Sandbox::new("limits");
    let mut rpc = RpcProcess::start_without_stdout_reader(&sandbox);
    let command = "i=0; while [ \"$i\" -lt 6000 ]; do printf 'line-%04d-abcdefghijklmnopqrstuvwxyz\\n' \"$i\"; i=$((i+1)); done";
    rpc.send(serde_json::json!({"id":"large","type":"bash","command":command}));

    // Keep the OS pipe unread long enough for the writer to encounter real
    // backpressure, then attach the line reader and require full recovery.
    thread::sleep(Duration::from_millis(250));
    rpc.start_stdout_reader();
    let records = rpc.read_until_response("large");
    let response = records.last().expect("large bash response");
    assert_eq!(response["success"], true);
    assert_eq!(response["data"]["exitCode"], 0);
    assert_eq!(response["data"]["truncated"], true);
    let streamed_bytes = records
        .iter()
        .filter(|record| record["type"] == "bash_execution_update")
        .filter_map(|record| record["delta"].as_str())
        .map(str::len)
        .sum::<usize>();
    assert!(
        streamed_bytes > 200_000,
        "stream lost output: {streamed_bytes}"
    );
    assert!(
        response["data"]["output"]
            .as_str()
            .unwrap_or_default()
            .len()
            < streamed_bytes,
        "bounded display output was not truncated"
    );
    let full_output_path = PathBuf::from(
        response["data"]["fullOutputPath"]
            .as_str()
            .expect("full output path"),
    );
    let full_output = fs::read_to_string(&full_output_path).expect("read full output artifact");
    assert_eq!(full_output.len(), streamed_bytes);
    assert!(full_output.contains("line-5999-abcdefghijklmnopqrstuvwxyz"));

    let deep = format!(
        "{{\"id\":\"deep\",\"type\":\"get_state\",\"value\":{}0{}}}",
        "[".repeat(300),
        "]".repeat(300)
    );
    rpc.send_raw(&deep);
    let malformed = rpc.read_until(|record| {
        record["type"] == "response" && record["command"] == "parse" && record["success"] == false
    });
    assert!(malformed.last().unwrap()["error"]
        .as_str()
        .is_some_and(|error| !error.is_empty()));

    rpc.send(serde_json::json!({"id":"after-deep","type":"get_state"}));
    let recovered = rpc.read_until_response("after-deep");
    assert_eq!(recovered.last().unwrap()["success"], true);
    rpc.finish();
    fs::remove_file(full_output_path).ok();
}

#[cfg(unix)]
#[test]
fn no_display_browser_proxy_offline_non_tty_path_is_side_effect_free() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new("platform");
    let browser_marker = sandbox.root.join("browser-was-opened");
    let browser = sandbox.root.join("fake-browser");
    fs::write(
        &browser,
        format!("#!/bin/sh\n: > '{}'\n", browser_marker.display()),
    )
    .expect("write fake browser");
    fs::set_permissions(&browser, fs::Permissions::from_mode(0o700))
        .expect("make fake browser executable");

    let mut child = sandbox
        .command()
        .env("BROWSER", &browser)
        .env("PI_OAUTH_NO_BROWSER", "1")
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .args([
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "--no-tools",
            "--no-session",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn non-TTY process");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(b"platform stdin\n")
        .expect("write non-TTY stdin");
    let output = child.wait_with_output().expect("wait for non-TTY process");
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "faux response to: platform stdin\n");
    assert!(output.stderr.is_empty(), "stderr: {}", stderr(&output));
    assert!(!browser_marker.exists(), "browser opener was invoked");
    assert!(
        jsonl_files(&sandbox.sessions).is_empty(),
        "--no-session wrote a file"
    );
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
    files
}

fn test_binary() -> PathBuf {
    std::env::var_os("PI_RUST_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pi")))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
