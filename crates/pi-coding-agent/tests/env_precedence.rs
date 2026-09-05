#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

//! Real-process coverage for ENV-001 (`PI_MODEL`/`PI_PROVIDER`) and ENV-002
//! (`PI_KEY`/provider key variables): environment defaults select the model
//! without CLI flags, CLI flags override the environment, empty values fall
//! through, invalid values fail with the value named, and request-scoped keys
//! never enter output or persistence. Footer/request-selection evidence and
//! live vendor precedence remain open (see the register).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ENV_SECRET: &str = "synthetic-env-pi-key-7c21";
const CLI_SECRET: &str = "synthetic-cli-api-key-94de";

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    agent_dir: PathBuf,
    sessions: PathBuf,
    project: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("pi-env-precedence-{tag}-{}", uuid::Uuid::new_v4()));
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
            .env("LC_ALL", "C")
            .env("PATH", std::env::var_os("PATH").unwrap_or_default());
        command
    }

    fn run_with_env(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        let mut command = self.command();
        for (key, value) in env {
            command.env(key, value);
        }
        command.args(args);
        command.output().expect("spawn real pi process")
    }

    fn assert_no_secret(&self, output: &Output, context: &str) {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        for secret in [ENV_SECRET, CLI_SECRET] {
            assert!(
                !stdout.contains(secret),
                "secret leaked to stdout in {context}"
            );
            assert!(
                !stderr.contains(secret),
                "secret leaked to stderr in {context}: {stderr}"
            );
        }
        let mut files = Vec::new();
        collect_files(&self.root, &mut files);
        assert!(!files.is_empty(), "expected sandbox artifacts in {context}");
        for path in files {
            let bytes = fs::read(&path).expect("read sandbox artifact");
            for secret in [ENV_SECRET, CLI_SECRET] {
                assert!(
                    !contains_bytes(&bytes, secret.as_bytes()),
                    "secret leaked into {} in {context}",
                    path.display()
                );
            }
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn pi_provider_and_pi_model_select_without_cli_flags() {
    let sandbox = Sandbox::new("env-defaults");
    let output = sandbox.run_with_env(
        &[("PI_PROVIDER", "faux"), ("PI_MODEL", "faux-1")],
        &["--no-tools", "env default probe"],
    );
    assert!(
        output.status.success(),
        "env defaults failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "faux response to: env default probe\n"
    );
}

#[test]
fn cli_flags_override_pi_provider_and_pi_model() {
    let sandbox = Sandbox::new("cli-wins");
    let output = sandbox.run_with_env(
        &[
            ("PI_PROVIDER", "definitely-missing-provider"),
            ("PI_MODEL", "missing-model"),
        ],
        &[
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "--no-tools",
            "cli override probe",
        ],
    );
    assert!(
        output.status.success(),
        "CLI override failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "faux response to: cli override probe\n"
    );
}

#[test]
fn empty_env_values_do_not_mask_cli_flags() {
    let sandbox = Sandbox::new("empty-fallthrough");
    let output = sandbox.run_with_env(
        &[("PI_PROVIDER", ""), ("PI_MODEL", "")],
        &[
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "--no-tools",
            "empty fallthrough probe",
        ],
    );
    assert!(
        output.status.success(),
        "empty env fallthrough failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "faux response to: empty fallthrough probe\n"
    );
}

#[test]
fn invalid_pi_provider_fails_with_the_value_named() {
    let sandbox = Sandbox::new("invalid-provider");
    let output = sandbox.run_with_env(
        &[("PI_PROVIDER", "definitely-missing-provider")],
        &["--no-tools", "--no-session", "invalid provider probe"],
    );
    assert!(
        !output.status.success(),
        "invalid PI_PROVIDER unexpectedly passed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Unknown provider")
            || stderr.contains("Provider not found")
            || stderr.contains("No model found"),
        "provider diagnostic missing: {stderr}"
    );
    assert!(
        stderr.contains("definitely-missing-provider"),
        "diagnostic does not name the value: {stderr}"
    );
}

#[test]
fn pi_key_authenticates_without_entering_output_or_persistence() {
    let sandbox = Sandbox::new("pi-key");
    let output = sandbox.run_with_env(
        &[
            ("PI_PROVIDER", "faux"),
            ("PI_MODEL", "faux-1"),
            ("PI_KEY", ENV_SECRET),
        ],
        &["--no-tools", "pi key probe"],
    );
    assert!(
        output.status.success(),
        "PI_KEY turn failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "faux response to: pi key probe\n"
    );
    sandbox.assert_no_secret(&output, "PI_KEY turn");
}

#[test]
fn cli_api_key_and_pi_key_both_stay_redacted() {
    let sandbox = Sandbox::new("both-keys");
    let output = sandbox.run_with_env(
        &[
            ("PI_PROVIDER", "faux"),
            ("PI_MODEL", "faux-1"),
            ("PI_KEY", ENV_SECRET),
        ],
        &["--api-key", CLI_SECRET, "--no-tools", "both keys probe"],
    );
    assert!(
        output.status.success(),
        "dual-key turn failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "faux response to: both keys probe\n"
    );
    sandbox.assert_no_secret(&output, "dual-key turn");
}

fn test_binary() -> PathBuf {
    std::env::var_os("PI_RUST_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pi")))
}

fn collect_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
