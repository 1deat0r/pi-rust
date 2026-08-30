//! Tool path helpers — port of `packages/agent/src/harness/tools/path-utils.ts`.

use unicode_normalization::UnicodeNormalization;

const UNICODE_SPACES: [char; 15] = [
    '\u{00A0}', '\u{2000}', '\u{2001}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}', '\u{2006}',
    '\u{2007}', '\u{2008}', '\u{2009}', '\u{200A}', '\u{202F}', '\u{205F}', '\u{3000}',
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
    // Keep the upstream order: the raw path wins, followed by the common
    // AM/PM typography variant, NFD, curly apostrophes, and NFD+curly. A
    // HashSet would make the first existing candidate nondeterministic.
    static AMPM_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        // Compile-time literal; a failure is a build defect.
        #[allow(clippy::panic)]
        regex::Regex::new(r" (?i:(AM|PM))\.")
            .unwrap_or_else(|error| panic!("static regex: {error}"))
    });
    let ampm = &*AMPM_RE;
    let nfd = resolved.nfd().collect::<String>();
    let candidates = [
        resolved.clone(),
        ampm.replace_all(&resolved, "\u{202F}$1.").into_owned(),
        nfd.clone(),
        resolved.replace('\'', "\u{2019}"),
        nfd.replace('\'', "\u{2019}"),
    ];
    let mut variants = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if !variants.iter().any(|variant| variant == &candidate) {
            variants.push(candidate);
        }
    }
    variants
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("pi-path-{tag}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn normalizes_every_upstream_unicode_space() {
        let input: String = UNICODE_SPACES.iter().copied().collect();
        assert_eq!(
            normalize_tool_path(&input),
            " ".repeat(UNICODE_SPACES.len())
        );
    }

    #[test]
    fn preserves_upstream_read_variant_order_and_deduplicates() {
        let variants = resolve_read_tool_path("/cwd", "café AM.txt");
        assert_eq!(
            variants,
            vec![
                "/cwd/café AM.txt",
                "/cwd/café\u{202F}AM.txt",
                "/cwd/cafe\u{301} AM.txt",
            ]
        );
    }

    #[test]
    fn existing_read_path_uses_nfd_variant() {
        let dir = temp_dir("nfd");
        let decomposed = dir.join("cafe\u{301}.txt");
        fs::write(&decomposed, "decomposed").unwrap();
        let resolved = resolve_read_tool_path_existing(&dir.display().to_string(), "café.txt");
        assert_eq!(std::path::PathBuf::from(resolved), decomposed);
        let _ = fs::remove_dir_all(dir);
    }
}
