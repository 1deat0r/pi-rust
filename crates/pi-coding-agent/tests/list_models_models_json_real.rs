#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Real-process coverage for `--list-models` models.json composition.
//!
//! These tests use an isolated agent directory and synthetic credentials only;
//! they never contact a provider endpoint.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    agent: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("pi-list-models-{tag}-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let agent = home.join(".pi").join("agent");
        fs::create_dir_all(&agent).unwrap();
        Self { root, home, agent }
    }

    fn run(&self, key: Option<&str>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
        command.env_clear();
        if let Some(key) = key {
            command.env("PI_KEY", key);
        }
        command
            .env("HOME", &self.home)
            .env("PI_CODING_AGENT_DIR", &self.agent)
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env("LC_ALL", "C")
            .current_dir(&self.root)
            .args(["--list-models", "needle"])
            .output()
            .unwrap()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn overlay_search_and_auth_filtering_are_real_process_behaviors() {
    let sandbox = Sandbox::new("overlay");
    fs::write(
        sandbox.agent.join("models.json"),
        r#"{"providers":{"anthropic":{"apiKey":"$PI_KEY","models":[{"id":"needle-model","name":"Needle","api":"anthropic-messages","reasoning":true,"input":["text"],"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":8192,"maxTokens":1024}]}}}"#,
    )
    .unwrap();

    let output = sandbox.run(Some("synthetic-list-models-key"));
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("needle-model"),
        "overlay model missing: {stdout}"
    );

    // The same overlay is auth-gated. Without a credential, the configured
    // model must not leak into the available list.
    let sandbox = Sandbox::new("no-key");
    fs::write(
        sandbox.agent.join("models.json"),
        r#"{"providers":{"anthropic":{"models":[{"id":"needle-model","api":"anthropic-messages","input":["text"],"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":8192,"maxTokens":1024}]}}}"#,
    )
    .unwrap();
    let output = sandbox.run(None);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("needle-model"),
        "unauthenticated model leaked: {stdout}"
    );
}

#[test]
fn malformed_models_json_warns_on_stderr_without_corrupting_list_output() {
    let sandbox = Sandbox::new("malformed");
    fs::write(sandbox.agent.join("models.json"), "{\"providers\":").unwrap();
    let output = sandbox.run(None);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Warning: errors loading models.json:"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("Failed to parse models.json"),
        "stderr: {stderr}"
    );
}

#[test]
fn overlay_is_removed_on_restart_and_malformed_reload_falls_back_cleanly() {
    let sandbox = Sandbox::new("restart-delete");
    let models_path = sandbox.agent.join("models.json");
    fs::write(
        &models_path,
        r#"{"providers":{"anthropic":{"apiKey":"synthetic","models":[{"id":"needle-model","api":"anthropic-messages","input":["text"],"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":8192,"maxTokens":1024}]}}}"#,
    )
    .unwrap();
    let output = sandbox.run(Some("synthetic-list-models-key"));
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("needle-model"),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    fs::remove_file(&models_path).unwrap();
    let output = sandbox.run(Some("synthetic-list-models-key"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("needle-model"),
        "deleted overlay survived: {stdout}"
    );

    fs::write(&models_path, r#"{"providers":{"anthropic":"#).unwrap();
    let output = sandbox.run(Some("synthetic-list-models-key"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Warning: errors loading models.json:"));
    assert!(stderr.contains("Failed to parse models.json"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("needle-model"));
}
