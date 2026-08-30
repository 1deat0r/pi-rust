#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Loopback-only recovery checks for the OpenAI Codex OAuth port.
//!
//! These fixtures prove request/callback/error semantics without contacting
//! OpenAI or treating a synthetic token as evidence of provider acceptance.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_ai::auth::{AuthEvent, AuthInteraction, AuthPrompt, OAuthAuth, OAuthCredential};
use pi_ai::oauth::{base64url_encode, generate_pkce, OpenAICodexOAuth};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Copy)]
enum FixtureMode {
    Success,
    Unauthorized,
    MalformedToken,
    DeviceCancellation,
}

struct AuthFixture {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl AuthFixture {
    async fn start(mode: FixtureMode) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let mut buffer = [0u8; 16 * 1024];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("")
                    .split('?')
                    .next()
                    .unwrap_or("")
                    .to_string();
                recorded.lock().unwrap().push(request);

                let (status, body, delay) = match (mode, path.as_str()) {
                    (FixtureMode::Success, "/oauth/token") => (
                        200,
                        json!({
                            "access_token": fake_jwt("fixture-account"),
                            "refresh_token": "new-refresh-token",
                            "expires_in": 3600
                        })
                        .to_string(),
                        None,
                    ),
                    (FixtureMode::Unauthorized, "/oauth/token") => (
                        401,
                        json!({
                            "error": {
                                "message": format!(
                                    "Could not validate refresh-secret; {} access-secret was rejected",
                                    "é".repeat(600)
                                ),
                                "code": "invalid_grant"
                            }
                        })
                        .to_string(),
                        None,
                    ),
                    (FixtureMode::MalformedToken, "/oauth/token") => (
                        200,
                        json!({"access_token": "malformed-access"}).to_string(),
                        None,
                    ),
                    (FixtureMode::DeviceCancellation, "/api/accounts/deviceauth/usercode") => (
                        200,
                        json!({
                            "device_auth_id": "fixture-device-auth",
                            "user_code": "ABCD-EFGH",
                            "interval": 0
                        })
                        .to_string(),
                        None,
                    ),
                    (FixtureMode::DeviceCancellation, "/api/accounts/deviceauth/token") => {
                        (200, String::new(), Some(Duration::from_secs(30)))
                    }
                    _ => (404, "{}".to_string(), None),
                };

                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                    continue;
                }
                let reason = match status {
                    200 => "OK",
                    401 => "Unauthorized",
                    _ => "Not Found",
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        Self {
            base_url,
            requests,
            task,
        }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for AuthFixture {
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

enum ManualBehavior {
    Immediate(String),
    WaitForAbort,
}

struct TestInteraction {
    method: &'static str,
    manual: ManualBehavior,
    events: Arc<Mutex<Vec<AuthEvent>>>,
    signal: Arc<AtomicBool>,
}

impl TestInteraction {
    fn new(method: &'static str, manual: ManualBehavior) -> Arc<Self> {
        Arc::new(Self {
            method,
            manual,
            events: Arc::new(Mutex::new(Vec::new())),
            signal: Arc::new(AtomicBool::new(false)),
        })
    }

    async fn auth_url(&self) -> String {
        for _ in 0..100 {
            if let Some(url) = self
                .events
                .lock()
                .unwrap()
                .iter()
                .find_map(|event| match event {
                    AuthEvent::AuthUrl { url, .. } => Some(url.clone()),
                    _ => None,
                })
            {
                return url;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("OAuth authorization URL was not emitted");
    }

    async fn saw_device_code(&self) -> bool {
        for _ in 0..100 {
            if self
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, AuthEvent::DeviceCode { .. }))
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        false
    }
}

impl AuthInteraction for TestInteraction {
    fn prompt(&self, prompt: &AuthPrompt) -> Result<String, String> {
        match prompt {
            AuthPrompt::Select { .. } => Ok(self.method.to_string()),
            _ => Err("unexpected synchronous OAuth prompt".to_string()),
        }
    }

    fn notify(&self, event: &AuthEvent) {
        self.events.lock().unwrap().push(event.clone());
    }

    fn signal(&self) -> Option<Arc<AtomicBool>> {
        Some(self.signal.clone())
    }

    fn prompt_async_with_abort<'a>(
        &'a self,
        _prompt: &'a AuthPrompt,
        abort: Arc<AtomicBool>,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        match &self.manual {
            ManualBehavior::Immediate(input) => {
                let input = input.clone();
                Box::pin(async move { Ok(input) })
            }
            ManualBehavior::WaitForAbort => Box::pin(async move {
                while !abort.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err("manual prompt cancelled".to_string())
            }),
        }
    }
}

async fn callback_status(url: String) -> reqwest::StatusCode {
    reqwest::get(url).await.unwrap().status()
}

#[test]
fn pkce_challenge_matches_the_s256_verifier() {
    let (verifier, challenge) = generate_pkce();
    let mut digest = Sha256::new();
    digest.update(verifier.as_bytes());
    assert_eq!(challenge, base64url_encode(&digest.finalize()));
    assert!((43..=128).contains(&verifier.len()));
    assert!(verifier
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn callback_rejects_wrong_state_then_accepts_a_valid_retry() {
    let fixture = AuthFixture::start(FixtureMode::Success).await;
    let interaction = TestInteraction::new("browser", ManualBehavior::WaitForAbort);
    let oauth = OpenAICodexOAuth::with_base_url_and_callback(&fixture.base_url, "127.0.0.1", 0);
    let login = {
        let oauth = oauth.clone();
        let interaction = interaction.clone();
        tokio::spawn(async move { oauth.login(interaction.as_ref()).await })
    };

    let authorize = url::Url::parse(&interaction.auth_url().await).unwrap();
    let state = authorize
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .unwrap();
    let redirect_uri = authorize
        .query_pairs()
        .find(|(key, _)| key == "redirect_uri")
        .map(|(_, value)| value.into_owned())
        .unwrap();

    let wrong = format!("{redirect_uri}?code=wrong-code&state=wrong-state");
    assert_eq!(
        callback_status(wrong).await,
        reqwest::StatusCode::BAD_REQUEST
    );
    let valid = format!("{redirect_uri}?code=browser-code&state={state}");
    assert_eq!(callback_status(valid).await, reqwest::StatusCode::OK);

    let credential = tokio::time::timeout(Duration::from_secs(2), login)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(credential.extra["accountId"], "fixture-account");
    assert!(fixture
        .requests()
        .iter()
        .any(|request| request.contains("code=browser-code")));
}

#[tokio::test]
async fn occupied_callback_port_falls_back_to_manual_code_exchange() {
    let fixture = AuthFixture::start(FixtureMode::Success).await;
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = occupied.local_addr().unwrap().port();
    let interaction = TestInteraction::new(
        "browser",
        ManualBehavior::Immediate("manual-code".to_string()),
    );
    let oauth = OpenAICodexOAuth::with_base_url_and_callback(&fixture.base_url, "127.0.0.1", port);

    let credential = oauth.login(interaction.as_ref()).await.unwrap();
    assert_eq!(credential.extra["accountId"], "fixture-account");
    assert!(fixture
        .requests()
        .iter()
        .any(|request| request.contains("code=manual-code")));
    drop(occupied);
}

#[tokio::test]
async fn unauthorized_refresh_reports_safe_detail_without_echoing_tokens() {
    let fixture = AuthFixture::start(FixtureMode::Unauthorized).await;
    let oauth = OpenAICodexOAuth::with_base_url_and_callback(&fixture.base_url, "127.0.0.1", 0);
    let credential = OAuthCredential {
        access: "access-secret".to_string(),
        refresh: "refresh-secret".to_string(),
        expires: 0,
        extra: Default::default(),
    };
    let error = oauth
        .refresh(&credential, &AtomicBool::new(false))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Could not validate"));
    assert!(!error.to_string().contains("refresh-secret"));
    assert!(!error.to_string().contains("access-secret"));
}

#[tokio::test]
async fn malformed_token_response_is_rejected_without_response_body_leakage() {
    let fixture = AuthFixture::start(FixtureMode::MalformedToken).await;
    let oauth = OpenAICodexOAuth::with_base_url_and_callback(&fixture.base_url, "127.0.0.1", 0);
    let credential = OAuthCredential {
        access: "old-access".to_string(),
        refresh: "refresh-secret".to_string(),
        expires: 0,
        extra: Default::default(),
    };
    let error = oauth
        .refresh(&credential, &AtomicBool::new(false))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("missing required fields"));
    assert!(!error.to_string().contains("malformed-access"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn device_flow_cancellation_interrupts_a_pending_network_poll() {
    let fixture = AuthFixture::start(FixtureMode::DeviceCancellation).await;
    let interaction = TestInteraction::new("device_code", ManualBehavior::WaitForAbort);
    let oauth = OpenAICodexOAuth::with_base_url_and_callback(&fixture.base_url, "127.0.0.1", 0);
    let login = {
        let oauth = oauth.clone();
        let interaction = interaction.clone();
        tokio::spawn(async move { oauth.login(interaction.as_ref()).await })
    };
    assert!(interaction.saw_device_code().await);
    interaction.signal.store(true, Ordering::SeqCst);
    let error = tokio::time::timeout(Duration::from_secs(2), login)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.to_string(), "Login cancelled");
    assert!(fixture
        .requests()
        .iter()
        .any(|request| request.contains("/api/accounts/deviceauth/token")));
}

#[tokio::test]
async fn network_failure_is_reported_without_refresh_token_echo() {
    let oauth = OpenAICodexOAuth::with_base_url_and_callback("http://127.0.0.1:1", "127.0.0.1", 0);
    let credential = OAuthCredential {
        access: "old-access".to_string(),
        refresh: "refresh-secret".to_string(),
        expires: 0,
        extra: Default::default(),
    };
    let error = oauth
        .refresh(&credential, &AtomicBool::new(false))
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("refresh-secret"));
    assert!(
        error.to_string().contains("request failed") || error.to_string().contains("timed out")
    );
}
