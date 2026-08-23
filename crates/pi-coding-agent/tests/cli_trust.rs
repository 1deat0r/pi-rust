//! Binary-level tests for project trust (`--approve` / `--no-approve`).
//!
//! A trust-requiring project (`.pi/settings.json`) gates project settings
//! loading: `--no-approve` skips them, `--approve` loads them. The observable
//! is the resolved provider: project settings can pin `defaultProvider` to
//! `faux`, which only takes effect when project settings are trusted.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    agent_dir: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("pi-trust-cli-{tag}-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let agent_dir = home.join(".pi").join("agent");
        fs::create_dir_all(&agent_dir).unwrap();
        Self {
            root,
            home,
            agent_dir,
        }
    }

    fn pi(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_pi"))
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("PI_CODING_AGENT_DIR", &self.agent_dir)
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
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A project whose `.pi/settings.json` pins the provider to `faux`.
fn trust_requiring_project(sandbox: &Sandbox) -> PathBuf {
    let cwd = sandbox.root.join("project");
    let pi_dir = cwd.join(".pi");
    fs::create_dir_all(&pi_dir).unwrap();
    fs::write(
        pi_dir.join("settings.json"),
        json!({ "defaultProvider": "faux", "defaultModel": "faux-1" }).to_string(),
    )
    .unwrap();
    cwd
}

#[test]
fn no_approve_skips_project_settings() {
    let sandbox = Sandbox::new("no-approve");
    let cwd = trust_requiring_project(&sandbox);
    // --no-approve: project settings are not loaded, so the default provider
    // (google) is used and the run fails with a provider-not-configured error
    // rather than resolving the faux provider.
    let out = sandbox.pi(&cwd, &["--no-approve", "--print", "hello"]);
    let stderr = sandbox.stderr(&out);
    // The run must not resolve faux (project settings skipped). It either
    // errors on the unconfigured default provider or on the missing key.
    assert!(
        stderr.contains("not configured")
            || stderr.contains("No API key")
            || stderr.contains("provider"),
        "expected a provider error, got stderr: {stderr}"
    );
}

#[test]
fn approve_loads_project_settings() {
    let sandbox = Sandbox::new("approve");
    let cwd = trust_requiring_project(&sandbox);
    // --approve: project settings load, provider resolves to faux, and the
    // run completes with the scripted faux reply.
    let out = sandbox.pi(&cwd, &["--approve", "--print", "hello"]);
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    let stdout = sandbox.stdout(&out);
    assert!(stdout.contains("faux response"), "got: {stdout}");
}

#[test]
fn trust_flags_parse_and_help_lists_them() {
    let sandbox = Sandbox::new("help");
    let out = sandbox.pi(&sandbox.root, &["--help"]);
    let stdout = sandbox.stdout(&out);
    assert!(
        stdout.contains("--approve, -a"),
        "help must list --approve: {stdout}"
    );
    assert!(
        stdout.contains("--no-approve, -na"),
        "help must list --no-approve: {stdout}"
    );
}
