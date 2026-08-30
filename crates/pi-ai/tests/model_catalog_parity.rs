#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

use std::sync::{Arc, Mutex};

use pi_ai::auth::ProviderAuth;
use pi_ai::event_stream::create_error_stream;
use pi_ai::model::Model;
use pi_ai::model_catalog::{merge_model_lists, remote_catalog_is_newer};
use pi_ai::models::{
    create_models, create_provider_with_fetch_models, CreateModelsOptions, CreateProviderOptions,
    InMemoryModelsStore, ModelsRefreshOptions, ModelsStore, ProviderApiSpec, ProviderStreams,
};
use pi_ai::providers::all::builtin_providers;
use pi_ai::types::{Context, SimpleStreamOptions, StreamOptions};

fn model(provider: &str, id: &str, name: &str) -> Model {
    let mut model = Model::new(id, name, "openai-responses", provider);
    model.base_url = "https://example.test/v1".to_string();
    model.input = vec![pi_ai::model::ModelInput::Text];
    model
}

fn streams() -> ProviderApiSpec {
    let stream: pi_ai::models::StreamFn = Arc::new(
        |model: &Model, _context: &Context, _options: Option<&StreamOptions>| {
            create_error_stream(&model.api, &model.provider, &model.id, "unused".to_string())
        },
    );
    let stream_simple: pi_ai::models::SimpleStreamFn = Arc::new(
        |model: &Model, _context: &Context, _options: Option<&SimpleStreamOptions>| {
            create_error_stream(&model.api, &model.provider, &model.id, "unused".to_string())
        },
    );
    ProviderApiSpec::Single(ProviderStreams {
        stream,
        stream_simple,
        fetch_deferred: None,
        cancel_deferred: None,
    })
}

fn provider_options(id: &str, models: Vec<Model>) -> CreateProviderOptions {
    CreateProviderOptions {
        id: id.to_string(),
        name: None,
        base_url: Some("https://example.test/v1".to_string()),
        headers: None,
        auth: ProviderAuth::default(),
        models,
        api: streams(),
        filter_models: None,
    }
}

#[test]
fn merge_replaces_in_place_and_appends_dynamic_models() {
    let baseline = vec![
        model("custom", "same", "baseline"),
        model("custom", "base", "base"),
    ];
    let dynamic = vec![
        model("custom", "same", "remote"),
        model("custom", "new", "new"),
    ];
    let merged = merge_model_lists(&baseline, &dynamic);

    assert_eq!(
        merged
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        ["same", "base", "new"]
    );
    assert_eq!(merged[0].name, "remote");
}

#[test]
fn generated_at_precedence_requires_a_newer_remote_timestamp() {
    assert!(remote_catalog_is_newer(Some(101), Some(100)));
    assert!(!remote_catalog_is_newer(Some(100), Some(100)));
    assert!(!remote_catalog_is_newer(Some(99), Some(100)));
    assert!(remote_catalog_is_newer(Some(99), None));
    assert!(!remote_catalog_is_newer(None, Some(100)));
}

#[test]
fn google_vertex_and_huggingface_catalogs_match_provider_oracle_entries() {
    let providers = builtin_providers();
    let provider = |id: &str| {
        providers
            .iter()
            .find(|provider| provider.id == id)
            .unwrap_or_else(|| panic!("missing provider {id}"))
    };

    let vertex = provider("google-vertex");
    assert_eq!(vertex.name, "Google Vertex AI");
    assert_eq!(
        vertex.base_url.as_deref(),
        Some("https://{location}-aiplatform.googleapis.com")
    );
    let vertex_model = vertex
        .models
        .iter()
        .find(|model| model.id == "gemini-3.6-flash")
        .expect("Gemini 3.6 Flash catalog entry");
    assert_eq!(vertex_model.cost.input, 0.75);
    assert_eq!(vertex_model.cost.output, 3.75);
    assert_eq!(vertex_model.cost.cache_read, 0.075);

    let huggingface = provider("huggingface");
    assert_eq!(huggingface.name, "Hugging Face");
    assert_eq!(
        huggingface.base_url.as_deref(),
        Some("https://router.huggingface.co/v1")
    );
    for model_id in [
        "Qwen/Qwen3-VL-235B-A22B-Instruct",
        "Qwen/Qwen3-VL-235B-A22B-Thinking",
        "Qwen/Qwen3.8-2.4T-A95B",
        "Qwen/Qwen3.8-27B",
        "deepseek-ai/DeepSeek-V4-Pro-0813",
        "zai-org/GLM-4.6V-Flash",
        "zai-org/GLM-5.3-Flash",
    ] {
        assert!(
            huggingface.models.iter().any(|model| model.id == model_id),
            "missing Hugging Face model {model_id}"
        );
    }
    let minimax_m2 = huggingface
        .models
        .iter()
        .find(|model| model.id == "MiniMaxAI/MiniMax-M2")
        .expect("MiniMax M2 catalog entry");
    assert_eq!(minimax_m2.max_tokens, 131_072);
    let minimax_m3 = huggingface
        .models
        .iter()
        .find(|model| model.id == "MiniMaxAI/MiniMax-M3")
        .expect("MiniMax M3 catalog entry");
    assert_eq!(minimax_m3.max_tokens, 512_000);

    for (id, name, base_url) in [
        ("groq", "Groq", "https://api.groq.com/openai/v1"),
        ("moonshotai", "Moonshot AI", "https://api.moonshot.ai/v1"),
    ] {
        let provider = provider(id);
        assert_eq!(provider.name, name);
        assert_eq!(provider.base_url.as_deref(), Some(base_url));
        assert!(!provider.models.is_empty(), "{id} catalog is empty");
        assert!(
            provider
                .models
                .iter()
                .all(|model| model.api == "openai-completions"),
            "{id} must use the OpenAI completions adapter"
        );
    }
}

#[test]
fn kimi_and_minimax_anthropic_catalogs_match_provider_oracle_entries() {
    let providers = builtin_providers();
    for (id, name, base_url, auth_name) in [
        (
            "kimi-coding",
            "Kimi For Coding",
            "https://api.kimi.com/coding",
            "Kimi API key",
        ),
        (
            "minimax",
            "MiniMax",
            "https://api.minimax.io/anthropic",
            "MiniMax API key",
        ),
        (
            "minimax-cn",
            "MiniMax CN",
            "https://api.minimaxi.com/anthropic",
            "MiniMax CN API key",
        ),
    ] {
        let provider = providers
            .iter()
            .find(|provider| provider.id == id)
            .unwrap_or_else(|| panic!("missing provider {id}"));
        assert_eq!(provider.name, name);
        assert_eq!(provider.base_url.as_deref(), Some(base_url));
        assert_eq!(
            provider.auth.api_key.as_ref().map(|auth| auth.name()),
            Some(auth_name)
        );
        assert!(!provider.models.is_empty());
        assert!(provider
            .models
            .iter()
            .all(|model| model.api == "anthropic-messages"));
    }

    for provider_id in ["minimax", "minimax-cn"] {
        let model = providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .and_then(|provider| {
                provider
                    .models
                    .iter()
                    .find(|model| model.id == "MiniMax-M3")
            })
            .unwrap_or_else(|| panic!("missing {provider_id} MiniMax-M3"));
        assert_eq!(model.context_window, 1_048_576);
        assert_eq!(model.max_tokens, 512_000);
    }

    let kimi = providers
        .iter()
        .find(|provider| provider.id == "kimi-coding")
        .expect("Kimi Coding provider");
    for model_id in ["k3", "kimi-for-coding", "kimi-for-coding-highspeed"] {
        let model = kimi
            .models
            .iter()
            .find(|model| model.id == model_id)
            .unwrap_or_else(|| panic!("missing Kimi model {model_id}"));
        assert_eq!(
            model.compat.as_ref().and_then(|compat| compat
                .get("forceAdaptiveThinking")
                .and_then(serde_json::Value::as_bool)),
            Some(true)
        );
    }
}

#[test]
fn moonshot_and_nvidia_catalogs_match_pinned_provider_contract() {
    let providers = builtin_providers();
    let provider = |id: &str| {
        providers
            .iter()
            .find(|provider| provider.id == id)
            .unwrap_or_else(|| panic!("missing provider {id}"))
    };

    for (id, name, base_url, auth_name) in [
        (
            "moonshotai",
            "Moonshot AI",
            "https://api.moonshot.ai/v1",
            "Moonshot AI API key",
        ),
        (
            "moonshotai-cn",
            "Moonshot AI CN",
            "https://api.moonshot.cn/v1",
            "Moonshot AI API key",
        ),
        (
            "nvidia",
            "NVIDIA",
            "https://integrate.api.nvidia.com/v1",
            "NVIDIA API key",
        ),
    ] {
        let provider = provider(id);
        assert_eq!(provider.name, name);
        assert_eq!(provider.base_url.as_deref(), Some(base_url));
        assert_eq!(
            provider.auth.api_key.as_ref().map(|auth| auth.name()),
            Some(auth_name)
        );
        assert!(!provider.models.is_empty(), "{id} catalog is empty");
        assert!(provider.models.iter().all(|model| {
            model.provider == id && model.api == "openai-completions" && model.base_url == base_url
        }));
    }

    for provider_id in ["moonshotai", "moonshotai-cn"] {
        let provider = provider(provider_id);
        let kimi_k3 = provider
            .models
            .iter()
            .find(|model| model.id == "kimi-k3")
            .unwrap_or_else(|| panic!("missing {provider_id} kimi-k3"));
        assert!(kimi_k3.reasoning);
        assert_eq!(kimi_k3.context_window, 1_048_576);
        assert_eq!(kimi_k3.max_tokens, 131_072);
    }

    let nvidia = provider("nvidia");
    let mut nvidia_ids = nvidia
        .models
        .iter()
        .map(|model| model.id.as_str())
        .collect::<Vec<_>>();
    nvidia_ids.sort_unstable();
    assert_eq!(
        nvidia_ids,
        vec![
            "deepseek-ai/deepseek-v4-flash-0731",
            "google/gemma-3-12b-it",
            "google/gemma-3-4b-it",
            "meta/llama-3.2-11b-vision-instruct",
            "meta/llama-3.2-90b-vision-instruct",
            "meta/muse-glimmer-30b",
            "minimaxai/minimax-m3",
            "mistralai/mistral-7b-instruct-v0.3",
            "moonshotai/kimi-k2.6",
            "moonshotai/kimi-k3",
            "nvidia/cosmos-reason2-8b",
            "nvidia/llama-3.1-nemotron-70b-instruct",
            "nvidia/llama-3.1-nemotron-ultra-253b-v1",
            "nvidia/nemotron-3-nano-30b-a3b",
            "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning",
            "nvidia/nemotron-3-super-120b-a12b",
            "nvidia/nemotron-3-ultra-550b-a55b",
            "nvidia/nemotron-3.5-lightning-30b-a3b",
            "openai/gpt-oss-120b",
            "openai/gpt-oss-20b",
            "poolside/laguna-xs-2.1",
            "stepfun-ai/step-3.7-flash",
        ]
    );
    assert!(nvidia.models.iter().all(|model| {
        model
            .headers
            .as_ref()
            .and_then(|headers| headers.get("NVCF-POLL-SECONDS"))
            .map(String::as_str)
            == Some("3600")
    }));

    let deepseek = nvidia
        .models
        .iter()
        .find(|model| model.id == "deepseek-ai/deepseek-v4-flash-0731")
        .expect("NVIDIA DeepSeek V4 Flash catalog entry");
    assert!(deepseek.reasoning);
    assert_eq!(deepseek.context_window, 1_000_000);
    assert_eq!(deepseek.max_tokens, 384_000);
    assert_eq!(
        deepseek
            .compat
            .as_ref()
            .and_then(|compat| compat.get("requiresReasoningContentOnAssistantMessages"))
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        deepseek
            .compat
            .as_ref()
            .and_then(|compat| compat.get("thinkingFormat"))
            .and_then(serde_json::Value::as_str),
        Some("deepseek")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn custom_provider_refresh_merges_replacements_and_new_models() {
    let calls = Arc::new(Mutex::new(0_u32));
    let calls_for_fetch = calls.clone();
    let provider = create_provider_with_fetch_models(
        provider_options("custom", vec![model("custom", "same", "baseline")]),
        move |_context| {
            let calls = calls_for_fetch.clone();
            async move {
                *calls.lock().unwrap() += 1;
                Ok(vec![
                    model("wrong-provider", "same", "remote replacement"),
                    model("wrong-provider", "new", "remote addition"),
                ])
            }
        },
    );
    let store = Arc::new(InMemoryModelsStore::new());
    let models = create_models(CreateModelsOptions {
        models_store: Some(store.clone()),
        ..Default::default()
    });
    models.set_provider(provider.clone());

    let result = models.refresh(ModelsRefreshOptions::default()).await;
    assert!(!result.aborted);
    assert!(
        result.errors.is_empty(),
        "refresh errors: {:?}",
        result.errors
    );
    assert_eq!(*calls.lock().unwrap(), 1);

    let current = models.get_models(Some("custom"));
    assert_eq!(
        current
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        ["same", "new"]
    );
    assert_eq!(current[0].name, "remote replacement");
    assert_eq!(current[0].provider, "wrong-provider");
    assert_eq!(provider.get_models().len(), 2);
    assert_eq!(store.read("custom").unwrap().models.len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn cached_dynamic_models_restore_without_network() {
    let store = Arc::new(InMemoryModelsStore::new());
    let online = create_provider_with_fetch_models(
        provider_options("custom", Vec::new()),
        |_context| async { Ok(vec![model("custom", "cached", "cached")]) },
    );
    let online_models = create_models(CreateModelsOptions {
        models_store: Some(store.clone()),
        ..Default::default()
    });
    online_models.set_provider(online);
    let result = online_models.refresh(ModelsRefreshOptions::default()).await;
    assert!(result.errors.is_empty());

    let calls = Arc::new(Mutex::new(0_u32));
    let calls_for_fetch = calls.clone();
    let offline = create_provider_with_fetch_models(
        provider_options("custom", Vec::new()),
        move |_context| {
            let calls = calls_for_fetch.clone();
            async move {
                *calls.lock().unwrap() += 1;
                Err("network must not be used".to_string())
            }
        },
    );
    let offline_models = create_models(CreateModelsOptions {
        models_store: Some(store),
        ..Default::default()
    });
    offline_models.set_provider(offline);
    let result = offline_models
        .refresh(ModelsRefreshOptions {
            allow_network: false,
            ..Default::default()
        })
        .await;

    assert!(
        result.errors.is_empty(),
        "refresh errors: {:?}",
        result.errors
    );
    assert_eq!(*calls.lock().unwrap(), 0);
    assert_eq!(
        offline_models.get_model("custom", "cached").unwrap().name,
        "cached"
    );
}
