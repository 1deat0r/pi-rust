#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Binary-level tests for `--mode json` (JSON event stream over stdout).

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    agent_dir: PathBuf,
    sessions: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("pi-json-mode-{tag}-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let agent_dir = home.join(".pi").join("agent");
        let sessions = root.join("sessions");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::create_dir_all(&sessions).unwrap();
        Self {
            root,
            home,
            agent_dir,
            sessions,
        }
    }

    fn pi(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_pi"))
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("PI_CODING_AGENT_DIR", &self.agent_dir)
            .env("PI_CODING_AGENT_SESSION_DIR", &self.sessions)
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env_remove("PI_PROVIDER")
            .env_remove("PI_MODEL")
            .env_remove("PI_KEY")
            .env_remove("PI_SESSION_ID")
            .args(args)
            .output()
            .expect("spawn pi")
    }

    fn pi_with_stdin(&self, cwd: &Path, args: &[&str], input: &str) -> std::process::Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_pi"))
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("PI_CODING_AGENT_DIR", &self.agent_dir)
            .env("PI_CODING_AGENT_SESSION_DIR", &self.sessions)
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env_remove("PI_PROVIDER")
            .env_remove("PI_MODEL")
            .env_remove("PI_KEY")
            .env_remove("PI_SESSION_ID")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn pi with stdin");
        child
            .stdin
            .take()
            .expect("pi stdin")
            .write_all(input.as_bytes())
            .expect("write pi stdin");
        child.wait_with_output().expect("wait for pi")
    }

    fn rpc(&self, cwd: &Path) -> Child {
        Command::new(env!("CARGO_BIN_EXE_pi"))
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("PI_CODING_AGENT_DIR", &self.agent_dir)
            .env("PI_CODING_AGENT_SESSION_DIR", &self.sessions)
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env_remove("PI_PROVIDER")
            .env_remove("PI_MODEL")
            .env_remove("PI_KEY")
            .args(["--mode", "rpc", "--provider", "faux", "--model", "faux-1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn pi RPC mode")
    }

    fn stdout(&self, output: &std::process::Output) -> String {
        String::from_utf8_lossy(&output.stdout).to_string()
    }
    fn stderr(&self, output: &std::process::Output) -> String {
        String::from_utf8_lossy(&output.stderr).to_string()
    }

    fn session_files(&self) -> Vec<PathBuf> {
        jsonl_files(&self.sessions)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
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

fn rpc_line(reader: &mut BufReader<ChildStdout>) -> serde_json::Value {
    let mut line = String::new();
    assert!(
        reader.read_line(&mut line).expect("read RPC stdout") > 0,
        "RPC process closed stdout before completing the command"
    );
    serde_json::from_str(line.trim()).expect("valid JSONL RPC record")
}

fn send_rpc(stdin: &mut ChildStdin, command: serde_json::Value) {
    writeln!(stdin, "{}", command).expect("write RPC command");
    stdin.flush().expect("flush RPC command");
}

fn read_until_rpc_response(
    reader: &mut BufReader<ChildStdout>,
    id: &str,
) -> Vec<serde_json::Value> {
    let mut records = Vec::new();
    loop {
        let record = rpc_line(reader);
        let done =
            record["type"] == "response" && record["id"] == id && record["success"].is_boolean();
        records.push(record);
        if done {
            return records;
        }
    }
}

fn read_until_settled(reader: &mut BufReader<ChildStdout>) -> Vec<serde_json::Value> {
    let mut records = Vec::new();
    loop {
        let record = rpc_line(reader);
        let settled = record["type"] == "agent_settled";
        records.push(record);
        if settled {
            return records;
        }
    }
}

fn persisted_message_entries(path: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(path)
        .expect("session JSONL")
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|entry| entry["type"] == "message")
        .collect()
}

fn message_text(entry: &serde_json::Value) -> Option<String> {
    entry["message"]["content"]
        .as_array()?
        .iter()
        .find_map(|block| block["text"].as_str().map(str::to_owned))
}

fn events(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSON event line"))
        .collect()
}

#[test]
fn json_mode_emits_event_lines() {
    let sandbox = Sandbox::new("events");
    let out = sandbox.pi(
        &sandbox.root,
        &[
            "--mode",
            "json",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "hello",
        ],
    );
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    let stdout = sandbox.stdout(&out);
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(!lines.is_empty(), "expected JSON event lines");
    let header: serde_json::Value = serde_json::from_str(lines[0]).expect("valid session header");
    assert_eq!(header["type"], "session");
    assert_eq!(header["version"], 3);
    assert!(header["timestamp"]
        .as_str()
        .is_some_and(|value| value.ends_with('Z')));
    assert_eq!(header["cwd"].as_str(), sandbox.root.to_str());
    assert!(header["id"].as_str().is_some_and(|id| !id.is_empty()));
    // Every event line is valid JSON with a type field; the first line is the
    // durable session header emitted by the upstream JSON print lifecycle.
    for line in &lines {
        let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON line");
        if v["type"] != "session" {
            assert!(v.get("type").is_some(), "event must carry a type: {line}");
        }
    }
    // The stream includes message_update events and the final text.
    let has_update = lines.iter().any(|l| {
        serde_json::from_str::<serde_json::Value>(l)
            .map(|v| v["type"] == "message_update")
            .unwrap_or(false)
    });
    assert!(has_update, "expected message_update events: {stdout}");
    let all = stdout.clone();
    assert!(
        all.contains("faux response to: hello"),
        "expected faux reply: {all}"
    );

    let assistant = lines
        .iter()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| event["type"] == "message_end" && event["message"]["role"] == "assistant")
        .expect("assistant message_end");
    assert_eq!(assistant["message"]["provider"], "faux");
    assert_eq!(assistant["message"]["model"], "faux-1");
    let persisted = sandbox.session_files();
    assert_eq!(persisted.len(), 1, "JSON mode must persist one session");
    let durable_header: serde_json::Value = fs::read_to_string(&persisted[0])
        .expect("durable session JSONL")
        .lines()
        .next()
        .and_then(|line| serde_json::from_str(line).ok())
        .expect("durable session header");
    assert_eq!(durable_header["type"], "session");
    assert_eq!(durable_header["version"], 3);
    assert!(durable_header["timestamp"]
        .as_str()
        .is_some_and(|value| value.ends_with('Z')));
    let messages = persisted_message_entries(&persisted[0]);
    assert_eq!(
        messages.len(),
        2,
        "expected one user and one assistant entry"
    );
    assert_eq!(messages[0]["message"]["role"], "user");
    assert_eq!(messages[1]["message"]["role"], "assistant");
}

#[test]
fn json_mode_multi_argv_matches_sequential_print_mode_contract() {
    let sandbox = Sandbox::new("multi-argv-batch");
    let out = sandbox.pi(
        &sandbox.root,
        &[
            "--mode",
            "json",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "first",
            "second",
        ],
    );
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    let records = events(&sandbox.stdout(&out));

    let user_starts = records
        .iter()
        .filter(|record| record["type"] == "message_start" && record["message"]["role"] == "user")
        .collect::<Vec<_>>();
    assert_eq!(
        user_starts.len(),
        2,
        "both argv prompts must reach the stream"
    );
    assert_eq!(message_text(user_starts[0]).as_deref(), Some("first"));
    assert_eq!(message_text(user_starts[1]).as_deref(), Some("second"));

    let assistant_ends = records
        .iter()
        .filter(|record| {
            record["type"] == "message_end" && record["message"]["role"] == "assistant"
        })
        .collect::<Vec<_>>();
    let text_ends = records
        .iter()
        .filter(|record| {
            record["type"] == "message_update"
                && record["assistantMessageEvent"]["type"] == "text_end"
        })
        .collect::<Vec<_>>();
    assert_eq!(
        records
            .iter()
            .filter(|r| r["type"] == "agent_start")
            .count(),
        2
    );
    assert_eq!(
        records.iter().filter(|r| r["type"] == "turn_start").count(),
        2
    );
    assert_eq!(assistant_ends.len(), 2);
    assert_eq!(text_ends.len(), 2);
    assert_eq!(
        records.iter().filter(|r| r["type"] == "turn_end").count(),
        2
    );
    assert_eq!(
        records.iter().filter(|r| r["type"] == "agent_end").count(),
        2
    );
    let assistant_texts: Vec<_> = assistant_ends
        .iter()
        .filter_map(|record| message_text(record))
        .collect();
    assert_eq!(assistant_texts.len(), 2);
    assert!(assistant_texts[0].contains("faux response to: first"));
    assert!(assistant_texts[1].contains("faux response to: second"));

    let files = sandbox.session_files();
    assert_eq!(files.len(), 1);
    let persisted = persisted_message_entries(&files[0]);
    assert_eq!(
        persisted.len(),
        4,
        "two turns must persist user+assistant pairs"
    );
    let persisted_roles: Vec<_> = persisted
        .iter()
        .map(|entry| entry["message"]["role"].as_str().unwrap())
        .collect();
    assert_eq!(persisted_roles, ["user", "assistant", "user", "assistant"]);
    let persisted_users: Vec<_> = persisted
        .iter()
        .filter(|entry| entry["message"]["role"] == "user")
        .filter_map(message_text)
        .collect();
    assert_eq!(persisted_users, ["first", "second"]);
}

#[test]
fn json_mode_piped_stdin_matches_positional_initial_prompt_contract() {
    let positional = Sandbox::new("positional-input");
    let positional_out = positional.pi(
        &positional.root,
        &[
            "--mode",
            "json",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "positional prompt",
        ],
    );
    assert!(
        positional_out.status.success(),
        "stderr: {}",
        positional.stderr(&positional_out)
    );
    let positional_records = events(&positional.stdout(&positional_out));
    let positional_users = positional_records
        .iter()
        .filter(|record| record["type"] == "message_start" && record["message"]["role"] == "user")
        .collect::<Vec<_>>();
    assert_eq!(positional_users.len(), 1);
    assert_eq!(
        message_text(positional_users[0]).as_deref(),
        Some("positional prompt")
    );
    assert!(
        positional_records.iter().any(|record| {
            record["type"] == "message_update"
                && record["assistantMessageEvent"]["type"] == "text_end"
                && record["assistantMessageEvent"]["content"]
                    .as_str()
                    .is_some_and(|text| text.contains("positional prompt"))
        }),
        "positional input must drive the faux response"
    );

    let piped = Sandbox::new("stdin-input");
    let piped_out = piped.pi_with_stdin(
        &piped.root,
        &["--mode", "json", "--provider", "faux", "--model", "faux-1"],
        "stdin prompt\n",
    );
    assert!(
        piped_out.status.success(),
        "stderr: {}",
        piped.stderr(&piped_out)
    );
    let piped_records = events(&piped.stdout(&piped_out));
    let piped_users = piped_records
        .iter()
        .filter(|record| record["type"] == "message_start" && record["message"]["role"] == "user")
        .collect::<Vec<_>>();
    assert_eq!(piped_users.len(), 1);
    assert_eq!(
        message_text(piped_users[0]).as_deref(),
        Some("stdin prompt")
    );
    let piped_assistant = piped_records
        .iter()
        .find(|record| record["type"] == "message_end" && record["message"]["role"] == "assistant")
        .expect("stdin assistant message");
    assert_eq!(
        message_text(piped_assistant).as_deref(),
        Some("faux response to: stdin prompt")
    );
    assert_eq!(piped.session_files().len(), 1);
    assert_eq!(
        persisted_message_entries(&piped.session_files()[0]).len(),
        2
    );
}

#[test]
fn text_mode_prints_only_the_final_text_while_json_mode_wraps_the_same_reply() {
    let text = Sandbox::new("text-output");
    let text_out = text.pi(
        &text.root,
        &[
            "--print",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "same prompt",
        ],
    );
    assert!(
        text_out.status.success(),
        "stderr: {}",
        text.stderr(&text_out)
    );
    assert_eq!(text.stdout(&text_out), "faux response to: same prompt\n");

    let json = Sandbox::new("json-output");
    let json_out = json.pi(
        &json.root,
        &[
            "--mode",
            "json",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "same prompt",
        ],
    );
    assert!(
        json_out.status.success(),
        "stderr: {}",
        json.stderr(&json_out)
    );
    let records = events(&json.stdout(&json_out));
    assert_eq!(
        records
            .iter()
            .filter(|record| record["type"] == "message_end"
                && record["message"]["role"] == "assistant")
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record["type"] == "message_update"
                && record["assistantMessageEvent"]["type"] == "text_end")
            .count(),
        1
    );
    assert!(!json
        .stdout(&json_out)
        .contains("\nfaux response to: same prompt\n"));
    assert!(json
        .stdout(&json_out)
        .contains("faux response to: same prompt"));
}

#[test]
fn json_mode_streams_terminal_error_as_event_and_exits_zero() {
    let sandbox = Sandbox::new("error");
    // A provider with no key terminates the stream in an error. Upstream
    // `runPrintMode` in *json* mode delivers the error as a JSON event line
    // on stdout and exits 0 (only text mode turns Error/Aborted into a
    // nonzero exit).
    let out = sandbox.pi(
        &sandbox.root,
        &[
            "--mode",
            "json",
            "--provider",
            "openai",
            "--model",
            "gpt-5.4",
            "hi",
        ],
    );
    assert!(
        out.status.success(),
        "json mode must exit 0, stderr: {}",
        sandbox.stderr(&out)
    );
    let stdout = sandbox.stdout(&out);
    // Every line is valid JSON carrying a type.
    let mut seen_error = false;
    for line in stdout.lines() {
        let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON line");
        if v["type"] != "session" {
            assert!(v.get("type").is_some(), "event must carry a type: {line}");
        }
        if v["type"] == "message_update" && v["assistantMessageEvent"]["type"] == "error" {
            seen_error = true;
            assert_eq!(v["assistantMessageEvent"]["error"]["provider"], "openai");
            assert_eq!(v["assistantMessageEvent"]["error"]["model"], "gpt-5.4");
        }
    }
    // A terminal error still reaches the stream as a message_update event
    // (the falsey text is not required; the event envelope is the contract).
    assert!(
        !stdout.trim().is_empty(),
        "expected JSON event lines on stdout"
    );
    assert!(
        seen_error,
        "expected a terminal error event on the wire: {stdout}"
    );
    let persisted = sandbox.session_files();
    assert_eq!(persisted.len(), 1);
    let assistant = persisted_message_entries(&persisted[0])
        .into_iter()
        .find(|entry| entry["message"]["role"] == "assistant")
        .expect("persisted error assistant");
    assert_eq!(assistant["message"]["stopReason"], "error");
    assert_eq!(assistant["message"]["provider"], "openai");
    assert_eq!(assistant["message"]["model"], "gpt-5.4");
}

fn assert_rpc_prompt_turn(records: &[serde_json::Value], id: &str, prompt: &str) {
    let response = records
        .iter()
        .find(|record| record["type"] == "response" && record["id"] == id)
        .expect("prompt response");
    assert_eq!(response["command"], "prompt");
    assert_eq!(response["success"], true);

    let user = records
        .iter()
        .find(|record| record["type"] == "message_start" && record["message"]["role"] == "user")
        .expect("user message_start");
    assert_eq!(message_text(user).as_deref(), Some(prompt));

    let assistant = records
        .iter()
        .find(|record| record["type"] == "message_end" && record["message"]["role"] == "assistant")
        .expect("assistant message_end");
    assert_eq!(assistant["message"]["stopReason"], "stop");
    assert_eq!(assistant["message"]["provider"], "faux");
    assert_eq!(assistant["message"]["model"], "faux-1");
    assert!(
        message_text(assistant)
            .as_deref()
            .is_some_and(|text| text.contains(&format!("faux response to: {prompt}"))),
        "unexpected faux assistant message: {assistant}"
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record["type"] == "agent_settled")
            .count(),
        1
    );
}

#[test]
fn rpc_stdin_drives_two_sequential_faux_model_turns_and_persists_them() {
    let sandbox = Sandbox::new("rpc-multi-turn");
    let mut child = sandbox.rpc(&sandbox.root);
    let mut stdin = child.stdin.take().expect("RPC stdin");
    let stdout = child.stdout.take().expect("RPC stdout");
    let mut reader = BufReader::new(stdout);

    send_rpc(
        &mut stdin,
        serde_json::json!({"id": "turn-1", "type": "prompt", "message": "first"}),
    );
    let first = read_until_settled(&mut reader);
    assert_rpc_prompt_turn(&first, "turn-1", "first");

    send_rpc(
        &mut stdin,
        serde_json::json!({"id": "turn-2", "type": "prompt", "message": "second"}),
    );
    let second = read_until_settled(&mut reader);
    assert_rpc_prompt_turn(&second, "turn-2", "second");

    drop(stdin);
    let status = child.wait().expect("wait for RPC process");
    let stderr = child
        .stderr
        .take()
        .map(|mut stream| {
            let mut text = String::new();
            stream.read_to_string(&mut text).expect("read RPC stderr");
            text
        })
        .unwrap_or_default();
    assert!(status.success(), "RPC process failed: {stderr}");

    let files = sandbox.session_files();
    assert_eq!(files.len(), 1, "two prompts must share one RPC session");
    let entries = persisted_message_entries(&files[0]);
    let users: Vec<_> = entries
        .iter()
        .filter(|entry| entry["message"]["role"] == "user")
        .filter_map(message_text)
        .collect();
    assert_eq!(users, ["first", "second"]);
    let assistants: Vec<_> = entries
        .iter()
        .filter(|entry| entry["message"]["role"] == "assistant")
        .collect();
    assert_eq!(
        assistants.len(),
        2,
        "expected two persisted assistant turns"
    );
    for assistant in assistants {
        assert_eq!(assistant["message"]["provider"], "faux");
        assert_eq!(assistant["message"]["model"], "faux-1");
    }
}

#[test]
fn rpc_stdin_keeps_tool_results_and_unknown_commands_in_protocol() {
    let sandbox = Sandbox::new("rpc-tool-errors");
    let mut child = sandbox.rpc(&sandbox.root);
    let mut stdin = child.stdin.take().expect("RPC stdin");
    let stdout = child.stdout.take().expect("RPC stdout");
    let mut reader = BufReader::new(stdout);

    send_rpc(
        &mut stdin,
        serde_json::json!({
            "id": "bash-ok",
            "type": "bash",
            "command": "printf tool-ok"
        }),
    );
    let ok_records = read_until_rpc_response(&mut reader, "bash-ok");
    let ok_response = ok_records
        .iter()
        .find(|record| record["type"] == "response" && record["id"] == "bash-ok")
        .expect("successful bash response");
    assert_eq!(ok_response["success"], true);
    assert_eq!(ok_response["data"]["output"], "tool-ok");
    assert_eq!(ok_response["data"]["exitCode"], 0);
    assert!(ok_records.iter().any(|record| {
        record["type"] == "bash_execution_update"
            && record["id"] == "bash-ok"
            && record["delta"] == "tool-ok"
    }));

    send_rpc(
        &mut stdin,
        serde_json::json!({
            "id": "bash-fail",
            "type": "bash",
            "command": "printf tool-error; exit 7"
        }),
    );
    let failure_records = read_until_rpc_response(&mut reader, "bash-fail");
    let failure_response = failure_records
        .iter()
        .find(|record| record["type"] == "response" && record["id"] == "bash-fail")
        .expect("failed bash response");
    assert_eq!(failure_response["success"], true);
    assert_eq!(failure_response["data"]["output"], "tool-error");
    assert_eq!(failure_response["data"]["exitCode"], 7);

    send_rpc(
        &mut stdin,
        serde_json::json!({"id": "unknown", "type": "not-a-command"}),
    );
    let unknown_records = read_until_rpc_response(&mut reader, "unknown");
    let unknown_response = unknown_records
        .iter()
        .find(|record| record["type"] == "response" && record["id"] == "unknown")
        .expect("unknown command response");
    assert_eq!(unknown_response["success"], false);
    assert_eq!(unknown_response["error"], "Unknown command: not-a-command");

    drop(stdin);
    let status = child.wait().expect("wait for RPC process");
    assert!(status.success(), "RPC tool/error protocol process failed");

    let files = sandbox.session_files();
    assert_eq!(files.len(), 1);
    let entries = persisted_message_entries(&files[0]);
    let bash_entries: Vec<_> = entries
        .iter()
        .filter(|entry| entry["message"]["role"] == "bashExecution")
        .collect();
    assert_eq!(bash_entries.len(), 2);
    assert!(bash_entries.iter().any(|entry| {
        entry["message"]["output"] == "tool-ok" && entry["message"]["exitCode"] == 0
    }));
    assert!(bash_entries.iter().any(|entry| {
        entry["message"]["output"] == "tool-error" && entry["message"]["exitCode"] == 7
    }));
}
