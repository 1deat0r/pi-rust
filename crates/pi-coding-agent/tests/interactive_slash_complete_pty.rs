//! Real-PTY coverage for every built-in interactive slash command named by
//! `docs/EXHAUSTIVE-PARITY-INVENTORY.md`.
//!
//! The test talks to the optimized or debug `pi` executable through a tmux
//! pseudo-terminal.  `PI_RUST_TEST_BINARY` selects an already-built release
//! binary; without it Cargo supplies the debug integration-test binary.  The
//! faux provider is used only to make model turns deterministic and offline.

#[cfg(unix)]
mod unix {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use std::thread;
    use std::time::{Duration, Instant};

    const INVENTORY_COMMANDS: &[&str] = &[
        "/settings",
        "/model",
        "/thinking",
        "/scoped-models",
        "/export",
        "/import",
        "/share",
        "/copy",
        "/name",
        "/session",
        "/changelog",
        "/hotkeys",
        "/fork",
        "/clone",
        "/tree",
        "/trust",
        "/login",
        "/logout",
        "/new",
        "/compact",
        "/resume",
        "/reload",
        "/quit",
    ];

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
        html: PathBuf,
        import: PathBuf,
        missing: PathBuf,
        bad_output: PathBuf,
        changelog: PathBuf,
        raw_log: PathBuf,
    }

    impl Sandbox {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("pi-complete-slash-{}", uuid::Uuid::new_v4()));
            let home = root.join("home");
            let agent_dir = home.join(".pi").join("agent");
            let project = root.join("project");
            let html = root.join("session.html");
            let import = root.join("import.jsonl");
            let missing = root.join("missing.jsonl");
            let bad_output = root.join("missing-parent").join("output.html");
            let changelog = root.join("CHANGELOG.md");
            let raw_log = root.join("pty.log");

            fs::create_dir_all(&agent_dir).unwrap();
            fs::create_dir_all(&project).unwrap();
            let import_content = [
                serde_json::json!({
                    "type": "session",
                    "version": 4,
                    "id": "pty-import-session",
                    "timestamp": "2026-08-22T00:00:00.000Z",
                    "cwd": project.to_string_lossy(),
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "pty-import-user",
                    "parentId": null,
                    "timestamp": "2026-08-22T00:00:01.000Z",
                    "message": {"role": "user", "content": [{"type": "text", "text": "imported pty prompt"}]},
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "pty-import-assistant",
                    "parentId": "pty-import-user",
                    "timestamp": "2026-08-22T00:00:02.000Z",
                    "message": {"role": "assistant", "content": [{"type": "text", "text": "imported pty response"}]},
                }),
            ]
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
            fs::write(&import, format!("{import_content}\n")).unwrap();
            fs::write(&changelog, "What's New\n- PTY command coverage\n").unwrap();

            Self {
                root,
                home,
                agent_dir,
                project,
                html,
                import,
                missing,
                bad_output,
                changelog,
                raw_log,
            }
        }

        fn session_files(&self) -> Vec<PathBuf> {
            let mut files = Vec::new();
            collect_jsonl(&self.agent_dir.join("sessions"), &mut files);
            files
        }

        fn wait_for_file(&self, path: &Path, needle: &str) -> String {
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
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn collect_jsonl(path: &Path, files: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_jsonl(&path, files);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            {
                files.push(path);
            }
        }
    }

    struct TmuxSession {
        name: String,
    }

    impl TmuxSession {
        fn start(sandbox: &Sandbox) -> Self {
            Self::start_with_options(sandbox, true, &[])
        }

        fn start_without_share_dry_run(sandbox: &Sandbox) -> Self {
            Self::start_with_options(sandbox, false, &[])
        }

        fn start_resuming(sandbox: &Sandbox) -> Self {
            Self::start_with_options(sandbox, true, &["--resume"])
        }

        fn start_with_options(sandbox: &Sandbox, share_dry_run: bool, extra_args: &[&str]) -> Self {
            let name = format!("pi-complete-slash-{}", uuid::Uuid::new_v4());
            let created = tmux(&[
                "new-session",
                "-d",
                "-x",
                "110",
                "-y",
                "34",
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
            // Keep /share's error path deterministic by excluding the
            // machine's user-local gh, while retaining git and the ordinary
            // Unix utilities used by the interactive runtime.
            let share_env = if share_dry_run {
                " PI_SHARE_DRY_RUN=1"
            } else {
                ""
            };
            let extra_args = extra_args.join(" ");
            let command = format!(
                "env HOME={} PI_CODING_AGENT_DIR={} PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 PI_OAUTH_NO_BROWSER=1 PI_CHANGELOG_PATH={} PATH=/usr/bin:/bin{} {} --approve --provider faux --model faux-1 --tui-mode fullscreen {}; exec tail -f /dev/null",
                shell_quote(&sandbox.home),
                shell_quote(&sandbox.agent_dir),
                shell_quote(&sandbox.changelog),
                share_env,
                shell_quote(&binary),
                extra_args,
            );
            let configured = tmux(&["set-option", "-t", &name, "remain-on-exit", "on"]);
            assert!(
                configured.status.success(),
                "tmux remain-on-exit failed: {}",
                stderr(&configured)
            );
            let sent = tmux(&["respawn-pane", "-k", "-t", &name, &command]);
            assert!(
                sent.status.success(),
                "tmux respawn-pane failed: {}",
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

        #[track_caller]
        fn wait_for<F>(&self, mut predicate: F) -> String
        where
            F: FnMut(&str) -> bool,
        {
            let caller = std::panic::Location::caller();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let capture = self.capture();
                if predicate(&capture) {
                    return capture;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for PTY output (caller {}:{}); last capture:\n{capture}",
                    caller.file(),
                    caller.line()
                );
                thread::sleep(Duration::from_millis(25));
            }
        }

        fn wait_for_change<F>(&self, previous: &str, mut predicate: F) -> String
        where
            F: FnMut(&str) -> bool,
        {
            self.wait_for(|capture| capture != previous && predicate(capture))
        }

        fn send_line(&self, line: &str) {
            let literal = tmux(&["send-keys", "-t", &self.name, "-l", "--", line]);
            assert!(
                literal.status.success(),
                "tmux literal input {line:?} failed: {}",
                stderr(&literal)
            );
            thread::sleep(Duration::from_millis(80));
            self.send_key("Enter");
        }

        fn send_text(&self, text: &str) {
            let output = tmux(&["send-keys", "-t", &self.name, "-l", "--", text]);
            assert!(
                output.status.success(),
                "tmux text input {text:?} failed: {}",
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

        fn resize(&self, width: u16, height: u16) {
            let width = width.to_string();
            let height = height.to_string();
            let output = tmux(&[
                "resize-window",
                "-t",
                &self.name,
                "-x",
                &width,
                "-y",
                &height,
            ]);
            assert!(
                output.status.success(),
                "tmux resize to {width}x{height} failed: {}",
                stderr(&output)
            );
            thread::sleep(Duration::from_millis(150));
        }

        fn settle(&self) {
            thread::sleep(Duration::from_millis(750));
        }

        fn command(&self, line: &str, expected: &str) -> String {
            let before = self.capture();
            self.send_line(line);
            let deadline = Instant::now() + Duration::from_secs(10);
            let capture = loop {
                let capture = self.capture();
                if capture != before && capture.contains(expected) {
                    break capture;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out on slash command {line:?}, expected {expected:?}; last capture:\n{capture}"
                );
                thread::sleep(Duration::from_millis(25));
            };
            self.settle();
            capture
        }

        fn dismiss_modal(&self) {
            let before = self.capture();
            self.send_key("Escape");
            let _ = self.wait_for_change(&before, |_| true);
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

    fn tmux(args: &[&str]) -> Output {
        Command::new("tmux")
            .args(args)
            .output()
            .expect("tmux must be installed for interactive PTY coverage")
    }

    fn stderr(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).trim().to_string()
    }

    fn shell_quote(path: &Path) -> String {
        let value = path.to_string_lossy();
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }

    fn assert_inventory_coverage(covered: &BTreeSet<&'static str>) {
        let expected = INVENTORY_COMMANDS.iter().copied().collect::<BTreeSet<_>>();
        assert_eq!(
            covered,
            &expected,
            "interactive slash inventory coverage mismatch; missing: {:?}; unexpected: {:?}",
            expected.difference(covered).collect::<Vec<_>>(),
            covered.difference(&expected).collect::<Vec<_>>(),
        );
    }

    fn assert_terminal_restored(session: &TmuxSession, sandbox: &Sandbox) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let raw = fs::read_to_string(&sandbox.raw_log).unwrap_or_default();
            if raw.contains("\x1b[?1049l") {
                assert!(
                    raw.contains("\x1b[?1049h"),
                    "alt-screen entry missing: {raw:?}"
                );
                assert!(raw.contains("\x1b[?25l"), "cursor hide missing: {raw:?}");
                assert!(raw.contains("\x1b[?25h"), "cursor restore missing: {raw:?}");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for terminal restore: {raw:?}"
            );
            thread::sleep(Duration::from_millis(25));
        }
        session.stop_pipe();
        session.wait_for_cooked_mode();
    }

    fn wait_for_any_session_file(sandbox: &Sandbox, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            for path in sandbox.session_files() {
                let content = fs::read_to_string(path).unwrap_or_default();
                if content.contains(needle) {
                    return content;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?} in a session JSONL file"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn all_inventory_slash_commands_run_through_real_pty() {
        let sandbox = Sandbox::new();
        let session = TmuxSession::start(&sandbox);
        session.wait_for(|capture| capture.contains("(faux/Faux Model)"));
        let mut covered = BTreeSet::new();

        session.send_line("pty seed prompt");
        session.wait_for(|capture| capture.contains("faux response to: pty seed prompt"));
        session.settle();

        session.command("/settings", "Default thinking level");
        covered.insert("/settings");
        session.dismiss_modal();
        session.command("/settings", "Default thinking level");
        session.dismiss_modal();

        session.command("/model", "faux/Faux Model");
        covered.insert("/model");
        session.dismiss_modal();
        session.command("/model", "faux/Faux Model");
        session.dismiss_modal();

        session.command("/thinking", "off");
        covered.insert("/thinking");
        session.send_key("Down");
        session.send_key("Enter");
        session.wait_for(|capture| capture.contains("Thinking: low"));
        session.command("/reload", "reloaded settings");
        covered.insert("/reload");
        sandbox.wait_for_file(
            &sandbox.agent_dir.join("settings.json"),
            "defaultThinkingLevel",
        );
        session.command("/thinking", "low");
        session.dismiss_modal();

        session.command("/scoped-models", "amazon-bedrock");
        covered.insert("/scoped-models");
        session.send_key("Enter");
        session.wait_for(|capture| capture.contains("Enabled "));
        session.send_key("Escape");
        session.wait_for(|capture| capture.contains("Scoped models:"));
        session.command("/scoped-models", "[x]");
        session.dismiss_modal();

        let files_before_clone = sandbox.session_files().len();
        session.command("/clone", "clone session");
        covered.insert("/clone");
        let files_after_clone = sandbox.session_files().len();
        assert!(
            files_after_clone > files_before_clone,
            "/clone did not persist a new JSONL session"
        );

        session.command("/copy", "copied");
        covered.insert("/copy");

        session.send_line("/fork");
        covered.insert("/fork");
        session.send_key("Escape");
        session.settle();
        session.send_line("/fork");
        session.send_key("Enter");
        session.wait_for(|capture| capture.contains("fork session"));
        session.send_key("C-u");
        session.settle();

        session.command(
            &format!("/export {}", sandbox.html.display()),
            "exported session to",
        );
        covered.insert("/export");
        sandbox.wait_for_file(&sandbox.html, "<html");
        session.command(
            &format!("/export {}", sandbox.html.display()),
            "exported session to",
        );
        let before_error = session.capture();
        session.send_line(&format!("/export {}", sandbox.bad_output.display()));
        session.wait_for_change(&before_error, |capture| capture.contains("export failed:"));

        session.command(&format!("/import {}", sandbox.import.display()), "imported");
        covered.insert("/import");
        session.command(
            &format!("/import {}", sandbox.missing.display()),
            "file not found:",
        );

        session.command("/share", "/share skipped");
        covered.insert("/share");
        session.command("/share", "/share skipped");

        session.command(
            "/name complete pty session",
            "session name: complete pty session",
        );
        covered.insert("/name");
        wait_for_any_session_file(&sandbox, "complete pty session");
        session.command("/name", "usage: /name <session-name>");

        session.command("/session", "session ");
        covered.insert("/session");
        session.command("/changelog", "What's New");
        covered.insert("/changelog");
        session.command("/hotkeys", "hotkeys: enter submit");
        covered.insert("/hotkeys");

        session.command("/tree", "session tree:");
        covered.insert("/tree");

        session.command("/trust allow", "default project trust: allow");
        covered.insert("/trust");
        session.command("/trust invalid", "usage: /trust <allow|deny|ask>");
        session.command("/trust deny", "default project trust: deny");

        session.command("/login faux", "Enter API key for Faux:");
        covered.insert("/login");
        session.send_key("Escape");
        session.wait_for(|capture| capture.contains("Login cancelled"));
        session.command("/login faux", "Enter API key for Faux:");
        session.send_text("pty-test-key");
        session.wait_for(|capture| capture.contains("••••••••••••"));
        session.send_key("Enter");
        session.wait_for(|capture| capture.contains("logged in to faux via API key"));
        let auth = sandbox.wait_for_file(&sandbox.agent_dir.join("auth.json"), "pty-test-key");
        assert!(auth.contains("faux"));
        assert!(auth.contains("pty-test-key"));

        session.command("/logout faux", "logged out faux");
        covered.insert("/logout");
        let auth_after_logout = fs::read_to_string(sandbox.agent_dir.join("auth.json")).unwrap();
        assert!(!auth_after_logout.contains("pty-test-key"));
        session.command("/logout", "No stored credentials to remove");

        let files_before_new = sandbox.session_files().len();
        session.command("/new", "started new session");
        covered.insert("/new");
        assert!(sandbox.session_files().len() > files_before_new);

        session.command("/resume", "session");
        covered.insert("/resume");
        session.send_key("Enter");
        session.wait_for(|capture| capture.contains("resumed session"));

        let compact_before = session.capture();
        session.send_line("/compact");
        let compact = session.wait_for_change(&compact_before, |capture| {
            capture.contains("compact") || capture.contains("compaction")
        });
        assert!(compact.contains("compact") || compact.contains("compaction"));
        covered.insert("/compact");

        session.command("/reload", "reloaded settings");
        sandbox.wait_for_file(
            &sandbox.agent_dir.join("settings.json"),
            "defaultProjectTrust",
        );
        session.command("/reload", "reloaded settings");

        session.send_line("/quit");
        covered.insert("/quit");
        assert_inventory_coverage(&covered);
        assert_terminal_restored(&session, &sandbox);
    }

    #[test]
    fn slash_errors_cancellation_and_restart_persist_without_mocked_turns() {
        let sandbox = Sandbox::new();
        let session = TmuxSession::start_without_share_dry_run(&sandbox);
        session.wait_for(|capture| capture.contains("(faux/Faux Model)"));
        session.send_line("restart persistence prompt");
        session
            .wait_for(|capture| capture.contains("faux response to: restart persistence prompt"));
        session.settle();

        session.command("/name restart-persisted", "session name: restart-persisted");
        session.command("/trust allow", "default project trust: allow");
        session.command("/thinking", "off");
        session.send_key("Down");
        session.send_key("Enter");
        session.wait_for(|capture| capture.contains("Thinking: low"));
        session.command("/reload", "reloaded settings");
        let settings_file = sandbox.wait_for_file(
            &sandbox.agent_dir.join("settings.json"),
            "defaultProjectTrust",
        );
        assert!(
            settings_file.contains("defaultThinkingLevel"),
            "thinking setting was not persisted: {settings_file}"
        );
        wait_for_any_session_file(&sandbox, "restart-persisted");

        session.command("/login unknown-provider", "no OAuth login available");
        session.command("/login faux", "Enter API key for Faux:");
        session.send_key("Escape");
        session.wait_for(|capture| capture.contains("Login cancelled"));

        session.command("/logout", "No stored credentials to remove");
        session.command("/import", "usage: /import <session.jsonl>");
        session.command(
            &format!("/export {}", sandbox.bad_output.display()),
            "export failed:",
        );
        session.command("/trust nope", "usage: /trust <allow|deny|ask>");

        // Exercise the non-dry-run share failure against a PATH that has no
        // gh. This is still a real command in the real process; only the
        // provider response is faux and no network is attempted.
        let before_share = session.capture();
        session.send_line("/share");
        let share_error = session.wait_for_change(&before_share, |capture| {
            capture.contains("GitHub CLI") || capture.contains("share failed")
        });
        assert!(share_error.contains("GitHub CLI") || share_error.contains("share failed"));

        session.send_line("/quit");
        assert_terminal_restored(&session, &sandbox);

        // A second real process discovers the persisted name, trust, and
        // thinking settings from disk.  The exact session is supplied by
        // --resume so the restart boundary is unambiguous.
        let restarted = TmuxSession::start_resuming(&sandbox);
        let startup = restarted.wait_for(|capture| {
            capture.contains("resumed session") && capture.contains("restart-persisted")
        });
        assert!(
            startup.contains("restart-persisted"),
            "session name was not reloaded at restart: {startup}"
        );
        restarted.send_key("C-d");
        assert_terminal_restored(&restarted, &sandbox);
    }

    #[test]
    fn hidden_components_cover_success_repeat_resize_cancellation_errors_and_restore() {
        let sandbox = Sandbox::new();
        let session = TmuxSession::start(&sandbox);
        session.wait_for(|capture| capture.contains("(faux/Faux Model)"));

        // /debug writes the same two sections as upstream and is observable
        // through the real agent directory, not a mocked command result.
        let debug_path = sandbox.agent_dir.join("pi-debug.log");
        let debug_capture = session.command("/debug", "✓ Debug log written");
        assert!(debug_capture.contains(debug_path.to_string_lossy().as_ref()));
        let debug = sandbox.wait_for_file(&debug_path, "Terminal:");
        let timestamp = debug
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("Debug output at "))
            .expect("debug log starts with the upstream timestamp header");
        assert_eq!(timestamp.len(), 24);
        assert_eq!(&timestamp[10..11], "T");
        assert_eq!(&timestamp[19..20], ".");
        assert_eq!(&timestamp[23..24], "Z");
        assert!(!debug.contains("Debug output at unix:"));
        assert!(debug.contains("All rendered lines with visible widths"));
        assert!(debug.contains("Agent messages (JSONL)"));

        // The animation must remain bounded by the terminal width and height.
        session.resize(38, 12);
        session.command("/debug", "✓ Debug log written");
        let narrow_debug = sandbox.wait_for_file(&debug_path, "Terminal: 38x12");
        assert!(narrow_debug.contains("Terminal: 38x12"));

        // Resize back to a full viewport before checking the component's
        // completion row. The narrow resize above is independently asserted
        // through the real /debug artifact.
        session.resize(110, 34);

        // Repeating the hidden command appends another ordinary component;
        // the debug snapshot proves both instances remain in the scene.
        session.command("/arminsayshi", "ARMIN SAYS HI");
        session.command("/arminsayshi", "ARMIN SAYS HI");
        session.command("/debug", "✓ Debug log written");
        let repeated = sandbox.wait_for_file(&debug_path, "Agent messages (JSONL)");
        assert!(
            repeated.matches("ARMIN SAYS HI").count() >= 2,
            "repeat did not retain both Armin components: {repeated}"
        );

        session.command("/dementedelves", "pi has joined Earendil");

        // Direct model selection exercises the exact provider/id trigger. The
        // faux provider only makes process startup deterministic; no Codex turn
        // is sent for this visual-only path.
        session.command(
            "/model opencode/kimi-k2.5",
            "Free Kimi K2.5 via OpenCode Zen",
        );

        // The auth flow owns the terminal reader temporarily and must return
        // it to the interactive loop after cancellation.
        session.command("/login faux", "Enter API key for Faux:");
        session.send_key("Escape");
        session.wait_for(|capture| capture.contains("Login cancelled"));

        // A registered command with invalid input is an explicit error path,
        // never a generic not-wired response.
        session.command("/model opencode/not-a-real-model", "model not found:");
        session.command("/import", "usage: /import <session.jsonl>");

        session.send_line("/quit");
        assert_terminal_restored(&session, &sandbox);
    }

    #[test]
    fn quit_restores_terminal_even_when_repeated_as_a_control_boundary() {
        let sandbox = Sandbox::new();
        let session = TmuxSession::start(&sandbox);
        session.wait_for(|capture| capture.contains("(faux/Faux Model)"));
        session.send_line("/quit");
        assert_terminal_restored(&session, &sandbox);
        // The pane remains alive under tmux's remain-on-exit wrapper; sending
        // another quit is harmless and proves shutdown does not require a
        // second interactive turn or a leaked raw terminal.
        session.send_line("/quit");
        session.wait_for_cooked_mode();
    }
}

#[cfg(not(unix))]
#[test]
fn interactive_slash_complete_pty_requires_unix_pty_support() {}
