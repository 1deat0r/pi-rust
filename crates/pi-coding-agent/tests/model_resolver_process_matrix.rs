#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Real child-process coverage for the shared model resolver across text and
//! JSON callers. All successful turns use the native faux provider; configured
//! duplicate providers use synthetic credentials and never make a request.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    agent: PathBuf,
    sessions: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "pi-model-resolver-process-{tag}-{}",
            uuid::Uuid::new_v4()
        ));
        let home = root.join("home");
        let agent = root.join("agent");
        let sessions = root.join("sessions");
        for path in [&home, &agent, &sessions] {
            fs::create_dir_all(path).unwrap();
        }
        Self {
            root,
            home,
            agent,
            sessions,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pi"))
            .env_clear()
            .current_dir(&self.root)
            .env("HOME", &self.home)
            .env("PI_CODING_AGENT_DIR", &self.agent)
            .env("PI_CODING_AGENT_SESSION_DIR", &self.sessions)
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env("LC_ALL", "C")
            .env("PATH", "/usr/bin:/bin")
            .args(args)
            .output()
            .expect("spawn pi")
    }

    fn assert_faux_turn(&self, args: &[&str], expected_prompt: &str) {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "args={args:?}\nstderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("faux response to: {expected_prompt}\n")
        );
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn text_mode_resolves_exact_case_provider_thinking_glob_and_custom_fallback() {
    let sandbox = Sandbox::new("text-success");
    sandbox.assert_faux_turn(
        &[
            "--print",
            "--no-session",
            "--provider",
            "FaUx",
            "--model",
            "FAUX-1",
            "case exact",
        ],
        "case exact",
    );
    sandbox.assert_faux_turn(
        &[
            "--print",
            "--no-session",
            "--model",
            "FAUX/FAUX-1",
            "provider scoped",
        ],
        "provider scoped",
    );
    sandbox.assert_faux_turn(
        &[
            "--print",
            "--no-session",
            "--provider",
            "faux",
            "--model",
            "faux-1:high",
            "thinking suffix",
        ],
        "thinking suffix",
    );
    sandbox.assert_faux_turn(
        &[
            "--print",
            "--no-session",
            "--provider",
            "faux",
            "--models",
            "FAUX-*",
            "glob scope",
        ],
        "glob scope",
    );

    let output = sandbox.run(&[
        "--print",
        "--no-session",
        "--provider",
        "faux",
        "--model",
        "not-in-catalog",
        "custom fallback",
    ]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "faux response to: custom fallback\n"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains(
        "Model \"not-in-catalog\" not found for provider \"faux\". Using custom model id."
    ));
}

#[test]
fn text_mode_rejects_unknown_provider_and_ambiguous_bare_model() {
    let sandbox = Sandbox::new("text-errors");
    let output = sandbox.run(&[
        "--print",
        "--no-session",
        "--provider",
        "missing-provider",
        "--model",
        "anything",
        "hello",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stdout).is_empty());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "Unknown provider \"missing-provider\". Use --list-models to see available providers/models.\n"
    );

    fs::write(
        sandbox.agent.join("models.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "providers": {
                "duplicate-one": {
                    "baseUrl": "http://127.0.0.1:9/v1",
                    "api": "openai-completions",
                    "apiKey": "synthetic-one-no-request",
                    "models": [{"id": "duplicate-id", "name": "Duplicate one"}]
                },
                "duplicate-two": {
                    "baseUrl": "http://127.0.0.1:9/v1",
                    "api": "openai-completions",
                    "apiKey": "synthetic-two-no-request",
                    "models": [{"id": "duplicate-id", "name": "Duplicate two"}]
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let output = sandbox.run(&[
        "--print",
        "--no-session",
        "--model",
        "duplicate-id",
        "ambiguous",
    ]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Model \"duplicate-id\" is ambiguous across providers:"));
    assert!(stderr.contains("duplicate-one/duplicate-id"));
    assert!(stderr.contains("duplicate-two/duplicate-id"));
    assert!(stderr.contains("More than one matching provider is authenticated."));
}

#[test]
fn json_mode_uses_the_same_case_and_thinking_suffix_resolution() {
    let sandbox = Sandbox::new("json");
    let output = sandbox.run(&[
        "--mode",
        "json",
        "--no-session",
        "--provider",
        "FaUx",
        "--model",
        "FAUX-1:high",
        "json resolver",
    ]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let assistant = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|record| record["type"] == "message_end" && record["message"]["role"] == "assistant")
        .expect("assistant message_end");
    assert_eq!(assistant["message"]["provider"], "faux");
    assert_eq!(assistant["message"]["model"], "faux-1");
}
