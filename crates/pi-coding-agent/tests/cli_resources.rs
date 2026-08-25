//! Binary-level resource tests (T6 #73/#74): `--skill`/`-ns` and
//! `--prompt-template`/`-np` wiring in the run path, verified through the
//! built `pi` binary.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    sessions: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("pi-resources-{tag}-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let sessions = root.join("sessions");
        fs::create_dir_all(home.join(".pi").join("agent")).unwrap();
        fs::create_dir_all(&sessions).unwrap();
        Self {
            root,
            home,
            sessions,
        }
    }

    fn pi(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_pi"))
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("PI_CODING_AGENT_SESSION_DIR", &self.sessions)
            .env_remove("PI_PROVIDER")
            .env_remove("PI_MODEL")
            .env_remove("PI_KEY")
            .env_remove("PI_SESSION_ID")
            .args(args)
            .output()
            .expect("spawn pi")
    }

    fn stdout(&self, output: &std::process::Output) -> String {
        String::from_utf8_lossy(&output.stdout).to_string()
    }
    fn stderr(&self, output: &std::process::Output) -> String {
        String::from_utf8_lossy(&output.stderr).to_string()
    }

    /// Concatenated content of every JSONL session file written under
    /// `sessions`.
    fn session_content(&self) -> String {
        walk_jsonl(&self.sessions)
            .iter()
            .map(|path| fs::read_to_string(path).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn walk_jsonl(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                out.extend(walk_jsonl(&p));
            } else if p.extension().map(|e| e == "jsonl").unwrap_or(false) {
                out.push(p);
            }
        }
    }
    out
}

#[test]
fn prompt_template_is_expanded_in_run_path() {
    let sandbox = Sandbox::new("prompt-template");
    let cwd = sandbox.root.join("proj");
    fs::create_dir_all(cwd.join(".pi").join("prompts")).unwrap();
    fs::write(
        cwd.join(".pi").join("prompts").join("summarize.md"),
        "---\ndescription: Summarize\n---\nSummarize the following: $@",
    )
    .unwrap();

    let out = sandbox.pi(
        &cwd,
        &[
            "-p",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "/summarize the docs",
        ],
    );
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    // The expanded text (not the literal `/template`) is what reaches the
    // agent, so it appears in the persisted session user message.
    let session = sandbox.session_content();
    assert!(
        session.contains("Summarize the following: the docs"),
        "expected expanded template in session, got:\n{session}"
    );
    assert!(
        !session.contains("\"/summarize the docs\""),
        "literal template should not be persisted:\n{session}"
    );
}

#[test]
fn global_prompt_template_is_discovered_in_run_path() {
    let sandbox = Sandbox::new("global-prompt-template");
    let cwd = sandbox.root.join("proj");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(sandbox.home.join(".pi").join("agent").join("prompts")).unwrap();
    fs::write(
        sandbox
            .home
            .join(".pi")
            .join("agent")
            .join("prompts")
            .join("global.md"),
        "---\ndescription: Global template\n---\nGlobal template: $@",
    )
    .unwrap();

    let out = sandbox.pi(
        &cwd,
        &[
            "-p",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "/global the docs",
        ],
    );
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    let session = sandbox.session_content();
    assert!(
        session.contains("Global template: the docs"),
        "expected global template expansion in session, got:\n{session}"
    );
    assert!(
        !session.contains("\"/global the docs\""),
        "literal global template invocation should not be persisted:\n{session}"
    );
}

#[test]
fn no_prompt_templates_skips_expansion() {
    let sandbox = Sandbox::new("no-prompt");
    let cwd = sandbox.root.join("proj");
    fs::create_dir_all(cwd.join(".pi").join("prompts")).unwrap();
    fs::write(
        cwd.join(".pi").join("prompts").join("summarize.md"),
        "---\ndescription: Summarize\n---\nSummarize the following: $@",
    )
    .unwrap();

    let out = sandbox.pi(
        &cwd,
        &[
            "-p",
            "--no-prompt-templates",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "/summarize the docs",
        ],
    );
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    // Expansion is disabled: the literal "/summarize the docs" reaches the agent.
    let session = sandbox.session_content();
    assert!(
        session.contains("/summarize the docs"),
        "expected literal prompt when -np, got:\n{session}"
    );
}

#[test]
fn no_approve_does_not_block_project_prompt_expansion_in_print_path() {
    let sandbox = Sandbox::new("untrusted-project-prompt");
    let cwd = sandbox.root.join("proj");
    fs::create_dir_all(cwd.join(".pi").join("prompts")).unwrap();
    fs::write(
        cwd.join(".pi").join("prompts").join("summarize.md"),
        "---\ndescription: Summarize\n---\nSummarize the following: $@",
    )
    .unwrap();

    let out = sandbox.pi(
        &cwd,
        &[
            "-p",
            "--no-approve",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "/summarize the docs",
        ],
    );
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    let session = sandbox.session_content();
    assert!(
        session.contains("Summarize the following: the docs"),
        "current print path still expands project templates under --no-approve:\n{session}"
    );
}

#[test]
fn no_prompt_templates_overrides_an_explicit_template_path() {
    let sandbox = Sandbox::new("no-prompt-explicit");
    let cwd = sandbox.root.join("proj");
    fs::create_dir_all(&cwd).unwrap();
    let template = sandbox.root.join("explicit.md");
    fs::write(
        &template,
        "---\ndescription: Explicit template\n---\nExplicit template: $@",
    )
    .unwrap();

    let out = sandbox.pi(
        &cwd,
        &[
            "-p",
            "--no-prompt-templates",
            "--prompt-template",
            &template.to_string_lossy(),
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "/explicit the docs",
        ],
    );
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    let session = sandbox.session_content();
    assert!(
        session.contains("/explicit the docs"),
        "expected literal prompt when -np suppresses explicit templates, got:\n{session}"
    );
    assert!(
        !session.contains("Explicit template: the docs"),
        "explicit template should not expand under -np:\n{session}"
    );
}

#[test]
fn missing_prompt_template_path_is_nonfatal_and_keeps_literal_prompt() {
    let sandbox = Sandbox::new("missing-prompt-template");
    let cwd = sandbox.root.join("proj");
    fs::create_dir_all(&cwd).unwrap();
    let missing = sandbox.root.join("missing.md");

    let out = sandbox.pi(
        &cwd,
        &[
            "-p",
            "--prompt-template",
            &missing.to_string_lossy(),
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "/missing the docs",
        ],
    );
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    let session = sandbox.session_content();
    assert!(
        session.contains("/missing the docs"),
        "missing template must leave the literal prompt unchanged:\n{session}"
    );
}

#[test]
fn no_session_keeps_a_successful_run_ephemeral() {
    let sandbox = Sandbox::new("no-session");
    let cwd = sandbox.root.join("proj");
    fs::create_dir_all(&cwd).unwrap();

    let out = sandbox.pi(
        &cwd,
        &[
            "-p",
            "--no-session",
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "hello",
        ],
    );
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    assert!(sandbox.stdout(&out).contains("faux response"));
    assert!(
        walk_jsonl(&sandbox.sessions).is_empty(),
        "--no-session must not persist JSONL sessions"
    );
}

#[test]
fn missing_session_path_reports_a_deterministic_error() {
    let sandbox = Sandbox::new("missing-session");
    let cwd = sandbox.root.join("proj");
    fs::create_dir_all(&cwd).unwrap();
    let missing = sandbox.root.join("missing-session.jsonl");

    let out = sandbox.pi(
        &cwd,
        &[
            "-p",
            "--session",
            &missing.to_string_lossy(),
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "hello",
        ],
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        sandbox.stderr(&out),
        format!("session not found: {}\n", missing.display())
    );
    assert!(sandbox.stdout(&out).is_empty());
}

#[test]
fn skill_path_loads_without_error() {
    let sandbox = Sandbox::new("skill");
    let cwd = sandbox.root.join("proj");
    fs::create_dir_all(cwd.join("skills").join("my-skill")).unwrap();
    fs::write(
        cwd.join("skills").join("my-skill").join("SKILL.md"),
        "---\nname: my-skill\ndescription: A test skill\n---\nSkill body",
    )
    .unwrap();

    let out = sandbox.pi(
        &cwd,
        &[
            "-p",
            "--no-session",
            "--skill",
            &cwd.join("skills").to_string_lossy(),
            "--provider",
            "faux",
            "--model",
            "faux-1",
            "hello",
        ],
    );
    assert!(out.status.success(), "stderr: {}", sandbox.stderr(&out));
    assert!(sandbox.stdout(&out).contains("faux response to: hello"));
}
