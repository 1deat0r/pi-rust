#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Binary-level tests for project trust (`--approve` / `--no-approve`).
//!
//! A trust-requiring project (`.pi/settings.json`) gates project settings
//! loading: `--no-approve` skips them, `--approve` loads them. The observable
//! is the resolved provider: project settings can pin `defaultProvider` to
//! `faux`, which only takes effect when project settings are trusted.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use pi_coding_agent::core::project_trust::ProjectTrustStore;
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
        command
            .env_clear()
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("PI_CODING_AGENT_DIR", &self.agent_dir)
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1");
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        command.args(args).output().expect("spawn pi")
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

fn write_global_settings(sandbox: &Sandbox, settings: serde_json::Value) {
    fs::write(
        sandbox.agent_dir.join("settings.json"),
        serde_json::to_string(&settings).unwrap(),
    )
    .unwrap();
}

#[test]
fn saved_trust_allows_project_settings_without_an_override() {
    let sandbox = Sandbox::new("saved-trust");
    let cwd = trust_requiring_project(&sandbox);
    let store = ProjectTrustStore::new(sandbox.agent_dir.to_str().unwrap());
    store.set(cwd.to_str().unwrap(), Some(true));

    let out = sandbox.pi(&cwd, &["--print", "hello"]);
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    assert!(
        sandbox.stdout(&out).contains("faux response"),
        "saved trust did not load project settings: {}",
        sandbox.stdout(&out)
    );
}

#[test]
fn global_default_project_trust_controls_headless_resolution() {
    let always = Sandbox::new("default-always");
    let always_cwd = trust_requiring_project(&always);
    write_global_settings(&always, json!({ "defaultProjectTrust": "always" }));
    let approved = always.pi(&always_cwd, &["--print", "hello"]);
    assert!(
        approved.status.success(),
        "stderr: {}",
        always.stderr(&approved)
    );
    assert!(always.stdout(&approved).contains("faux response"));

    let never = Sandbox::new("default-never");
    let never_cwd = trust_requiring_project(&never);
    write_global_settings(&never, json!({ "defaultProjectTrust": "never" }));
    let denied = never.pi(&never_cwd, &["--print", "hello"]);
    let combined = format!("{}\n{}", never.stdout(&denied), never.stderr(&denied));
    assert!(
        !combined.contains("faux response"),
        "defaultProjectTrust=never loaded project provider: {combined}"
    );
}

#[test]
fn saved_trust_is_applied_to_json_mode_startup() {
    let sandbox = Sandbox::new("json-saved-trust");
    let cwd = trust_requiring_project(&sandbox);
    let store = ProjectTrustStore::new(sandbox.agent_dir.to_str().unwrap());
    store.set(cwd.to_str().unwrap(), Some(true));

    let out = sandbox.pi(&cwd, &["--mode", "json", "hello"]);
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    assert!(
        sandbox.stdout(&out).contains("faux response"),
        "JSON mode ignored saved project trust: {}",
        sandbox.stdout(&out)
    );
}

#[test]
fn explicit_trust_flags_override_saved_decisions() {
    let approved_override = Sandbox::new("override-approve");
    let approved_cwd = trust_requiring_project(&approved_override);
    let approved_store = ProjectTrustStore::new(approved_override.agent_dir.to_str().unwrap());
    approved_store.set(approved_cwd.to_str().unwrap(), Some(false));

    let approved = approved_override.pi(&approved_cwd, &["--approve", "--print", "hello"]);
    assert!(
        approved.status.success(),
        "--approve must override saved denial: {}",
        approved_override.stderr(&approved)
    );
    assert!(approved_override
        .stdout(&approved)
        .contains("faux response"));
    assert_eq!(
        approved_store.get(approved_cwd.to_str().unwrap()),
        Some(false)
    );

    let denied_override = Sandbox::new("override-deny");
    let denied_cwd = trust_requiring_project(&denied_override);
    let denied_store = ProjectTrustStore::new(denied_override.agent_dir.to_str().unwrap());
    denied_store.set(denied_cwd.to_str().unwrap(), Some(true));

    let denied = denied_override.pi(&denied_cwd, &["--no-approve", "--print", "hello"]);
    assert!(!denied.status.success());
    let combined = format!(
        "{}\n{}",
        denied_override.stdout(&denied),
        denied_override.stderr(&denied)
    );
    assert!(
        !combined.contains("faux response"),
        "--no-approve must override saved approval: {combined}"
    );
    assert_eq!(denied_store.get(denied_cwd.to_str().unwrap()), Some(true));
}

#[cfg(unix)]
mod interactive_prompt {
    use super::*;
    use std::thread;
    use std::time::{Duration, Instant};

    fn tmux(args: &[&str]) -> std::process::Output {
        Command::new("tmux")
            .args(args)
            .output()
            .expect("tmux must be installed for the trust prompt test")
    }

    fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }

    fn capture(session: &str) -> String {
        let output = tmux(&["capture-pane", "-p", "-t", session]);
        assert!(output.status.success(), "capture failed");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn wait_for(session: &str, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let output = capture(session);
            if output.contains(needle) {
                return output;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?}"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn interactive_ask_prompt_saves_project_trust() {
        let sandbox = Sandbox::new("interactive-prompt");
        let cwd = trust_requiring_project(&sandbox);
        let session = format!("pi-trust-prompt-{}", uuid::Uuid::new_v4());
        let created = tmux(&[
            "new-session",
            "-d",
            "-x",
            "100",
            "-y",
            "30",
            "-c",
            cwd.to_str().unwrap(),
            "-s",
            &session,
        ]);
        assert!(created.status.success(), "tmux session creation failed");

        let command = format!(
            "env HOME={} PI_CODING_AGENT_DIR={} PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 {} --provider faux --model faux-1",
            shell_quote(sandbox.home.to_str().unwrap()),
            shell_quote(sandbox.agent_dir.to_str().unwrap()),
            shell_quote(env!("CARGO_BIN_EXE_pi")),
        );
        let launched = tmux(&["send-keys", "-t", &session, &command, "Enter"]);
        assert!(launched.status.success(), "interactive launch failed");
        let prompt = wait_for(&session, "Trust project folder?");
        assert!(
            prompt.contains("Enter confirm") && prompt.contains("Esc cancel"),
            "startup trust prompt was not rendered by the TUI selector: {prompt}"
        );

        let answered = tmux(&["send-keys", "-t", &session, "Enter"]);
        assert!(answered.status.success(), "trust answer failed");
        wait_for(&session, "faux-1");

        let quit = tmux(&["send-keys", "-t", &session, "/quit", "Enter"]);
        assert!(quit.status.success(), "interactive quit failed");
        thread::sleep(Duration::from_millis(150));
        let _ = tmux(&["kill-session", "-t", &session]);

        let store = ProjectTrustStore::new(sandbox.agent_dir.to_str().unwrap());
        assert_eq!(store.get(cwd.to_str().unwrap()), Some(true));
    }

    #[test]
    fn interactive_trust_selector_saves_parent_and_cancel_preserves_it() {
        let sandbox = Sandbox::new("interactive-selector");
        let cwd = trust_requiring_project(&sandbox);
        let session = format!("pi-trust-selector-{}", uuid::Uuid::new_v4());
        let created = tmux(&[
            "new-session",
            "-d",
            "-x",
            "100",
            "-y",
            "30",
            "-c",
            cwd.to_str().unwrap(),
            "-s",
            &session,
        ]);
        assert!(created.status.success(), "tmux session creation failed");

        let command = format!(
            "env HOME={} PI_CODING_AGENT_DIR={} PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 {} --provider faux --model faux-1",
            shell_quote(sandbox.home.to_str().unwrap()),
            shell_quote(sandbox.agent_dir.to_str().unwrap()),
            shell_quote(env!("CARGO_BIN_EXE_pi")),
        );
        let launched = tmux(&["send-keys", "-t", &session, &command, "Enter"]);
        assert!(launched.status.success(), "interactive launch failed");
        let prompt = wait_for(&session, "Trust project folder?");
        assert!(prompt.contains("Esc cancel"));
        let answered = tmux(&["send-keys", "-t", &session, "Enter"]);
        assert!(answered.status.success(), "trust answer failed");
        wait_for(&session, "faux-1");

        let opened = tmux(&["send-keys", "-t", &session, "/trust", "Enter"]);
        assert!(opened.status.success(), "trust selector command failed");
        wait_for(&session, "Project trust");
        let selected_parent = tmux(&["send-keys", "-t", &session, "j", "Enter"]);
        assert!(
            selected_parent.status.success(),
            "parent trust selection failed"
        );
        wait_for(&session, "Saved trust decision: trusted");

        let store = ProjectTrustStore::new(sandbox.agent_dir.to_str().unwrap());
        let saved_parent = store.get_entry(cwd.to_str().unwrap()).unwrap();
        assert!(saved_parent.decision);
        assert_ne!(saved_parent.path, cwd.to_string_lossy().into_owned());

        let reopened = tmux(&["send-keys", "-t", &session, "/trust", "Enter"]);
        assert!(reopened.status.success(), "trust selector reopen failed");
        wait_for(&session, "Project trust");
        let cancelled = tmux(&["send-keys", "-t", &session, "Escape"]);
        assert!(cancelled.status.success(), "trust selector cancel failed");
        thread::sleep(Duration::from_millis(150));
        let after_cancel = store.get_entry(cwd.to_str().unwrap()).unwrap();
        assert_eq!(after_cancel, saved_parent);

        let quit = tmux(&["send-keys", "-t", &session, "/quit", "Enter"]);
        assert!(quit.status.success(), "interactive quit failed");
        thread::sleep(Duration::from_millis(150));
        let _ = tmux(&["kill-session", "-t", &session]);

        let restart_session = format!("pi-trust-restart-{}", uuid::Uuid::new_v4());
        let restarted = tmux(&[
            "new-session",
            "-d",
            "-x",
            "100",
            "-y",
            "30",
            "-c",
            cwd.to_str().unwrap(),
            "-s",
            &restart_session,
        ]);
        assert!(restarted.status.success(), "tmux restart session failed");
        let relaunched = tmux(&["send-keys", "-t", &restart_session, &command, "Enter"]);
        assert!(relaunched.status.success(), "interactive restart failed");
        let restarted_screen = wait_for(&restart_session, "faux-1");
        assert!(
            !restarted_screen.contains("Trust project folder?"),
            "saved parent trust unexpectedly prompted again: {restarted_screen}"
        );
        let quit_restart = tmux(&["send-keys", "-t", &restart_session, "/quit", "Enter"]);
        assert!(
            quit_restart.status.success(),
            "interactive restart quit failed"
        );
        thread::sleep(Duration::from_millis(150));
        let _ = tmux(&["kill-session", "-t", &restart_session]);
    }

    #[test]
    fn interactive_startup_trust_cancel_continues_untrusted_without_persisting() {
        let sandbox = Sandbox::new("interactive-startup-cancel");
        let cwd = trust_requiring_project(&sandbox);
        let session = format!("pi-trust-cancel-{}", uuid::Uuid::new_v4());
        let created = tmux(&[
            "new-session",
            "-d",
            "-x",
            "100",
            "-y",
            "30",
            "-c",
            cwd.to_str().unwrap(),
            "-s",
            &session,
        ]);
        assert!(created.status.success(), "tmux session creation failed");

        let command = format!(
            "env HOME={} PI_CODING_AGENT_DIR={} PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 {} --provider faux --model faux-1",
            shell_quote(sandbox.home.to_str().unwrap()),
            shell_quote(sandbox.agent_dir.to_str().unwrap()),
            shell_quote(env!("CARGO_BIN_EXE_pi")),
        );
        let launched = tmux(&["send-keys", "-t", &session, &command, "Enter"]);
        assert!(launched.status.success(), "interactive launch failed");
        let prompt = wait_for(&session, "Trust project folder?");
        assert!(prompt.contains("Esc cancel"));

        let cancelled = tmux(&["send-keys", "-t", &session, "Escape"]);
        assert!(cancelled.status.success(), "startup trust cancel failed");
        wait_for(&session, "faux-1");

        let store = ProjectTrustStore::new(sandbox.agent_dir.to_str().unwrap());
        assert_eq!(store.get(cwd.to_str().unwrap()), None);

        let quit = tmux(&["send-keys", "-t", &session, "/quit", "Enter"]);
        assert!(quit.status.success(), "interactive quit failed");
        thread::sleep(Duration::from_millis(150));
        let _ = tmux(&["kill-session", "-t", &session]);
    }
}
