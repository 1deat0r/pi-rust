//! Coding-agent core modules (port of `packages/coding-agent/src/core/`).

pub mod auth_guidance;
pub mod auth_storage;
pub mod cache_stats;
pub mod context_files;
pub mod diagnostics;
pub mod event_bus;
pub mod export_html;
pub mod extensions;
pub mod http_dispatcher;
pub mod model_config;
pub mod model_registry;
pub mod model_resolver;
pub mod model_runtime;
pub mod models_store;
pub mod package_manager;
pub mod pi_manifest;
pub mod project_trust;
pub mod prompt_templates;
pub mod provider_attribution;
pub mod provider_composer;
pub mod remote_catalog_provider;
pub mod resolve_config_value;
pub mod session_cwd;
pub mod session_migration;
pub mod settings;
pub mod skills;
pub mod slash_commands;
pub mod telemetry;
pub mod timings;
pub mod tools;
pub mod usage_totals;
pub mod version_check;

#[cfg(test)]
pub(crate) async fn environment_test_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}
