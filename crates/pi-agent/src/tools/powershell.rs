//! Optional PowerShell tool — port of
//! `packages/coding-agent/src/core/tools/powershell.ts` (#8512).
//!
//! Reuses the bash execution machinery with a PowerShell binary plus
//! its required invocation args. The tool is optional (not in the
//! default set) and only resolves on Windows.

use std::sync::Arc;

use pi_ai::types::json_tool;

use super::bash;
use crate::tools::AgentTool;

/// PowerShell invocation args (upstream `POWERSHELL_ARGS`).
pub const POWERSHELL_ARGS: &[&str] = &[
    "-NoProfile",
    "-NonInteractive",
    "-ExecutionPolicy",
    "Bypass",
    "-Command",
];

/// UTF-8 prefix prepended to every command so output decodes correctly
/// (upstream `UTF8_OUTPUT_PREFIX`).
pub const UTF8_OUTPUT_PREFIX: &str =
    "try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}\n";

/// Resolve PowerShell on Windows, preferring PowerShell 7. Errors on
/// other platforms and when no executable is found (upstream
/// `getPowerShellConfig`).
pub fn powershell_config() -> Result<(String, Vec<String>), String> {
    if std::env::consts::OS != "windows" {
        return Err("The powershell tool is only available on Windows.".to_string());
    }
    for candidate in ["pwsh.exe", "powershell.exe"] {
        if let Some(path) = find_executable_on_path(candidate) {
            return Ok((
                path,
                POWERSHELL_ARGS.iter().map(|s| (*s).to_string()).collect(),
            ));
        }
    }
    Err("No PowerShell executable found. Install PowerShell or add powershell.exe/pwsh.exe to PATH."
        .to_string())
}

fn find_executable_on_path(executable: &str) -> Option<String> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(executable);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

/// Build the optional powershell tool (upstream `createPowerShellTool`).
/// Returns an error when PowerShell is unavailable; callers keep the
/// tool out of the active set in that case.
pub fn powershell_tool(cwd: String) -> Result<AgentTool, String> {
    let (shell_path, shell_args) = powershell_config()?;
    Ok(AgentTool::new(
        json_tool(
            "powershell",
            "Execute a PowerShell command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first). Optionally provide a timeout in seconds.",
            &serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "PowerShell command to execute"},
                    "timeout": {"type": "number", "description": "Timeout in seconds (optional, no default timeout)"}
                },
                "required": ["command"]
            }),
        ),
        "Powershell",
        Arc::new(move |_tool_call_id, args, signal, on_update| {
            let cwd = cwd.clone();
            let shell_path = shell_path.clone();
            let shell_args = shell_args.clone();
            Box::pin(async move {
                let command = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "powershell: missing required argument command".to_string())?;
                let timeout = args.get("timeout").and_then(|v| v.as_f64());
                let command = format!("{UTF8_OUTPUT_PREFIX}{command}");
                bash::execute_bash_with_updates_and_shell_args(
                    &command,
                    timeout,
                    &cwd,
                    signal,
                    on_update,
                    Some(&shell_path),
                    &shell_args,
                )
                .await
            })
        }),
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn powershell_args_match_upstream() {
        assert_eq!(
            POWERSHELL_ARGS,
            &[
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command"
            ]
        );
    }

    #[test]
    fn powershell_config_rejects_non_windows() {
        if std::env::consts::OS == "windows" {
            return;
        }
        assert_eq!(
            powershell_config().unwrap_err(),
            "The powershell tool is only available on Windows."
        );
    }

    #[test]
    fn utf8_prefix_matches_upstream() {
        assert!(UTF8_OUTPUT_PREFIX.starts_with("try { [Console]::OutputEncoding="));
    }
}
