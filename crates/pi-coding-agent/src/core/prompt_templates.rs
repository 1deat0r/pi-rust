//! Prompt template loading/expansion — port of
//! `packages/coding-agent/src/core/prompt-templates.ts`.

use std::path::{Path, PathBuf};

use pi_agent::harness::frontmatter::parse_frontmatter;

use crate::config::CONFIG_DIR_NAME;
use crate::core::diagnostics::{ResourceDiagnostic, ResourceDiagnosticKind};

/// A prompt template loaded from a markdown file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub content: String,
    pub file_path: String,
}

/// Parse a command argument string respecting quoted strings (bash-style).
pub fn parse_command_args(args_string: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quote: Option<char> = None;

    for char in args_string.chars() {
        if let Some(q) = in_quote {
            if char == q {
                in_quote = None;
            } else {
                current.push(char);
            }
        } else if char == '"' || char == '\'' {
            in_quote = Some(char);
        } else if char.is_whitespace() {
            if !current.is_empty() {
                args.push(std::mem::take(&mut current));
            }
            current.clear();
        } else {
            current.push(char);
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

/// Substitute argument placeholders in template content.
///
/// Supports `$1`, `$2`, ... positional; `$@`/`$ARGUMENTS` all args;
/// `${N:-default}` positional-with-default; `${@:-default}`/`${ARGUMENTS:-default}`
/// all-args-with-default; `${@:N}` slice from N; `${@:N:L}` slice of L from N.
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let all_args = args.join(" ");

    // Iterative scan: find each `$...` token and substitute, leaving the rest.
    let mut result = String::with_capacity(content.len());
    let mut rest = content;

    while let Some(dollar) = find_dollar(rest) {
        result.push_str(&rest[..dollar]);
        let token_start = dollar + 1;
        let after = &rest[token_start..];

        // Bracketed forms: `${...}` (inner is after the `{`, before the `}`).
        if let Some(close) = find_brace_close(after) {
            let inner = &after[1..close];
            let replacement = substitute_bracket(inner, args, &all_args);
            result.push_str(&replacement);
            rest = &after[close + 1..];
            continue;
        }

        // Simple form: `$NAME` where NAME is [A-Za-z0-9@]+.
        let name_len = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '@')
            .count();
        if name_len == 0 {
            result.push('$');
            rest = after;
            continue;
        }
        let name = &after[..name_len];
        let replacement = match name {
            "@" | "ARGUMENTS" => all_args.clone(),
            "--" => return result, // `$--` not handled upstream; bail to literal
            _ => {
                if let Ok(index) = name.parse::<usize>() {
                    args.get(index - 1).cloned().unwrap_or_default()
                } else {
                    format!("${name}")
                }
            }
        };
        result.push_str(&replacement);
        rest = &after[name_len..];
    }

    result.push_str(rest);
    result
}

fn find_dollar(s: &str) -> Option<usize> {
    s.char_indices().find(|(_, c)| *c == '$').map(|(i, _)| i)
}

fn find_brace_close(s: &str) -> Option<usize> {
    s.char_indices().find(|(_, c)| *c == '}').map(|(i, _)| i)
}

fn substitute_bracket(inner: &str, args: &[String], all_args: &str) -> String {
    // `${@:-default}` / `${ARGUMENTS:-default}` — all-args with default when empty.
    if let Some(default) = inner
        .strip_prefix("@:-")
        .or_else(|| inner.strip_prefix("ARGUMENTS:-"))
    {
        if all_args.is_empty() {
            return default.to_string();
        }
        return all_args.to_string();
    }
    // `${@:N}` / `${@:N:L}` — bash-style slicing of all args.
    if let Some(slice) = inner.strip_prefix("@:") {
        return slice_args(slice, args);
    }
    // `${target:-default}` — positional arg with default when missing/empty.
    if let Some((target, default)) = inner.split_once(":-") {
        let value = match target {
            "@" | "ARGUMENTS" => all_args.to_string(),
            other => other
                .parse::<usize>()
                .ok()
                .and_then(|idx| args.get(idx - 1))
                .cloned()
                .unwrap_or_default(),
        };
        if value.is_empty() {
            default.to_string()
        } else {
            value
        }
    } else {
        String::new()
    }
}

fn slice_args(slice: &str, args: &[String]) -> String {
    let mut parts = slice.split(':');
    let raw_start: usize = parts.next().and_then(|v| v.parse().ok()).unwrap_or(1);
    // Convert to 0-indexed; treat 0 (or 1) as 1 per bash convention (args start at 1).
    let start = raw_start.saturating_sub(1);
    let length: Option<usize> = parts.next().and_then(|v| v.parse().ok());
    match length {
        Some(len) => args.get(start..start + len).unwrap_or_default().join(" "),
        None => args.get(start..).unwrap_or_default().join(" "),
    }
}

fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

fn load_template_from_file(file_path: &Path) -> Option<PromptTemplate> {
    let raw_content = std::fs::read_to_string(file_path).ok()?;
    let raw_content = strip_bom(&raw_content);
    let (frontmatter, body) = parse_frontmatter(raw_content)?;

    let name = file_path
        .file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    // Description from frontmatter or the first non-empty body line.
    let mut description = frontmatter
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default();
    if description.is_empty() {
        if let Some(first_line) = body.lines().find(|l| !l.trim().is_empty()) {
            description = if first_line.chars().count() > 60 {
                format!(
                    "{}...",
                    &first_line[..first_line
                        .char_indices()
                        .nth(60)
                        .map(|(i, _)| i)
                        .unwrap_or(first_line.len())]
                )
            } else {
                first_line.to_string()
            };
        }
    }

    let argument_hint = frontmatter
        .get("argument-hint")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Some(PromptTemplate {
        name,
        description,
        argument_hint,
        content: body,
        file_path: file_path.to_string_lossy().into_owned(),
    })
}

fn load_templates_from_dir(dir: &Path) -> Vec<PromptTemplate> {
    let mut templates = Vec::new();
    if !dir.is_dir() {
        return templates;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return templates,
    };
    for entry in entries.flatten() {
        let full_path = dir.join(entry.file_name());
        if full_path.is_file() && full_path.to_string_lossy().ends_with(".md") {
            if let Some(template) = load_template_from_file(&full_path) {
                templates.push(template);
            }
        }
    }
    templates
}

fn resolve_path(path: &str, cwd: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(cwd).join(p)
    }
}

/// Load prompt templates from the global (`<agentDir>/prompts`), project
/// (`<cwd>/.pi/prompts`), and explicit `--prompt-template` paths.
pub fn load_prompt_templates(
    cwd: &str,
    agent_dir: &str,
    prompt_paths: &[String],
    include_defaults: bool,
    no_prompt_templates: bool,
) -> (Vec<PromptTemplate>, Vec<ResourceDiagnostic>) {
    let mut templates = Vec::new();
    let mut diagnostics = Vec::new();

    let global_dir = Path::new(agent_dir).join("prompts");
    let project_dir = Path::new(cwd).join(CONFIG_DIR_NAME).join("prompts");

    if include_defaults && !no_prompt_templates {
        templates.extend(load_templates_from_dir(&global_dir));
        templates.extend(load_templates_from_dir(&project_dir));
    }

    for raw_path in prompt_paths {
        let resolved = resolve_path(raw_path, cwd);
        if !resolved.exists() {
            diagnostics.push(ResourceDiagnostic {
                kind: ResourceDiagnosticKind::Error,
                message: "Prompt template path does not exist".to_string(),
                path: Some(resolved.to_string_lossy().into_owned()),
                collision: None,
            });
            continue;
        }
        if resolved.is_dir() {
            templates.extend(load_templates_from_dir(&resolved));
        } else if resolved.is_file() && resolved.to_string_lossy().ends_with(".md") {
            if let Some(template) = load_template_from_file(&resolved) {
                templates.push(template);
            }
        }
    }

    (templates, diagnostics)
}

/// Expand a prompt template if the text starts with `/template-name`. Returns
/// the original text when it isn't a template invocation.
pub fn expand_prompt_template(text: &str, templates: &[PromptTemplate]) -> String {
    if !text.starts_with('/') {
        return text.to_string();
    }
    let Some(stripped) = text.strip_prefix('/') else {
        return text.to_string();
    };
    let (template_name, args_string) = match stripped.find(char::is_whitespace) {
        Some(idx) => (&stripped[..idx], &stripped[idx..]),
        None => (stripped, ""),
    };
    let Some(template) = templates.iter().find(|t| t.name == template_name) else {
        return text.to_string();
    };
    let args = parse_command_args(args_string.trim());
    substitute_args(&template.content, &args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_command_args_respects_quotes() {
        assert_eq!(
            parse_command_args("a b \"c d\" 'e f'"),
            vec!["a", "b", "c d", "e f"]
        );
        assert_eq!(parse_command_args(""), Vec::<String>::new());
        assert_eq!(parse_command_args("  spaced  out  "), vec!["spaced", "out"]);
    }

    #[test]
    fn substitute_positional_and_all_args() {
        let args: Vec<String> = vec!["one".into(), "two".into()];
        assert_eq!(substitute_args("$1 $2 $@", &args), "one two one two");
        assert_eq!(substitute_args("$ARGUMENTS", &args), "one two");
        assert_eq!(substitute_args("$1 $3", &args), "one ");
        assert_eq!(substitute_args("$3", &args), "");
    }

    #[test]
    fn substitute_defaults_and_slices() {
        let args: Vec<String> = vec!["one".into(), "two".into(), "three".into()];
        assert_eq!(substitute_args("${1:-default}", &args), "one");
        assert_eq!(substitute_args("${9:-default}", &args), "default");
        assert_eq!(substitute_args("${@:-nope}", &args), "one two three");
        assert_eq!(substitute_args("${@:-nope}", &[]), "nope");
        assert_eq!(substitute_args("${@:2}", &args), "two three");
        assert_eq!(substitute_args("${@:2:2}", &args), "two three");
    }

    #[test]
    fn non_template_text_passes_through() {
        let templates = vec![PromptTemplate {
            name: "summarize".into(),
            description: "d".into(),
            argument_hint: None,
            content: "Summarize: $@".into(),
            file_path: "/t/summarize.md".into(),
        }];
        assert_eq!(expand_prompt_template("hello", &templates), "hello");
        assert_eq!(
            expand_prompt_template("/unknown x", &templates),
            "/unknown x"
        );
        assert_eq!(
            expand_prompt_template("/summarize the docs", &templates),
            "Summarize: the docs"
        );
    }

    #[test]
    fn loads_templates_from_dir() {
        let dir = std::env::temp_dir().join(format!("pi-prompt-tpl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let prompts_dir = dir.join(".pi").join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        std::fs::write(
            prompts_dir.join("hello.md"),
            "---\ndescription: Greet\nargument-hint: name\n---\nHello, $1!",
        )
        .unwrap();
        std::fs::write(
            prompts_dir.join("README.md"),
            "no frontmatter but still loaded\n",
        )
        .unwrap();
        let (templates, diag) = load_prompt_templates(
            &dir.to_string_lossy(),
            &dir.join("agent").to_string_lossy(),
            &[],
            true,
            false,
        );
        assert!(diag.is_empty());
        assert!(templates
            .iter()
            .any(|t| t.name == "hello" && t.content.contains("Hello, $1")));
        assert!(templates.iter().any(|t| t.name == "README"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_explicit_prompt_path_is_diagnosed() {
        let dir = std::env::temp_dir().join(format!("pi-prompt-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (_, diag) = load_prompt_templates(
            &dir.to_string_lossy(),
            &dir.join("agent").to_string_lossy(),
            &["/nonexistent/foo.md".to_string()],
            false,
            false,
        );
        assert!(diag.iter().any(|d| d.kind == ResourceDiagnosticKind::Error));
        std::fs::remove_dir_all(&dir).ok();
    }
}
