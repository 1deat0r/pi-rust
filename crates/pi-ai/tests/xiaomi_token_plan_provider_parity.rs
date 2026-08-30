#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Offline registration/catalog parity for Xiaomi and Qwen Token Plan rows.
//!
//! The pinned upstream provider modules are all OpenAI Completions adapters.
//! These tests keep the observable registration contract (display names,
//! endpoints, auth labels/sources, and generated model ids) separate from
//! live-vendor inference evidence.

use std::collections::BTreeMap;
use std::sync::Arc;

use pi_ai::auth::{ApiKeyCredential, AuthContext};
use pi_ai::models::Provider;
use pi_ai::providers::{
    qwen_token_plan_cn_provider, qwen_token_plan_individual_provider, qwen_token_plan_provider,
    xiaomi_provider, xiaomi_token_plan_ams_provider, xiaomi_token_plan_cn_provider,
    xiaomi_token_plan_sgp_provider,
};

type XiaomiCase = (
    &'static str,
    fn() -> Provider,
    &'static str,
    &'static str,
    &'static str,
    &'static [&'static str],
);

type QwenCase = (
    &'static str,
    fn() -> Provider,
    &'static str,
    &'static str,
    &'static str,
    usize,
);

fn fixture_context(env_name: &'static str, value: &'static str) -> AuthContext {
    let env_name = env_name.to_string();
    let value = value.to_string();
    AuthContext {
        env: Arc::new(move |name| (name == env_name.as_str()).then(|| value.clone())),
        file_exists: Arc::new(|_| false),
    }
}

fn model_ids(provider: &Provider) -> Vec<&str> {
    provider
        .models
        .iter()
        .map(|model| model.id.as_str())
        .collect()
}

#[test]
fn xiaomi_rows_match_pinned_registration_catalog_and_auth_contract() {
    let cases: &[XiaomiCase] = &[
        (
            "xiaomi",
            xiaomi_provider,
            "Xiaomi",
            "Xiaomi API key",
            "https://api.xiaomimimo.com/v1",
            &["mimo-v2.5", "mimo-v2.5-pro", "mimo-v2.5-pro-ultraspeed"],
        ),
        (
            "xiaomi-token-plan-ams",
            xiaomi_token_plan_ams_provider,
            "Xiaomi Token Plan AMS",
            "Xiaomi Token Plan AMS API key",
            "https://token-plan-ams.xiaomimimo.com/v1",
            &["mimo-v2.5", "mimo-v2.5-pro"],
        ),
        (
            "xiaomi-token-plan-cn",
            xiaomi_token_plan_cn_provider,
            "Xiaomi Token Plan CN",
            "Xiaomi Token Plan CN API key",
            "https://token-plan-cn.xiaomimimo.com/v1",
            &["mimo-v2.5", "mimo-v2.5-pro"],
        ),
        (
            "xiaomi-token-plan-sgp",
            xiaomi_token_plan_sgp_provider,
            "Xiaomi Token Plan SGP",
            "Xiaomi Token Plan SGP API key",
            "https://token-plan-sgp.xiaomimimo.com/v1",
            &["mimo-v2.5", "mimo-v2.5-pro"],
        ),
    ];

    for &(id, constructor, name, auth_name, base_url, expected_ids) in cases {
        let provider = constructor();
        assert_eq!(provider.id, id);
        assert_eq!(provider.name, name);
        assert_eq!(provider.base_url.as_deref(), Some(base_url));
        assert_eq!(model_ids(&provider).as_slice(), expected_ids);
        assert!(provider
            .models
            .iter()
            .all(|model| model.api == "openai-completions"));
        assert!(provider
            .models
            .iter()
            .all(|model| model.base_url == base_url));

        let auth = provider.auth.api_key.expect("Xiaomi API-key auth");
        assert_eq!(auth.name(), auth_name);
        let context = fixture_context(
            match id {
                "xiaomi" => "XIAOMI_API_KEY",
                "xiaomi-token-plan-ams" => "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
                "xiaomi-token-plan-cn" => "XIAOMI_TOKEN_PLAN_CN_API_KEY",
                "xiaomi-token-plan-sgp" => "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
                _ => unreachable!("case is exhaustive"),
            },
            "fixture-key",
        );
        let resolved = auth.resolve(&context, None).expect("fixture env key");
        assert_eq!(
            resolved.source.as_deref(),
            Some(match id {
                "xiaomi" => "XIAOMI_API_KEY",
                "xiaomi-token-plan-ams" => "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
                "xiaomi-token-plan-cn" => "XIAOMI_TOKEN_PLAN_CN_API_KEY",
                "xiaomi-token-plan-sgp" => "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
                _ => unreachable!("case is exhaustive"),
            })
        );
        assert_eq!(resolved.auth.api_key.as_deref(), Some("fixture-key"));

        let stored = ApiKeyCredential {
            key: Some("stored-fixture-key".to_string()),
            env: None,
        };
        let stored_result = auth
            .resolve(&context, Some(&stored))
            .expect("stored key takes precedence");
        assert_eq!(stored_result.source.as_deref(), Some("stored credential"));
        assert_eq!(
            stored_result.auth.api_key.as_deref(),
            Some("stored-fixture-key")
        );

        let empty_context = AuthContext {
            env: Arc::new(|_| Some("  ".to_string())),
            file_exists: Arc::new(|_| false),
        };
        assert!(auth.resolve(&empty_context, None).is_none());
    }
}

#[test]
fn qwen_token_plan_rows_keep_exact_catalog_dimensions_and_shared_key_boundary() {
    let cases: &[QwenCase] = &[
        (
            "qwen-token-plan",
            qwen_token_plan_provider,
            "Qwen Token Plan",
            "Qwen Token Plan API key",
            "QWEN_TOKEN_PLAN_API_KEY",
            17,
        ),
        (
            "qwen-token-plan-cn",
            qwen_token_plan_cn_provider,
            "Qwen Token Plan CN",
            "Qwen Token Plan CN API key",
            "QWEN_TOKEN_PLAN_CN_API_KEY",
            17,
        ),
        (
            "qwen-token-plan-individual",
            qwen_token_plan_individual_provider,
            "Qwen Token Plan Individual",
            "Qwen Token Plan Individual API key",
            "QWEN_TOKEN_PLAN_API_KEY",
            8,
        ),
    ];

    for &(id, constructor, name, auth_name, env_name, expected_count) in cases {
        let provider = constructor();
        assert_eq!(provider.id, id);
        assert_eq!(provider.name, name);
        assert_eq!(provider.models.len(), expected_count);
        assert!(provider
            .models
            .iter()
            .all(|model| model.api == "openai-completions"));
        let auth = provider.auth.api_key.expect("Qwen API-key auth");
        assert_eq!(auth.name(), auth_name);

        let context = fixture_context(env_name, "qwen-fixture-key");
        let resolved = auth.resolve(&context, None).expect("fixture env key");
        assert_eq!(resolved.source.as_deref(), Some(env_name));
        assert_eq!(resolved.auth.api_key.as_deref(), Some("qwen-fixture-key"));
    }

    let individual = qwen_token_plan_individual_provider();
    let envs = BTreeMap::from([(
        "QWEN_TOKEN_PLAN_CN_API_KEY".to_string(),
        "wrong-region-key".to_string(),
    )]);
    let context = AuthContext {
        env: Arc::new(move |name| envs.get(name).cloned()),
        file_exists: Arc::new(|_| false),
    };
    let auth = individual.auth.api_key.expect("individual API-key auth");
    assert!(auth.resolve(&context, None).is_none());
}
