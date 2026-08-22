//! System prompt helpers — port of `packages/agent/src/harness/system-prompt.ts`.

use crate::types::Skill;

/// Escape XML special characters (upstream `escapeXml`).
pub fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Format skills into the `<available_skills>` block used in the system
/// prompt (upstream `formatSkillsForSystemPrompt`). Skills with
/// `disable_model_invocation` set are filtered out; an empty visible set
/// returns an empty string.
pub fn format_skills_for_system_prompt(skills: &[Skill]) -> String {
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }
    let mut lines: Vec<String> = Vec::new();
    lines.push(
        "The following skills provide specialized instructions for specific tasks.".to_string(),
    );
    lines.push("Read the full skill file when the task matches its description.".to_string());
    lines.push(
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_string(),
    );
    lines.push(String::new());
    lines.push("<available_skills>".to_string());
    for skill in &visible {
        lines.push("  <skill>".to_string());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!("    <description>{}</description>", escape_xml(&skill.description)));
        lines.push(format!("    <location>{}</location>", escape_xml(&skill.file_path)));
        lines.push("  </skill>".to_string());
    }
    lines.push("</available_skills>".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, disabled: bool) -> Skill {
        Skill {
            name: name.into(),
            description: format!("desc {name}"),
            content: "content".into(),
            file_path: format!("/skills/{name}/SKILL.md"),
            disable_model_invocation: disabled,
        }
    }

    #[test]
    fn empty_skills_produce_empty_block() {
        assert_eq!(format_skills_for_system_prompt(&[]), "");
        assert_eq!(
            format_skills_for_system_prompt(&[skill("hidden", true)]),
            ""
        );
    }

    #[test]
    fn visible_skills_render_xml_block() {
        let out = format_skills_for_system_prompt(&[
            skill("A&B", false),
            skill("hidden", true),
            skill("plain", false),
        ]);
        assert!(out.contains("<available_skills>"));
        assert!(out.contains("</available_skills>"));
        assert!(out.contains("<name>A&amp;B</name>"));
        assert!(out.contains("<description>desc plain</description>"));
        assert!(out.contains("<location>/skills/plain/SKILL.md</location>"));
        assert!(!out.contains("hidden"), "disabled skill excluded");
        // order preserved
        let ai = out.find("A&amp;B").unwrap();
        let plain = out.find("desc plain").unwrap();
        assert!(ai < plain);
    }

    #[test]
    fn escape_xml_covers_special_chars() {
        assert_eq!(escape_xml(r#"<a href="x">it's & more</a>"#), "&lt;a href=&quot;x&quot;&gt;it&apos;s &amp; more&lt;/a&gt;");
    }
}
