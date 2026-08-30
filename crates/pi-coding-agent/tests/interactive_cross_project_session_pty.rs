#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Real-PTY coverage for interactive `--session` when the selected session
//! belongs to another project.  The source JSONL is a deterministic fixture;
//! no provider turn is used to manufacture it.

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
        sessions: PathBuf,
        current: PathBuf,
        source_file: PathBuf,
    }

    impl Sandbox {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "pi-cross-project-session-{tag}-{}",
                uuid::Uuid::new_v4()
            ));
            let home = root.join("home");
            let agent_dir = home.join(".pi").join("agent");
            let sessions = root.join("sessions");
            let current = root.join("current-project");
            let source = root.join("source-project");
            let source_dir = sessions.join(session_directory_name(&source));
            let source_file = source_dir.join("2026-08-29T00-00-00-000Z_cross-source.jsonl");
            fs::create_dir_all(&agent_dir).unwrap();
            fs::create_dir_all(&current).unwrap();
            fs::create_dir_all(&source).unwrap();
            fs::create_dir_all(source_dir).unwrap();
            fs::write(&source_file, source_session(&source)).unwrap();
            Self {
                root,
                home,
                agent_dir,
                sessions,
                current,
                source_file,
            }
        }

        fn session_files(&self) -> Vec<PathBuf> {
            let mut files = Vec::new();
            collect_jsonl(&self.sessions, &mut files);
            files.sort();
            files
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn session_directory_name(cwd: &Path) -> String {
        let cwd = cwd.to_string_lossy();
        let stripped = cwd.strip_prefix('/').unwrap_or(cwd.as_ref());
        let replaced = stripped.replace(['/', '\\', ':'], "-");
        format!("--{replaced}--")
    }

    fn source_session(source: &Path) -> String {
        let cwd = source.to_string_lossy();
        let header = serde_json::json!({
            "type": "session",
            "version": 3,
            "id": "cross-source",
            "timestamp": "2026-08-29T00:00:00.000Z",
            "cwd": cwd,
        });
        let message = serde_json::json!({
            "type": "message",
            "id": "cross-source-user",
            "parentId": null,
            "timestamp": "2026-08-29T00:00:01.000Z",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": "cross-project preserved transcript"}],
            },
        });
        format!("{}\n{}\n", header, message)
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

    fn tmux(args: &[&str]) -> Output {
        Command::new("tmux")
            .args(args)
            .output()
            .expect("tmux must be installed for cross-project PTY coverage")
    }

    fn shell_quote(value: &Path) -> String {
        let value = value.to_string_lossy();
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    fn binary() -> PathBuf {
        std::env::var_os("PI_RUST_TEST_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pi")))
    }

    struct TmuxSession {
        name: String,
    }

    impl TmuxSession {
        fn start(sandbox: &Sandbox) -> Self {
            let name = format!("pi-cross-project-{}", uuid::Uuid::new_v4());
            let created = tmux(&[
                "new-session",
                "-d",
                "-x",
                "110",
                "-y",
                "34",
                "-c",
                sandbox.current.to_str().unwrap(),
                "-s",
                &name,
                "tail",
                "-f",
                "/dev/null",
            ]);
            assert!(
                created.status.success(),
                "tmux new-session failed: {created:?}"
            );
            let command = format!(
                "env HOME={} PI_CODING_AGENT_DIR={} PI_CODING_AGENT_SESSION_DIR={} PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 PI_OAUTH_NO_BROWSER=1 PATH=/usr/bin:/bin {} --approve --provider faux --model faux-1 --tui-mode fullscreen --session {};",
                shell_quote(&sandbox.home),
                shell_quote(&sandbox.agent_dir),
                shell_quote(&sandbox.sessions),
                shell_quote(&binary()),
                shell_quote(&sandbox.source_file),
            );
            let configured = tmux(&["set-option", "-t", &name, "remain-on-exit", "on"]);
            assert!(
                configured.status.success(),
                "tmux remain-on-exit failed: {configured:?}"
            );
            let started = tmux(&["respawn-pane", "-k", "-t", &name, &command]);
            assert!(started.status.success(), "tmux launch failed: {started:?}");
            Self { name }
        }

        fn capture(&self) -> String {
            let output = tmux(&["capture-pane", "-p", "-t", &self.name]);
            assert!(output.status.success(), "tmux capture failed: {output:?}");
            String::from_utf8_lossy(&output.stdout).into_owned()
        }

        fn wait_for<F>(&self, mut predicate: F) -> String
        where
            F: FnMut(&str) -> bool,
        {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                let capture = self.capture();
                if predicate(&capture) {
                    return capture;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for PTY state; last capture:\n{capture}"
                );
                thread::sleep(Duration::from_millis(40));
            }
        }

        fn send_key(&self, key: &str) {
            let output = tmux(&["send-keys", "-t", &self.name, key]);
            assert!(output.status.success(), "tmux key failed: {output:?}");
        }

        fn send_line(&self, line: &str) {
            let output = tmux(&["send-keys", "-t", &self.name, "-l", "--", line]);
            assert!(output.status.success(), "tmux text failed: {output:?}");
            self.send_key("Enter");
        }

        fn wait_for_exit(&self) {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let output = tmux(&["list-panes", "-t", &self.name, "-F", "#{pane_dead}"]);
                let dead = String::from_utf8_lossy(&output.stdout).trim() == "1";
                if dead {
                    return;
                }
                assert!(Instant::now() < deadline, "PTY process did not exit");
                thread::sleep(Duration::from_millis(40));
            }
        }
    }

    impl Drop for TmuxSession {
        fn drop(&mut self) {
            let _ = tmux(&["kill-session", "-t", &self.name]);
        }
    }

    fn child_with_parent_and_text(sandbox: &Sandbox) -> (PathBuf, serde_json::Value, String) {
        let children = sandbox
            .session_files()
            .into_iter()
            .filter(|path| path != &sandbox.source_file)
            .collect::<Vec<_>>();
        assert_eq!(children.len(), 1, "expected one forked child: {children:?}");
        let child = children.into_iter().next().unwrap();
        let content = fs::read_to_string(&child).unwrap();
        let header = serde_json::from_str::<serde_json::Value>(
            content.lines().next().expect("child header"),
        )
        .unwrap();
        (child, header, content)
    }

    #[test]
    fn cross_project_session_cancel_exits_without_fork() {
        let sandbox = Sandbox::new("cancel");
        assert_eq!(sandbox.session_files(), vec![sandbox.source_file.clone()]);
        let session = TmuxSession::start(&sandbox);
        session.wait_for(|capture| {
            capture.contains("Session found in different project")
                && capture.contains("Fork this session into current directory? [y/N]")
        });
        session.send_key("Enter");
        session.wait_for_exit();
        assert_eq!(sandbox.session_files(), vec![sandbox.source_file.clone()]);
        assert!(session.capture().contains("Aborted."));
    }

    #[test]
    fn cross_project_session_yes_forks_to_current_project_with_parent_and_transcript() {
        let sandbox = Sandbox::new("yes");
        let session = TmuxSession::start(&sandbox);
        session.wait_for(|capture| {
            capture.contains("Fork this session into current directory? [y/N]")
        });
        session.send_key("y");
        session.wait_for(|capture| capture.contains("forked session cross-so"));
        let (child, header, content) = child_with_parent_and_text(&sandbox);
        assert_eq!(
            header["cwd"].as_str(),
            Some(sandbox.current.to_str().unwrap())
        );
        let parent = header
            .get("parentSessionId")
            .or_else(|| header.get("parentSession"))
            .and_then(serde_json::Value::as_str);
        assert_eq!(parent, Some("cross-source"));
        assert!(content.contains("cross-project preserved transcript"));
        assert!(child.starts_with(
            sandbox
                .sessions
                .join(session_directory_name(&sandbox.current))
        ));
        session.send_line("/quit");
        session.wait_for_exit();
    }
}

#[cfg(not(unix))]
#[test]
fn cross_project_session_pty_requires_unix() {}
