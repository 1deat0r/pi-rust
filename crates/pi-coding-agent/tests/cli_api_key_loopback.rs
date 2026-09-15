#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Real-process coverage for CLI-008 request-scoped `--api-key`:
//! the CLI key reaches the provider as a Bearer header, is never persisted,
//! an empty CLI value falls back to `PI_KEY`, and with neither the run fails
//! closed instead of going direct.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output};
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
        let root = std::env::temp_dir().join(format!("pi-api-key-{tag}-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let agent_dir = home.join(".pi").join("agent");
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

    fn write_loopback_provider(&self, port: u16) {
        fs::write(
            self.agent_dir.join("models.json"),
            format!(r#"{{"providers":{{"openai":{{"baseUrl":"http://127.0.0.1:{port}/v1"}}}}}}"#),
        )
        .expect("write loopback models overlay");
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

    fn run(&self, args: &[&str]) -> Output {
        self.command()
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

fn read_request(stream: &mut TcpStream) -> String {
    let mut buffer = [0u8; 65536];
    let mut raw = Vec::new();
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                raw.extend_from_slice(&buffer[..count]);
                if raw.windows(4).any(|window| window == b"\r\n\r\n") {
                    // Headers complete; give the body a short window.
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(150)));
                    if let Ok(count) = stream.read(&mut buffer) {
                        raw.extend_from_slice(&buffer[..count]);
                    }
                    break;
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&raw).into_owned()
}

/// Accept repeated loopback connections (catalog probes, then the real
/// turn), reply to each with a minimal Responses-API SSE stream, and send
/// every captured raw request through the channel.
fn serve_turns(listener: TcpListener) -> std::sync::mpsc::Receiver<String> {
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for _ in 0..8 {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
            let raw = read_request(&mut stream);
            let body = concat!(
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"key-loopback\"}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"loopback reply\"}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"message-loopback\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"loopback reply\"}]}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"key-loopback\",\"status\":\"completed\",\"usage\":{\"input_tokens\":7,\"output_tokens\":2,\"total_tokens\":9}}}\n\n",
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            if sender.send(raw).is_err() {
                break;
            }
        }
    });
    receiver
}

fn wait_for_bearer_request(
    captured: &std::sync::mpsc::Receiver<String>,
    expected_key: &str,
) -> String {
    let deadline = std::time::Instant::now() + REQUEST_TIMEOUT;
    let needle = format!("authorization: bearer {expected_key}");
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "authenticated request never captured for {expected_key}"
        );
        let request = captured
            .recv_timeout(remaining)
            .expect("loopback request captured");
        if request.to_lowercase().contains(&needle) {
            return request;
        }
        // Catalog/other probes are answered but not asserted.
    }
}

#[test]
fn api_key_reaches_the_provider_as_bearer_and_is_never_persisted() {
    let sandbox = Sandbox::new("bearer");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback provider");
    let port = listener.local_addr().expect("loopback port").port();
    sandbox.write_loopback_provider(port);
    let captured = serve_turns(listener);

    let out = sandbox.run(&[
        "--print",
        "--provider",
        "openai",
        "--model",
        "gpt-5.4",
        "--api-key",
        "sk-cli-secret-1",
        "hello",
    ]);
    assert!(
        out.status.success(),
        "api-key turn failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("loopback reply"),
        "no reply"
    );
    wait_for_bearer_request(&captured, "sk-cli-secret-1");

    // The request-scoped key must never be persisted.
    let auth_path = sandbox.agent_dir.join("auth.json");
    assert!(
        !auth_path.exists(),
        "api key must not be persisted to auth storage"
    );
}

#[test]
fn empty_api_key_falls_back_to_pi_key_and_neither_fails_closed() {
    let sandbox = Sandbox::new("empty-fallback");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback provider");
    let port = listener.local_addr().expect("loopback port").port();
    sandbox.write_loopback_provider(port);
    let captured = serve_turns(listener);

    // Empty CLI value does not override a usable PI_KEY (upstream truthiness).
    let out = sandbox
        .command()
        .env("PI_KEY", "sk-env-secret-2")
        .args([
            "--print",
            "--provider",
            "openai",
            "--model",
            "gpt-5.4",
            "--api-key",
            "",
            "hello",
        ])
        .output()
        .expect("spawn fallback child");
    assert!(
        out.status.success(),
        "fallback turn failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    wait_for_bearer_request(&captured, "sk-env-secret-2");

    // Neither source usable: the run fails closed instead of going direct.
    let bare = Sandbox::new("neither");
    let dead_listener = TcpListener::bind("127.0.0.1:0").expect("bind dead port");
    let dead_port = dead_listener.local_addr().expect("dead port").port();
    bare.write_loopback_provider(dead_port);
    drop(dead_listener); // nothing is listening: any direct attempt errors
    let failed = bare.run(&[
        "--print",
        "--provider",
        "openai",
        "--model",
        "gpt-5.4",
        "hello",
    ]);
    assert!(
        !failed.status.success(),
        "unauthenticated run must fail: {}",
        String::from_utf8_lossy(&failed.stderr)
    );
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(
        stderr.contains("api key")
            || stderr.contains("API key")
            || stderr.contains("not configured")
            || stderr.contains("No models available"),
        "expected an auth-failure diagnostic, got: {stderr}"
    );
}
