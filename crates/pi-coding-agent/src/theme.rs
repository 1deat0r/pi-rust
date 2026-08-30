//! Theme registry + color resolution — port of
//! `packages/coding-agent/src/modes/interactive/theme/theme.ts` (the
//! data/JSON resolution side used by HTML export and, later, the TUI).
//!
//! The ANSI painting layer lives in `interactive::tui_theme`; this module owns
//! the parsed JSON registry shared by HTML export, selectors, and the TUI.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use indexmap::IndexMap;

use serde::Deserialize;

use crate::config;
use crate::core::extensions::SourceInfo;

pub const DEFAULT_THEME: &str = "dark";
pub const LIGHT_THEME: &str = "light";

const BUILTIN_DARK: &str = include_str!("../data/themes/dark.json");
const BUILTIN_LIGHT: &str = include_str!("../data/themes/light.json");

const REQUIRED_COLOR_KEYS: &[&str] = &[
    "accent",
    "border",
    "borderAccent",
    "borderMuted",
    "success",
    "error",
    "warning",
    "muted",
    "dim",
    "text",
    "thinkingText",
    "selectedBg",
    "userMessageBg",
    "userMessageText",
    "customMessageBg",
    "customMessageText",
    "customMessageLabel",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
    "toolTitle",
    "toolOutput",
    "mdHeading",
    "mdLink",
    "mdLinkUrl",
    "mdCode",
    "mdCodeBlock",
    "mdCodeBlockBorder",
    "mdQuote",
    "mdQuoteBorder",
    "mdHr",
    "mdListBullet",
    "toolDiffAdded",
    "toolDiffRemoved",
    "toolDiffContext",
    "syntaxComment",
    "syntaxKeyword",
    "syntaxFunction",
    "syntaxVariable",
    "syntaxString",
    "syntaxNumber",
    "syntaxType",
    "syntaxOperator",
    "syntaxPunctuation",
    "thinkingOff",
    "thinkingMinimal",
    "thinkingLow",
    "thinkingMedium",
    "thinkingHigh",
    "thinkingXhigh",
    "bashMode",
];

/// A color value: hex string `#rrggbb`, empty string (default terminal
/// color), a variable reference (a key in `vars`), or a 256-color index.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum ColorValue {
    Str(String),
    Int(u8),
}

/// The subset of the theme JSON schema that export/color resolution reads.
/// Unknown fields are ignored (upstream validates with typebox; we keep the
/// same acceptance by parsing the shipped files and rejecting malformed
/// shapes only when a needed field is missing).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ThemeJson {
    #[serde(rename = "$schema")]
    pub schema: Option<String>,
    pub name: String,
    #[serde(default)]
    pub vars: IndexMap<String, ColorValue>,
    pub colors: IndexMap<String, ColorValue>,
    #[serde(default)]
    pub export: Option<ThemeExportSection>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ThemeExportSection {
    #[serde(default)]
    pub page_bg: Option<ColorValue>,
    #[serde(default)]
    pub card_bg: Option<ColorValue>,
    #[serde(default)]
    pub info_bg: Option<ColorValue>,
}

fn parse_theme_json(label: &str, content: &str) -> Result<ThemeJson, String> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let theme: ThemeJson =
        serde_json::from_str(content).map_err(|e| format!("Failed to parse theme {label}: {e}"))?;
    let mut missing_colors: Vec<_> = REQUIRED_COLOR_KEYS
        .iter()
        .filter(|key| !theme.colors.contains_key(**key))
        .copied()
        .collect();
    if !missing_colors.is_empty() {
        missing_colors.sort_unstable();
        let missing = missing_colors
            .iter()
            .map(|key| format!("  - {key}"))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!(
            "Invalid theme \"{label}\":\n\nMissing required color tokens:\n{missing}\n\nPlease add these colors to your theme's \"colors\" object.\nSee the built-in themes (dark.json, light.json) for reference values."
        ));
    }
    if theme.name.contains('/') {
        return Err(format!(
            "Invalid theme name \"{}\": theme names cannot contain \"/\" because it is reserved for automatic light/dark theme settings.",
            theme.name
        ));
    }
    Ok(theme)
}

/// Parsed theme identity exposed to resource consumers and selectors.
///
/// Builtin themes are embedded for runtime use but retain the shipped source
/// path for provenance and selector/resource metadata. Custom and registered
/// themes carry the resolved source path that produced them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeInfo {
    pub name: String,
    pub source_path: Option<PathBuf>,
    /// Owning extension metadata for themes discovered through an extension
    /// resource hook. Builtins, user settings, and custom themes have no
    /// extension owner and therefore keep this field empty.
    pub source_info: Option<SourceInfo>,
}

#[derive(Debug, Clone)]
struct RegisteredTheme {
    info: ThemeInfo,
    json: ThemeJson,
}

static REGISTERED_THEMES: OnceLock<RwLock<BTreeMap<String, RegisteredTheme>>> = OnceLock::new();

fn registered_theme_store() -> &'static RwLock<BTreeMap<String, RegisteredTheme>> {
    REGISTERED_THEMES.get_or_init(|| RwLock::new(BTreeMap::new()))
}

fn resolved_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn resolve_input_path(raw_path: &str, cwd: &Path) -> PathBuf {
    let expanded = config::expand_tilde_path(raw_path.trim());
    let path = Path::new(&expanded);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    resolved_path(&joined)
}

fn theme_files_in_path(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        return if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
            vec![resolved_path(path)]
        } else {
            Vec::new()
        };
    }
    if !path.is_dir() {
        return Vec::new();
    }

    let mut files: Vec<PathBuf> = std::fs::read_dir(path)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|entry| {
            entry.is_file() && entry.extension().and_then(|ext| ext.to_str()) == Some("json")
        })
        .map(|entry| resolved_path(&entry))
        .collect();
    files.sort();
    files
}

fn parse_theme_file(
    path: &Path,
    source_info: Option<SourceInfo>,
) -> Result<RegisteredTheme, String> {
    let source_path = resolved_path(path);
    let content = std::fs::read_to_string(&source_path)
        .map_err(|error| format!("Failed to read theme {}: {error}", source_path.display()))?;
    let json = parse_theme_json(&source_path.display().to_string(), &content)?;
    let name = json.name.clone();
    Ok(RegisteredTheme {
        info: ThemeInfo {
            name,
            source_path: Some(source_path),
            source_info,
        },
        json,
    })
}

fn registered_theme_infos_impl() -> Vec<ThemeInfo> {
    registered_theme_store()
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .values()
        .map(|theme| theme.info.clone())
        .collect()
}

/// Replace the extension theme registry with the valid themes found at
/// `paths`.
///
/// Paths are resolved relative to `cwd`; each path may be a `.json` file or a
/// directory whose immediate `.json` files are loaded. Invalid paths/themes
/// are ignored here, matching the resource loader's warning-and-continue
/// behavior. Duplicate files and duplicate parsed names are deduplicated with
/// the first path winning. Calling this during reload replaces stale entries;
/// it never accumulates them.
pub fn register_theme_paths(paths: &[String], cwd: &Path) -> Vec<ThemeInfo> {
    let sources = paths
        .iter()
        .map(|path| (path.clone(), None))
        .collect::<Vec<_>>();
    register_theme_sources(&sources, cwd)
}

/// Register settings/CLI themes together with extension-discovered themes,
/// retaining the owning extension's `SourceInfo` on every parsed theme.
pub fn register_theme_sources(
    paths: &[(String, Option<SourceInfo>)],
    cwd: &Path,
) -> Vec<ThemeInfo> {
    let mut seen_paths = HashSet::new();
    let mut seen_names = HashSet::new();
    let mut loaded = Vec::new();

    for (raw_path, source_info) in paths {
        for path in theme_files_in_path(&resolve_input_path(raw_path, cwd)) {
            if !seen_paths.insert(path.clone()) {
                continue;
            }
            let Ok(theme) = parse_theme_file(&path, source_info.clone()) else {
                continue;
            };
            if !seen_names.insert(theme.info.name.clone()) {
                continue;
            }
            loaded.push(theme);
        }
    }

    let mut registry = registered_theme_store()
        .write()
        .unwrap_or_else(|error| error.into_inner());
    registry.clear();
    for theme in loaded {
        registry.insert(theme.info.name.clone(), theme);
    }
    registry.values().map(|theme| theme.info.clone()).collect()
}

/// Reload one registered theme in place without discarding sibling extension
/// themes. This is the file-watcher equivalent of upstream's
/// `registeredThemes.set(...)`; startup/reload still uses `register_theme_paths`
/// to replace the complete discovered set.
pub fn reload_registered_theme_path(path: &Path) -> Result<ThemeInfo, String> {
    let source_info = registered_theme_store()
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .values()
        .find(|theme| theme.info.source_path.as_deref() == Some(path))
        .and_then(|theme| theme.info.source_info.clone());
    let theme = parse_theme_file(path, source_info)?;
    let info = theme.info.clone();
    registered_theme_store()
        .write()
        .unwrap_or_else(|error| error.into_inner())
        .insert(info.name.clone(), theme);
    Ok(info)
}

/// Return the currently registered extension themes, sorted by parsed name.
pub fn registered_theme_infos() -> Vec<ThemeInfo> {
    registered_theme_infos_impl()
}

/// Return all selector-visible themes, with duplicate parsed names removed.
/// Builtins win the listing position, followed by custom themes and then the
/// registered extension themes, matching upstream `getAvailableThemesWithPaths`.
pub fn available_themes_with_paths() -> Vec<ThemeInfo> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |info: ThemeInfo| {
        if seen.insert(info.name.clone()) {
            result.push(info);
        }
    };

    for name in builtin_themes().keys() {
        add(ThemeInfo {
            name: name.clone(),
            // Keep the same discoverable source path as upstream's shipped
            // theme files, even though the Rust build embeds their contents.
            source_path: Some(builtin_theme_path(name)),
            source_info: None,
        });
    }
    for info in custom_theme_infos() {
        add(info);
    }
    for info in registered_theme_infos_impl() {
        add(info);
    }

    result.sort_by(|left, right| left.name.cmp(&right.name));
    result
}

fn builtin_theme_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("data")
        .join("themes")
        .join(format!("{name}.json"))
}

/// Return selector-visible theme names in upstream sort order.
pub fn available_theme_names() -> Vec<String> {
    available_themes_with_paths()
        .into_iter()
        .map(|theme| theme.name)
        .collect()
}

fn custom_theme_infos() -> Vec<ThemeInfo> {
    theme_files_in_path(&custom_themes_dir())
        .into_iter()
        .filter_map(|path| parse_theme_file(&path, None).ok().map(|theme| theme.info))
        .collect()
}

fn registered_theme(name: &str) -> Option<RegisteredTheme> {
    registered_theme_store()
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .get(name)
        .cloned()
}

#[cfg(test)]
pub(crate) fn test_theme_registry_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// Built-in theme registry (embedded copies of the shipped dark/light JSON).
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
pub fn builtin_themes() -> BTreeMap<String, ThemeJson> {
    let mut map = BTreeMap::new();
    map.insert(
        "dark".to_string(),
        parse_theme_json("dark", BUILTIN_DARK).expect("embedded dark theme parses"),
    );
    map.insert(
        "light".to_string(),
        parse_theme_json("light", BUILTIN_LIGHT).expect("embedded light theme parses"),
    );
    map
}

/// `getCustomThemesDir()` — `~/.pi/agent/themes` (or `PI_CODING_AGENT_DIR`).
pub fn custom_themes_dir() -> PathBuf {
    config::get_agent_dir().join("themes")
}

/// Load a theme JSON by name: registered extension themes first, then builtin
/// and custom themes. Registered themes take activation/resolution precedence
/// over a custom or builtin theme with the same parsed name.
pub fn load_theme_json(name: &str) -> Result<ThemeJson, String> {
    if let Some(theme) = registered_theme(name) {
        return Ok(theme.json);
    }
    let builtins = builtin_themes();
    if let Some(theme) = builtins.get(name) {
        return Ok(theme.clone());
    }
    let theme_path = custom_themes_dir().join(format!("{name}.json"));
    if !theme_path.exists() {
        return Err(format!("Theme not found: {name}"));
    }
    let content = std::fs::read_to_string(&theme_path)
        .map_err(|e| format!("Failed to read theme {name}: {e}"))?;
    parse_theme_json(name, &content)
}

/// Resolve a color value through variable references (with cycle detection).
pub fn resolve_var_refs(
    value: &ColorValue,
    vars: &IndexMap<String, ColorValue>,
    visited: &mut Vec<String>,
) -> Result<ColorValue, String> {
    match value {
        ColorValue::Int(_) => Ok(value.clone()),
        ColorValue::Str(s) if s.is_empty() || s.starts_with('#') => Ok(value.clone()),
        ColorValue::Str(name) => {
            if visited.iter().any(|v| v == name) {
                return Err(format!("Circular variable reference detected: {name}"));
            }
            let next = vars
                .get(name)
                .ok_or_else(|| format!("Variable reference not found: {name}"))?;
            visited.push(name.clone());
            resolve_var_refs(next, vars, visited)
        }
    }
}

/// Resolve every color in a colors map through `vars`.
pub fn resolve_theme_colors(
    colors: &IndexMap<String, ColorValue>,
    vars: &IndexMap<String, ColorValue>,
) -> Result<IndexMap<String, ColorValue>, String> {
    let mut resolved = IndexMap::new();
    for (key, value) in colors {
        resolved.insert(key.clone(), resolve_var_refs(value, vars, &mut Vec::new())?);
    }
    Ok(resolved)
}

/// Upstream `withThemeColorFallbacks` — fill optional colors with their
/// canonical fallbacks.
pub fn with_theme_color_fallbacks(
    colors: &IndexMap<String, ColorValue>,
) -> IndexMap<String, ColorValue> {
    let mut out = colors.clone();
    if !out.contains_key("thinkingMax") {
        if let Some(xhigh) = out.get("thinkingXhigh") {
            out.insert("thinkingMax".to_string(), xhigh.clone());
        }
    }
    if !out.contains_key("scrollbarThumb") {
        if let Some(selected) = out.get("selectedBg") {
            out.insert("scrollbarThumb".to_string(), selected.clone());
        }
    }
    if !out.contains_key("searchMatchBg") {
        if let Some(selected) = out.get("selectedBg") {
            out.insert("searchMatchBg".to_string(), selected.clone());
        }
    }
    if !out.contains_key("searchMatchText") {
        if let Some(text) = out.get("text") {
            out.insert("searchMatchText".to_string(), text.clone());
        }
    }
    out
}

/// Standard ANSI palette (0-15) — same table as upstream.
fn ansi_basic(index: u8) -> &'static str {
    const BASIC: [&str; 16] = [
        "#000000", "#800000", "#008000", "#808000", "#000080", "#800080", "#008080", "#c0c0c0",
        "#808080", "#ff0000", "#00ff00", "#ffff00", "#0000ff", "#ff00ff", "#00ffff", "#ffffff",
    ];
    BASIC[index as usize]
}

/// Convert a 256-color index to a hex string (upstream `ansi256ToHex`).
pub fn ansi256_to_hex(index: u8) -> String {
    let i = index as u16;
    if i < 16 {
        return ansi_basic(index).to_string();
    }
    if i < 232 {
        let cube = i - 16;
        let r = cube / 36;
        let g = (cube % 36) / 6;
        let b = cube % 6;
        let comp = |n: u16| if n == 0 { 0 } else { 55 + n * 40 };
        format!("#{:02x}{:02x}{:02x}", comp(r), comp(g), comp(b))
    } else {
        let gray = 8 + (i - 232) * 10;
        format!("#{gray:02x}{gray:02x}{gray:02x}")
    }
}

/// Convert a hex string to RGB (upstream `hexToRgb`).
pub fn hex_to_rgb(hex: &str) -> Result<(u8, u8, u8), String> {
    let cleaned = hex.trim_start_matches('#');
    if cleaned.len() != 6 {
        return Err(format!("Invalid hex color: {hex}"));
    }
    let r =
        u8::from_str_radix(&cleaned[0..2], 16).map_err(|_| format!("Invalid hex color: {hex}"))?;
    let g =
        u8::from_str_radix(&cleaned[2..4], 16).map_err(|_| format!("Invalid hex color: {hex}"))?;
    let b =
        u8::from_str_radix(&cleaned[4..6], 16).map_err(|_| format!("Invalid hex color: {hex}"))?;
    Ok((r, g, b))
}

/// Resolve a color to a CSS string for HTML export:
/// - 256-index → hex
/// - empty string → default text color for the theme
/// - hex string → as-is
pub fn resolve_color_css(value: &ColorValue, default_text: &str) -> String {
    match value {
        ColorValue::Int(i) => ansi256_to_hex(*i),
        ColorValue::Str(s) if s.is_empty() => default_text.to_string(),
        ColorValue::Str(s) => s.clone(),
    }
}

/// Upstream `getResolvedThemeColors` — all theme colors as CSS strings.
pub fn get_resolved_theme_colors(
    theme_name: Option<&str>,
) -> Result<IndexMap<String, String>, String> {
    let default = default_theme();
    let name = theme_name.unwrap_or(&default);
    let is_light = name == LIGHT_THEME;
    let theme_json = load_theme_json(name)?;
    let resolved = resolve_theme_colors(&theme_json.colors, &theme_json.vars)?;
    let with_fallbacks = with_theme_color_fallbacks(&resolved);
    let default_text = if is_light { "#000000" } else { "#e5e5e7" };
    Ok(with_fallbacks
        .iter()
        .map(|(k, v)| (k.clone(), resolve_color_css(v, default_text)))
        .collect())
}

/// Explicit export colors from a theme's `export` section.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExportColors {
    pub page_bg: Option<String>,
    pub card_bg: Option<String>,
    pub info_bg: Option<String>,
}

/// Upstream `getThemeExportColors` — explicit export colors if present.
pub fn get_theme_export_colors(theme_name: Option<&str>) -> Result<ExportColors, String> {
    let default = default_theme();
    let name = theme_name.unwrap_or(&default);
    let theme_json = load_theme_json(name)?;
    let Some(export) = theme_json.export.as_ref() else {
        return Ok(ExportColors::default());
    };
    let resolve = |value: &Option<ColorValue>| match value {
        Some(ColorValue::Int(i)) => Some(ansi256_to_hex(*i)),
        Some(ColorValue::Str(s)) if s.is_empty() => None,
        Some(ColorValue::Str(s)) => {
            // Resolve var refs, then output hex for indices.
            match resolve_var_refs(
                &ColorValue::Str(s.clone()),
                &theme_json.vars,
                &mut Vec::new(),
            ) {
                Ok(ColorValue::Int(i)) => Some(ansi256_to_hex(i)),
                Ok(ColorValue::Str(s)) if s.is_empty() => None,
                Ok(ColorValue::Str(s)) => Some(s),
                Err(_) => None,
            }
        }
        None => None,
    };
    Ok(ExportColors {
        page_bg: resolve(&export.page_bg),
        card_bg: resolve(&export.card_bg),
        info_bg: resolve(&export.info_bg),
    })
}

/// Whether a theme is a "light" theme (upstream `isLightTheme`).
pub fn is_light_theme(theme_name: Option<&str>) -> bool {
    theme_name.unwrap_or(&default_theme()) == LIGHT_THEME
}

/// Parse the trailing background index out of `COLORFGBG`
/// (upstream `getColorFgBgBackgroundIndex`).
fn colorfgbg_background_index(colorfgbg: &str) -> Option<u8> {
    for part in colorfgbg.split(';').rev() {
        let bg = part.trim().parse::<u8>().ok();
        if bg.is_some() {
            return bg;
        }
    }
    None
}

fn rgb_luminance(r: u8, g: u8, b: u8) -> f64 {
    let to_linear = |c: f64| {
        let s = c / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * to_linear(r as f64) + 0.7152 * to_linear(g as f64) + 0.0722 * to_linear(b as f64)
}

/// Detect the terminal theme from `COLORFGBG` (upstream
/// `detectTerminalBackgroundFromEnv`); falls back to dark.
pub fn default_theme() -> String {
    if let Some(bg) = std::env::var("COLORFGBG")
        .ok()
        .and_then(|v| colorfgbg_background_index(&v))
    {
        let hex = ansi256_to_hex(bg);
        if let Ok((r, g, b)) = hex_to_rgb(&hex) {
            return if rgb_luminance(r, g, b) >= 0.5 {
                LIGHT_THEME.to_string()
            } else {
                DEFAULT_THEME.to_string()
            };
        }
    }
    DEFAULT_THEME.to_string()
}

/// Upstream `parseAutoThemeSetting` — "light/dark" slash-form setting.
pub fn parse_auto_theme_setting(theme_setting: Option<&str>) -> Option<(String, String)> {
    let setting = theme_setting?;
    let slash = setting.find('/')?;
    if setting[slash + 1..].contains('/') {
        return None;
    }
    let light = setting[..slash].trim();
    let dark = setting[slash + 1..].trim();
    if light.is_empty() || dark.is_empty() {
        return None;
    }
    Some((light.to_string(), dark.to_string()))
}

/// Upstream `resolveThemeSetting` (env-free terminal-theme variant).
pub fn resolve_theme_setting(theme_setting: Option<&str>, terminal_theme: &str) -> Option<String> {
    if let Some((light, dark)) = parse_auto_theme_setting(theme_setting) {
        return Some(if terminal_theme == "light" {
            light
        } else {
            dark
        });
    }
    if theme_setting?.contains('/') {
        return None;
    }
    theme_setting.map(|s| s.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn test_theme_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pi-theme-{label}-{}", uuid::Uuid::new_v4()))
    }

    fn write_test_theme(path: &Path, name: &str, accent: &str) {
        let mut value: serde_json::Value = serde_json::from_str(BUILTIN_DARK).unwrap();
        value["name"] = serde_json::Value::String(name.to_string());
        value["colors"]["accent"] = serde_json::Value::String(accent.to_string());
        std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    }

    #[test]
    fn builtin_themes_parse() {
        let themes = builtin_themes();
        assert_eq!(themes.len(), 2);
        assert_eq!(themes["dark"].name, "dark");
        assert_eq!(themes["light"].name, "light");
        assert!(themes["dark"].colors.contains_key("accent"));
        assert!(themes["dark"].export.is_some());
    }

    #[test]
    fn invalid_theme_reports_missing_required_color_tokens() {
        let error = parse_theme_json(
            "fixture",
            r##"{"name":"fixture","colors":{"accent":"#fff"}}"##,
        )
        .unwrap_err();
        assert!(error.starts_with("Invalid theme \"fixture\":"));
        assert!(error.contains("Missing required color tokens:"));
        assert!(error.contains("  - bashMode"));
    }

    #[test]
    fn available_builtin_themes_retain_shipped_source_paths() {
        let infos = available_themes_with_paths();
        for name in ["dark", "light"] {
            let info = infos.iter().find(|info| info.name == name).unwrap();
            assert_eq!(
                info.source_path.as_deref(),
                Some(builtin_theme_path(name).as_path())
            );
            assert!(
                info.source_path.as_ref().is_some_and(|path| path.is_file()),
                "shipped builtin theme path must exist: {:?}",
                info.source_path
            );
        }
    }

    #[test]
    fn var_references_resolve() {
        let themes = builtin_themes();
        let dark = &themes["dark"];
        let vars = &dark.vars;
        // accent -> "accent" var -> "#8abeb7"
        let resolved =
            resolve_var_refs(&ColorValue::Str("accent".into()), vars, &mut Vec::new()).unwrap();
        assert_eq!(resolved, ColorValue::Str("#8abeb7".into()));
        // Direct hex passes through
        let direct =
            resolve_var_refs(&ColorValue::Str("#123456".into()), vars, &mut Vec::new()).unwrap();
        assert_eq!(direct, ColorValue::Str("#123456".into()));
        // Empty passes through
        let empty =
            resolve_var_refs(&ColorValue::Str(String::new()), vars, &mut Vec::new()).unwrap();
        assert_eq!(empty, ColorValue::Str(String::new()));
        // Integer passes through
        let int = resolve_var_refs(&ColorValue::Int(7), vars, &mut Vec::new()).unwrap();
        assert_eq!(int, ColorValue::Int(7));
    }

    #[test]
    fn variable_cycles_error() {
        let mut vars = IndexMap::new();
        vars.insert("a".to_string(), ColorValue::Str("b".to_string()));
        vars.insert("b".to_string(), ColorValue::Str("a".to_string()));
        let err =
            resolve_var_refs(&ColorValue::Str("a".into()), &vars, &mut Vec::new()).unwrap_err();
        assert!(err.contains("Circular"));
    }

    #[test]
    fn missing_variable_errors() {
        let vars = IndexMap::new();
        let err =
            resolve_var_refs(&ColorValue::Str("nope".into()), &vars, &mut Vec::new()).unwrap_err();
        assert!(err.contains("Variable reference not found"));
    }

    #[test]
    fn fallbacks_are_applied() {
        let themes = builtin_themes();
        let dark = &themes["dark"];
        let fb = with_theme_color_fallbacks(&dark.colors);
        assert_eq!(
            fb.get("thinkingMax"),
            Some(&ColorValue::Str("#ff5fff".into()))
        );
        assert_eq!(
            fb.get("scrollbarThumb"),
            Some(&ColorValue::Str("selectedBg".into()))
        );
        assert_eq!(
            fb.get("searchMatchBg"),
            Some(&ColorValue::Str("selectedBg".into()))
        );
        assert_eq!(
            fb.get("searchMatchText"),
            Some(&ColorValue::Str("text".into()))
        );
    }

    #[test]
    fn ansi256_conversions() {
        assert_eq!(ansi256_to_hex(0), "#000000");
        assert_eq!(ansi256_to_hex(15), "#ffffff");
        assert_eq!(ansi256_to_hex(16), "#000000"); // cube 0,0,0
        assert_eq!(ansi256_to_hex(231), "#ffffff"); // cube 5,5,5 -> 255,255,255
        assert_eq!(ansi256_to_hex(232), "#080808");
        assert_eq!(ansi256_to_hex(255), "#eeeeee");
        assert_eq!(ansi256_to_hex(21), "#0000ff"); // cube r0 g0 b5 -> 255
    }

    #[test]
    fn resolved_theme_colors_dark() {
        let colors = get_resolved_theme_colors(Some("dark")).unwrap();
        assert_eq!(colors.get("accent").map(|s| s.as_str()), Some("#8abeb7"));
        assert_eq!(colors.get("border").map(|s| s.as_str()), Some("#5f87ff"));
        assert_eq!(
            colors.get("userMessageBg").map(|s| s.as_str()),
            Some("#343541")
        );
        assert_eq!(
            colors.get("thinkingMax").map(|s| s.as_str()),
            Some("#ff5fff")
        );
        assert_eq!(colors.get("text").map(|s| s.as_str()), Some("#d4d4d4"));
    }

    #[test]
    fn resolved_theme_colors_light() {
        let colors = get_resolved_theme_colors(Some("light")).unwrap();
        assert_eq!(colors.get("accent").map(|s| s.as_str()), Some("#5a8080"));
        assert_eq!(
            colors.get("userMessageBg").map(|s| s.as_str()),
            Some("#e8e8e8")
        );
        // Empty values fall back to black on light themes
        assert_eq!(colors.get("text").map(|s| s.as_str()), Some("#1f2328"));
    }

    #[test]
    fn export_colors() {
        let colors = get_theme_export_colors(Some("dark")).unwrap();
        assert_eq!(colors.page_bg.as_deref(), Some("#18181e"));
        assert_eq!(colors.card_bg.as_deref(), Some("#1e1e24"));
        assert_eq!(colors.info_bg.as_deref(), Some("#3c3728"));
        let light = get_theme_export_colors(Some("light")).unwrap();
        assert_eq!(light.page_bg.as_deref(), Some("#f8f8f8"));
        assert_eq!(light.card_bg.as_deref(), Some("#ffffff"));
        assert_eq!(light.info_bg.as_deref(), Some("#fffae6"));
    }

    #[test]
    fn unknown_theme_errors() {
        assert!(get_resolved_theme_colors(Some("nope")).is_err());
        assert!(load_theme_json("nope").is_err());
    }

    #[test]
    fn extension_theme_registration_parses_name_source_dedupes_and_replaces() {
        let _lock = test_theme_registry_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let dir = test_theme_dir("registration");
        std::fs::create_dir_all(&dir).unwrap();
        let first = dir.join("a.json");
        let duplicate = dir.join("b.json");
        let name = format!("extension-theme-{}", uuid::Uuid::new_v4());
        write_test_theme(&first, &name, "#123456");
        write_test_theme(&duplicate, &name, "#654321");

        let paths = vec![
            dir.to_string_lossy().into_owned(),
            first.to_string_lossy().into_owned(),
        ];
        let registered = register_theme_paths(&paths, Path::new("."));
        assert_eq!(registered.len(), 1);
        assert_eq!(registered[0].name, name);
        assert_eq!(
            registered[0].source_path,
            Some(std::fs::canonicalize(&first).unwrap())
        );
        assert!(available_theme_names().contains(&name));
        assert_eq!(
            get_resolved_theme_colors(Some(&name))
                .unwrap()
                .get("accent")
                .map(String::as_str),
            Some("#123456")
        );
        assert_eq!(load_theme_json(&name).unwrap().name, name);

        // Reload replaces the previous registry instead of accumulating it.
        assert!(register_theme_paths(&[], Path::new(".")).is_empty());
        assert!(!available_theme_names().contains(&name));
        assert!(get_resolved_theme_colors(Some(&name)).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn extension_theme_registration_retains_source_info() {
        let _lock = test_theme_registry_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let dir = test_theme_dir("source-info");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("extension.json");
        let name = format!("extension-source-info-{}", uuid::Uuid::new_v4());
        write_test_theme(&path, &name, "#123456");
        let source_info = SourceInfo {
            path: "/extensions/example.ts".to_string(),
            source: "extension:example".to_string(),
            scope: "temporary".to_string(),
            origin: "top-level".to_string(),
            base_dir: Some(dir.to_string_lossy().into_owned()),
        };

        let registered = register_theme_sources(
            &[(
                path.to_string_lossy().into_owned(),
                Some(source_info.clone()),
            )],
            Path::new("."),
        );
        assert_eq!(registered[0].source_info, Some(source_info));
        assert_eq!(
            registered[0].source_path,
            Some(std::fs::canonicalize(&path).unwrap())
        );

        assert!(register_theme_paths(&[], Path::new(".")).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn custom_theme_dir_loading() {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        // Load a custom theme JSON from ~/.pi/agent/themes via a temp agent dir.
        let dir = std::env::temp_dir().join(format!("pi-theme-test-{}", std::process::id()));
        let themes = dir.join("themes");
        std::fs::create_dir_all(&themes).unwrap();
        let custom = r##"{     "$schema": "https://example.com/schema.json",
            "name": "custom",
            "vars": { "brand": "#ff5500" },
            "colors": {
                "accent": "brand",
                "border": "brand",
                "borderAccent": "brand",
                "borderMuted": "brand",
                "success": "brand",
                "error": "brand",
                "warning": "brand",
                "muted": "brand",
                "dim": "brand",
                "text": "brand",
                "thinkingText": "brand",
                "selectedBg": "brand",
                "userMessageBg": "brand",
                "userMessageText": "brand",
                "customMessageBg": "brand",
                "customMessageText": "brand",
                "customMessageLabel": "brand",
                "toolPendingBg": "brand",
                "toolSuccessBg": "brand",
                "toolErrorBg": "brand",
                "toolTitle": "brand",
                "toolOutput": "brand",
                "mdHeading": "brand",
                "mdLink": "brand",
                "mdLinkUrl": "brand",
                "mdCode": "brand",
                "mdCodeBlock": "brand",
                "mdCodeBlockBorder": "brand",
                "mdQuote": "brand",
                "mdQuoteBorder": "brand",
                "mdHr": "brand",
                "mdListBullet": "brand",
                "toolDiffAdded": "brand",
                "toolDiffRemoved": "brand",
                "toolDiffContext": "brand",
                "syntaxComment": "brand",
                "syntaxKeyword": "brand",
                "syntaxFunction": "brand",
                "syntaxVariable": "brand",
                "syntaxString": "brand",
                "syntaxNumber": "brand",
                "syntaxType": "brand",
                "syntaxOperator": "brand",
                "syntaxPunctuation": "brand",
                "thinkingOff": "brand",
                "thinkingMinimal": "brand",
                "thinkingLow": "brand",
                "thinkingMedium": "brand",
                "thinkingHigh": "brand",
                "thinkingXhigh": "brand",
                "bashMode": "brand"
            },
            "export": { "pageBg": "#ffffff", "cardBg": "#111111" }
        }"##;
        std::fs::write(themes.join("custom.json"), custom).unwrap();
        std::env::set_var("PI_CODING_AGENT_DIR", dir.to_str().unwrap());
        let colors = get_resolved_theme_colors(Some("custom")).unwrap();
        assert_eq!(colors.get("accent").map(|s| s.as_str()), Some("#ff5500"));
        let export = get_theme_export_colors(Some("custom")).unwrap();
        assert_eq!(export.page_bg.as_deref(), Some("#ffffff"));
        assert_eq!(export.card_bg.as_deref(), Some("#111111"));
        std::env::remove_var("PI_CODING_AGENT_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auto_theme_setting_parsing() {
        assert_eq!(
            parse_auto_theme_setting(Some("light/dark")),
            Some(("light".into(), "dark".into()))
        );
        assert_eq!(parse_auto_theme_setting(Some("light")), None);
        assert_eq!(parse_auto_theme_setting(Some("a/b/c")), None);
        assert_eq!(parse_auto_theme_setting(Some(" /dark")), None);
        assert_eq!(parse_auto_theme_setting(None), None);
        assert_eq!(
            resolve_theme_setting(Some("light/dark"), "light").as_deref(),
            Some("light")
        );
        assert_eq!(
            resolve_theme_setting(Some("light/dark"), "dark").as_deref(),
            Some("dark")
        );
        assert_eq!(
            resolve_theme_setting(Some("dark"), "light").as_deref(),
            Some("dark")
        );
        assert_eq!(resolve_theme_setting(Some("a/b/c"), "light"), None);
    }

    #[test]
    fn default_theme_detection_from_env() {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        // No COLORFGBG -> dark
        std::env::remove_var("COLORFGBG");
        assert_eq!(default_theme(), "dark");
        // COLORFGBG with a light background index (15 = white) -> light
        std::env::set_var("COLORFGBG", "15");
        assert_eq!(default_theme(), "light");
        // Dark background index (0 = black) -> dark
        std::env::set_var("COLORFGBG", "0");
        assert_eq!(default_theme(), "dark");
        std::env::remove_var("COLORFGBG");
    }
}
