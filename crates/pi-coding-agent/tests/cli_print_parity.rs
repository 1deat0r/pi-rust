//! Binary-level print-mode parity tests (T3 #42/#43): sequential multi-turn
//! prompting and terminal error/abort exit semantics, matching upstream
//! `modes/print-mode.ts`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("PI_CODING_AGENT_SESSION_DIR", &self.sessions)
            .env_remove("PI_PROVIDER")
            .env_remove("PI_MODEL")
            .env_remove("PI_KEY")
            .env_remove("PI_SESSION_ID")
            .args(args)
            .output()
            .expect("spawn pi")
    }

    fn stdout(&self, output: &std::process::Output) -> String {
        String::from_utf8_lossy(&output.stdout).to_string()
    }
    fn stderr(&self, output: &std::process::Output) -> String {
        String::from_utf8_lossy(&output.stderr).to_string()
    }

    /// Walk `sessions` recursively and count assistant-role message entries in
    /// the session JSONL files that the run just wrote.
    fn count_assistant_entries(&self) -> usize {
        walk_jsonl(&self.sessions)
            .into_iter()
            .map(|path| count_assistants(&path))
            .sum()
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

    let assistant_entries = sandbox.count_assistant_entries();
    // Sequential turns ⇒ two assistant entries persisted (a batched run would
    // persist a single assistant turn).
    assert_eq!(
        assistant_entries, 2,
        "expected two assistant turns, got {assistant_entries}"
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
