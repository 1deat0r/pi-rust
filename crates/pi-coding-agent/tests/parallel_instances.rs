//! Real process-isolation coverage for concurrent pi-rust invocations.
//!
//! Each child receives a clean environment, a distinct agent/config/auth
//! root, a distinct session root, and a distinct working directory.  The
//! provider is deliberately the repository's offline faux provider: this
//! proves local process and persistence isolation, not live Codex behavior.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Barrier};
use std::thread;

use serde_json::Value;

fn test_binary() -> PathBuf {
    std::env::var_os("PI_RUST_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pi")))
}

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    agent_dir: PathBuf,
    sessions: PathBuf,
    project: PathBuf,
    settings: String,
    auth_key: String,
}

impl Sandbox {
    fn new(label: &str, auth_key: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "pi-parallel-instance-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        let home = root.join("home");
        let agent_dir = root.join("agent");
        let sessions = root.join("sessions");
        let project = root.join("project");
        for path in [
            &home,
            &agent_dir,
            &sessions,
            &project,
            &root.join("xdg-config"),
            &root.join("xdg-data"),
        ] {
            fs::create_dir_all(path).expect("create isolated process directory");
        }

        // The run deliberately omits --provider/--model, so this file proves
        // that each child resolves its config from its own PI_CODING_AGENT_DIR.
        let settings = serde_json::json!({
            "defaultProvider": "faux",
            "defaultModel": "faux-1",
            "defaultThinkingLevel": "low",
        })
        .to_string();
        fs::write(agent_dir.join("settings.json"), &settings)
            .expect("write isolated settings.json");

        // The auth command below prints this value through the real auth
        // storage path, making accidental cross-root reads observable.
        fs::write(
            agent_dir.join("auth.json"),
            serde_json::json!({
                "google": { "type": "api_key", "key": auth_key }
            })
            .to_string(),
        )
        .expect("write isolated auth.json");

        Self {
            root,
            home,
            agent_dir,
            sessions,
            project,
            settings,
            auth_key: auth_key.to_string(),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(test_binary());
        command
            .env_clear()
            .current_dir(&self.project)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.root.join("xdg-config"))
            .env("XDG_DATA_HOME", self.root.join("xdg-data"))
            .env("PI_CODING_AGENT_DIR", &self.agent_dir)
            .env("PI_CODING_AGENT_SESSION_DIR", &self.sessions)
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env("PI_TELEMETRY", "0")
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .env("RUST_BACKTRACE", "1");
        command
    }

    fn turn_command(&self, session_id: &str, prompt: &str) -> Command {
        let mut command = self.command();
        command
            .args(["--print", "--no-tools", "--session-id", session_id, prompt])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn auth_command(&self) -> Command {
        let mut command = self.command();
        command
            .args(["auth", "print-api-key", "--provider", "google"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn spawn_simultaneously(left: Command, right: Command) -> (Child, Child) {
    let barrier = Arc::new(Barrier::new(2));
    let left_barrier = Arc::clone(&barrier);
    let right_barrier = Arc::clone(&barrier);
    let left = thread::spawn(move || {
        left_barrier.wait();
        let mut left = left;
        left.spawn().expect("spawn left pi-rust process")
    });
    let right = thread::spawn(move || {
        right_barrier.wait();
        let mut right = right;
        right.spawn().expect("spawn right pi-rust process")
    });
    (
        left.join().expect("left spawn worker should not panic"),
        right.join().expect("right spawn worker should not panic"),
    )
}

fn wait_simultaneously(left: Child, right: Child) -> (Output, Output) {
    let left = thread::spawn(move || {
        left.wait_with_output()
            .expect("wait for left pi-rust process")
    });
    let right = thread::spawn(move || {
        right
            .wait_with_output()
            .expect("wait for right pi-rust process")
    });
    (
        left.join().expect("left wait worker should not panic"),
        right.join().expect("right wait worker should not panic"),
    )
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
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
    files.sort();
    files
}

fn transient_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(transient_files(&path));
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if name.ends_with(".lock") || name.ends_with(".tmp") || name.contains(".migration-") {
            files.push(path);
        }
    }
    files.sort();
    files
}

fn assert_session(sandbox: &Sandbox, session_id: &str) {
    let files = jsonl_files(&sandbox.sessions);
    assert_eq!(files.len(), 1, "expected one session in the isolated root");
    assert!(files[0].starts_with(&sandbox.sessions));

    let content = fs::read_to_string(&files[0]).expect("read isolated session");
    let header: Value = serde_json::from_str(content.lines().next().expect("session header"))
        .expect("session header is JSON");
    assert_eq!(header["kind"], "header");
    assert_eq!(header["version"], 4);
    assert_eq!(header["id"], session_id);
    assert_eq!(
        header["cwd"].as_str(),
        sandbox.project.to_str(),
        "session cwd must remain the child working directory"
    );

    // PI_CODING_AGENT_SESSION_DIR must not silently fall back to the agent
    // root's default `sessions` directory.
    assert!(jsonl_files(&sandbox.agent_dir).is_empty());
    assert!(
        transient_files(&sandbox.root).is_empty(),
        "orphaned lock/temp state: {:?}",
        transient_files(&sandbox.root)
    );
}

#[test]
fn concurrent_instances_keep_terminal_and_persistence_state_separate() {
    let left = Sandbox::new("left", "LEFT_ISOLATION_AUTH_KEY");
    let right = Sandbox::new("right", "RIGHT_ISOLATION_AUTH_KEY");
    assert!(test_binary().is_file(), "test binary must be compiled");

    let (left_process, right_process) = spawn_simultaneously(
        left.turn_command("left-session", "LEFT_PARALLEL_PROMPT"),
        right.turn_command("right-session", "RIGHT_PARALLEL_PROMPT"),
    );
    let (left_output, right_output) = wait_simultaneously(left_process, right_process);

    assert!(
        left_output.status.success(),
        "left stderr: {}",
        stderr(&left_output)
    );
    assert!(
        right_output.status.success(),
        "right stderr: {}",
        stderr(&right_output)
    );
    assert_eq!(
        stdout(&left_output),
        "faux response to: LEFT_PARALLEL_PROMPT\n"
    );
    assert_eq!(
        stdout(&right_output),
        "faux response to: RIGHT_PARALLEL_PROMPT\n"
    );
    assert!(stderr(&left_output).is_empty());
    assert!(stderr(&right_output).is_empty());

    assert_session(&left, "left-session");
    assert_session(&right, "right-session");
    assert_eq!(
        fs::read_to_string(left.agent_dir.join("settings.json")).unwrap(),
        left.settings
    );
    assert_eq!(
        fs::read_to_string(right.agent_dir.join("settings.json")).unwrap(),
        right.settings
    );

    // Run the real auth command concurrently as a second proof that
    // PI_CODING_AGENT_DIR isolates credential roots, not only sessions.
    let (left_auth, right_auth) = spawn_simultaneously(left.auth_command(), right.auth_command());
    let (left_auth, right_auth) = wait_simultaneously(left_auth, right_auth);
    assert!(
        left_auth.status.success(),
        "left auth stderr: {}",
        stderr(&left_auth)
    );
    assert!(
        right_auth.status.success(),
        "right auth stderr: {}",
        stderr(&right_auth)
    );
    assert_eq!(stdout(&left_auth), format!("{}\n", left.auth_key));
    assert_eq!(stdout(&right_auth), format!("{}\n", right.auth_key));
    assert!(stderr(&left_auth).is_empty());
    assert!(stderr(&right_auth).is_empty());
}

#[test]
fn concurrent_shared_root_migration_is_atomic_and_leaves_no_staging_race() {
    let sandbox = Sandbox::new("shared", "SHARED_ISOLATION_AUTH_KEY");
    let legacy_dir = sandbox.sessions.join("--shared-project--");
    fs::create_dir_all(&legacy_dir).expect("create legacy session directory");
    let legacy_path = legacy_dir.join("legacy.jsonl");
    fs::write(
        &legacy_path,
        format!(
            "{{\"type\":\"session\",\"version\":3,\"id\":\"legacy-shared\",\"timestamp\":\"2026-08-22T00:00:00.000Z\",\"cwd\":\"{}\"}}\n{{\"type\":\"message\",\"id\":\"legacy-message\",\"parentId\":null,\"timestamp\":\"2026-08-22T00:00:01.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"legacy\"}}}}\n",
            sandbox.project.display()
        ),
    )
    .expect("write legacy session");

    let (left_process, right_process) = spawn_simultaneously(
        sandbox.turn_command("shared-left-session", "SHARED_LEFT_PROMPT"),
        sandbox.turn_command("shared-right-session", "SHARED_RIGHT_PROMPT"),
    );
    let (left_output, right_output) = wait_simultaneously(left_process, right_process);
    assert!(
        left_output.status.success(),
        "left stderr: {}",
        stderr(&left_output)
    );
    assert!(
        right_output.status.success(),
        "right stderr: {}",
        stderr(&right_output)
    );
    assert_eq!(
        stdout(&left_output),
        "faux response to: SHARED_LEFT_PROMPT\n"
    );
    assert_eq!(
        stdout(&right_output),
        "faux response to: SHARED_RIGHT_PROMPT\n"
    );

    let migrated: Value = serde_json::from_str(
        fs::read_to_string(&legacy_path)
            .expect("read migrated shared session")
            .lines()
            .next()
            .expect("migrated session header"),
    )
    .expect("migrated session header is JSON");
    assert_eq!(migrated["kind"], "header");
    assert_eq!(migrated["version"], 4);
    assert!(
        transient_files(&sandbox.root).is_empty(),
        "orphaned migration state: {:?}",
        transient_files(&sandbox.root)
    );
    assert_eq!(jsonl_files(&sandbox.sessions).len(), 3);
}
