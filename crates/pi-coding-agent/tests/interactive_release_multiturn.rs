//! Permanent real-terminal two-turn coverage for the Rust `pi` binary.
//!
//! The test drives the actual line editor through tmux, checks both visible
//! faux responses, verifies that both turns reach the JSONL session, and
//! proves that `/quit` restores the terminal. Set `PI_RUST_TEST_BINARY` to an
//! absolute path to exercise an already-built release binary; otherwise Cargo
//! supplies the debug integration-test binary.

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
        agent_dir: PathBuf,
        project: PathBuf,
        raw_log: PathBuf,
    }

    impl Sandbox {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .join(format!("pi-interactive-multiturn-{}", uuid::Uuid::new_v4()));
            let home = root.join("home");
            let agent_dir = home.join(".pi").join("agent");
            let project = root.join("project");
            let raw_log = root.join("tmux-output.log");
            fs::create_dir_all(&agent_dir).unwrap();
            fs::create_dir_all(&project).unwrap();
            Self {
                root,
                home,
                agent_dir,
                project,
                raw_log,
            }
        }

        fn session_text(&self) -> String {
            let mut text = String::new();
            append_files(&self.agent_dir.join("sessions"), &mut text);
            text
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    struct TmuxSession {
        name: String,
    }

    impl TmuxSession {
        fn start(sandbox: &Sandbox) -> Self {
            let name = format!("pi-interactive-multiturn-{}", uuid::Uuid::new_v4());
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
            assert!(
                created.status.success(),
                "tmux new-session failed: {}",
                stderr(&created)
            );

            let pipe_command = format!("cat > {}", shell_quote(&sandbox.raw_log));
            let piped = tmux(&["pipe-pane", "-o", "-t", &name, &pipe_command]);
            assert!(
                piped.status.success(),
                "tmux pipe-pane failed: {}",
                stderr(&piped)
            );

            let binary = test_binary();
            assert!(
                binary.is_file(),
                "pi test binary does not exist: {}",
                binary.display()
            );
            let command = format!(
                "env HOME={} PI_CODING_AGENT_DIR={} PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 PI_SHARE_DRY_RUN=1 {} --approve --provider faux --model faux-1; exec tail -f /dev/null",
                shell_quote(&sandbox.home),
                shell_quote(&sandbox.agent_dir),
                shell_quote(&binary),
            );
            let configured = tmux(&["set-option", "-t", &name, "remain-on-exit", "on"]);
            assert!(
                configured.status.success(),
                "tmux remain-on-exit setup failed: {}",
                stderr(&configured)
            );
            let sent = tmux(&["respawn-pane", "-k", "-t", &name, &command]);
            assert!(
                sent.status.success(),
                "tmux respawn-pane startup failed: {}",
                stderr(&sent)
            );

            Self { name }
        }

        fn capture(&self) -> String {
            let output = tmux(&["capture-pane", "-p", "-t", &self.name]);
            assert!(
                output.status.success(),
                "tmux capture-pane failed: {}",
                stderr(&output)
            );
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

        fn send_line(&self, line: &str) {
            let output = tmux(&["send-keys", "-t", &self.name, line, "Enter"]);
            assert!(
                output.status.success(),
                "tmux send-keys {line:?} failed: {}",
                stderr(&output)
            );
        }

        fn wait_for_raw(&self, sandbox: &Sandbox, needle: &str) -> String {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let raw = fs::read_to_string(&sandbox.raw_log).unwrap_or_default();
                if raw.contains(needle) {
                    return raw;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for raw PTY sequence {needle:?}; last raw output: {raw:?}"
                );
                thread::sleep(Duration::from_millis(25));
            }
        }

        fn pane_tty(&self) -> String {
            let output = tmux(&["display-message", "-p", "-t", &self.name, "#{pane_tty}"]);
            assert!(
                output.status.success(),
                "tmux pane tty lookup failed: {}",
                stderr(&output)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }

        fn wait_for_cooked_mode(&self) {
            let tty = self.pane_tty();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let output = Command::new("stty")
                    .args(["-a", "-F", &tty])
                    .output()
                    .expect("stty must be installed for PTY mode inspection");
                let state = String::from_utf8_lossy(&output.stdout).to_lowercase();
                if state.split_whitespace().any(|token| token == "icanon")
                    && state.split_whitespace().any(|token| token == "echo")
                {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "PTY did not return to cooked mode: {state}"
                );
                thread::sleep(Duration::from_millis(25));
            }
        }

        fn stop_pipe(&self) {
            let output = tmux(&["pipe-pane", "-t", &self.name]);
            assert!(
                output.status.success(),
                "tmux pipe-pane stop failed: {}",
                stderr(&output)
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    impl Drop for TmuxSession {
        fn drop(&mut self) {
            let _ = tmux(&["kill-session", "-t", &self.name]);
        }
    }

    fn test_binary() -> PathBuf {
        std::env::var_os("PI_RUST_TEST_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pi")))
    }

    fn append_files(path: &Path, output: &mut String) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                append_files(&path, output);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            {
                output.push_str(&fs::read_to_string(path).unwrap_or_default());
            }
        }
    }

    fn tmux(args: &[&str]) -> Output {
        Command::new("tmux")
            .args(args)
            .output()
            .expect("tmux must be installed for interactive PTY tests")
    }

    fn stderr(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).trim().to_string()
    }

    fn shell_quote(path: &Path) -> String {
        let value = path.to_string_lossy();
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }

    #[test]
    fn two_interactive_turns_persist_and_restore_terminal() {
        let sandbox = Sandbox::new();
        let session = TmuxSession::start(&sandbox);
        session.wait_for(|capture| capture.contains("(faux/Faux Model)"));

        session.send_line("first release TUI turn");
        session.wait_for(|capture| capture.contains("faux response to: first release TUI turn"));
        session.send_line("second release TUI turn");
        session.wait_for(|capture| capture.contains("faux response to: second release TUI turn"));

        let transcript = sandbox.session_text();
        assert!(
            transcript.contains("faux response to: first release TUI turn"),
            "first response was not persisted in JSONL: {transcript}"
        );
        assert!(
            transcript.contains("faux response to: second release TUI turn"),
            "second response was not persisted in JSONL: {transcript}"
        );
        assert!(
            transcript.matches("faux response to:").count() >= 2,
            "expected two persisted assistant responses: {transcript}"
        );

        session.send_line("/quit");
        let raw = session.wait_for_raw(&sandbox, "\x1b[?1049l");
        assert!(
            raw.contains("\x1b[?1049h"),
            "alternate-screen entry missing"
        );
        assert!(raw.contains("\x1b[?25l"), "cursor hide missing");
        assert!(raw.contains("\x1b[?25h"), "cursor restore missing");
        session.wait_for_cooked_mode();
        session.stop_pipe();
    }
}
