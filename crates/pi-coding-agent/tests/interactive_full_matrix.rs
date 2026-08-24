//! Deterministic real-terminal coverage for the complete interactive surface.
//!
//! The `upstream_pi` oracle is pinned locally at
//! `5cd93f688aaab89dbb6dfa4aca535f21796ae185`. Its interactive contract is
//! exercised here at the user-visible boundary: raw mode and cursor state are
//! inspected through the pane tty, terminal bytes are captured with tmux, and
//! commands/control input are sent through the actual editor event loop.

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
        html: PathBuf,
        import: PathBuf,
        missing: PathBuf,
        raw_log: PathBuf,
    }

    impl Sandbox {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("pi-interactive-full-{}", uuid::Uuid::new_v4()));
            let home = root.join("home");
            let agent_dir = home.join(".pi").join("agent");
            let project = root.join("project");
            let html = root.join("session.html");
            let import = root.join("import.jsonl");
            let missing = root.join("missing.jsonl");
            let raw_log = root.join("tmux-output.log");

            fs::create_dir_all(&agent_dir).unwrap();
            fs::create_dir_all(&project).unwrap();
            fs::write(
                &import,
                include_str!("fixtures/interactive-full/import_session.jsonl"),
            )
            .unwrap();

            Self {
                root,
                home,
                agent_dir,
                project,
                html,
                import,
                missing,
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
        fn start(sandbox: &Sandbox, model: &str) -> Self {
            let name = format!("pi-interactive-full-{}", uuid::Uuid::new_v4());
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

            let command = format!(
                "env HOME={} PI_CODING_AGENT_DIR={} PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 PI_SHARE_DRY_RUN=1 {} --approve --provider faux --model {}",
                shell_quote(&sandbox.home),
                shell_quote(&sandbox.agent_dir),
                shell_quote(Path::new(env!("CARGO_BIN_EXE_pi"))),
                shell_quote(Path::new(model)),
            );
            let configured = tmux(&["set-option", "-t", &name, "remain-on-exit", "on"]);
            assert!(
                configured.status.success(),
                "tmux remain-on-exit setup failed: {}",
                stderr(&configured)
            );

            let command = format!("{command}; exec tail -f /dev/null");
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

        fn wait_for_capture<F>(&self, mut predicate: F) -> String
        where
            F: FnMut(&str) -> bool,
        {
            self.try_wait_for_capture(Duration::from_secs(8), &mut predicate)
                .unwrap_or_else(|| {
                    panic!(
                        "timed out waiting for PTY output; last capture:\n{}",
                        self.capture()
                    )
                })
        }

        fn try_wait_for_capture<F>(&self, timeout: Duration, predicate: &mut F) -> Option<String>
        where
            F: FnMut(&str) -> bool,
        {
            let deadline = Instant::now() + timeout;
            loop {
                let capture = self.capture();
                if predicate(&capture) {
                    return Some(capture);
                }
                if Instant::now() >= deadline {
                    return None;
                }
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

        fn resize(&self, width: &str, height: &str) {
            let output = tmux(&["resize-window", "-t", &self.name, "-x", width, "-y", height]);
            assert!(
                output.status.success(),
                "tmux resize-window failed: {}",
                stderr(&output)
            );
        }

        fn pane_size(&self) -> (u16, u16) {
            let output = tmux(&[
                "display-message",
                "-p",
                "-t",
                &self.name,
                "#{pane_width}x#{pane_height}",
            ]);
            assert!(
                output.status.success(),
                "tmux display-message failed: {}",
                stderr(&output)
            );
            let value = String::from_utf8_lossy(&output.stdout);
            let (width, height) = value
                .trim()
                .split_once('x')
                .unwrap_or_else(|| panic!("invalid tmux pane size: {value:?}"));
            (
                width.parse().expect("tmux width must be numeric"),
                height.parse().expect("tmux height must be numeric"),
            )
        }

        fn wait_for_size(&self, expected: (u16, u16)) {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                if self.pane_size() == expected {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for pane size {expected:?}; got {:?}",
                    self.pane_size()
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

        fn stty_state(&self) -> String {
            let tty = self.pane_tty();
            let output = Command::new("stty")
                .args(["-a", "-F", &tty])
                .output()
                .expect("stty must be installed for PTY mode inspection");
            assert!(
                output.status.success(),
                "stty failed for {tty}: {}",
                stderr(&output)
            );
            String::from_utf8_lossy(&output.stdout).to_lowercase()
        }

        fn has_stty_flag(state: &str, flag: &str) -> bool {
            state.split_whitespace().any(|token| token == flag)
        }

        fn assert_raw_mode(&self) {
            let state = self.stty_state();
            assert!(
                Self::has_stty_flag(&state, "-icanon"),
                "PTY is not in raw mode: {state}"
            );
            assert!(
                Self::has_stty_flag(&state, "-echo"),
                "PTY echo was not disabled: {state}"
            );
        }

        fn wait_for_cooked_mode(&self) {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let state = self.stty_state();
                if Self::has_stty_flag(&state, "icanon") && Self::has_stty_flag(&state, "echo") {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "PTY did not return to cooked mode: {state}"
                );
                thread::sleep(Duration::from_millis(25));
            }
        }

        fn wait_for_raw_contains(&self, path: &Path, needle: &str) -> String {
            self.try_wait_for_raw_contains(path, needle, Duration::from_secs(5))
                .unwrap_or_else(|| {
                    panic!(
                        "timed out waiting for raw PTY sequence {needle:?}; last raw output: {:?}",
                        fs::read_to_string(path).unwrap_or_default()
                    )
                })
        }

        fn wait_for_raw_growth(&self, path: &Path, previous_len: u64) {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let current_len = fs::metadata(path)
                    .map(|metadata| metadata.len())
                    .unwrap_or(0);
                if current_len > previous_len {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for PTY redraw after resize; raw output length stayed at {previous_len}"
                );
                thread::sleep(Duration::from_millis(25));
            }
        }

        fn try_wait_for_raw_contains(
            &self,
            path: &Path,
            needle: &str,
            timeout: Duration,
        ) -> Option<String> {
            let deadline = Instant::now() + timeout;
            loop {
                let raw = fs::read_to_string(path).unwrap_or_default();
                if raw.contains(needle) {
                    return Some(raw);
                }
                if Instant::now() >= deadline {
                    return None;
                }
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

    fn tmux(args: &[&str]) -> Output {
        Command::new("tmux")
            .args(args)
            .output()
            .expect("tmux must be installed for the interactive PTY matrix")
    }

    fn stderr(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).trim().to_string()
    }

    fn shell_quote(path: &Path) -> String {
        let value = path.to_string_lossy();
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }

    fn fixture_rows(text: &str, fields: usize) -> Vec<Vec<String>> {
        text.lines()
            .enumerate()
            .filter_map(|(line_number, line)| {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    return None;
                }
                let mut row: Vec<String> = line
                    .splitn(fields, '|')
                    .map(|field| field.trim().to_string())
                    .collect();
                assert!(
                    row.len() == fields || (fields == 3 && row.len() == 2),
                    "fixture row {line_number} must have {fields} fields: {line}"
                );
                while row.len() < fields {
                    row.push(String::new());
                }
                Some(row)
            })
            .collect()
    }

    fn expand_fixture_value(value: &str, sandbox: &Sandbox) -> String {
        value
            .replace("$HTML", &sandbox.html.to_string_lossy())
            .replace("$IMPORT", &sandbox.import.to_string_lossy())
            .replace("$MISSING", &sandbox.missing.to_string_lossy())
    }

    fn contains_visible_text(capture: &str, expected: &str) -> bool {
        let compact_capture: String = capture.split_whitespace().collect();
        let compact_expected: String = expected.split_whitespace().collect();
        compact_capture.contains(&compact_expected)
    }

    fn assert_terminal_restored(session: &TmuxSession, sandbox: &Sandbox) {
        let raw = session.wait_for_raw_contains(&sandbox.raw_log, "\x1b[?1049l");
        assert!(raw.contains("\x1b[?25h"), "cursor restore missing: {raw:?}");
        session.stop_pipe();
        let raw = fs::read_to_string(&sandbox.raw_log).unwrap_or_default();
        let tmux_mouse_enable = "\x1b[?1000h\x1b[?1002h\x1b[?1004h\x1b[?1006h";
        let tmux_mouse_disable = "\x1b[?1006l\x1b[?1004l\x1b[?1003l\x1b[?1002l\x1b[?1000l";
        assert!(
            raw.contains("\x1b[?1049h"),
            "alternate-screen entry missing"
        );
        assert!(raw.contains("\x1b[?1049l"), "alternate-screen exit missing");
        assert!(raw.contains("\x1b[?25l"), "cursor hide missing");
        assert!(raw.contains("\x1b[?25h"), "cursor restore missing");
        assert!(
            raw.contains(tmux_mouse_enable),
            "tmux button-motion capability bytes missing: {raw:?}"
        );
        assert!(
            !raw.contains("\x1b[?1003h"),
            "tmux unexpectedly enabled all-motion mouse reporting: {raw:?}"
        );
        assert!(
            raw.contains(tmux_mouse_disable),
            "tmux mouse cleanup bytes missing: {raw:?}"
        );
        assert!(raw.contains("\x1b[?7l"), "autowrap disable missing");
        assert!(raw.contains("\x1b[?7h"), "autowrap restore missing");
        assert!(
            raw.contains("\x1b[?2004h"),
            "bracketed-paste enable missing"
        );
        assert!(
            raw.contains("\x1b[?2004l"),
            "bracketed-paste cleanup missing"
        );
        assert!(raw.contains("\x1b[?2026h"), "sync-update begin missing");
        assert!(raw.contains("\x1b[?2026l"), "sync-update end missing");
        session.wait_for_cooked_mode();
    }

    #[test]
    fn full_slash_command_matrix_covers_terminal_lifecycle_and_resize() {
        let sandbox = Sandbox::new();
        let session = TmuxSession::start(&sandbox, "faux-1");
        session.wait_for_capture(|capture| capture.contains("(faux/Faux Model)"));
        session.assert_raw_mode();

        let startup_raw = session.wait_for_raw_contains(&sandbox.raw_log, "\x1b[?1049h");
        assert!(
            startup_raw.contains("\x1b[?25l"),
            "cursor hide was not emitted on startup: {startup_raw:?}"
        );

        let raw_before_resize = fs::metadata(&sandbox.raw_log)
            .expect("startup raw log must exist")
            .len();
        session.resize("72", "18");
        session.wait_for_size((72, 18));
        session.wait_for_raw_growth(&sandbox.raw_log, raw_before_resize);

        for (fixture_index, row) in fixture_rows(
            include_str!("fixtures/interactive-full/slash_commands.txt"),
            3,
        )
        .into_iter()
        .enumerate()
        {
            let command = expand_fixture_value(&row[0], &sandbox);
            let expected = expand_fixture_value(&row[1], &sandbox);

            if row[0] == "/clear" {
                session.send_line("matrix prompt");
                session.wait_for_capture(|capture| {
                    capture.contains("faux response to: matrix prompt")
                });
            }

            let before = session.capture();
            session.send_line(&command);
            let mut matches = |capture: &str| {
                if expected == "__clear_transcript__" {
                    capture != before && !capture.contains("faux response to: matrix prompt")
                } else {
                    capture != before && contains_visible_text(capture, &expected)
                }
            };
            assert!(
                session
                    .try_wait_for_capture(Duration::from_secs(8), &mut matches)
                    .is_some(),
                "timed out on slash fixture row {fixture_index}: command={command:?}, expected={expected:?}, follow_up={:?}; last capture:\n{}",
                row.get(2),
                session.capture()
            );

            if row.get(2).is_some_and(|follow_up| follow_up == "escape") {
                session.send_key("Escape");
                thread::sleep(Duration::from_millis(100));
            }
        }

        assert!(
            sandbox.html.exists(),
            "the /export fixture did not create {}",
            sandbox.html.display()
        );
        let html = fs::read_to_string(&sandbox.html).unwrap();
        assert!(html.contains("<html"), "exported artifact is not HTML");

        session.send_line("/quit");
        assert_terminal_restored(&session, &sandbox);
    }

    #[test]
    fn control_key_matrix_restores_terminal_after_interrupt_and_eof_input() {
        for row in fixture_rows(
            include_str!("fixtures/interactive-full/termination_paths.txt"),
            3,
        ) {
            let sandbox = Sandbox::new();
            let session = TmuxSession::start(&sandbox, "faux-1");
            session.wait_for_capture(|capture| capture.contains("(faux/Faux Model)"));
            session.assert_raw_mode();

            session.send_key(&row[0]);
            session.wait_for_raw_contains(&sandbox.raw_log, "\x1b[?1049l");

            assert_terminal_restored(&session, &sandbox);
            assert_eq!(row[2], "raw terminal restored");
        }
    }

    #[test]
    fn startup_error_path_never_enters_raw_mode() {
        for row in fixture_rows(include_str!("fixtures/interactive-full/error_paths.txt"), 3) {
            let sandbox = Sandbox::new();
            let session = TmuxSession::start(&sandbox, &row[0]);
            let expected = expand_fixture_value(&row[1], &sandbox);
            let capture =
                session.wait_for_capture(|capture| contains_visible_text(capture, &expected));
            assert!(
                contains_visible_text(&capture, &expected),
                "startup error missing from pane: expected={expected:?}, capture={capture}"
            );

            let forbidden = row[2].replace("\\x1b", "\x1b");
            let raw = fs::read_to_string(&sandbox.raw_log).unwrap_or_default();
            assert!(
                !raw.contains(&forbidden),
                "startup error entered terminal mode unexpectedly: {raw:?}"
            );
            session.stop_pipe();
            session.wait_for_cooked_mode();
        }
    }
}
