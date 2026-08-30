#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Focused authentication parity tests.
//!
//! These tests deliberately use loopback HTTP servers and synthetic tokens.
//! They do not contact, authenticate to, or persist credentials for any real
//! provider account.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pi_ai::auth::{
    classify_oauth_failure, env_api_key_auth, ApiKeyCredential, AuthCheck, AuthContext, AuthEvent,
    AuthInteraction, AuthPrompt, Credential, CredentialStore, InMemoryCredentialStore, ModelAuth,
    OAuthAuth, OAuthCredential, OAuthFailureKind, ProviderAuth,
};
use pi_ai::auth_flows::{poll_for_access_token, start_device_flow, DeviceCodeResponse};
use pi_ai::model::Model;
use pi_ai::models::{
    create_models, create_provider, AuthType, CreateModelsOptions, CreateProviderOptions,
    ModelsErrorCode, ProviderApiSpec, ProviderStreams,
};
use pi_ai::oauth::{base64url_encode, OpenAICodexOAuth};
use pi_ai::providers::qwen_token_plan_provider;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

struct PromptInteraction {
    answer: String,
}

impl AuthInteraction for PromptInteraction {
    fn prompt(&self, _prompt: &AuthPrompt) -> Result<String, String> {
        Ok(self.answer.clone())
    }

    fn notify(&self, _event: &AuthEvent) {}
}

fn test_context(values: BTreeMap<String, String>) -> AuthContext {
    let values = Arc::new(values);
    AuthContext {
        env: Arc::new(move |name| values.get(name).cloned()),
        file_exists: Arc::new(|_| false),
    }
}

#[test]
fn api_key_precedence_login_and_debug_redaction_match_contract() {
    let auth = env_api_key_auth("Example API key", vec!["FIRST_KEY", "SECOND_KEY"]);
    let ctx = test_context(BTreeMap::from([
        ("FIRST_KEY".to_string(), "   ".to_string()),
        ("SECOND_KEY".to_string(), "environment-secret".to_string()),
    ]));

    let empty_stored = ApiKeyCredential {
        key: Some("  ".to_string()),
        env: None,
    };
    assert_eq!(
        auth.check(&ctx, Some(&empty_stored)),
        Some(AuthCheck {
            source: Some("SECOND_KEY".to_string()),
            auth_type: "api_key",
        })
    );
    let resolved = auth.resolve(&ctx, Some(&empty_stored)).expect("env key");
    assert_eq!(resolved.auth.api_key.as_deref(), Some("environment-secret"));
    assert_eq!(resolved.source.as_deref(), Some("SECOND_KEY"));

    let stored = ApiKeyCredential {
        key: Some("stored-secret".to_string()),
        env: Some(BTreeMap::from([(
            "ACCOUNT_ID".to_string(),
            "account-secret".to_string(),
        )])),
    };
    let stored_result = auth.resolve(&ctx, Some(&stored)).expect("stored key");
    assert_eq!(stored_result.auth.api_key.as_deref(), Some("stored-secret"));
    assert_eq!(stored_result.source.as_deref(), Some("stored credential"));

    let prompted = auth
        .login(&PromptInteraction {
            answer: "prompt-secret".to_string(),
        })
        .expect("default API-key login");
    assert_eq!(prompted.key.as_deref(), Some("prompt-secret"));

    let debug = format!(
        "{:?} {:?} {:?} {:?}",
        stored,
        OAuthCredential {
            refresh: "refresh-secret".to_string(),
            access: "access-secret".to_string(),
            expires: 1,
            extra: BTreeMap::from([("accountId".to_string(), json!("account-secret"))]),
        },
        ModelAuth {
            api_key: Some("model-secret".to_string()),
            headers: Some(BTreeMap::from([(
                "Authorization".to_string(),
                Some("header-secret".to_string()),
            )])),
            base_url: None,
        },
        resolved,
    );
    for secret in [
        "stored-secret",
        "account-secret",
        "refresh-secret",
        "access-secret",
        "model-secret",
        "header-secret",
        "environment-secret",
    ] {
        assert!(
            !debug.contains(secret),
            "secret leaked through Debug: {secret}"
        );
    }
    assert!(debug.contains("<redacted>"));
}

#[test]
fn offline_oauth_error_markers_are_authoritative_and_fallbacks_are_ordered() {
    assert_eq!(
        classify_oauth_failure("OpenAI Codex OAuth login failed [protocol]: unauthorized input"),
        OAuthFailureKind::Protocol
    );
    assert_eq!(
        classify_oauth_failure("OAuth request failed: timed out"),
        OAuthFailureKind::Timeout
    );
    assert_eq!(
        classify_oauth_failure("OAuth server temporarily unavailable"),
        OAuthFailureKind::Server
    );
    assert_eq!(
        classify_oauth_failure("OAuth request failed (401)"),
        OAuthFailureKind::Unauthorized
    );
}

#[test]
fn credential_schema_preserves_oauth_extensions_and_store_mutations_are_atomic() {
    let raw = json!({
        "type": "oauth",
        "refresh": "refresh-secret",
        "access": "access-secret",
        "expires": 123,
        "accountId": "account-1",
        "availableModelIds": ["model-a"],
        "providerExtension": {"retained": true}
    });
    let credential: Credential = serde_json::from_value(raw.clone()).expect("OAuth schema");
    let Credential::OAuth(credential) = &credential else {
        panic!("expected OAuth credential");
    };
    assert_eq!(credential.extra["accountId"], json!("account-1"));
    assert_eq!(credential.extra["availableModelIds"], json!(["model-a"]));
    assert_eq!(credential.extra["providerExtension"]["retained"], true);
    let encoded = serde_json::to_value(credential).expect("OAuth serialization");
    assert_eq!(encoded["accountId"], "account-1");
    assert_eq!(encoded["providerExtension"]["retained"], true);

    let store = Arc::new(InMemoryCredentialStore::new());
    let workers = (0..8)
        .map(|_| {
            let store = store.clone();
            std::thread::spawn(move || {
                for _ in 0..25 {
                    store.modify("provider", &|current| {
                        let count = current
                            .and_then(|credential| match credential {
                                Credential::OAuth(credential) => credential
                                    .extra
                                    .get("count")
                                    .and_then(|value| value.as_u64()),
                                Credential::ApiKey(_) => None,
                            })
                            .unwrap_or(0)
                            + 1;
                        Some(Credential::OAuth(OAuthCredential {
                            refresh: "refresh".to_string(),
                            access: "access".to_string(),
                            expires: 1,
                            extra: BTreeMap::from([("count".to_string(), json!(count))]),
                        }))
                    });
                }
            })
        })
        .collect::<Vec<_>>();
    for worker in workers {
        worker.join().expect("store worker");
    }
    let final_credential = store.read("provider").expect("stored credential");
    let Credential::OAuth(final_credential) = final_credential else {
        panic!("expected OAuth credential");
    };
    assert_eq!(final_credential.extra["count"], json!(200));
    assert_eq!(store.list()[0].credential_type, "oauth");
}

struct Route {
    path: String,
    status: u16,
    body: String,
}

async fn start_route_server(routes: Vec<Route>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind route server");
    let base_url = format!("http://{}", listener.local_addr().expect("route address"));
    let task = tokio::spawn(async move {
        for route in routes {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = [0u8; 16 * 1024];
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("")
                .split('?')
                .next()
                .unwrap_or("");
            assert_eq!(path, route.path);
            let reason = match route.status {
                200 => "OK",
                400 => "Bad Request",
                404 => "Not Found",
                _ => "Test Response",
            };
            let response = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                route.status,
                reason,
                route.body.len(),
                route.body
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    (base_url, task)
}

#[tokio::test]
async fn device_flow_uses_real_loopback_http_and_parses_non_success_oauth_json() {
    let (base_url, server) = start_route_server(vec![
        Route {
            path: "/device".to_string(),
            status: 200,
            body: json!({
                "device_code": "device-1",
                "user_code": "USER-1",
                "verification_uri": "http://127.0.0.1:9/verify",
                "interval": 0,
                "expires_in": 30
            })
            .to_string(),
        },
        Route {
            path: "/token".to_string(),
            status: 400,
            body: json!({"error": "authorization_pending"}).to_string(),
        },
        Route {
            path: "/token".to_string(),
            status: 200,
            body: json!({"access_token": "loopback-access"}).to_string(),
        },
    ])
    .await;
    let client = reqwest::Client::new();
    let device = start_device_flow(
        &client,
        &format!("{base_url}/device"),
        &[("client_id", "local-client")],
        &[],
    )
    .await
    .expect("start device flow");
    assert_eq!(device.user_code, "USER-1");
    assert_eq!(device.verification_uri, "http://127.0.0.1:9/verify");
    let access = poll_for_access_token(
        &client,
        &format!("{base_url}/token"),
        &[("client_id", "local-client")],
        &[],
        &device,
        None,
    )
    .await
    .expect("poll access token");
    assert_eq!(access, "loopback-access");
    server.await.expect("route server");
}

#[tokio::test]
async fn device_poll_cancels_an_in_flight_loopback_response() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind cancellation server");
    let base_url = format!("http://{}", listener.local_addr().expect("server address"));
    let token_seen = Arc::new(Notify::new());
    let token_seen_server = token_seen.clone();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("token request");
        let mut buffer = [0u8; 8192];
        let _ = socket.read(&mut buffer).await;
        token_seen_server.notify_one();
        std::future::pending::<()>().await;
    });
    let client = reqwest::Client::new();
    let device = DeviceCodeResponse {
        device_code: "device-2".to_string(),
        user_code: "USER-2".to_string(),
        verification_uri: "http://127.0.0.1:9/verify".to_string(),
        interval: Some(0.0),
        expires_in: 30,
    };
    let signal = Arc::new(AtomicBool::new(false));
    let signal_for_poll = signal.clone();
    let poll = tokio::spawn(async move {
        poll_for_access_token(
            &client,
            &format!("{base_url}/token"),
            &[("client_id", "local-client")],
            &[],
            &device,
            Some(&signal_for_poll),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(3), token_seen.notified())
        .await
        .expect("token request reached server");
    signal.store(true, Ordering::SeqCst);
    let result = tokio::time::timeout(Duration::from_secs(2), poll)
        .await
        .expect("poll cancellation timeout")
        .expect("poll task");
    assert_eq!(result.unwrap_err(), "Login cancelled");
    server.abort();
}

fn fake_jwt(account_id: &str) -> String {
    let payload = json!({
        "https://api.openai.com/auth": {"chatgpt_account_id": account_id}
    });
    format!(
        "e30.{}.sig",
        base64url_encode(serde_json::to_string(&payload).expect("payload").as_bytes())
    )
}

#[tokio::test]
async fn openai_refresh_uses_real_loopback_http_and_extracts_account() {
    let access = fake_jwt("account-loopback");
    let (base_url, server) = start_route_server(vec![Route {
        path: "/oauth/token".to_string(),
        status: 200,
        body: json!({
            "access_token": access,
            "refresh_token": "refresh-loopback-2",
            "expires_in": 3600
        })
        .to_string(),
    }])
    .await;
    let oauth = OpenAICodexOAuth::with_base_url_and_callback(&base_url, "127.0.0.1", 0);
    let signal = AtomicBool::new(false);
    let credential = oauth
        .refresh(
            &OAuthCredential {
                refresh: "refresh-loopback-1".to_string(),
                access: "old-access".to_string(),
                expires: 0,
                extra: BTreeMap::new(),
            },
            &signal,
        )
        .await
        .expect("OpenAI refresh");
    assert_eq!(credential.refresh, "refresh-loopback-2");
    assert_eq!(credential.extra["accountId"], "account-loopback");
    server.await.expect("route server");
}

#[tokio::test]
async fn openai_refresh_redacts_error_response_and_cancels_before_request() {
    let (base_url, server) = start_route_server(vec![Route {
        path: "/oauth/token".to_string(),
        status: 400,
        body: json!({
            "error": "invalid_grant",
            "error_description": "refresh-loopback-secret"
        })
        .to_string(),
    }])
    .await;
    let oauth = OpenAICodexOAuth::with_base_url_and_callback(&base_url, "127.0.0.1", 0);
    let signal = AtomicBool::new(false);
    let error = oauth
        .refresh(
            &OAuthCredential {
                refresh: "refresh-loopback-secret".to_string(),
                access: "old-access".to_string(),
                expires: 0,
                extra: BTreeMap::new(),
            },
            &signal,
        )
        .await
        .expect_err("refresh must fail");
    assert!(!error.contains("refresh-loopback-secret"));
    server.await.expect("route server");

    let canceled = AtomicBool::new(true);
    let error = oauth
        .refresh(
            &OAuthCredential {
                refresh: "never-sent".to_string(),
                access: "old-access".to_string(),
                expires: 0,
                extra: BTreeMap::new(),
            },
            &canceled,
        )
        .await
        .expect_err("pre-canceled refresh");
    assert_eq!(error, "Login cancelled");
}

struct ModelsOAuth {
    refresh_calls: Arc<AtomicUsize>,
    fail_refresh: bool,
}

#[async_trait::async_trait]
impl OAuthAuth for ModelsOAuth {
    fn name(&self) -> &str {
        "Models OAuth fixture"
    }

    fn is_subscription(&self) -> bool {
        false
    }

    fn login_label(&self) -> Option<&str> {
        Some("fixture")
    }

    async fn login(&self, _interaction: &dyn AuthInteraction) -> Result<OAuthCredential, String> {
        Ok(OAuthCredential {
            refresh: "login-refresh".to_string(),
            access: "login-access".to_string(),
            expires: u64::MAX,
            extra: BTreeMap::new(),
        })
    }

    async fn refresh(
        &self,
        _credential: &OAuthCredential,
        signal: &AtomicBool,
    ) -> Result<OAuthCredential, String> {
        self.refresh_calls.fetch_add(1, Ordering::SeqCst);
        if signal.load(Ordering::SeqCst) {
            return Err("Login cancelled".to_string());
        }
        if self.fail_refresh {
            return Err("fixture refresh failed".to_string());
        }
        Ok(OAuthCredential {
            refresh: "rotated-refresh".to_string(),
            access: "rotated-access".to_string(),
            expires: u64::MAX,
            extra: BTreeMap::new(),
        })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> Option<ModelAuth> {
        Some(ModelAuth {
            api_key: Some(credential.access.clone()),
            ..Default::default()
        })
    }
}

fn oauth_test_provider(oauth: Arc<dyn OAuthAuth>) -> pi_ai::models::Provider {
    let stream = Arc::new(
        |_model: &Model,
         _context: &pi_ai::types::Context,
         _options: Option<&pi_ai::types::StreamOptions>| {
            pi_ai::create_error_stream("fixture", "oauth-test", "model", "unused".to_string())
        },
    );
    let stream_simple = Arc::new(
        |_model: &Model,
         _context: &pi_ai::types::Context,
         _options: Option<&pi_ai::types::SimpleStreamOptions>| {
            pi_ai::create_error_stream("fixture", "oauth-test", "model", "unused".to_string())
        },
    );
    create_provider(CreateProviderOptions {
        id: "oauth-test".to_string(),
        name: Some("OAuth test".to_string()),
        base_url: None,
        headers: None,
        auth: ProviderAuth {
            api_key: None,
            oauth: Some(oauth),
        },
        models: vec![Model::new("model", "Model", "fixture", "oauth-test")],
        api: ProviderApiSpec::Single(ProviderStreams {
            stream,
            stream_simple,
            fetch_deferred: None,
            cancel_deferred: None,
        }),
        filter_models: None,
    })
}

#[tokio::test]
async fn models_refreshes_expiring_oauth_before_auth_and_persists_rotation() {
    let store = Arc::new(InMemoryCredentialStore::new());
    let old = OAuthCredential {
        refresh: "old-refresh".to_string(),
        access: "old-access".to_string(),
        expires: 0,
        extra: BTreeMap::new(),
    };
    store.modify("oauth-test", &|_| Some(Credential::OAuth(old.clone())));
    let calls = Arc::new(AtomicUsize::new(0));
    let models = create_models(CreateModelsOptions {
        credentials: Some(store.clone()),
        ..Default::default()
    });
    models.set_provider(oauth_test_provider(Arc::new(ModelsOAuth {
        refresh_calls: calls.clone(),
        fail_refresh: false,
    })));

    let auth = models
        .get_auth_async("oauth-test", None, None)
        .await
        .expect("auth resolution")
        .expect("configured OAuth");
    assert_eq!(auth.auth.api_key.as_deref(), Some("rotated-access"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        store.read("oauth-test"),
        Some(Credential::OAuth(OAuthCredential {
            refresh: "rotated-refresh".to_string(),
            access: "rotated-access".to_string(),
            expires: u64::MAX,
            extra: BTreeMap::new(),
        }))
    );
}

#[tokio::test]
async fn models_preserves_oauth_credential_and_classifies_refresh_failure() {
    let store = Arc::new(InMemoryCredentialStore::new());
    let old = OAuthCredential {
        refresh: "old-refresh".to_string(),
        access: "old-access".to_string(),
        expires: 0,
        extra: BTreeMap::new(),
    };
    store.modify("oauth-test", &|_| Some(Credential::OAuth(old.clone())));
    let models = create_models(CreateModelsOptions {
        credentials: Some(store.clone()),
        ..Default::default()
    });
    models.set_provider(oauth_test_provider(Arc::new(ModelsOAuth {
        refresh_calls: Arc::new(AtomicUsize::new(0)),
        fail_refresh: true,
    })));

    let error = models
        .get_auth_async("oauth-test", None, None)
        .await
        .expect_err("refresh failure");
    assert_eq!(error.code, ModelsErrorCode::Oauth);
    assert!(error.message.contains("OAuth refresh failed"));
    assert_eq!(store.read("oauth-test"), Some(Credential::OAuth(old)));
}

#[test]
fn stored_unsupported_credential_does_not_fall_back_to_ambient_api_key() {
    let store = Arc::new(InMemoryCredentialStore::new());
    store.modify("owned", &|_| {
        Some(Credential::OAuth(OAuthCredential {
            refresh: "refresh".to_string(),
            access: "access".to_string(),
            expires: u64::MAX,
            extra: BTreeMap::new(),
        }))
    });
    let models = create_models(CreateModelsOptions {
        credentials: Some(store),
        auth_context: Some(test_context(BTreeMap::from([(
            "OWNED_KEY".to_string(),
            "ambient-secret".to_string(),
        )]))),
        ..Default::default()
    });
    let stream = Arc::new(
        |_model: &Model,
         _context: &pi_ai::types::Context,
         _options: Option<&pi_ai::types::StreamOptions>| {
            pi_ai::create_error_stream("fixture", "owned", "model", "unused".to_string())
        },
    );
    let stream_simple = Arc::new(
        |_model: &Model,
         _context: &pi_ai::types::Context,
         _options: Option<&pi_ai::types::SimpleStreamOptions>| {
            pi_ai::create_error_stream("fixture", "owned", "model", "unused".to_string())
        },
    );
    let provider = create_provider(CreateProviderOptions {
        id: "owned".to_string(),
        name: Some("Owned".to_string()),
        base_url: None,
        auth: ProviderAuth {
            api_key: Some(env_api_key_auth("Ambient", vec!["OWNED_KEY"])),
            oauth: None,
        },
        models: vec![Model::new("model", "Model", "fixture", "owned")],
        api: ProviderApiSpec::Single(ProviderStreams {
            stream,
            stream_simple,
            fetch_deferred: None,
            cancel_deferred: None,
        }),
        headers: None,
        filter_models: None,
    });
    models.set_provider(provider);
    assert!(models.check_auth("owned").is_none());
    assert!(models.get_auth("owned", None).is_none());
}

#[tokio::test]
async fn models_login_and_logout_update_the_shared_credential_store() {
    let store = Arc::new(InMemoryCredentialStore::new());
    let models = create_models(CreateModelsOptions {
        credentials: Some(store.clone()),
        ..Default::default()
    });
    models.set_provider(oauth_test_provider(Arc::new(ModelsOAuth {
        refresh_calls: Arc::new(AtomicUsize::new(0)),
        fail_refresh: false,
    })));

    let credential = models
        .login(
            "oauth-test",
            AuthType::OAuth,
            &PromptInteraction {
                answer: "unused".to_string(),
            },
        )
        .await
        .expect("OAuth login");
    assert!(matches!(credential, Credential::OAuth(_)));
    assert!(store.read("oauth-test").is_some());

    models.logout("oauth-test").await.expect("OAuth logout");
    assert!(store.read("oauth-test").is_none());
}

#[tokio::test]
async fn qwen_token_plan_api_key_login_persists_across_model_recreation() {
    let store = Arc::new(InMemoryCredentialStore::new());
    let models = create_models(CreateModelsOptions {
        credentials: Some(store.clone()),
        auth_context: Some(test_context(BTreeMap::new())),
        ..Default::default()
    });
    models.set_provider(qwen_token_plan_provider());

    let credential = models
        .login(
            "qwen-token-plan",
            AuthType::ApiKey,
            &PromptInteraction {
                answer: "fixture-qwen-token-plan-key".to_string(),
            },
        )
        .await
        .expect("Qwen API-key login");
    assert_eq!(
        credential,
        Credential::ApiKey(ApiKeyCredential {
            key: Some("fixture-qwen-token-plan-key".to_string()),
            env: None,
        })
    );
    assert_eq!(store.read("qwen-token-plan"), Some(credential));

    let restarted = create_models(CreateModelsOptions {
        credentials: Some(store.clone()),
        auth_context: Some(test_context(BTreeMap::new())),
        ..Default::default()
    });
    restarted.set_provider(qwen_token_plan_provider());
    let auth = restarted
        .get_auth("qwen-token-plan", None)
        .expect("persisted Qwen auth");
    assert_eq!(auth.source.as_deref(), Some("stored credential"));
    assert_eq!(
        auth.auth.api_key.as_deref(),
        Some("fixture-qwen-token-plan-key")
    );

    restarted
        .logout("qwen-token-plan")
        .await
        .expect("Qwen logout");
    assert!(store.read("qwen-token-plan").is_none());
}
