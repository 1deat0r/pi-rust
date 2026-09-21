#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Offline parity for the intentional-divergence `meta-ai` provider row.
//!
//! There is no upstream pi oracle for Meta Model API (Muse Spark); the
//! oracle is `docs/meta-ai-provider-spec.md`, which records the live
//! `/v1/models` roster (2026-09-21) and the dev.meta.ai reasoning spec.
//! These tests pin the observable registration contract: catalog ids,
//! endpoint/auth labels, and the reasoning-effort mapping (`none` 400s →
//! `minimal`; `max` standard-tier 1.3 only).

use std::collections::BTreeMap;
use std::sync::Arc;

use pi_ai::auth::AuthContext;
use pi_ai::providers::meta_ai_provider;
use pi_ai::{ModelInput, ModelThinkingLevel};

fn env_context(vars: &[(&str, &str)]) -> AuthContext {
    let map: BTreeMap<String, String> = vars
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    AuthContext {
        env: Arc::new(move |name| map.get(name).cloned()),
        file_exists: Arc::new(|_| false),
    }
}

fn level_map(id: &str) -> std::collections::BTreeMap<ModelThinkingLevel, Option<String>> {
    let provider = meta_ai_provider();
    let model = provider
        .models
        .iter()
        .find(|m| m.id == id)
        .unwrap_or_else(|| panic!("meta-ai catalog is missing {id}"));
    model
        .thinking_level_map
        .clone()
        .unwrap_or_else(|| panic!("meta-ai model {id} has no thinking_level_map"))
}

#[test]
fn meta_ai_catalog_carries_the_live_chat_roster() {
    let provider = meta_ai_provider();
    assert_eq!(provider.id, "meta-ai");
    let mut ids: Vec<&str> = provider.models.iter().map(|m| m.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        [
            "muse-spark-1.1",
            "muse-spark-1.2",
            "muse-spark-1.2-contributor",
            "muse-spark-1.3",
            "muse-spark-1.3-contributor",
        ],
        "live /v1/models chat roster per docs/meta-ai-provider-spec.md",
    );
    for model in &provider.models {
        assert_eq!(model.api, "openai-completions");
        assert_eq!(model.provider, "meta-ai");
        assert_eq!(model.base_url, "https://api.meta.ai/v1");
        assert!(model.reasoning, "Spark models always reason");
        assert_eq!(model.context_window, 1_048_576);
        assert!(
            model.input.contains(&ModelInput::Text),
            "chat models accept text"
        );
    }
}

#[test]
fn meta_ai_auth_prefers_documented_model_api_key() {
    let provider = meta_ai_provider();
    let auth = provider.auth.api_key.expect("meta-ai API-key auth");

    // Documented var resolves.
    let resolved = auth
        .resolve(&env_context(&[("MODEL_API_KEY", "docs-key")]), None)
        .expect("MODEL_API_KEY resolves");
    assert_eq!(resolved.source.as_deref(), Some("MODEL_API_KEY"));

    // Working alias resolves when the documented var is absent.
    let resolved = auth
        .resolve(&env_context(&[("META_API_KEY", "local-key")]), None)
        .expect("META_API_KEY resolves");
    assert_eq!(resolved.source.as_deref(), Some("META_API_KEY"));

    // Documented var wins when both are set (first-set-wins resolution).
    let resolved = auth
        .resolve(
            &env_context(&[("MODEL_API_KEY", "docs-key"), ("META_API_KEY", "local-key")]),
            None,
        )
        .expect("either key resolves");
    assert_eq!(resolved.source.as_deref(), Some("MODEL_API_KEY"));
}

#[test]
fn meta_ai_effort_map_never_emits_bare_none() {
    for id in [
        "muse-spark-1.3",
        "muse-spark-1.2",
        "muse-spark-1.1",
        "muse-spark-1.3-contributor",
        "muse-spark-1.2-contributor",
    ] {
        let map = level_map(id);
        // `none` returns HTTP 400 on Muse; disabled maps to minimal.
        assert_eq!(
            map.get(&ModelThinkingLevel::Off),
            Some(&Some("minimal".to_string())),
            "{id} off must map to minimal, never omit or none"
        );
        for level in [
            ModelThinkingLevel::Minimal,
            ModelThinkingLevel::Low,
            ModelThinkingLevel::Medium,
            ModelThinkingLevel::High,
            ModelThinkingLevel::Xhigh,
        ] {
            assert_eq!(
                map.get(&level),
                Some(&Some(level.as_str().to_string())),
                "{id} {level:?} passes through verbatim"
            );
        }
    }
}

#[test]
fn meta_ai_max_effort_is_standard_tier_13_only() {
    assert_eq!(
        level_map("muse-spark-1.3").get(&ModelThinkingLevel::Max),
        Some(&Some("max".to_string())),
        "standard-tier 1.3 supports max",
    );
    for id in [
        "muse-spark-1.2",
        "muse-spark-1.1",
        "muse-spark-1.3-contributor",
        "muse-spark-1.2-contributor",
    ] {
        assert_eq!(
            level_map(id).get(&ModelThinkingLevel::Max),
            Some(&None),
            "{id} must omit max (standard-tier 1.3 only)",
        );
    }
}
