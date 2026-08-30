#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Real PTY coverage for the official experimental first-run setup.
//!
//! The test drives the binary through tmux, exercises theme preview and the
//! analytics choice, verifies the persisted settings, and confirms that the
//! normal interactive mode starts without a provider turn.

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use std::thread;
    use std::time::{Duration, Instant};

    struct Sandbox {
        root: PathBuf,
        home: PathBuf,
        project: PathBuf,
        raw_log: PathBuf,
    }

    impl Sandbox {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .join(format!("pi-first-time-setup-pty-{}", uuid::Uuid::new_v4()));
            let home = root.join("home");
            let project = root.join("project");
            let raw_log = root.join("startup.log");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&project).unwrap();
            Self {
                root,
                home,
                project,
                raw_log,
            }
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    struct TmuxSession {
        name: String,
        raw_log: PathBuf,
    }

    impl TmuxSession {
        fn start(sandbox: &Sandbox) -> Self {
            let name = format!("pi-first-time-setup-{}", uuid::Uuid::new_v4());
            let created = tmux(&[
                "new-session",
                "-d",
                "-x",
                "100",
                "-y",
                "30",
                "-c",
                sandbox.project.to_str().unwrap(),
                "-s",
                &name,
                "tail",
                "-f",
                "/dev/null",
            ]);
            assert!(created.status.success(), "tmux start: {}", stderr(&created));
            let pipe = format!("cat > {}", shell_quote(&sandbox.raw_log));
            let piped = tmux(&["pipe-pane", "-o", "-t", &name, &pipe]);
            assert!(piped.status.success(), "tmux pipe: {}", stderr(&piped));

            // Deliberately omit PI_CODING_AGENT_DIR: the setup gate must use
            // HOME/.pi/agent for this official-distribution test.
            let command = format!(
                "env HOME={} PI_EXPERIMENTAL=1 PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 {} --no-session --provider faux --model faux-1 --tui-mode fullscreen; exec tail -f /dev/null",
                shell_quote(&sandbox.home),
                shell_quote(Path::new(env!("CARGO_BIN_EXE_pi"))),
            );
            let started = tmux(&["respawn-pane", "-k", "-t", &name, &command]);
            assert!(
                started.status.success(),
                "tmux launch: {}",
                stderr(&started)
            );
            Self {
                name,
                raw_log: sandbox.raw_log.clone(),
            }
        }

        fn capture(&self) -> String {
            let output = tmux(&["capture-pane", "-p", "-t", &self.name]);
            assert!(output.status.success(), "tmux capture: {}", stderr(&output));
            String::from_utf8_lossy(&output.stdout).into_owned()
        }

        fn wait_for<F>(&self, mut predicate: F) -> String
        where
            F: FnMut(&str) -> bool,
        {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let capture = self.capture();
                if predicate(&capture) {
                    return capture;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for PTY output; last capture:\n{capture}"
                );
                thread::sleep(Duration::from_millis(25));
            }
        }

        fn send_key(&self, key: &str) {
            let output = tmux(&["send-keys", "-t", &self.name, key]);
            assert!(
                output.status.success(),
                "tmux send-keys {key:?}: {}",
                stderr(&output)
            );
        }

        fn restart(&self, sandbox: &Sandbox) {
            let command = format!(
                "env HOME={} PI_EXPERIMENTAL=1 PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 {} --no-session --provider faux --model faux-1 --tui-mode fullscreen; exec tail -f /dev/null",
                shell_quote(&sandbox.home),
                shell_quote(Path::new(env!("CARGO_BIN_EXE_pi"))),
            );
            let restarted = tmux(&["respawn-pane", "-k", "-t", &self.name, &command]);
            assert!(
                restarted.status.success(),
                "tmux restart: {}",
                stderr(&restarted)
            );
        }

        fn wait_for_settings(&self, predicate: impl Fn(&str) -> bool) -> String {
            let path = self
                .raw_log
                .parent()
                .unwrap()
                .join("home/.pi/agent/settings.json");
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let content = fs::read_to_string(&path).unwrap_or_default();
                if predicate(&content) {
                    return content;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for settings at {}: {content}",
                    path.display()
                );
                thread::sleep(Duration::from_millis(25));
            }
        }

        fn wait_for_terminal_restore(&self) {
            self.wait_for_terminal_restore_from(0);
        }

        fn wait_for_terminal_restore_from(&self, offset: u64) {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let bytes = fs::read(&self.raw_log).unwrap_or_default();
                let raw = String::from_utf8_lossy(bytes.get(offset as usize..).unwrap_or_default());
                if raw.contains("\x1b[?1049l") {
                    assert!(
                        raw.contains("\x1b[?25h"),
                        "cursor was not restored: {raw:?}"
                    );
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "terminal was not restored: {raw:?}"
                );
                thread::sleep(Duration::from_millis(25));
            }
            let tty = tmux(&["display-message", "-p", "-t", &self.name, "#{pane_tty}"]);
            let tty = String::from_utf8_lossy(&tty.stdout).trim().to_string();
            let state = Command::new("stty")
                .args(["-a", "-F", &tty])
                .output()
                .expect("stty must be available");
            assert!(state.status.success());
            let state = String::from_utf8_lossy(&state.stdout).to_lowercase();
            assert!(state.split_whitespace().any(|flag| flag == "icanon"));
            assert!(state.split_whitespace().any(|flag| flag == "echo"));
        }
    }

    impl Drop for TmuxSession {
        fn drop(&mut self) {
            let _ = tmux(&["kill-session", "-t", &self.name]);
        }
    }

    fn tmux(args: &[&str]) -> Output {
        Command::new("tmux")
            .args(args)
            .output()
            .expect("tmux must be installed for first-run setup PTY coverage")
    }

    fn stderr(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).trim().to_string()
    }

    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
    }

    #[test]
    fn first_run_setup_persists_theme_and_analytics_without_a_provider_turn() {
        let sandbox = Sandbox::new();
        let session = TmuxSession::start(&sandbox);
        let initial = session.wait_for(|capture| capture.contains("Pick a theme."));
        assert!(initial.contains("Dark"));
        assert!(initial.contains("Light"));

        // Preview light, then confirm the theme and opt out of analytics.
        session.send_key("Down");
        let preview = session.wait_for(|capture| capture.contains("Light"));
        assert!(!preview.contains("Opt-in"));
        session.send_key("Enter");
        session.wait_for(|capture| capture.contains("Opt-in to anonymous usage data sharing?"));
        session.send_key("Down");
        session.wait_for(|capture| capture.contains("Don't share"));
        session.send_key("Enter");

        let settings = session.wait_for_settings(|content| {
            content.contains("\"theme\": \"light\"")
                && content.contains("\"enableAnalytics\": false")
        });
        assert!(!settings.contains("trackingId"));
        let normal_mode = session.wait_for(|capture| capture.contains("faux-1"));
        assert!(!normal_mode.contains("faux response to:"));

        session.send_key("C-d");
        session.wait_for_terminal_restore();

        // A completed setup must be skipped on the next real process start.
        session.restart(&sandbox);
        let restarted = session.wait_for(|capture| capture.contains("faux-1"));
        assert!(!restarted.contains("Pick a theme."));
        assert!(!restarted.contains("faux response to:"));
        let restart_raw_offset = fs::metadata(&session.raw_log).unwrap().len();
        session.send_key("C-d");
        session.wait_for_terminal_restore_from(restart_raw_offset);
    }

    #[test]
    fn first_run_setup_cancel_skips_setup_persistence_and_restores_terminal() {
        let sandbox = Sandbox::new();
        let session = TmuxSession::start(&sandbox);
        session.wait_for(|capture| capture.contains("Pick a theme."));
        session.send_key("Escape");
        let normal_mode = session.wait_for(|capture| capture.contains("faux-1"));
        assert!(!normal_mode.contains("faux response to:"));

        thread::sleep(Duration::from_millis(100));
        let settings_path = sandbox.home.join(".pi").join("agent").join("settings.json");
        let settings = fs::read_to_string(settings_path).unwrap_or_default();
        assert!(!settings.contains("\"theme\""));
        assert!(!settings.contains("enableAnalytics"));
        assert!(!settings.contains("trackingId"));

        session.send_key("C-d");
        session.wait_for_terminal_restore();
    }
}

#[test]
fn startup_completion_is_idempotent() {
    let settled = std::sync::atomic::AtomicBool::new(false);
    assert!(pi_coding_agent::interactive::startup::settle_once(&settled));
    assert!(!pi_coding_agent::interactive::startup::settle_once(
        &settled
    ));
}

#[cfg(not(unix))]
#[test]
fn first_time_setup_pty_requires_unix_terminal_support() {}
