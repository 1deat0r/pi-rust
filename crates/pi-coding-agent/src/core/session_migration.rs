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
            if let Some(first_kept_index) = entry.get("firstKeptEntryIndex").and_then(Value::as_u64).map(|i| i as usize) {
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
    let header = entries.iter().find(|e| e.get("type").and_then(Value::as_str) == Some("session"));
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
        assert_eq!(compaction["firstKeptEntryId"].as_str().unwrap(), entries[1]["id"].as_str().unwrap());
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
