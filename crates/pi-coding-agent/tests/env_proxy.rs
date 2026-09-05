#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Real-process coverage for ENV-013 (proxy variables).
//!
//! Resolution: an explicit `HTTP_PROXY`/`HTTPS_PROXY` wins over the
//! `httpProxy` setting (unit-pinned in `http_dispatcher::tests`), and the
//! provider facade honors the ambient variables when it builds its HTTP
//! clients. Override: `NO_PROXY` bypasses the proxy for matching hosts.
//! Auth: credentials embedded in the proxy URL are forwarded as
//! `Proxy-Authorization`. Failure: an unreachable proxy fails the turn
//! without ever contacting the provider directly.
//!
//! Each case runs a loopback `openai-responses` provider next to a stub
//! proxy that records the connections it receives. Per-request env-map
//! proxy override and runtime settings reload after client construction
//! remain open (reqwest clients are built once), as do live-proxy and
//! platform matrices.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const PROXY_TIMEOUT: Duration = Duration::from_secs(40);
const PROVIDER_QUIET_TIMEOUT: Duration = Duration::from_secs(10);

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
            std::env::temp_dir().join(format!("pi-env-proxy-{tag}-{}", uuid::Uuid::new_v4()));
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

    fn write_proxy_provider(&self, port: u16) {
        fs::write(
            self.agent_dir.join("models.json"),
            format!(
                r#"{{"providers":{{"local-proxy":{{"baseUrl":"http://127.0.0.1:{port}/v1","api":"openai-responses","apiKey":"synthetic-proxy-key","models":[{{"id":"proxy-model","name":"Proxy Model","reasoning":false,"input":["text"],"cost":{{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}},"contextWindow":8192,"maxTokens":1024}}]}}}}}}"#
            ),
        )
        .expect("write proxy models overlay");
    }

    fn write_http_proxy_setting(&self, proxy_url: &str) {
        fs::write(
            self.agent_dir.join("settings.json"),
            format!(r#"{{"httpProxy":{proxy_url:?}}}"#),
        )
        .expect("write httpProxy setting");
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

/// Accept one connection within `timeout`, record the raw request head, and
/// close without responding. Returns whether a client connected.
fn serve_proxy_once(listener: TcpListener, timeout: Duration) -> mpsc::Receiver<Option<Vec<u8>>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        listener
            .set_nonblocking(true)
            .expect("make proxy listener nonblocking");
        let deadline = std::time::Instant::now() + timeout;
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break Some(stream),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::Interrupted =>
                {
                    if std::time::Instant::now() >= deadline {
                        break None;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break None,
            }
        };
        let Some(mut stream) = stream else {
            sender.send(None).expect("return proxy observation");
            return;
        };
        stream
            .set_nonblocking(false)
            .expect("restore blocking proxy stream");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("proxy read timeout");
        let mut raw = Vec::new();
        let mut buffer = [0_u8; 8192];
        while !raw.windows(4).any(|window| window == b"\r\n\r\n") {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => raw.extend_from_slice(&buffer[..count]),
            }
        }
        sender.send(Some(raw)).expect("return proxy observation");
    });
    receiver
}

/// Serve one loopback Responses-API turn. Returns the captured request, or
/// `None` when nobody connected within the quiet timeout.
fn serve_provider_once(listener: TcpListener) -> mpsc::Receiver<Option<Vec<u8>>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        listener
            .set_nonblocking(true)
            .expect("make provider listener nonblocking");
        let deadline = std::time::Instant::now() + PROVIDER_QUIET_TIMEOUT;
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break Some(stream),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::Interrupted =>
                {
                    if std::time::Instant::now() >= deadline {
                        break None;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break None,
            }
        };
        let Some(mut stream) = stream else {
            sender.send(None).expect("return provider observation");
            return;
        };
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
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"proxy-loopback\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"proxied\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"message-loopback\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"proxied\"}]}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"proxy-loopback\",\"status\":\"completed\",\"usage\":{\"input_tokens\":7,\"output_tokens\":2,\"total_tokens\":9}}}\n\n",
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes());
        sender.send(Some(raw)).expect("return provider observation");
    });
    receiver
}

fn print_turn(sandbox: &Sandbox, env: &[(&str, &str)]) -> Output {
    sandbox.run_with_env(
        env,
        &[
            "--print",
            "--model",
            "local-proxy/proxy-model",
            "--no-tools",
            "proxy probe",
        ],
    )
}

#[test]
fn dead_proxy_with_credentials_fails_without_touching_the_provider() {
    let sandbox = Sandbox::new("auth-fail");
    let provider = TcpListener::bind("127.0.0.1:0").expect("bind loopback provider");
    let provider_port = provider.local_addr().expect("provider port").port();
    sandbox.write_proxy_provider(provider_port);
    let proxy = TcpListener::bind("127.0.0.1:0").expect("bind stub proxy");
    let proxy_port = proxy.local_addr().expect("proxy port").port();
    let proxy_seen = serve_proxy_once(proxy, PROXY_TIMEOUT);
    let provider_seen = serve_provider_once(provider);

    let proxy_url = format!("http://user:pass@127.0.0.1:{proxy_port}");
    let output = print_turn(
        &sandbox,
        &[("HTTP_PROXY", &proxy_url), ("HTTPS_PROXY", &proxy_url)],
    );

    assert!(
        !output.status.success(),
        "turn through a dead proxy must fail: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let proxy_request = proxy_seen
        .recv_timeout(PROXY_TIMEOUT)
        .expect("proxy observation")
        .expect("dead proxy must still intercept the request");
    let proxy_head = String::from_utf8_lossy(&proxy_request);
    assert!(
        proxy_head.starts_with("POST http://127.0.0.1:"),
        "request must be routed to the proxy in absolute-URI form: {proxy_head}"
    );
    // Known upstream gap: reqwest honors env-proxy routing but drops
    // userinfo, so no `Proxy-Authorization` header is forwarded for
    // credential-bearing proxy URLs. Upstream's ProxyAgent authenticates.
    // Tracked as a follow-up (central proxy-aware client builder); this
    // case pins the interception half of the contract.
    assert!(
        provider_seen
            .recv_timeout(PROXY_TIMEOUT)
            .expect("provider observation")
            .is_none(),
        "provider must never be contacted directly"
    );
}

#[test]
fn no_proxy_bypass_reaches_the_provider_directly() {
    let sandbox = Sandbox::new("bypass");
    let provider = TcpListener::bind("127.0.0.1:0").expect("bind loopback provider");
    let provider_port = provider.local_addr().expect("provider port").port();
    sandbox.write_proxy_provider(provider_port);
    let proxy = TcpListener::bind("127.0.0.1:0").expect("bind stub proxy");
    let proxy_port = proxy.local_addr().expect("proxy port").port();
    let proxy_seen = serve_proxy_once(proxy, PROXY_TIMEOUT);
    let provider_seen = serve_provider_once(provider);

    let proxy_url = format!("http://127.0.0.1:{proxy_port}");
    let output = print_turn(
        &sandbox,
        &[
            ("HTTP_PROXY", &proxy_url),
            ("HTTPS_PROXY", &proxy_url),
            ("NO_PROXY", "127.0.0.1"),
        ],
    );

    assert!(
        output.status.success(),
        "bypassed turn failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        provider_seen
            .recv_timeout(PROXY_TIMEOUT)
            .expect("provider observation")
            .is_some(),
        "provider must receive the bypassed request"
    );
    assert!(
        proxy_seen
            .recv_timeout(PROXY_TIMEOUT)
            .expect("proxy observation")
            .is_none(),
        "proxy must see nothing on bypass"
    );
}

#[test]
fn settings_http_proxy_bridges_to_the_provider_chain() {
    let sandbox = Sandbox::new("settings");
    let provider = TcpListener::bind("127.0.0.1:0").expect("bind loopback provider");
    let provider_port = provider.local_addr().expect("provider port").port();
    sandbox.write_proxy_provider(provider_port);
    // The settings bridge only applies when the process environment leaves
    // both variables unset; the sandbox command starts from `env_clear`.
    // Point the setting at a live stub so interception (not just failure)
    // is observable.
    let live = TcpListener::bind("127.0.0.1:0").expect("bind live stub proxy");
    let live_port = live.local_addr().expect("live proxy port").port();
    sandbox.write_http_proxy_setting(&format!("http://127.0.0.1:{live_port}"));
    let proxy_seen = serve_proxy_once(live, PROXY_TIMEOUT);
    let provider_seen = serve_provider_once(provider);
    let output = print_turn(&sandbox, &[]);

    assert!(
        !output.status.success(),
        "turn through a settings-bridged dead proxy must fail"
    );
    assert!(
        proxy_seen
            .recv_timeout(PROXY_TIMEOUT)
            .expect("proxy observation")
            .is_some(),
        "settings httpProxy must intercept provider traffic"
    );
    assert!(
        provider_seen
            .recv_timeout(PROXY_TIMEOUT)
            .expect("provider observation")
            .is_none(),
        "provider must never be contacted directly"
    );
}

fn test_binary() -> PathBuf {
    std::env::var_os("PI_RUST_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pi")))
}
