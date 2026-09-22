#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Port of `packages/coding-agent/test/session-file-invalid.test.ts`.
//!
//! `--session <non-session-file> -p ...` must print the upstream
//! `openSessionOrExit` diagnostic (`Error: Session file is not a valid
//! pi session: <path>`), exit 1 with no stack frames, and leave the
//! non-session file byte-identical.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    agent_dir: PathBuf,
    sessions: PathBuf,
    project: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "pi-session-file-invalid-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
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

    fn run(&self, args: &[&str]) -> Output {
        Command::new(test_binary())
            .current_dir(&self.project)
            .env_clear()
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.root.join("xdg-config"))
            .env("XDG_DATA_HOME", self.root.join("xdg-data"))
            .env("PI_CODING_AGENT_DIR", &self.agent_dir)
            .env("PI_CODING_AGENT_SESSION_DIR", &self.sessions)
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env("LC_ALL", "C")
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

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn session_flag_invalid_file_prints_friendly_error_and_preserves_content() {
    let sandbox = Sandbox::new("flag");
    let session_file = sandbox.root.join("non-session-data.log");
    let original = b"{\"type\":\"event\",\"data\":\"not a session\"}\n";
    fs::write(&session_file, original).expect("write non-session file");

    let output = sandbox.run(&["--session", session_file.to_str().unwrap(), "-p", "hi"]);

    assert_eq!(output.status.code(), Some(1), "exit status: {output:?}");
    let diagnostics = stderr(&output);
    assert!(
        diagnostics.contains(&format!(
            "Error: Session file is not a valid pi session: {}",
            session_file.display()
        )),
        "friendly diagnostic missing: {diagnostics}"
    );
    assert!(
        !diagnostics.contains("SessionManager.open"),
        "internal call leaked: {diagnostics}"
    );
    assert!(
        !diagnostics.contains("at "),
        "stack frame leaked: {diagnostics}"
    );
    assert_eq!(
        fs::read(&session_file).unwrap(),
        original,
        "non-session file must stay byte-identical"
    );
}
