#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! End-to-end PTY matrix for the user-visible Rust Pi terminal application.
//!
//! These tests deliberately drive the compiled `pi` binary through a real
//! tmux-backed PTY.  Every normal case uses an isolated HOME, agent root,
//! session root, and project directory; no process-global configuration is
//! changed.  The live-provider case is separate and ignored by default so an
//! offline test run never silently turns into a network or credential test.

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output, Stdio};
    use std::sync::{Mutex, OnceLock};
    use std::thread;
    use std::time::{Duration, Instant};

    const STARTUP_TIMEOUT: Duration = Duration::from_secs(12);
    const TURN_TIMEOUT: Duration = Duration::from_secs(18);
    // Four 25 ms tmux capture polls cover one 60 Hz frame plus PTY polling
    // jitter while still catching an owner-loop stall behind scene work.
    const OWNER_LOOP_ECHO_THRESHOLD: Duration = Duration::from_millis(100);

    fn pty_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn has_pty_support() -> bool {
        if fs::metadata("/dev/ptmx").is_err() {
            return false;
        }
        Command::new("tmux")
            .arg("-V")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn skip_without_pty() -> bool {
        if has_pty_support() {
            return false;
        }
        eprintln!(
            "SKIP: interactive_real_pty_matrix requires Unix /dev/ptmx and the tmux executable"
        );
        true
    }

    fn test_binary() -> PathBuf {
        std::env::var_os("PI_RUST_TEST_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pi")))
    }

    struct Sandbox {
        root: PathBuf,
        home: PathBuf,
        xdg_config: PathBuf,
        xdg_data: PathBuf,
        agent_dir: PathBuf,
        sessions: PathBuf,
        project: PathBuf,
        raw_log: PathBuf,
    }

    impl Sandbox {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "pi-interactive-real-pty-{label}-{}",
                uuid::Uuid::new_v4()
            ));
            let home = root.join("home");
            let xdg_config = root.join("xdg-config");
            let xdg_data = root.join("xdg-data");
            let agent_dir = root.join("agent");
            let sessions = root.join("sessions");
            let project = root.join("project");
            let raw_log = root.join("pty.raw");
            for directory in [
                &home,
                &xdg_config,
                &xdg_data,
                &agent_dir,
                &sessions,
                &project,
            ] {
                fs::create_dir_all(directory).expect("create isolated PTY directory");
            }
            Self {
                root,
                home,
                xdg_config,
                xdg_data,
                agent_dir,
                sessions,
                project,
                raw_log,
            }
        }

        fn session_text(&self) -> String {
            let mut paths = Vec::new();
            collect_jsonl_files(&self.sessions, &mut paths);
            paths
                .into_iter()
                .filter_map(|path| fs::read_to_string(path).ok())
                .collect::<Vec<_>>()
                .join("\n")
        }

        fn copy_auth(&self, source: &Path) {
            fs::copy(source, self.agent_dir.join("auth.json"))
                .expect("copy operator auth into disposable agent root");
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
        fn start_faux(sandbox: &Sandbox, width: u16, height: u16, continue_session: bool) -> Self {
            Self::start(
                sandbox,
                width,
                height,
                "faux",
                "faux-1",
                continue_session,
                false,
                None,
            )
        }

        fn start_faux_streaming(sandbox: &Sandbox, width: u16, height: u16) -> Self {
            Self::start(
                sandbox,
                width,
                height,
                "faux",
                "faux-1",
                false,
                false,
                Some(2),
            )
        }

        fn start_live(sandbox: &Sandbox, width: u16, height: u16) -> Self {
            Self::start(
                sandbox,
                width,
                height,
                "openai-codex",
                "gpt-5.5",
                false,
                true,
                None,
            )
        }

        #[allow(clippy::too_many_arguments)]
        fn start(
            sandbox: &Sandbox,
            width: u16,
            height: u16,
            provider: &str,
            model: &str,
            continue_session: bool,
            live_provider: bool,
            faux_stream_delay_ms: Option<u64>,
        ) -> Self {
            let name = format!("pi-real-pty-{}", uuid::Uuid::new_v4());
            let width_arg = width.to_string();
            let height_arg = height.to_string();
            let created = tmux(&[
                "new-session",
                "-d",
                "-x",
                &width_arg,
                "-y",
                &height_arg,
                "-c",
                sandbox
                    .project
                    .to_str()
                    .expect("isolated project path is UTF-8"),
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

            let pipe = format!("cat > {}", shell_quote(&sandbox.raw_log));
            let piped = tmux(&["pipe-pane", "-o", "-t", &name, &pipe]);
            assert!(
                piped.status.success(),
                "tmux pipe-pane failed: {}",
                stderr(&piped)
            );

            let offline = if live_provider {
                String::new()
            } else {
                " PI_OFFLINE=1".to_string()
            };
            let continue_flag = if continue_session {
                " --continue".to_string()
            } else {
                String::new()
            };
            let no_tools = if live_provider { " --no-tools" } else { "" };
            let faux_stream_delay = faux_stream_delay_ms
                .map(|delay_ms| format!(" PI_RUST_INTERACTIVE_FAUX_STREAM_DELAY_MS={delay_ms}"))
                .unwrap_or_default();
            let binary = test_binary();
            assert!(
                binary.is_file(),
                "pi test binary does not exist: {}",
                binary.display()
            );
            let command = format!(
                "env -i HOME={} XDG_CONFIG_HOME={} XDG_DATA_HOME={} PI_CODING_AGENT_DIR={} PI_CODING_AGENT_SESSION_DIR={} PI_SKIP_VERSION_CHECK=1 PI_TELEMETRY=0 PI_SHARE_DRY_RUN=1 PI_DISABLE_OSC52=1 TERM=xterm-256color COLORTERM=truecolor LANG=C.UTF-8 LC_ALL=C.UTF-8 PATH=/usr/bin:/bin{}{} {} --approve --provider {} --model {} --thinking off --tui-mode fullscreen --session-dir {} --no-extensions --no-skills --no-prompt-templates --no-themes --no-context-files{}{}; exec tail -f /dev/null",
                shell_quote(&sandbox.home),
                shell_quote(&sandbox.xdg_config),
                shell_quote(&sandbox.xdg_data),
                shell_quote(&sandbox.agent_dir),
                shell_quote(&sandbox.sessions),
                offline,
                faux_stream_delay,
                shell_quote(&binary),
                shell_quote_text(provider),
                shell_quote_text(model),
                shell_quote(&sandbox.sessions),
                no_tools,
                continue_flag,
            );
            let configured = tmux(&["set-option", "-t", &name, "remain-on-exit", "on"]);
            assert!(
                configured.status.success(),
                "tmux remain-on-exit setup failed: {}",
                stderr(&configured)
            );
            let launched = tmux(&["respawn-pane", "-k", "-t", &name, &command]);
            assert!(
                launched.status.success(),
                "tmux respawn-pane launch failed: {}",
                stderr(&launched)
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

        fn capture_history(&self) -> String {
            let output = tmux(&["capture-pane", "-p", "-S", "-", "-t", &self.name]);
            assert!(
                output.status.success(),
                "tmux history capture failed: {}",
                stderr(&output)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        }

        #[track_caller]
        fn wait_for<F>(&self, timeout: Duration, mut predicate: F) -> String
        where
            F: FnMut(&str) -> bool,
        {
            let deadline = Instant::now() + timeout;
            loop {
                let capture = self.capture();
                if predicate(&capture) {
                    return capture;
                }
                if Instant::now() >= deadline {
                    panic!("timed out waiting for PTY output; last capture:\n{capture}");
                }
                thread::sleep(Duration::from_millis(25));
            }
        }

        fn wait_for_faux_ready(&self) -> String {
            self.wait_for(STARTUP_TIMEOUT, |capture| {
                // Older snapshots rendered `(faux/Faux Model)`; the current
                // Rust footer renders the canonical model id `faux-1`.
                capture.contains("(faux/Faux Model)") || capture.contains("faux-1")
            })
        }

        #[track_caller]
        fn wait_for_history<F>(&self, timeout: Duration, mut predicate: F) -> String
        where
            F: FnMut(&str) -> bool,
        {
            let deadline = Instant::now() + timeout;
            loop {
                let capture = self.capture_history();
                if predicate(&capture) {
                    return capture;
                }
                if Instant::now() >= deadline {
                    panic!("timed out waiting for PTY history; last capture:\n{capture}");
                }
                thread::sleep(Duration::from_millis(25));
            }
        }

        #[track_caller]
        fn wait_for_raw<F>(&self, path: &Path, timeout: Duration, mut predicate: F) -> String
        where
            F: FnMut(&str) -> bool,
        {
            let deadline = Instant::now() + timeout;
            loop {
                let raw = fs::read_to_string(path).unwrap_or_default();
                if predicate(&raw) {
                    return raw;
                }
                if Instant::now() >= deadline {
                    panic!(
                        "timed out waiting for raw PTY output; last byte count={}",
                        raw.len()
                    );
                }
                thread::sleep(Duration::from_millis(25));
            }
        }

        fn send_text(&self, text: &str) {
            let output = tmux(&["send-keys", "-t", &self.name, "-l", "--", text]);
            assert!(
                output.status.success(),
                "tmux literal input failed: {}",
                stderr(&output)
            );
        }

        fn send_key(&self, key: &str) {
            let output = tmux(&["send-keys", "-t", &self.name, key]);
            assert!(
                output.status.success(),
                "tmux key {key:?} failed: {}",
                stderr(&output)
            );
        }

        fn send_line(&self, line: &str) {
            self.send_text(line);
            thread::sleep(Duration::from_millis(20));
            self.send_key("Enter");
        }

        fn send_alt_enter(&self) {
            // With Kitty keyboard protocol active, Pi's official matcher
            // requires the CSI-u Alt+Enter identity.  tmux's `M-Enter`
            // helper emits the ambiguous legacy ESC+CR mapping, which Pi
            // intentionally reserves for Shift+Enter in this mode.
            self.send_text("\x1b[13;3u");
        }

        fn send_bracketed_paste(&self, text: &str) {
            let mut loader = Command::new("tmux")
                .args(["load-buffer", "-"])
                .stdin(Stdio::piped())
                .spawn()
                .expect("tmux load-buffer must start");
            loader
                .stdin
                .take()
                .expect("tmux load-buffer stdin")
                .write_all(text.as_bytes())
                .expect("write bracketed paste data");
            let loaded = loader.wait().expect("wait for tmux load-buffer");
            assert!(loaded.success(), "tmux load-buffer failed");
            let pasted = tmux(&["paste-buffer", "-p", "-d", "-t", &self.name]);
            assert!(
                pasted.status.success(),
                "tmux bracketed paste failed: {}",
                stderr(&pasted)
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
                "tmux pane-size query failed: {}",
                stderr(&output)
            );
            let value = String::from_utf8_lossy(&output.stdout);
            let (width, height) = value
                .trim()
                .split_once('x')
                .unwrap_or_else(|| panic!("invalid tmux pane size: {value:?}"));
            (
                width.parse().expect("tmux pane width must be numeric"),
                height.parse().expect("tmux pane height must be numeric"),
            )
        }

        fn wait_for_size(&self, expected: (u16, u16)) {
            let deadline = Instant::now() + Duration::from_secs(4);
            loop {
                if self.pane_size() == expected {
                    return;
                }
                if Instant::now() >= deadline {
                    panic!(
                        "timed out waiting for pane size {expected:?}; got {:?}",
                        self.pane_size()
                    );
                }
                thread::sleep(Duration::from_millis(25));
            }
        }

        fn pane_tty(&self) -> String {
            let output = tmux(&["display-message", "-p", "-t", &self.name, "#{pane_tty}"]);
            assert!(
                output.status.success(),
                "tmux pane-tty query failed: {}",
                stderr(&output)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }

        fn stty_state(&self) -> String {
            let tty = self.pane_tty();
            let output = Command::new("stty")
                .args(["-a", "-F", &tty])
                .output()
                .expect("stty must be installed for PTY mode assertions");
            assert!(
                output.status.success(),
                "stty failed for {tty}: {}",
                stderr(&output)
            );
            String::from_utf8_lossy(&output.stdout).to_lowercase()
        }

        fn assert_raw_mode(&self) {
            let state = self.stty_state();
            assert!(
                state.split_whitespace().any(|flag| flag == "-icanon"),
                "PTY did not enter raw mode: {state}"
            );
            assert!(
                state.split_whitespace().any(|flag| flag == "-echo"),
                "PTY echo was not disabled: {state}"
            );
        }

        fn wait_for_cooked_mode(&self) {
            let deadline = Instant::now() + Duration::from_secs(6);
            loop {
                let state = self.stty_state();
                if state.split_whitespace().any(|flag| flag == "icanon")
                    && state.split_whitespace().any(|flag| flag == "echo")
                {
                    return;
                }
                if Instant::now() >= deadline {
                    panic!("PTY did not return to cooked mode: {state}");
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

        fn quit(&self, sandbox: &Sandbox) {
            // A history predicate can observe the assistant response on the
            // same render tick that the streaming task is settling. Give the
            // owner loop one scheduling window before handing it `/quit`, so
            // shutdown tests exercise the idle command path rather than
            // queueing the quit text as a follow-up.
            thread::sleep(Duration::from_millis(150));
            self.send_line("/quit");
            self.wait_for_raw(sandbox.raw_log.as_path(), STARTUP_TIMEOUT, |raw| {
                raw.contains("\x1b[?1049l")
            });
            self.wait_for_cooked_mode();
            self.stop_pipe();
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
            .expect("tmux must be installed for the real PTY matrix")
    }

    fn stderr(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).trim().to_string()
    }

    fn shell_quote(path: &Path) -> String {
        shell_quote_text(&path.to_string_lossy())
    }

    fn shell_quote_text(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }

    fn visible(text: &str) -> String {
        pi_tui::strip_terminal_sequences(text)
            .replace("\r\n", "\n")
            .replace('\r', "\n")
    }

    fn assert_no_json_envelope(text: &str) {
        let text = visible(text);
        for forbidden in [
            "```json",
            "\"type\":\"toolCall\"",
            "\"type\": \"toolCall\"",
            "\"role\":\"bashExecution\"",
            "\"role\": \"bashExecution\"",
            "\"excludeFromContext\"",
        ] {
            assert!(
                !text.contains(forbidden),
                "normal TUI exposed a serialized JSON envelope marker {forbidden:?}:\n{text}"
            );
        }
    }

    fn collect_jsonl_files(path: &Path, output: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_jsonl_files(&path, output);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            {
                output.push(path);
            }
        }
        output.sort();
    }

    fn assert_valid_jsonl(text: &str) -> Vec<serde_json::Value> {
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .enumerate()
            .map(|(line_number, line)| {
                serde_json::from_str(line).unwrap_or_else(|error| {
                    panic!(
                        "session JSONL line {} was invalid: {error}: {line}",
                        line_number + 1
                    )
                })
            })
            .collect()
    }

    fn json_contains_string(value: &serde_json::Value, expected: &str) -> bool {
        match value {
            serde_json::Value::String(string) => string.contains(expected),
            serde_json::Value::Array(values) => values
                .iter()
                .any(|value| json_contains_string(value, expected)),
            serde_json::Value::Object(object) => object
                .values()
                .any(|value| json_contains_string(value, expected)),
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
                false
            }
        }
    }

    fn bash_record(value: &serde_json::Value, command_fragment: &str) -> Option<bool> {
        match value {
            serde_json::Value::Object(object) => {
                if object.get("role").and_then(serde_json::Value::as_str) == Some("bashExecution")
                    && object
                        .get("command")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|command| command.contains(command_fragment))
                {
                    return Some(
                        object
                            .get("excludeFromContext")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false),
                    );
                }
                object
                    .values()
                    .find_map(|child| bash_record(child, command_fragment))
            }
            serde_json::Value::Array(values) => values
                .iter()
                .find_map(|child| bash_record(child, command_fragment)),
            serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::String(_) => None,
        }
    }

    fn find_operator_auth() -> Option<PathBuf> {
        let mut candidates = Vec::new();
        if let Some(agent) = std::env::var_os("PI_CODING_AGENT_DIR") {
            candidates.push(PathBuf::from(agent).join("auth.json"));
        }
        if let Some(home) = std::env::var_os("HOME") {
            candidates.push(PathBuf::from(home).join(".pi/agent/auth.json"));
        }
        candidates
            .into_iter()
            .find(|path| fs::metadata(path).is_ok())
    }

    #[test]
    fn pty_launch_and_render_matrix_at_80x24_100x30_and_160x50() {
        let _guard = pty_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if skip_without_pty() {
            return;
        }
        for (width, height) in [(80, 24), (100, 30), (160, 50)] {
            let sandbox = Sandbox::new(&format!("launch-{width}x{height}"));
            let session = TmuxSession::start_faux(&sandbox, width, height, false);
            let startup = session.wait_for_faux_ready();
            session.wait_for_size((width, height));
            session.assert_raw_mode();
            let raw = session.wait_for_raw(&sandbox.raw_log, STARTUP_TIMEOUT, |raw| {
                raw.contains("\x1b[?1049h") && raw.contains("\x1b[?25l")
            });
            assert!(
                raw.contains("\x1b[?2004h"),
                "bracketed paste was not enabled"
            );
            assert!(
                raw.contains("\x1b[?2026h"),
                "synchronized updates were not enabled"
            );
            let startup = visible(&startup);
            assert!(
                startup.contains("Faux Model") || startup.contains("faux-1"),
                "model was not rendered: {startup}"
            );
            assert!(
                !startup.contains("Update available"),
                "pi-rust displayed an upstream Pi update notice: {startup}"
            );
            assert_no_json_envelope(&startup);
            session.quit(&sandbox);
        }
    }

    #[test]
    fn pty_composer_supports_unicode_editing_and_multiline_input() {
        let _guard = pty_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if skip_without_pty() {
            return;
        }
        let sandbox = Sandbox::new("composer");
        let session = TmuxSession::start_faux(&sandbox, 100, 30, false);
        session.wait_for_faux_ready();
        session.assert_raw_mode();

        session.send_text("MATRIX_EDIT_SHOULD_BE_REPLACED");
        session.wait_for(STARTUP_TIMEOUT, |capture| {
            capture.contains("MATRIX_EDIT_SHOULD_BE_REPLACED")
        });
        session.send_key("C-u");
        // Avoid markdown emphasis markers in the prompt itself so this case
        // isolates editor Unicode submission and transcript persistence.
        let unicode_prompt = "MATRIX-UNICODE-日本語-🙂";
        session.send_text(unicode_prompt);
        session.wait_for(STARTUP_TIMEOUT, |capture| {
            capture.contains("MATRIX-UNICODE-日本語-🙂")
        });
        session.send_key("Enter");
        session.wait_for_history(TURN_TIMEOUT, |capture| {
            capture.contains("faux response to: MATRIX-UNICODE-日本語-🙂")
        });

        let multiline = "MATRIX-MULTI-FIRST\nMATRIX-MULTI-SECOND";
        session.send_bracketed_paste(multiline);
        session.wait_for(STARTUP_TIMEOUT, |capture| {
            capture.contains("MATRIX-MULTI-SECOND")
        });
        session.send_key("Enter");
        session.wait_for_history(TURN_TIMEOUT, |capture| {
            capture.contains("MATRIX-MULTI-FIRST") && capture.contains("MATRIX-MULTI-SECOND")
        });

        let screen = session.capture_history();
        assert_no_json_envelope(&screen);
        session.quit(&sandbox);
        let transcript = sandbox.session_text();
        assert_valid_jsonl(&transcript);
        assert!(
            transcript.contains(unicode_prompt),
            "Unicode prompt was not persisted"
        );
        assert!(
            transcript.contains("MATRIX-MULTI-FIRST") && transcript.contains("MATRIX-MULTI-SECOND"),
            "multiline prompt was not persisted: {transcript}"
        );
    }

    #[test]
    fn pty_rapid_unicode_burst_reaches_local_echo_without_accumulated_lag() {
        let _guard = pty_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if skip_without_pty() {
            return;
        }
        let sandbox = Sandbox::new("composer-latency");
        let session = TmuxSession::start_faux(&sandbox, 100, 30, false);
        session.wait_for_faux_ready();

        // This is a real tmux-backed PTY burst.  The timestamp is taken before
        // tmux writes the bracketed paste, and the predicate observes the
        // terminal's local echo rather than a provider response.
        let burst = format!(
            "MATRIX-LATENCY-{}-日本語-🙂\nMATRIX-LATENCY-TAIL",
            "rapid-".repeat(700)
        );
        let started = Instant::now();
        session.send_bracketed_paste(&burst);
        let echoed = session.wait_for(Duration::from_secs(3), |capture| {
            let visible = visible(capture);
            // Upstream intentionally replaces a large bracketed paste with a
            // prompt-local marker. The original payload is restored only when
            // the editor submits the prompt.
            visible.contains("[paste #1 ") && visible.contains(" chars]")
        });
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(1000),
            "local echo accumulated {elapsed:?} for a {}-byte Unicode/multiline burst: {echoed}",
            burst.len()
        );

        session.send_key("Enter");
        let expanded = session.wait_for_history(TURN_TIMEOUT, |capture| {
            capture.contains("MATRIX-LATENCY-TAIL")
        });
        assert_no_json_envelope(&expanded);
        session.quit(&sandbox);
        let transcript = sandbox.session_text();
        let records = assert_valid_jsonl(&transcript);
        assert!(
            records
                .iter()
                .any(|record| json_contains_string(record, &burst)),
            "submitted PTY prompt was not persisted with its expanded Unicode/multiline payload; marker echo was: {echoed}"
        );
    }

    #[test]
    fn pty_idle_composer_echoes_each_printable_key_within_one_frame() {
        let _guard = pty_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if skip_without_pty() {
            return;
        }
        let sandbox = Sandbox::new("owner-loop-latency");
        // This is an authenticated-provider-shaped interactive launch, but it
        // deliberately submits no turn. The assertion therefore measures only
        // the real tmux/PTY -> owner-loop -> composer repaint path, without a
        // faux response being used as live evidence.
        let session = TmuxSession::start_live(&sandbox, 100, 30);
        session.wait_for(STARTUP_TIMEOUT, |capture| {
            capture.contains("gpt-5.5") || capture.contains("openai-codex")
        });

        let probe = "MATRIX-OWNER-Δ";
        let mut expected = String::new();
        let mut samples = Vec::new();
        for character in probe.chars() {
            expected.push(character);
            let started = Instant::now();
            session.send_text(&character.to_string());
            let capture = session.wait_for(Duration::from_millis(250), |capture| {
                visible(capture).contains(&expected)
            });
            let elapsed = started.elapsed();
            samples.push(elapsed);
            assert!(
                elapsed <= OWNER_LOOP_ECHO_THRESHOLD,
                "owner-loop composer key {character:?} took {elapsed:?}; expected {expected:?}; last capture:\n{capture}"
            );
        }
        assert!(
            samples
                .iter()
                .all(|elapsed| *elapsed <= OWNER_LOOP_ECHO_THRESHOLD),
            "idle composer repaint exceeded the one-frame-plus-PTY budget: {samples:?}"
        );
        // Clear the draft with the first real Ctrl+C before `quit` sends its
        // `/quit` command; otherwise the command is appended to the probe and
        // submitted as a normal prompt.
        session.send_key("C-c");
        let cleared = session.wait_for(Duration::from_millis(250), |capture| {
            !visible(capture).contains(probe)
        });
        assert!(
            !visible(&cleared).contains(probe),
            "Ctrl+C did not clear the idle composer draft: {cleared}"
        );
        session.quit(&sandbox);
    }

    #[test]
    fn pty_settings_toggle_updates_the_live_panel_and_persists() {
        let _guard = pty_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if skip_without_pty() {
            return;
        }
        let sandbox = Sandbox::new("settings-toggle");
        fs::write(
            sandbox.agent_dir.join("settings.json"),
            r#"{"compaction":{"enabled":false}}"#,
        )
        .expect("seed isolated settings file");
        let session = TmuxSession::start_faux(&sandbox, 100, 30, false);
        session.wait_for_faux_ready();

        session.send_line("/settings");
        let opened = session.wait_for(STARTUP_TIMEOUT, |capture| {
            let visible = visible(capture);
            visible.contains("Auto-compact") && visible.contains("false")
        });
        assert!(
            visible(&opened).contains("Auto-compact"),
            "settings panel did not open: {opened}"
        );

        // The first enabled row is Auto-compact. One real Enter press must
        // cycle false -> true and emit the live settings callback.
        session.send_key("Enter");
        let changed = session.wait_for(STARTUP_TIMEOUT, |capture| {
            let visible = visible(capture);
            visible.contains("Auto-compact") && visible.contains("true")
        });
        assert!(
            visible(&changed).contains("Auto-compact") && visible(&changed).contains("true"),
            "settings change was not reflected in the live panel: {changed}"
        );

        session.send_key("Escape");
        session.quit(&sandbox);

        let saved: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(sandbox.agent_dir.join("settings.json"))
                .expect("settings file should remain after quit"),
        )
        .expect("settings file should contain valid JSON");
        assert_eq!(
            saved
                .pointer("/compaction/enabled")
                .and_then(serde_json::Value::as_bool),
            Some(true),
            "live settings callback did not persist compaction.enabled: {saved}"
        );
    }

    #[test]
    fn pty_alt_enter_queues_a_follow_up_during_a_live_turn() {
        let _guard = pty_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if skip_without_pty() {
            return;
        }
        let sandbox = Sandbox::new("queue");
        let session = TmuxSession::start_faux_streaming(&sandbox, 100, 30);
        session.wait_for_faux_ready();

        // The faux provider yields at deterministic token boundaries.  A
        // sizeable first response keeps the real streaming loop active while
        // the Alt+Enter bytes are consumed by that loop, rather than by idle
        // composer handling after the turn has already ended.
        let first = format!("MATRIX-QUEUE-FIRST-{}", "x".repeat(16_000));
        session.send_bracketed_paste(&first);
        session.send_key("Enter");
        session.send_text("MATRIX-QUEUE-FOLLOWUP");
        session.send_alt_enter();
        session.wait_for(STARTUP_TIMEOUT, |capture| capture.contains("queued"));
        let settled = session.wait_for_history(TURN_TIMEOUT, |capture| {
            capture.contains("faux response to: MATRIX-QUEUE-FOLLOWUP")
        });
        assert_no_json_envelope(&settled);
        session.quit(&sandbox);
        let transcript = sandbox.session_text();
        assert_valid_jsonl(&transcript);
        assert!(transcript.contains("MATRIX-QUEUE-FIRST-"));
        assert!(transcript.contains("MATRIX-QUEUE-FOLLOWUP"));
    }

    #[test]
    fn pty_ctrl_c_clears_drafts_and_interrupts_a_running_bash_operation() {
        let _guard = pty_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if skip_without_pty() {
            return;
        }

        let draft_sandbox = Sandbox::new("ctrl-c-draft");
        let draft_session = TmuxSession::start_faux(&draft_sandbox, 100, 30, false);
        draft_session.wait_for_faux_ready();
        draft_session.send_text("MATRIX_DRAFT_TO_CLEAR");
        draft_session.wait_for(STARTUP_TIMEOUT, |capture| {
            capture.contains("MATRIX_DRAFT_TO_CLEAR")
        });
        draft_session.send_key("C-c");
        thread::sleep(Duration::from_millis(100));
        let cleared = draft_session.capture();
        assert!(
            !cleared.contains("MATRIX_DRAFT_TO_CLEAR"),
            "Ctrl+C left the draft visible"
        );
        assert!(
            !cleared.contains("Input cleared"),
            "Ctrl+C created a non-Pi status message"
        );
        draft_session.assert_raw_mode();
        draft_session.send_key("C-c");
        draft_session.wait_for_raw(&draft_sandbox.raw_log, STARTUP_TIMEOUT, |raw| {
            raw.contains("\x1b[?1049l")
        });
        draft_session.wait_for_cooked_mode();
        draft_session.stop_pipe();

        let bash_sandbox = Sandbox::new("ctrl-c-bash");
        let bash_session = TmuxSession::start_faux(&bash_sandbox, 100, 30, false);
        bash_session.wait_for_faux_ready();
        bash_session.send_line("!sleep 5");
        bash_session.wait_for(STARTUP_TIMEOUT, |capture| capture.contains("sleep 5"));
        bash_session.send_key("C-c");
        let cancelled = bash_session.wait_for(TURN_TIMEOUT, |capture| {
            capture.contains("cancelled") || capture.contains("✗")
        });
        assert_no_json_envelope(&cancelled);
        bash_session.send_line("MATRIX-AFTER-BASH-INTERRUPT");
        bash_session.wait_for_history(TURN_TIMEOUT, |capture| {
            capture.contains("faux response to: MATRIX-AFTER-BASH-INTERRUPT")
        });
        bash_session.quit(&bash_sandbox);
        let transcript = bash_sandbox.session_text();
        let values = assert_valid_jsonl(&transcript);
        assert_eq!(
            values
                .iter()
                .find_map(|value| bash_record(value, "sleep 5")),
            Some(false),
            "cancelled bash execution was not persisted as a visible-context record"
        );
        assert!(transcript.contains("MATRIX-AFTER-BASH-INTERRUPT"));
    }

    #[test]
    fn pty_slash_help_bash_and_bash_transcript_use_pi_style_projection() {
        let _guard = pty_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if skip_without_pty() {
            return;
        }
        let sandbox = Sandbox::new("slash-bash");
        let session = TmuxSession::start_faux(&sandbox, 100, 30, false);
        session.wait_for_faux_ready();

        session.send_line("/help");
        let unknown_command = session.wait_for_history(STARTUP_TIMEOUT, |capture| {
            capture.contains("faux response to: /help")
        });
        assert_no_json_envelope(&unknown_command);
        for command in ["/theme", "/clear", "/llama"] {
            session.send_line(command);
            let fallback = session.wait_for_history(STARTUP_TIMEOUT, |capture| {
                capture.contains(&format!("faux response to: {command}"))
            });
            assert_no_json_envelope(&fallback);
            assert!(
                !fallback.contains("commands:"),
                "{command} unexpectedly dispatched as a builtin:\n{fallback}"
            );
        }

        session.send_line("!printf MATRIX-BASH-VISIBLE");
        let bash = session.wait_for_history(TURN_TIMEOUT, |capture| {
            capture.contains("$ printf MATRIX-BASH-VISIBLE")
                && capture.contains("MATRIX-BASH-VISIBLE")
        });
        assert_no_json_envelope(&bash);

        session.send_line("!!printf MATRIX-BASH-EXCLUDED");
        let excluded = session.wait_for_history(TURN_TIMEOUT, |capture| {
            capture.contains("$ printf MATRIX-BASH-EXCLUDED")
                && capture.contains("MATRIX-BASH-EXCLUDED")
        });
        assert_no_json_envelope(&excluded);
        session.quit(&sandbox);

        let transcript = sandbox.session_text();
        let values = assert_valid_jsonl(&transcript);
        assert_eq!(
            values
                .iter()
                .find_map(|value| bash_record(value, "printf MATRIX-BASH-VISIBLE")),
            Some(false),
            "! command did not persist as a context-visible bashExecution"
        );
        assert_eq!(
            values
                .iter()
                .find_map(|value| bash_record(value, "printf MATRIX-BASH-EXCLUDED")),
            Some(true),
            "!! command did not persist with excludeFromContext=true"
        );
        assert!(!transcript.contains("\"type\":\"toolCall\""));
    }

    #[test]
    fn pty_persistence_survives_restart_with_continue() {
        let _guard = pty_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if skip_without_pty() {
            return;
        }
        let sandbox = Sandbox::new("restart");
        let first = TmuxSession::start_faux(&sandbox, 100, 30, false);
        first.wait_for_faux_ready();
        first.send_line("MATRIX-PERSIST-FIRST");
        first.wait_for_history(TURN_TIMEOUT, |capture| {
            capture.contains("faux response to: MATRIX-PERSIST-FIRST")
        });
        first.quit(&sandbox);

        let before_restart = sandbox.session_text();
        let before_values = assert_valid_jsonl(&before_restart);
        assert!(!before_values.is_empty());
        assert!(before_restart.contains("MATRIX-PERSIST-FIRST"));

        let second = TmuxSession::start_faux(&sandbox, 100, 30, true);
        let resumed = second.wait_for_history(STARTUP_TIMEOUT, |capture| {
            capture.contains("MATRIX-PERSIST-FIRST")
        });
        assert_no_json_envelope(&resumed);
        second.send_line("MATRIX-PERSIST-SECOND");
        second.wait_for_history(TURN_TIMEOUT, |capture| {
            capture.contains("faux response to: MATRIX-PERSIST-SECOND")
        });
        second.quit(&sandbox);

        let after_restart = sandbox.session_text();
        assert_valid_jsonl(&after_restart);
        assert!(after_restart.contains("MATRIX-PERSIST-FIRST"));
        assert!(after_restart.contains("MATRIX-PERSIST-SECOND"));
    }

    #[test]
    fn pty_two_isolated_instances_run_in_parallel_without_state_crossing() {
        let _guard = pty_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if skip_without_pty() {
            return;
        }
        let left_sandbox = Sandbox::new("parallel-left");
        let right_sandbox = Sandbox::new("parallel-right");
        let left = TmuxSession::start_faux(&left_sandbox, 80, 24, false);
        let right = TmuxSession::start_faux(&right_sandbox, 160, 50, false);
        left.wait_for_faux_ready();
        right.wait_for_faux_ready();

        let left_marker = "MATRIX-PARALLEL-LEFT";
        let right_marker = "MATRIX-PARALLEL-RIGHT";
        thread::scope(|scope| {
            let left_task = scope.spawn(|| {
                left.send_line(left_marker);
                left.wait_for_history(TURN_TIMEOUT, |capture| {
                    capture.contains("faux response to: MATRIX-PARALLEL-LEFT")
                });
            });
            let right_task = scope.spawn(|| {
                right.send_line(right_marker);
                right.wait_for_history(TURN_TIMEOUT, |capture| {
                    capture.contains("faux response to: MATRIX-PARALLEL-RIGHT")
                });
            });
            left_task.join().expect("left parallel PTY task");
            right_task.join().expect("right parallel PTY task");
        });

        left.quit(&left_sandbox);
        right.quit(&right_sandbox);
        let left_text = left_sandbox.session_text();
        let right_text = right_sandbox.session_text();
        assert_valid_jsonl(&left_text);
        assert_valid_jsonl(&right_text);
        assert!(left_text.contains(left_marker));
        assert!(right_text.contains(right_marker));
        assert!(
            !left_text.contains(right_marker),
            "left instance saw right state"
        );
        assert!(
            !right_text.contains(left_marker),
            "right instance saw left state"
        );
        assert_no_json_envelope(&left_text);
        assert_no_json_envelope(&right_text);
    }

    /// Live evidence is intentionally excluded from the offline matrix. Run
    /// it explicitly with `cargo test --test interactive_real_pty_matrix
    /// --ignored -- --nocapture` after setting `PI_RUST_LIVE_CODEX=1` and
    /// providing a stored OpenAI Codex OAuth credential.
    #[test]
    #[ignore = "requires explicit live-provider credentials and network access"]
    fn live_codex_provider_pty_turn_is_separate_from_offline_matrix() {
        let _guard = pty_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if std::env::var("PI_RUST_LIVE_CODEX").ok().as_deref() != Some("1") {
            eprintln!(
                "SKIP: live Codex PTY case requires PI_RUST_LIVE_CODEX=1; it is never part of the offline matrix"
            );
            return;
        }
        if skip_without_pty() {
            return;
        }
        let Some(auth_source) = find_operator_auth() else {
            eprintln!(
                "SKIP: live Codex PTY case found no auth.json; authenticate with /login openai-codex first"
            );
            return;
        };
        let sandbox = Sandbox::new("live-codex");
        sandbox.copy_auth(&auth_source);
        let session = TmuxSession::start_live(&sandbox, 100, 30);
        session.wait_for(STARTUP_TIMEOUT, |capture| {
            capture.contains("openai-codex")
                || capture.contains("$0.000 (sub)")
                || capture.contains("gpt-5.5")
        });
        const LIVE_MARKER: &str = "MATRIX_LIVE_CODEX_PTY_OK";
        session.send_line(LIVE_MARKER);
        let response = session.wait_for_history(Duration::from_secs(45), |capture| {
            // Markdown treats the underscores as emphasis delimiters in the
            // visible transcript, so compare the terminal projection as well
            // as the source marker used for the request.
            capture
                .replace('_', "")
                .matches("MATRIXLIVECODEXPTYOK")
                .count()
                >= 2
        });
        assert_no_json_envelope(&response);
        session.quit(&sandbox);
        assert_valid_jsonl(&sandbox.session_text());
    }
}

#[cfg(not(unix))]
#[test]
fn interactive_real_pty_matrix_skipped_without_unix_pty() {
    eprintln!(
        "SKIP: interactive_real_pty_matrix requires Unix PTY support and tmux; no PTY cases ran"
    );
}
