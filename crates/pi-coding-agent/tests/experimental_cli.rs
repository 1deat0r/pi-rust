#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

use pi_client::PiClient;
use pi_protocol::{Command as ProtocolCommand, ServerEvent, SessionPhase};

fn pi() -> Command {
    Command::new(env!("CARGO_BIN_EXE_pi"))
}

fn unique_socket() -> PathBuf {
    std::env::temp_dir().join(format!(
        "pi-experimental-cli-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn unix_address(path: &Path) -> String {
    format!("unix://{}", path.display())
}

#[test]
fn server_and_client_are_disabled_without_the_experimental_gate() {
    let output = pi()
        .arg("server")
        .env_remove("PI_EXPERIMENTAL")
        .output()
        .expect("run pi");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("require PI_EXPERIMENTAL=1"), "{stderr}");
}

#[test]
fn experimental_commands_validate_addresses_and_auth_without_leaking_tokens() {
    let output = pi()
        .args([
            "server",
            "--listen",
            "ws://localhost",
            "--auth-token",
            "secret",
        ])
        .env("PI_EXPERIMENTAL", "1")
        .output()
        .expect("run pi");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Unsupported --listen transport"),
        "{stderr}"
    );
    assert!(!stderr.contains("secret"), "{stderr}");

    let output = pi()
        .args(["client", "--help"])
        .env("PI_EXPERIMENTAL", "1")
        .output()
        .expect("run pi");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Usage: pi client"));
}

#[test]
fn server_starts_real_socket_and_client_completes_handshake_list_and_close() {
    let socket = unique_socket();
    let address = unix_address(&socket);
    let session_root = std::env::temp_dir().join(format!(
        "pi-experimental-empty-sessions-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&session_root).unwrap();
    let mut server = pi()
        .args(["server", "--listen", &address])
        .env("PI_EXPERIMENTAL", "1")
        .env("PI_OFFLINE", "1")
        .env("PI_CODING_AGENT_SESSION_DIR", &session_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start experimental server");

    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(
            Instant::now() < deadline,
            "server did not create {}\nstatus={:?}",
            socket.display(),
            server.try_wait().expect("poll server")
        );
        thread::sleep(Duration::from_millis(20));
    }

    let client = pi()
        .args(["client", "--connect", &address])
        .env("PI_EXPERIMENTAL", "1")
        .env("PI_OFFLINE", "1")
        .output()
        .expect("run experimental client");
    assert!(client.status.success(), "{client:?}");
    let stdout = String::from_utf8_lossy(&client.stdout);
    assert!(
        stdout.contains("Connected to experimental server"),
        "{stdout}"
    );
    assert!(stdout.contains("0 sessions"), "{stdout}");

    // SIGINT exercises the server's actual graceful close path, including
    // PiServer listener cleanup. The signal utility is available on the Unix
    // platforms where UnixListener is supported.
    let signal = Command::new("kill")
        .args(["-INT", &server.id().to_string()])
        .status()
        .expect("send SIGINT");
    assert!(signal.success());
    let status = server.wait().expect("wait for server");
    assert!(status.success(), "{status}");
    assert!(!socket.exists(), "server socket was not cleaned up");
    let _ = std::fs::remove_dir_all(&session_root);
}

#[cfg(unix)]
#[test]
fn server_sigterm_and_sighup_close_listener_and_remove_socket() {
    for signal_name in ["TERM", "HUP"] {
        let socket = unique_socket();
        let address = unix_address(&socket);
        let session_root = std::env::temp_dir().join(format!(
            "pi-experimental-signal-{}-{}-{}",
            signal_name.to_ascii_lowercase(),
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&session_root).unwrap();
        let mut server = pi()
            .args(["server", "--listen", &address])
            .env("PI_EXPERIMENTAL", "1")
            .env("PI_OFFLINE", "1")
            .env("PI_CODING_AGENT_SESSION_DIR", &session_root)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start signal-test experimental server");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            assert!(Instant::now() < deadline, "server did not create socket");
            thread::sleep(Duration::from_millis(20));
        }

        let signal = Command::new("kill")
            .args([format!("-{signal_name}"), server.id().to_string()])
            .status()
            .expect("send experimental server signal");
        assert!(signal.success());
        let status = server.wait().expect("wait for experimental server");
        assert!(status.success(), "{signal_name} status: {status}");
        assert!(!socket.exists(), "{signal_name} left server socket behind");
        let _ = std::fs::remove_dir_all(session_root);
    }
}

#[test]
fn client_reports_connection_failures() {
    let socket = unique_socket();
    let output = pi()
        .args(["client", "--connect", &unix_address(&socket)])
        .env("PI_EXPERIMENTAL", "1")
        .output()
        .expect("run pi");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("connect experimental server"));
}

#[tokio::test]
async fn experimental_server_executes_and_persists_provider_turn() {
    let socket = unique_socket();
    let session_root = std::env::temp_dir().join(format!(
        "pi-experimental-sessions-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&session_root).unwrap();
    let address = unix_address(&socket);
    let mut server = pi()
        .args(["server", "--listen", &address])
        .env("PI_EXPERIMENTAL", "1")
        .env("PI_PROVIDER", "faux")
        .env("PI_CODING_AGENT_SESSION_DIR", &session_root)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start provider-backed experimental server");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(Instant::now() < deadline, "server did not create socket");
        thread::sleep(Duration::from_millis(20));
    }

    let client = PiClient::connect_with_timeouts(
        socket.to_str().unwrap(),
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    let progress_seen = Arc::new(AtomicBool::new(false));
    let progress_seen_listener = progress_seen.clone();
    client.subscribe(Arc::new(move |event| {
        if matches!(event, ServerEvent::SessionProgress { .. }) {
            progress_seen_listener.store(true, Ordering::SeqCst);
        }
    }));
    let created = client
        .request(ProtocolCommand::Create {
            cwd: None,
            name: Some("provider-turn".into()),
            model: None,
            thinking_level: None,
        })
        .await
        .unwrap();
    let session_id = match created {
        pi_protocol::CommandResult::Create { session } => session.id,
        other => panic!("unexpected create response: {other:?}"),
    };
    let prompt = client.request(ProtocolCommand::Prompt {
        session_id: session_id.clone(),
        text: "real provider-backed experimental prompt".into(),
    });
    let prompted = prompt.await.unwrap();
    let snapshot = match prompted {
        pi_protocol::CommandResult::Prompt { session } => session,
        other => panic!("unexpected prompt response: {other:?}"),
    };
    assert_eq!(snapshot.phase, SessionPhase::Idle);
    assert!(
        snapshot.transcript.iter().any(|item| match item {
            pi_protocol::TranscriptItem::User(item) => item.role == "assistant",
            pi_protocol::TranscriptItem::Assistant(_) => true,
            pi_protocol::TranscriptItem::Tool(_) => false,
        }),
        "snapshot after provider turn: {snapshot:?}"
    );
    assert!(
        progress_seen.load(Ordering::SeqCst),
        "provider deltas were not published"
    );

    client.close().await.unwrap();
    client.dispose().await.unwrap();
    let _ = Command::new("kill")
        .args(["-INT", &server.id().to_string()])
        .status()
        .unwrap();
    assert!(server.wait().unwrap().success());
    let durable = std::fs::read_dir(&session_root)
        .unwrap()
        .flat_map(|entry| std::fs::read_dir(entry.unwrap().path()).unwrap())
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect::<Vec<_>>();
    assert!(durable
        .iter()
        .any(|contents| contents.contains("\"role\":\"assistant\"")));
    let _ = std::fs::remove_dir_all(&session_root);
}

#[test]
fn authenticated_server_requires_preface_and_supports_authenticated_client() {
    let socket = unique_socket();
    let address = unix_address(&socket);
    let session_root = std::env::temp_dir().join(format!(
        "pi-experimental-auth-sessions-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&session_root).unwrap();
    let mut server = pi()
        .args(["server", "--listen", &address, "--auth-token", "test-token"])
        .env("PI_EXPERIMENTAL", "1")
        .env("PI_PROVIDER", "faux")
        .env("PI_CODING_AGENT_SESSION_DIR", &session_root)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start authenticated experimental server");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(
            Instant::now() < deadline,
            "authenticated server did not create socket"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let rejected = pi()
        .args(["client", "--connect", &address])
        .env("PI_EXPERIMENTAL", "1")
        .output()
        .expect("run unauthenticated client");
    assert!(!rejected.status.success());
    let accepted = pi()
        .args([
            "client",
            "--connect",
            &address,
            "--auth-token",
            "test-token",
        ])
        .env("PI_EXPERIMENTAL", "1")
        .output()
        .expect("run authenticated client");
    assert!(accepted.status.success(), "{accepted:?}");
    let _ = Command::new("kill")
        .args(["-INT", &server.id().to_string()])
        .status()
        .unwrap();
    assert!(server.wait().unwrap().success());
    assert!(!socket.exists());
    let _ = std::fs::remove_dir_all(&session_root);
}
