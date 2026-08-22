//! Session → HTML export — port of
//! `packages/coding-agent/src/core/export-html/index.ts`.
//!
//! Renders a session JSONL file into the standalone self-contained HTML
//! viewer (marked.js + highlight.js vendored) with theme-colored CSS.
//!
//! Documented divergence: custom tool pre-rendering (extension-owned
//! `renderCall`/`renderResult` TUI components → ANSI → HTML) is not wired
//! because the extension system is not yet ported; the template-generated
//! tools (bash/read/write/edit/ls) render exactly like upstream from the
//! session entries. Template assets are embedded at compile time (upstream
//! reads them from disk beside the package).

use std::path::Path;

use base64::Engine;
use serde_json::{Map, Value};

use crate::config::APP_NAME;
use crate::theme;

const TEMPLATE_HTML: &str = include_str!("../../data/export-html/template.html");
const TEMPLATE_CSS: &str = include_str!("../../data/export-html/template.css");
const TEMPLATE_JS: &str = include_str!("../../data/export-html/template.js");
const VENDOR_MARKED_JS: &str = include_str!("../../data/export-html/vendor/marked.min.js");
const VENDOR_HIGHLIGHT_JS: &str = include_str!("../../data/export-html/vendor/highlight.min.js");

/// Tools rendered directly by the HTML template (not pre-rendered via
/// TUI→ANSI→HTML pipeline); mirrors upstream `TEMPLATE_RENDERED_TOOLS`.
const TEMPLATE_RENDERED_TOOLS: [&str; 5] = ["bash", "read", "write", "edit", "ls"];

/// Error type mirroring upstream thrown Error messages.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("{0}")]
    Message(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl ExportError {
    fn msg(message: impl Into<String>) -> Self {
        ExportError::Message(message.into())
    }
}

// ---------------------------------------------------------------------------
// Color helpers (export-html/index.ts)
// ---------------------------------------------------------------------------

fn parse_color(color: &str) -> Option<(u8, u8, u8)> {
    // hex #RRGGBB
    if color.starts_with('#') && color.len() == 7 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&color[1..3], 16),
            u8::from_str_radix(&color[3..5], 16),
            u8::from_str_radix(&color[5..7], 16),
        ) {
            return Some((r, g, b));
        }
    }
    // rgb(r,g,b)
    if let Some(rest) = color.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')')) {
        let parts: Vec<&str> = rest.split(',').map(|s| s.trim()).collect();
        if parts.len() == 3 {
            if let (Ok(r), Ok(g), Ok(b)) = (
                parts[0].parse::<u8>(),
                parts[1].parse::<u8>(),
                parts[2].parse::<u8>(),
            ) {
                return Some((r, g, b));
            }
        }
    }
    None
}

fn get_luminance((r, g, b): (u8, u8, u8)) -> f64 {
    let to_linear = |c: f64| {
        let s = c / 255.0;
        if s <= 0.03928 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * to_linear(r as f64) + 0.7152 * to_linear(g as f64) + 0.0722 * to_linear(b as f64)
}

/// Factor > 1 lightens, < 1 darkens (clamped to u8).
fn adjust_brightness(color: &str, factor: f64) -> String {
    match parse_color(color) {
        None => color.to_string(),
        Some((r, g, b)) => {
            let adj = |c: u8| ((c as f64 * factor).round()).clamp(0.0, 255.0) as u8;
            format!("rgb({}, {}, {})", adj(r), adj(g), adj(b))
        }
    }
}

/// Upstream `deriveExportColors` — page/card/info backgrounds derived from a
/// base color when a theme doesn't specify explicit export colors.
fn derive_export_colors(base_color: &str) -> (String, String, String) {
    match parse_color(base_color) {
        None => (
            "rgb(24, 24, 30)".to_string(),
            "rgb(30, 30, 36)".to_string(),
            "rgb(60, 55, 40)".to_string(),
        ),
        Some((r, g, b)) => {
            let luminance = get_luminance((r, g, b));
            if luminance > 0.5 {
                (
                    adjust_brightness(base_color, 0.96),
                    base_color.to_string(),
                    format!("rgb({}, {}, {})", (r as u16 + 10).min(255), (g as u16 + 5).min(255), (b as u16).saturating_sub(20)),
                )
            } else {
                (
                    adjust_brightness(base_color, 0.7),
                    adjust_brightness(base_color, 0.85),
                    format!("rgb({}, {}, {})", (r as u16 + 20).min(255), (g as u16 + 15).min(255), b),
                )
            }
        }
    }
}

/// Upstream `generateThemeVars` — CSS custom properties for the theme.
fn generate_theme_vars(theme_name: Option<&str>) -> Result<String, ExportError> {
    let colors = theme::get_resolved_theme_colors(theme_name)
        .map_err(|e| ExportError::msg(format!("Failed to resolve theme colors: {e}")))?;
    let mut lines: Vec<String> = Vec::new();
    for (key, value) in &colors {
        lines.push(format!("--{key}: {value};"));
    }
    let theme_export = theme::get_theme_export_colors(theme_name).unwrap_or_default();
    let user_message_bg = colors.get("userMessageBg").cloned().unwrap_or_else(|| "#343541".to_string());
    let derived = derive_export_colors(&user_message_bg);
    let page_bg = theme_export.page_bg.unwrap_or_else(|| derived.0.clone());
    let card_bg = theme_export.card_bg.unwrap_or_else(|| derived.1.clone());
    let info_bg = theme_export.info_bg.unwrap_or_else(|| derived.2.clone());
    lines.push(format!("--exportPageBg: {page_bg};"));
    lines.push(format!("--exportCardBg: {card_bg};"));
    lines.push(format!("--exportInfoBg: {info_bg};"));
    Ok(lines.join("\n      "))
}

/// The rendered session payload passed to the template (mirrors upstream
/// `SessionData`; omitted keys stay absent so JSON output matches).
pub struct SessionData {
    pub header: Value,
    pub entries: Vec<Value>,
    pub leaf_id: Option<String>,
    pub system_prompt: Option<String>,
    pub tools: Option<Vec<Value>>,
    pub rendered_tools: Option<Map<String, Value>>,
}

fn session_data_to_base64(data: &SessionData) -> Result<String, ExportError> {
    let mut map = Map::new();
    map.insert("header".to_string(), data.header.clone());
    map.insert("entries".to_string(), Value::Array(data.entries.clone()));
    map.insert("leafId".to_string(), match &data.leaf_id {
        Some(id) => Value::String(id.clone()),
        None => Value::Null,
    });
    if let Some(sp) = &data.system_prompt {
        map.insert("systemPrompt".to_string(), Value::String(sp.clone()));
    }
    if let Some(tools) = &data.tools {
        map.insert("tools".to_string(), Value::Array(tools.clone()));
    }
    if let Some(rt) = &data.rendered_tools {
        map.insert("renderedTools".to_string(), Value::Object(rt.clone()));
    }
    let json = serde_json::to_string(&Value::Object(map)).map_err(|e| ExportError::msg(format!("serialize session data: {e}")))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(json.as_bytes()))
}


/// JS `String.prototype.replace(searchString, replacement)` semantics with no
/// capture groups (ES GetSubstitution): `$$` -> `$`, `$&` -> the matched
/// string, ``$` `` -> text before the match, `$'` -> text after the match,
/// `$n` -> literal `$n` (no groups), other `$` -> literal. Replaces the
/// FIRST occurrence only, matching JS.
pub fn js_replace(haystack: &str, search: &str, replacement: &str) -> String {
    let Some(rel_pos) = haystack.find(search) else {
        return haystack.to_string();
    };
    let before = &haystack[..rel_pos];
    let after = &haystack[rel_pos + search.len()..];
    let mut out = String::with_capacity(before.len() + replacement.len() + after.len());
    out.push_str(before);
    let mut chars = replacement.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some((_, '$')) => {
                chars.next();
                out.push('$');
            }
            Some((_, '&')) => {
                chars.next();
                out.push_str(search);
            }
            Some((_, '`')) => {
                chars.next();
                out.push_str(before);
            }
            Some((_, '\'')) => {
                chars.next();
                out.push_str(after);
            }
            Some((_, d)) if d.is_ascii_digit() => {
                // ES parses up to two digits; with no capture groups the
                // result is the literal `$` + digits.
                let mut digits = String::new();
                while digits.len() < 2 {
                    match chars.peek() {
                        Some((_, d2)) if d2.is_ascii_digit() => {
                            digits.push(*d2);
                            chars.next();
                        }
                        _ => break,
                    }
                }
                out.push('$');
                out.push_str(&digits);
            }
            _ => out.push('$'),
        }
    }
    out.push_str(after);
    out
}

/// Core HTML generation (upstream `generateHtml`).
pub fn generate_html(data: &SessionData, theme_name: Option<&str>) -> Result<String, ExportError> {
    let theme_vars = generate_theme_vars(theme_name)?;
    let colors = theme::get_resolved_theme_colors(theme_name)
        .map_err(|e| ExportError::msg(format!("Failed to resolve theme colors: {e}")))?;
    let theme_export = theme::get_theme_export_colors(theme_name).unwrap_or_default();
    let derived = derive_export_colors(colors.get("userMessageBg").map(String::as_str).unwrap_or("#343541"));
    let body_bg = theme_export.page_bg.clone().unwrap_or_else(|| derived.0.clone());
    let container_bg = theme_export.card_bg.clone().unwrap_or_else(|| derived.1.clone());
    let info_bg = theme_export.info_bg.clone().unwrap_or_else(|| derived.2.clone());

    let session_data_b64 = session_data_to_base64(data)?;

    let css = js_replace(&js_replace(&js_replace(&js_replace(TEMPLATE_CSS, "{{THEME_VARS}}", &theme_vars),
        "{{BODY_BG}}", &body_bg), "{{CONTAINER_BG}}", &container_bg), "{{INFO_BG}}", &info_bg);

    let html = js_replace(TEMPLATE_HTML, "{{CSS}}", &css);
    let html = js_replace(&html, "{{JS}}", TEMPLATE_JS);
    let html = js_replace(&html, "{{SESSION_DATA}}", &session_data_b64);
    let html = js_replace(&html, "{{MARKED_JS}}", VENDOR_MARKED_JS);
    let html = js_replace(&html, "{{HIGHLIGHT_JS}}", VENDOR_HIGHLIGHT_JS);
    Ok(html)
}

// ---------------------------------------------------------------------------
// Session file loading (mirrors SessionManager.open + getHeader/getEntries)
// ---------------------------------------------------------------------------

/// A session file's header, entries, and leaf id (as parsed for export).
pub struct LoadedSession {
    pub header: Option<Value>,
    pub entries: Vec<Value>,
    pub leaf_id: Option<String>,
}

/// Parse a session JSONL file.
/// Mirrors upstream `loadEntriesFromFile` + index building over the union of
/// the two session header formats this build produces:
/// - coding-agent format: first record `{"type":"session", ...}` (upstream's
///   own session files; `exportFromFile` parity).
/// - pi-agent v4 format: first record `{"kind":"header", "createdAt": ...}`
///   (the RPC/interactive session repo format). A `timestamp` field is
///   synthesized from `createdAt` for the HTML viewer's date display.
///
/// If the file is empty or its first parsed record is not a valid header, all
/// entries are dropped and `(None, [], None)` is returned — exactly like
/// upstream `loadEntriesFromFile`, which returns `[]` for a headerless file.
pub fn load_session_file(path: &str) -> Result<LoadedSession, ExportError> {
    let content = std::fs::read_to_string(path)?;
    let mut file_entries: Vec<Value> = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<Value>(line) {
            file_entries.push(entry);
        }
    }
    if file_entries.is_empty() {
        return Ok(LoadedSession { header: None, entries: Vec::new(), leaf_id: None });
    }
    let first = &file_entries[0];
    let session_type = first.get("type").and_then(|t| t.as_str());
    let kind = first.get("kind").and_then(|t| t.as_str());
    let valid_header = match (session_type, kind) {
        (Some("session"), _) => first.get("id").and_then(|i| i.as_str()).is_some(),
        (_, Some("header")) => first.get("id").and_then(|i| i.as_str()).is_some(),
        _ => false,
    };
    if !valid_header {
        return Ok(LoadedSession { header: None, entries: Vec::new(), leaf_id: None });
    }
    let mut header = first.clone();
    // Synthesize `timestamp` from pi-agent v4 `createdAt` (ms epoch) so the
    // viewer's date shows for RPC/interactive session files.
    if session_type != Some("session") && header.get("timestamp").is_none() {
        if let Some(created) = header.get("createdAt").and_then(|c| c.as_u64()) {
            let secs = created / 1000;
            let millis = (created % 1000) as u32 * 1_000_000;
            let ts = time_from_epoch_ms(secs, millis);
            header
                .as_object_mut()
                .expect("header is an object")
                .insert("timestamp".to_string(), Value::String(ts));
        }
    }
    let entries: Vec<Value> = file_entries
        .iter()
        .filter(|e| {
            let t = e.get("type").and_then(|t| t.as_str());
            let k = e.get("kind").and_then(|t| t.as_str());
            !(t == Some("session") || k == Some("header"))
        })
        .cloned()
        .collect();
    let leaf_id = entries
        .last()
        .and_then(|e| e.get("id").and_then(|i| i.as_str()).map(|s| s.to_string()));
    Ok(LoadedSession { header: Some(header), entries, leaf_id })
}

/// Format an epoch (seconds, nanos) as an ISO-8601 UTC timestamp (used to
/// synthesize the HTML viewer's `timestamp` for pi-agent v4 headers).
fn time_from_epoch_ms(secs: u64, nanos: u32) -> String {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days as i64);
    let (hh, mm, ss) = (secs_of_day / 3600, (secs_of_day % 3600) / 60, secs_of_day % 60);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y,
        m,
        d,
        hh,
        mm,
        ss,
        nanos / 1_000_000
    )
}

/// Howard Hinnant's civil-from-days algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Which tools are pre-rendered (upstream `TEMPLATE_RENDERED_TOOLS`).
pub fn is_template_rendered_tool(name: &str) -> bool {
    TEMPLATE_RENDERED_TOOLS.contains(&name)
}

// ---------------------------------------------------------------------------
// Export entry points
// ---------------------------------------------------------------------------

/// Export a session JSONL file to HTML (upstream `exportFromFile`).
pub fn export_session_file(
    input_path: &str,
    output_path: Option<&str>,
    theme_name: Option<&str>,
) -> Result<String, ExportError> {
    let resolved_input = Path::new(input_path);
    if !resolved_input.exists() {
        return Err(ExportError::msg(format!("File not found: {input_path}")));
    }
    let loaded = load_session_file(input_path)?;
    let data = SessionData {
        header: loaded.header.unwrap_or_else(|| serde_json::json!({"type": "session"})),
        entries: loaded.entries,
        leaf_id: loaded.leaf_id,
        system_prompt: None,
        tools: None,
        rendered_tools: None,
    };
    let html = generate_html(&data, theme_name)?;
    let output_path = default_output_path(input_path, output_path);
    std::fs::write(&output_path, html)?;
    Ok(output_path)
}

/// Compute the output path: explicit path if given, else
/// `<APP_NAME>-session-<basename-without-jsonl>.html` in the cwd.
fn default_output_path(input_path: &str, output_path: Option<&str>) -> String {
    if let Some(p) = output_path {
        return p.to_string();
    }
    let basename = Path::new(input_path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "session".to_string());
    format!("{APP_NAME}-session-{basename}.html")
}

/// Pre-render custom tool calls/results (upstream `preRenderCustomTools`).
///
/// The current port has no custom tool renderers (the extension system is
/// pending), so every custom-tool block is skipped exactly as upstream does
/// when `toolRenderer` returns `undefined` for a tool — i.e. no-op delivery.
/// Kept as a named function so the entry-scan contract is documented and the
/// extension-backed implementation can slot in when extensions land.
pub fn pre_render_custom_tools(_entries: &[Value]) -> Map<String, Value> {
    Map::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp_session(path: &Path) -> String {
        let header = r#"{"type":"session","version":4,"id":"sess_001","timestamp":"2026-08-22T00:00:00.000Z","cwd":"/tmp"}"#;
        let e1 = r#"{"type":"message","id":"msg_1","parentId":null,"timestamp":"2026-08-22T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}"#;
        let e2 = r#"{"type":"message","id":"msg_2","parentId":"msg_1","timestamp":"2026-08-22T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hi there"}]}}"#;
        let content = format!("{header}\n{e1}\n{e2}\n");
        std::fs::write(path, content).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn derives_export_colors_dark_and_light() {
        let (page, card, info) = derive_export_colors("#343541");
        assert_eq!(page, "rgb(36, 37, 46)");
        assert_eq!(card, "rgb(44, 45, 55)");
        assert_eq!(info, "rgb(72, 68, 65)");
        // Parse-error fallback
        let (page2, card2, info2) = derive_export_colors("nonsense");
        assert_eq!(page2, "rgb(24, 24, 30)");
        assert_eq!(card2, "rgb(30, 30, 36)");
        assert_eq!(info2, "rgb(60, 55, 40)");
    }

    #[test]
    fn adjusts_brightness() {
        assert_eq!(adjust_brightness("#000000", 0.7), "rgb(0, 0, 0)");
        assert_eq!(adjust_brightness("#ffffff", 0.5), "rgb(128, 128, 128)");
        assert_eq!(adjust_brightness("not-a-color", 0.5), "not-a-color");
    }

    #[test]
    fn theme_vars_have_export_colors() {
        let vars = generate_theme_vars(Some("dark")).unwrap();
        assert!(vars.contains("--accent: #8abeb7;"));
        assert!(vars.contains("--exportPageBg: #18181e;"));
        assert!(vars.contains("--exportCardBg: #1e1e24;"));
        assert!(vars.contains("--exportInfoBg: #3c3728;"));
        let vars_light = generate_theme_vars(Some("light")).unwrap();
        assert!(vars_light.contains("--userMessageBg: #e8e8e8;"));
    }

    #[test]
    fn loads_session_file() {
        let dir = std::env::temp_dir().join(format!("pi-export-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let path_str = write_tmp_session(&path);
        let loaded = load_session_file(&path_str).unwrap();
        let header = loaded.header.unwrap();
        assert_eq!(header["type"], "session");
        assert_eq!(loaded.entries.len(), 2);
        assert_eq!(loaded.leaf_id.as_deref(), Some("msg_2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_fails() {
        let err = export_session_file("/definitely/not/here.jsonl", None, None).unwrap_err();
        assert!(err.to_string().contains("File not found"));
    }

    #[test]
    fn exports_pi_agent_v4_header_session() {
        let dir = std::env::temp_dir().join(format!("pi-export-v4-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let content = r#"{"kind":"header","version":4,"id":"sess_001","createdAt":1787375500419,"cwd":"/tmp"}
{"type":"message","id":"msg_1","parentId":null,"timestamp":"2026-08-22T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}
"#;
        std::fs::write(&path, content).unwrap();
        let loaded = load_session_file(path.to_str().unwrap()).unwrap();
        let header = loaded.header.unwrap();
        assert_eq!(header["kind"], "header");
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.leaf_id.as_deref(), Some("msg_1"));
        // timestamp synthesized from createdAt
        assert_eq!(header["timestamp"], "2026-08-22T05:11:40.419Z");
        let out = dir.join("out.html");
        let out_str = export_session_file(path.to_str().unwrap(), Some(out.to_str().unwrap()), Some("dark")).unwrap();
        let html = std::fs::read_to_string(&out_str).unwrap();
        assert!(html.contains("Session Export"));
        // The timestamp travels in the base64 session payload (the viewer
        // renders it client-side with toLocaleString).
        let marker = r#"<script id="session-data" type="application/json">"#;
        let start = html.find(marker).unwrap() + marker.len();
        let end = html[start..].find("</script>").unwrap();
        let b64 = &html[start..start + end];
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        let data: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(data["header"]["timestamp"], "2026-08-22T05:11:40.419Z");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exports_html_with_theme() {
        let dir = std::env::temp_dir().join(format!("pi-export-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let path_str = write_tmp_session(&path);
        let out = dir.join("out.html");
        let out_str = export_session_file(&path_str, Some(out.to_str().unwrap()), Some("dark")).unwrap();
        let html = std::fs::read_to_string(&out_str).unwrap();
        // Template markers substituted (the {{HIGHLIGHT_JS}} occurrence
        // inside the vendored highlight.min.js is reproduced verbatim, like
        // upstream's JS replace chain — see the oracle parity test).
        for marker in ["{{CSS}}", "{{JS}}", "{{SESSION_DATA}}", "{{MARKED_JS}}"] {
            assert!(!html.contains(marker), "unsubstituted marker {marker}");
        }
        // Theme vars injected
        assert!(html.contains("--accent: #8abeb7;"));
        assert!(html.contains("--exportPageBg: #18181e;"));
        // Vendored JS present
        assert!(html.contains("marked") || html.contains("hljs"));
        // Session data is base64 and round-trips
        let marker = r#"<script id="session-data" type="application/json">"#;
        let start = html.find(marker).unwrap() + marker.len();
        let end = html[start..].find("</script>").unwrap();
        let b64 = &html[start..start + end];
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        let data: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(data["header"]["id"], "sess_001");
        assert_eq!(data["entries"].as_array().unwrap().len(), 2);
        assert_eq!(data["leafId"], "msg_2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_output_name() {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("pi-export-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("2026-08-22_abc123.jsonl");
        let path_str = write_tmp_session(&path);
        // chdir so the default resolves deterministically
        let cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let out = export_session_file(&path_str, None, Some("dark")).unwrap();
        assert!(out.ends_with("pi-session-2026-08-22_abc123.html"));
        assert!(Path::new(&out).exists());
        std::env::set_current_dir(cwd).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }


    #[test]
    fn js_replace_semantics_match_es() {
        assert_eq!(js_replace("A{{M}}B", "{{M}}", "$&"), "A{{M}}B");
        assert_eq!(js_replace("A{{M}}B", "{{M}}", "$$"), "A$B");
        assert_eq!(js_replace("A{{M}}B", "{{M}}", "$1"), "A$1B");
        assert_eq!(js_replace("A{{M}}B", "{{M}}", "$0"), "A$0B");
        assert_eq!(js_replace("A{{M}}B", "{{M}}", "$`"), "AAB");
        assert_eq!(js_replace("A{{M}}B", "{{M}}", "$'"), "ABB");
        assert_eq!(js_replace("A{{M}}B", "{{M}}", "x$1y"), "Ax$1yB");
        assert_eq!(js_replace("A{{M}}B", "{{M}}", "no-dollar"), "Ano-dollarB");
        assert_eq!(js_replace("A{{M}}B", "{{M}}", "$10"), "A$10B");
        // Only first occurrence is replaced (JS behavior)
        assert_eq!(js_replace("{{M}}x{{M}}", "{{M}}", "R"), "Rx{{M}}");
        // No marker present -> unchanged
        assert_eq!(js_replace("hello", "{{M}}", "R"), "hello");
    }

    #[test]
    fn template_rendered_tools_match() {
        assert!(is_template_rendered_tool("bash"));
        assert!(is_template_rendered_tool("edit"));
        assert!(!is_template_rendered_tool("custom_tool"));
    }
}
