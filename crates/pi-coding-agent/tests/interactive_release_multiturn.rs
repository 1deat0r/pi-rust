#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

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
            Self::start_with_version_endpoint(sandbox, None)
        }

        fn start_with_version_endpoint(sandbox: &Sandbox, endpoint: Option<&str>) -> Self {
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
            let version_endpoint = endpoint
                .map(|value| format!(" PI_VERSION_CHECK_URL={value}"))
                .unwrap_or_default();
            let offline_environment = if endpoint.is_some() {
                ""
            } else {
                " PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1"
            };
            let command = format!(
                "env HOME={} PI_CODING_AGENT_DIR={}{} PI_SHARE_DRY_RUN=1{} {} --approve --provider faux --model faux-1 --tui-mode fullscreen; exec tail -f /dev/null",
                shell_quote(&sandbox.home),
                shell_quote(&sandbox.agent_dir),
                offline_environment,
                version_endpoint,
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
            self.wait_for_with_poll(&mut predicate, Duration::from_millis(25))
                .1
        }

        fn wait_for_with_poll<F>(&self, predicate: &mut F, poll: Duration) -> (Duration, String)
        where
            F: FnMut(&str) -> bool,
        {
            let started = Instant::now();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let capture = self.capture();
                if predicate(&capture) {
                    return (started.elapsed(), capture);
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for PTY output; last capture:\n{capture}"
                );
                thread::sleep(poll);
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

        fn send_text(&self, text: &str) {
            let output = tmux(&["send-keys", "-l", "-t", &self.name, text]);
            assert!(
                output.status.success(),
                "tmux literal send-keys {text:?} failed: {}",
                stderr(&output)
            );
        }

        fn send_key(&self, key: &str) {
            let output = tmux(&["send-keys", "-t", &self.name, key]);
            assert!(
                output.status.success(),
                "tmux send-keys {key:?} failed: {}",
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
        session.wait_for(|capture| capture.contains("faux-1"));

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

    #[test]
    fn composer_echoes_a_real_typing_burst_without_a_stall() {
        let sandbox = Sandbox::new();
        let session = TmuxSession::start(&sandbox);
        session.wait_for(|capture| capture.contains("faux-1"));

        let probe = "typing-lag-probe-42";
        let started = Instant::now();
        for character in probe.chars() {
            session.send_text(&character.to_string());
        }
        let capture = session.wait_for(|capture| capture.contains(probe));
        let elapsed = started.elapsed();
        assert!(
            elapsed <= Duration::from_millis(750),
            "composer took {elapsed:?} to echo {probe:?}; last capture:\n{capture}"
        );

        // Use the editor's real kill-line binding so the probe does not get
        // concatenated with the shutdown command while the input queue is
        // still draining.
        session.send_key("C-u");
        session.send_line("/quit");
        session.wait_for_raw(&sandbox, "\x1b[?1049l");
        session.wait_for_cooked_mode();
        session.stop_pipe();
    }

    #[test]
    fn composer_echoes_each_real_keystroke_without_accumulating_input_lag() {
        let sandbox = Sandbox::new();
        let session = TmuxSession::start(&sandbox);
        session.wait_for(|capture| capture.contains("faux-1"));

        // Exercise the same hot path after real turns have populated the
        // transcript. A regression that rebuilds all historical blocks on
        // every composer key can look fine on an empty session while becoming
        // visibly laggy once the user has been working for a while.
        for turn in 0..6 {
            let prompt = format!("latency history turn {turn}");
            let response = format!("faux response to: {prompt}");
            session.send_line(&prompt);
            session.wait_for(|capture| capture.contains(&response));
        }

        let probe = "per-key-latency-Δ-42";
        let mut expected = String::new();
        let mut samples = Vec::new();
        for character in probe.chars() {
            expected.push(character);
            let started = Instant::now();
            session.send_text(&character.to_string());
            let (waited, capture) = session.wait_for_with_poll(
                &mut |capture: &str| capture.contains(&expected),
                Duration::from_millis(2),
            );
            let elapsed = started.elapsed().max(waited);
            samples.push(elapsed);
            assert!(
                elapsed <= Duration::from_millis(150),
                "keystroke {character:?} took {elapsed:?} to echo; expected {expected:?}; last capture:\n{capture}"
            );
        }

        samples.sort_unstable();
        let p95 = samples[(samples.len() * 95 / 100).min(samples.len() - 1)];
        let max = *samples.last().expect("probe must have samples");
        eprintln!(
            "composer per-key echo latency: samples={} p95={p95:?} max={max:?}",
            samples.len()
        );
        assert!(
            p95 <= Duration::from_millis(16),
            "composer p95 echo latency exceeded one 60 Hz frame: {p95:?}; samples={samples:?}"
        );
        assert!(
            max <= Duration::from_millis(50),
            "composer max echo latency exceeded 50 ms: {max:?}; samples={samples:?}"
        );

        session.send_key("C-u");
        session.send_line("/quit");
        session.wait_for_raw(&sandbox, "\x1b[?1049l");
        session.wait_for_cooked_mode();
        session.stop_pipe();
    }

    #[test]
    fn startup_does_not_query_upstream_pi_or_show_update_notice() {
        let sandbox = Sandbox::new();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback probe");
        listener
            .set_nonblocking(true)
            .expect("make loopback probe nonblocking");
        let endpoint = format!("http://{}/latest-version", listener.local_addr().unwrap());
        let session = TmuxSession::start_with_version_endpoint(&sandbox, Some(&endpoint));
        let capture = session.wait_for(|capture| capture.contains("faux-1"));
        assert!(!capture.contains("Update available"), "{capture}");
        assert!(!capture.contains("pi.dev"), "{capture}");
        thread::sleep(Duration::from_millis(500));
        assert!(
            listener.accept().is_err(),
            "interactive startup unexpectedly queried the upstream release endpoint"
        );
        session.send_line("startup boundary check");
        let capture = session
            .wait_for(|capture| capture.contains("faux response to: startup boundary check"));
        assert!(!capture.contains("Update available"), "{capture}");
        session.send_line("/quit");
        session.wait_for_raw(&sandbox, "\x1b[?1049l");
        session.wait_for_cooked_mode();
        session.stop_pipe();
    }
}
