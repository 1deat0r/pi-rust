use std::sync::{Arc, Mutex};

use pi_ai::auth::ProviderAuth;
use pi_ai::event_stream::create_error_stream;
use pi_ai::model::Model;
use pi_ai::model_catalog::{merge_model_lists, remote_catalog_is_newer};
use pi_ai::models::{
    create_models, create_provider_with_fetch_models, CreateModelsOptions, CreateProviderOptions,
    InMemoryModelsStore, ModelsRefreshOptions, ModelsStore, ProviderApiSpec, ProviderStreams,
};
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
