#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Real process evidence for CLI-035 context suppression.
//!
//! The loopback peer is only a transport fixture: it captures the request
//! sent by the built `pi` process and returns an OpenAI Responses-shaped SSE
//! reply. No provider or Codex turn is mocked inside the coding-agent.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::mpsc;
use std::thread;

fn request_body(stream: &mut TcpStream) -> String {
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 8192];
    let (header_end, content_length) = loop {
        let count = stream.read(&mut buffer).expect("read loopback request");
        assert!(count > 0, "loopback client closed before request headers");
        raw.extend_from_slice(&buffer[..count]);
        let Some(header_end) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let header_end = header_end + 4;
        let headers = String::from_utf8_lossy(&raw[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("valid content length"))
            })
            .expect("content-length header");
        break (header_end, content_length);
    };

    while raw.len() < header_end + content_length {
        let count = stream
            .read(&mut buffer)
            .expect("read loopback request body");
        assert!(count > 0, "loopback client closed before request body");
        raw.extend_from_slice(&buffer[..count]);
    }
    String::from_utf8(raw[header_end..header_end + content_length].to_vec())
        .expect("request body is UTF-8 JSON")
}

fn response_body() -> &'static str {
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"loopback-response\"}}\n\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"loopback-message\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}\n\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"loopback reply\"}\n\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"loopback-message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"loopback reply\",\"annotations\":[]}]}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"loopback-response\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2,\"total_tokens\":3}}}\n\n"
}

fn system_prompt(request: &str) -> String {
    let request: serde_json::Value = serde_json::from_str(request).expect("valid request JSON");
    request["input"][0]["content"]
        .as_str()
        .expect("first input item contains the system prompt")
        .to_owned()
}

fn run_pi(
    root: &Path,
    home: &Path,
    agent_dir: &Path,
    sessions: &Path,
    no_context_files: bool,
) -> std::process::Output {
    let mut extra_args = vec!["--no-tools"];
    if no_context_files {
        extra_args.push("--no-context-files");
    }
    run_pi_with_args(root, home, agent_dir, sessions, &extra_args)
}

fn run_pi_with_args(
    root: &Path,
    home: &Path,
    agent_dir: &Path,
    sessions: &Path,
    extra_args: &[&str],
) -> std::process::Output {
    let mut args = vec![
        "--print",
        "--provider",
        "openai",
        "--model",
        "gpt-4o",
        "--api-key",
        "synthetic-loopback-key",
        "--no-session",
    ];
    args.extend_from_slice(extra_args);
    args.push("context probe");

    Command::new(env!("CARGO_BIN_EXE_pi"))
        .env_clear()
        .current_dir(root)
        .env(
            "PATH",
            std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
        )
        .env("HOME", home)
        .env("PI_CODING_AGENT_DIR", agent_dir)
        .env("PI_CODING_AGENT_SESSION_DIR", sessions)
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .args(args)
        .output()
        .expect("spawn pi loopback process")
}

#[test]
fn composed_prompt_reaches_the_provider_with_upstream_section_order() {
    let root = std::env::temp_dir().join(format!(
        "pi-agent008-system-prompt-{}",
        uuid::Uuid::new_v4()
    ));
    let home = root.join("home");
    let agent_dir = home.join(".pi").join("agent");
    let sessions = root.join("sessions");
    let skill_dir = agent_dir.join("skills").join("agent008");
    fs::create_dir_all(&skill_dir).expect("create skill directory");
    fs::create_dir_all(&sessions).expect("create session directory");
    fs::write(
        root.join("AGENTS.md"),
        "AGENT008_CONTEXT: provider-visible project context",
    )
    .expect("write context fixture");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: agent008\ndescription: AGENT008_SKILL provider-visible skill\n---\nbody\n",
    )
    .expect("write skill fixture");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback provider");
    let address = listener.local_addr().expect("loopback provider address");
    let (requests_tx, requests_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept provider request");
            requests.push(request_body(&mut stream));
            let body = response_body();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write provider response");
        }
        requests_tx.send(requests).expect("send captured requests");
    });

    fs::write(
        agent_dir.join("models.json"),
        format!(r#"{{"providers":{{"openai":{{"baseUrl":"http://{address}/v1"}}}}}}"#),
    )
    .expect("write loopback models config");

    let default = run_pi_with_args(&root, &home, &agent_dir, &sessions, &["--tools", "read"]);
    assert!(
        default.status.success(),
        "default stderr: {}",
        String::from_utf8_lossy(&default.stderr)
    );
    let custom = run_pi_with_args(
        &root,
        &home,
        &agent_dir,
        &sessions,
        &[
            "--tools",
            "read",
            "--system-prompt",
            "AGENT008_OVERRIDE: custom base",
            "--append-system-prompt",
            "AGENT008_APPEND_ONE",
            "--append-system-prompt",
            "AGENT008_APPEND_TWO",
        ],
    );
    assert!(
        custom.status.success(),
        "custom stderr: {}",
        String::from_utf8_lossy(&custom.stderr)
    );

    let requests = requests_rx.recv().expect("captured provider requests");
    server.join().expect("join loopback provider");
    assert_eq!(requests.len(), 2);

    let default_prompt = system_prompt(&requests[0]);
    assert!(default_prompt.starts_with("You are an expert coding assistant"));
    assert!(default_prompt.contains("- read: Read file contents"));
    assert!(!default_prompt.contains("- bash: Execute bash commands"));
    assert!(default_prompt.contains("AGENT008_CONTEXT"));
    assert!(default_prompt.contains("AGENT008_SKILL"));

    let custom_prompt = system_prompt(&requests[1]);
    let base = custom_prompt
        .find("AGENT008_OVERRIDE")
        .expect("custom base");
    let append_one = custom_prompt
        .find("AGENT008_APPEND_ONE")
        .expect("first append");
    let append_two = custom_prompt
        .find("AGENT008_APPEND_TWO")
        .expect("second append");
    let context = custom_prompt.find("AGENT008_CONTEXT").expect("context");
    let skill = custom_prompt.find("AGENT008_SKILL").expect("skill");
    let cwd = custom_prompt
        .find("Current working directory:")
        .expect("working directory");
    assert!(base < append_one);
    assert!(append_one < append_two);
    assert!(append_two < context);
    assert!(context < skill);
    assert!(skill < cwd);
    assert!(custom_prompt.ends_with(&format!("{}\n", root.to_string_lossy())));
    assert!(String::from_utf8_lossy(&default.stdout).contains("loopback reply"));
    assert!(String::from_utf8_lossy(&custom.stdout).contains("loopback reply"));

    fs::remove_dir_all(root).expect("remove loopback fixture");
}

#[test]
fn no_context_files_changes_the_provider_visible_prompt() {
    let root =
        std::env::temp_dir().join(format!("pi-cli-context-loopback-{}", uuid::Uuid::new_v4()));
    let home = root.join("home");
    let agent_dir = home.join(".pi").join("agent");
    let sessions = root.join("sessions");
    fs::create_dir_all(&agent_dir).expect("create agent dir");
    fs::create_dir_all(&sessions).expect("create session dir");
    fs::write(
        root.join("AGENTS.md"),
        "CLI035_CONTEXT_MARKER: project-only instruction",
    )
    .expect("write context fixture");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback provider");
    let address = listener.local_addr().expect("loopback provider address");
    let (requests_tx, requests_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept provider request");
            requests.push(request_body(&mut stream));
            let body = response_body();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write provider response");
        }
        requests_tx.send(requests).expect("send captured requests");
    });

    fs::write(
        agent_dir.join("models.json"),
        format!(r#"{{"providers":{{"openai":{{"baseUrl":"http://{address}/v1"}}}}}}"#),
    )
    .expect("write loopback models config");

    let normal = run_pi(&root, &home, &agent_dir, &sessions, false);
    assert!(
        normal.status.success(),
        "normal stderr: {}",
        String::from_utf8_lossy(&normal.stderr)
    );
    let suppressed = run_pi(&root, &home, &agent_dir, &sessions, true);
    assert!(
        suppressed.status.success(),
        "suppressed stderr: {}",
        String::from_utf8_lossy(&suppressed.stderr)
    );

    let requests = requests_rx.recv().expect("captured provider requests");
    server.join().expect("join loopback provider");
    assert_eq!(requests.len(), 2);
    let normal_prompt = system_prompt(&requests[0]);
    let suppressed_prompt = system_prompt(&requests[1]);
    assert!(normal_prompt.contains("CLI035_CONTEXT_MARKER"));
    assert!(!suppressed_prompt.contains("CLI035_CONTEXT_MARKER"));
    assert!(suppressed_prompt.contains("Current working directory"));
    assert!(String::from_utf8_lossy(&normal.stdout).contains("loopback reply"));
    assert!(String::from_utf8_lossy(&suppressed.stdout).contains("loopback reply"));

    fs::remove_dir_all(root).expect("remove loopback fixture");
}

#[test]
fn unicode_system_prompt_reaches_the_provider_and_empty_value_keeps_default() {
    let root = std::env::temp_dir().join(format!("pi-agent009-unicode-{}", uuid::Uuid::new_v4()));
    let home = root.join("home");
    let agent_dir = home.join(".pi").join("agent");
    let sessions = root.join("sessions");
    fs::create_dir_all(&agent_dir).expect("create agent dir");
    fs::create_dir_all(&sessions).expect("create session dir");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback provider");
    let address = listener.local_addr().expect("loopback provider address");
    let (requests_tx, requests_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept provider request");
            requests.push(request_body(&mut stream));
            let body = response_body();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write provider response");
        }
        requests_tx.send(requests).expect("send captured requests");
    });

    fs::write(
        agent_dir.join("models.json"),
        format!(r#"{{"providers":{{"openai":{{"baseUrl":"http://{address}/v1"}}}}}}"#),
    )
    .expect("write loopback models config");

    let unicode = run_pi_with_args(
        &root,
        &home,
        &agent_dir,
        &sessions,
        &["--system-prompt", "Ünïcødé 織号 🌍 prompt"],
    );
    assert!(
        unicode.status.success(),
        "unicode stderr: {}",
        String::from_utf8_lossy(&unicode.stderr)
    );
    let empty = run_pi_with_args(
        &root,
        &home,
        &agent_dir,
        &sessions,
        &["--system-prompt", ""],
    );
    assert!(
        empty.status.success(),
        "empty stderr: {}",
        String::from_utf8_lossy(&empty.stderr)
    );

    let requests = requests_rx.recv().expect("captured provider requests");
    server.join().expect("join loopback provider");
    assert_eq!(requests.len(), 2);

    let unicode_prompt = system_prompt(&requests[0]);
    assert!(
        unicode_prompt.starts_with("Ünïcødé 織号 🌍 prompt"),
        "unicode prompt lost on the wire: {unicode_prompt}"
    );

    // An explicitly empty CLI value retains the upstream default prompt
    // instead of shadowing it with an empty string.
    let empty_prompt = system_prompt(&requests[1]);
    assert!(
        empty_prompt.starts_with("You are an expert coding assistant"),
        "empty value must keep the default: {empty_prompt}"
    );

    fs::remove_dir_all(root).expect("remove loopback fixture");
}
