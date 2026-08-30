#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Real tmux coverage for the interactive app actions added by this slice.
//! No provider response is fabricated here: this test exercises editor and
//! clipboard actions before a normal faux-provider prompt and then restarts.

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use std::thread;
    use std::time::{Duration, Instant};

    struct Sandbox {
        root: PathBuf,
        home: PathBuf,
        agent: PathBuf,
        project: PathBuf,
        editor: PathBuf,
        raw: PathBuf,
    }

    impl Sandbox {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "pi-interactive-editor-clipboard-{}",
                uuid::Uuid::new_v4()
            ));
            let home = root.join("home");
            let agent = root.join("agent");
            let project = root.join("project");
            let editor = root.join("editor.sh");
            let raw = root.join("raw.log");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&agent).unwrap();
            fs::create_dir_all(&project).unwrap();
            fs::write(
                &editor,
                "#!/bin/sh\nprintf 'from-external-editor\\n' > \"$1\"\n",
            )
            .unwrap();
            fs::set_permissions(&editor, fs::Permissions::from_mode(0o700)).unwrap();
            Self {
                root,
                home,
                agent,
                project,
                editor,
                raw,
            }
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    struct Session {
        name: String,
    }

    impl Session {
        fn start(sandbox: &Sandbox) -> Self {
            let name = format!("pi-editor-{}", uuid::Uuid::new_v4());
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
                "tmux create: {}",
                stderr(&created)
            );
            let pipe = format!("cat > {}", shell_quote(&sandbox.raw));
            let piped = tmux(&["pipe-pane", "-o", "-t", &name, &pipe]);
            assert!(piped.status.success(), "tmux pipe: {}", stderr(&piped));
            let binary = std::env::var_os("PI_RUST_TEST_BINARY")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pi")));
            let command = format!(
                "env HOME={} PI_CODING_AGENT_DIR={} PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 VISUAL= EDITOR={} DISPLAY= WAYLAND_DISPLAY= XDG_SESSION_TYPE=x11 PI_DISABLE_OSC52=1 PATH=/usr/bin:/bin {} --approve --provider faux --model faux-1 --tui-mode fullscreen; exec tail -f /dev/null",
                shell_quote(&sandbox.home),
                shell_quote(&sandbox.agent),
                shell_quote(&sandbox.editor),
                shell_quote(&binary),
            );
            let configured = tmux(&["set-option", "-t", &name, "remain-on-exit", "on"]);
            assert!(
                configured.status.success(),
                "tmux remain: {}",
                stderr(&configured)
            );
            let started = tmux(&["respawn-pane", "-k", "-t", &name, &command]);
            assert!(started.status.success(), "tmux start: {}", stderr(&started));
            Self { name }
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
            let deadline = Instant::now() + Duration::from_secs(12);
            loop {
                let capture = self.capture();
                if predicate(&capture) {
                    return capture;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for TUI output; last capture:\n{capture}"
                );
                thread::sleep(Duration::from_millis(30));
            }
        }

        fn send_line(&self, line: &str) {
            let output = tmux(&["send-keys", "-t", &self.name, "-l", "--", line]);
            assert!(output.status.success(), "send line: {}", stderr(&output));
            let output = tmux(&["send-keys", "-t", &self.name, "Enter"]);
            assert!(output.status.success(), "send enter: {}", stderr(&output));
        }

        fn send_key(&self, key: &str) {
            let output = tmux(&["send-keys", "-t", &self.name, key]);
            assert!(
                output.status.success(),
                "send key {key}: {}",
                stderr(&output)
            );
        }

        fn wait_for_raw(&self, sandbox: &Sandbox, needle: &str) {
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                let raw = fs::read_to_string(&sandbox.raw).unwrap_or_default();
                if raw.contains(needle) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "missing raw PTY sequence {needle:?}"
                );
                thread::sleep(Duration::from_millis(30));
            }
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            let _ = tmux(&["kill-session", "-t", &self.name]);
        }
    }

    fn tmux(args: &[&str]) -> Output {
        Command::new("tmux")
            .args(args)
            .output()
            .expect("tmux is required for interactive PTY coverage")
    }

    fn stderr(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).trim().to_string()
    }

    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
    }

    #[test]
    fn external_editor_clipboard_failure_and_restart_are_real() {
        let sandbox = Sandbox::new();
        let session = Session::start(&sandbox);
        session.wait_for(|capture| capture.contains("escape interrupt"));

        session.send_line("before external editor");
        session.wait_for(|capture| capture.contains("faux response to: before external editor"));
        thread::sleep(Duration::from_millis(750));

        session.send_key("C-g");
        session.wait_for(|capture| capture.contains("external editor complete"));
        session.send_key("Enter");
        session.wait_for(|capture| capture.contains("faux response to: from-external-editor"));

        // No DISPLAY/WAYLAND_DISPLAY and no OSC52 terminal backend are set in
        // the child, so this must be a truthful, deterministic failure.
        session.send_key("C-v");
        session.wait_for(|capture| capture.contains("clipboard unavailable"));

        session.send_line("/new");
        session.wait_for(|capture| capture.contains("started new session"));
        session.send_line("after restart");
        session.wait_for(|capture| capture.contains("faux response to: after restart"));
        session.send_line("/quit");
        session.wait_for_raw(&sandbox, "\x1b[?1049l");
    }
}

#[cfg(not(unix))]
#[test]
fn interactive_editor_clipboard_pty_requires_unix_tmux() {}
