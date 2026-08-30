#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Real process-group suspend/resume coverage for the interactive TUI.
//!
//! The test uses tmux as a controlling PTY and the shell's job-control
//! `fg` command. It therefore exercises the actual terminal handoff, SIGTSTP,
//! SIGCONT, redraw, and final cleanup rather than simulating signals in Rust.

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use std::thread;
    use std::time::{Duration, Instant};

    fn test_binary() -> PathBuf {
        std::env::var_os("PI_RUST_TEST_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pi")))
    }

    struct Sandbox {
        root: PathBuf,
        home: PathBuf,
        agent_dir: PathBuf,
        project: PathBuf,
        raw_log: PathBuf,
    }

    impl Sandbox {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "pi-interactive-suspend-resume-{}",
                uuid::Uuid::new_v4()
            ));
            let home = root.join("home");
            let agent_dir = root.join("agent");
            let project = root.join("project");
            let raw_log = root.join("pty.log");
            fs::create_dir_all(&home).expect("create isolated home");
            fs::create_dir_all(&agent_dir).expect("create isolated agent root");
            fs::create_dir_all(&project).expect("create isolated project");
            Self {
                root,
                home,
                agent_dir,
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
    }

    impl TmuxSession {
        fn start(sandbox: &Sandbox) -> Option<Self> {
            let name = format!("pi-suspend-resume-{}", uuid::Uuid::new_v4());
            let created = tmux(&[
                "new-session",
                "-d",
                "-x",
                "100",
                "-y",
                "30",
                "-c",
                sandbox.project.to_str().expect("project path"),
                "-s",
                &name,
            ]);
            if !created.status.success() {
                eprintln!(
                    "skipping suspend/resume PTY test: tmux could not create a controlling PTY: {}",
                    stderr(&created)
                );
                return None;
            }

            let pipe_command = format!("cat > {}", shell_quote(&sandbox.raw_log));
            let piped = tmux(&["pipe-pane", "-o", "-t", &name, &pipe_command]);
            if !piped.status.success() {
                eprintln!(
                    "skipping suspend/resume PTY test: tmux could not attach the PTY capture: {}",
                    stderr(&piped)
                );
                let _ = tmux(&["kill-session", "-t", &name]);
                return None;
            }

            let command = format!(
                "env HOME={} PI_CODING_AGENT_DIR={} PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 {} --no-session --approve --provider faux --model faux-1 --tui-mode fullscreen",
                shell_quote(&sandbox.home),
                shell_quote(&sandbox.agent_dir),
                shell_quote(&test_binary()),
            );
            let started = tmux(&["send-keys", "-t", &name, "-l", "--", &command]);
            assert!(
                started.status.success(),
                "tmux launch command failed: {}",
                stderr(&started)
            );
            let enter = tmux(&["send-keys", "-t", &name, "Enter"]);
            assert!(
                enter.status.success(),
                "tmux launch Enter failed: {}",
                stderr(&enter)
            );
            Some(Self { name })
        }

        fn capture(&self) -> String {
            let output = tmux(&["capture-pane", "-p", "-t", &self.name]);
            assert!(
                output.status.success(),
                "tmux capture failed: {}",
                stderr(&output)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        }

        fn wait_for_capture<F>(&self, mut predicate: F)
        where
            F: FnMut(&str) -> bool,
        {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let capture = self.capture();
                if predicate(&capture) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for PTY capture; last capture:\n{capture}"
                );
                thread::sleep(Duration::from_millis(25));
            }
        }

        fn send_key(&self, key: &str) {
            let output = tmux(&["send-keys", "-t", &self.name, key]);
            assert!(
                output.status.success(),
                "tmux send-keys {key:?} failed: {}",
                stderr(&output)
            );
        }

        fn send_line(&self, line: &str) {
            let output = tmux(&["send-keys", "-t", &self.name, "-l", "--", line]);
            assert!(
                output.status.success(),
                "tmux send-keys line failed: {}",
                stderr(&output)
            );
            self.send_key("Enter");
        }

        fn raw_log(&self, sandbox: &Sandbox) -> String {
            fs::read_to_string(&sandbox.raw_log).unwrap_or_default()
        }

        fn wait_for_raw(&self, sandbox: &Sandbox, marker: &str) -> String {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let raw = self.raw_log(sandbox);
                if raw.contains(marker) {
                    return raw;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for PTY bytes {marker:?}; last bytes: {raw:?}"
                );
                thread::sleep(Duration::from_millis(25));
            }
        }

        fn wait_for_raw_count(&self, sandbox: &Sandbox, marker: &str, count: usize) -> String {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let raw = self.raw_log(sandbox);
                if raw.matches(marker).count() >= count {
                    return raw;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for {count} occurrences of {marker:?}; got {} in {raw:?}",
                    raw.matches(marker).count()
                );
                thread::sleep(Duration::from_millis(25));
            }
        }

        fn pane_tty(&self) -> String {
            let output = tmux(&["display-message", "-p", "-t", &self.name, "#{pane_tty}"]);
            assert!(output.status.success(), "tmux pane tty lookup failed");
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }

        fn wait_for_cooked_mode(&self) {
            let tty = self.pane_tty();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let output = Command::new("stty")
                    .args(["-a", "-F", &tty])
                    .output()
                    .expect("stty must be available for PTY mode inspection");
                let state = String::from_utf8_lossy(&output.stdout).to_lowercase();
                if state.split_whitespace().any(|flag| flag == "icanon")
                    && state.split_whitespace().any(|flag| flag == "echo")
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

        fn quit(&self, sandbox: &Sandbox) {
            self.send_line("/quit");
            let raw = self.wait_for_raw(sandbox, "\x1b[?1049l");
            assert!(
                raw.contains("\x1b[?1049h"),
                "alternate-screen entry missing"
            );
            assert!(raw.contains("\x1b[?25l"), "cursor hide missing");
            assert!(raw.contains("\x1b[?25h"), "cursor restore missing");
            self.wait_for_cooked_mode();
        }
    }

    impl Drop for TmuxSession {
        fn drop(&mut self) {
            let _ = tmux(&["kill-session", "-t", &self.name]);
        }
    }

    fn tmux_available() -> bool {
        match Command::new("tmux").arg("-V").output() {
            Ok(output) if output.status.success() => true,
            Ok(output) => {
                eprintln!(
                    "skipping suspend/resume PTY test: tmux is unavailable: {}",
                    stderr(&output)
                );
                false
            }
            Err(error) => {
                eprintln!("skipping suspend/resume PTY test: tmux is unavailable: {error}");
                false
            }
        }
    }

    fn tmux(args: &[&str]) -> Output {
        Command::new("tmux")
            .args(args)
            .output()
            .expect("tmux availability was checked before PTY test")
    }

    fn stderr(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).trim().to_string()
    }

    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
    }

    #[test]
    fn ctrl_z_restores_terminal_stops_process_group_and_redraws_after_fg() {
        if !tmux_available() {
            return;
        }
        let sandbox = Sandbox::new();
        let Some(session) = TmuxSession::start(&sandbox) else {
            return;
        };
        session.wait_for_capture(|capture| capture.contains("escape interrupt"));

        session.send_key("C-z");
        let suspended_raw = session.wait_for_raw(&sandbox, "\x1b[?1049l");
        assert!(
            suspended_raw.contains("\x1b[?2004l"),
            "bracketed paste was not disabled before suspend: {suspended_raw:?}"
        );
        session.wait_for_cooked_mode();

        // `fg` is meaningful only if SIGTSTP actually stopped the foreground
        // job. A terminal-only mock would leave no stopped job to continue.
        session.send_line("fg");
        let resumed_raw = session.wait_for_raw_count(&sandbox, "\x1b[?1049h", 2);
        assert!(
            resumed_raw.matches("\x1b[?1049l").count() >= 1,
            "suspend cleanup was not emitted: {resumed_raw:?}"
        );
        session.wait_for_capture(|capture| capture.contains("escape interrupt"));
        session.send_line("prompt after foreground resume");
        session.wait_for_capture(|capture| {
            capture.contains("faux response to: prompt after foreground resume")
        });
        session.quit(&sandbox);
    }
}

#[cfg(not(unix))]
#[test]
fn ctrl_z_suspend_resume_requires_unix_process_groups() {
    eprintln!("skipping suspend/resume PTY test: Unix process-group job control is unsupported");
}
