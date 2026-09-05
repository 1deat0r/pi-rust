#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

//! Aggregate real-process coverage for credential redaction and concurrent
//! instance persistence. Credentials are synthetic and no vendor request is
//! made; the offline failure path is intentional.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Barrier};
use std::thread;

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
            "pi-cross-secret-concurrency-{tag}-{}",
            uuid::Uuid::new_v4()
        ));
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

    fn command(&self) -> Command {
        let mut command = Command::new(test_binary());
        command
            .current_dir(&self.project)
            .env_clear()
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

    fn run(&self, args: &[&str]) -> Output {
        self.command()
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

#[test]
fn request_scoped_api_keys_never_enter_output_or_persistence() {
    let sandbox = Sandbox::new("secret");
    let secret = "synthetic-cross-cutting-api-key-9f37";

    let success = sandbox.run(&[
        "--provider",
        "faux",
        "--model",
        "faux-1",
        "--api-key",
        secret,
        "--no-tools",
        "safe durable prompt",
    ]);
    assert!(
        success.status.success(),
        "faux stderr: {}",
        stderr(&success)
    );
    assert_eq!(stdout(&success), "faux response to: safe durable prompt\n");
    assert_output_does_not_contain(&success, secret);

    let failure = sandbox.run(&[
        "--provider",
        "definitely-missing-provider",
        "--model",
        "missing-model",
        "--api-key",
        secret,
        "--no-tools",
        "--no-session",
        "safe offline failure",
    ]);
    assert!(
        !failure.status.success(),
        "unknown provider unexpectedly passed"
    );
    assert!(
        stderr(&failure).contains("Unknown provider")
            || stderr(&failure).contains("Provider not found")
            || stderr(&failure).contains("No model found"),
        "provider diagnostic missing: {}",
        stderr(&failure)
    );
    assert_output_does_not_contain(&failure, secret);

    let files = regular_files(&sandbox.root);
    assert!(
        files
            .iter()
            .any(|path| path.extension().is_some_and(|ext| ext == "jsonl")),
        "durable success created no session"
    );
    for path in files {
        let bytes = fs::read(&path).expect("read sandbox artifact");
        assert!(
            !bytes
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "synthetic API key leaked into {}",
            path.display()
        );
    }
}

#[test]
fn concurrent_real_processes_keep_sessions_isolated_and_valid() {
    let sandbox = Sandbox::new("concurrent");
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for prompt in ["parallel-alpha", "parallel-beta"] {
        let mut command = sandbox.command();
        command.args([
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "--no-tools",
            prompt,
        ]);
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            barrier.wait();
            let output = command.output().expect("spawn concurrent pi process");
            (prompt, output)
        }));
    }
    barrier.wait();

    for worker in workers {
        let (prompt, output) = worker.join().expect("join concurrent pi process");
        assert!(
            output.status.success(),
            "{prompt} stderr: {}",
            stderr(&output)
        );
        assert_eq!(stdout(&output), format!("faux response to: {prompt}\n"));
    }

    let sessions = jsonl_files(&sandbox.sessions);
    assert_eq!(
        sessions.len(),
        2,
        "expected one session per process: {sessions:?}"
    );
    let parsed = sessions
        .iter()
        .map(|path| {
            fs::read_to_string(path)
                .expect("read concurrent session")
                .lines()
                .map(|line| {
                    serde_json::from_str::<serde_json::Value>(line).unwrap_or_else(|error| {
                        panic!("invalid JSONL in {}: {error}", path.display())
                    })
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    for prompt in ["parallel-alpha", "parallel-beta"] {
        let matching = parsed
            .iter()
            .filter(|records| {
                records
                    .iter()
                    .any(|record| value_contains_exact(record, prompt))
            })
            .count();
        assert_eq!(
            matching, 1,
            "{prompt} was duplicated or lost across sessions"
        );
    }
    for records in &parsed {
        let has_alpha = records
            .iter()
            .any(|record| value_contains_exact(record, "parallel-alpha"));
        let has_beta = records
            .iter()
            .any(|record| value_contains_exact(record, "parallel-beta"));
        assert_ne!(
            has_alpha, has_beta,
            "session transcript was cross-contaminated"
        );
    }
    assert_no_staging_files(&sandbox.root);
}

fn assert_output_does_not_contain(output: &Output, secret: &str) {
    assert!(!stdout(output).contains(secret), "secret leaked to stdout");
    assert!(!stderr(output).contains(secret), "secret leaked to stderr");
}

fn regular_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(regular_files(&path));
        } else if path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    files
}

fn jsonl_files(root: &Path) -> Vec<PathBuf> {
    regular_files(root)
        .into_iter()
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect()
}

fn assert_no_staging_files(root: &Path) {
    let residue = regular_files(root)
        .into_iter()
        .filter(|path| {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            name.contains(".tmp") || name.ends_with(".lock")
        })
        .collect::<Vec<_>>();
    assert!(residue.is_empty(), "staging residue: {residue:?}");
}

fn value_contains_exact(value: &serde_json::Value, expected: &str) -> bool {
    match value {
        serde_json::Value::String(value) => value == expected,
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| value_contains_exact(value, expected)),
        serde_json::Value::Object(values) => values
            .values()
            .any(|value| value_contains_exact(value, expected)),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            false
        }
    }
}

fn test_binary() -> PathBuf {
    std::env::var_os("PI_RUST_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pi")))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
