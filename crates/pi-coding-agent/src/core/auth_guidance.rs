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

/// Match the auth failures emitted by the pi-ai facade and provider
/// adaptors. The coding-agent layer owns the actionable `/login` guidance,
/// so every mode can normalize these low-level messages at its output
/// boundary without changing provider wire errors or retry behavior.
pub fn is_auth_failure_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("no api key")
        || lower.contains("provider is not configured")
        || lower.contains("authheader requires")
        || lower.contains("authentication failed")
        || lower.contains("unauthorized")
        || lower.contains("status 401")
        || lower.contains("http 401")
        || lower.contains(" 401")
}

/// Format the upstream OAuth-specific guidance or the provider API-key
/// guidance for a low-level provider error. Already-normalized messages are
/// returned unchanged so retry/error envelopes cannot accumulate duplicate
/// documentation blocks.
pub fn format_provider_auth_failure(provider: &str, oauth_capable: bool, message: &str) -> String {
    if message.contains("Use /login")
        || (message.starts_with("Authentication failed for ") && message.contains("Run '/login "))
    {
        return message.to_string();
    }
    if !is_auth_failure_message(message) {
        return message.to_string();
    }
    if oauth_capable {
        format!(
            "Authentication failed for \"{provider}\". Credentials may have expired or network is unavailable. Run '/login {provider}' to re-authenticate."
        )
    } else {
        format_no_api_key_found_message(provider)
    }
}

/// Rewrite an assistant terminal error in place while preserving its model,
/// usage, and stop-reason fields.
pub fn rewrite_assistant_error(
    message: &mut pi_ai::types::AssistantMessage,
    provider: &str,
    oauth_capable: bool,
) {
    let Some(raw) = message.error_message() else {
        return;
    };
    let formatted = format_provider_auth_failure(provider, oauth_capable, raw);
    if formatted != raw {
        message.set_error_message(formatted);
    }
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

    #[test]
    fn provider_failures_get_one_actionable_auth_message() {
        let key =
            format_provider_auth_failure("google", false, "Provider is not configured: google");
        assert_eq!(key.matches("Use /login").count(), 1);
        assert!(key.starts_with("No API key found for google."));

        let oauth = format_provider_auth_failure(
            "openrouter",
            true,
            "Provider is not configured: openrouter",
        );
        assert!(oauth.contains("Authentication failed for \"openrouter\""));
        assert!(oauth.contains("Run '/login openrouter'"));

        let normal = format_provider_auth_failure("google", false, "network timeout");
        assert_eq!(normal, "network timeout");
    }
}
