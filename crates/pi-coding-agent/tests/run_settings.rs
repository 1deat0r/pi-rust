#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Binary-level settings wiring: `pi -p` without `--provider`/`--model` must
//! resolve defaults from settings.json (global, then project) — public seam:
//! the spawned `pi` binary's stdout/session output.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    sessions: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("pi-run-settings-{tag}-{}", uuid::Uuid::new_v4()));
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

    fn write_global_settings(&self, v: serde_json::Value) {
        fs::write(
            self.home.join(".pi").join("agent").join("settings.json"),
            v.to_string(),
        )
        .unwrap();
    }

    fn write_project_settings(&self, project: &Path, v: serde_json::Value) {
        fs::create_dir_all(project.join(".pi")).unwrap();
        fs::write(project.join(".pi").join("settings.json"), v.to_string()).unwrap();
    }

    fn command(&self, project: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
        command
            .env_clear()
            .current_dir(project)
            .env("HOME", &self.home)
            .env("PI_CODING_AGENT_SESSION_DIR", &self.sessions);
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        command
    }

    /// Run `pi -p <message>` in `project` with the sandboxed HOME.
    fn pi(&self, project: &Path, message: &str) -> String {
        let out = self
            .command(project)
            .args(["-p", message])
            .output()
            .expect("spawn pi");
        assert!(
            out.status.success(),
            "pi exited with {:?}\nstdout: {}\nstderr: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn pi_run_uses_global_settings_default_provider() {
    let sandbox = Sandbox::new("global");
    sandbox.write_global_settings(json!({
        "defaultProvider": "faux",
        "defaultModel": "faux-1"
    }));
    let project = sandbox.root.join("project");
    fs::create_dir_all(&project).unwrap();

    let stdout = sandbox.pi(&project, "hello from global settings");
    assert!(
        stdout.contains("faux response to: hello from global settings"),
        "unexpected stdout: {stdout}"
    );
}

#[test]
fn pi_run_prefers_cli_provider_over_settings_default() {
    let sandbox = Sandbox::new("cli");
    // Settings default is a provider that would fail if used.
    sandbox.write_global_settings(json!({
        "defaultProvider": "google",
        "defaultModel": "gemini-2.5-pro"
    }));
    let project = sandbox.root.join("project");
    fs::create_dir_all(&project).unwrap();

    let out = sandbox
        .command(&project)
        .args(["-p", "--provider", "faux", "cli wins"])
        .output()
        .expect("spawn pi");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        stdout.contains("faux response to: cli wins"),
        "unexpected stdout: {stdout}"
    );
}

#[test]
fn pi_run_project_settings_override_global() {
    let sandbox = Sandbox::new("project");
    // Global default would fail (google not ported); the project default must win.
    sandbox.write_global_settings(json!({
        "defaultProvider": "google",
        "defaultModel": "gemini-2.5-pro"
    }));
    let project = sandbox.root.join("project");
    fs::create_dir_all(&project).unwrap();
    sandbox.write_project_settings(
        &project,
        json!({ "defaultProvider": "faux", "defaultModel": "faux-1" }),
    );

    // Project settings are trust-gated (upstream resolveProjectTrusted):
    // --approve loads them so the project default wins.
    let out = sandbox
        .command(&project)
        .args(["-p", "--approve", "project wins"])
        .output()
        .expect("spawn pi");
    assert!(
        out.status.success(),
        "pi exited with {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        stdout.contains("faux response to: project wins"),
        "unexpected stdout: {stdout}"
    );
}
