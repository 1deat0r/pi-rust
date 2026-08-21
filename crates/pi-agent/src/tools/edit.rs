//! Edit tool — port of `packages/agent/src/harness/tools/edit.ts`
//! (old_string/new_string unique replacement).

use super::path_utils::resolve_tool_path;
use pi_ai::types::ToolResultMessage;

pub async fn execute_edit(
    tool_call_id: &str,
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: Option<bool>,
    cwd: &str,
) -> Result<ToolResultMessage, String> {
    let absolute = resolve_tool_path(cwd, path);
    let content = std::fs::read_to_string(&absolute).map_err(|e| format!("Failed to read {path}: {e}"))?;

    let count = if replace_all == Some(true) {
        content.matches(old_string).count()
    } else {
        content.matches(old_string).count().min(1)
    };
    if count == 0 {
        return Err(format!(
            "The string to replace was not found in the file: {old_string:?}"
        ));
    }
    if !replace_all.unwrap_or(false) && content.matches(old_string).count() > 1 {
        return Err(format!(
            "String to replace appears {} times; use replaceAll=true to replace all occurrences",
            content.matches(old_string).count()
        ));
    }
    let updated = if replace_all == Some(true) {
        content.replace(old_string, new_string)
    } else {
        content.replacen(old_string, new_string, 1)
    };
    std::fs::write(&absolute, updated).map_err(|e| format!("Failed to write {path}: {e}"))?;
    Ok(ToolResultMessage::text(
        tool_call_id,
        "edit",
        format!("Successfully edited {path} ({} replacement{})", count, if count == 1 { "" } else { "s" }),
        false,
    ))
}
