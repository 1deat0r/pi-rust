#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

use pi_ai::model::Model;
use pi_coding_agent::core::model_resolver::try_match_model;
use pi_coding_agent::core::version_check::{
    check_for_new_pi_version, compare_package_versions, get_latest_pi_release,
};

fn model(id: &str) -> Model {
    let mut model = Model::new(id, id, "openai-chat", "test-provider");
    model.base_url = "http://127.0.0.1:1/v1".to_string();
    model
}

#[test]
fn model_alias_detection_requires_a_date_separator() {
    let models = vec![model("widget12345678"), model("widget-20251231")];
    let matched = try_match_model("widget", &models).expect("a partial model match");
    assert_eq!(matched.id, "widget12345678");
}

#[test]
fn version_comparison_preserves_prerelease_ordering() {
    assert_eq!(
        compare_package_versions("0.84.2-beta.2", "0.84.2-beta.10"),
        Some(std::cmp::Ordering::Less)
    );
    assert!(pi_coding_agent::core::version_check::is_newer_package_version("0.85.0", "0.84.2"));
}

#[tokio::test]
async fn version_checks_honor_offline_and_skip_guards() {
    let old_offline = std::env::var_os("PI_OFFLINE");
    let old_skip = std::env::var_os("PI_SKIP_VERSION_CHECK");
    let old_endpoint = std::env::var_os("PI_VERSION_CHECK_URL");

    std::env::set_var("PI_OFFLINE", "1");
    std::env::set_var("PI_VERSION_CHECK_URL", "http://127.0.0.1:1/unreachable");
    assert!(get_latest_pi_release("0.84.2", false)
        .await
        .expect("offline checks are not errors")
        .is_none());

    std::env::remove_var("PI_OFFLINE");
    std::env::set_var("PI_SKIP_VERSION_CHECK", "1");
    assert!(check_for_new_pi_version("0.84.2").await.is_none());

    match old_offline {
        Some(value) => std::env::set_var("PI_OFFLINE", value),
        None => std::env::remove_var("PI_OFFLINE"),
    }
    match old_skip {
        Some(value) => std::env::set_var("PI_SKIP_VERSION_CHECK", value),
        None => std::env::remove_var("PI_SKIP_VERSION_CHECK"),
    }
    match old_endpoint {
        Some(value) => std::env::set_var("PI_VERSION_CHECK_URL", value),
        None => std::env::remove_var("PI_VERSION_CHECK_URL"),
    }
}
