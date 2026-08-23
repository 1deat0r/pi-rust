//! Prompt templates — port of `packages/agent/src/harness/prompt-templates.ts`
//! (`loadPromptTemplates` over the local filesystem, sourced variants,
//! frontmatter parsing, `parseCommandArgs`, `substituteArgs`, and
//! `formatPromptTemplateInvocation`).

use std::path::{Path, PathBuf};

use crate::types::PromptTemplate;

/// Stable diagnostic codes emitted while loading prompt templates
/// (upstream `PromptTemplateDiagnosticCode`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptTemplateDiagnostic {
    pub code: &'static str,
    pub message: String,
    pub path: String,
}

/// Load prompt templates from one or more paths (directories load direct
/// `.md` children non-recursively; files load explicit `.md` files). Missing
/// paths and non-markdown files are skipped; read/parse failures are returned
/// as diagnostics.
pub fn load_prompt_templates(
    cwd: &str,
    paths: &[String],
) -> (Vec<PromptTemplate>, Vec<PromptTemplateDiagnostic>) {
    let mut templates = Vec::new();
    let mut diagnostics = Vec::new();
    for path in paths {
        let file_path = resolve(cwd, path);
        match std::fs::metadata(&file_path) {
            Ok(meta) if meta.is_dir() => {
                load_templates_from_dir(&file_path, &mut templates, &mut diagnostics);
            }
            Ok(meta) if meta.is_file() => {
                let name = file_path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if name.to_lowercase().ends_with(".md") {
                    load_template_from_file(&file_path, &name, &mut templates, &mut diagnostics);
                }
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => diagnostics.push(PromptTemplateDiagnostic {
                code: "file_info_failed",
                message: e.to_string(),
                path: path.clone(),
            }),
        }
    }
    (templates, diagnostics)
}

/// Source-tagged variant of [`load_prompt_templates`]. Source values are
/// preserved exactly and attached to every loaded template and diagnostic.
pub fn load_sourced_prompt_templates<TSource: Clone>(
    cwd: &str,
    inputs: &[(String, TSource)],
) -> (
    Vec<(PromptTemplate, TSource)>,
    Vec<(PromptTemplateDiagnostic, TSource)>,
) {
    let mut templates = Vec::new();
    let mut diagnostics = Vec::new();
    for (path, source) in inputs {
        let (mut ts, mut ds) = load_prompt_templates(cwd, std::slice::from_ref(path));
        for t in ts.drain(..) {
            templates.push((t, source.clone()));
        }
        for d in ds.drain(..) {
            diagnostics.push((d, source.clone()));
        }
    }
    (templates, diagnostics)
}

fn load_templates_from_dir(
    dir: &Path,
    templates: &mut Vec<PromptTemplate>,
    diagnostics: &mut Vec<PromptTemplateDiagnostic>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            diagnostics.push(PromptTemplateDiagnostic {
                code: "list_failed",
                message: e.to_string(),
                path: dir.to_string_lossy().into_owned(),
            });
            return;
        }
    };
    let mut names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        names.push(name);
    }
    // Node localeCompare order: case-insensitive codepoint ordering.
    names.sort_by(|a, b| {
        a.to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| a.cmp(b))
    });
    for name in names {
        if !name.to_lowercase().ends_with(".md") {
            continue;
        }
        let entry_path = dir.join(&name);
        match std::fs::metadata(&entry_path) {
            Ok(meta) if meta.is_file() => {
                load_template_from_file(&entry_path, &name, templates, diagnostics);
            }
            Ok(_) => {}
            Err(e) => diagnostics.push(PromptTemplateDiagnostic {
                code: "file_info_failed",
                message: e.to_string(),
                path: entry_path.to_string_lossy().into_owned(),
            }),
        }
    }
}

fn load_template_from_file(
    file_path: &Path,
    file_name: &str,
    templates: &mut Vec<PromptTemplate>,
    diagnostics: &mut Vec<PromptTemplateDiagnostic>,
) {
    let raw = match std::fs::read_to_string(file_path) {
        Ok(c) => c,
        Err(e) => {
            diagnostics.push(PromptTemplateDiagnostic {
                code: "read_failed",
                message: e.to_string(),
                path: file_path.to_string_lossy().into_owned(),
            });
            return;
        }
    };
    let Some((frontmatter, body)) = parse_frontmatter(&raw) else {
        diagnostics.push(PromptTemplateDiagnostic {
            code: "parse_failed",
            message: "could not parse YAML frontmatter".to_string(),
            path: file_path.to_string_lossy().into_owned(),
        });
        return;
    };
    let first_line = body.lines().map(|l| l.trim()).find(|l| !l.is_empty());
    let mut description = frontmatter
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if description.is_empty() {
        if let Some(line) = first_line {
            let chars: Vec<char> = line.chars().collect();
            description = chars.iter().take(60).collect();
            if chars.len() > 60 {
                description.push_str("...");
            }
        }
    }
    templates.push(PromptTemplate {
        name: file_name.trim_end_matches(".md").to_string(),
        description: Some(description),
        content: body,
    });
}

/// Shared frontmatter parser (see `harness/frontmatter.rs`).
fn parse_frontmatter(content: &str) -> Option<(serde_yaml::Value, String)> {
    super::frontmatter::parse_frontmatter(content)
}

/// Resolve a template path against `cwd` (relative paths are joined, absolute
/// paths pass through; `~` expansion is handled by config/callers).
fn resolve(cwd: &str, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(cwd).join(p)
    }
}

/// Parse an argument string using simple shell-style single and double quotes
/// (upstream `parseCommandArgs`).
pub fn parse_command_args(args_string: &str) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quote: Option<char> = None;
    for ch in args_string.chars() {
        if let Some(q) = in_quote {
            if ch == q {
                in_quote = None;
            } else {
                current.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            in_quote = Some(ch);
        } else if ch == ' ' || ch == '\t' {
            if !current.is_empty() {
                args.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

/// Substitute prompt template placeholders (`$1`, `$@`, `$ARGUMENTS`,
/// `${@:N}`, `${@:N:L}`) with command arguments (upstream `substituteArgs`).
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let mut result = content.to_string();
    result = regex_replace(&result, &regex::Regex::new(r"\$(\d+)").unwrap(), |caps| {
        let num: usize = caps[1].parse().unwrap_or(0);
        args.get(num.wrapping_sub(1)).cloned().unwrap_or_default()
    });
    result = regex_replace(
        &result,
        &regex::Regex::new(r"\$\{@:(\d+)(?::(\d+))?\}").unwrap(),
        |caps| {
            let start_raw: isize = caps[1].parse().unwrap_or(1);
            let mut start = start_raw - 1;
            if start < 0 {
                start = 0;
            }
            let start = start as usize;
            if let Some(len) = caps.get(2) {
                let len: usize = len.as_str().parse().unwrap_or(0);
                args.iter()
                    .skip(start)
                    .take(len)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                args.iter()
                    .skip(start)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        },
    );
    let all_args = args.join(" ");
    result = result.replace("$ARGUMENTS", &all_args);
    result = result.replace("$@", &all_args);
    result
}

fn regex_replace(
    text: &str,
    re: &regex::Regex,
    mut f: impl FnMut(&regex::Captures) -> String,
) -> String {
    let mut out = String::new();
    let mut last = 0;
    for caps in re.captures_iter(text) {
        let m = caps.get(0).unwrap();
        out.push_str(&text[last..m.start()]);
        out.push_str(&f(&caps));
        last = m.end();
    }
    out.push_str(&text[last..]);
    out
}

/// Format a prompt template invocation with positional arguments
/// (upstream `formatPromptTemplateInvocation`).
pub fn format_prompt_template_invocation(template: &PromptTemplate, args: &[String]) -> String {
    substitute_args(&template.content, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pi-agent-pt-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn loads_templates_from_dir_sorted() {
        let dir = tmpdir("dir");
        std::fs::write(
            dir.join("b.md"),
            "---\ndescription: B template\n---\nBody B\n",
        )
        .unwrap();
        std::fs::write(dir.join("a.md"), "# A template\n\nBody A\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "not a template").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join("nested.md"), "nested").unwrap();
        let (templates, diagnostics) = load_prompt_templates(
            &dir.to_string_lossy(),
            &[dir.to_string_lossy().into_owned()],
        );
        assert!(
            diagnostics.is_empty(),
            "unexpected diagnostics: {diagnostics:?}"
        );
        assert_eq!(templates.len(), 2);
        assert_eq!(templates[0].name, "a");
        assert_eq!(templates[0].description.as_deref(), Some("# A template"));
        assert_eq!(templates[1].name, "b");
        assert_eq!(templates[1].description.as_deref(), Some("B template"));
        assert!(templates[1].content.contains("Body B"));
        // nested file is NOT loaded (non-recursive)
        assert!(!templates.iter().any(|t| t.name == "nested"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_path_is_skipped_and_parse_failure_diagnosed() {
        let dir = tmpdir("missing");
        let missing = dir.join("nope.md").to_string_lossy().into_owned();
        let (templates, diagnostics) = load_prompt_templates(&dir.to_string_lossy(), &[missing]);
        assert!(templates.is_empty());
        assert!(diagnostics.is_empty(), "missing path skipped silently");
        let bad = dir.join("bad.md");
        std::fs::write(&bad, "---\n{{{{not yaml\n---\nbody").unwrap();
        let (templates, diagnostics) = load_prompt_templates(
            &dir.to_string_lossy(),
            &[bad.to_string_lossy().into_owned()],
        );
        assert!(templates.is_empty());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "parse_failed");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn first_line_description_truncates_to_60() {
        let dir = tmpdir("desc");
        let long = "x".repeat(80);
        std::fs::write(
            dir.join("t.md"),
            format!("# This is a much longer first line {long}\nbody"),
        )
        .unwrap();
        let (templates, _) = load_prompt_templates(
            &dir.to_string_lossy(),
            &[dir.to_string_lossy().into_owned()],
        );
        let desc = templates[0].description.as_deref().unwrap();
        assert!(desc.ends_with("..."));
        assert!(desc.chars().count() <= 63);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_command_args_handles_quotes() {
        assert_eq!(parse_command_args("alpha beta"), vec!["alpha", "beta"]);
        assert_eq!(
            parse_command_args("one \"two three\" four"),
            vec!["one", "two three", "four"]
        );
        // Upstream quote handling is a naive toggle: the apostrophe inside
        // "it's" opens a quote that closes at the next apostrophe, so the
        // whole remainder joins into one argument. Parity over intuition.
        assert_eq!(
            parse_command_args("it's a 'quoted' pair"),
            vec!["its a quoted pair"]
        );
        assert_eq!(
            parse_command_args("one 'two three' four"),
            vec!["one", "two three", "four"]
        );
        assert_eq!(parse_command_args("let's go"), vec!["lets go"]);
        assert_eq!(parse_command_args(""), Vec::<String>::new());
        assert_eq!(parse_command_args("  spaced  out  "), vec!["spaced", "out"]);
    }

    #[test]
    fn substitute_args_port_behavior() {
        let args = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(substitute_args("$1 $2 $3", &args), "a b c");
        assert_eq!(substitute_args("$9 missing", &args), " missing");
        assert_eq!(substitute_args("$@", &args), "a b c");
        assert_eq!(substitute_args("$ARGUMENTS", &args), "a b c");
        assert_eq!(substitute_args("${@:2}", &args), "b c");
        assert_eq!(substitute_args("${@:2:1}", &args), "b");
        assert_eq!(substitute_args("${@:0}", &args), "a b c");
        assert_eq!(
            substitute_args("no placeholders here", &args),
            "no placeholders here"
        );
    }

    #[test]
    fn format_invocation_uses_template_content() {
        let template = PromptTemplate {
            name: "t".into(),
            description: Some("d".into()),
            content: "Review: $1\nAll: $@".into(),
        };
        let formatted =
            format_prompt_template_invocation(&template, &["code".to_string(), "docs".to_string()]);
        assert_eq!(formatted, "Review: code\nAll: code docs");
    }
}
