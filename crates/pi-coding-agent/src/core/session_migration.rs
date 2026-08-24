//! Legacy session-file migration — port of the migration surface in
//! `packages/coding-agent/src/core/session-manager.ts`.
//!
//! Upstream mapping correction: the v1→v2→v3 legacy `.session` migration
//! lives in `session-manager.ts` (`migrateSessionEntries`/`parseSessionEntries`),
//! NOT in the harness `jsonl/repo.ts` (the JSONL codec only reads version-4
//! files). Session files are legacy `{type:"session",...}` JSONL documents;
//! the harness JSONL repo is the v4 format. The v3→v4 import path is part of
//! the coding-agent session runtime (P4/P8).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Current legacy session file version (upstream `CURRENT_SESSION_VERSION`).
pub const CURRENT_SESSION_VERSION: u32 = 3;

/// Mirrors upstream `createSessionId()` (uuidv7 in the JS; the port ids
/// sessions with uuid v4 — see the JSONL repo; shape divergence is documented
/// in PLAN.md).
pub fn create_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Mirrors upstream `assertValidSessionId` (same pattern as the JSONL repo).
pub fn assert_valid_session_id(id: &str) -> Result<(), String> {
    let valid = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && id.starts_with(|c: char| c.is_ascii_alphanumeric())
        && id.ends_with(|c: char| c.is_ascii_alphanumeric());
    if valid {
        Ok(())
    } else {
        Err("Session id must be non-empty, contain only alphanumeric characters, '-', '_', and '.', and start and end with an alphanumeric character".to_string())
    }
}

/// Mirrors upstream `randomUUID().slice(0, 8)` — 8 hex chars from the first
/// four bytes of a fresh UUID, collision-checked against existing ids.
fn generate_id(by_id: &HashSet<String>) -> String {
    for _ in 0..100 {
        let candidate = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        if !by_id.contains(&candidate) {
            return candidate;
        }
    }
    // Fallback to full UUID if somehow we have collisions.
    uuid::Uuid::new_v4().to_string()
}

/// Migrate v1 → v2: add id/parentId tree structure. Mutates in place.
fn migrate_v1_to_v2(entries: &mut [Value]) {
    let mut ids = HashSet::new();
    let mut prev_id: Option<String> = None;
    let mut compaction_conversions: Vec<(usize, usize)> = Vec::new();

    for (index, entry) in entries.iter_mut().enumerate() {
        if entry.get("type").and_then(Value::as_str) == Some("session") {
            entry["version"] = Value::from(2u32);
            continue;
        }

        let id = generate_id(&ids);
        ids.insert(id.clone());
        entry["id"] = Value::from(id.clone());
        entry["parentId"] = match &prev_id {
            Some(prev) => Value::from(prev.clone()),
            None => Value::Null,
        };
        prev_id = Some(id);

        // Record indexes that need firstKeptEntryIndex → firstKeptEntryId
        // conversion; the target lookup must happen after the id pass so the
        // whole array is populated.
        if entry.get("type").and_then(Value::as_str) == Some("compaction") {
            if let Some(first_kept_index) = entry
                .get("firstKeptEntryIndex")
                .and_then(Value::as_u64)
                .map(|i| i as usize)
            {
                compaction_conversions.push((index, first_kept_index));
            }
        }
    }

    for (compaction_index, first_kept_index) in compaction_conversions {
        let target_id = entries
            .get(first_kept_index)
            .filter(|t| t.get("type").and_then(Value::as_str) != Some("session"))
            .and_then(|t| t.get("id").cloned());
        let entry = &mut entries[compaction_index];
        if let Some(target_id) = target_id {
            entry["firstKeptEntryId"] = target_id;
        }
        if let Some(obj) = entry.as_object_mut() {
            obj.remove("firstKeptEntryIndex");
        }
    }
}

/// Migrate v2 → v3: rename hookMessage role to custom. Mutates in place.
fn migrate_v2_to_v3(entries: &mut [Value]) {
    for entry in entries.iter_mut() {
        if entry.get("type").and_then(Value::as_str) == Some("session") {
            entry["version"] = Value::from(3u32);
            continue;
        }
        if entry.get("type").and_then(Value::as_str) == Some("message") {
            let role_changed = entry
                .get("message")
                .and_then(|m| m.get("role"))
                .and_then(Value::as_str)
                .is_some_and(|role| role == "hookMessage");
            if role_changed {
                if let Some(message) = entry.get_mut("message").and_then(Value::as_object_mut) {
                    message["role"] = Value::from("custom".to_string());
                }
            }
        }
    }
}

/// Run all necessary migrations to bring entries to current version.
/// Mutates entries in place. Returns true if any migration was applied.
fn migrate_to_current_version(entries: &mut [Value]) -> bool {
    let header = entries
        .iter()
        .find(|e| e.get("type").and_then(Value::as_str) == Some("session"));
    let version = header
        .and_then(|h| h.get("version").and_then(Value::as_u64))
        .unwrap_or(1) as u32;

    if version >= CURRENT_SESSION_VERSION {
        return false;
    }

    if version < 2 {
        migrate_v1_to_v2(entries);
    }
    if version < 3 {
        migrate_v2_to_v3(entries);
    }

    true
}

/// Exported for the CLI legacy resume path (`migrateSessionEntries`).
pub fn migrate_session_entries(entries: &mut [Value]) {
    migrate_to_current_version(entries);
}

/// Exported for migration tests (`parseSessionEntries`).
/// Malformed lines are skipped, exactly like upstream.
pub fn parse_session_entries(content: &str) -> Vec<Value> {
    let mut entries: Vec<Value> = Vec::new();
    for line in content.trim().split('\n') {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<Value>(line) {
            entries.push(entry);
        }
    }
    entries
}

/// Type-preserving object access helper used by tests to compare migrated
/// shapes without depending on serde internals.
pub fn field<'a>(entry: &'a Value, key: &str) -> Option<&'a Value> {
    entry.get(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn v1_entries() -> Vec<Value> {
        vec![
            json!({ "type": "session", "id": "sess-1", "timestamp": "2025-01-01T00:00:00Z", "cwd": "/tmp" }),
            json!({ "type": "message", "timestamp": "2025-01-01T00:00:01Z", "message": { "role": "user", "content": "hi", "timestamp": 1 } }),
            json!({ "type": "message", "timestamp": "2025-01-01T00:00:02Z", "message": { "role": "assistant", "content": [{ "type": "text", "text": "hello" }], "api": "test", "provider": "test", "model": "test", "usage": { "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0 }, "stopReason": "stop", "timestamp": 2 } }),
        ]
    }

    #[test]
    fn adds_id_and_parent_id_to_v1_entries() {
        let mut entries = v1_entries();
        migrate_session_entries(&mut entries);

        // Header should have version set (v3 is current after hookMessage->custom migration).
        assert_eq!(entries[0]["version"], json!(3));

        let msg1 = &entries[1];
        let msg2 = &entries[2];

        let id1 = msg1["id"].as_str().unwrap().to_string();
        assert_eq!(id1.len(), 8);
        assert!(msg1["parentId"].is_null());

        let id2 = msg2["id"].as_str().unwrap().to_string();
        assert_eq!(id2.len(), 8);
        assert_eq!(msg2["parentId"].as_str().unwrap(), &id1);
        assert_ne!(id1, id2);
    }

    #[test]
    fn is_idempotent_for_already_migrated() {
        let mut entries: Vec<Value> = vec![
            json!({ "type": "session", "id": "sess-1", "version": 2, "timestamp": "2025-01-01T00:00:00Z", "cwd": "/tmp" }),
            json!({ "type": "message", "id": "abc12345", "parentId": null, "timestamp": "2025-01-01T00:00:01Z", "message": { "role": "user", "content": "hi", "timestamp": 1 } }),
            json!({ "type": "message", "id": "def67890", "parentId": "abc12345", "timestamp": "2025-01-01T00:00:02Z", "message": { "role": "assistant", "content": [{ "type": "text", "text": "hello" }], "api": "test", "provider": "test", "model": "test", "usage": { "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0 }, "stopReason": "stop", "timestamp": 2 } }),
        ];
        migrate_session_entries(&mut entries);

        // IDs are unchanged (already migrated), and the version advances to 3.
        assert_eq!(entries[1]["id"], json!("abc12345"));
        assert_eq!(entries[2]["id"], json!("def67890"));
        assert_eq!(entries[2]["parentId"], json!("abc12345"));
        assert_eq!(entries[0]["version"], json!(3));
    }

    #[test]
    fn renames_hook_message_role_to_custom_in_v2() {
        let mut entries = vec![
            json!({ "type": "session", "id": "sess-1", "version": 2, "timestamp": "t", "cwd": "/tmp" }),
            json!({ "type": "message", "id": "abc12345", "parentId": null, "timestamp": "t", "message": { "role": "hookMessage", "content": "x", "timestamp": 1 } }),
        ];
        migrate_session_entries(&mut entries);
        assert_eq!(entries[1]["message"]["role"], json!("custom"));
    }

    #[test]
    fn converts_compaction_first_kept_index_to_id() {
        let mut entries = vec![
            json!({ "type": "session", "id": "sess-1", "version": 1, "timestamp": "t", "cwd": "/tmp" }),
            json!({ "type": "message", "timestamp": "t", "message": { "role": "user", "content": "u", "timestamp": 1 } }),
            json!({ "type": "message", "timestamp": "t", "message": { "role": "user", "content": "v", "timestamp": 2 } }),
            json!({ "type": "compaction", "summary": "s", "firstKeptEntryIndex": 1, "tokensBefore": 5, "timestamp": "t" }),
        ];
        migrate_session_entries(&mut entries);
        let compaction = &entries[3];
        // firstKeptEntryId points at the entry that used to sit at index 1.
        assert_eq!(
            compaction["firstKeptEntryId"].as_str().unwrap(),
            entries[1]["id"].as_str().unwrap()
        );
        assert!(compaction.get("firstKeptEntryIndex").is_none());
    }

    #[test]
    fn parse_session_entries_skips_malformed_lines() {
        let content = "{ \"type\": \"session\", \"id\": \"s-1\", \"timestamp\": \"t\", \"cwd\": \"/tmp\" }\nnot json\n{ \"type\": \"message\", \"timestamp\": \"t\", \"message\": { \"role\": \"user\", \"content\": \"u\", \"timestamp\": 1 } }\n";
        let entries = parse_session_entries(content);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn assert_valid_session_id_matches_upstream_pattern() {
        assert!(assert_valid_session_id("abc").is_ok());
        assert!(assert_valid_session_id("a-b_c.d").is_ok());
        assert!(assert_valid_session_id("").is_err());
        assert!(assert_valid_session_id("-abc").is_err());
        assert!(assert_valid_session_id("abc-").is_err());
        assert!(assert_valid_session_id("a b").is_err());
    }
}

// ---------------------------------------------------------------------------
// v3 → v4 conversion (legacy session file -> harness JSONL repo format)
// ---------------------------------------------------------------------------

/// Parse an ISO-8601 timestamp (e.g. "2026-08-22T00:00:01.000Z") into epoch
/// milliseconds. Falls back to `now_ms` when unparseable.
fn iso_timestamp_to_ms(value: &Value, now_ms: u64) -> u64 {
    let Some(s) = value.as_str() else {
        return now_ms;
    };
    let s = s.trim();
    // Bare epoch-seconds / epoch-ms numbers.
    if let Ok(ms) = s.parse::<u64>() {
        return ms;
    }
    // YYYY-MM-DDTHH:MM:SS[.fff]Z (UTC only; the legacy format is always Z).
    let digits: Vec<u64> = s
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .map(|c| c.to_digit(10).unwrap_or(0) as u64)
        .collect();
    if digits.len() < 14 {
        return now_ms;
    }
    let y = digits[0] * 1000 + digits[1] * 100 + digits[2] * 10 + digits[3];
    let mo = digits[4] * 10 + digits[5];
    let d = digits[6] * 10 + digits[7];
    let h = digits[8] * 10 + digits[9];
    let mi = digits[10] * 10 + digits[11];
    let se = digits[12] * 10 + digits[13];
    let millis = if digits.len() > 14 {
        digits[14] * 100
            + digits.get(15).copied().unwrap_or(0) * 10
            + digits.get(16).copied().unwrap_or(0)
    } else {
        0
    };
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return now_ms;
    }
    // Days since epoch for the given date (proleptic Gregorian).
    let days = days_from_civil(y, mo, d);
    let secs = days * 86_400 + h as i64 * 3_600 + mi as i64 * 60 + se as i64;
    (secs as u64).saturating_mul(1000).saturating_add(millis)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// algorithm).
fn days_from_civil(y: u64, m: u64, d: u64) -> i64 {
    let y = y as i64;
    let m = m as i64;
    let d = d as i64;
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Convert a legacy (v1–v3) session file into the v4 harness JSONL format.
/// Returns the v4 file content (header + entries). The legacy header's
/// `type:"session"` becomes `kind:"header"`; message entries gain
/// `kind:"entry"`, `lane:"main"`, and a `seq`; ISO timestamps become epoch
/// ms. Non-message entries (model_change, thinking_level_change, ...) are
/// carried through with the same shape.
pub fn convert_legacy_to_v4(content: &str) -> Result<String, String> {
    let mut entries = parse_session_entries(content);
    if entries.is_empty() {
        return Err("session file is empty".to_string());
    }
    migrate_session_entries(&mut entries);

    let now_ms = pi_ai::types::now_ms();
    let header = entries.remove(0);
    let id = header
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "session header is missing id".to_string())?
        .to_string();
    let cwd = header
        .get("cwd")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let created_at = iso_timestamp_to_ms(header.get("timestamp").unwrap_or(&Value::Null), now_ms);

    let mut out = String::new();
    out.push_str(&format!(
        "{}\n",
        serde_json::json!({
            "kind": "header",
            "version": 4,
            "id": id,
            "createdAt": created_at,
            "cwd": cwd,
        })
    ));

    let mut seq = 0u64;
    for entry in entries {
        seq += 1;
        let entry_type = entry
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("message")
            .to_string();
        let entry_id = entry
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("m-{}", uuid::Uuid::new_v4()));
        let parent_id = entry
            .get("parentId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let timestamp = iso_timestamp_to_ms(entry.get("timestamp").unwrap_or(&Value::Null), now_ms);

        let mut v4_entry = serde_json::Map::new();
        v4_entry.insert("kind".to_string(), serde_json::json!("entry"));
        v4_entry.insert("lane".to_string(), serde_json::json!("main"));
        v4_entry.insert("type".to_string(), serde_json::json!(entry_type));
        v4_entry.insert("id".to_string(), serde_json::json!(entry_id));
        v4_entry.insert("seq".to_string(), serde_json::json!(seq));
        match parent_id {
            Some(pid) => {
                v4_entry.insert("parentId".to_string(), serde_json::json!(pid));
            }
            None => {
                v4_entry.insert("parentId".to_string(), Value::Null);
            }
        }
        v4_entry.insert("timestamp".to_string(), serde_json::json!(timestamp));
        // Carry the message payload (or custom data) through. The v4 message
        // payload requires a `timestamp` (epoch ms) on user/assistant/toolResult
        // messages; legacy files may omit it, so inject the entry timestamp.
        for key in [
            "message",
            "customType",
            "data",
            "summary",
            "retainedTail",
            "tokensBefore",
            "details",
            "usage",
            "provider",
            "modelId",
            "thinkingLevel",
            "activeToolNames",
        ] {
            if let Some(value) = entry.get(key) {
                let mut value = value.clone();
                if key == "message" {
                    if let Some(obj) = value.as_object_mut() {
                        if !obj.contains_key("timestamp") {
                            obj.insert("timestamp".to_string(), serde_json::json!(timestamp));
                        }
                    }
                }
                v4_entry.insert(key.to_string(), value);
            }
        }
        out.push_str(&format!("{}\n", serde_json::Value::Object(v4_entry)));
    }
    Ok(out)
}

/// Migrate one legacy v1/v2/v3 file in place before a v4 repository opens it.
/// The converted content is staged beside the source and atomically renamed so
/// a failed conversion never replaces the user's original session file.
pub fn migrate_legacy_session_file(path: &Path) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    let content = std::fs::read_to_string(path)
        .map_err(|error| format!("read legacy session {}: {error}", path.display()))?;
    let Some(first_line) = content.lines().find(|line| !line.trim().is_empty()) else {
        return Ok(false);
    };
    let Ok(first_value) = serde_json::from_str::<Value>(first_line) else {
        // The v4 repository already ignores files with malformed headers
        // during inventory; migration must not make startup stricter.
        return Ok(false);
    };
    if first_value.get("type").and_then(Value::as_str) != Some("session") {
        return Ok(false);
    }

    let converted = convert_legacy_to_v4(&content)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            format!(
                "legacy session path has no valid filename: {}",
                path.display()
            )
        })?;
    let temporary_path = path.with_file_name(format!(".{file_name}.migration.tmp"));
    std::fs::write(&temporary_path, converted).map_err(|error| {
        format!(
            "stage migrated session {}: {error}",
            temporary_path.display()
        )
    })?;
    if let Err(error) = std::fs::rename(&temporary_path, path) {
        let _ = std::fs::remove_file(&temporary_path);
        return Err(format!(
            "publish migrated session {}: {error}",
            path.display()
        ));
    }
    Ok(true)
}

/// Migrate every legacy session below a JSONL session root. Startup invokes
/// this before creating/listing the active session so resume and fork/clone
/// selectors see the same v4 inventory as newly-created sessions.
pub fn migrate_legacy_sessions_in_root(root: &Path) -> Result<usize, String> {
    if !root.exists() {
        return Ok(0);
    }
    let mut directories: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(root)
        .map_err(|error| format!("list session root {}: {error}", root.display()))?
    {
        let entry = entry.map_err(|error| format!("read session root entry: {error}"))?;
        let path = entry.path();
        if path.is_dir() {
            directories.push(path);
        }
    }

    let mut migrated = 0;
    for directory in directories {
        for entry in std::fs::read_dir(&directory)
            .map_err(|error| format!("list session directory {}: {error}", directory.display()))?
        {
            let entry = entry.map_err(|error| format!("read session directory entry: {error}"))?;
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            if migrate_legacy_session_file(&path)? {
                migrated += 1;
            }
        }
    }
    Ok(migrated)
}

#[cfg(test)]
mod v4_conversion_tests {
    use super::*;

    fn v3_file() -> String {
        [
            r#"{"type":"session","version":3,"id":"sess-legacy","timestamp":"2026-08-22T00:00:00.000Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-08-22T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}"#,
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-08-22T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}"#,
        ]
        .join("\n")
    }

    #[test]
    fn converts_v3_header_and_entries_to_v4() {
        let v4 = convert_legacy_to_v4(&v3_file()).unwrap();
        let lines: Vec<&str> = v4.trim().split('\n').collect();
        assert_eq!(lines.len(), 3);
        let header: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["kind"], "header");
        assert_eq!(header["version"], 4);
        assert_eq!(header["id"], "sess-legacy");
        assert_eq!(header["createdAt"].as_u64().unwrap(), 1_787_356_800_000);
        let entry1: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(entry1["kind"], "entry");
        assert_eq!(entry1["lane"], "main");
        assert_eq!(entry1["seq"], 1);
        assert_eq!(entry1["type"], "message");
        assert_eq!(entry1["message"]["content"][0]["text"], "hello");
        let entry2: Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(entry2["seq"], 2);
        assert_eq!(entry2["parentId"], "m1");
    }

    #[test]
    fn migrates_v1_entries_before_conversion() {
        let v1 = [
            r#"{"type":"session","id":"s1","timestamp":"2026-08-22T00:00:00.000Z","cwd":"/tmp"}"#,
            r#"{"type":"message","timestamp":"2026-08-22T00:00:01.000Z","message":{"role":"user","content":"hi"}}"#,
        ]
        .join("\n");
        let v4 = convert_legacy_to_v4(&v1).unwrap();
        let lines: Vec<&str> = v4.trim().split('\n').collect();
        let header: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["version"], 4);
        let entry: Value = serde_json::from_str(lines[1]).unwrap();
        assert!(entry["id"].as_str().is_some(), "v1 entries gain ids");
        assert_eq!(entry["kind"], "entry");
    }

    #[test]
    fn empty_file_errors() {
        assert!(convert_legacy_to_v4("").is_err());
    }
}

#[cfg(test)]
mod repo_integration_tests {
    use super::*;
    use pi_agent::fs::StdFileSystem;
    use pi_agent::session::jsonl::repo::JsonlSessionRepo;

    #[tokio::test]
    async fn converted_v4_file_opens_in_the_repo() {
        let root = std::env::temp_dir().join(format!("pi-migrate-repo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let v3 = [
            r#"{"type":"session","version":3,"id":"sess-legacy","timestamp":"2026-08-22T00:00:00.000Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-08-22T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}"#,
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-08-22T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}"#,
        ]
        .join("\n");
        let v4 = convert_legacy_to_v4(&v3).unwrap();
        let session_root = root.join("sessions");
        std::fs::create_dir_all(&session_root).unwrap();
        let path = session_root.join("imported-sess-legacy.jsonl");
        std::fs::write(&path, &v4).unwrap();

        let repo = JsonlSessionRepo::new(
            StdFileSystem::new("/tmp"),
            session_root.to_string_lossy().into_owned(),
        );
        let metadata = pi_agent::session::types::SessionMetadata {
            id: "sess-legacy".to_string(),
            created_at: 0,
            cwd: "/tmp".to_string(),
            path: path.to_string_lossy().into_owned(),
            modified_at: 0,
            source_format: 4,
            parent_session_id: None,
            legacy_parent_session_path: None,
            metadata: None,
        };
        let session = repo
            .open(&metadata)
            .await
            .expect("repo opens converted v4 file");
        let entries = session
            .find_entries(&pi_agent::session::state::EntryQuery {
                order: Some(pi_agent::session::state::EntryOrder::OldestFirst),
                id: None,
                entry_type: None,
                custom_type: None,
                cursor: None,
                limit: None,
            })
            .await
            .unwrap();
        assert_eq!(entries.len(), 2, "both messages imported");
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod filesystem_migration_tests {
    use super::*;

    fn legacy_file() -> String {
        [
            r#"{"type":"session","version":3,"id":"legacy-file","timestamp":"2026-08-22T00:00:00.000Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-08-22T00:00:01.000Z","message":{"role":"user","content":"hello"}}"#,
        ]
        .join("\n")
    }

    #[test]
    fn rewrites_a_legacy_file_as_v4_and_is_idempotent() {
        let root = std::env::temp_dir().join(format!("pi-legacy-file-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("session.jsonl");
        std::fs::write(&path, legacy_file()).unwrap();

        assert!(migrate_legacy_session_file(&path).unwrap());
        let migrated = std::fs::read_to_string(&path).unwrap();
        assert!(migrated
            .lines()
            .next()
            .unwrap()
            .contains(r#""kind":"header""#));
        assert!(!migrate_legacy_session_file(&path).unwrap());
        assert!(!path.with_file_name(".session.jsonl.migration.tmp").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn migrates_all_jsonl_files_under_the_session_root() {
        let root = std::env::temp_dir().join(format!("pi-legacy-root-{}", uuid::Uuid::new_v4()));
        let directory = root.join("--tmp--");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("legacy.jsonl"), legacy_file()).unwrap();
        std::fs::write(directory.join("already.txt"), "not a session").unwrap();

        assert_eq!(migrate_legacy_sessions_in_root(&root).unwrap(), 1);
        assert!(std::fs::read_to_string(directory.join("legacy.jsonl"))
            .unwrap()
            .starts_with("{\"kind\":\"header\""));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn leaves_non_session_or_malformed_files_untouched() {
        let root = std::env::temp_dir().join(format!("pi-legacy-invalid-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let malformed = root.join("malformed.jsonl");
        std::fs::write(&malformed, "not json\n").unwrap();
        let regular = root.join("regular.jsonl");
        std::fs::write(&regular, "{\"kind\":\"header\",\"version\":4}\n").unwrap();

        assert!(!migrate_legacy_session_file(&malformed).unwrap());
        assert!(!migrate_legacy_session_file(&regular).unwrap());
        assert_eq!(std::fs::read_to_string(&malformed).unwrap(), "not json\n");
        std::fs::remove_dir_all(root).unwrap();
    }
}
