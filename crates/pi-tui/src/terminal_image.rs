//! Terminal image capabilities — port of `packages/tui/src/terminal-image.ts`
//! (the parts the markdown renderer and Image component use).

use std::process::{Command, Stdio};
use std::sync::RwLock;
use std::thread;
use std::time::{Duration, Instant};

/// Image protocol support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProtocol {
    Kitty,
    ITerm2,
}

/// Terminal capabilities for images, truecolor, hyperlinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCapabilities {
    pub images: Option<ImageProtocol>,
    pub true_color: bool,
    pub hyperlinks: bool,
}

static CAPS: RwLock<Option<TerminalCapabilities>> = RwLock::new(None);
static CELL_DIMENSIONS: RwLock<(u32, u32)> = RwLock::new((9, 18));

/// The environment inputs used by terminal capability detection.
///
/// Keeping the matrix separate from process-global environment reads makes
/// terminal support testable without racing other tests or mutating the
/// caller's environment.  `from_process` is the only place that reads
/// environment variables for the normal runtime path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilityEnvironment {
    pub term_program: Option<String>,
    pub terminal_emulator: Option<String>,
    pub term: Option<String>,
    pub color_term: Option<String>,
    pub kitty_window_id: bool,
    pub ghostty_resources_dir: bool,
    pub wezterm_pane: bool,
    pub warp_session_id: bool,
    pub warp_terminal_session_uuid: bool,
    pub iterm_session_id: bool,
    pub wt_session: bool,
    pub tmux: bool,
    pub is_windows: bool,
}

impl CapabilityEnvironment {
    fn from_process() -> Self {
        fn value(name: &str) -> Option<String> {
            std::env::var(name).ok().map(|value| value.to_lowercase())
        }
        Self {
            term_program: value("TERM_PROGRAM"),
            terminal_emulator: value("TERMINAL_EMULATOR"),
            term: value("TERM"),
            color_term: value("COLORTERM"),
            kitty_window_id: std::env::var_os("KITTY_WINDOW_ID").is_some(),
            ghostty_resources_dir: std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some(),
            wezterm_pane: std::env::var_os("WEZTERM_PANE").is_some(),
            warp_session_id: std::env::var_os("WARP_SESSION_ID").is_some(),
            warp_terminal_session_uuid: std::env::var_os("WARP_TERMINAL_SESSION_UUID").is_some(),
            iterm_session_id: std::env::var_os("ITERM_SESSION_ID").is_some(),
            wt_session: std::env::var_os("WT_SESSION").is_some(),
            tmux: std::env::var_os("TMUX").is_some(),
            is_windows: cfg!(windows),
        }
    }
}

/// Detect capabilities from the environment.
pub fn detect_capabilities() -> TerminalCapabilities {
    detect_capabilities_with_tmux_probe(probe_tmux_hyperlinks)
}

/// Detect capabilities while allowing callers/tests to supply the tmux probe.
pub fn detect_capabilities_with_tmux_probe<F>(tmux_forwards_hyperlink: F) -> TerminalCapabilities
where
    F: FnOnce() -> bool,
{
    detect_capabilities_for_environment(
        &CapabilityEnvironment::from_process(),
        tmux_forwards_hyperlink,
    )
}

/// Detect capabilities for an explicit environment matrix.
///
/// This is public so downstream fixture tests can verify a terminal family
/// without changing process environment variables.  The precedence mirrors
/// the upstream detector: tmux and screen are transport layers and therefore
/// take precedence over the outer terminal's image protocol.
pub fn detect_capabilities_for_environment<F>(
    environment: &CapabilityEnvironment,
    tmux_forwards_hyperlink: F,
) -> TerminalCapabilities
where
    F: FnOnce() -> bool,
{
    let term_program = environment.term_program.as_deref().unwrap_or_default();
    let term_emulator = environment.terminal_emulator.as_deref().unwrap_or_default();
    let term = environment.term.as_deref().unwrap_or_default();
    let color_term = environment.color_term.as_deref().unwrap_or_default();
    let has_true_color = color_term == "truecolor" || color_term == "24bit";

    let tmux = environment.tmux || term.starts_with("tmux");
    if tmux {
        return TerminalCapabilities {
            images: None,
            true_color: has_true_color,
            hyperlinks: tmux_forwards_hyperlink(),
        };
    }
    if term.starts_with("screen") {
        return TerminalCapabilities {
            images: None,
            true_color: has_true_color,
            hyperlinks: false,
        };
    }
    if environment.kitty_window_id
        || term_program == "kitty"
        || term_program == "ghostty"
        || term.contains("ghostty")
        || environment.ghostty_resources_dir
        || environment.wezterm_pane
        || term_program == "wezterm"
        || term_program == "warpterminal"
        || environment.warp_session_id
        || environment.warp_terminal_session_uuid
    {
        return TerminalCapabilities {
            images: Some(ImageProtocol::Kitty),
            true_color: true,
            hyperlinks: true,
        };
    }
    if environment.iterm_session_id || term_program == "iterm.app" {
        return TerminalCapabilities {
            images: Some(ImageProtocol::ITerm2),
            true_color: true,
            hyperlinks: true,
        };
    }
    if environment.wt_session || term_program == "vscode" || term_program == "alacritty" {
        return TerminalCapabilities {
            images: None,
            true_color: true,
            hyperlinks: true,
        };
    }
    if term_emulator == "jetbrains-jediterm" {
        return TerminalCapabilities {
            images: None,
            true_color: true,
            hyperlinks: false,
        };
    }
    if environment.is_windows {
        return TerminalCapabilities {
            images: None,
            true_color: true,
            hyperlinks: false,
        };
    }
    TerminalCapabilities {
        images: None,
        true_color: has_true_color,
        hyperlinks: false,
    }
}

/// Check whether the attached tmux client advertises OSC 8 hyperlink
/// forwarding. tmux strips hyperlinks unless `client_termfeatures` includes
/// `hyperlinks`; failures and timeouts conservatively return `false`.
pub fn probe_tmux_hyperlinks() -> bool {
    let mut child = match Command::new("tmux")
        .args(["display-message", "-p", "#{client_termfeatures}"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };

    let deadline = Instant::now() + Duration::from_millis(250);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = match child.wait_with_output() {
                    Ok(output) => output,
                    Err(_) => return false,
                };
                return status.success()
                    && parse_tmux_client_termfeatures(&String::from_utf8_lossy(&output.stdout));
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Parse the comma-separated `client_termfeatures` value returned by tmux.
pub fn parse_tmux_client_termfeatures(termfeatures: &str) -> bool {
    termfeatures
        .split(',')
        .map(str::trim)
        .any(|feature| feature == "hyperlinks")
}

pub fn get_capabilities() -> TerminalCapabilities {
    cached_capabilities(&CAPS, detect_capabilities)
}

/// Read the capability cache, detect on a miss, and publish the result.
///
/// The explicit read-guard drop is important: a cache miss must release the
/// read lock before acquiring the write lock, including on the first TTY
/// render in a fresh process.
fn cached_capabilities<F>(
    cache: &RwLock<Option<TerminalCapabilities>>,
    detect: F,
) -> TerminalCapabilities
where
    F: FnOnce() -> TerminalCapabilities,
{
    let cached = cache.read().unwrap_or_else(|e| e.into_inner());
    if let Some(caps) = *cached {
        return caps;
    }
    drop(cached);

    let caps = detect();
    let mut guard = cache.write().unwrap_or_else(|e| e.into_inner());
    *guard = Some(caps);
    caps
}

pub fn set_capabilities(caps: TerminalCapabilities) {
    let mut guard = CAPS.write().unwrap_or_else(|e| e.into_inner());
    *guard = Some(caps);
}

/// Clear the process capability cache so a later call re-runs detection.
/// This is primarily useful for deterministic fixture tests and parity with
/// the upstream `resetCapabilitiesCache` helper.
pub fn reset_capabilities_cache() {
    let mut guard = CAPS.write().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

/// Allocate a non-zero Kitty image id without relying on a platform RNG API.
/// A monotonic id is sufficient for the TUI's lifetime and deterministic in
/// tests; callers may still provide an explicit id to `encode_kitty`.
pub fn allocate_image_id() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT_IMAGE_ID: AtomicU32 = AtomicU32::new(1);
    let id = NEXT_IMAGE_ID.fetch_add(1, Ordering::Relaxed);
    if id == 0 {
        NEXT_IMAGE_ID.store(2, Ordering::Relaxed);
        1
    } else {
        id
    }
}

/// Delete one Kitty image while retaining the terminal's other placements.
pub fn delete_kitty_image(image_id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\")
}

/// Delete all Kitty placements and their uploaded image data.
pub fn delete_all_kitty_images() -> &'static str {
    "\x1b_Ga=d,d=A,q=2\x1b\\"
}

/// Wrap text in an OSC 8 hyperlink sequence.
pub fn hyperlink(text: &str, url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// Whether a rendered line contains a kitty/iTerm2 inline image sequence.
pub fn is_image_line(line: &str) -> bool {
    line.starts_with("\x1b_G")
        || line.starts_with("\x1b]1337;File=")
        || line.contains("\x1b_G")
        || line.contains("\x1b]1337;File=")
}

/// Default cell dimensions (used by image sizing).
pub fn get_cell_dimensions() -> (u32, u32) {
    *CELL_DIMENSIONS
        .read()
        .unwrap_or_else(|error| error.into_inner())
}

/// Update measured terminal cell dimensions in pixels. The TUI calls this
/// when it receives the terminal's `CSI 6;height;width t` response.
pub fn set_cell_dimensions(width_px: u32, height_px: u32) {
    if width_px == 0 || height_px == 0 {
        return;
    }
    *CELL_DIMENSIONS
        .write()
        .unwrap_or_else(|error| error.into_inner()) = (width_px, height_px);
}

/// Parse the terminal response to a `CSI 16 t` cell-size query.
///
/// Terminals report this as `CSI 6 ; height ; width t`. Keep the parser
/// deliberately strict: a malformed response must remain available to the
/// normal key/input path rather than changing image sizing unexpectedly.
pub fn parse_cell_size_response(data: &str) -> Option<(u32, u32)> {
    let body = data.strip_prefix("\x1b[6;")?.strip_suffix('t')?;
    let mut fields = body.split(';');
    let height = fields.next()?.parse::<u32>().ok()?;
    let width = fields.next()?.parse::<u32>().ok()?;
    if fields.next().is_some() || height == 0 || width == 0 {
        return None;
    }
    Some((width, height))
}

/// Decode base64 into bytes (used by image dimension parsing).
fn decode_base64(data: &str) -> Option<Vec<u8>> {
    // Minimal base64 decoder (standard alphabet, no whitespace).
    let mut out = Vec::new();
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for &b in data.as_bytes() {
        let val = match b {
            b'A'..=b'Z' => (b - b'A') as u32,
            b'a'..=b'z' => (b - b'a' + 26) as u32,
            b'0'..=b'9' => (b - b'0' + 52) as u32,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// Parse PNG dimensions from base64 data (port of `getPngDimensions`).
pub fn get_png_dimensions(base64_data: &str) -> Option<(u32, u32)> {
    let bytes = decode_base64(base64_data)?;
    if bytes.len() < 24 {
        return None;
    }
    if bytes[0] != 0x89 || bytes[1] != 0x50 || bytes[2] != 0x4e || bytes[3] != 0x47 {
        return None;
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some((width, height))
}

/// Parse JPEG dimensions from base64 (upstream `getJpegDimensions`).
pub fn get_jpeg_dimensions(base64_data: &str) -> Option<(u32, u32)> {
    let bytes = decode_base64(base64_data)?;
    if bytes.len() < 2 || bytes[0] != 0xff || bytes[1] != 0xd8 {
        return None;
    }
    let mut offset = 2usize;
    while offset + 9 < bytes.len() {
        if bytes[offset] != 0xff {
            offset += 1;
            continue;
        }
        let marker = bytes[offset + 1];
        if (0xc0..=0xc2).contains(&marker) {
            let height = u16::from_be_bytes([bytes[offset + 5], bytes[offset + 6]]) as u32;
            let width = u16::from_be_bytes([bytes[offset + 7], bytes[offset + 8]]) as u32;
            return Some((width, height));
        }
        if offset + 3 >= bytes.len() {
            return None;
        }
        let length = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
        if length < 2 {
            return None;
        }
        offset += 2 + length;
    }
    None
}

/// Parse GIF dimensions from base64 (upstream `getGifDimensions`).
pub fn get_gif_dimensions(base64_data: &str) -> Option<(u32, u32)> {
    let bytes = decode_base64(base64_data)?;
    if bytes.len() < 10 {
        return None;
    }
    let sig = std::str::from_utf8(&bytes[..6]).ok()?;
    if sig != "GIF87a" && sig != "GIF89a" {
        return None;
    }
    let width = u16::from_le_bytes([bytes[6], bytes[7]]) as u32;
    let height = u16::from_le_bytes([bytes[8], bytes[9]]) as u32;
    Some((width, height))
}

/// Parse WebP dimensions from base64 (upstream `getWebpDimensions`).
pub fn get_webp_dimensions(base64_data: &str) -> Option<(u32, u32)> {
    let bytes = decode_base64(base64_data)?;
    if bytes.len() < 30 {
        return None;
    }
    if &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }
    let chunk = &bytes[12..16];
    match chunk {
        b"VP8 " => {
            if bytes.len() < 30 {
                return None;
            }
            let width = (u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3fff) as u32;
            let height = (u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3fff) as u32;
            Some((width, height))
        }
        b"VP8L" => {
            if bytes.len() < 25 {
                return None;
            }
            let bits = u32::from_le_bytes([bytes[21], bytes[22], bytes[23], bytes[24]]);
            let width = (bits & 0x3fff) + 1;
            let height = ((bits >> 14) & 0x3fff) + 1;
            Some((width, height))
        }
        b"VP8X" => {
            if bytes.len() < 30 {
                return None;
            }
            let width =
                ((bytes[24] as u32) | ((bytes[25] as u32) << 8) | ((bytes[26] as u32) << 16)) + 1;
            let height =
                ((bytes[27] as u32) | ((bytes[28] as u32) << 8) | ((bytes[29] as u32) << 16)) + 1;
            Some((width, height))
        }
        _ => None,
    }
}

/// Get dimensions from base64 for a mime type.
pub fn get_image_dimensions(base64_data: &str, mime_type: &str) -> Option<(u32, u32)> {
    match mime_type {
        "image/png" => get_png_dimensions(base64_data),
        "image/jpeg" => get_jpeg_dimensions(base64_data),
        "image/gif" => get_gif_dimensions(base64_data),
        "image/webp" => get_webp_dimensions(base64_data),
        _ => None,
    }
}

/// Text fallback when the terminal cannot render inline images.
pub fn image_fallback(
    mime_type: &str,
    dimensions: Option<(u32, u32)>,
    filename: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(filename) = filename {
        let home = std::env::var("HOME").unwrap_or_default();
        let display = if !home.is_empty()
            && (filename == home || filename.starts_with(&format!("{home}/")))
        {
            format!("~{}", filename.trim_start_matches(&home))
        } else {
            filename.to_string()
        };
        parts.push(display);
    }
    parts.push(format!("[{mime_type}]"));
    if let Some((w, h)) = dimensions {
        parts.push(format!("{w}x{h}"));
    }
    format!("[Image: {}]", parts.join(" "))
}

pub fn encode_kitty(
    base64_data: &str,
    columns: usize,
    rows: usize,
    image_id: Option<u32>,
    move_cursor: bool,
) -> String {
    const CHUNK_SIZE: usize = 4096;
    let mut params = vec!["a=T".to_string(), "f=100".to_string(), "q=2".to_string()];
    if !move_cursor {
        params.push("C=1".to_string());
    }
    if columns > 0 {
        params.push(format!("c={columns}"));
    }
    if rows > 0 {
        params.push(format!("r={rows}"));
    }
    if let Some(id) = image_id {
        params.push(format!("i={id}"));
    }
    let joined = params.join(",");
    if base64_data.len() <= CHUNK_SIZE {
        return format!("\x1b_G{joined};{base64_data}\x1b\\");
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut offset = 0usize;
    let mut is_first = true;
    while offset < base64_data.len() {
        let chunk = &base64_data[offset..(offset + CHUNK_SIZE).min(base64_data.len())];
        let is_last = offset + CHUNK_SIZE >= base64_data.len();
        if is_first {
            chunks.push(format!("\x1b_G{joined},m=1;{chunk}\x1b\\"));
            is_first = false;
        } else if is_last {
            chunks.push(format!("\x1b_Gm=0;{chunk}\x1b\\"));
        } else {
            chunks.push(format!("\x1b_Gm=1;{chunk}\x1b\\"));
        }
        offset += CHUNK_SIZE;
    }
    chunks.join("")
}

pub fn encode_iterm2(base64_data: &str, columns: usize, preserve_aspect_ratio: bool) -> String {
    // iTerm2's `size` is the decoded byte count, not the base64 character
    // count.  The latter is only equal for a few accidental inputs.
    let size = decode_base64(base64_data)
        .map(|bytes| bytes.len())
        .unwrap_or_else(|| base64_data.len());
    let mut params = vec!["inline=1".to_string(), format!("size={size}")];
    if columns > 0 {
        params.push(format!("width={columns}"));
    }
    params.push("height=auto".to_string());
    if !preserve_aspect_ratio {
        params.push("preserveAspectRatio=0".to_string());
    }
    format!("\x1b]1337;File={};{base64_data}\x07", params.join(";"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tmux_client_termfeatures() {
        assert!(parse_tmux_client_termfeatures("hyperlinks,RGB,usstyle"));
        assert!(parse_tmux_client_termfeatures(" RGB , hyperlinks "));
        assert!(!parse_tmux_client_termfeatures("RGB,usstyle"));
        assert!(!parse_tmux_client_termfeatures(""));
    }

    #[test]
    fn parses_cell_size_response_height_then_width() {
        assert_eq!(parse_cell_size_response("\x1b[6;18;9t"), Some((9, 18)));
        assert_eq!(parse_cell_size_response("\x1b[6;18;9"), None);
        assert_eq!(parse_cell_size_response("\x1b[6;0;9t"), None);
        assert_eq!(parse_cell_size_response("\x1b[6;18;9;1t"), None);
    }

    #[test]
    fn cell_dimensions_are_updated_only_with_positive_values() {
        let original = get_cell_dimensions();
        set_cell_dimensions(11, 22);
        assert_eq!(get_cell_dimensions(), (11, 22));
        set_cell_dimensions(0, 0);
        assert_eq!(get_cell_dimensions(), (11, 22));
        set_cell_dimensions(original.0, original.1);
    }

    #[test]
    fn uncached_capability_detection_releases_read_lock_before_write() {
        let cache = RwLock::new(None);
        let expected = TerminalCapabilities {
            images: None,
            true_color: true,
            hyperlinks: false,
        };

        let actual = cached_capabilities(&cache, || expected);
        assert!(actual.true_color);
        assert!(cache.read().unwrap().is_some());
    }

    #[test]
    fn capability_matrix_covers_named_terminal_families_and_fallback() {
        let cases = [
            (
                "Kitty",
                CapabilityEnvironment {
                    kitty_window_id: true,
                    ..Default::default()
                },
                TerminalCapabilities {
                    images: Some(ImageProtocol::Kitty),
                    true_color: true,
                    hyperlinks: true,
                },
            ),
            (
                "Ghostty",
                CapabilityEnvironment {
                    term_program: Some("ghostty".into()),
                    ..Default::default()
                },
                TerminalCapabilities {
                    images: Some(ImageProtocol::Kitty),
                    true_color: true,
                    hyperlinks: true,
                },
            ),
            (
                "WezTerm",
                CapabilityEnvironment {
                    wezterm_pane: true,
                    ..Default::default()
                },
                TerminalCapabilities {
                    images: Some(ImageProtocol::Kitty),
                    true_color: true,
                    hyperlinks: true,
                },
            ),
            (
                "Warp",
                CapabilityEnvironment {
                    term_program: Some("warpterminal".into()),
                    ..Default::default()
                },
                TerminalCapabilities {
                    images: Some(ImageProtocol::Kitty),
                    true_color: true,
                    hyperlinks: true,
                },
            ),
            (
                "iTerm2",
                CapabilityEnvironment {
                    iterm_session_id: true,
                    ..Default::default()
                },
                TerminalCapabilities {
                    images: Some(ImageProtocol::ITerm2),
                    true_color: true,
                    hyperlinks: true,
                },
            ),
            (
                "VS Code",
                CapabilityEnvironment {
                    term_program: Some("vscode".into()),
                    ..Default::default()
                },
                TerminalCapabilities {
                    images: None,
                    true_color: true,
                    hyperlinks: true,
                },
            ),
            (
                "Alacritty",
                CapabilityEnvironment {
                    term_program: Some("alacritty".into()),
                    ..Default::default()
                },
                TerminalCapabilities {
                    images: None,
                    true_color: true,
                    hyperlinks: true,
                },
            ),
            (
                "JetBrains",
                CapabilityEnvironment {
                    terminal_emulator: Some("jetbrains-jediterm".into()),
                    ..Default::default()
                },
                TerminalCapabilities {
                    images: None,
                    true_color: true,
                    hyperlinks: false,
                },
            ),
            (
                "screen",
                CapabilityEnvironment {
                    term: Some("screen-256color".into()),
                    color_term: Some("truecolor".into()),
                    ..Default::default()
                },
                TerminalCapabilities {
                    images: None,
                    true_color: true,
                    hyperlinks: false,
                },
            ),
            (
                "Windows Terminal",
                CapabilityEnvironment {
                    wt_session: true,
                    ..Default::default()
                },
                TerminalCapabilities {
                    images: None,
                    true_color: true,
                    hyperlinks: true,
                },
            ),
            (
                "unknown with truecolor",
                CapabilityEnvironment {
                    color_term: Some("24bit".into()),
                    ..Default::default()
                },
                TerminalCapabilities {
                    images: None,
                    true_color: true,
                    hyperlinks: false,
                },
            ),
        ];

        for (name, environment, expected) in cases {
            let actual = detect_capabilities_for_environment(&environment, || false);
            assert_eq!(actual, expected, "capability mismatch for {name}");
        }
    }

    #[test]
    fn tmux_transport_wins_over_outer_terminal_and_probe_controls_links() {
        let environment = CapabilityEnvironment {
            term_program: Some("kitty".into()),
            term: Some("tmux-256color".into()),
            tmux: true,
            ..Default::default()
        };
        assert_eq!(
            detect_capabilities_for_environment(&environment, || true),
            TerminalCapabilities {
                images: None,
                true_color: false,
                hyperlinks: true,
            }
        );
        assert!(!detect_capabilities_for_environment(&environment, || false).hyperlinks);
    }

    #[test]
    fn iterm2_size_uses_decoded_payload_bytes() {
        let encoded = encode_iterm2("SGVsbG8=", 0, true);
        assert!(encoded.contains("size=5"));
        assert!(!encoded.contains("size=8"));
    }
}
