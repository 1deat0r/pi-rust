#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Real-process coverage for ENV-011 (`PI_CACHE_RETENTION`).
//!
//! The provider resolvers already implement the pinned upstream rule
//! (explicit option wins, `PI_CACHE_RETENTION=long` maps to long, everything
//! else falls back to short with no warning). This fixture proves the
//! environment leg end to end through a loopback `openai-responses`
//! provider: `long` reaches the wire as `"prompt_cache_retention":"24h"`,
//! while an unset or invalid value sends no retention field (short).
//! Print and JSON modes share the ambient resolution path, so both are
//! exercised. Live-vendor breadth and platform evidence remain open.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

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
            std::env::temp_dir().join(format!("pi-env-cache-{tag}-{}", uuid::Uuid::new_v4()));
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

    fn write_cache_provider(&self, port: u16) {
        fs::write(
            self.agent_dir.join("models.json"),
            format!(
                r#"{{"providers":{{"local-cache":{{"baseUrl":"http://127.0.0.1:{port}/v1","api":"openai-responses","apiKey":"synthetic-cache-key","models":[{{"id":"cache-model","name":"Cache Model","reasoning":false,"input":["text"],"cost":{{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}},"contextWindow":8192,"maxTokens":1024}}]}}}}}}"#
            ),
        )
        .expect("write cache models overlay");
    }

    fn command(&self) -> Command {
        let mut command = Command::new(test_binary());
        command
            .current_dir(&self.project)
            .env_clear()
            .env(
                "PATH",
                std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
            )
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

    fn run_with_env(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        let mut command = self.command();
        for (key, value) in env {
            command.env(key, value);
        }
        command.args(args);
        command.output().expect("spawn real pi process")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Serve exactly one loopback Responses-API turn, capture the raw request
/// bytes, and reply with a minimal completed text turn.
fn serve_one_cache_turn(listener: TcpListener) -> mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept loopback turn");
        stream
            .set_read_timeout(Some(REQUEST_TIMEOUT))
            .expect("read timeout");
        let raw = read_request(&mut stream);
        let body = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"cache-loopback\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"cached\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"message-loopback\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"cached\"}]}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"cache-loopback\",\"status\":\"completed\",\"usage\":{\"input_tokens\":7,\"output_tokens\":2,\"total_tokens\":9}}}\n\n",
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(response.as_bytes())
            .expect("write loopback turn");
        sender.send(raw).expect("return captured request");
    });
    receiver
}

fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 8192];
    let (header_end, content_length) = loop {
        let count = stream.read(&mut buffer).expect("read loopback request");
        assert!(count > 0, "client closed before request headers");
        raw.extend_from_slice(&buffer[..count]);
        let Some(end) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let header_end = end + 4;
        let headers = String::from_utf8_lossy(&raw[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("content length"))
            })
            .unwrap_or(0);
        break (header_end, content_length);
    };
    while raw.len() < header_end + content_length {
        let count = stream.read(&mut buffer).expect("read request body");
        assert!(count > 0, "client closed before request body");
        raw.extend_from_slice(&buffer[..count]);
    }
    raw
}

fn cache_turn(env: &[(&str, &str)], extra_args: &[&str]) -> (Output, String) {
    let sandbox = Sandbox::new("retention");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let port = listener.local_addr().expect("loopback port").port();
    sandbox.write_cache_provider(port);
    let captured = serve_one_cache_turn(listener);
    let mut args = vec![
        "--print",
        "--model",
        "local-cache/cache-model",
        "--no-tools",
        "cache probe",
    ];
    args.extend_from_slice(extra_args);
    let output = sandbox.run_with_env(env, &args);
    let raw = captured
        .recv_timeout(REQUEST_TIMEOUT)
        .expect("loopback request captured");
    (output, String::from_utf8_lossy(&raw).into_owned())
}

#[test]
fn pi_cache_retention_long_reaches_the_provider_request() {
    let (output, request) = cache_turn(&[("PI_CACHE_RETENTION", "long")], &[]);
    assert!(
        output.status.success(),
        "long turn failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        request.contains("\"prompt_cache_retention\":\"24h\""),
        "provider request lost the long retention: {}",
        request_body(&request)
    );
}

#[test]
fn unset_cache_retention_sends_no_retention_field() {
    let (output, request) = cache_turn(&[], &[]);
    assert!(
        output.status.success(),
        "default turn failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !request.contains("prompt_cache_retention"),
        "default short retention must not send a retention field: {}",
        request_body(&request)
    );
}

#[test]
fn invalid_cache_retention_falls_back_to_short() {
    let (output, request) = cache_turn(&[("PI_CACHE_RETENTION", "ultra")], &[]);
    assert!(
        output.status.success(),
        "invalid-level turn failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !request.contains("prompt_cache_retention"),
        "invalid retention must fall back to short: {}",
        request_body(&request)
    );
}

#[test]
fn pi_cache_retention_long_reaches_json_mode_request() {
    let (output, request) = cache_turn(&[("PI_CACHE_RETENTION", "long")], &["--mode", "json"]);
    assert!(
        output.status.success(),
        "JSON long turn failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        request.contains("\"prompt_cache_retention\":\"24h\""),
        "JSON provider request lost the long retention: {}",
        request_body(&request)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("agent_settled"),
        "JSON mode did not settle its turn: {stdout}"
    );
}

fn request_body(raw: &str) -> &str {
    raw.split("\r\n\r\n").nth(1).unwrap_or(raw)
}

fn test_binary() -> PathBuf {
    std::env::var_os("PI_RUST_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pi")))
}
