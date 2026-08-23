//! Config-value resolution — port of
//! `packages/coding-agent/src/core/resolve-config-value.ts`.
//!
//! Values may be shell commands (`!cmd`, cached per process), environment
//! templates (`$VAR` / `${VAR}`; `$$`/`$!` escape to literals), or literals.
//! Used by auth-storage and (later) the model registry.

use std::collections::HashMap;
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

fn env_var_name_valid(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_alphabetic() || b == b'_' || (i > 0 && b.is_ascii_digit()))
}

#[derive(Debug, Clone, PartialEq)]
enum TemplatePart {
    Literal(String),
    Env(String),
}

#[derive(Debug, Clone, PartialEq)]
enum ConfigValueReference {
    Command(String),
    Template(Vec<TemplatePart>),
}

fn append_literal(parts: &mut Vec<TemplatePart>, value: &str) {
    if value.is_empty() {
        return;
    }
    if let Some(TemplatePart::Literal(prev)) = parts.last_mut() {
        prev.push_str(value);
        return;
    }
    parts.push(TemplatePart::Literal(value.to_string()));
}

fn parse_config_value_template(config: &str) -> Vec<TemplatePart> {
    let mut parts: Vec<TemplatePart> = Vec::new();
    let bytes = config.as_bytes();
    let mut index = 0usize;

    while index < bytes.len() {
        let dollar = config[index..].find('$').map(|i| index + i);
        let Some(dollar_index) = dollar else {
            append_literal(&mut parts, &config[index..]);
            break;
        };
        append_literal(&mut parts, &config[index..dollar_index]);
        let next_char = bytes.get(dollar_index + 1).copied();

        if matches!(next_char, Some(b'$') | Some(b'!')) {
            append_literal(&mut parts, &(next_char.unwrap() as char).to_string());
            index = dollar_index + 2;
            continue;
        }

        if next_char == Some(b'{') {
            let rest = &config[dollar_index + 2..];
            let Some(end_offset) = rest.find('}') else {
                append_literal(&mut parts, "$");
                index = dollar_index + 1;
                continue;
            };
            let end_index = dollar_index + 2 + end_offset;
            let name = &config[dollar_index + 2..end_index];
            if env_var_name_valid(name) {
                parts.push(TemplatePart::Env(name.to_string()));
            } else {
                append_literal(&mut parts, &config[dollar_index..=end_index]);
            }
            index = end_index + 1;
            continue;
        }

        // `$NAME` prefix match: [A-Za-z_][A-Za-z0-9_]*
        let after = &config[dollar_index + 1..];
        let match_len = {
            let b = after.as_bytes();
            let mut n = 0;
            if !b.is_empty() && (b[0].is_ascii_alphabetic() || b[0] == b'_') {
                n = 1;
                while n < b.len() && (b[n].is_ascii_alphanumeric() || b[n] == b'_') {
                    n += 1;
                }
            }
            n
        };
        if match_len > 0 {
            parts.push(TemplatePart::Env(after[..match_len].to_string()));
            index = dollar_index + 1 + match_len;
            continue;
        }

        append_literal(&mut parts, "$");
        index = dollar_index + 1;
    }

    parts
}

fn parse_config_value_reference(config: &str) -> ConfigValueReference {
    if config.starts_with('!') {
        return ConfigValueReference::Command(config.to_string());
    }
    ConfigValueReference::Template(parse_config_value_template(config))
}

fn resolve_env_config_value(name: &str, env: Option<&HashMap<String, String>>) -> Option<String> {
    env.and_then(|e| e.get(name))
        .cloned()
        .or_else(|| std::env::var(name).ok())
}

fn get_template_env_var_names(parts: &[TemplatePart]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for part in parts {
        if let TemplatePart::Env(name) = part {
            if !names.iter().any(|n| n == name) {
                names.push(name.clone());
            }
        }
    }
    names
}

fn resolve_template(
    parts: &[TemplatePart],
    env: Option<&HashMap<String, String>>,
) -> Option<String> {
    let mut resolved = String::new();
    for part in parts {
        match part {
            TemplatePart::Literal(value) => resolved.push_str(value),
            TemplatePart::Env(name) => {
                let env_value = resolve_env_config_value(name, env)?;
                resolved.push_str(&env_value);
            }
        }
    }
    Some(resolved)
}

pub fn get_config_value_env_var_name(config: &str) -> Option<String> {
    match parse_config_value_reference(config) {
        ConfigValueReference::Command(_) => None,
        ConfigValueReference::Template(parts) => {
            if parts.len() == 1 {
                if let TemplatePart::Env(name) = &parts[0] {
                    return Some(name.clone());
                }
            }
            None
        }
    }
}

pub fn get_config_value_env_var_names(config: &str) -> Vec<String> {
    match parse_config_value_reference(config) {
        ConfigValueReference::Command(_) => Vec::new(),
        ConfigValueReference::Template(parts) => get_template_env_var_names(&parts),
    }
}

pub fn get_missing_config_value_env_var_names(
    config: &str,
    env: Option<&HashMap<String, String>>,
) -> Vec<String> {
    get_config_value_env_var_names(config)
        .into_iter()
        .filter(|name| resolve_env_config_value(name, env).is_none())
        .collect()
}

pub fn is_command_config_value(config: &str) -> bool {
    matches!(
        parse_config_value_reference(config),
        ConfigValueReference::Command(_)
    )
}

pub fn is_config_value_configured(config: &str, env: Option<&HashMap<String, String>>) -> bool {
    get_missing_config_value_env_var_names(config, env).is_empty()
}

fn execute_with_default_shell(command: &str) -> Option<String> {
    // Upstream `execSync(command)` runs through the system default shell with
    // a 10s timeout. The port's shell-config surface (utils/shell.ts) is not
    // yet ported, so this mirrors the default `/bin/sh -c` path, polling
    // try_wait to enforce the timeout without a wait-timeout dependency.
    let Ok(mut child) = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return None;
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    };
    if !status.success() {
        return None;
    }
    let output = child.wait_with_output().ok()?;
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn execute_command_uncached(command_config: &str) -> Option<String> {
    execute_with_default_shell(&command_config[1..])
}

fn command_result_cache() -> &'static Mutex<HashMap<String, Option<String>>> {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<String, Option<String>>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolve a config value (API key, header value, etc.) to an actual value.
pub fn resolve_config_value(config: &str, env: Option<&HashMap<String, String>>) -> Option<String> {
    match parse_config_value_reference(config) {
        ConfigValueReference::Command(command_config) => {
            let cache = command_result_cache();
            if let Some(value) = cache.lock().unwrap().get(&command_config).cloned() {
                return value;
            }
            let result = execute_command_uncached(&command_config);
            cache
                .lock()
                .unwrap()
                .insert(command_config.clone(), result.clone());
            result
        }
        ConfigValueReference::Template(parts) => resolve_template(&parts, env),
    }
}

/// Clear the config value command cache. Exported for testing.
pub fn clear_config_value_cache() {
    command_result_cache().lock().unwrap().clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_plain_literal() {
        assert_eq!(
            resolve_config_value("sk-123", None).as_deref(),
            Some("sk-123")
        );
    }

    #[test]
    fn interpolates_env_template_variants() {
        std::env::set_var("PI_TEST_AUTH_KEY", "from-env");
        let plain = resolve_config_value("$PI_TEST_AUTH_KEY", None);
        assert_eq!(plain.as_deref(), Some("from-env"));
        let braced = resolve_config_value("${PI_TEST_AUTH_KEY}", None);
        assert_eq!(braced.as_deref(), Some("from-env"));
        // missing env var leaves the template unresolved
        assert_eq!(resolve_config_value("$PI_TEST_MISSING_KEY", None), None);
        // literal parts combine
        assert_eq!(
            resolve_config_value("prefix-$PI_TEST_AUTH_KEY", None).as_deref(),
            Some("prefix-from-env")
        );
    }

    #[test]
    fn escapes_double_dollar_and_dollar_bang() {
        assert_eq!(
            resolve_config_value("$$HOME", None).as_deref(),
            Some("$HOME")
        );
        assert_eq!(resolve_config_value("a$!b", None).as_deref(), Some("a!b"));
    }

    #[test]
    fn command_values_run_and_cache() {
        clear_config_value_cache();
        let value = resolve_config_value("!echo hello-world", None);
        assert_eq!(value.as_deref(), Some("hello-world"));
        // cached
        let value2 = resolve_config_value("!echo hello-world", None);
        assert_eq!(value2.as_deref(), Some("hello-world"));
    }

    #[test]
    fn classification_helpers() {
        assert!(is_command_config_value("!echo hi"));
        assert!(!is_command_config_value("plain"));
        assert_eq!(
            get_config_value_env_var_name("$FOO").as_deref(),
            Some("FOO")
        );
        assert_eq!(get_config_value_env_var_name("${FOO}x"), None);
        assert_eq!(get_config_value_env_var_names("$A-$B"), vec!["A", "B"]);
        std::env::set_var("PI_TEST_SET_A", "1");
        assert!(is_config_value_configured("$PI_TEST_SET_A", None));
        assert!(!is_config_value_configured("$PI_TEST_UNSET_B", None));
    }

    #[test]
    fn invalid_env_names_stay_literal() {
        assert_eq!(
            resolve_config_value("${1bad}", None).as_deref(),
            Some("${1bad}")
        );
    }
}
