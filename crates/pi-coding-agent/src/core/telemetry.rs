//! Install/update telemetry gate — port of
//! `packages/coding-agent/src/core/telemetry.ts`.
//!
//! `PI_TELEMETRY` overrides the `enableInstallTelemetry` setting: set to a
//! truthy value (`1`/`true`/`yes`, case-insensitive) it enables; set to
//! anything else (`0`/`false`/`no`) it disables; unset it defers to the
//! setting. The gate controls anonymous install/update reporting to
//! `pi.dev` and the optional provider attribution headers.

use crate::core::settings::SettingsManager;

/// Upstream `isTruthyEnvFlag`: only `1`, `true`, or `yes` (case-insensitive)
/// count as enabled.
fn is_truthy_env_flag(value: &str) -> bool {
    value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
}

/// Upstream `isInstallTelemetryEnabled`. `telemetry_env` defaults to the
/// `PI_TELEMETRY` process env.
pub fn is_install_telemetry_enabled(
    settings: &SettingsManager,
    telemetry_env: Option<&str>,
) -> bool {
    match telemetry_env {
        Some(value) => is_truthy_env_flag(value),
        None => settings.get_enable_install_telemetry(),
    }
}

/// Convenience: resolve telemetry gating from the process environment.
pub fn is_install_telemetry_enabled_from_env(settings: &SettingsManager) -> bool {
    is_install_telemetry_enabled(settings, std::env::var("PI_TELEMETRY").ok().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settings(telemetry: bool) -> SettingsManager {
        SettingsManager::in_memory(
            serde_json::from_value(json!({
                "enableInstallTelemetry": telemetry
            }))
            .unwrap(),
        )
    }

    #[test]
    fn unset_env_defers_to_settings() {
        assert!(is_install_telemetry_enabled(&settings(true), None));
        assert!(!is_install_telemetry_enabled(&settings(false), None));
    }

    #[test]
    fn truthy_env_enables_despite_disabled_setting() {
        for v in ["1", "true", "TRUE", "Yes", "YES", "True"] {
            assert!(
                is_install_telemetry_enabled(&settings(false), Some(v)),
                "expected env {v:?} to force-enable"
            );
        }
    }

    #[test]
    fn falsy_env_disables_despite_enabled_setting() {
        for v in ["0", "false", "FALSE", "no", "off", "2", "nonsense"] {
            assert!(
                !is_install_telemetry_enabled(&settings(true), Some(v)),
                "expected env {v:?} to disable"
            );
        }
    }

    #[test]
    fn empty_env_is_treated_as_disabled_only_when_set() {
        // Empty is "set", so it disables (isTruthyEnvFlag("") == false).
        assert!(!is_install_telemetry_enabled(&settings(true), Some("")));
    }
}
