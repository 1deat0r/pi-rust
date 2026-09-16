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
fn copilot_gpt_models_route_through_responses_api() {
    // Regression pin for upstream #9253 (fixed #9209): GPT, Grok, OSWE,
    // and MAI-Code Copilot models are only served through the Copilot
    // /responses endpoint, so every `gpt-*` catalog entry must use the
    // openai-responses adapter.
    let providers = builtin_providers();
    let copilot = providers
        .iter()
        .find(|provider| provider.id == "github-copilot")
        .expect("GitHub Copilot provider");
    let gpt_models: Vec<_> = copilot
        .models
        .iter()
        .filter(|model| model.id.starts_with("gpt-"))
        .collect();
    assert!(
        !gpt_models.is_empty(),
        "expected at least one Copilot gpt-* catalog entry"
    );
    for model in &gpt_models {
        assert_eq!(
            model.api, "openai-responses",
            "Copilot {} must route through Responses",
            model.id
        );
    }
    copilot
        .models
        .iter()
        .find(|model| model.id == "gpt-6-astra")
        .expect("Copilot GPT-6 Astra catalog entry");
}

#[test]
fn deepseek_flash_catalog_uses_canonical_model_upstream_9423() {
    // Regression pin for upstream #9423 (12f59336a): retired Flash
    // aliases are replaced by canonical `deepseek-flash` (V4.1 Flash)
    // with refreshed pricing; V4 Pro pricing is refreshed too.
    let providers = builtin_providers();
    let deepseek = providers
        .iter()
        .find(|provider| provider.id == "deepseek")
        .expect("DeepSeek provider");
    assert!(
        deepseek
            .models
            .iter()
            .all(|model| model.api == "openai-completions"),
        "DeepSeek must use the OpenAI completions adapter"
    );
    for retired in ["deepseek-v4-flash", "deepseek-v4-flash-vision-exp"] {
        assert!(
            deepseek.models.iter().all(|model| model.id != retired),
            "retired DeepSeek alias {retired} must be gone"
        );
    }
    let flash = deepseek
        .models
        .iter()
        .find(|model| model.id == "deepseek-flash")
        .expect("canonical DeepSeek Flash catalog entry");
    assert_eq!(flash.name, "DeepSeek V4.1 Flash");
    assert_eq!(flash.cost.input, 0.3);
    assert_eq!(flash.cost.output, 1.2);
    assert_eq!(flash.cost.cache_read, 0.006);
    let pro = deepseek
        .models
        .iter()
        .find(|model| model.id == "deepseek-v4-pro")
        .expect("DeepSeek V4 Pro catalog entry");
    assert_eq!(pro.cost.input, 1.32);
    assert_eq!(pro.cost.output, 3.96);
    assert_eq!(pro.cost.cache_read, 0.044);
}

#[test]
fn retired_codex_models_are_absent_upstream_9394() {
    // Regression pin for upstream #9394 (2e6fe2f98): GPT-5.4 and
    // GPT-5.4 mini left the OpenAI Codex catalog (unavailable to
    // ChatGPT accounts). The `openai` provider entries are unaffected.
    let providers = builtin_providers();
    let codex = providers
        .iter()
        .find(|provider| provider.id == "openai-codex")
        .expect("OpenAI Codex provider");
    for retired in ["gpt-5.4", "gpt-5.4-mini"] {
        assert!(
            codex.models.iter().all(|model| model.id != retired),
            "retired Codex model {retired} must be gone"
        );
    }
    assert!(
        codex.models.iter().any(|model| model.id == "gpt-5.5"),
        "Codex GPT-5.5 successor must remain"
    );
}

#[test]
fn baseten_models_send_session_affinity_upstream_9629() {
    // Regression pin for upstream #9629 (6671c6047): Baseten automatic
    // prompt caching needs session affinity so related requests land on
    // the same replica.
    let providers = builtin_providers();
    let baseten = providers
        .iter()
        .find(|provider| provider.id == "baseten")
        .expect("Baseten provider");
    assert!(!baseten.models.is_empty());
    for model in &baseten.models {
        assert_eq!(
            model
                .compat
                .as_ref()
                .and_then(|compat| compat.get("sendSessionAffinityHeaders"))
                .and_then(serde_json::Value::as_bool),
            Some(true),
            "{} must send session affinity",
            model.id
        );
    }
    // Wire proof lives in-module
    // (`openrouter_session_affinity_header_is_opt_in_and_overridable`
    // covers the openai-format header set); the data contract here is
    // that every Baseten entry opts in.
}

#[test]
fn image_catalog_refresh_matches_upstream_drift() {
    // Regression pin for the post-pin image catalog refresh
    // (bdee230f1): Microsoft image models renamed to "Microsoft AI",
    // plus GPT Image 2.5 Flare/Sunburst additions.
    use pi_ai::images::catalog_images;
    let images = catalog_images("openrouter");
    assert!(!images.is_empty());
    for (id, name) in [
        ("microsoft/mai-image-2.5", "Microsoft AI: MAI-Image-2.5"),
        (
            "microsoft/mai-image-2.5-pro",
            "Microsoft AI: MAI-Image-2.5 Pro",
        ),
        ("microsoft/mai-image-2.6", "Microsoft AI: MAI-Image-2.6"),
        (
            "microsoft/mai-image-2.6-flash",
            "Microsoft AI: MAI-Image-2.6 Flash",
        ),
        ("openai/gpt-image-2.5-flare", "OpenAI: GPT Image 2.5 Flare"),
        (
            "openai/gpt-image-2.5-sunburst",
            "OpenAI: GPT Image 2.5 Sunburst",
        ),
    ] {
        assert_eq!(
            images.iter().find(|m| m.id == id).map(|m| m.name.as_str()),
            Some(name),
            "image model {id}"
        );
    }
}

#[test]
fn fireworks_thinking_metadata_matches_upstream_9323() {
    // Regression pin for upstream #9323 (6b94ae2ec): Fireworks
    // Messages models carry unsigned-thinking replay + session
    // affinity without eager streaming or tool cache-control; effort
    // advertising models (plus verified fallbacks) use adaptive
    // thinking; GLM-5.2 and Kimi-K3 aliases collapse to distinct
    // native levels.
    use pi_ai::model::get_supported_thinking_levels;

    let providers = builtin_providers();
    let fireworks = providers
        .iter()
        .find(|provider| provider.id == "fireworks")
        .expect("Fireworks provider");
    let find = |id: &str| {
        fireworks
            .models
            .iter()
            .find(|model| model.id == id)
            .unwrap_or_else(|| panic!("missing Fireworks model {id}"))
    };
    let compat_bool = |model: &Model, key: &str| {
        model
            .compat
            .as_ref()
            .and_then(|compat| compat.get(key))
            .and_then(serde_json::Value::as_bool)
    };

    // Every Messages-lane entry replays unsigned thinking signatures.
    for model in fireworks
        .models
        .iter()
        .filter(|m| m.api == "anthropic-messages")
    {
        assert_eq!(
            compat_bool(model, "allowEmptySignature"),
            Some(true),
            "{} must allow empty signatures",
            model.id
        );
        assert_eq!(
            compat_bool(model, "supportsToolReferences"),
            Some(true),
            "{} must support tool references",
            model.id
        );
    }

    // Effort-advertising models (and verified fallbacks) use adaptive.
    for id in [
        "accounts/fireworks/models/deepseek-v4-flash-0731",
        "accounts/fireworks/models/deepseek-v4-flash-vision-exp",
        "accounts/fireworks/models/deepseek-v4-pro-0813",
        "accounts/fireworks/models/gpt-oss-120b",
        "accounts/fireworks/models/minimax-m3",
        "accounts/fireworks/models/muse-glimmer-30b",
        "accounts/fireworks/models/qwen3p7-plus",
        "accounts/fireworks/models/qwen3p8-max",
        "accounts/fireworks/models/qwen3p8-2p4t-a95b",
    ] {
        assert_eq!(
            compat_bool(find(id), "forceAdaptiveThinking"),
            Some(true),
            "{id} must force adaptive thinking"
        );
    }

    // Toggle-only models without a verified fallback stay budget-based.
    for id in [
        "accounts/fireworks/models/kimi-k2p6",
        "accounts/fireworks/models/kimi-k2p7-code",
        "accounts/fireworks/models/nemotron-3-ultra-nvfp4",
        "accounts/fireworks/models/nemotron-lightning-3p5-30b-a3b",
        "accounts/fireworks/models/inkling",
    ] {
        assert_eq!(
            compat_bool(find(id), "forceAdaptiveThinking"),
            None,
            "{id} must not force adaptive thinking"
        );
    }

    // Verified fallback level maps.
    let levels = |id: &str| {
        get_supported_thinking_levels(find(id))
            .iter()
            .map(|level| level.as_str().to_string())
            .collect::<Vec<_>>()
    };
    for id in [
        "accounts/fireworks/models/deepseek-v4-flash-0731",
        "accounts/fireworks/models/deepseek-v4-flash-vision-exp",
        "accounts/fireworks/models/deepseek-v4-pro-0813",
    ] {
        assert_eq!(levels(id), ["off", "low", "high", "max"], "{id} levels");
    }
    for id in [
        "accounts/fireworks/models/qwen3p8-max",
        "accounts/fireworks/models/qwen3p8-2p4t-a95b",
    ] {
        assert_eq!(levels(id), ["off", "low", "medium", "xhigh"], "{id} levels");
    }

    // Alias collapse: only distinct native effort levels are exposed.
    for id in [
        "accounts/fireworks/models/glm-5p2",
        "accounts/fireworks/routers/glm-5p2-fast",
    ] {
        assert_eq!(levels(id), ["off", "high", "max"], "{id} levels");
    }
    for id in [
        "accounts/fireworks/models/kimi-k3",
        "accounts/fireworks/routers/kimi-k3-fast",
    ] {
        assert_eq!(levels(id), ["low", "high", "max"], "{id} levels");
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
