//! External-editor integration for the interactive prompt.
//!
//! This is deliberately a small, Rust-native equivalent of Pi's
//! `external-editor.ts`: the editor receives a private temporary markdown
//! file, owns the terminal while it is running, and the directory is removed
//! on every exit path.

use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalEditorResult {
    Complete(String),
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalEditorOptions {
    pub command: String,
    pub content: String,
}

/// Split an editor command without invoking a shell.
///
/// Pi's upstream implementation accepts a command plus space-separated
/// arguments. Supporting quotes and backslash escapes here keeps the same
/// user-facing setting useful for paths containing spaces while avoiding
/// shell expansion or injection.
pub fn split_command(command: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut has_content = false;

    for ch in command.chars() {
        if escaped {
            word.push(ch);
            has_content = true;
            escaped = false;
            continue;
        }
        match ch {
            '\\' if !in_single => escaped = true,
            '\'' if !in_double => {
                in_single = !in_single;
                has_content = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                has_content = true;
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if has_content {
                    words.push(std::mem::take(&mut word));
                    has_content = false;
                }
            }
            c => {
                word.push(c);
                has_content = true;
            }
        }
    }

    if escaped {
        return Err("external editor command ends with an escape".to_string());
    }
    if in_single || in_double {
        return Err("external editor command has an unterminated quote".to_string());
    }
    if has_content {
        words.push(word);
    }
    if words.is_empty() {
        return Err("external editor command is empty".to_string());
    }
    Ok(words)
}

fn private_temp_directory() -> Result<PathBuf, String> {
    let path = std::env::temp_dir().join(format!("pi-editor-{}", Uuid::new_v4()));
    fs::create_dir(&path).map_err(|error| format!("create temporary editor directory: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = fs::set_permissions(&path, fs::Permissions::from_mode(0o700)) {
            let _ = fs::remove_dir_all(&path);
            return Err(format!("protect temporary editor directory: {error}"));
        }
    }
    Ok(path)
}

fn write_prompt_file(path: &Path, content: &str) -> Result<(), String> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("create editor prompt: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("protect editor prompt: {error}"))?;
    }
    use std::io::Write;
    let mut file = file;
    file.write_all(content.as_bytes())
        .map_err(|error| format!("write editor prompt: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("flush editor prompt: {error}"))
}

fn read_prompt_file(path: &Path) -> Result<String, String> {
    let mut content = String::new();
    fs::File::open(path)
        .map_err(|error| format!("read edited prompt: {error}"))?
        .read_to_string(&mut content)
        .map_err(|error| format!("read edited prompt: {error}"))?;
    let mut content = content
        .strip_prefix('\u{feff}')
        .unwrap_or(&content)
        .to_string();
    if content.ends_with('\n') {
        content.pop();
    }
    Ok(content)
}

fn run_editor(options: ExternalEditorOptions, directory: PathBuf) -> ExternalEditorResult {
    let result = (|| {
        let parts = split_command(&options.command)?;
        let file_path = directory.join("prompt.md");
        write_prompt_file(&file_path, &options.content)?;

        println!(
            "Launching external editor: {}\nPi will resume when the editor exits.",
            options.command
        );
        let mut child = Command::new(&parts[0])
            .args(&parts[1..])
            .arg(&file_path)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("launch external editor: {error}"))?;
        let status = child
            .wait()
            .map_err(|error| format!("wait for external editor: {error}"))?;
        if !status.success() {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if status.signal() == Some(2) || status.code() == Some(130) {
                    return Ok(ExternalEditorResult::Cancelled);
                }
            }
            return Ok(ExternalEditorResult::Failed(format!(
                "external editor exited unsuccessfully ({status})"
            )));
        }
        Ok(ExternalEditorResult::Complete(read_prompt_file(
            &file_path,
        )?))
    })();

    let result = match result {
        Ok(result) => result,
        Err(error) => ExternalEditorResult::Failed(error),
    };
    let _ = fs::remove_dir_all(directory);
    result
}

/// Launch an external editor without blocking the async interactive loop.
pub async fn edit_in_external_editor(options: ExternalEditorOptions) -> ExternalEditorResult {
    let directory = match private_temp_directory() {
        Ok(directory) => directory,
        Err(error) => return ExternalEditorResult::Failed(error),
    };
    let result = tokio::task::spawn_blocking(move || run_editor(options, directory.clone()))
        .await
        .unwrap_or_else(|error| {
            ExternalEditorResult::Failed(format!("editor task failed: {error}"))
        });
    result
}

/// Synchronous entry point used by deterministic process tests and callers
/// that already own a blocking thread.
pub fn edit_in_external_editor_blocking(options: ExternalEditorOptions) -> ExternalEditorResult {
    let directory = match private_temp_directory() {
        Ok(directory) => directory,
        Err(error) => return ExternalEditorResult::Failed(error),
    };
    run_editor(options, directory)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fixture_script(name: &str, body: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("pi-editor-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let script = root.join(name);
        fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        (root, script)
    }

    #[test]
    fn editor_success_reads_bom_and_removes_directory() {
        let (root, script) =
            fixture_script("editor.sh", "printf '\\357\\273\\277edited\\n' > \"$1\"");
        let result = edit_in_external_editor_blocking(ExternalEditorOptions {
            command: script.display().to_string(),
            content: "original".into(),
        });
        assert_eq!(result, ExternalEditorResult::Complete("edited".into()));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn editor_failure_is_not_success() {
        let (root, script) = fixture_script("editor.sh", "exit 17");
        let result = edit_in_external_editor_blocking(ExternalEditorOptions {
            command: script.display().to_string(),
            content: "original".into(),
        });
        assert!(matches!(result, ExternalEditorResult::Failed(_)));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn command_parser_handles_quotes_and_rejects_malformed_input() {
        assert_eq!(
            split_command("editor --flag 'file name'").unwrap(),
            vec!["editor", "--flag", "file name"]
        );
        assert!(split_command("editor 'unfinished").is_err());
    }
}
