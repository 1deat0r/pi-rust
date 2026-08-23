//! Theme registry + color resolution — port of
//! `packages/coding-agent/src/modes/interactive/theme/theme.ts` (the
//! data/JSON resolution side used by HTML export and, later, the TUI).
//!
//! Scope note (documented divergence): the upstream module additionally
//! carries a `Theme` class that paints ANSI strings for the TUI (fg/bg
//! sequences, truecolor/256 fallback), a global theme proxy, file watchers,
//! and registered-theme management. This port covers the JSON side needed by
//! export-html (and by the upcoming TUI theming): builtin theme data
//! (embedded), custom-theme loading from `~/.pi/agent/themes`, variable
//! resolution, 256-color conversion, and the resolved CSS colors / export
//! colors. The ANSI painting side will land with the interactive TUI work.

use std::path::PathBuf;

use std::collections::BTreeMap;

use indexmap::IndexMap;

use serde::Deserialize;

use crate::config;

pub const DEFAULT_THEME: &str = "dark";
pub const LIGHT_THEME: &str = "light";

const BUILTIN_DARK: &str = include_str!("../data/themes/dark.json");
const BUILTIN_LIGHT: &str = include_str!("../data/themes/light.json");

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
#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize, Default)]
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
    serde_json::from_str(content).map_err(|e| format!("Failed to parse theme {label}: {e}"))
}

/// Built-in theme registry (embedded copies of the shipped dark/light JSON).
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

/// Load a theme JSON by name: builtin first, then the custom themes dir.
/// Mirrors upstream `loadThemeJson`.
pub fn load_theme_json(name: &str) -> Result<ThemeJson, String> {
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
mod tests {
    use super::*;

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
    fn custom_theme_dir_loading() {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();
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
        let _guard = ENV_LOCK.lock().unwrap();
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
