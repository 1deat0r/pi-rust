#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

use std::sync::atomic::AtomicBool;

use pi_coding_agent::core::skills::{load_skills, LoadSkillsOptions};
use pi_coding_agent::interactive::{build_autocomplete_provider_with_skills, expand_skill_command};
use pi_tui::autocomplete::AutocompleteProvider;

fn temp_root() -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "pi-interactive-skill-commands-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn skill_commands_use_loaded_descriptions_and_follow_the_setting() {
    let root = temp_root();
    let skill_dir = root.join("skills/demo");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let skill_path = skill_dir.join("SKILL.md");
    std::fs::write(
        &skill_path,
        "\u{feff}---\nname: demo\ndescription: Loaded demo description\ndisable-model-invocation: true\n---\n\nFollow the demo procedure.\n",
    )
    .unwrap();

    let (skills, diagnostics) = load_skills(LoadSkillsOptions {
        cwd: root.to_string_lossy().into_owned(),
        agent_dir: root.join("empty-agent").to_string_lossy().into_owned(),
        skill_paths: vec![root.join("skills").to_string_lossy().into_owned()],
    });
    assert!(
        diagnostics.is_empty(),
        "unexpected skill diagnostics: {diagnostics:?}"
    );
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "demo");
    assert!(skills[0].disable_model_invocation);

    let enabled =
        build_autocomplete_provider_with_skills(root.to_string_lossy().into_owned(), &skills, true);
    let suggestions = enabled
        .get_suggestions(
            &["/skill:".to_string()],
            0,
            "/skill:".len(),
            false,
            &AtomicBool::new(false),
        )
        .expect("enabled skill command should autocomplete");
    let item = suggestions
        .items
        .iter()
        .find(|item| item.value == "skill:demo")
        .expect("loaded skill command missing");
    assert_eq!(
        item.description.as_deref(),
        Some("[t] Loaded demo description")
    );

    let disabled = build_autocomplete_provider_with_skills(
        root.to_string_lossy().into_owned(),
        &skills,
        false,
    );
    assert!(
        disabled
            .get_suggestions(
                &["/skill:".to_string()],
                0,
                "/skill:".len(),
                false,
                &AtomicBool::new(false),
            )
            .is_none(),
        "disabled skill commands must be absent"
    );

    let expanded = expand_skill_command("/skill:demo extra instructions", &skills);
    assert!(expanded.contains("<skill name=\"demo\""));
    assert!(expanded.contains("Follow the demo procedure."));
    assert!(!expanded.contains("disable-model-invocation"));
    assert!(expanded.ends_with("extra instructions"));
    assert_eq!(
        expand_skill_command("/skill:missing", &skills),
        "/skill:missing"
    );

    let _ = std::fs::remove_dir_all(root);
}
