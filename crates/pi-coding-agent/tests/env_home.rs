#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Real-process coverage for ENV-014 (`HOME` / `USERPROFILE` / XDG roots).
//!
//! Platform path resolution: with no agent-dir override, the provider
//! catalog resolves under `$HOME/.pi/agent`. Missing-home fallback: with
//! `HOME` (and `USERPROFILE`) unset, an explicit `PI_CODING_AGENT_DIR`
//! still carries the whole turn. XDG cache roots for the HuggingFace token
//! search are pinned deterministically in `core::llama::tests`.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::thread;
use std::time::Duration;

struct Sandbox {
    root: PathBuf,
    home: Option<PathBuf>,
    agent_dir: Option<PathBuf>,
    sessions: PathBuf,
    project: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("pi-env-home-{tag}-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let agent_dir = root.join("agent");
        let sessions = root.join("sessions");
        let project = root.join("project");
        for path in [&home, &agent_dir, &sessions, &project] {
            fs::create_dir_all(path).expect("create isolated test directory");
        }
        Self {
            root,
            home: Some(home),
            agent_dir: Some(agent_dir),
            sessions,
            project,
        }
    }

    fn write_home_provider(&self, port: u16) {
        let agent_dir = self.home.as_ref().unwrap().join(".pi/agent");
        fs::create_dir_all(&agent_dir).expect("create home agent dir");
        fs::write(
            agent_dir.join("models.json"),
            format!(
                r#"{{"providers":{{"local-home":{{"baseUrl":"http://127.0.0.1:{port}/v1","api":"openai-responses","apiKey":"synthetic-home-key","models":[{{"id":"home-model","name":"Home Model","reasoning":false,"input":["text"],"cost":{{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}},"contextWindow":8192,"maxTokens":1024}}]}}}}}}"#
            ),
        )
        .expect("write home models overlay");
    }

    fn write_agent_provider(&self, port: u16) {
        let agent_dir = self.agent_dir.as_ref().unwrap();
        fs::write(
            agent_dir.join("models.json"),
            format!(
                r#"{{"providers":{{"local-home":{{"baseUrl":"http://127.0.0.1:{port}/v1","api":"openai-responses","apiKey":"synthetic-home-key","models":[{{"id":"home-model","name":"Home Model","reasoning":false,"input":["text"],"cost":{{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}},"contextWindow":8192,"maxTokens":1024}}]}}}}}}"#
            ),
        )
        .expect("write agent models overlay");
    }

    fn command(&self) -> Command {
        let mut command = Command::new(test_binary());
        command.current_dir(&self.project).env_clear().env(
            "PATH",
            std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
        );
        match &self.home {
            Some(home) => {
                command.env("HOME", home);
            }
            None => {
                command.env_remove("HOME");
                command.env_remove("USERPROFILE");
            }
        }
        if let Some(agent_dir) = &self.agent_dir {
            command.env("PI_CODING_AGENT_DIR", agent_dir);
        }
        command
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
                "local-home/home-model",
                "--no-tools",
                "home probe",
            ])
            .output()
            .expect("spawn real pi process")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn serve_provider_once(listener: TcpListener) -> thread::JoinHandle<bool> {
    thread::spawn(move || {
        listener
            .set_nonblocking(true)
            .expect("make provider listener nonblocking");
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
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
            return false;
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
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"home-loopback\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"homed\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"message-loopback\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"homed\"}]}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"home-loopback\",\"status\":\"completed\",\"usage\":{\"input_tokens\":7,\"output_tokens\":2,\"total_tokens\":9}}}\n\n",
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes());
        true
    })
}

#[test]
fn catalog_resolves_under_home_without_agent_dir_override() {
    let mut sandbox = Sandbox::new("home-derived");
    sandbox.agent_dir = None;
    let provider = TcpListener::bind("127.0.0.1:0").expect("bind loopback provider");
    let port = provider.local_addr().expect("provider port").port();
    sandbox.write_home_provider(port);
    let served = serve_provider_once(provider);

    let output = sandbox.run_print();
    assert!(
        output.status.success(),
        "home-derived turn failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(served.join().expect("provider thread"), "provider hit");
}

#[test]
fn missing_home_falls_back_to_explicit_agent_dir() {
    let mut sandbox = Sandbox::new("no-home");
    sandbox.home = None;
    let provider = TcpListener::bind("127.0.0.1:0").expect("bind loopback provider");
    let port = provider.local_addr().expect("provider port").port();
    sandbox.write_agent_provider(port);
    let served = serve_provider_once(provider);

    let output = sandbox.run_print();
    assert!(
        output.status.success(),
        "homeless turn failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(served.join().expect("provider thread"), "provider hit");
}

fn test_binary() -> PathBuf {
    std::env::var_os("PI_RUST_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pi")))
}
