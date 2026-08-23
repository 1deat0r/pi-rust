//! Auth-guidance messages — port of `packages/coding-agent/src/core/auth-guidance.ts`.
//!
//! When a provider/auth failure occurs, upstream appends actionable guidance
//! (pointing at `/login` and the bundled providers.md / models.md docs). These
//! formatters shape that text 1:1.

use std::path::PathBuf;

const UNKNOWN_PROVIDER: &str = "unknown";

/// Base directory holding the bundled docs tree. Mirrors upstream `getDocsPath`
/// resolution: an explicit `PI_PACKAGE_DIR` wins, else the compiled binary's
/// directory (upstream `isBunBinary → dirname(process.execPath)`).
fn get_package_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("PI_PACKAGE_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn get_docs_path() -> PathBuf {
    get_package_dir().join("docs")
}

/// `getProviderLoginHelp` — points the user at `/login` and the docs.
pub fn get_provider_login_help() -> String {
    let docs = get_docs_path();
    format!(
        "Use /login to log into a provider via OAuth or API key. See:\n  {}\n  {}",
        docs.join("providers.md").display(),
        docs.join("models.md").display(),
    )
}

/// `formatNoModelsAvailableMessage`.
pub fn format_no_models_available_message() -> String {
    format!("No models available. {}", get_provider_login_help())
}

/// `formatNoModelSelectedMessage`.
pub fn format_no_model_selected_message() -> String {
    format!(
        "No model selected.\n\n{}\n\nThen use /model to select a model.",
        get_provider_login_help()
    )
}

/// `formatNoApiKeyFoundMessage(provider)` — names the provider (or "the
/// selected model" when unknown).
pub fn format_no_api_key_found_message(provider: &str) -> String {
    let display = if provider == UNKNOWN_PROVIDER {
        "the selected model".to_string()
    } else {
        provider.to_string()
    };
    format!(
        "No API key found for {display}.\n\n{}",
        get_provider_login_help()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_api_key_message_names_provider_or_selected_model() {
        let anthropic = format_no_api_key_found_message("anthropic");
        assert!(anthropic.starts_with("No API key found for anthropic."));
        assert!(anthropic.contains("Use /login"));
        assert!(anthropic.contains("providers.md"));

        let unknown = format_no_api_key_found_message(UNKNOWN_PROVIDER);
        assert!(unknown.starts_with("No API key found for the selected model."));
    }

    #[test]
    fn no_models_and_no_model_selected_messages_include_help() {
        assert!(format_no_models_available_message().starts_with("No models available. Use /login"));
        let selected = format_no_model_selected_message();
        assert!(selected.starts_with("No model selected."));
        assert!(selected.contains("/model to select a model"));
    }

    #[test]
    fn login_help_has_docs_paths_on_own_lines() {
        let help = get_provider_login_help();
        assert!(help.starts_with("Use /login"));
        let lines: Vec<&str> = help.lines().collect();
        assert!(lines[1].ends_with("/docs/providers.md"));
        assert!(lines[2].ends_with("/docs/models.md"));
    }
}
