#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Real PTY coverage for the interactive `--no-session` boundary.
//!
//! This is intentionally a process-level test: an ephemeral interactive run
//! must still be usable, while every operation that requires durable session
//! state must report the documented recovery path and must not create a
//! JSONL session behind the user's back.

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
                "pi-interactive-no-session-{}",
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

        fn jsonl_files(&self) -> Vec<PathBuf> {
            fn visit(path: &Path, out: &mut Vec<PathBuf>) {
                let Ok(entries) = fs::read_dir(path) else {
                    return;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        visit(&path, out);
                    } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                        out.push(path);
                    }
                }
            }
            let mut files = Vec::new();
            visit(&self.root, &mut files);
            files
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
            let name = format!("pi-no-session-{}", uuid::Uuid::new_v4());
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
                "tail",
                "-f",
                "/dev/null",
            ]);
            assert!(created.status.success(), "tmux start: {}", stderr(&created));
            let pipe = format!("cat > {}", shell_quote(&sandbox.raw_log));
            let piped = tmux(&["pipe-pane", "-o", "-t", &name, &pipe]);
            assert!(piped.status.success(), "tmux pipe: {}", stderr(&piped));
            let command = format!(
                "env HOME={} PI_CODING_AGENT_DIR={} PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 {} --no-session --approve --provider faux --model faux-1 --tui-mode fullscreen; exec tail -f /dev/null",
                shell_quote(&sandbox.home),
                shell_quote(&sandbox.agent_dir),
                shell_quote(&test_binary()),
            );
            let configured = tmux(&["set-option", "-t", &name, "remain-on-exit", "on"]);
            assert!(
                configured.status.success(),
                "tmux remain-on-exit: {}",
                stderr(&configured)
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

        fn send_line(&self, line: &str) {
            let output = tmux(&["send-keys", "-t", &self.name, "-l", "--", line]);
            assert!(output.status.success(), "tmux input: {}", stderr(&output));
            thread::sleep(Duration::from_millis(80));
            let output = tmux(&["send-keys", "-t", &self.name, "Enter"]);
            assert!(output.status.success(), "tmux enter: {}", stderr(&output));
        }

        fn command(&self, line: &str, expected: &str) {
            let before = self.capture();
            self.send_line(line);
            self.wait_for(|capture| capture != before && capture.contains(expected));
            thread::sleep(Duration::from_millis(750));
        }

        fn quit(&self) {
            self.send_line("/quit");
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let raw = fs::read_to_string(&self.raw_log).unwrap_or_default();
                if raw.contains("\x1b[?1049l") {
                    assert!(raw.contains("\x1b[?1049h"), "alt-screen entry missing");
                    assert!(raw.contains("\x1b[?25l"), "cursor hide missing");
                    assert!(raw.contains("\x1b[?25h"), "cursor restore missing");
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
            .expect("tmux must be installed for PTY coverage")
    }

    fn stderr(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).trim().to_string()
    }

    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
    }

    #[test]
    fn no_session_is_real_and_durable_commands_fail_safely() {
        let sandbox = Sandbox::new();
        let session = TmuxSession::start(&sandbox);
        session.wait_for(|capture| capture.contains("faux-1"));

        session.send_line("ephemeral Unicode prompt 日本語 🙂 é");
        session.wait_for(|capture| capture.contains("faux response to: ephemeral Unicode prompt"));
        thread::sleep(Duration::from_millis(750));

        for (command, expected) in [
            ("/export", "requires a persistent session"),
            ("/import", "requires a persistent session"),
            ("/fork", "requires a persistent session"),
            ("/clone", "requires a persistent session"),
            ("/resume", "requires a persistent session"),
            ("/share", "requires a persistent session"),
        ] {
            session.command(command, expected);
        }

        session.command("/new", "started new session");
        session.send_line("second ephemeral turn");
        session.wait_for(|capture| capture.contains("faux response to: second ephemeral turn"));
        thread::sleep(Duration::from_millis(750));
        session.command("/session", "session ");
        session.quit();

        assert!(
            sandbox.jsonl_files().is_empty(),
            "--no-session created durable JSONL files: {:?}",
            sandbox.jsonl_files()
        );
    }
}

#[cfg(not(unix))]
#[test]
fn interactive_no_session_pty_requires_unix_pty_support() {}
