#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Focused parity checks for configuration/resource discovery seams.
//!
//! These use real temporary filesystem trees and the public loaders; no
//! provider or network behavior is simulated here.

use std::path::{Path, PathBuf};

use pi_coding_agent::core::prompt_templates::load_prompt_templates;
use pi_coding_agent::core::skills::{load_skills, LoadSkillsOptions};

fn temp_root(tag: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("pi-config-residual-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).expect("create temporary test root");
    root
}

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().expect("test file parent")).expect("create test parent");
    std::fs::write(path, content).expect("write test file");
}

const SKILL: &str = "---\nname: project-skill\ndescription: A project skill\n---\n\nBody\n";

#[test]
fn skill_discovery_isolates_scope_ignore_rules_and_matches_case_sensitive_md() {
    let root = temp_root("skills");
    let user_skills = root.join("agent/skills");
    let project_skills = root.join("project/.pi/skills");

    // This rule must stay scoped to the user root. The project has a file with
    // the same relative name and upstream loads that project file.
    write(&user_skills.join(".gitignore"), "ignored.md\n");
    write(
        &user_skills.join("ignored.md"),
        "---\nname: user-skill\ndescription: User skill\n---\nuser\n",
    );
    write(&project_skills.join("ignored.md"), SKILL);
    write(
        &project_skills.join("README.MD"),
        "---\nname: uppercase\ndescription: Uppercase extension\n---\nbody\n",
    );

    let (skills, diagnostics) = load_skills(LoadSkillsOptions {
        cwd: root.join("project").to_string_lossy().into_owned(),
        agent_dir: root.join("agent").to_string_lossy().into_owned(),
        skill_paths: Vec::new(),
    });

    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "project-skill");
    std::fs::remove_dir_all(root).expect("remove temporary test root");
}

#[test]
fn explicit_prompt_paths_trim_cli_whitespace_and_ignore_missing_paths() {
    let root = temp_root("prompts");
    let prompt = root.join("hello.md");
    write(&prompt, "---\ndescription: Greeting\n---\nHello, $1!\n");

    let (templates, diagnostics) = load_prompt_templates(
        &root.to_string_lossy(),
        &root.join("agent").to_string_lossy(),
        &[
            format!("  {}  ", prompt.display()),
            root.join("missing.md").to_string_lossy().into_owned(),
        ],
        false,
        false,
    );

    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].name, "hello");
    assert_eq!(templates[0].content.trim(), "Hello, $1!");
    std::fs::remove_dir_all(root).expect("remove temporary test root");
}
