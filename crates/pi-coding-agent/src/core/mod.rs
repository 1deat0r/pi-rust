//! Coding-agent core modules (port of `packages/coding-agent/src/core/`).

pub mod auth_storage;
pub mod diagnostics;
pub mod event_bus;
pub mod extensions;
pub mod export_html;
pub mod model_config;
pub mod model_runtime;
pub mod model_registry;
pub mod model_resolver;
pub mod models_store;
pub mod prompt_templates;
pub mod project_trust;
pub mod package_manager;
pub mod pi_manifest;
pub mod provider_attribution;
pub mod provider_composer;
pub mod remote_catalog_provider;
pub mod resolve_config_value;
pub mod session_migration;
pub mod settings;
pub mod skills;
pub mod slash_commands;
pub mod telemetry;
pub mod tools;
pub mod usage_totals;
