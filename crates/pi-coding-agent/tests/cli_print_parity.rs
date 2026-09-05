#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Binary-level print-mode parity tests (T3 #42/#43): sequential multi-turn
//! prompting and terminal error/abort exit semantics, matching upstream
//! print-mode implementation.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    sessions: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("pi-print-parity-{tag}-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let sessions = root.join("sessions");
        fs::create_dir_all(home.join(".pi").join("agent")).unwrap();
        fs::create_dir_all(&sessions).unwrap();
        Self {
            root,
            home,
            sessions,
        }
    }

    fn pi(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_pi"))
            .env_clear()
            .current_dir(cwd)
            .env(
                "PATH",
                std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
            )
            .env("HOME", &self.home)
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
            .env_clear()
            .current_dir(cwd)
            .env(
                "PATH",
                std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
            )
            .env("HOME", &self.home)
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
        child.wait_with_output().expect("wait for pi with stdin")
    }

    fn stdout(&self, output: &std::process::Output) -> String {
        String::from_utf8_lossy(&output.stdout).to_string()
    }
    fn stderr(&self, output: &std::process::Output) -> String {
        String::from_utf8_lossy(&output.stderr).to_string()
    }

    fn session(&self) -> PathBuf {
        let files = walk_jsonl(&self.sessions);
        assert_eq!(files.len(), 1, "expected one session, found {files:?}");
        files.into_iter().next().unwrap()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn walk_jsonl(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                out.extend(walk_jsonl(&p));
            } else if p.extension().map(|e| e == "jsonl").unwrap_or(false) {
                out.push(p);
            }
        }
    }
    out
}

/// Count `"role":"assistant"` occurrences across all message lines (a
/// line-per-${seq} scan would be over-engineered; the role string is unique
/// to assistant messages).
fn count_assistants(path: &Path) -> usize {
    let content = fs::read_to_string(path).unwrap_or_default();
    content.matches("\"role\":\"assistant\"").count()
}

fn read_header(path: &Path) -> serde_json::Value {
    let content = fs::read_to_string(path).expect("session JSONL");
    serde_json::from_str(content.lines().next().expect("session header")).expect("valid header")
}

fn message_entries(path: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(path)
        .expect("session JSONL")
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|entry| entry["kind"] == "entry" && entry["type"] == "message")
        .collect()
}

fn message_text(entry: &serde_json::Value) -> Option<String> {
    entry["message"]["content"]
        .as_array()?
        .iter()
        .find_map(|block| block["text"].as_str().map(str::to_owned))
}

#[test]
fn multiple_messages_are_prompted_as_sequential_turns() {
    let sandbox = Sandbox::new("multi-turn");
    let cwd = sandbox.root.clone();
    let out = sandbox.pi(
        &cwd,
        &[
            "-p",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "first",
            "second",
        ],
    );
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    // The visible final output is the last assistant turn.
    assert!(
        sandbox.stdout(&out).contains("faux response to: second"),
        "no final reply"
    );

    let session = sandbox.session();
    let entries = message_entries(&session);
    let user_texts: Vec<_> = entries
        .iter()
        .filter(|entry| entry["message"]["role"] == "user")
        .filter_map(message_text)
        .collect();
    assert_eq!(user_texts, ["first", "second"]);

    let assistants: Vec<_> = entries
        .iter()
        .filter(|entry| entry["message"]["role"] == "assistant")
        .collect();
    let assistant_entries = assistants.len();
    // Sequential turns ⇒ two assistant entries persisted (a batched run would
    // persist a single assistant turn).
    assert_eq!(
        assistant_entries, 2,
        "expected two assistant turns, got {assistant_entries}"
    );
    assert_eq!(
        assistants
            .iter()
            .filter_map(|entry| message_text(entry))
            .collect::<Vec<_>>(),
        ["faux response to: first", "faux response to: second"]
    );
    for assistant in assistants {
        assert_eq!(assistant["message"]["provider"], "faux");
        assert_eq!(assistant["message"]["model"], "faux-1");
    }
}
#[test]
fn unicode_and_whitespace_messages_survive_verbatim() {
    let sandbox = Sandbox::new("unicode-messages");
    let cwd = sandbox.root.clone();
    let messages = ["héllo wörld 🌍", "  padded  ", "\ttabbed", "   "];
    let mut argv = vec!["-p", "--provider", "faux", "--model", "faux-1"];
    argv.extend(messages);
    let out = sandbox.pi(&cwd, &argv);
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    // Only the last assistant turn is visible; whitespace-only input must
    // survive verbatim (upstream applies no trim/filter to messages).
    assert_eq!(sandbox.stdout(&out), "faux response to:    \n");

    let entries = message_entries(&sandbox.session());
    let user_texts: Vec<_> = entries
        .iter()
        .filter(|entry| entry["message"]["role"] == "user")
        .filter_map(message_text)
        .collect();
    assert_eq!(user_texts, messages);
    let assistant_texts: Vec<_> = entries
        .iter()
        .filter(|entry| entry["message"]["role"] == "assistant")
        .filter_map(message_text)
        .collect();
    assert_eq!(
        assistant_texts,
        messages
            .iter()
            .map(|text| format!("faux response to: {text}"))
            .collect::<Vec<_>>()
    );
}

#[test]
fn bare_model_ambiguous_across_providers_names_both() {
    let sandbox = Sandbox::new("bare-ambiguous");
    let cwd = sandbox.root.clone();
    let out = sandbox.pi(&cwd, &["-p", "--model", "gemini-2.5-flash", "hello"]);
    assert!(!out.status.success(), "expected nonzero exit");
    let stderr = sandbox.stderr(&out);
    assert!(
        stderr.contains("Model \"gemini-2.5-flash\" is ambiguous across providers")
            && stderr.contains("google/gemini-2.5-flash")
            && stderr.contains("google-vertex/gemini-2.5-flash")
            && stderr.contains("Use --provider or provider/model"),
        "expected upstream ambiguity diagnostic, got: {stderr}"
    );
}
#[test]
fn uppercase_provider_and_model_flags_resolve_canonically() {
    let sandbox = Sandbox::new("provider-case");
    let cwd = sandbox.root.clone();
    let out = sandbox.pi(
        &cwd,
        &["-p", "--provider", "FAUX", "--model", "FAUX-1", "hello"],
    );
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    assert_eq!(sandbox.stdout(&out), "faux response to: hello\n");

    let entries = message_entries(&sandbox.session());
    let assistant = entries
        .iter()
        .find(|entry| entry["message"]["role"] == "assistant")
        .expect("assistant turn");
    assert_eq!(assistant["message"]["provider"], "faux");
    assert_eq!(assistant["message"]["model"], "faux-1");
}

#[test]
fn piped_stdin_is_the_initial_text_print_prompt() {
    let sandbox = Sandbox::new("stdin");
    let out = sandbox.pi_with_stdin(
        &sandbox.root,
        &["-p", "--provider", "faux", "--model", "faux-1"],
        "stdin prompt\n",
    );
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    assert_eq!(sandbox.stdout(&out), "faux response to: stdin prompt\n");

    let entries = message_entries(&sandbox.session());
    assert_eq!(entries.len(), 2, "expected one durable user/assistant turn");
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry["message"]["role"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["user", "assistant"]
    );
    assert_eq!(
        entries.iter().filter_map(message_text).collect::<Vec<_>>(),
        [
            "stdin prompt".to_string(),
            "faux response to: stdin prompt".to_string()
        ]
    );
}

#[test]
fn terminal_provider_error_exits_nonzero_with_raw_message() {
    let sandbox = Sandbox::new("provider-error");
    let cwd = sandbox.root.clone();
    // A real provider with no key terminates in an error; print mode must
    // surface it on stderr and exit nonzero (upstream exitCode = 1).
    let out = sandbox.pi(
        &cwd,
        &["-p", "--provider", "openai", "--model", "gpt-5.4", "hi"],
    );
    assert!(!out.status.success(), "expected nonzero exit");
    let stderr = sandbox.stderr(&out);
    assert!(
        !stderr.starts_with("Error: model error"),
        "print mode must surface the raw error, got: {stderr}"
    );
    // Upstream prints the raw error message (not wrapped).
    assert!(!stderr.is_empty(), "expected an error message on stderr");
    assert!(
        !sandbox.stdout(&out).contains("faux"),
        "no reply expected on stdout"
    );
}

#[test]
fn custom_faux_model_warns_and_completes_a_real_turn() {
    let sandbox = Sandbox::new("custom-model");
    let out = sandbox.pi(
        &sandbox.root,
        &[
            "-p",
            "--provider",
            "faux",
            "--model",
            "missing-model",
            "hello",
        ],
    );
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    let stderr = sandbox.stderr(&out);
    assert!(
        stderr.contains(
            "Model \"missing-model\" not found for provider \"faux\". Using custom model id."
        ),
        "expected deterministic custom-model warning, got: {stderr}"
    );
    assert_eq!(sandbox.stdout(&out), "faux response to: hello\n");
    assert!(
        !walk_jsonl(&sandbox.sessions).is_empty(),
        "custom provider model must complete and persist a durable turn"
    );
}

#[test]
fn text_file_argument_is_merged_into_the_first_prompt() {
    let sandbox = Sandbox::new("text-file");
    fs::write(sandbox.root.join("context.txt"), "file context").unwrap();

    let out = sandbox.pi(
        &sandbox.root,
        &[
            "-p",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "@context.txt",
            "answer the question",
        ],
    );
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));

    let content = fs::read_to_string(sandbox.session()).unwrap();
    assert!(content.contains("file context"), "file contents were lost");
    assert!(
        content.contains("answer the question"),
        "the positional prompt was lost"
    );
    assert_eq!(count_assistants(&sandbox.session()), 1);
}

#[test]
fn image_file_argument_is_attached_and_normalized() {
    let sandbox = Sandbox::new("image-file");
    let cwd = sandbox.root.clone();
    let mut bmp = vec![0u8; 58];
    bmp[0..2].copy_from_slice(b"BM");
    let bmp_len = bmp.len() as u32;
    bmp[2..6].copy_from_slice(&bmp_len.to_le_bytes());
    bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
    bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
    bmp[18..22].copy_from_slice(&1i32.to_le_bytes());
    bmp[22..26].copy_from_slice(&1i32.to_le_bytes());
    bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
    bmp[28..30].copy_from_slice(&24u16.to_le_bytes());
    bmp[34..38].copy_from_slice(&4u32.to_le_bytes());
    bmp[56] = 0xff;
    fs::write(cwd.join("pixel.bmp"), bmp).unwrap();

    let out = sandbox.pi(
        &cwd,
        &[
            "-p",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "@pixel.bmp",
            "inspect",
        ],
    );
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    let session = walk_jsonl(&sandbox.sessions)
        .into_iter()
        .find_map(|path| fs::read_to_string(path).ok())
        .expect("session JSONL");
    assert!(session.contains("\"type\":\"image\""));
    assert!(session.contains("\"mimeType\":\"image/png\""));
    // The tag is JSON-escaped in the persisted session transcript.
    assert!(session.contains("<file name=\\\""));
}

#[test]
fn print_mode_auto_compaction_persists_and_continues() {
    let sandbox = Sandbox::new("auto-compaction");
    fs::write(
        sandbox.home.join(".pi/agent/settings.json"),
        r#"{"compaction":{"enabled":true,"reserveTokens":127999,"keepRecentTokens":1}}"#,
    )
    .unwrap();

    let cwd = sandbox.root.clone();
    let out = sandbox.pi(
        &cwd,
        &[
            "-p",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "first",
            "second",
        ],
    );
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    assert!(
        sandbox.stdout(&out).contains("faux response to: second"),
        "print mode did not continue after compaction"
    );
    let session = walk_jsonl(&sandbox.sessions)
        .into_iter()
        .find_map(|path| fs::read_to_string(path).ok())
        .expect("session JSONL");
    assert!(
        session.contains("\"type\":\"compaction\""),
        "session JSONL did not persist a compaction entry: {session}"
    );
    assert!(
        session.matches("\"type\":\"message\"").count() >= 4,
        "expected both turns to remain in session history"
    );
}

#[test]
fn continue_reopens_the_newest_session_and_appends_in_place() {
    let sandbox = Sandbox::new("continue");
    let cwd = sandbox.root.clone();

    let first = sandbox.pi(
        &cwd,
        &["-p", "--provider", "faux", "--model", "faux-1", "first"],
    );
    assert!(first.status.success(), "stderr: {}", sandbox.stderr(&first));

    let before = walk_jsonl(&sandbox.sessions);
    assert_eq!(before.len(), 1, "first run creates one session");

    let continued = sandbox.pi(
        &cwd,
        &[
            "-p",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "--continue",
            "second",
        ],
    );
    assert!(
        continued.status.success(),
        "stderr: {}",
        sandbox.stderr(&continued)
    );
    assert!(sandbox
        .stdout(&continued)
        .contains("faux response to: second"));

    let after = walk_jsonl(&sandbox.sessions);
    assert_eq!(after.len(), 1, "--continue must not create a new session");
    assert_eq!(count_assistants(&after[0]), 2);
    let content = fs::read_to_string(&after[0]).unwrap();
    assert!(content.contains("faux response to: first"));
    assert!(content.contains("faux response to: second"));
}

#[test]
fn resume_reopens_the_only_session_without_rewriting_it() {
    let sandbox = Sandbox::new("resume");
    let cwd = sandbox.root.clone();
    let first = sandbox.pi(
        &cwd,
        &["-p", "--provider", "faux", "--model", "faux-1", "first"],
    );
    assert!(first.status.success(), "stderr: {}", sandbox.stderr(&first));

    let resumed = sandbox.pi(
        &cwd,
        &[
            "-p",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "--resume",
            "second",
        ],
    );
    assert!(
        resumed.status.success(),
        "stderr: {}",
        sandbox.stderr(&resumed)
    );
    let files = walk_jsonl(&sandbox.sessions);
    assert_eq!(files.len(), 1);
    assert_eq!(count_assistants(&files[0]), 2);
    let content = fs::read_to_string(&files[0]).unwrap();
    assert!(content.contains("faux response to: first"));
    assert!(content.contains("faux response to: second"));
}

#[test]
fn fork_accepts_a_session_path_and_persists_parent_metadata() {
    let sandbox = Sandbox::new("fork");
    let cwd = sandbox.root.clone();
    let first = sandbox.pi(
        &cwd,
        &["-p", "--provider", "faux", "--model", "faux-1", "first"],
    );
    assert!(first.status.success(), "stderr: {}", sandbox.stderr(&first));
    let source = walk_jsonl(&sandbox.sessions)
        .into_iter()
        .next()
        .expect("source session");
    let source_id = read_header(&source)["id"].as_str().unwrap().to_string();

    let source_arg = source.to_string_lossy().into_owned();
    let forked = sandbox.pi(
        &cwd,
        &[
            "-p",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "--fork",
            &source_arg,
            "child",
        ],
    );
    assert!(
        forked.status.success(),
        "stderr: {}",
        sandbox.stderr(&forked)
    );
    assert!(sandbox.stdout(&forked).contains("faux response to: child"));

    let files = walk_jsonl(&sandbox.sessions);
    assert_eq!(files.len(), 2, "fork must create a child file");
    let child = files
        .iter()
        .find(|path| {
            read_header(path)
                .get("parentSessionId")
                .and_then(serde_json::Value::as_str)
                == Some(source_id.as_str())
        })
        .expect("fork header parentSessionId");
    assert_eq!(
        count_assistants(child),
        2,
        "fork copies history then appends"
    );
}

#[test]
fn fork_accepts_a_bare_session_id_and_independent_writes_diverge() {
    let sandbox = Sandbox::new("fork-id");
    let cwd = sandbox.root.clone();
    let first = sandbox.pi(
        &cwd,
        &["-p", "--provider", "faux", "--model", "faux-1", "first"],
    );
    assert!(first.status.success(), "stderr: {}", sandbox.stderr(&first));
    let source = walk_jsonl(&sandbox.sessions)
        .into_iter()
        .next()
        .expect("source session");
    let source_id = read_header(&source)["id"].as_str().unwrap().to_string();

    // Fork by bare id (no path, no extension).
    let forked = sandbox.pi(
        &cwd,
        &[
            "-p",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "--fork",
            &source_id,
            "child",
        ],
    );
    assert!(
        forked.status.success(),
        "stderr: {}",
        sandbox.stderr(&forked)
    );
    let files = walk_jsonl(&sandbox.sessions);
    assert_eq!(files.len(), 2, "fork by id must create a child file");
    let child = files
        .iter()
        .find(|path| read_header(path)["parentSessionId"].as_str() == Some(source_id.as_str()))
        .expect("fork-by-id header parentSessionId");
    assert!(sandbox.stdout(&forked).contains("faux response to: child"));

    // Independent writes: appending to the child must not touch the source.
    let source_len = fs::metadata(&source).unwrap().len();
    let again = sandbox.pi(
        &cwd,
        &[
            "-p",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "--session",
            child.to_str().unwrap(),
            "second child turn",
        ],
    );
    assert!(again.status.success(), "stderr: {}", sandbox.stderr(&again));
    assert_eq!(
        fs::metadata(&source).unwrap().len(),
        source_len,
        "source must stay untouched by child writes"
    );
    let child_content = fs::read_to_string(child).unwrap();
    assert!(child_content.contains("second child turn"));

    // Missing target: unknown ids fail with the fork diagnostic.
    let missing = sandbox.pi(
        &cwd,
        &[
            "-p",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "--fork",
            "no-such-session-id",
            "x",
        ],
    );
    assert!(
        !missing.status.success(),
        "unknown fork target must fail: {}",
        sandbox.stderr(&missing)
    );
}
