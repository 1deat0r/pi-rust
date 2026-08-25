//! Real-terminal coverage for the interactive slash-command checkpoint.
//!
//! The fixture rows are submitted through the actual editor and event loop in
//! tmux. This keeps command coverage at the user-visible boundary while the
//! focused unit tests continue to cover pure parsing and state helpers.

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use std::thread;
    use std::time::{Duration, Instant};

    struct Sandbox {
        root: PathBuf,
        home: PathBuf,
        agent_dir: PathBuf,
        project: PathBuf,
        binary: PathBuf,
        html: PathBuf,
        missing: PathBuf,
        raw_log: PathBuf,
    }

    impl Sandbox {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("pi-interactive-slash-{}", uuid::Uuid::new_v4()));
            let home = root.join("home");
            let agent_dir = home.join(".pi").join("agent");
            let project = root.join("project");
            let binary = root.join("pi");
            let html = root.join("session.html");
            let missing = root.join("missing.jsonl");
            let raw_log = root.join("tmux-output.log");
            fs::create_dir_all(&agent_dir).unwrap();
            fs::create_dir_all(&project).unwrap();
            symlink(env!("CARGO_BIN_EXE_pi"), &binary).unwrap();
            Self {
                root,
                home,
                agent_dir,
                project,
                binary,
                html,
                missing,
                raw_log,
            }
        }

        fn seed_session(&self) {
            let mut command = Command::new(&self.binary);
            command
                .current_dir(&self.project)
                .env("HOME", &self.home)
                .env("PI_CODING_AGENT_DIR", &self.agent_dir)
                .env("PI_OFFLINE", "1")
                .env("PI_SKIP_VERSION_CHECK", "1")
                .env("PI_SHARE_DRY_RUN", "1")
                .args([
                    "--print",
                    "--approve",
                    "--provider",
                    "faux",
                    "--model",
                    "faux-1",
                    "--session-id",
                    "seed-session",
                    "seed",
                ]);
            let output = command.output().expect("seed session command must start");
            assert!(
                output.status.success(),
                "seed session failed: {}",
                stderr(&output)
            );
        }

        fn seed_session_path(&self) -> PathBuf {
            let sessions_root = self.agent_dir.join("sessions");
            let session_dir = fs::read_dir(&sessions_root)
                .expect("seed session directory")
                .filter_map(Result::ok)
                .find(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
                .expect("seed session cwd directory");
            fs::read_dir(session_dir.path())
                .expect("seed session files")
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .find(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.ends_with("_seed-session.jsonl"))
                })
                .expect("seed session JSONL file")
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
            let name = format!("pi-interactive-slash-{}", uuid::Uuid::new_v4());
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

            let command = format!(
                "env HOME={} PI_CODING_AGENT_DIR={} PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 PI_SHARE_DRY_RUN=1 {} --approve --provider faux --model faux-1",
                shell_quote(&sandbox.home),
                shell_quote(&sandbox.agent_dir),
                shell_quote(&sandbox.binary),
            );
            let sent = tmux(&["send-keys", "-t", &name, &command, "Enter"]);
            assert!(
                sent.status.success(),
                "tmux send-keys startup failed: {}",
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
            let deadline = Instant::now() + Duration::from_secs(8);
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

        fn send_key(&self, key: &str) {
            let output = tmux(&["send-keys", "-t", &self.name, key]);
            assert!(
                output.status.success(),
                "tmux send-keys {key:?} failed: {}",
                stderr(&output)
            );
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

    fn tmux(args: &[&str]) -> Output {
        Command::new("tmux")
            .args(args)
            .output()
            .expect("tmux must be installed for the interactive PTY parity test")
    }

    fn stderr(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).trim().to_string()
    }

    fn shell_quote(path: &Path) -> String {
        let value = path.to_string_lossy();
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }

    fn wait_for_file(path: &Path, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let content = fs::read_to_string(path).unwrap_or_default();
            if content.contains(needle) {
                return content;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?} in {}",
                path.display()
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn slash_command_fixture_runs_through_real_interactive_terminal() {
        let sandbox = Sandbox::new();
        sandbox.seed_session();
        let session = TmuxSession::start(&sandbox);
        session.wait_for(|capture| capture.contains("(faux/Faux Model)"));

        for row in include_str!("fixtures/interactive/slash_commands.txt").lines() {
            let row = row.trim();
            if row.is_empty() || row.starts_with('#') {
                continue;
            }
            let mut fields = row.split('|');
            let command = fields
                .next()
                .unwrap_or_else(|| panic!("fixture row must be command|expected: {row}"));
            let expected = fields
                .next()
                .unwrap_or_else(|| panic!("fixture row must be command|expected: {row}"));
            let follow_up = fields.next();
            assert!(
                fields.next().is_none(),
                "fixture row supports at most command|expected|follow-up: {row}"
            );
            let command = command
                .replace("$HTML", &sandbox.html.display().to_string())
                .replace("$MISSING", &sandbox.missing.display().to_string())
                .replace("$SEED", &sandbox.seed_session_path().display().to_string());
            let capture_before = session.capture();
            session.send_line(&command);
            match follow_up {
                Some("enter") => session.send_key("Enter"),
                Some("enter-clear") => {
                    session.send_key("Enter");
                    session.send_key("C-u");
                }
                Some("clear") => session.send_key("C-u"),
                Some(other) => panic!("unsupported fixture follow-up {other:?}: {row}"),
                None => {}
            }
            let capture =
                session.wait_for(|capture| capture.contains(expected) && capture != capture_before);

            if command == "/help" {
                for builtin in pi_coding_agent::interactive::slash::BUILTIN_SLASH_COMMANDS {
                    assert!(
                        capture.contains(&format!("/{}", builtin.name)),
                        "help banner omitted /{}:\n{capture}",
                        builtin.name
                    );
                }
            }
            if command == "/reload" {
                wait_for_file(
                    &sandbox.agent_dir.join("settings.json"),
                    "defaultProjectTrust",
                );
            }
            if command.starts_with("/export ") {
                let html = wait_for_file(&sandbox.html, "<html");
                assert!(html.contains("session"), "unexpected HTML export: {html}");
            }
            if command == "/resume" {
                // `/new` leaves the original empty session alongside the
                // seeded one; move past that first item, then confirm the
                // seeded transcript.
                session.send_key("Down");
                session.send_key("Enter");
                let resumed = session.wait_for(|capture| capture.contains("resumed session"));
                let transcript =
                    session.wait_for(|capture| capture.contains("faux response to: seed"));
                assert!(
                    transcript.contains("faux response to: seed"),
                    "resume did not rehydrate the seeded transcript:\n{resumed}\n{transcript}"
                );
            }
        }

        session.send_line("/quit");
        thread::sleep(Duration::from_millis(150));
        session.stop_pipe();
        let raw = fs::read_to_string(&sandbox.raw_log).unwrap_or_default();
        assert!(
            raw.contains("\x1b[?1049h"),
            "alternate-screen entry missing"
        );
        assert!(raw.contains("\x1b[?1049l"), "alternate-screen exit missing");
        assert!(raw.contains("\x1b[?25l"), "cursor hide missing");
        assert!(raw.contains("\x1b[?25h"), "cursor restore missing");
    }
}
