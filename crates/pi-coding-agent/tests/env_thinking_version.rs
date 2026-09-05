#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Real-process coverage for ENV-007 (`PI_REASONING_LEVEL`) and ENV-009
//! (`PI_SKIP_VERSION_CHECK` / `PI_VERSION`).
//!
//! ENV-007: the environment selects the thinking level exactly like
//! `--thinking` (CLI beats env beats settings default), an invalid value
//! warns and falls through, and the selected level reaches the provider
//! request through a loopback `openai-responses` reasoning provider.
//! Interactive footer/request-selection and live-vendor evidence remain open.
//!
//! ENV-009: startup performs no release-service request and shows no update
//! banner with or without `PI_SKIP_VERSION_CHECK`, `--version` reports the
//! binary version, and a standalone `PI_VERSION` override is ignored (pinned
//! upstream has no such variable). Installer provenance remains open.

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
            std::env::temp_dir().join(format!("pi-env-thinking-{tag}-{}", uuid::Uuid::new_v4()));
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

    fn write_think_provider(&self, port: u16) {
        fs::write(
            self.agent_dir.join("models.json"),
            format!(
                r#"{{"providers":{{"local-think":{{"baseUrl":"http://127.0.0.1:{port}/v1","api":"openai-responses","apiKey":"synthetic-think-key","models":[{{"id":"think-model","name":"Think Model","reasoning":true,"input":["text"],"cost":{{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}},"contextWindow":8192,"maxTokens":1024}}]}}}}}}"#
            ),
        )
        .expect("write think models overlay");
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
fn serve_one_think_turn(listener: TcpListener) -> mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept loopback turn");
        stream
            .set_read_timeout(Some(REQUEST_TIMEOUT))
            .expect("read timeout");
        let raw = read_request(&mut stream);
        let body = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"think-loopback\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"thoughtful\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"message-loopback\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"thoughtful\"}]}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"think-loopback\",\"status\":\"completed\",\"usage\":{\"input_tokens\":7,\"output_tokens\":2,\"total_tokens\":9}}}\n\n",
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

fn think_turn_effort(env: &[(&str, &str)], extra_args: &[&str]) -> (Output, String) {
    let sandbox = Sandbox::new("effort");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let port = listener.local_addr().expect("loopback port").port();
    sandbox.write_think_provider(port);
    let captured = serve_one_think_turn(listener);
    let mut args = vec![
        "--print",
        "--model",
        "local-think/think-model",
        "--no-tools",
        "thinking probe",
    ];
    args.extend_from_slice(extra_args);
    let output = sandbox.run_with_env(env, &args);
    let raw = captured
        .recv_timeout(REQUEST_TIMEOUT)
        .expect("loopback request captured");
    (output, String::from_utf8_lossy(&raw).into_owned())
}

#[test]
fn pi_reasoning_level_reaches_the_provider_request() {
    let (output, request) = think_turn_effort(&[("PI_REASONING_LEVEL", "high")], &[]);
    assert!(
        output.status.success(),
        "high turn failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        request.contains("\"effort\":\"high\""),
        "provider request lost the env level: {}",
        request_body(&request)
    );

    let (output, request) = think_turn_effort(&[("PI_REASONING_LEVEL", "low")], &[]);
    assert!(
        output.status.success(),
        "low turn failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        request.contains("\"effort\":\"low\""),
        "provider request lost the env level: {}",
        request_body(&request)
    );
}

#[test]
fn cli_thinking_beats_pi_reasoning_level() {
    let (output, request) =
        think_turn_effort(&[("PI_REASONING_LEVEL", "low")], &["--thinking", "high"]);
    assert!(
        output.status.success(),
        "CLI override turn failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        request.contains("\"effort\":\"high\""),
        "CLI did not override the env level: {}",
        request_body(&request)
    );
}

#[test]
fn invalid_pi_reasoning_level_warns_and_falls_back_to_default() {
    let (output, request) = think_turn_effort(&[("PI_REASONING_LEVEL", "ultra")], &[]);
    assert!(
        output.status.success(),
        "invalid-level turn failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Invalid PI_REASONING_LEVEL \"ultra\""),
        "invalid level did not warn with the value: {stderr}"
    );
    // A clean sandbox has no settings default, so the builtin default
    // (`medium`) reaches the provider request.
    assert!(
        request.contains("\"effort\":\"medium\""),
        "invalid level did not fall back to default: {}",
        request_body(&request)
    );
}

#[test]
fn version_output_has_no_update_banner_with_or_without_skip_flag() {
    for env in [
        vec![("PI_SKIP_VERSION_CHECK", "1")],
        vec![("PI_SKIP_VERSION_CHECK", "")],
        vec![],
    ] {
        let sandbox = Sandbox::new("version");
        let output = sandbox.run_with_env(&env, &["--version"]);
        assert!(
            output.status.success(),
            "version failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stdout.contains("0.84.2"),
            "unexpected version output: {stdout}"
        );
        for stream in [&stdout, &stderr] {
            assert!(
                !stream.to_lowercase().contains("update available"),
                "update banner leaked: {stream}"
            );
        }
    }
}

#[test]
fn pi_version_override_is_ignored() {
    // Pinned upstream has no standalone PI_VERSION variable: setting it must
    // change nothing observable and fail nothing.
    let sandbox = Sandbox::new("pi-version");
    let output = sandbox.run_with_env(&[("PI_VERSION", "9.9.9")], &["--version"]);
    assert!(
        output.status.success(),
        "version with override failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("0.84.2") && !stdout.contains("9.9.9"),
        "PI_VERSION leaked into version output: {stdout}"
    );

    let turn = sandbox.run_with_env(
        &[
            ("PI_VERSION", "9.9.9"),
            ("PI_PROVIDER", "faux"),
            ("PI_MODEL", "faux-1"),
        ],
        &["--no-tools", "version override probe"],
    );
    assert!(
        turn.status.success(),
        "turn with PI_VERSION failed: {}",
        String::from_utf8_lossy(&turn.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&turn.stdout),
        "faux response to: version override probe\n"
    );
}

#[test]
fn print_turns_show_no_update_banner() {
    let sandbox = Sandbox::new("no-banner");
    let output = sandbox.run_with_env(
        &[("PI_PROVIDER", "faux"), ("PI_MODEL", "faux-1")],
        &["--no-tools", "banner probe"],
    );
    assert!(
        output.status.success(),
        "banner probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    for stream in [
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    ] {
        assert!(
            !stream.to_lowercase().contains("update available"),
            "update banner leaked: {stream}"
        );
    }
}

fn request_body(raw: &str) -> &str {
    raw.split("\r\n\r\n").nth(1).unwrap_or(raw)
}

fn test_binary() -> PathBuf {
    std::env::var_os("PI_RUST_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pi")))
}
