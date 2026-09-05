#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

fn read_request(stream: &mut TcpStream) {
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
}

fn write_response(stream: &mut TcpStream, status: u16, body: &str, content_type: &str) {
    let reason = if status == 200 {
        "OK"
    } else {
        "Service Unavailable"
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .expect("write loopback response");
}

fn success_sse() -> &'static str {
    "data: {\"id\":\"retry-response\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"retry recovered\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"retry-response\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}\n\ndata: [DONE]\n\n"
}

struct Sandbox {
    root: std::path::PathBuf,
    home: std::path::PathBuf,
    agent_dir: std::path::PathBuf,
}

impl Sandbox {
    fn new(settings: serde_json::Value) -> Self {
        let root = std::env::temp_dir().join(format!("pi-retry-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let agent_dir = home.join(".pi").join("agent");
        fs::create_dir_all(&agent_dir).expect("create agent dir");
        fs::write(
            agent_dir.join("settings.json"),
            serde_json::to_vec_pretty(&settings).expect("settings JSON"),
        )
        .expect("write settings");
        Self {
            root,
            home,
            agent_dir,
        }
    }

    fn configure_provider(&self, address: std::net::SocketAddr) {
        fs::write(
            self.agent_dir.join("models.json"),
            format!(
                r#"{{"providers":{{"openai":{{"baseUrl":"http://{address}/v1","api":"openai-completions","models":[{{"id":"retry-model","name":"Retry Model"}}]}}}}}}"#
            ),
        )
        .expect("write models config");
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
        command
            .env_clear()
            .current_dir(&self.root)
            .env(
                "PATH",
                std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
            )
            .env("HOME", &self.home)
            .env("PI_CODING_AGENT_DIR", &self.agent_dir)
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .args([
                "--print",
                "--provider",
                "openai",
                "--model",
                "retry-model",
                "--api-key",
                "synthetic-retry-key",
                "--no-session",
                "--no-tools",
                "retry probe",
            ]);
        command
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn retry_settings(enabled: bool, max_retries: u64, base_delay_ms: u64) -> serde_json::Value {
    serde_json::json!({
        "defaultProjectTrust": "always",
        "retry": {
            "enabled": enabled,
            "maxRetries": max_retries,
            "baseDelayMs": base_delay_ms,
            "provider": {"maxRetries": 0, "maxRetryDelayMs": 1000}
        }
    })
}

fn serve(
    responses: Vec<(u16, &'static str, &'static str)>,
) -> (std::net::SocketAddr, thread::JoinHandle<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind retry peer");
    let address = listener.local_addr().expect("retry peer address");
    let server = thread::spawn(move || {
        let mut count = 0;
        for (status, body, content_type) in responses {
            let (mut stream, _) = listener.accept().expect("accept provider request");
            read_request(&mut stream);
            count += 1;
            write_response(&mut stream, status, body, content_type);
        }
        count
    });
    (address, server)
}

#[test]
fn enabled_agent_retry_recovers_but_disabled_policy_stops_after_one_request() {
    let error = r#"{"error":{"message":"service overloaded"}}"#;
    let (address, server) = serve(vec![
        (503, error, "application/json"),
        (200, success_sse(), "text/event-stream"),
    ]);
    let sandbox = Sandbox::new(retry_settings(true, 1, 1));
    sandbox.configure_provider(address);
    let output = sandbox.command().output().expect("run retry-enabled pi");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("retry recovered"));
    assert_eq!(server.join().expect("join retry server"), 2);

    let (address, server) = serve(vec![(503, error, "application/json")]);
    let sandbox = Sandbox::new(retry_settings(false, 4, 1));
    sandbox.configure_provider(address);
    let output = sandbox.command().output().expect("run retry-disabled pi");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("retry recovered"));
    assert_eq!(server.join().expect("join disabled server"), 1);
}

#[test]
fn non_retryable_quota_body_is_terminal_even_when_retry_is_enabled() {
    let (address, server) = serve(vec![(
        429,
        r#"{"error":{"message":"insufficient_quota for plan"}}"#,
        "application/json",
    )]);
    let sandbox = Sandbox::new(retry_settings(true, 3, 1));
    sandbox.configure_provider(address);
    let output = sandbox.command().output().expect("run quota pi");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("retry recovered"));
    assert_eq!(server.join().expect("join quota server"), 1);
}

#[test]
fn provider_max_retries_is_applied_independently_of_agent_retry_policy() {
    let error = r#"{"error":{"message":"service overloaded"}}"#;
    let (address, server) = serve(vec![
        (503, error, "application/json"),
        (200, success_sse(), "text/event-stream"),
    ]);
    let mut settings = retry_settings(false, 0, 1);
    settings["retry"]["provider"]["maxRetries"] = serde_json::json!(1);
    let sandbox = Sandbox::new(settings);
    sandbox.configure_provider(address);
    let output = sandbox.command().output().expect("run provider-retry pi");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("retry recovered"));
    assert_eq!(server.join().expect("join provider-retry server"), 2);
}

#[test]
fn sigterm_aborts_agent_retry_backoff_before_a_second_request() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind abort peer");
    listener
        .set_nonblocking(true)
        .expect("make abort peer nonblocking");
    let address = listener.local_addr().expect("abort peer address");
    let (first_tx, first_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut count = 0;
        while std::time::Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    read_request(&mut stream);
                    count += 1;
                    write_response(
                        &mut stream,
                        503,
                        r#"{"error":{"message":"service overloaded"}}"#,
                        "application/json",
                    );
                    let _ = first_tx.send(());
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept abort request: {error}"),
            }
        }
        count
    });
    let sandbox = Sandbox::new(retry_settings(true, 3, 10_000));
    sandbox.configure_provider(address);
    let mut child = sandbox
        .command()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn retry-backoff pi");
    first_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first provider request");
    let kill = Command::new("/bin/kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("signal retry-backoff pi");
    assert!(kill.success());
    let status = child.wait().expect("wait retry-backoff pi");
    assert_eq!(status.code(), Some(143));
    assert_eq!(server.join().expect("join abort server"), 1);
}
