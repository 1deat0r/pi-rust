//! Tool path helpers — port of `packages/agent/src/harness/tools/path-utils.ts`.

const UNICODE_SPACES: [char; 9] = [
    '\u{00A0}', '\u{2000}', '\u{2001}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}', '\u{2006}',
    '\u{3000}',
];

/// Normalizes tool paths: unicode spaces become regular spaces, a leading
/// `@` (used to disambiguate paths starting with `-`) is stripped.
pub fn normalize_tool_path(path: &str) -> String {
    let normalized: String = path
        .chars()
        .map(|c| if UNICODE_SPACES.contains(&c) { ' ' } else { c })
        .collect();
    match normalized.strip_prefix('@') {
        Some(rest) => rest.to_string(),
        None => normalized,
    }
}

/// Resolves a tool path against the working directory.
pub fn resolve_tool_path(cwd: &str, path: &str) -> String {
    let normalized = normalize_tool_path(path);
    let p = std::path::Path::new(&normalized);
    if p.is_absolute() {
        p.to_string_lossy().into_owned()
    } else {
        std::path::Path::new(cwd)
            .join(p)
            .to_string_lossy()
            .into_owned()
    }
}

/// Read-tool variant resolution: tries the raw path plus macOS/i18n variants
/// that upstream checks (narrow no-break space around AM/PM, NFD, curly
/// apostrophe).
pub fn resolve_read_tool_path(cwd: &str, path: &str) -> Vec<String> {
    let resolved = resolve_tool_path(cwd, path);
    let mut variants = vec![resolved.clone()];
    // AM/PM with regular space -> narrow no-break space
    let ampm = regex::Regex::new(r" (AM|PM)\.").expect("static regex");
    variants.push(ampm.replace_all(&resolved, " \u{202F}$1.").into_owned());
    // NFD normalization (macOS decomposed forms)
    variants.push(resolved.clone());
    // curly apostrophe variants
    variants.push(resolved.replace('\'', "\u{2019}"));
    variants
        .iter()
        .map(|v| v.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect()
}

/// Picks the first read path variant that exists.
pub fn resolve_read_tool_path_existing(cwd: &str, path: &str) -> String {
    let primary = resolve_tool_path(cwd, path);
    for variant in resolve_read_tool_path(cwd, path) {
        if std::path::Path::new(&variant).exists() {
            return variant;
        }
    }
    primary
}
