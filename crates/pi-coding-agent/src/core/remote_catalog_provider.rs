//! Persisted pi.dev catalog overlay — port of
//! `packages/coding-agent/src/core/remote-catalog-provider.ts`.
//!
//! The upstream wraps each built-in provider with a `refreshModels` that
//! merges a remote `https://pi.dev/api/models/providers/<id>` catalog over
//! the bundled model list, persisting the overlay in `models-store.json`
//! (ETag/Last-Modified revalidation, 4h freshness window).
//!
//! The Rust port implements the pure-data surface here (merge/parse/freshness
//! semantics) so the model-registry and `update --models` paths can consume
//! the same rules. The live HTTP fetch is a TODO: `pi update --models` in the
//! port currently documents this divergence (see commands/package.rs) until
//! the pi.dev catalog fetch + Models facade refresh plumbing lands.

use pi_ai::model::Model;
use pi_ai::models::ModelsStoreEntry;

pub const DEFAULT_CATALOG_BASE_URL: &str = "https://pi.dev";
pub const REMOTE_CATALOG_ATTEMPT_TIMEOUT_MS: u64 = 4_000;
pub const REMOTE_CATALOG_REFRESH_INTERVAL_MS: u64 = 4 * 60 * 60 * 1000;

/// Merge a dynamic catalog over a baseline (upstream `mergeModels`): entries
/// with matching ids are replaced in place; new ids are appended.
pub fn merge_models(baseline: &[Model], dynamic: &[Model]) -> Vec<Model> {
    let mut merged: Vec<Model> = baseline.to_vec();
    for model in dynamic {
        if let Some(index) = merged.iter().position(|entry| entry.id == model.id) {
            merged[index] = model.clone();
        } else {
            merged.push(model.clone());
        }
    }
    merged
}

/// Parse a remote catalog response body (upstream `parseCatalog`). Accepts an
/// array, `{ models: [...] }`, or a map of entries.
pub fn parse_catalog(provider_id: &str, value: &serde_json::Value) -> Result<Vec<Model>, String> {
    let entries: Option<Vec<&serde_json::Value>> = match value {
        serde_json::Value::Array(entries) => Some(entries.iter().collect()),
        serde_json::Value::Object(obj) => {
            if let Some(models) = obj.get("models") {
                models.as_array().map(|a| a.iter().collect())
            } else {
                Some(obj.values().collect())
            }
        }
        _ => None,
    };
    let Some(entries) = entries else {
        return Err(format!("Invalid model catalog for provider \"{provider_id}\""));
    };
    let mut models = Vec::new();
    for entry in entries {
        let Some(obj) = entry.as_object() else { continue };
        if !obj.contains_key("id") {
            continue;
        }
        // Upstream spreads `{ ...model, provider: providerId }` after filtering
        // on `id`, so a missing provider in the body is fine; the provider id
        // always wins.
        let mut entry = entry.clone();
        if let Some(obj) = entry.as_object_mut() {
            obj.insert("provider".to_string(), serde_json::Value::String(provider_id.to_string()));
        }
        match serde_json::from_value::<Model>(entry) {
            Ok(model) => models.push(model),
            Err(_) => continue,
        }
    }
    Ok(models)
}

/// The remote subset of a stored catalog entry that is newer than the
/// bundled catalog (upstream `remoteModels`).
pub fn remote_models(
    entry: Option<&ModelsStoreEntry>,
    local_generated_at: Option<u64>,
) -> Vec<Model> {
    let Some(entry) = entry else { return Vec::new() };
    if let Some(local_generated_at) = local_generated_at {
        if entry.last_modified.is_none() || entry.last_modified.unwrap_or(0) <= local_generated_at {
            return Vec::new();
        }
    }
    entry.models.clone()
}

/// Whether a refresh should be skipped due to the freshness window
/// (upstream inline check: `now - checkedAt < interval`).
pub fn within_refresh_freshness_window(entry: Option<&ModelsStoreEntry>, now_ms: u64) -> bool {
    match entry {
        Some(entry) => {
            if let (Some(checked_at), Some(last_modified)) = (entry.checked_at, entry.last_modified) {
                now_ms.saturating_sub(checked_at) < REMOTE_CATALOG_REFRESH_INTERVAL_MS
                    && last_modified > 0
            } else {
                false
            }
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model(provider: &str, id: &str) -> Model {
        Model::new(id, id, "openai-responses", provider)
    }

    #[test]
    fn merge_models_replaces_and_appends() {
        let baseline = vec![model("p", "a"), model("p", "b")];
        let dynamic = vec![model("p", "b"), model("p", "c")];
        let merged = merge_models(&baseline, &dynamic);
        let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
        // Replaced entry carries dynamic provider tag.
        assert_eq!(merged[1].provider, "p");
    }

    #[test]
    fn parse_catalog_array_and_objects() {
        let array = json!([
            { "id": "m1", "name": "M1", "api": "openai-responses", "provider": "original",
              "baseUrl": "https://demo.example.com/v1", "reasoning": false,
              "input": ["text"], "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
              "contextWindow": 128000, "maxTokens": 16384 }
        ]);
        let models = parse_catalog("demo", &array).unwrap_or_else(|e| panic!("parse failed: {e}"));
        assert_eq!(models.len(), 1, "models: {models:?}");
        assert_eq!(models[0].provider, "demo", "provider must be overwritten with the provider id");

        let wrapped = json!({ "models": [ { "id": "m2", "name": "M2", "api": "openai-responses",
            "baseUrl": "https://demo.example.com/v1", "reasoning": false,
            "input": ["text"], "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 128000, "maxTokens": 16384 } ] });
        let models = parse_catalog("demo", &wrapped).unwrap_or_else(|e| panic!("wrapped parse failed: {e}"));
        assert_eq!(models.len(), 1, "wrapped models: {models:?}");
        assert_eq!(models[0].id, "m2");

        // An object without "models" falls back to Object.values (upstream),
        // filtering entries without ids to an empty list.
        let empty_object = json!({ "foo": 1 });
        assert!(parse_catalog("demo", &empty_object).unwrap().is_empty());

        // A non-object/array body is invalid.
        let invalid = json!("nope");
        assert!(parse_catalog("demo", &invalid).is_err());

        // Entries without an id are skipped.
        let no_ids = json!([{ "name": "x" }]);
        let models = parse_catalog("demo", &no_ids).unwrap();
        assert!(models.is_empty());
    }

    #[test]
    fn remote_models_filters_by_local_generated_at() {
        let entry = ModelsStoreEntry {
            models: vec![model("p", "remote")],
            last_modified: Some(500),
            checked_at: Some(1000),
            etag: None,
        };
        // Catalog generated locally at 600 > 500 → overlay suppressed.
        assert!(remote_models(Some(&entry), Some(600)).is_empty());
        // Catalog generated locally at 400 < 500 → overlay applies.
        assert_eq!(remote_models(Some(&entry), Some(400)).len(), 1);
        assert!(remote_models(None, None).is_empty());
    }

    #[test]
    fn freshness_window_suppresses_recheck() {
        let entry = ModelsStoreEntry {
            models: vec![model("p", "remote")],
            last_modified: Some(500),
            checked_at: Some(1_000_000),
            etag: None,
        };
        let now = 1_000_000 + REMOTE_CATALOG_REFRESH_INTERVAL_MS - 1;
        assert!(within_refresh_freshness_window(Some(&entry), now));
        let later = 1_000_000 + REMOTE_CATALOG_REFRESH_INTERVAL_MS + 1;
        assert!(!within_refresh_freshness_window(Some(&entry), later));
        assert!(!within_refresh_freshness_window(None, now));
    }
}
