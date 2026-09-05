#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Real-process coverage for DIST-004 (upgrade/rollback).
//!
//! pi-rust cannot self-update a compiled installation (the `update` command
//! exits nonzero with a rebuild instruction, matching the upstream
//! unavailable-installation path), so "version replacement" means replacing
//! the binary file: sessions, settings, and auth all live outside the
//! binary. This harness builds a disposable install root, copies the test
//! binary in as v1, runs a loopback turn, replaces the binary file
//! (simulating an upgrade), runs a second turn, and proves the v1 session
//! file plus settings/auth bytes survive byte-identical. A failed
//! self-update (`pi update`) must likewise leave every state file untouched.
//!
//! Live installer provenance and platform breadth remain open.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const TURN_TIMEOUT: Duration = Duration::from_secs(60);

struct Sandbox {
    root: PathBuf,
    install_dir: PathBuf,
    home: PathBuf,
    agent_dir: PathBuf,
    sessions: PathBuf,
    project: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("pi-dist-upgrade-{tag}-{}", uuid::Uuid::new_v4()));
        let install_dir = root.join("install");
        let home = root.join("home");
        let agent_dir = root.join("agent");
        let sessions = root.join("sessions");
        let project = root.join("project");
        for path in [&install_dir, &home, &agent_dir, &sessions, &project] {
            fs::create_dir_all(path).expect("create isolated test directory");
        }
        let binary = install_dir.join("pi");
        fs::copy(test_binary(), &binary).expect("stage disposable install");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700))
            .expect("make staged binary executable");
        Self {
            root,
            install_dir,
            home,
            agent_dir,
            sessions,
            project,
        }
    }

    fn installed_binary(&self) -> PathBuf {
        self.install_dir.join("pi")
    }

    /// Simulate version replacement by rewriting the installed binary file.
    fn replace_installed_binary(&self) {
        fs::copy(test_binary(), self.installed_binary()).expect("replace installed binary");
        fs::set_permissions(self.installed_binary(), fs::Permissions::from_mode(0o700))
            .expect("make replaced binary executable");
    }

    fn write_state(&self, port: u16) {
        fs::write(
            self.agent_dir.join("models.json"),
            format!(
                r#"{{"providers":{{"local-up":{{"baseUrl":"http://127.0.0.1:{port}/v1","api":"openai-responses","apiKey":"synthetic-upgrade-key","models":[{{"id":"up-model","name":"Upgrade Model","reasoning":false,"input":["text"],"cost":{{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}},"contextWindow":8192,"maxTokens":1024}}]}}}}}}"#
            ),
        )
        .expect("write upgrade models overlay");
        fs::write(
            self.agent_dir.join("settings.json"),
            r#"{"showCacheMissNotices":true}"#,
        )
        .expect("write upgrade settings marker");
        fs::write(
            self.agent_dir.join("auth.json"),
            r#"{"local-up":{"type":"api_key","key":"seeded-upgrade-key"}}"#,
        )
        .expect("write upgrade auth seed");
    }

    fn command(&self) -> Command {
        let mut command = Command::new(self.installed_binary());
        command
            .current_dir(&self.project)
            .env_clear()
            .env(
                "PATH",
                std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
            )
            .env("HOME", &self.home)
            .env("PI_CODING_AGENT_DIR", &self.agent_dir)
            .env("PI_CODING_AGENT_SESSION_DIR", &self.sessions)
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env("LC_ALL", "C");
        command
    }

    fn run_print(&self) -> Output {
        self.command()
            .args([
                "--print",
                "--model",
                "local-up/up-model",
                "--no-tools",
                "upgrade probe",
            ])
            .output()
            .expect("spawn installed pi process")
    }

    fn run_update(&self) -> Output {
        self.command()
            .arg("update")
            .output()
            .expect("spawn installed pi update")
    }

    /// Snapshot every state file outside the binary: settings/auth bytes
    /// plus the full session tree.
    fn snapshot(&self) -> StateSnapshot {
        StateSnapshot {
            settings: fs::read(self.agent_dir.join("settings.json")).expect("read settings"),
            auth: fs::read(self.agent_dir.join("auth.json")).expect("read auth"),
            sessions: read_tree(&self.sessions),
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct StateSnapshot {
    settings: Vec<u8>,
    auth: Vec<u8>,
    sessions: BTreeMap<String, Vec<u8>>,
}

fn read_tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).expect("list state dir");
        for entry in entries {
            let entry = entry.expect("read state entry");
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let name = path
                    .strip_prefix(root)
                    .expect("relative state path")
                    .to_string_lossy()
                    .into_owned();
                files.insert(name, fs::read(&path).expect("read state file"));
            }
        }
    }
    files
}

/// Serve loopback Responses-API turns until the harness is done, counting
/// how many provider requests arrived.
fn serve_provider(listener: TcpListener, done: mpsc::Receiver<()>) -> mpsc::Receiver<usize> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        listener
            .set_nonblocking(true)
            .expect("make provider listener nonblocking");
        let mut served = 0_usize;
        while matches!(done.try_recv(), Err(mpsc::TryRecvError::Empty)) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_nonblocking(false)
                        .expect("restore blocking provider stream");
                    stream
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .expect("provider read timeout");
                    let mut raw = Vec::new();
                    let mut buffer = [0_u8; 8192];
                    while !raw.windows(4).any(|window| window == b"\r\n\r\n") {
                        match stream.read(&mut buffer) {
                            Ok(0) | Err(_) => break,
                            Ok(count) => raw.extend_from_slice(&buffer[..count]),
                        }
                    }
                    let body = concat!(
                        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"upgrade-loopback\"}}\n\n",
                        "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"upgraded\"}\n\n",
                        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"message-loopback\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"upgraded\"}]}}\n\n",
                        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"upgrade-loopback\",\"status\":\"completed\",\"usage\":{\"input_tokens\":7,\"output_tokens\":2,\"total_tokens\":9}}}\n\n",
                    );
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes());
                    served += 1;
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::Interrupted =>
                {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break,
            }
        }
        sender.send(served).expect("return served count");
    });
    receiver
}

#[test]
fn version_replacement_preserves_sessions_settings_and_auth() {
    let sandbox = Sandbox::new("replace");
    let provider = TcpListener::bind("127.0.0.1:0").expect("bind loopback provider");
    let port = provider.local_addr().expect("provider port").port();
    sandbox.write_state(port);
    let (done_sender, done_receiver) = mpsc::channel();
    let served = serve_provider(provider, done_receiver);

    let first = sandbox.run_print();
    assert!(
        first.status.success(),
        "v1 turn failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let before = sandbox.snapshot();
    assert!(
        !before.sessions.is_empty(),
        "v1 turn must persist a session file"
    );

    sandbox.replace_installed_binary();
    let second = sandbox.run_print();
    assert!(
        second.status.success(),
        "post-upgrade turn failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let after = sandbox.snapshot();

    assert_eq!(after.settings, before.settings, "settings must survive");
    assert_eq!(after.auth, before.auth, "auth must survive");
    for (name, bytes) in &before.sessions {
        assert_eq!(
            after.sessions.get(name),
            Some(bytes),
            "v1 session file {name} must survive byte-identical"
        );
    }
    drop(done_sender);
    let count = served.recv_timeout(TURN_TIMEOUT).expect("served count");
    assert!(
        count >= 2,
        "both turns must reach the provider (each turn makes a preflight plus the turn request): {count}"
    );
}

#[test]
fn failed_self_update_leaves_state_untouched() {
    let sandbox = Sandbox::new("failed-update");
    let provider = TcpListener::bind("127.0.0.1:0").expect("bind loopback provider");
    let port = provider.local_addr().expect("provider port").port();
    sandbox.write_state(port);
    let (done_sender, done_receiver) = mpsc::channel();
    let served = serve_provider(provider, done_receiver);

    let first = sandbox.run_print();
    assert!(
        first.status.success(),
        "pre-update turn failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let before = sandbox.snapshot();

    let update = sandbox.run_update();
    assert!(
        !update.status.success(),
        "self-update of a compiled install must fail"
    );
    let stderr = String::from_utf8_lossy(&update.stderr);
    assert!(
        stderr.contains("cannot self-update"),
        "failure must name the unavailable path: {stderr}"
    );
    assert_eq!(
        sandbox.snapshot(),
        before,
        "failed update must not touch state"
    );
    drop(done_sender);
    let count = served.recv_timeout(TURN_TIMEOUT).expect("served count");
    assert!(
        count >= 1,
        "the pre-update turn must reach the provider: {count}"
    );
}

fn test_binary() -> PathBuf {
    std::env::var_os("PI_RUST_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pi")))
}
