use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use pi_ai::model::Model;
use pi_ai::model_catalog::get_builtin_model_data_generated_at;
use pi_ai::models::{CreateModelsOptions, InMemoryModelsStore, ModelsStore, ModelsStoreEntry};
use pi_coding_agent::core::remote_catalog_provider::{
    merge_models, parse_catalog, refresh_catalogs_for_providers, remote_models,
    REMOTE_CATALOG_REFRESH_INTERVAL_MS,
};

fn model(provider: &str, id: &str, name: &str) -> Model {
    let mut model = Model::new(id, name, "openai-responses", provider);
    model.base_url = "https://example.test/v1".to_string();
    model.input = vec![pi_ai::model::ModelInput::Text];
    model
}

fn temp_agent_dir(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("pi-model-catalog-{label}-{}", uuid::Uuid::new_v4()))
}

struct EnvGuard {
    key: &'static str,
    previous: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self { key, previous }
    }

    fn remove(key: &'static str) -> Self {
        let previous = std::env::var(key).ok();
        std::env::remove_var(key);
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

fn environment_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

struct MockServer {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockServer {
    fn start(responses: Vec<(&str, &str, &str)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock catalog server");
        let address = listener.local_addr().expect("mock server address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_thread = requests.clone();
        let responses = responses
            .into_iter()
            .map(|(status, headers, body)| {
                (status.to_string(), headers.to_string(), body.to_string())
            })
            .collect::<Vec<_>>();
        let handle = thread::spawn(move || {
            for (status, headers, body) in responses {
                let (mut stream, _) = listener.accept().expect("accept catalog request");
                let mut bytes = [0_u8; 16 * 1024];
                let read = stream.read(&mut bytes).expect("read catalog request");
                requests_for_thread
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&bytes[..read]).into_owned());
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write catalog response");
            }
        });
        Self {
            base_url: format!("http://{address}"),
            requests,
            handle: Some(handle),
        }
    }

    fn finish(mut self) -> Vec<String> {
        self.handle
            .take()
            .expect("mock server handle")
            .join()
            .expect("mock server thread");
        Arc::try_unwrap(self.requests)
            .expect("mock server request ownership")
            .into_inner()
            .expect("mock server request lock")
    }
}

fn catalog_body() -> &'static str {
    r#"{"models":[{"id":"remote-demo","name":"Remote Demo","api":"openai-responses","provider":"wrong-provider","baseUrl":"https://demo.example.com/v1","reasoning":false,"input":["text"],"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":128000,"maxTokens":16384,"providerSpecific":{"tier":"gold"}}]}"#
}

fn store_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("models-store.json")
}

#[tokio::test(flavor = "current_thread")]
async fn http_refresh_parses_rfc_dates_and_preserves_unknown_model_fields() {
    let _guard = environment_lock().lock().await;
    let _offline = EnvGuard::remove("PI_OFFLINE");
    let server = MockServer::start(vec![(
        "200 OK",
        "ETag: \"catalog-1\"\r\nLast-Modified: Sunday, 06-Nov-94 08:49:37 GMT\r\n",
        catalog_body(),
    )]);
    let _endpoint = EnvGuard::set("PI_MODEL_CATALOG_URL", &server.base_url);
    let agent_dir = temp_agent_dir("success");
    let providers = vec!["custom/provider".to_string()];

    let refreshed = refresh_catalogs_for_providers(&agent_dir, true, &providers)
        .await
        .expect("catalog refresh");
    let requests = server.finish();
    assert_eq!(refreshed, 1);
    assert!(requests[0].contains("GET /api/models/providers/custom%2Fprovider HTTP/1.1"));
    assert!(requests[0].contains("user-agent: pi/"));

    let store = pi_coding_agent::core::models_store::FileModelsStore::new(store_path(&agent_dir));
    let entry = store.read("custom/provider").expect("stored catalog");
    assert_eq!(entry.models[0].provider, "custom/provider");
    assert_eq!(entry.etag.as_deref(), Some("\"catalog-1\""));
    assert_eq!(entry.last_modified, Some(784_111_777_000));
    let raw = std::fs::read_to_string(store_path(&agent_dir)).expect("raw catalog store");
    assert!(
        raw.contains("providerSpecific"),
        "unknown fields were dropped: {raw}"
    );
    assert!(
        raw.contains("gold"),
        "unknown field value was dropped: {raw}"
    );
    let _ = std::fs::remove_dir_all(agent_dir);
}

#[tokio::test(flavor = "current_thread")]
async fn etag_304_keeps_cached_body_and_refreshes_freshness() {
    let _guard = environment_lock().lock().await;
    let _offline = EnvGuard::remove("PI_OFFLINE");
    let first = MockServer::start(vec![("200 OK", "ETag: \"catalog-1\"\r\n", catalog_body())]);
    let _endpoint = EnvGuard::set("PI_MODEL_CATALOG_URL", &first.base_url);
    let agent_dir = temp_agent_dir("etag");
    refresh_catalogs_for_providers(&agent_dir, true, &["google".to_string()])
        .await
        .expect("initial catalog refresh");
    let _ = first.finish();
    let before = std::fs::read_to_string(store_path(&agent_dir)).expect("stored body");

    let second = MockServer::start(vec![("304 Not Modified", "ETag: \"catalog-1\"\r\n", "")]);
    let _endpoint = EnvGuard::set("PI_MODEL_CATALOG_URL", &second.base_url);
    refresh_catalogs_for_providers(&agent_dir, true, &["google".to_string()])
        .await
        .expect("304 refresh");
    let requests = second.finish();
    assert!(requests[0].contains("if-none-match: \"catalog-1\""));

    let after = std::fs::read_to_string(store_path(&agent_dir)).expect("updated body");
    assert!(after.contains("remote-demo"));
    assert!(after.contains("providerSpecific"));
    let before_checked_at: serde_json::Value = serde_json::from_str(&before).unwrap();
    let after_checked_at: serde_json::Value = serde_json::from_str(&after).unwrap();
    let before_checked_at_value = before_checked_at["google"]["checkedAt"]
        .as_i64()
        .expect("numeric prior checkedAt");
    let after_checked_at_value = after_checked_at["google"]["checkedAt"]
        .as_i64()
        .expect("numeric updated checkedAt");
    assert!(after_checked_at_value >= before_checked_at_value);
    assert_eq!(after_checked_at["google"]["etag"], "\"catalog-1\"");
    let _ = std::fs::remove_dir_all(agent_dir);
}

#[tokio::test(flavor = "current_thread")]
async fn not_found_and_not_implemented_keep_models_but_clear_validator() {
    let _guard = environment_lock().lock().await;
    let _offline = EnvGuard::remove("PI_OFFLINE");
    let first = MockServer::start(vec![("200 OK", "ETag: \"catalog-1\"\r\n", catalog_body())]);
    let _endpoint = EnvGuard::set("PI_MODEL_CATALOG_URL", &first.base_url);
    let agent_dir = temp_agent_dir("unavailable");
    refresh_catalogs_for_providers(&agent_dir, true, &["google".to_string()])
        .await
        .expect("initial catalog refresh");
    let _ = first.finish();

    let second = MockServer::start(vec![("404 Not Found", "", "missing")]);
    let _endpoint = EnvGuard::set("PI_MODEL_CATALOG_URL", &second.base_url);
    refresh_catalogs_for_providers(&agent_dir, true, &["google".to_string()])
        .await
        .expect("404 refresh");
    let _ = second.finish();

    let store = pi_coding_agent::core::models_store::FileModelsStore::new(store_path(&agent_dir));
    let entry = store.read("google").expect("catalog after 404");
    assert_eq!(entry.models[0].id, "remote-demo");
    assert_eq!(entry.last_modified, Some(0));
    assert!(entry.etag.is_none());
    let _ = std::fs::remove_dir_all(agent_dir);
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_payload_reports_upstream_catalog_error_without_overwriting_cache() {
    let _guard = environment_lock().lock().await;
    let _offline = EnvGuard::remove("PI_OFFLINE");
    let server = MockServer::start(vec![("200 OK", "", "null")]);
    let _endpoint = EnvGuard::set("PI_MODEL_CATALOG_URL", &server.base_url);
    let agent_dir = temp_agent_dir("malformed");
    let error = refresh_catalogs_for_providers(&agent_dir, true, &["google".to_string()])
        .await
        .expect_err("malformed catalog must fail");
    let _ = server.finish();
    assert!(
        error.contains("Invalid model catalog for provider \"google\""),
        "unexpected error: {error}"
    );
    assert!(!store_path(&agent_dir).exists());
    let _ = std::fs::remove_dir_all(agent_dir);
}

#[tokio::test(flavor = "current_thread")]
async fn refresh_is_offline_without_touching_the_network() {
    let _guard = environment_lock().lock().await;
    let _offline = EnvGuard::set("PI_OFFLINE", "1");
    let agent_dir = temp_agent_dir("offline");
    let error = refresh_catalogs_for_providers(&agent_dir, true, &["google".to_string()])
        .await
        .expect_err("offline refresh must fail");
    assert_eq!(
        error,
        "model catalog refresh is unavailable in offline mode"
    );
    assert!(!store_path(&agent_dir).exists());
    let _ = std::fs::remove_dir_all(agent_dir);
}

#[test]
fn runtime_overlay_honors_generated_at_and_merge_order() {
    let store = Arc::new(InMemoryModelsStore::new());
    let mut replacement = model("google", "gemini-2.5-flash", "Remote Flash");
    replacement.api = "google-generative-ai".to_string();
    store.write(
        "google",
        &ModelsStoreEntry {
            models: vec![replacement],
            last_modified: Some(get_builtin_model_data_generated_at().unwrap() + 1),
            checked_at: Some(1),
            etag: Some("etag".to_string()),
        },
    );
    let models = pi_ai::providers::builtin_models(CreateModelsOptions {
        models_store: Some(store),
        ..Default::default()
    });
    let merged = models
        .get_model("google", "gemini-2.5-flash")
        .expect("remote model");
    assert_eq!(merged.name, "Remote Flash");
    assert_eq!(merged.api, "google-generative-ai");
}

#[test]
fn older_remote_overlay_is_suppressed_but_merge_helpers_match_upstream() {
    let stored = ModelsStoreEntry {
        models: vec![model("google", "remote", "Remote")],
        last_modified: Some(100),
        checked_at: Some(200),
        etag: None,
    };
    assert!(remote_models(Some(&stored), Some(101)).is_empty());
    assert_eq!(remote_models(Some(&stored), Some(99)).len(), 1);
    assert!(remote_models(Some(&stored), None).len() == 1);
    assert!(std::hint::black_box(REMOTE_CATALOG_REFRESH_INTERVAL_MS) > 0);

    let merged = merge_models(
        &[model("google", "same", "Local")],
        &[
            model("google", "same", "Remote"),
            model("google", "new", "New"),
        ],
    );
    assert_eq!(
        merged
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        ["same", "new"]
    );
    assert_eq!(merged[0].name, "Remote");
}

#[test]
fn parse_catalog_matches_upstream_shape_and_exact_top_level_error() {
    let parsed = parse_catalog(
        "demo",
        &serde_json::json!({
            "dynamic": {
                "id": "dynamic",
                "name": "Dynamic",
                "api": "openai-responses",
                "baseUrl": "https://example.test/v1",
                "reasoning": false,
                "input": ["text"],
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
                "contextWindow": 1000,
                "maxTokens": 100
            }
        }),
    )
    .expect("keyed catalog");
    assert_eq!(parsed[0].provider, "demo");
    assert_eq!(
        parse_catalog("demo", &serde_json::json!(null)).unwrap_err(),
        "Invalid model catalog for provider \"demo\""
    );
}
