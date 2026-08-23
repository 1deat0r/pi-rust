//! Persisted pi.dev catalog overlay — port of
//! `packages/coding-agent/src/core/remote-catalog-provider.ts`.
//!
//! The upstream wraps each built-in provider with a `refreshModels` that
//! merges a remote `https://pi.dev/api/models/providers/<id>` catalog over
//! the bundled model list, persisting the overlay in `models-store.json`
//! (ETag/Last-Modified revalidation, 4h freshness window).
//!
//! The Rust port implements the same merge/parse/freshness semantics and the
//! live HTTP refresh used by `pi update --models`.

use pi_ai::model::Model;
use pi_ai::models::ModelsStore;
use pi_ai::models::ModelsStoreEntry;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DEFAULT_CATALOG_BASE_URL: &str = "https://pi.dev";
pub const REMOTE_CATALOG_ATTEMPT_TIMEOUT_MS: u64 = 4_000;
pub const REMOTE_CATALOG_REFRESH_INTERVAL_MS: u64 = 4 * 60 * 60 * 1000;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn catalog_url(provider_id: &str) -> Result<reqwest::Url, String> {
    let base = std::env::var("PI_MODEL_CATALOG_URL")
        .unwrap_or_else(|_| DEFAULT_CATALOG_BASE_URL.to_string());
    let mut url =
        reqwest::Url::parse(&base).map_err(|e| format!("invalid model catalog URL: {e}"))?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| "model catalog URL cannot be a base URL".to_string())?;
        segments.pop_if_empty();
        segments
            .push("api")
            .push("models")
            .push("providers")
            .push(provider_id);
    }
    Ok(url)
}

/// Refresh all dynamic provider catalogs and persist the same
/// `models-store.json` entries used by the upstream runtime. Returns the
/// number of providers successfully refreshed.
pub async fn refresh_catalogs(agent_dir: &Path, force: bool) -> Result<usize, String> {
    if std::env::var_os("PI_OFFLINE").is_some() {
        return Err("model catalog refresh is unavailable in offline mode".to_string());
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(REMOTE_CATALOG_ATTEMPT_TIMEOUT_MS))
        .build()
        .map_err(|e| format!("create model catalog client: {e}"))?;
    let store =
        crate::core::models_store::FileModelsStore::new(agent_dir.join("models-store.json"));
    let mut refreshed = 0usize;
    let mut errors = Vec::new();
    for provider_id in pi_ai::model_catalog::get_builtin_providers() {
        let stored = store.read(&provider_id);
        if !force && within_refresh_freshness_window(stored.as_ref(), now_ms()) {
            continue;
        }
        let url = match catalog_url(&provider_id) {
            Ok(url) => url,
            Err(error) => {
                errors.push(format!("{provider_id}: {error}"));
                continue;
            }
        };
        let mut request = client
            .get(url)
            .header("accept", "application/json")
            .header("user-agent", format!("pi/{}", crate::config::VERSION));
        if let Some(etag) = stored
            .as_ref()
            .and_then(|entry| entry.etag.as_deref())
            .filter(|_| stored.as_ref().is_some_and(|e| !e.models.is_empty()))
        {
            request = request.header("if-none-match", etag);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                errors.push(format!("{provider_id}: {error}"));
                continue;
            }
        };
        let checked_at = now_ms();
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            if let Some(mut entry) = stored {
                entry.checked_at = Some(checked_at);
                store.write(&provider_id, &entry);
                refreshed += 1;
            }
            continue;
        }
        if response.status() == reqwest::StatusCode::NOT_FOUND
            || response.status() == reqwest::StatusCode::NOT_IMPLEMENTED
        {
            let models = stored
                .as_ref()
                .map(|entry| entry.models.clone())
                .unwrap_or_default();
            store.write(
                &provider_id,
                &ModelsStoreEntry {
                    models,
                    last_modified: Some(0),
                    checked_at: Some(checked_at),
                    etag: None,
                },
            );
            refreshed += 1;
            continue;
        }
        if !response.status().is_success() {
            if let Some(mut entry) = stored {
                // Keep a valid cached body/ETag while moving the freshness
                // clock, matching upstream's transient-failure publication.
                entry.checked_at = Some(checked_at);
                store.write(&provider_id, &entry);
            }
            errors.push(format!(
                "{provider_id}: model catalog request failed: {}",
                response.status()
            ));
            continue;
        }
        let etag = response
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let last_modified = response
            .headers()
            .get("last-modified")
            .and_then(|value| value.to_str().ok())
            .and_then(parse_http_date_ms)
            .unwrap_or(0);
        let body = match response.json::<serde_json::Value>().await {
            Ok(body) => body,
            Err(error) => {
                errors.push(format!(
                    "{provider_id}: invalid model catalog JSON: {error}"
                ));
                continue;
            }
        };
        let models = match parse_catalog(&provider_id, &body) {
            Ok(models) => models,
            Err(error) => {
                errors.push(format!("{provider_id}: {error}"));
                continue;
            }
        };
        store.write(
            &provider_id,
            &ModelsStoreEntry {
                models,
                last_modified: Some(last_modified),
                checked_at: Some(checked_at),
                etag,
            },
        );
        refreshed += 1;
    }
    if errors.is_empty() {
        Ok(refreshed)
    } else {
        Err(format!(
            "could not refresh model catalogs: {}",
            errors.join("; ")
        ))
    }
}

fn parse_http_date_ms(value: &str) -> Option<u64> {
    // HTTP-date has three wire representations (RFC 7231 §7.1.1.1). The
    // upstream uses Date.parse and stores 0 for an invalid/missing header, so
    // this parser intentionally returns None rather than falling back to the
    // request time.
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    fn parse_time(value: &str) -> Option<(u32, u32, u32)> {
        let mut parts = value.split(':');
        let hour = parts.next()?.parse::<u32>().ok()?;
        let minute = parts.next()?.parse::<u32>().ok()?;
        let second = parts.next()?.parse::<u32>().ok()?;
        if parts.next().is_some() || hour >= 24 || minute >= 60 || second >= 60 {
            return None;
        }
        Some((hour, minute, second))
    }

    fn month_number(month: &str, months: &[&str; 12]) -> Option<u32> {
        months
            .iter()
            .position(|candidate| candidate.eq_ignore_ascii_case(month))
            .map(|index| index as u32 + 1)
    }

    fn days_in_month(year: i64, month: u32) -> u32 {
        match month {
            2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
            2 => 28,
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        }
    }

    // Days since 1970-01-01, using Howard Hinnant's civil-calendar formula.
    fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
        let adjusted_year = year - i64::from(month <= 2);
        let era = (if adjusted_year >= 0 {
            adjusted_year
        } else {
            adjusted_year - 399
        }) / 400;
        let year_of_era = adjusted_year - era * 400;
        let month_prime = i64::from(month) + if month > 2 { -3 } else { 9 };
        let day_of_year = (153 * month_prime + 2) / 5 + i64::from(day) - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        era * 146_097 + day_of_era - 719_468
    }

    let trimmed = value.trim();
    let (day, month, year, time) = if let Some((_, rest)) = trimmed.split_once(',') {
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.len() != 3 && fields.len() != 5 {
            return None;
        }
        let (day, month, year, time) = if fields.len() == 3 {
            let date_parts: Vec<&str> = fields[0].split('-').collect();
            if date_parts.len() != 3 {
                return None;
            }
            let short_year = date_parts[2].parse::<u32>().ok()?;
            let year = if short_year >= 69 {
                1900 + i64::from(short_year)
            } else {
                2000 + i64::from(short_year)
            };
            (
                date_parts[0].parse::<u32>().ok()?,
                month_number(date_parts[1], &MONTHS)?,
                year,
                fields[1],
            )
        } else {
            (
                fields[0].parse::<u32>().ok()?,
                month_number(fields[1], &MONTHS)?,
                fields[2].parse::<i64>().ok()?,
                fields[3],
            )
        };
        (day, month, year, time)
    } else {
        // ANSI C asctime format: `Sun Nov  6 08:49:37 1994`.
        let fields: Vec<&str> = trimmed.split_whitespace().collect();
        if fields.len() != 5 {
            return None;
        }
        (
            fields[2].parse::<u32>().ok()?,
            month_number(fields[1], &MONTHS)?,
            fields[4].parse::<i64>().ok()?,
            fields[3],
        )
    };
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) || year < 0 {
        return None;
    }
    let (hour, minute, second) = parse_time(time)?;
    let seconds = days_from_civil(year, month, day)
        .checked_mul(86_400)?
        .checked_add(i64::from(hour * 3_600 + minute * 60 + second))?;
    u64::try_from(seconds).ok()?.checked_mul(1_000)
}

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
        return Err(format!(
            "Invalid model catalog for provider \"{provider_id}\""
        ));
    };
    let mut models = Vec::new();
    for entry in entries {
        let Some(obj) = entry.as_object() else {
            continue;
        };
        if !obj.contains_key("id") {
            continue;
        }
        // Upstream spreads `{ ...model, provider: providerId }` after filtering
        // on `id`, so a missing provider in the body is fine; the provider id
        // always wins.
        let mut entry = entry.clone();
        if let Some(obj) = entry.as_object_mut() {
            obj.insert(
                "provider".to_string(),
                serde_json::Value::String(provider_id.to_string()),
            );
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
    let Some(entry) = entry else {
        return Vec::new();
    };
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
            if let (Some(checked_at), Some(_last_modified)) =
                (entry.checked_at, entry.last_modified)
            {
                now_ms.saturating_sub(checked_at) < REMOTE_CATALOG_REFRESH_INTERVAL_MS
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
        assert_eq!(
            models[0].provider, "demo",
            "provider must be overwritten with the provider id"
        );

        let wrapped = json!({ "models": [ { "id": "m2", "name": "M2", "api": "openai-responses",
            "baseUrl": "https://demo.example.com/v1", "reasoning": false,
            "input": ["text"], "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 128000, "maxTokens": 16384 } ] });
        let models =
            parse_catalog("demo", &wrapped).unwrap_or_else(|e| panic!("wrapped parse failed: {e}"));
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

    #[test]
    fn parses_all_http_date_forms_and_rejects_invalid_values() {
        let expected = 784_111_777_000;
        assert_eq!(
            parse_http_date_ms("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(expected)
        );
        assert_eq!(
            parse_http_date_ms("Sunday, 06-Nov-94 08:49:37 GMT"),
            Some(expected)
        );
        assert_eq!(
            parse_http_date_ms("Sun Nov  6 08:49:37 1994"),
            Some(expected)
        );
        assert_eq!(parse_http_date_ms("not a date"), None);
        assert_eq!(parse_http_date_ms("Sun, 31 Feb 1994 08:49:37 GMT"), None);
    }

    #[test]
    fn invalid_last_modified_is_older_than_any_generated_catalog() {
        let entry = ModelsStoreEntry {
            models: vec![model("p", "remote")],
            last_modified: Some(0),
            checked_at: Some(1_000),
            etag: None,
        };
        assert!(remote_models(Some(&entry), Some(1)).is_empty());
        assert!(within_refresh_freshness_window(Some(&entry), 1_001));
    }
}
