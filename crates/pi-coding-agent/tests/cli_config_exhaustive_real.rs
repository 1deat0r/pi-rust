#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Focused real-process coverage for CLI/config residuals.
//!
//! Each case starts the built `pi` executable with an isolated HOME and
//! explicit agent/session roots. Successful turns use only the deterministic
//! offline faux provider; parser and diagnostic cases stop before a turn.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    agent: PathBuf,
    sessions: PathBuf,
    project: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "pi-cli-config-exhaustive-{tag}-{}",
            uuid::Uuid::new_v4()
        ));
        let home = root.join("home");
        let agent = root.join("agent");
        let sessions = root.join("sessions");
        let project = root.join("project");
        for path in [&home, &agent, &sessions, &project] {
            fs::create_dir_all(path).expect("create isolated CLI path");
        }
        Self {
            root,
            home,
            agent,
            sessions,
            project,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
        command
            .env_clear()
            .current_dir(&self.project)
            .env("HOME", &self.home)
            .env("PI_CODING_AGENT_DIR", &self.agent)
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

    fn write_global_settings(&self, settings: &str) {
        fs::write(self.agent.join("settings.json"), settings).expect("write global settings");
    }

    fn write_project_settings(&self, settings: &str) {
        let directory = self.project.join(".pi");
        fs::create_dir_all(&directory).expect("create project settings directory");
        fs::write(directory.join("settings.json"), settings).expect("write project settings");
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn error(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn jsonl_files(root: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return result;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            result.extend(jsonl_files(&path));
        } else if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            result.push(path);
        }
    }
    result
}

#[test]
fn clean_home_empty_settings_still_allows_provider_and_model_env_fallback() {
    let sandbox = Sandbox::new("empty-settings-env");
    sandbox.write_global_settings("");

    let mut command = sandbox.command();
    command
        .env("PI_PROVIDER", "faux")
        .env("PI_MODEL", "faux-1")
        .args([
            "--mode",
            "text",
            "--no-tools",
            "--no-session",
            "env fallback",
        ]);
    let output = command.output().expect("spawn real pi process");

    assert!(output.status.success(), "stderr: {}", error(&output));
    assert_eq!(text(&output), "faux response to: env fallback\n");
    assert!(error(&output).is_empty());
    assert!(jsonl_files(&sandbox.sessions).is_empty());
}

#[test]
fn project_settings_require_approve_and_relative_session_env_is_real_filesystem_state() {
    let sandbox = Sandbox::new("project-session");
    sandbox
        .write_global_settings(r#"{"defaultProvider":"google","defaultModel":"gemini-not-used"}"#);
    sandbox.write_project_settings(r#"{"defaultProvider":"faux","defaultModel":"faux-1"}"#);

    let relative_sessions = PathBuf::from("relative-sessions");
    let mut command = sandbox.command();
    command
        .env("PI_CODING_AGENT_SESSION_DIR", &relative_sessions)
        .args([
            "--approve",
            "--session-id",
            "bounded-session",
            "--print",
            "project fallback",
        ]);
    let output = command.output().expect("spawn real pi process");

    assert!(output.status.success(), "stderr: {}", error(&output));
    assert_eq!(text(&output), "faux response to: project fallback\n");
    assert_eq!(
        error(&output),
        "Warning: No project session found with id 'bounded-session'; creating a new session with that id.\n"
    );
    let files = jsonl_files(&sandbox.project.join(&relative_sessions));
    assert_eq!(files.len(), 1, "session was not written below the env root");
    let contents = fs::read_to_string(&files[0]).expect("read session JSONL");
    assert!(contents.contains("\"id\":\"bounded-session\""));
}

#[test]
fn unknown_long_value_is_forwarded_as_one_extension_candidate_at_process_boundary() {
    let sandbox = Sandbox::new("unknown-forwarding");
    let output = sandbox.run(&[
        "--provider",
        "faux",
        "--model",
        "faux-1",
        "--not-registered",
        "candidate-value",
        "--print",
        "must not run",
    ]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output), "\n");
    assert_eq!(error(&output), "Unknown flag: --not-registered\n");
}
