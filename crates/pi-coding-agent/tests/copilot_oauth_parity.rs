use std::fs;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use pi_ai::auth::{AuthInteraction, ModelAuth, OAuthAuth, OAuthCredential};
use pi_ai::providers::all::github_copilot_provider_with_oauth;
use pi_coding_agent::commands::auth::{check_provider_auth_with_options, get_provider_credential};
use pi_coding_agent::core::auth_storage::{
    refresh_oauth_credential_in_storage, AuthStorage, AuthStorageData, Credential, CredentialInfo,
};
use pi_coding_agent::core::model_registry::ModelRegistry;

struct TestOAuth {
    result: Result<OAuthCredential, String>,
}

impl OAuthAuth for TestOAuth {
    fn name(&self) -> &str {
        "Test Copilot OAuth"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login_label(&self) -> Option<&str> {
        None
    }

    fn login<'life0, 'life1, 'async_trait>(
        &'life0 self,
        _interaction: &'life1 dyn AuthInteraction,
    ) -> Pin<Box<dyn Future<Output = Result<OAuthCredential, String>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: Sync + 'async_trait,
    {
        Box::pin(async { Err("not used".to_string()) })
    }

    fn refresh<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        _credential: &'life1 OAuthCredential,
        _signal: &'life2 AtomicBool,
    ) -> Pin<Box<dyn Future<Output = Result<OAuthCredential, String>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: Sync + 'async_trait,
    {
        let result = self.result.clone();
        Box::pin(async move { result })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> Option<ModelAuth> {
        Some(ModelAuth {
            api_key: Some(credential.access.clone()),
            headers: None,
            base_url: None,
        })
    }
}

fn temp_auth_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "pi-copilot-oauth-parity-{name}-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn write_expired_credential(path: &std::path::Path, extra: serde_json::Value) {
    fs::write(
        path,
        serde_json::json!({
            "github-copilot": {
                "type": "oauth",
                "access": "old-access",
                "refresh": "ghu-refresh",
                "expires": 0,
                "enterpriseUrl": "https://company.ghe.com/path",
                "scope": extra,
            }
        })
        .to_string(),
    )
    .unwrap();
}

fn registry_with_oauth(oauth: Arc<dyn OAuthAuth>) -> ModelRegistry {
    let registry = pi_coding_agent::commands::auth::create_auth_check_model_registry();
    registry.register_provider(github_copilot_provider_with_oauth(oauth));
    registry
}

#[tokio::test]
async fn auth_storage_refresh_persists_the_rotated_copilot_credential() {
    let storage = AuthStorage::in_memory(AuthStorageData::from([(
        "github-copilot".to_string(),
        Credential::OAuth {
            access: "old-access".to_string(),
            refresh: "ghu-refresh".to_string(),
            expires: 0,
            extra: serde_json::from_value(serde_json::json!({
                "enterpriseUrl": "company.ghe.com",
                "scope": ["read:user"],
            }))
            .unwrap(),
        },
    )]));
    let oauth: Arc<dyn OAuthAuth> = Arc::new(TestOAuth {
        result: Ok(OAuthCredential {
            access: "fresh-access".to_string(),
            refresh: "rotated-refresh".to_string(),
            expires: u64::MAX,
            extra: serde_json::from_value(serde_json::json!({
                "enterpriseUrl": "company.ghe.com",
                "availableModelIds": ["gpt-4.1"],
            }))
            .unwrap(),
        }),
    });

    let refreshed =
        refresh_oauth_credential_in_storage(&storage, "github-copilot", oauth, None, None)
            .await
            .unwrap();

    // The real HTTP exchange is covered in pi-ai's mock fixture; this store
    // fixture keeps the persistence and extra-field contract deterministic.
    assert!(refreshed.is_some());
    let stored = storage
        .read(
            "github-copilot",
            &pi_coding_agent::core::auth_storage::AuthOperationOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(stored, refreshed);
}

#[tokio::test]
async fn no_refresh_returns_the_expired_access_token_without_mutating_storage() {
    let path = temp_auth_path("no-refresh");
    write_expired_credential(&path, serde_json::json!("fixture"));
    let oauth: Arc<dyn OAuthAuth> = Arc::new(TestOAuth {
        result: Err("network must not be used with --no-refresh".to_string()),
    });
    let registry = registry_with_oauth(oauth);

    let credential = get_provider_credential("github-copilot", &registry, &path, false, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        credential,
        Credential::OAuth {
            access: "old-access".to_string(),
            refresh: "ghu-refresh".to_string(),
            expires: 0,
            extra: serde_json::from_value(serde_json::json!({
                "enterpriseUrl": "https://company.ghe.com/path",
                "scope": "fixture",
            }))
            .unwrap(),
        }
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&fs::read_to_string(&path).unwrap()).unwrap()
            ["github-copilot"]["access"],
        "old-access"
    );
    let _ = fs::remove_file(path);
}

#[tokio::test]
async fn auth_check_refreshes_expired_oauth_by_default_and_reports_failures_without_overwrite() {
    let path = temp_auth_path("refresh-error");
    write_expired_credential(&path, serde_json::json!("fixture"));
    let oauth: Arc<dyn OAuthAuth> = Arc::new(TestOAuth {
        result: Err("token refresh failed (400): invalid_grant".to_string()),
    });
    let registry = registry_with_oauth(oauth);

    let no_refresh =
        check_provider_auth_with_options("github-copilot", &registry, &path, false).await;
    assert_eq!(no_refresh.status, "ready");
    assert_eq!(no_refresh.auth_type.as_deref(), Some("oauth"));

    // A live refresh failure is an invalid auth state, while the old
    // credential remains available for a subsequent re-login/retry.
    let refreshed =
        check_provider_auth_with_options("github-copilot", &registry, &path, true).await;
    assert_eq!(refreshed.status, "invalid");
    assert_eq!(refreshed.reason.as_deref(), Some("invalid_state"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&fs::read_to_string(&path).unwrap()).unwrap()
            ["github-copilot"]["access"],
        "old-access"
    );
    let _ = fs::remove_file(path);
}

#[test]
fn credential_metadata_is_not_secret_bearing() {
    let info = CredentialInfo {
        provider_id: "github-copilot".to_string(),
        credential_type: "oauth",
    };
    assert_eq!(info.provider_id, "github-copilot");
    assert_eq!(info.credential_type, "oauth");
}
