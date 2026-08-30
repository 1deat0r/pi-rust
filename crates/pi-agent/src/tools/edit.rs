//! Edit tool — port of `packages/agent/src/harness/tools/edit.ts`: multiple
//! disjoint exact-text replacements matched against the original file, with
//! fuzzy fallback, BOM/line-ending preservation, and diff/patch details.

use super::edit_diff::{
    apply_edits_to_normalized_content, detect_line_ending, generate_diff_string,
    generate_unified_patch, normalize_to_lf, restore_line_endings, strip_bom, Edit,
};
use super::path_utils::resolve_tool_path;
use pi_ai::types::ToolResultMessage;
use serde_json::Value;

/// Tool execute for `edit` (upstream createEditTool.execute, direct fs).
pub async fn execute_edit(
    tool_call_id: &str,
    path: &str,
    edits: Vec<Edit>,
    cwd: &str,
) -> Result<ToolResultMessage, String> {
    execute_edit_with_abort(tool_call_id, path, edits, cwd, None).await
}

/// Edit with the agent-loop abort flag attached. These checks mirror the
/// upstream signal boundaries around file inspection, read, and write while
/// keeping the existing public convenience API unchanged.
pub async fn execute_edit_with_abort(
    tool_call_id: &str,
    path: &str,
    edits: Vec<Edit>,
    cwd: &str,
    abort: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<ToolResultMessage, String> {
    if crate::agent::is_aborted(abort.as_ref()) {
        return Err("Operation aborted".to_string());
    }
    let absolute = resolve_tool_path(cwd, path);
    let key = crate::harness::tools::resolve_mutation_key(cwd, path);
    let path = path.to_string();
    let tool_call_id = tool_call_id.to_string();
    crate::harness::tools::with_file_mutation_queue(key, async move {
        if crate::agent::is_aborted(abort.as_ref()) {
            return Err("Operation aborted".to_string());
        }
        let metadata = std::fs::metadata(&absolute)
            .map_err(|e| format!("Could not edit file: {path}. Error code: {e}."))?;
        if !metadata.is_file() {
            return Err(format!("Could not edit file: {path}. Path is not a file."));
        }

        let content = std::fs::read_to_string(&absolute)
            .map_err(|e| format!("Could not edit file: {path}. Error code: {e}."))?;
        if crate::agent::is_aborted(abort.as_ref()) {
            return Err("Operation aborted".to_string());
        }

        let (bom, text) = strip_bom(&content);
        let original_ending = detect_line_ending(&text);
        let normalized_content = normalize_to_lf(&text);
        let result = apply_edits_to_normalized_content(&normalized_content, &edits, &path)?;
        if crate::agent::is_aborted(abort.as_ref()) {
            return Err("Operation aborted".to_string());
        }

        let final_content = bom + &restore_line_endings(&result.new_content, original_ending);
        std::fs::write(&absolute, final_content)
            .map_err(|e| format!("Could not edit file: {path}. Error code: {e}."))?;
        if crate::agent::is_aborted(abort.as_ref()) {
            return Err("Operation aborted".to_string());
        }

        let (diff, first_changed_line) =
            generate_diff_string(&result.base_content, &result.new_content, 4);
        let patch = generate_unified_patch(&path, &result.base_content, &result.new_content, 4);

        let details = serde_json::json!({
            "diff": diff,
            "patch": patch,
            "firstChangedLine": first_changed_line,
        });

        Ok(ToolResultMessage::text(
            tool_call_id,
            "edit",
            format!("Successfully replaced {} block(s) in {path}.", edits.len()),
            false,
        )
        .with_details_usage_timestamp(None, Some(details), pi_ai::types::now_ms()))
    })
    .await
}

/// Normalize the raw arguments accepted by the upstream edit tool before
/// schema validation. Provider adapters have historically sent `edits` as a
/// JSON string, a single edit object, or the legacy top-level `oldText` /
/// `newText` pair.
pub fn prepare_edit_arguments(mut args: Value) -> Value {
    let Some(object) = args.as_object_mut() else {
        return args;
    };

    if let Some(Value::String(raw)) = object.get("edits").cloned() {
        if let Ok(parsed) = serde_json::from_str::<Value>(&raw) {
            if parsed.is_array() {
                object.insert("edits".to_string(), parsed);
            } else if is_single_edit(&parsed) {
                object.insert("edits".to_string(), Value::Array(vec![parsed]));
            }
        }
    } else if let Some(edit) = object.get("edits").cloned() {
        if is_single_edit(&edit) {
            object.insert("edits".to_string(), Value::Array(vec![edit]));
        }
    }

    let legacy_edit = match (
        object.get("oldText").and_then(Value::as_str),
        object.get("newText").and_then(Value::as_str),
    ) {
        (Some(old_text), Some(new_text)) => Some(serde_json::json!({
            "oldText": old_text,
            "newText": new_text,
        })),
        _ => None,
    };

    if let Some(legacy_edit) = legacy_edit {
        let mut edits = object
            .get("edits")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        edits.push(legacy_edit);
        object.insert("edits".to_string(), Value::Array(edits));
        object.remove("oldText");
        object.remove("newText");
    }

    args
}

fn is_single_edit(value: &Value) -> bool {
    value.is_object()
        && value.get("oldText").and_then(Value::as_str).is_some()
        && value.get("newText").and_then(Value::as_str).is_some()
}

/// Upstream `prepareEditArguments` + `validateEditInput`: accepts `edits` as
/// an array, a JSON string, or a single edit object, and appends legacy
/// top-level `oldText`/`newText` fields.
pub fn extract_edits(args: &serde_json::Value) -> Result<Vec<Edit>, String> {
    let mut edits: Vec<Edit> = Vec::new();
    let push_from_value = |edits: &mut Vec<Edit>, v: &serde_json::Value| {
        if let Some(old) = v.get("oldText").and_then(|x| x.as_str()) {
            if let Some(new) = v.get("newText").and_then(|x| x.as_str()) {
                edits.push(Edit {
                    old_text: old.to_string(),
                    new_text: new.to_string(),
                });
            }
        }
    };
    match args.get("edits") {
        Some(serde_json::Value::String(raw)) => {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(raw) {
                match &parsed {
                    serde_json::Value::Array(items) => {
                        for item in items {
                            push_from_value(&mut edits, item);
                        }
                    }
                    other => push_from_value(&mut edits, other),
                }
            }
        }
        Some(serde_json::Value::Array(items)) => {
            for item in items {
                push_from_value(&mut edits, item);
            }
        }
        Some(other) => push_from_value(&mut edits, other),
        None => {}
    }
    if let (Some(old), Some(new)) = (
        args.get("oldText").and_then(|v| v.as_str()),
        args.get("newText").and_then(|v| v.as_str()),
    ) {
        edits.push(Edit {
            old_text: old.to_string(),
            new_text: new.to_string(),
        });
    }
    if edits.is_empty() {
        return Err(
            "Edit tool input is invalid. edits must contain at least one replacement.".to_string(),
        );
    }
    Ok(edits)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn edit(old: &str, new: &str) -> Edit {
        Edit {
            old_text: old.to_string(),
            new_text: new.to_string(),
        }
    }

    fn tool_text(msg: &ToolResultMessage) -> String {
        use pi_ai::types::ContentBlock;
        match msg {
            ToolResultMessage::ToolResult { content, .. } => content
                .iter()
                .filter_map(|c| match c {
                    ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        }
    }

    fn tmpdir(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("pi-edit-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        (dir.clone(), dir)
    }

    #[tokio::test]
    async fn applies_disjoint_edits_and_updates_file() {
        let (dir, _) = tmpdir("disjoint");
        let file = dir.join("edit.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma\ndelta\n").unwrap();
        let msg = execute_edit(
            "e",
            "edit.txt",
            vec![edit("alpha\n", "ALPHA\n"), edit("gamma\n", "GAMMA\n")],
            &dir.display().to_string(),
        )
        .await
        .unwrap();
        assert_eq!(
            tool_text(&msg),
            "Successfully replaced 2 block(s) in edit.txt."
        );
        let details = msg.details().unwrap();
        assert!(details
            .get("diff")
            .and_then(|v| v.as_str())
            .unwrap()
            .contains("ALPHA"));
        assert!(details
            .get("diff")
            .and_then(|v| v.as_str())
            .unwrap()
            .contains("GAMMA"));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "ALPHA\nbeta\nGAMMA\ndelta\n"
        );
    }

    #[tokio::test]
    async fn rejects_overlapping_edits_leaves_file_unchanged() {
        let (dir, _) = tmpdir("overlap");
        let file = dir.join("edit.txt");
        std::fs::write(&file, "one\ntwo\nthree\n").unwrap();
        let err = execute_edit(
            "e",
            &file.display().to_string(),
            vec![
                edit("one\ntwo\n", "ONE\nTWO\n"),
                edit("two\nthree\n", "TWO\nTHREE\n"),
            ],
            &dir.display().to_string(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("overlap"), "got: {err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\ntwo\nthree\n");
    }

    #[tokio::test]
    async fn rejects_missing_and_duplicate() {
        let (dir, _) = tmpdir("missing");
        let file = dir.join("edit.txt");
        std::fs::write(&file, "foo foo foo").unwrap();
        let err = execute_edit(
            "e",
            &file.display().to_string(),
            vec![edit("bar", "baz")],
            &dir.display().to_string(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("Could not find the exact text"), "got: {err}");

        let err = execute_edit(
            "e",
            &file.display().to_string(),
            vec![edit("foo", "bar")],
            &dir.display().to_string(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("Found 3 occurrences"), "got: {err}");
    }

    #[tokio::test]
    async fn aborts_before_inspecting_the_file() {
        let (dir, _) = tmpdir("aborted");
        let abort = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let error = execute_edit_with_abort(
            "e",
            "missing.txt",
            vec![edit("before", "after")],
            &dir.display().to_string(),
            Some(abort),
        )
        .await
        .unwrap_err();
        assert_eq!(error, "Operation aborted");
    }

    #[tokio::test]
    async fn preserves_bom_and_crlf() {
        let (dir, _) = tmpdir("bom");
        let file = dir.join("edit.txt");
        std::fs::write(&file, "\u{FEFF}one\r\ntwo\r\n").unwrap();
        execute_edit(
            "e",
            &file.display().to_string(),
            vec![edit("two", "TWO")],
            &dir.display().to_string(),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "\u{FEFF}one\r\nTWO\r\n"
        );
    }

    #[tokio::test]
    async fn edits_regular_file_through_symlink() {
        let (dir, _) = tmpdir("symlink");
        let target = dir.join("target.txt");
        std::fs::write(&target, "before\n").unwrap();
        let link = dir.join("link.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();

        execute_edit(
            "e",
            &link.display().to_string(),
            vec![edit("before", "after")],
            &dir.display().to_string(),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "after\n");
    }

    #[tokio::test]
    async fn fuzzy_matches_smart_quote() {
        let (dir, _) = tmpdir("fuzzy");
        let file = dir.join("f.rs");
        std::fs::write(
            &file,
            "fn main() {\n    println!(\"it\u{2019}s fine\");\n}\n",
        )
        .unwrap();
        execute_edit(
            "e",
            &file.display().to_string(),
            vec![edit("it's fine", "it is fine")],
            &dir.display().to_string(),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "fn main() {\n    println!(\"it is fine\");\n}\n"
        );
    }

    #[test]
    fn prepare_edit_input_normalizes_variants() {
        // edits as JSON string
        let args = serde_json::json!({
            "path": "x.ts",
            "edits": "[{\"oldText\":\"a\",\"newText\":\"b\"}]"
        });
        assert_eq!(extract_edits(&args).unwrap().len(), 1);

        // single edit object
        let args =
            serde_json::json!({ "path": "x.ts", "edits": { "oldText": "a", "newText": "b" } });
        assert_eq!(extract_edits(&args).unwrap().len(), 1);

        // legacy top-level oldText/newText appended to edits array
        let args = serde_json::json!({
            "path": "x.ts",
            "edits": [{ "oldText": "a", "newText": "b" }],
            "oldText": "c",
            "newText": "d"
        });
        assert_eq!(extract_edits(&args).unwrap().len(), 2);

        // missing edits entirely -> error
        let args = serde_json::json!({ "path": "x.ts" });
        assert!(extract_edits(&args).is_err());
    }

    #[test]
    fn prepare_edit_arguments_runs_before_schema_validation() {
        let prepared = prepare_edit_arguments(serde_json::json!({
            "path": "x.ts",
            "edits": {"oldText": "a", "newText": "b"},
        }));
        assert_eq!(
            prepared,
            serde_json::json!({
                "path": "x.ts",
                "edits": [{"oldText": "a", "newText": "b"}],
            })
        );

        let prepared = prepare_edit_arguments(serde_json::json!({
            "path": "x.ts",
            "oldText": "a",
            "newText": "b",
        }));
        assert_eq!(
            prepared,
            serde_json::json!({
                "path": "x.ts",
                "edits": [{"oldText": "a", "newText": "b"}],
            })
        );

        let prepared = prepare_edit_arguments(serde_json::json!({
            "path": "x.ts",
            "edits": [{"oldText": "a", "newText": "b"}],
            "oldText": "c",
            "newText": "d",
        }));
        assert_eq!(prepared["edits"].as_array().unwrap().len(), 2);
        assert!(prepared.get("oldText").is_none());
        assert!(prepared.get("newText").is_none());
    }
}
