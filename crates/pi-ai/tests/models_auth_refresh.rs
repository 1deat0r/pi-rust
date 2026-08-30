#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Deterministic Models OAuth refresh and stale-credential coverage.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pi_ai::auth::{
    AuthInteraction, Credential, CredentialStore, InMemoryCredentialStore, ModelAuth, OAuthAuth,
    OAuthCredential, ProviderAuth,
};
use pi_ai::event_stream::AssistantMessageEventStream;
use pi_ai::model::Model;
use pi_ai::models::{
    create_models, create_provider, CreateModelsOptions, CreateProviderOptions, Provider,
    ProviderApiSpec, ProviderStreams,
};
use pi_ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, DoneReason, StopReason,
    StreamOptions,
};
use tokio::sync::Notify;

#[derive(Clone)]
struct FixtureOAuth {
    refreshes: Arc<AtomicUsize>,
    refreshed_access: String,
    refreshed_expires: u64,
    refresh_error: Option<String>,
    refresh_started: Option<Arc<Notify>>,
    release_refresh: Option<Arc<Notify>>,
}

impl FixtureOAuth {
    fn new(refreshes: Arc<AtomicUsize>, refreshed_access: &str, refreshed_expires: u64) -> Self {
        Self {
            refreshes,
            refreshed_access: refreshed_access.to_string(),
            refreshed_expires,
            refresh_error: None,
            refresh_started: None,
            release_refresh: None,
        }
    }

    fn failing(mut self, error: &str) -> Self {
        self.refresh_error = Some(error.to_string());
        self
    }

    fn gated(mut self, started: Arc<Notify>, release: Arc<Notify>) -> Self {
        self.refresh_started = Some(started);
        self.release_refresh = Some(release);
        self
    }
}

#[async_trait::async_trait]
impl OAuthAuth for FixtureOAuth {
    fn name(&self) -> &str {
        "Models OAuth fixture"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login_label(&self) -> Option<&str> {
        None
    }

    async fn login(&self, _interaction: &dyn AuthInteraction) -> Result<OAuthCredential, String> {
        Ok(OAuthCredential {
            refresh: "fixture-refresh-new".to_string(),
            access: self.refreshed_access.clone(),
            expires: self.refreshed_expires,
            extra: BTreeMap::new(),
        })
    }

    async fn refresh(
        &self,
        credential: &OAuthCredential,
        _signal: &std::sync::atomic::AtomicBool,
    ) -> Result<OAuthCredential, String> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        if let Some(started) = &self.refresh_started {
            started.notify_one();
        }
        if let Some(release) = &self.release_refresh {
            release.notified().await;
        }
        if let Some(error) = &self.refresh_error {
            return Err(error.clone());
        }
        Ok(OAuthCredential {
            refresh: "fixture-refresh-new".to_string(),
            access: self.refreshed_access.clone(),
            expires: self.refreshed_expires,
            extra: credential.extra.clone(),
        })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> Option<ModelAuth> {
        Some(ModelAuth {
            api_key: Some(credential.access.clone()),
            ..Default::default()
        })
    }
}

fn credential(access: &str, refresh: &str, expires: u64) -> OAuthCredential {
    OAuthCredential {
        refresh: refresh.to_string(),
        access: access.to_string(),
        expires,
        extra: BTreeMap::new(),
    }
}

fn success_stream(model: &Model) -> AssistantMessageEventStream {
    let mut stream = AssistantMessageEventStream::new();
    let mut message = AssistantMessage::new();
    message.set_api_provider_model(&model.api, &model.provider, &model.id);
    message.content_mut().push(ContentBlock::text("ok"));
    message.set_stop_reason(StopReason::Stop);
    stream.push(AssistantMessageEvent::Done {
        reason: DoneReason::Stop,
        message,
    });
    stream
}

fn fixture_provider(
    oauth: Arc<dyn OAuthAuth>,
    requests: Arc<AtomicUsize>,
    seen_tokens: Arc<Mutex<Vec<String>>>,
) -> Provider {
    let stream_requests = requests.clone();
    let stream_tokens = seen_tokens.clone();
    let stream = Arc::new(
        move |model: &Model, _context: &Context, options: Option<&StreamOptions>| {
            let token = options
                .and_then(|options| options.base.api_key.clone())
                .unwrap_or_default();
            stream_tokens.lock().unwrap().push(token);
            if stream_requests.fetch_add(1, Ordering::SeqCst) == 0 {
                pi_ai::create_error_stream(
                    &model.api,
                    &model.provider,
                    &model.id,
                    "401 Unauthorized: bearer token expired".to_string(),
                )
            } else {
                success_stream(model)
            }
        },
    );
    let simple_stream = Arc::new(
        |_model: &Model,
         _context: &Context,
         _options: Option<&pi_ai::types::SimpleStreamOptions>| {
            pi_ai::create_error_stream(
                "fixture",
                "models-auth-refresh",
                "model",
                "simple fixture unused".to_string(),
            )
        },
    );
    create_provider(CreateProviderOptions {
        id: "models-auth-refresh".to_string(),
        name: Some("Models OAuth refresh fixture".to_string()),
        base_url: None,
        headers: None,
        auth: ProviderAuth {
            api_key: None,
            oauth: Some(oauth),
        },
        models: vec![Model::new(
            "model",
            "Models OAuth refresh fixture",
            "fixture",
            "models-auth-refresh",
        )],
        api: ProviderApiSpec::Single(ProviderStreams {
            stream,
            stream_simple: simple_stream,
            fetch_deferred: None,
            cancel_deferred: None,
        }),
        filter_models: None,
    })
}

fn setup(
    stored: OAuthCredential,
    oauth: Arc<dyn OAuthAuth>,
) -> (pi_ai::models::Models, Arc<InMemoryCredentialStore>, Model) {
    let store = Arc::new(InMemoryCredentialStore::new());
    store.modify("models-auth-refresh", &|_| {
        Some(Credential::OAuth(stored.clone()))
    });
    let models = create_models(CreateModelsOptions {
        credentials: Some(store.clone()),
        ..Default::default()
    });
    let requests = Arc::new(AtomicUsize::new(0));
    models.set_provider(fixture_provider(
        oauth,
        requests,
        Arc::new(Mutex::new(Vec::new())),
    ));
    let model = models
        .get_model("models-auth-refresh", "model")
        .expect("fixture model");
    (models, store, model)
}

#[tokio::test]
async fn fresh_oauth_credential_is_used_without_refresh() {
    let refreshes = Arc::new(AtomicUsize::new(0));
    let old = credential("old-access", "old-refresh", u64::MAX);
    let oauth = Arc::new(FixtureOAuth::new(refreshes.clone(), "new-access", u64::MAX));
    let (models, store, _model) = setup(old.clone(), oauth);

    let auth = models
        .get_auth_async("models-auth-refresh", None, None)
        .await
        .expect("auth resolution")
        .expect("configured OAuth");
    assert_eq!(auth.auth.api_key.as_deref(), Some("old-access"));
    assert_eq!(refreshes.load(Ordering::SeqCst), 0);
    assert_eq!(
        store.read("models-auth-refresh"),
        Some(Credential::OAuth(old))
    );
}

#[tokio::test]
async fn near_expiry_oauth_refreshes_before_auth_and_persists_rotation() {
    let refreshes = Arc::new(AtomicUsize::new(0));
    let oauth = Arc::new(FixtureOAuth::new(refreshes.clone(), "new-access", u64::MAX));
    let (models, store, _model) = setup(credential("old-access", "old-refresh", 1), oauth);

    let auth = models
        .get_auth_async("models-auth-refresh", None, None)
        .await
        .expect("auth resolution")
        .expect("configured OAuth");
    assert_eq!(auth.auth.api_key.as_deref(), Some("new-access"));
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    let Credential::OAuth(stored) = store
        .read("models-auth-refresh")
        .expect("rotated credential")
    else {
        panic!("expected OAuth credential");
    };
    assert_eq!(stored.access, "new-access");
    assert_eq!(stored.refresh, "fixture-refresh-new");
}

#[tokio::test]
async fn concurrent_auth_callers_refresh_once_and_share_the_fresh_credential() {
    let refreshes = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let oauth = Arc::new(
        FixtureOAuth::new(refreshes.clone(), "new-access", u64::MAX)
            .gated(started.clone(), release.clone()),
    );
    let (models, _store, _model) = setup(credential("old-access", "old-refresh", 1), oauth);

    let first_models = models.clone();
    let first = tokio::spawn(async move {
        first_models
            .get_auth_async("models-auth-refresh", None, None)
            .await
    });
    started.notified().await;
    let second_models = models.clone();
    let second = tokio::spawn(async move {
        second_models
            .get_auth_async("models-auth-refresh", None, None)
            .await
    });
    tokio::task::yield_now().await;
    release.notify_one();

    let first = first
        .await
        .expect("first task")
        .expect("first auth")
        .expect("first OAuth");
    let second = second
        .await
        .expect("second task")
        .expect("second auth")
        .expect("second OAuth");
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(first.auth.api_key, second.auth.api_key);
    assert_eq!(first.auth.api_key.as_deref(), Some("new-access"));
}

#[tokio::test]
async fn refresh_failure_is_actionable_redacted_and_preserves_old_credential() {
    let refreshes = Arc::new(AtomicUsize::new(0));
    let old = credential("old-access", "old-refresh", 1);
    let oauth = Arc::new(
        FixtureOAuth::new(refreshes.clone(), "new-access", u64::MAX)
            .failing("invalid_grant while exchanging old-access with old-refresh"),
    );
    let (models, store, _model) = setup(old.clone(), oauth);

    let error = models
        .get_auth_async("models-auth-refresh", None, None)
        .await
        .expect_err("refresh must fail");
    assert_eq!(error.code, pi_ai::models::ModelsErrorCode::Oauth);
    assert!(error
        .message
        .contains("OAuth refresh failed for models-auth-refresh"));
    assert!(error.message.contains("invalid_grant"));
    assert!(!error.message.contains("old-access"));
    assert!(!error.message.contains("old-refresh"));
    assert_eq!(
        store.read("models-auth-refresh"),
        Some(Credential::OAuth(old))
    );
}

#[tokio::test]
async fn stale_provider_failure_refreshes_once_and_retries_the_request() {
    let refreshes = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let old = credential("old-access", "old-refresh", u64::MAX);
    let oauth = Arc::new(FixtureOAuth::new(refreshes.clone(), "new-access", u64::MAX));
    let store = Arc::new(InMemoryCredentialStore::new());
    store.modify("models-auth-refresh", &|_| {
        Some(Credential::OAuth(old.clone()))
    });
    let models = create_models(CreateModelsOptions {
        credentials: Some(store.clone()),
        ..Default::default()
    });
    let sync_tokens = Arc::new(Mutex::new(Vec::new()));
    let provider = fixture_provider(oauth, requests.clone(), sync_tokens.clone());
    models.set_provider(provider);
    let model = models
        .get_model("models-auth-refresh", "model")
        .expect("fixture model");

    let message = models
        .stream(&model, &Context::default(), Some(&StreamOptions::default()))
        .for_each(|_| {})
        .await;
    assert_eq!(message.stop_reason(), Some(StopReason::Stop));
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(
        sync_tokens.lock().unwrap().as_slice(),
        ["old-access".to_string(), "new-access".to_string()]
    );
    let Credential::OAuth(stored) = store.read("models-auth-refresh").expect("fresh credential")
    else {
        panic!("expected OAuth credential");
    };
    assert_eq!(stored.access, "new-access");
}
