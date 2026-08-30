#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Opt-in live-provider regression coverage for interactive tool rendering.
//!
//! This is live evidence from the real OpenAI Codex provider, not a mock. It
//! is intentionally skipped unless `PI_RUST_LIVE_CODEX=1` is set.

#[cfg(unix)]
mod unix {
    use serde_json::Value;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use std::thread;
    use std::time::{Duration, Instant};

    const FIXTURE_NAME: &str = "pi-live-tool-render-fixture.txt";
    // Keep these sentinels alphanumeric: the live transcript is Markdown, so
    // punctuation-heavy tokens can be transformed by the visual renderer
    // even when the persisted assistant text is correct.
    const FIXTURE_CONTENT: &str = "PIRUSTLIVECODEXFILECONTENT";
    const EXACT_RESPONSE: &str = "PIRUSTLIVECODEXREADOK";
    const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
    const TURN_TIMEOUT: Duration = Duration::from_secs(90);

    struct Sandbox {
        root: PathBuf,
        home: PathBuf,
        agent_dir: PathBuf,
        sessions: PathBuf,
        project: PathBuf,
        raw_log: PathBuf,
    }

    impl Sandbox {
        fn new(auth_source: &Path) -> Self {
            let root = std::env::temp_dir().join(format!(
                "pi-interactive-live-codex-{}",
                uuid::Uuid::new_v4()
            ));
            let home = root.join("home");
            let agent_dir = root.join("agent");
            let sessions = root.join("sessions");
            let project = root.join("project");
            let raw_log = root.join("tmux-output.log");
            for directory in [&home, &agent_dir, &sessions, &project] {
                fs::create_dir_all(directory).expect("create isolated live PTY directory");
            }

            // Copy the real credential into the disposable store. The child
            // may refresh this copy; the operator's auth.json is never opened
            // for writing and is never used as the child store.
            fs::copy(auth_source, agent_dir.join("auth.json"))
                .expect("copy operator auth store into isolated live PTY root");
            fs::write(project.join(FIXTURE_NAME), format!("{FIXTURE_CONTENT}\n"))
                .expect("write isolated read fixture");

            Self {
                root,
                home,
                agent_dir,
                sessions,
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
            let name = format!("pi-live-codex-{}", uuid::Uuid::new_v4());
            let created = tmux(&[
                "new-session",
                "-d",
                "-x",
                "120",
                "-y",
                "36",
                "-c",
                sandbox.project.to_str().expect("project path is UTF-8"),
                "-s",
                &name,
                "tail",
                "-f",
                "/dev/null",
            ]);
            assert!(
                created.status.success(),
                "tmux session creation failed: {}",
                stderr(&created)
            );

            let configured = tmux(&["set-option", "-t", &name, "remain-on-exit", "on"]);
            assert!(
                configured.status.success(),
                "tmux remain-on-exit setup failed: {}",
                stderr(&configured)
            );

            // Pipe before respawning the pane so the raw log includes startup
            // bytes as well as the later live tool lifecycle.
            let pipe_command = format!("cat > {}", shell_quote(&sandbox.raw_log));
            let piped = tmux(&["pipe-pane", "-o", "-t", &name, &pipe_command]);
            assert!(
                piped.status.success(),
                "tmux PTY capture setup failed: {}",
                stderr(&piped)
            );

            let binary = test_binary();
            assert!(
                binary.is_file(),
                "pi test binary does not exist: {}",
                binary.display()
            );
            let command = format!(
                "env -i HOME={} XDG_CONFIG_HOME={} XDG_DATA_HOME={} PI_CODING_AGENT_DIR={} PI_CODING_AGENT_SESSION_DIR={} PI_SKIP_VERSION_CHECK=1 PI_TELEMETRY=0 TERM=xterm-256color LANG=C LC_ALL=C PATH=/usr/bin:/bin {} --approve --provider openai-codex --model gpt-5.5 --thinking off --tools read --no-extensions --no-skills --no-prompt-templates --no-themes --no-context-files --tui-mode fullscreen; exec tail -f /dev/null",
                shell_quote(&sandbox.home),
                shell_quote(&sandbox.root.join("xdg-config")),
                shell_quote(&sandbox.root.join("xdg-data")),
                shell_quote(&sandbox.agent_dir),
                shell_quote(&sandbox.sessions),
                shell_quote(&binary),
            );
            let launched = tmux(&["respawn-pane", "-k", "-t", &name, &command]);
            assert!(
                launched.status.success(),
                "tmux pi launch failed: {}",
                stderr(&launched)
            );

            Self { name }
        }

        fn capture(&self) -> String {
            let output = tmux(&["capture-pane", "-p", "-t", &self.name]);
            assert!(
                output.status.success(),
                "tmux pane capture failed: {}",
                stderr(&output)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        }

        fn raw(&self, sandbox: &Sandbox) -> String {
            fs::read_to_string(&sandbox.raw_log).unwrap_or_default()
        }

        fn wait_for_capture<F>(&self, timeout: Duration, mut predicate: F) -> bool
        where
            F: FnMut(&str) -> bool,
        {
            let deadline = Instant::now() + timeout;
            loop {
                let capture = self.capture();
                if predicate(&capture) {
                    return true;
                }
                if Instant::now() >= deadline {
                    return false;
                }
                thread::sleep(Duration::from_millis(25));
            }
        }

        fn wait_for_raw<F>(&self, sandbox: &Sandbox, timeout: Duration, mut predicate: F) -> bool
        where
            F: FnMut(&str) -> bool,
        {
            let deadline = Instant::now() + timeout;
            loop {
                let raw = self.raw(sandbox);
                if predicate(&raw) {
                    return true;
                }
                if Instant::now() >= deadline {
                    return false;
                }
                thread::sleep(Duration::from_millis(25));
            }
        }

        fn send_line(&self, line: &str) {
            let text = tmux(&["send-keys", "-t", &self.name, "-l", "--", line]);
            assert!(
                text.status.success(),
                "tmux prompt input failed: {}",
                stderr(&text)
            );
            let enter = tmux(&["send-keys", "-t", &self.name, "Enter"]);
            assert!(
                enter.status.success(),
                "tmux prompt submission failed: {}",
                stderr(&enter)
            );
        }

        fn force_redraw(&self) {
            let resized = tmux(&["resize-window", "-t", &self.name, "-x", "121", "-y", "36"]);
            assert!(
                resized.status.success(),
                "tmux redraw resize failed: {}",
                stderr(&resized)
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

    fn operator_auth_path() -> Option<PathBuf> {
        let mut agent_dirs = Vec::new();
        if let Some(agent_dir) = std::env::var_os("PI_CODING_AGENT_DIR") {
            agent_dirs.push(PathBuf::from(agent_dir));
        }
        if let Some(home) = std::env::var_os("HOME") {
            agent_dirs.push(PathBuf::from(home).join(".pi").join("agent"));
        }
        if let Some(home) = dirs::home_dir() {
            agent_dirs.push(home.join(".pi").join("agent"));
        }

        agent_dirs.dedup();
        agent_dirs
            .into_iter()
            .map(|dir| dir.join("auth.json"))
            .find(|path| {
                let Ok(content) = fs::read_to_string(path) else {
                    return false;
                };
                let Ok(value) = serde_json::from_str::<Value>(&content) else {
                    return false;
                };
                value
                    .get("openai-codex")
                    .is_some_and(|credential| credential.is_object())
            })
    }

    fn tmux(args: &[&str]) -> Output {
        Command::new("tmux")
            .args(args)
            .output()
            .expect("tmux must be installed for the live PTY regression")
    }

    fn tmux_has_session(name: &str) -> bool {
        tmux(&["has-session", "-t", name]).status.success()
    }

    fn stderr(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).trim().to_string()
    }

    fn shell_quote(path: &Path) -> String {
        let value = path.to_string_lossy();
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }

    fn visible(text: &str) -> String {
        pi_tui::strip_terminal_sequences(text)
            .replace("\r\n", "\n")
            .replace('\r', "\n")
    }

    fn line_has(text: &str, needles: &[&str]) -> bool {
        text.lines()
            .any(|line| needles.iter().all(|needle| line.contains(needle)))
    }

    fn exact_line(text: &str, expected: &str) -> bool {
        text.lines().any(|line| line.trim() == expected)
    }

    fn jsonl_files(root: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let Ok(entries) = fs::read_dir(root) else {
            return files;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(jsonl_files(&path));
            } else if path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            {
                files.push(path);
            }
        }
        files.sort();
        files
    }

    fn wait_for_session_file(root: &Path, timeout: Duration) -> Option<PathBuf> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(path) = jsonl_files(root).into_iter().next() {
                return Some(path);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn collect_tool_calls(value: &Value, calls: &mut Vec<(String, Value)>) {
        match value {
            Value::Object(object) => {
                if object.get("type").and_then(Value::as_str) == Some("toolCall") {
                    if let (Some(name), Some(arguments)) = (
                        object.get("name").and_then(Value::as_str),
                        object.get("arguments"),
                    ) {
                        calls.push((name.to_string(), arguments.clone()));
                    }
                }
                for child in object.values() {
                    collect_tool_calls(child, calls);
                }
            }
            Value::Array(values) => {
                for child in values {
                    collect_tool_calls(child, calls);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }

    fn collect_assistant_texts(path: &Path) -> Vec<String> {
        let content = fs::read_to_string(path).expect("read isolated session JSONL");
        content
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|entry| {
                entry.get("kind").and_then(Value::as_str) == Some("entry")
                    && entry.get("type").and_then(Value::as_str) == Some("message")
                    && entry
                        .get("message")
                        .and_then(|message| message.get("role"))
                        .and_then(Value::as_str)
                        == Some("assistant")
            })
            .flat_map(|entry| {
                entry
                    .get("message")
                    .and_then(|message| message.get("content"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn live_codex_read_tool_render_pty_regression() {
        if std::env::var("PI_RUST_LIVE_CODEX").ok().as_deref() != Some("1") {
            eprintln!(
                "skipping live Codex PTY regression; set PI_RUST_LIVE_CODEX=1 and run the targeted test with a logged-in openai-codex account"
            );
            return;
        }
        if Command::new("tmux").arg("-V").output().is_err() {
            eprintln!(
                "skipping live Codex PTY regression; tmux is required (install tmux and rerun with PI_RUST_LIVE_CODEX=1)"
            );
            return;
        }
        let Some(auth_source) = operator_auth_path() else {
            eprintln!(
                "skipping live Codex PTY regression; no usable openai-codex auth.json was found (run `pi /login openai-codex`, then rerun with PI_RUST_LIVE_CODEX=1)"
            );
            return;
        };

        let sandbox = Sandbox::new(&auth_source);
        let session = TmuxSession::start(&sandbox);
        let session_name = session.name.clone();
        let root = sandbox.root.clone();

        assert!(
            session.wait_for_raw(&sandbox, STARTUP_TIMEOUT, |raw| !raw.is_empty()),
            "live PTY produced no captured startup bytes; raw output was intentionally omitted"
        );
        assert!(
            session.wait_for_capture(STARTUP_TIMEOUT, |capture| {
                let startup = visible(capture);
                startup.contains("GPT-5.5") || startup.contains("gpt-5.5")
            }),
            "live Codex PTY did not reach the gpt-5.5 startup screen"
        );

        let prompt = format!(
            "Use the built-in read tool exactly once to read {FIXTURE_NAME} in the current repository. Do not read any other file and do not call any other tool. After it succeeds, reply with exactly {EXACT_RESPONSE} and nothing else."
        );
        session.send_line(&prompt);

        assert!(
            session.wait_for_raw(&sandbox, TURN_TIMEOUT, |raw| {
                line_has(&visible(raw), &["⏳", "read", FIXTURE_NAME])
            }),
            "raw PTY capture did not contain a running read block with its path; output was intentionally omitted"
        );

        // The raw PTY stream contains cursor-addressing writes, so multiple
        // screen rows can be adjacent in the captured byte stream. The
        // screen snapshot below checks line identity; the stream gate only
        // needs to prove that the exact response was emitted at all.
        let exact_response_seen = session.wait_for_raw(&sandbox, TURN_TIMEOUT, |raw| {
            visible(raw).contains(EXACT_RESPONSE)
        });
        if !exact_response_seen {
            let raw = session.raw(&sandbox);
            let visible_raw = visible(&raw);
            let candidates = visible_raw
                .lines()
                .filter(|line| {
                    let line = line.trim();
                    line.contains("read")
                        || line.contains("PIRUST")
                        || line.contains("CODEX")
                        || line.contains("tool")
                })
                .rev()
                .take(16)
                .collect::<Vec<_>>();
            panic!(
                "raw PTY capture did not contain the exact final response; candidate lines: {candidates:?}"
            );
        }
        // The completed turn is committed after the live component clears;
        // a resize makes the idle loop render that settled transcript.
        session.force_redraw();
        let settled_seen = session.wait_for_capture(Duration::from_secs(5), |capture| {
            let settled = visible(capture);
            exact_line(&settled, EXACT_RESPONSE) && line_has(&settled, &["✓", "read"])
        });
        if !settled_seen {
            let settled = visible(&session.capture());
            panic!(
                "settled PTY screen did not contain the exact response and successful collapsed read block; capture:\n{settled}"
            );
        }
        let settled = visible(&session.capture());
        assert!(
            exact_line(&settled, EXACT_RESPONSE),
            "final assistant response was not the exact expected line"
        );
        assert!(
            line_has(&settled, &["✓", "read"]),
            "final PTY screen did not show a settled successful read block"
        );
        assert!(
            !settled.contains("### You"),
            "normal live transcript still exposed the legacy user heading"
        );
        assert!(
            session.wait_for_raw(&sandbox, Duration::from_secs(5), |raw| {
                raw.contains("\x1b]133;A\x07")
                    && raw.contains("\x1b]133;B\x07")
                    && raw.contains("\x1b]133;C\x07")
            }),
            "live user transcript did not emit OSC133 semantic-zone markers"
        );
        for forbidden in [
            "```json",
            "```",
            "\"arguments\"",
            "\"file_path\"",
            "\"path\"",
        ] {
            assert!(
                !settled.contains(forbidden),
                "final PTY screen exposed a fenced or argument-envelope JSON marker: {forbidden}"
            );
        }

        session.send_line("/quit");
        assert!(
            session.wait_for_raw(&sandbox, STARTUP_TIMEOUT, |raw| raw.contains("\x1b[?1049l")),
            "live PTY did not restore the alternate screen; raw output was intentionally omitted"
        );
        let session_file = wait_for_session_file(&sandbox.sessions, STARTUP_TIMEOUT)
            .expect("live Codex turn did not persist an isolated session");
        let mut calls = Vec::new();
        let session_text = fs::read_to_string(&session_file).expect("read isolated session");
        assert!(
            session_text.contains(FIXTURE_CONTENT),
            "persisted live read result did not contain the isolated fixture content"
        );
        for line in session_text.lines() {
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                collect_tool_calls(&value, &mut calls);
            }
        }
        assert_eq!(
            calls.len(),
            1,
            "live turn did not contain exactly one tool call"
        );
        assert_eq!(
            calls[0].0, "read",
            "live turn used a tool other than built-in read"
        );
        assert!(
            calls[0].1.to_string().contains(FIXTURE_NAME),
            "the single read call did not target the isolated repo fixture"
        );
        assert_eq!(
            collect_assistant_texts(&session_file)
                .into_iter()
                .filter(|text| text == EXACT_RESPONSE)
                .count(),
            1,
            "the persisted live assistant response was not exactly the expected text"
        );

        drop(session);
        assert!(
            !tmux_has_session(&session_name),
            "live PTY tmux session was not cleaned up"
        );
        drop(sandbox);
        assert!(!root.exists(), "live PTY temporary root was not cleaned up");
    }
}
