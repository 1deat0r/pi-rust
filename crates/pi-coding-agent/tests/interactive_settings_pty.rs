#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Focused settings and skill-command coverage for the interactive terminal.
//!
//! The Unix test drives the compiled binary through tmux so autocomplete,
//! settings callbacks, persistence, and restart behavior are exercised at the
//! process boundary. Component tests below remain deterministic and do not
//! require a terminal or provider.

use pi_coding_agent::core::settings::{SettingsManager, SettingsMap};
use pi_coding_agent::interactive::selectors::settings_selector_items;
use pi_coding_agent::interactive::settings_panel::{SettingChoice, SettingEntry, SettingsPanel};
use pi_tui::{strip_ansi_codes, Component, TuiKey};

fn setting_submenu(id: &str, label: &str, values: &[(&str, &str)]) -> SettingEntry {
    SettingEntry::choice_submenu(
        id,
        label,
        values[0].0.to_string(),
        format!("Configure {label}"),
        values
            .iter()
            .map(|(value, display)| SettingChoice::new(*value, *display))
            .collect(),
    )
}

#[test]
fn settings_components_search_skip_disabled_and_commit_nested_values() {
    let mut panel = SettingsPanel::new(vec![
        SettingEntry::info("disabled", "Disabled", "managed".to_string()).with_disabled(true),
        SettingEntry::cycle(
            "autocompact",
            "Auto-compact",
            "false".to_string(),
            vec!["true".to_string(), "false".to_string()],
        ),
        setting_submenu(
            "warnings",
            "Warnings",
            &[("true", "Warnings enabled"), ("false", "Warnings disabled")],
        ),
        setting_submenu(
            "model-thinking",
            "Default thinking level per model",
            &[("off", "off"), ("minimal", "minimal")],
        ),
        setting_submenu("theme", "Theme", &[("light", "Light"), ("dark", "Dark")]),
    ]);
    panel.set_focused(true);

    let initial = panel.render(80).join("\n");
    assert!(initial.contains("Auto-compact"));
    assert!(!strip_ansi_codes(&initial).contains("→ Disabled"));

    // The disabled first row is skipped and one Down press selects exactly
    // the next enabled row.
    panel.handle_input(&TuiKey::simple("down"));
    let moved = strip_ansi_codes(&panel.render(80).join("\n"));
    assert!(moved.contains("→ Warnings"), "rendered settings: {moved}");

    // Nested Escape returns to the main list without committing.
    panel.handle_input(&TuiKey::simple("enter"));
    assert!(panel.is_submenu_open());
    panel.handle_input(&TuiKey::simple("escape"));
    assert!(!panel.is_submenu_open());
    assert!(panel.drain_changes().is_empty());

    // Search narrows the live list, and the nested choice commits its value.
    for character in "warnings".chars() {
        panel.handle_input(&TuiKey::simple(character.to_string()));
    }
    let filtered = strip_ansi_codes(&panel.render(80).join("\n"));
    assert!(filtered.contains("Warnings"));
    panel.handle_input(&TuiKey::simple("enter"));
    panel.handle_input(&TuiKey::simple("down"));
    panel.handle_input(&TuiKey::simple("enter"));
    assert_eq!(
        panel.drain_changes(),
        vec![("warnings".to_string(), "false".to_string())]
    );

    // The selector builder still exposes the complete settings registry to
    // the parent runtime, including the capability-independent endpoints.
    let settings = SettingsManager::in_memory(SettingsMap::new());
    let entries = settings_selector_items(&settings);
    assert_eq!(
        entries.first().map(|entry| entry.id.as_str()),
        Some("autocompact")
    );
    assert_eq!(entries.last().map(|entry| entry.id.as_str()), Some("theme"));
    assert!(entries.iter().any(|entry| entry.id == "model-thinking"));
}

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use std::sync::{Mutex, OnceLock};
    use std::thread;
    use std::time::{Duration, Instant};

    const TIMEOUT: Duration = Duration::from_secs(12);

    fn test_binary() -> PathBuf {
        std::env::var_os("PI_RUST_TEST_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pi")))
    }

    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
    }

    fn shell_quote_text(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }

    fn tmux(args: &[&str]) -> Output {
        Command::new("tmux")
            .args(args)
            .output()
            .expect("tmux must be installed for interactive settings PTY coverage")
    }

    fn stderr(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).trim().to_string()
    }

    fn visible(text: &str) -> String {
        pi_tui::strip_terminal_sequences(text)
            .replace("\r\n", "\n")
            .replace('\r', "\n")
    }

    fn pty_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct Sandbox {
        root: PathBuf,
        home: PathBuf,
        agent_dir: PathBuf,
        sessions: PathBuf,
        project: PathBuf,
        settings: PathBuf,
        prompt_template: PathBuf,
    }

    impl Sandbox {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .join(format!("pi-interactive-settings-{}", uuid::Uuid::new_v4()));
            let home = root.join("home");
            let agent_dir = root.join("agent");
            let sessions = root.join("sessions");
            let project = root.join("project");
            let settings = agent_dir.join("settings.json");
            let prompt_template = project.join("explicit-review.md");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&agent_dir).unwrap();
            fs::create_dir_all(&sessions).unwrap();
            fs::create_dir_all(project.join(".pi/skills/demo")).unwrap();
            fs::write(
                project.join(".pi/skills/demo/SKILL.md"),
                "---\nname: demo\ndescription: A deterministic PTY demo skill\n---\n\nPTY skill body marker\n",
            )
            .unwrap();
            fs::write(
                &prompt_template,
                "---\ndescription: Explicit deterministic review template\n---\nINITIAL TEMPLATE FIRST=$1 ALL=$@ REST=${@:2}\n",
            )
            .unwrap();
            fs::write(
                &settings,
                r#"{"compaction":{"enabled":false},"warnings":{"anthropic-extra-usage":true},"theme":"light"}"#,
            )
            .unwrap();
            Self {
                root,
                home,
                agent_dir,
                sessions,
                project,
                settings,
                prompt_template,
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
            Self::start_with_args(sandbox, &[])
        }

        fn start_with_args(sandbox: &Sandbox, extra_args: &[&str]) -> Self {
            let name = format!("pi-settings-{}", uuid::Uuid::new_v4());
            let project = sandbox.project.to_str().expect("project path");
            let created = tmux(&[
                "new-session",
                "-d",
                "-x",
                "110",
                "-y",
                "36",
                "-c",
                project,
                "-s",
                &name,
                "tail",
                "-f",
                "/dev/null",
            ]);
            assert!(created.status.success(), "tmux start: {}", stderr(&created));
            let binary = shell_quote(&test_binary());
            let extra = extra_args
                .iter()
                .map(|arg| shell_quote_text(arg))
                .collect::<Vec<_>>()
                .join(" ");
            let command = format!(
                "env -i HOME={} PI_CODING_AGENT_DIR={} PI_CODING_AGENT_SESSION_DIR={} PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 PI_TELEMETRY=0 TERM=xterm-256color COLORTERM=truecolor LANG=C.UTF-8 LC_ALL=C.UTF-8 PATH=/usr/bin:/bin {} --approve --provider faux --model faux-1 --thinking off --tui-mode fullscreen --session-dir {} --no-extensions --no-prompt-templates --no-themes --no-context-files {}; exec tail -f /dev/null",
                shell_quote(&sandbox.home),
                shell_quote(&sandbox.agent_dir),
                shell_quote(&sandbox.sessions),
                binary,
                shell_quote(&sandbox.sessions),
                extra,
            );
            let configured = tmux(&["set-option", "-t", &name, "remain-on-exit", "on"]);
            assert!(
                configured.status.success(),
                "tmux remain-on-exit: {}",
                stderr(&configured)
            );
            let launched = tmux(&["respawn-pane", "-k", "-t", &name, &command]);
            assert!(
                launched.status.success(),
                "tmux launch: {}",
                stderr(&launched)
            );
            Self { name }
        }

        fn capture(&self) -> String {
            let output = tmux(&["capture-pane", "-p", "-S", "-", "-t", &self.name]);
            assert!(output.status.success(), "tmux capture: {}", stderr(&output));
            String::from_utf8_lossy(&output.stdout).into_owned()
        }

        fn wait_for<F>(&self, mut predicate: F) -> String
        where
            F: FnMut(&str) -> bool,
        {
            let deadline = Instant::now() + TIMEOUT;
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

        fn send_text(&self, text: &str) {
            let output = tmux(&["send-keys", "-t", &self.name, "-l", "--", text]);
            assert!(output.status.success(), "tmux text: {}", stderr(&output));
            thread::sleep(Duration::from_millis(60));
        }

        fn send_key(&self, key: &str) {
            let output = tmux(&["send-keys", "-t", &self.name, key]);
            assert!(output.status.success(), "tmux key: {}", stderr(&output));
            thread::sleep(Duration::from_millis(80));
        }

        fn send_line(&self, line: &str) {
            self.send_text(line);
            self.send_key("Enter");
        }

        fn quit(&self) {
            self.send_line("/quit");
            thread::sleep(Duration::from_millis(500));
        }
    }

    impl Drop for TmuxSession {
        fn drop(&mut self) {
            let _ = tmux(&["kill-session", "-t", &self.name]);
        }
    }

    fn require_tmux() -> bool {
        if fs::metadata("/dev/ptmx").is_err() {
            eprintln!("SKIP: /dev/ptmx is unavailable");
            return false;
        }
        let output = Command::new("tmux").arg("-V").output();
        if !output.is_ok_and(|output| output.status.success()) {
            eprintln!("SKIP: tmux is unavailable");
            return false;
        }
        true
    }

    #[test]
    fn real_pty_settings_nested_paths_skill_composer_and_restart_persist() {
        let _guard = pty_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !require_tmux() {
            return;
        }
        let sandbox = Sandbox::new();
        let prompt_template = sandbox.prompt_template.to_string_lossy().into_owned();
        let session = TmuxSession::start_with_args(
            &sandbox,
            &["--prompt-template", prompt_template.as_str()],
        );
        session.wait_for(|capture| capture.contains("faux-1"));

        // An explicit template remains available even though this process is
        // launched with --no-prompt-templates. Quoted, positional, all-args,
        // and slice expansion all travel through the real composer/provider.
        session.send_line("/explicit-review \"alpha beta\" gamma");
        let template_expansion = session.wait_for(|capture| {
            visible(capture)
                .contains("INITIAL TEMPLATE FIRST=alpha beta ALL=alpha beta gamma REST=gamma")
        });
        assert!(visible(&template_expansion).contains("REST=gamma"));

        // Real composer autocomplete uses the loaded project skill, then a
        // second Enter submits the selected completion through the agent loop.
        session.send_text("/skill:");
        let completion = session.wait_for(|capture| {
            let screen = visible(capture);
            screen.contains("skill:demo") && screen.contains("A deterministic PTY demo skill")
        });
        assert!(visible(&completion).contains("skill:demo"));
        session.send_key("Enter");
        // Allow the completion selection to settle before the second Enter.
        // The startup skill inventory also contains the skill name, so using
        // that text as a readiness predicate would not prove selection.
        thread::sleep(Duration::from_millis(200));
        session.send_key("Enter");
        let expanded = session.wait_for(|capture| {
            visible(capture).contains("faux response to: <skill name=\"demo\"")
        });
        assert!(visible(&expanded).contains("<skill name=\"demo\""));

        // Replace the skill while the process stays alive. `/reload` must
        // rebuild both autocomplete metadata and invocation content without
        // retaining the stale description/body.
        fs::write(
            sandbox.project.join(".pi/skills/demo/SKILL.md"),
            "---\nname: demo\ndescription: Reloaded deterministic PTY skill\n---\n\nRELOADED SKILL BODY\n",
        )
        .unwrap();
        fs::write(
            &sandbox.prompt_template,
            "---\ndescription: Reloaded deterministic review template\n---\nRELOADED TEMPLATE FIRST=$1 ALL=$@\n",
        )
        .unwrap();
        session.send_line("/reload");
        session.wait_for(|capture| visible(capture).contains("reloaded settings"));
        session.send_line("/explicit-review delta epsilon");
        let reloaded_template = session.wait_for(|capture| {
            visible(capture).contains("RELOADED TEMPLATE FIRST=delta ALL=delta epsilon")
        });
        assert!(visible(&reloaded_template).contains("RELOADED TEMPLATE FIRST=delta"));
        session.send_text("/skill:");
        let reloaded_completion = session.wait_for(|capture| {
            let screen = visible(capture);
            screen.contains("skill:demo") && screen.contains("Reloaded deterministic PTY skill")
        });
        assert!(visible(&reloaded_completion).contains("Reloaded deterministic PTY skill"));
        session.send_key("Enter");
        thread::sleep(Duration::from_millis(200));
        session.send_key("Enter");
        let reloaded_expansion =
            session.wait_for(|capture| visible(capture).contains("RELOADED SKILL BODY"));
        assert!(visible(&reloaded_expansion).contains("RELOADED SKILL BODY"));

        // Give the owner loop a settled composer before opening the next
        // modal, just as a human would.
        thread::sleep(Duration::from_millis(750));

        session.send_line("/settings");
        session.wait_for(|capture| visible(capture).contains("Auto-compact"));

        // Search plus exact one-step Down/Up, followed by a real commit.
        session.send_text("auto");
        session.wait_for(|capture| {
            let screen = visible(capture);
            screen.contains("Auto-compact") && screen.contains("Auto-resize images")
        });
        session.send_key("Down");
        session.wait_for(|capture| visible(capture).contains("→ Auto-resize images"));
        session.send_key("Up");
        session.wait_for(|capture| visible(capture).contains("→ Auto-compact"));
        session.send_key("Enter");
        session.wait_for(|capture| {
            let screen = visible(capture);
            screen.contains("Auto-compact") && screen.contains("true")
        });
        session.send_key("Escape");

        // Warnings: open, cancel without a change, reopen, then commit the
        // second choice through the actual settings callback.
        session.send_line("/settings");
        session.wait_for(|capture| visible(capture).contains("Auto-compact"));
        session.send_text("warnings");
        session.wait_for(|capture| visible(capture).contains("Warnings"));
        session.send_key("Enter");
        session.wait_for(|capture| visible(capture).contains("Anthropic extra usage"));
        session.send_key("Escape");
        session.wait_for(|capture| visible(capture).contains("Type to search"));
        session.send_key("Enter");
        session.wait_for(|capture| visible(capture).contains("Anthropic extra usage"));
        session.send_key("Down");
        session.send_key("Enter");
        session.wait_for(|capture| visible(capture).contains("→ Anthropic extra usage  false"));
        session.send_key("Escape");
        // The warning submenu remains open after a live toggle, so the first
        // Escape returns to the parent list and the second closes /settings.
        session.send_key("Escape");

        // Model-thinking: enter the real two-stage picker and commit the
        // built-in faux model's only supported level, `off`. Non-off provider
        // capability filtering is covered by the selector unit matrix.
        session.send_line("/settings");
        session.wait_for(|capture| visible(capture).contains("Auto-compact"));
        session.send_text("default thinking");
        session.wait_for(|capture| visible(capture).contains("Default thinking level per model"));
        session.send_key("Enter");
        session.wait_for(|capture| visible(capture).contains("Per-Model Thinking Level"));
        session.send_text("faux-1");
        // The faux model is already visible before typing, and capture-pane
        // includes scrollback. Require both the live query and its selected
        // row so this wait proves that filtering processed the literal input.
        session.wait_for(|capture| {
            let screen = visible(capture);
            screen.contains("> faux-1") && screen.contains("→ faux-1 [faux]")
        });
        session.send_key("Enter");
        session.wait_for(|capture| {
            let screen = visible(capture);
            screen.contains("Thinking Level for faux-1 [faux]") && screen.contains("Step 2/2")
        });
        session.send_key("Enter");
        // The stepped submenu loops back to its model stage after the live
        // change. Close the submenu and the parent settings modal, then open
        // a fresh panel to prove the persisted summary is reloaded rather
        // than relying on the submenu's transient callback payload.
        session.send_key("Escape");
        session.send_key("Escape");
        session.send_line("/settings");
        session.wait_for(|capture| visible(capture).contains("Auto-compact"));
        session.send_text("default thinking");
        session.wait_for(|capture| {
            let screen = visible(capture);
            screen.contains("Default thinking level per model") && screen.contains("1 configured")
        });
        session.send_key("Escape");

        // Theme: enter the non-searchable upstream selector, navigate one
        // row to the concrete theme, and commit it so restart can prove the
        // settings file was read again.
        session.send_line("/settings");
        session.wait_for(|capture| visible(capture).contains("Auto-compact"));
        session.send_text("theme");
        session.wait_for(|capture| visible(capture).contains("Theme"));
        session.send_key("Enter");
        session.wait_for(|capture| visible(capture).contains("Select a theme"));
        session.send_key("Up");
        session.wait_for(|capture| visible(capture).contains("→ dark"));
        session.send_key("Enter");
        session.wait_for(|capture| {
            let screen = visible(capture);
            screen.contains("Theme") && screen.contains("dark")
        });
        session.send_key("Escape");

        // TUI mode is a live renderer policy, not merely a persisted restart
        // preference. Switching away from fullscreen must return to the
        // regular scrollback renderer while preserving the running session.
        session.send_line("/settings");
        session.wait_for(|capture| visible(capture).contains("Auto-compact"));
        session.send_text("tui mode");
        session.wait_for(|capture| visible(capture).contains("TUI mode"));
        session.send_key("Enter");
        let mode_changed =
            session.wait_for(|capture| visible(capture).contains("TUI mode: regular"));
        assert!(visible(&mode_changed).contains("TUI mode: regular"));

        session.quit();
        drop(session);

        let saved: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(&sandbox.settings).expect("settings.json should be persisted"),
        )
        .expect("settings.json should remain valid JSON");
        assert_eq!(
            saved.pointer("/compaction/enabled"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            saved.pointer("/warnings/anthropic-extra-usage"),
            Some(&serde_json::json!(false))
        );
        assert_eq!(saved.pointer("/theme"), Some(&serde_json::json!("dark")));
        assert_eq!(
            saved.pointer("/tuiMode"),
            Some(&serde_json::json!("regular"))
        );
        assert_eq!(
            saved
                .get("modelThinkingLevels")
                .and_then(|levels| levels.get("faux/faux-1")),
            Some(&serde_json::json!("off"))
        );

        // A fresh interactive process must read those persisted values.
        let restarted = TmuxSession::start(&sandbox);
        restarted.wait_for(|capture| capture.contains("faux-1"));
        restarted.send_line("/settings");
        restarted.wait_for(|capture| {
            let screen = visible(capture);
            screen.contains("Auto-compact") && screen.contains("true")
        });
        restarted.send_text("warnings");
        restarted.wait_for(|capture| visible(capture).contains("Warnings"));
        restarted.send_key("Enter");
        let warning_state = restarted
            .wait_for(|capture| visible(capture).contains("→ Anthropic extra usage  false"));
        assert!(visible(&warning_state).contains("false"));
        restarted.send_key("Escape");
        restarted.send_key("Escape");
        restarted.send_line("/quit");
    }
}
