use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use pi_ai::auth::{Credential, OAuthAuth, OAuthCredential};
use pi_ai::oauth::{
    github_copilot_base_url, github_copilot_urls, normalize_github_copilot_domain,
    GitHubCopilotOAuth,
};
use pi_ai::providers::all::github_copilot_provider;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone)]
struct MockResponse {
    status: u16,
    reason: &'static str,
    body: String,
}

impl MockResponse {
    fn json(body: serde_json::Value) -> Self {
        Self {
            status: 200,
            reason: "OK",
            body: body.to_string(),
        }
    }

    fn error(status: u16, reason: &'static str, body: &str) -> Self {
        Self {
            status,
            reason,
            body: body.to_string(),
        }
    }
}

async fn read_request(socket: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end;
    let content_length;
    loop {
        let count = socket.read(&mut chunk).await.unwrap();
        assert!(count > 0, "mock client closed before sending a request");
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            header_end = end + 4;
            let headers = String::from_utf8_lossy(&bytes[..end]);
            content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    (name.eq_ignore_ascii_case("content-length"))
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            break;
        }
    }
    while bytes.len() < header_end + content_length {
        let count = socket.read(&mut chunk).await.unwrap();
        assert!(
            count > 0,
            "mock client closed before sending the request body"
        );
        bytes.extend_from_slice(&chunk[..count]);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

async fn spawn_mock_server(
    responses: Vec<MockResponse>,
) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let task = tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            captured.lock().unwrap().push(request);
            let body = response.body;
            let response = format!(
                "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                response.status,
                response.reason,
                body.len(),
                body,
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    (format!("http://{address}"), requests, task)
}

fn refresh_credential(extra: BTreeMap<String, serde_json::Value>) -> OAuthCredential {
    OAuthCredential {
        access: "old-access".to_string(),
        refresh: "ghu-refresh".to_string(),
        expires: 0,
        extra,
    }
}

#[test]
fn enterprise_domain_and_proxy_endpoint_precedence_match_upstream() {
    assert_eq!(
        normalize_github_copilot_domain("  company.ghe.com/path  ").as_deref(),
        Some("company.ghe.com")
    );
    assert_eq!(
        normalize_github_copilot_domain("https://company.ghe.com").as_deref(),
        Some("company.ghe.com")
    );
    assert_eq!(normalize_github_copilot_domain("  "), None);
    assert_eq!(normalize_github_copilot_domain("not a url"), None);

    let urls = github_copilot_urls("company.ghe.com");
    assert_eq!(
        urls.device_code_url,
        "https://company.ghe.com/login/device/code"
    );
    assert_eq!(
        urls.access_token_url,
        "https://company.ghe.com/login/oauth/access_token"
    );
    assert_eq!(
        urls.copilot_token_url,
        "https://api.company.ghe.com/copilot_internal/v2/token"
    );

    let token = "tid=test;proxy-ep=proxy.enterprise.githubcopilot.com;";
    assert_eq!(
        github_copilot_base_url(Some(token), Some("company.ghe.com")),
        "https://api.enterprise.githubcopilot.com"
    );
    assert_eq!(
        github_copilot_base_url(Some("tid=test"), Some("company.ghe.com")),
        "https://copilot-api.company.ghe.com"
    );
    assert_eq!(
        github_copilot_base_url(None, None),
        "https://api.individual.githubcopilot.com"
    );
}

#[tokio::test]
async fn refresh_exchanges_the_refresh_token_then_fetches_models_once() {
    let (base_url, requests, server) = spawn_mock_server(vec![
        MockResponse::json(serde_json::json!({
            "token": "tid=new;exp=2000000000;proxy-ep=proxy.enterprise.githubcopilot.com;",
            "expires_at": 2_000_000_000u64,
        })),
        MockResponse::json(serde_json::json!({
            "data": [
                {"id": "gpt-4.1", "model_picker_enabled": true, "capabilities": {"supports": {"tool_calls": true}}},
                {"id": "tool-disabled", "model_picker_enabled": true, "capabilities": {"supports": {"tool_calls": false}}}
            ]
        })),
    ])
    .await;
    let oauth = GitHubCopilotOAuth::with_base_url(&base_url);
    let mut extra = BTreeMap::new();
    extra.insert(
        "enterpriseUrl".to_string(),
        serde_json::Value::String("https://company.ghe.com/path".to_string()),
    );

    let refreshed = oauth
        .refresh(&refresh_credential(extra), &AtomicBool::new(false))
        .await
        .unwrap();
    server.await.unwrap();

    assert_eq!(
        refreshed.access,
        "tid=new;exp=2000000000;proxy-ep=proxy.enterprise.githubcopilot.com;"
    );
    assert_eq!(refreshed.refresh, "ghu-refresh");
    assert_eq!(refreshed.expires, 1_999_999_700_000);
    assert_eq!(
        refreshed
            .extra
            .get("enterpriseUrl")
            .and_then(|value| value.as_str()),
        Some("company.ghe.com")
    );
    assert_eq!(
        refreshed
            .extra
            .get("availableModelIds")
            .and_then(|value| value.as_array())
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["gpt-4.1"]
    );

    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("GET /copilot_internal/v2/token HTTP/1.1"));
    assert!(requests[0]
        .to_ascii_lowercase()
        .contains("authorization: bearer ghu-refresh"));
    assert!(requests[0].contains("GitHubCopilotChat/0.35.0"));
    assert!(requests[1].starts_with("GET /models HTTP/1.1"));
    assert!(requests[1]
        .to_ascii_lowercase()
        .contains("authorization: bearer tid=new"));

    let auth = oauth.to_auth(&refreshed).unwrap();
    assert_eq!(auth.api_key.as_deref(), Some(refreshed.access.as_str()));
    assert_eq!(auth.base_url.as_deref(), Some(base_url.as_str()));
}

#[tokio::test]
async fn refresh_preserves_upstream_error_text_and_does_not_retry_model_throttling() {
    let (base_url, requests, server) = spawn_mock_server(vec![
        MockResponse::json(serde_json::json!({
            "token": "tid=new;proxy-ep=proxy.individual.githubcopilot.com;",
            "expires_at": 2_000_000_000u64,
        })),
        MockResponse::error(
            429,
            "Too Many Requests",
            "{\"error\":\"too many requests\"}",
        ),
    ])
    .await;
    let oauth = GitHubCopilotOAuth::with_base_url(&base_url);
    let error = oauth
        .refresh(
            &refresh_credential(BTreeMap::new()),
            &AtomicBool::new(false),
        )
        .await
        .unwrap_err();
    server.await.unwrap();

    assert!(error.starts_with("429 Too Many Requests: {\"error\":\"too many requests\"}"));
    assert_eq!(requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn invalid_token_response_fields_are_rejected_before_model_fetch() {
    let (base_url, requests, server) =
        spawn_mock_server(vec![MockResponse::json(serde_json::json!({
            "expires_at": 2_000_000_000u64,
        }))])
        .await;
    let oauth = GitHubCopilotOAuth::with_base_url(&base_url);
    let error = oauth
        .refresh(
            &refresh_credential(BTreeMap::new()),
            &AtomicBool::new(false),
        )
        .await
        .unwrap_err();
    server.await.unwrap();

    assert_eq!(error, "Invalid Copilot token response fields");
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[test]
fn provider_filters_oauth_models_only_when_the_catalog_metadata_is_valid() {
    let provider = github_copilot_provider();
    let filter = provider
        .filter_models
        .as_ref()
        .expect("Copilot has an OAuth model filter");
    let mut extra = BTreeMap::new();
    extra.insert(
        "availableModelIds".to_string(),
        serde_json::json!(["gpt-4.1"]),
    );
    let credential = Credential::OAuth(OAuthCredential {
        access: "access".to_string(),
        refresh: "refresh".to_string(),
        expires: 1,
        extra,
    });
    let filtered = filter(&provider.models, Some(&credential));
    assert_eq!(
        filtered
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        vec!["gpt-4.1"]
    );

    let invalid = Credential::OAuth(OAuthCredential {
        access: "access".to_string(),
        refresh: "refresh".to_string(),
        expires: 1,
        extra: BTreeMap::from([("availableModelIds".to_string(), serde_json::json!([1]))]),
    });
    assert_eq!(
        filter(&provider.models, Some(&invalid)).len(),
        provider.models.len()
    );
}
