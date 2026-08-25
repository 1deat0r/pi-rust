//! PTY coverage for the real `pi config` selector.
//!
//! The component tests exercise state transitions in isolation. This test
//! drives the binary through tmux so crossterm sees a real terminal: the
//! selector is rendered on an alternate screen, resize events reach the
//! event loop, keyboard input changes settings, and the alternate screen is
//! restored when the selector closes.

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
            let root = std::env::temp_dir().join(format!("pi-config-pty-{}", uuid::Uuid::new_v4()));
            let home = root.join("home");
            let agent_dir = home.join(".pi").join("agent");
            let project = root.join("project");
            let raw_log = root.join("tmux-output.log");
            fs::create_dir_all(agent_dir.join("skills").join("alpha")).unwrap();
            fs::create_dir_all(agent_dir.join("skills").join("beta")).unwrap();
            fs::create_dir_all(project.join(".pi").join("skills").join("project")).unwrap();

            fs::write(
                agent_dir.join("skills").join("alpha").join("SKILL.md"),
                "# Alpha\n",
            )
            .unwrap();
            fs::write(
                agent_dir.join("skills").join("beta").join("SKILL.md"),
                "# Beta\n",
            )
            .unwrap();
            fs::write(
                project
                    .join(".pi")
                    .join("skills")
                    .join("project")
                    .join("SKILL.md"),
                "# Project\n",
            )
            .unwrap();

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
        fn start(sandbox: &Sandbox) -> Self {
            let name = format!("pi-config-pty-{}", uuid::Uuid::new_v4());
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
                "env HOME={} PI_CODING_AGENT_DIR={} PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 {} config --approve",
                shell_quote(&sandbox.home),
                shell_quote(&sandbox.agent_dir),
                shell_quote(Path::new(env!("CARGO_BIN_EXE_pi"))),
            );
            let sent = tmux(&["send-keys", "-t", &name, &command, "Enter"]);
            assert!(
                sent.status.success(),
                "tmux send-keys failed: {}",
                stderr(&sent)
            );

            Self { name }
        }

        fn capture(&self) -> String {
            let output = tmux(&["capture-pane", "-p", "-t", &self.name]);
            String::from_utf8_lossy(&output.stdout).into_owned()
        }

        fn wait_for<F>(&self, mut predicate: F) -> String
        where
            F: FnMut(&str) -> bool,
        {
            let deadline = Instant::now() + Duration::from_secs(5);
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
                "tmux send-keys {key:?} failed: {}",
                stderr(&output)
            );
        }

        fn resize(&self, width: &str, height: &str) {
            let output = tmux(&["resize-window", "-t", &self.name, "-x", width, "-y", height]);
            assert!(
                output.status.success(),
                "tmux resize-window failed: {}",
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
            thread::sleep(Duration::from_millis(50));
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
            .expect("tmux must be installed for the PTY parity test")
    }

    fn stderr(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).trim().to_string()
    }

    fn shell_quote(path: &Path) -> String {
        let value = path.to_string_lossy();
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }

    fn normalized_lines(capture: &str) -> Vec<String> {
        capture
            .lines()
            .map(|line| line.trim_end().to_string())
            .collect()
    }

    fn contains_sequence(lines: &[String], expected: &[&str]) -> bool {
        lines.windows(expected.len()).any(|window| {
            window
                .iter()
                .zip(expected)
                .all(|(actual, wanted)| actual == wanted)
        })
    }

    #[test]
    fn config_selector_runs_in_a_pty_and_restores_the_terminal() {
        let sandbox = Sandbox::new();
        let session = TmuxSession::start(&sandbox);

        let initial = session.wait_for(|capture| {
            capture.contains("Global Resources")
                && capture.contains("[x] alpha")
                && capture.contains("[x] beta")
        });
        let initial_lines = normalized_lines(&initial);
        assert!(
            contains_sequence(
                &initial_lines,
                &[
                    "Global Resources",
                    "~/.pi/agent/settings.json",
                    "Search:",
                    "",
                    "  User (~/.pi/agent/)",
                    "    Skills",
                    ">       [x] alpha",
                    "        [x] beta",
                    "",
                    "↑/↓ select · PgUp/PgDn page · Space toggle · Tab switch scope · Esc close",
                ]
            ),
            "unexpected selector snapshot:\n{initial}"
        );

        // Resize while the selector is active. The next event must update the
        // backend dimensions without closing the component or losing rows.
        session.resize("70", "18");
        let resized = session
            .wait_for(|capture| capture.contains("Global Resources") && capture.contains("alpha"));
        assert!(
            resized.contains("beta"),
            "resize lost selector rows:\n{resized}"
        );

        // Down selects the second row and Space persists a global unload.
        session.send_key("Down");
        let selected = session.wait_for(|capture| capture.contains(">       [x] beta"));
        assert!(
            selected.contains("alpha"),
            "keyboard navigation lost alpha row:\n{selected}"
        );
        session.send_key("Space");
        session.wait_for(|capture| capture.contains(">       [ ] beta"));
        let global_settings = sandbox.agent_dir.join("settings.json");
        let global_json = wait_for_file(&global_settings, "-skills/beta/SKILL.md");
        assert!(global_json.contains("-skills/beta/SKILL.md"));

        // Tab enters project scope, Up returns to the inherited alpha row,
        // and Space records a project-local unload override.
        session.send_key("Tab");
        session.wait_for(|capture| capture.contains("Project Local Resources"));
        session.send_key("Up");
        session.send_key("Space");
        let project_settings = sandbox.project.join(".pi").join("settings.json");
        let project_needle = format!(
            "-{}",
            sandbox
                .agent_dir
                .join("skills")
                .join("alpha")
                .join("SKILL.md")
                .display()
        );
        let project_json = wait_for_file(&project_settings, &project_needle);
        assert!(project_json.contains(&project_needle));
        let project_view =
            session.wait_for(|capture| capture.contains("[-] alpha  project unload"));
        assert!(project_view.contains("Project Local Resources"));

        session.send_key("Escape");
        thread::sleep(Duration::from_millis(150));
        session.stop_pipe();
        let raw = fs::read_to_string(&sandbox.raw_log).unwrap_or_default();
        assert!(
            raw.contains("\x1b[?1049h"),
            "alternate-screen entry missing: {raw:?}"
        );
        assert!(
            raw.contains("\x1b[?1049l"),
            "alternate-screen exit missing: {raw:?}"
        );
        assert!(raw.contains("\x1b[?25l"), "cursor hide missing: {raw:?}");
        assert!(raw.contains("\x1b[?25h"), "cursor restore missing: {raw:?}");
    }

    fn wait_for_file(path: &Path, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(3);
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
}
