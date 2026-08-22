//! Shared YAML frontmatter parser used by prompt templates and skills
//! (port of the `parseFrontmatter` helpers in `harness/prompt-templates.ts`
//! and `harness/skills.ts`).

/// Parse YAML frontmatter from markdown content. Returns `(frontmatter,
/// body)`; `None` when YAML parsing fails. Content without a leading `---`
/// fence (or without a closing fence) returns a null frontmatter and the full
/// body.
pub fn parse_frontmatter(content: &str) -> Option<(serde_yaml::Value, String)> {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    if !normalized.starts_with("---") {
        return Some((serde_yaml::Value::Null, normalized));
    }
    let Some(end_index) = normalized.find("\n---") else {
        return Some((serde_yaml::Value::Null, normalized));
    };
    let yaml_string = &normalized[4..end_index];
    let body = normalized[end_index + 4..].trim().to_string();
    match serde_yaml::from_str::<serde_yaml::Value>(yaml_string) {
        Ok(value) => Some((value, body)),
        Err(_) => None,
    }
}
