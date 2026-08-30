#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! End-to-end OpenAI Codex OAuth transport tests against loopback fixtures.
//! No test in this file contacts OpenAI or prints/stores a real credential.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_ai::auth::{AuthEvent, AuthInteraction, AuthPrompt, OAuthAuth, OAuthCredential};
use pi_ai::oauth::{base64url_encode, parse_openai_codex_authorization_input, OpenAICodexOAuth};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Copy)]
enum MockMode {
    Browser,
    Device,
    DeviceFailure,
    DeviceSecretFailure,
}

struct MockServer {
    base_url: String,
    device_polls: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl MockServer {
    async fn start(mode: MockMode) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let device_polls = Arc::new(AtomicUsize::new(0));
        let polls = device_polls.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let polls = polls.clone();
                tokio::spawn(async move {
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
                    let (status, body) = match (mode, path) {
                        (_, "/oauth/token") => (
                            200,
                            json!({
                                "access_token": fake_jwt("acct-from-fixture"),
                                "refresh_token": "fixture-refresh-2",
                                "expires_in": 3600
                            })
                            .to_string(),
                        ),
                        (MockMode::Browser, _) => (404, "{}".to_string()),
                        (MockMode::Device, "/api/accounts/deviceauth/usercode") => (
                            200,
                            json!({
                                "device_auth_id": "fixture-device-id",
                                "user_code": "ABCD-EFGH",
                                "interval": " 0 "
                            })
                            .to_string(),
                        ),
                        (MockMode::Device, "/api/accounts/deviceauth/token") => {
                            if polls.fetch_add(1, Ordering::SeqCst) == 0 {
                                (403, "{}".to_string())
                            } else {
                                (
                                    200,
                                    json!({
                                        "authorization_code": "fixture-device-code",
                                        "code_verifier": "fixture-device-verifier"
                                    })
                                    .to_string(),
                                )
                            }
                        }
                        (
                            MockMode::DeviceFailure | MockMode::DeviceSecretFailure,
                            "/api/accounts/deviceauth/usercode",
                        ) => (
                            200,
                            json!({
                                "device_auth_id": "fixture-device-id",
                                "user_code": "ABCD-EFGH",
                                "interval": 0
                            })
                            .to_string(),
                        ),
                        (MockMode::DeviceFailure, "/api/accounts/deviceauth/token") => (
                            500,
                            json!({
                                "error": "server_error",
                                "error_description": "try again later"
                            })
                            .to_string(),
                        ),
                        (MockMode::DeviceSecretFailure, "/api/accounts/deviceauth/token") => (
                            500,
                            json!({
                                "error": "server_error",
                                "detail": "fixture-device-id ABCD-EFGH"
                            })
                            .to_string(),
                        ),
                        (_, _) => (404, "{}".to_string()),
                    };
                    let reason = if status == 200 { "OK" } else { "Not Found" };
                    let response = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        Self {
            base_url,
            device_polls,
            task,
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn fake_jwt(account_id: &str) -> String {
    let payload = json!({
        "https://api.openai.com/auth": {"chatgpt_account_id": account_id}
    });
    format!(
        "e30.{}.sig",
        base64url_encode(serde_json::to_string(&payload).unwrap().as_bytes())
    )
}

#[derive(Default)]
struct BrowserInteraction {
    events: Mutex<Vec<AuthEvent>>,
}

impl AuthInteraction for BrowserInteraction {
    fn prompt(&self, prompt: &AuthPrompt) -> Result<String, String> {
        match prompt {
            AuthPrompt::Select { .. } => Ok("browser".to_string()),
            _ => Err("browser fixture should wait for callback".to_string()),
        }
    }

    fn notify(&self, event: &AuthEvent) {
        self.events.lock().unwrap().push(event.clone());
    }

    fn prompt_async_with_abort<'a>(
        &'a self,
        _prompt: &'a AuthPrompt,
        abort: Arc<std::sync::atomic::AtomicBool>,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            while !abort.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err("browser fixture prompt cancelled by callback".to_string())
        })
    }
}

#[derive(Default)]
struct DeviceInteraction {
    events: Mutex<Vec<AuthEvent>>,
}

impl AuthInteraction for DeviceInteraction {
    fn prompt(&self, prompt: &AuthPrompt) -> Result<String, String> {
        match prompt {
            AuthPrompt::Select { .. } => Ok("device_code".to_string()),
            _ => Err("device fixture has no cancellation prompt".to_string()),
        }
    }

    fn notify(&self, event: &AuthEvent) {
        self.events.lock().unwrap().push(event.clone());
    }
}

fn auth_url(interaction: &BrowserInteraction) -> String {
    interaction
        .events
        .lock()
        .unwrap()
        .iter()
        .find_map(|event| match event {
            AuthEvent::AuthUrl { url, .. } => Some(url.clone()),
            _ => None,
        })
        .expect("browser auth URL")
}

struct SelectorInteraction {
    method: String,
}

impl AuthInteraction for SelectorInteraction {
    fn prompt(&self, _prompt: &AuthPrompt) -> Result<String, String> {
        Ok(self.method.clone())
    }

    fn notify(&self, _event: &AuthEvent) {}
}

struct CancellingSelectorInteraction {
    signal: Arc<std::sync::atomic::AtomicBool>,
}

impl AuthInteraction for CancellingSelectorInteraction {
    fn prompt(&self, _prompt: &AuthPrompt) -> Result<String, String> {
        self.signal.store(true, Ordering::SeqCst);
        Ok("browser".to_string())
    }

    fn notify(&self, _event: &AuthEvent) {}

    fn signal(&self) -> Option<Arc<std::sync::atomic::AtomicBool>> {
        Some(self.signal.clone())
    }
}

#[test]
fn authorization_input_and_fake_jwt_contracts() {
    let parsed = parse_openai_codex_authorization_input(
        "http://localhost:1455/auth/callback?code=abc%2D123&state=state-1",
    );
    assert_eq!(parsed.code.as_deref(), Some("abc-123"));
    assert_eq!(parsed.state.as_deref(), Some("state-1"));
    assert_eq!(
        parse_openai_codex_authorization_input("raw-code#raw-state")
            .state
            .as_deref(),
        Some("raw-state")
    );
    assert_eq!(
        parse_openai_codex_authorization_input("raw-code")
            .code
            .as_deref(),
        Some("raw-code")
    );
    assert_eq!(fake_jwt("acct").split('.').count(), 3);
}

#[tokio::test]
async fn offline_unknown_codex_login_method_matches_upstream_error() {
    let oauth = OpenAICodexOAuth::with_base_url_and_callback("http://127.0.0.1:1", "127.0.0.1", 0);
    let error = oauth
        .login(&SelectorInteraction {
            method: "1".to_string(),
        })
        .await
        .expect_err("numeric aliases are not official login method ids");
    assert_eq!(error, "Unknown OpenAI Codex login method: 1");
}

#[tokio::test]
async fn cancelling_login_method_selection_never_starts_browser_or_device_flow() {
    let signal = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let oauth = OpenAICodexOAuth::with_base_url_and_callback("http://127.0.0.1:1", "127.0.0.1", 0);
    let error = oauth
        .login(&CancellingSelectorInteraction { signal })
        .await
        .expect_err("cancelled selection");
    assert_eq!(error, "Login cancelled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_callback_exchanges_code_and_extracts_account() {
    let mock = MockServer::start(MockMode::Browser).await;
    let callback_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let oauth =
        OpenAICodexOAuth::with_base_url_and_callback(&mock.base_url, "127.0.0.1", callback_port);
    let interaction = Arc::new(BrowserInteraction::default());
    let login_oauth = oauth.clone();
    let login_interaction = interaction.clone();
    let login = tokio::spawn(async move { login_oauth.login(login_interaction.as_ref()).await });

    for _ in 0..100 {
        if !interaction.events.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let url = auth_url(&interaction);
    let authorize = url::Url::parse(&url).unwrap();
    let state = authorize
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .unwrap();
    let callback = format!(
        "http://127.0.0.1:{callback_port}/auth/callback?code=fixture-browser-code&state={state}"
    );
    let response = reqwest::get(callback).await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let credential = login.await.unwrap().unwrap();
    assert_eq!(credential.access, fake_jwt("acct-from-fixture"));
    assert_eq!(credential.refresh, "fixture-refresh-2");
    assert_eq!(
        credential
            .extra
            .get("accountId")
            .and_then(|value| value.as_str()),
        Some("acct-from-fixture")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn device_code_flow_handles_pending_then_exchanges() {
    let mock = MockServer::start(MockMode::Device).await;
    let oauth = OpenAICodexOAuth::with_base_url_and_callback(&mock.base_url, "127.0.0.1", 0);
    let interaction = DeviceInteraction::default();
    let credential = oauth.login(&interaction).await.unwrap();
    assert_eq!(credential.access, fake_jwt("acct-from-fixture"));
    assert!(mock.device_polls.load(Ordering::SeqCst) >= 2);
    assert!(interaction
        .events
        .lock()
        .unwrap()
        .iter()
        .any(|event| matches!(
            event,
            AuthEvent::DeviceCode { user_code, .. } if user_code == "ABCD-EFGH"
        )));
}

#[tokio::test]
async fn device_auth_failure_preserves_upstream_status_and_body() {
    let mock = MockServer::start(MockMode::DeviceFailure).await;
    let oauth = OpenAICodexOAuth::with_base_url_and_callback(&mock.base_url, "127.0.0.1", 0);
    let error = oauth
        .login(&DeviceInteraction::default())
        .await
        .expect_err("device auth failure");
    assert_eq!(
        error,
        "OpenAI Codex device auth failed with status 500: {\"error\":\"server_error\",\"error_description\":\"try again later\"}"
    );
}

#[tokio::test]
async fn device_auth_failure_redacts_device_values_from_response_body() {
    let mock = MockServer::start(MockMode::DeviceSecretFailure).await;
    let oauth = OpenAICodexOAuth::with_base_url_and_callback(&mock.base_url, "127.0.0.1", 0);
    let error = oauth
        .login(&DeviceInteraction::default())
        .await
        .expect_err("device auth failure");
    assert!(!error.contains("fixture-device-id"));
    assert!(!error.contains("ABCD-EFGH"));
    assert!(error.contains("OpenAI Codex device auth failed with status 500"));
}

#[tokio::test]
async fn refresh_uses_refresh_grant_and_rejects_missing_account_claim() {
    let mock = MockServer::start(MockMode::Browser).await;
    let oauth = OpenAICodexOAuth::with_base_url_and_callback(&mock.base_url, "127.0.0.1", 0);
    let credential = OAuthCredential {
        access: fake_jwt("old-account"),
        refresh: "fixture-refresh-1".to_string(),
        expires: 0,
        extra: Default::default(),
    };
    let refreshed = oauth
        .refresh(&credential, &std::sync::atomic::AtomicBool::new(false))
        .await
        .unwrap();
    assert_eq!(refreshed.refresh, "fixture-refresh-2");
    assert_eq!(
        refreshed
            .extra
            .get("accountId")
            .and_then(|value| value.as_str()),
        Some("acct-from-fixture")
    );
}
