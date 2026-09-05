#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Real PTY proof that an active interactive process recomposes models.json
//! through Pi's explicit `/reload` boundary and uses the refreshed model on a
//! subsequent turn.

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use std::sync::{Mutex, OnceLock};
    use std::thread;
    use std::time::{Duration, Instant};

    fn pty_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn test_binary() -> PathBuf {
        std::env::var_os("PI_RUST_TEST_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pi")))
    }

    struct Sandbox {
        root: PathBuf,
        home: PathBuf,
        agent: PathBuf,
        project: PathBuf,
        models: PathBuf,
    }

    impl Sandbox {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "pi-models-json-live-reload-{}",
                uuid::Uuid::new_v4()
            ));
            let home = root.join("home");
            let agent = home.join(".pi").join("agent");
            let project = root.join("project");
            let models = agent.join("models.json");
            fs::create_dir_all(&agent).unwrap();
            fs::create_dir_all(&project).unwrap();
            Self {
                root,
                home,
                agent,
                project,
                models,
            }
        }

        fn write_model_name(&self, name: &str) {
            fs::write(
                &self.models,
                serde_json::to_vec_pretty(&serde_json::json!({
                    "providers": {
                        "faux": {
                            "modelOverrides": {
                                "faux-1": {"name": name}
                            }
                        }
                    }
                }))
                .unwrap(),
            )
            .unwrap();
        }

        fn session_contains(&self, needle: &str) -> bool {
            fn visit(path: &Path, needle: &str) -> bool {
                let Ok(entries) = fs::read_dir(path) else {
                    return false;
                };
                entries.flatten().any(|entry| {
                    let path = entry.path();
                    if path.is_dir() {
                        visit(&path, needle)
                    } else {
                        fs::read_to_string(path)
                            .map(|content| content.contains(needle))
                            .unwrap_or(false)
                    }
                })
            }
            visit(&self.agent.join("sessions"), needle)
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
            let name = format!("pi-model-reload-{}", uuid::Uuid::new_v4());
            assert_success(
                tmux(&[
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
                ]),
                "tmux new-session",
            );
            let binary = test_binary();
            assert!(binary.is_file(), "missing pi binary: {}", binary.display());
            let command = format!(
                "env HOME={} PI_CODING_AGENT_DIR={} PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 PATH=/usr/bin:/bin {} --approve --provider faux --model faux-1 --tui-mode fullscreen; exec tail -f /dev/null",
                shell_quote(&sandbox.home),
                shell_quote(&sandbox.agent),
                shell_quote(&binary),
            );
            assert_success(
                tmux(&["respawn-pane", "-k", "-t", &name, &command]),
                "tmux respawn-pane",
            );
            Self { name }
        }

        fn capture(&self) -> String {
            let output = tmux(&["capture-pane", "-p", "-t", &self.name]);
            assert_success_ref(&output, "tmux capture-pane");
            String::from_utf8_lossy(&output.stdout).into_owned()
        }

        #[track_caller]
        fn wait_for<F>(&self, mut predicate: F) -> String
        where
            F: FnMut(&str) -> bool,
        {
            let caller = std::panic::Location::caller();
            let deadline = Instant::now() + Duration::from_secs(12);
            loop {
                let capture = self.capture();
                if predicate(&capture) {
                    return capture;
                }
                assert!(
                    Instant::now() < deadline,
                    "PTY timeout at {}:{}; last capture:\n{capture}",
                    caller.file(),
                    caller.line()
                );
                thread::sleep(Duration::from_millis(25));
            }
        }

        fn send_line(&self, line: &str) {
            assert_success(
                tmux(&["send-keys", "-t", &self.name, "-l", "--", line]),
                "tmux literal input",
            );
            thread::sleep(Duration::from_millis(80));
            assert_success(
                tmux(&["send-keys", "-t", &self.name, "Enter"]),
                "tmux enter",
            );
        }

        fn send_key(&self, key: &str) {
            assert_success(tmux(&["send-keys", "-t", &self.name, key]), "tmux key");
        }
    }

    impl Drop for TmuxSession {
        fn drop(&mut self) {
            let _ = tmux(&["kill-session", "-t", &self.name]);
        }
    }

    #[test]
    fn models_json_reload_updates_active_model_and_next_turn_without_restart() {
        let _guard = pty_lock().lock().unwrap();
        let sandbox = Sandbox::new();
        sandbox.write_model_name("Before Reload");
        let session = TmuxSession::start(&sandbox);
        session.wait_for(|capture| capture.contains("faux-1"));

        session.send_line("/model");
        session.wait_for(|capture| capture.contains("Before Reload"));
        session.send_key("Escape");
        session.wait_for(|capture| !capture.contains("Before Reload"));
        thread::sleep(Duration::from_millis(150));

        sandbox.write_model_name("After Reload");
        let before_reload = session.capture();
        session.send_line("/reload");
        session
            .wait_for(|capture| capture != before_reload && capture.contains("reloaded settings"));
        session.send_line("/model");
        session.wait_for(|capture| {
            capture.contains("After Reload") && !capture.contains("Before Reload")
        });
        session.send_key("Enter");

        let prompt = "same process refreshed turn";
        session.send_line(prompt);
        session
            .wait_for(|capture| capture.contains("faux response to: same process refreshed turn"));
        session.send_line("/quit");

        let deadline = Instant::now() + Duration::from_secs(5);
        while !sandbox.session_contains(prompt) {
            assert!(
                Instant::now() < deadline,
                "refreshed subsequent turn was not persisted"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn tmux(args: &[&str]) -> Output {
        Command::new("tmux")
            .args(args)
            .output()
            .expect("tmux must be installed")
    }

    fn assert_success(output: Output, operation: &str) {
        assert_success_ref(&output, operation);
    }

    fn assert_success_ref(output: &Output, operation: &str) {
        assert!(
            output.status.success(),
            "{operation} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn shell_quote(path: &Path) -> String {
        let value = path.to_string_lossy();
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}
