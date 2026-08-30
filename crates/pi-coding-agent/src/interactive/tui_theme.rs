//! TUI theme painting — the ANSI side of
//! `packages/coding-agent/src/modes/interactive/theme/theme.ts`.
//!
//! Resolves theme colors through `crate::theme` (the JSON/color layer ported
//! by the parent) and paints ANSI truecolor or ANSI256 fg/bg sequences based
//! on the attached terminal's advertised capabilities. Falls back to
//! uncolored text when a color is unknown.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, SystemTime};

use crate::theme;
use indexmap::IndexMap;
use pi_tui::terminal_image::get_capabilities;

static THEME_STATE: RwLock<Option<ThemeState>> = RwLock::new(None);
pub type ThemeChangeCallback = Arc<dyn Fn() + Send + Sync>;
static THEME_CHANGE_CALLBACKS: RwLock<Vec<ThemeChangeCallback>> = RwLock::new(Vec::new());
static THEME_WATCHER: Mutex<Option<ThemeWatcher>> = Mutex::new(None);

struct ThemeWatcher {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

#[derive(Debug, Clone)]
struct ThemeState {
    name: String,
    colors: HashMap<String, String>,
}

/// Resolve and cache the active theme's colors.
pub fn load_theme(name: &str) {
    if try_load_theme(name).is_ok() {
        return;
    }
    let colors = theme::get_resolved_theme_colors(Some(theme::DEFAULT_THEME)).unwrap_or_default();
    replace_theme_state(theme::DEFAULT_THEME, colors);
}

/// Load a named theme without silently hiding an invalid selector choice.
/// Interactive callers can surface the returned error while the legacy
/// `load_theme` entry point retains upstream init/fallback behavior.
pub fn try_load_theme(name: &str) -> Result<(), String> {
    let colors = theme::get_resolved_theme_colors(Some(name))?;
    replace_theme_state(name, colors);
    Ok(())
}

fn replace_theme_state(name: &str, colors: IndexMap<String, String>) {
    let colors: HashMap<String, String> = colors.into_iter().collect();
    let mut guard = THEME_STATE.write().unwrap_or_else(|e| e.into_inner());
    *guard = Some(ThemeState {
        name: name.to_string(),
        colors,
    });
    drop(guard);
    let callbacks = THEME_CHANGE_CALLBACKS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    for callback in callbacks {
        callback();
    }
}

/// Register a callback invoked whenever the active theme is replaced.
pub fn on_theme_change(callback: ThemeChangeCallback) {
    THEME_CHANGE_CALLBACKS
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .push(callback);
}

pub fn stop_theme_watcher() {
    let watcher = THEME_WATCHER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(mut watcher) = watcher {
        watcher.stop.store(true, Ordering::Release);
        if let Some(handle) = watcher.handle.take() {
            if handle.thread().id() != thread::current().id() {
                let _ = handle.join();
            }
        }
    }
}

/// Watch a custom theme file and reload it after an on-disk change.
pub fn watch_theme_file(path: PathBuf, name: String) {
    stop_theme_watcher();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_thread = Arc::clone(&stop);
    let watched_path = path;
    let handle = thread::spawn(move || {
        let mut last_modified = std::fs::metadata(&watched_path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        while !stop_for_thread.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(100));
            let Ok(modified) =
                std::fs::metadata(&watched_path).and_then(|metadata| metadata.modified())
            else {
                continue;
            };
            if modified <= last_modified {
                continue;
            }
            last_modified = modified;
            if theme::reload_registered_theme_path(&watched_path).is_ok() {
                load_theme(&name);
            }
        }
    });
    *THEME_WATCHER.lock().unwrap_or_else(|e| e.into_inner()) = Some(ThemeWatcher {
        stop,
        handle: Some(handle),
    });
}

/// Start watching the source file for the currently active custom theme.
pub fn watch_active_theme() {
    let Some(name) = active_theme_name() else {
        stop_theme_watcher();
        return;
    };
    if name == theme::DEFAULT_THEME || name == theme::LIGHT_THEME {
        stop_theme_watcher();
        return;
    }
    let path = theme::available_themes_with_paths()
        .into_iter()
        .find(|info| info.name == name)
        .and_then(|info| info.source_path);
    match path {
        Some(path) => watch_theme_file(path, name),
        None => stop_theme_watcher(),
    }
}

pub fn active_theme_name() -> Option<String> {
    THEME_STATE
        .read()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.name.clone()))
}

fn color_value(name: &str) -> Option<String> {
    let guard = THEME_STATE.read().ok()?;
    let state = guard.as_ref()?;
    state.colors.get(name).cloned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColorMode {
    Truecolor,
    Ansi256,
}

fn color_mode() -> ColorMode {
    if get_capabilities().true_color {
        ColorMode::Truecolor
    } else {
        ColorMode::Ansi256
    }
}

/// Resolve a hex color to an RGB triple if known.
fn parse_color_rgb(value: &str) -> Option<(u8, u8, u8)> {
    let hex = value.trim_start_matches('#');
    if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
        return Some((r, g, b));
    }
    None
}

const ANSI256_CUBE_VALUES: [u8; 6] = [0, 95, 135, 175, 215, 255];

fn closest_cube_index(value: u8) -> usize {
    let mut closest = 0;
    let mut minimum = f64::INFINITY;
    for (index, candidate) in ANSI256_CUBE_VALUES.iter().enumerate() {
        let distance = f64::from(value.abs_diff(*candidate)).powi(2);
        if distance < minimum {
            minimum = distance;
            closest = index;
        }
    }
    closest
}

fn closest_gray_index(gray: f64) -> usize {
    let mut closest = 0;
    let mut minimum = f64::INFINITY;
    for index in 0..24 {
        let candidate = 8.0 + index as f64 * 10.0;
        let distance = (gray - candidate).powi(2);
        if distance < minimum {
            minimum = distance;
            closest = index;
        }
    }
    closest
}

fn color_distance(left: (f64, f64, f64), right: (f64, f64, f64)) -> f64 {
    let dr = left.0 - right.0;
    let dg = left.1 - right.1;
    let db = left.2 - right.2;
    dr * dr * 0.299 + dg * dg * 0.587 + db * db * 0.114
}

/// Match upstream's nearest-color selection for a 256-color terminal.
fn rgb_to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    let r_index = closest_cube_index(r);
    let g_index = closest_cube_index(g);
    let b_index = closest_cube_index(b);
    let cube = (
        f64::from(ANSI256_CUBE_VALUES[r_index]),
        f64::from(ANSI256_CUBE_VALUES[g_index]),
        f64::from(ANSI256_CUBE_VALUES[b_index]),
    );
    let source = (f64::from(r), f64::from(g), f64::from(b));
    let cube_distance = color_distance(source, cube);

    let gray = (0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b)).round();
    let gray_index = closest_gray_index(gray);
    let gray_value = 8.0 + gray_index as f64 * 10.0;
    let gray_distance = color_distance(source, (gray_value, gray_value, gray_value));
    let spread = r.max(g).max(b) - r.min(g).min(b);

    if spread < 10 && gray_distance < cube_distance {
        (232 + gray_index) as u8
    } else {
        (16 + 36 * r_index + 6 * g_index + b_index) as u8
    }
}

fn ansi_color(value: &str, foreground: bool, mode: ColorMode) -> Option<String> {
    if value.is_empty() {
        return Some(if foreground {
            "\x1b[39m".to_string()
        } else {
            "\x1b[49m".to_string()
        });
    }
    let (r, g, b) = parse_color_rgb(value)?;
    Some(match mode {
        ColorMode::Truecolor => {
            let channel = if foreground { 38 } else { 48 };
            format!("\x1b[{channel};2;{r};{g};{b}m")
        }
        ColorMode::Ansi256 => {
            let channel = if foreground { 38 } else { 48 };
            format!("\x1b[{channel};5;{}m", rgb_to_ansi256(r, g, b))
        }
    })
}

fn paint(name: &str, text: &str, foreground: bool, mode: ColorMode) -> String {
    let Some(color) = color_value(name).and_then(|value| ansi_color(&value, foreground, mode))
    else {
        return text.to_string();
    };
    let reset = if foreground { "\x1b[39m" } else { "\x1b[49m" };
    format!("{color}{text}{reset}")
}

/// Wrap text with the terminal's native foreground color mode.
pub fn fg(name: &str, text: impl AsRef<str>) -> String {
    paint(name, text.as_ref(), true, color_mode())
}

/// Wrap text with the terminal's native background color mode.
pub fn bg(name: &str, text: impl AsRef<str>) -> String {
    paint(name, text.as_ref(), false, color_mode())
}

/// Bold text.
pub fn bold(text: impl AsRef<str>) -> String {
    format!("\x1b[1m{}\x1b[22m", text.as_ref())
}

/// Italic text.
pub fn italic(text: impl AsRef<str>) -> String {
    format!("\x1b[3m{}\x1b[23m", text.as_ref())
}

/// Underlined text.
pub fn underline(text: impl AsRef<str>) -> String {
    format!("\x1b[4m{}\x1b[24m", text.as_ref())
}

/// Strikethrough text.
pub fn strikethrough(text: impl AsRef<str>) -> String {
    format!("\x1b[9m{}\x1b[29m", text.as_ref())
}

/// Inverse video.
pub fn inverse(text: impl AsRef<str>) -> String {
    format!("\x1b[7m{}\x1b[27m", text.as_ref())
}

/// Dim text.
pub fn dim(text: impl AsRef<str>) -> String {
    format!("\x1b[2m{}\x1b[22m", text.as_ref())
}

/// Build the Markdown theme from the resolved colors.
pub fn markdown_theme() -> pi_tui::components::markdown::MarkdownTheme {
    use pi_tui::components::markdown::MarkdownTheme;
    // Closures capture `fg`/`bg` helpers by reference via the closure itself.
    MarkdownTheme {
        heading: Box::new(|s| fg("mdHeading", s)),
        link: Box::new(|s| fg("mdLink", s)),
        link_url: Box::new(|s| fg("mdLinkUrl", s)),
        code: Box::new(|s| fg("mdCode", s)),
        code_block: Box::new(|s| fg("mdCodeBlock", s)),
        code_block_border: Box::new(|s| fg("mdCodeBlockBorder", s)),
        quote: Box::new(|s| fg("mdQuote", s)),
        quote_border: Box::new(|s| fg("mdQuoteBorder", s)),
        hr: Box::new(|s| fg("mdHr", s)),
        list_bullet: Box::new(|s| fg("mdListBullet", s)),
        bold: Box::new(|s| format!("\x1b[1m{s}\x1b[22m")),
        italic: Box::new(|s| format!("\x1b[3m{s}\x1b[23m")),
        strikethrough: Box::new(|s| format!("\x1b[9m{s}\x1b[29m")),
        underline: Box::new(|s| format!("\x1b[4m{s}\x1b[24m")),
        highlight_code: None,
        code_block_indent: Some("  ".to_string()),
    }
}

/// Default style for user message text (renders pre-inserted ANSI as-is).
pub fn user_message_style() -> pi_tui::components::markdown::DefaultTextStyle {
    use pi_tui::components::markdown::DefaultTextStyle;
    let color: Box<dyn Fn(&str) -> String + Send + Sync> = Box::new(|s| fg("userMessageText", s));
    DefaultTextStyle {
        color: Some(color),
        bg_color: None,
        bold: false,
        italic: false,
        strikethrough: false,
        underline: false,
    }
}

/// Editor border color function.
pub fn editor_border() -> std::sync::Arc<dyn Fn(&str) -> String + Send + Sync> {
    std::sync::Arc::new(|s| fg("borderMuted", s))
}

/// Return the editor border color used after Pi applies the active thinking
/// level. The constructor starts with `borderMuted`; interactive mode then
/// replaces it with this level-specific color before the first frame.
pub fn thinking_border(level: &str) -> std::sync::Arc<dyn Fn(&str) -> String + Send + Sync> {
    let token = match level {
        "minimal" => "thinkingMinimal",
        "low" => "thinkingLow",
        "medium" => "thinkingMedium",
        "high" => "thinkingHigh",
        "xhigh" => "thinkingXhigh",
        "max" => "thinkingMax",
        "off" => "thinkingOff",
        _ => "thinkingOff",
    };
    let token = token.to_string();
    std::sync::Arc::new(move |s| fg(&token, s))
}

/// Return the editor border color used by Pi's bash mode.
pub fn bash_mode_border() -> std::sync::Arc<dyn Fn(&str) -> String + Send + Sync> {
    std::sync::Arc::new(|s| fg("bashMode", s))
}

/// Default style color fn for assistant thinking text.
pub fn thinking_color() -> Box<dyn Fn(&str) -> String + Send + Sync> {
    Box::new(|s| fg("thinkingText", s))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn expected_paint(value: &str, foreground: bool) -> String {
        let prefix = ansi_color(value, foreground, color_mode()).unwrap();
        let reset = if foreground { "\x1b[39m" } else { "\x1b[49m" };
        format!("{prefix}x{reset}")
    }

    fn test_theme_dir() -> PathBuf {
        std::env::temp_dir().join(format!("pi-tui-theme-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn registered_extension_theme_activation_updates_colors_and_name() {
        let _lock = crate::theme::test_theme_registry_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let dir = test_theme_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("extension.json");
        let name = format!("tui-extension-theme-{}", uuid::Uuid::new_v4());
        let mut value: serde_json::Value =
            serde_json::from_str(include_str!("../../data/themes/dark.json")).unwrap();
        value["name"] = serde_json::Value::String(name.clone());
        value["colors"]["accent"] = serde_json::Value::String("#123456".to_string());
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

        crate::theme::register_theme_paths(&[path.to_string_lossy().into_owned()], Path::new("."));
        load_theme(&name);

        assert_eq!(active_theme_name().as_deref(), Some(name.as_str()));
        assert_eq!(fg("accent", "x"), expected_paint("#123456", true));

        crate::theme::register_theme_paths(&[], Path::new("."));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn markdown_theme_uses_upstream_theme_tokens_and_invalid_names_fall_back() {
        load_theme(crate::theme::DEFAULT_THEME);
        let markdown = markdown_theme();
        assert_eq!((markdown.heading)("x"), expected_paint("#f0c674", true));
        assert_eq!(editor_border()("x"), expected_paint("#505050", true));
        assert_eq!(thinking_border("off")("x"), expected_paint("#505050", true));
        assert_eq!(
            thinking_border("medium")("x"),
            expected_paint("#81a2be", true)
        );
        assert_eq!(
            thinking_border("high")("x"),
            expected_paint("#b294bb", true)
        );
        assert_eq!(
            thinking_border("xhigh")("x"),
            expected_paint("#d183e8", true)
        );
        assert_eq!(thinking_border("max")("x"), expected_paint("#ff5fff", true));
        assert_eq!(bash_mode_border()("x"), expected_paint("#b5bd68", true));

        load_theme("missing-theme-for-fallback");
        assert_eq!(
            active_theme_name().as_deref(),
            Some(crate::theme::DEFAULT_THEME)
        );
    }

    #[test]
    fn theme_change_callbacks_fire_after_activation() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_callback = Arc::clone(&calls);
        on_theme_change(Arc::new(move || {
            calls_for_callback.fetch_add(1, Ordering::Relaxed);
        }));
        let before = calls.load(Ordering::Relaxed);
        load_theme(crate::theme::DEFAULT_THEME);
        assert!(calls.load(Ordering::Relaxed) > before);
    }

    #[test]
    fn theme_watcher_reloads_in_place_without_dropping_sibling_themes() {
        use std::thread;
        use std::time::{Duration, Instant};

        let _lock = crate::theme::test_theme_registry_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let dir = test_theme_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let first_path = dir.join("first.json");
        let second_path = dir.join("second.json");
        let first_name = format!("tui-watched-theme-{}", uuid::Uuid::new_v4());
        let second_name = format!("tui-sibling-theme-{}", uuid::Uuid::new_v4());

        let mut first: serde_json::Value =
            serde_json::from_str(include_str!("../../data/themes/dark.json")).unwrap();
        first["name"] = serde_json::Value::String(first_name.clone());
        first["colors"]["accent"] = serde_json::Value::String("#112233".to_string());
        let mut second = first.clone();
        second["name"] = serde_json::Value::String(second_name.clone());
        std::fs::write(&first_path, serde_json::to_vec(&first).unwrap()).unwrap();
        std::fs::write(&second_path, serde_json::to_vec(&second).unwrap()).unwrap();

        crate::theme::register_theme_paths(
            &[
                first_path.to_string_lossy().into_owned(),
                second_path.to_string_lossy().into_owned(),
            ],
            std::path::Path::new("."),
        );
        load_theme(&first_name);
        watch_theme_file(first_path.clone(), first_name.clone());
        // Let the polling watcher capture the original mtime before the
        // fixture writes the changed theme.
        thread::sleep(Duration::from_millis(200));

        first["colors"]["accent"] = serde_json::Value::String("#aabbcc".to_string());
        std::fs::write(&first_path, serde_json::to_vec(&first).unwrap()).unwrap();
        let expected = expected_paint("#aabbcc", true);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && fg("accent", "x") != expected {
            thread::sleep(Duration::from_millis(25));
        }

        assert_eq!(fg("accent", "x"), expected);
        assert!(crate::theme::registered_theme_infos()
            .iter()
            .any(|theme| theme.name == second_name));
        stop_theme_watcher();
        crate::theme::register_theme_paths(&[], std::path::Path::new("."));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ansi_color_matches_upstream_truecolor_and_ansi256_modes() {
        assert_eq!(
            ansi_color("#123456", true, ColorMode::Truecolor),
            Some("\x1b[38;2;18;52;86m".to_string())
        );
        assert_eq!(
            ansi_color("#123456", true, ColorMode::Ansi256),
            Some("\x1b[38;5;23m".to_string())
        );
        assert_eq!(
            ansi_color("#8abeb7", false, ColorMode::Ansi256),
            Some("\x1b[48;5;109m".to_string())
        );
        assert_eq!(
            ansi_color("#808080", true, ColorMode::Ansi256),
            Some("\x1b[38;5;244m".to_string())
        );
        assert_eq!(
            ansi_color("", true, ColorMode::Ansi256),
            Some("\x1b[39m".to_string())
        );
        assert_eq!(
            ansi_color("", false, ColorMode::Ansi256),
            Some("\x1b[49m".to_string())
        );
    }
}
