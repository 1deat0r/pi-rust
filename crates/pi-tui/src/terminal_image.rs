//! Terminal image capabilities — port of `packages/tui/src/terminal-image.ts`
//! (the parts the markdown renderer and Image component use).

use std::sync::RwLock;

/// Image protocol support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProtocol {
    Kitty,
    ITerm2,
}

/// Terminal capabilities for images, truecolor, hyperlinks.
#[derive(Debug, Clone, Copy)]
pub struct TerminalCapabilities {
    pub images: Option<ImageProtocol>,
    pub true_color: bool,
    pub hyperlinks: bool,
}

static CAPS: RwLock<Option<TerminalCapabilities>> = RwLock::new(None);

/// Detect capabilities from the environment (documented divergence: upstream
/// additionally probes tmux client_termfeatures for OSC 8 forwarding; the
/// port keys off the same env vars with conservative fallbacks).
pub fn detect_capabilities() -> TerminalCapabilities {
    fn get(name: &str) -> Option<String> {
        std::env::var(name).ok().map(|v| v.to_lowercase())
    }
    let term_program = get("TERM_PROGRAM").unwrap_or_default();
    let term_emulator = get("TERMINAL_EMULATOR").unwrap_or_default();
    let term = get("TERM").unwrap_or_default();
    let color_term = get("COLORTERM").unwrap_or_default();
    let has_true_color = color_term == "truecolor" || color_term == "24bit";

    let tmux = std::env::var("TMUX").is_ok() || term.starts_with("tmux");
    if tmux {
        return TerminalCapabilities { images: None, true_color: has_true_color, hyperlinks: false };
    }
    if term.starts_with("screen") {
        return TerminalCapabilities { images: None, true_color: has_true_color, hyperlinks: false };
    }
    let in_env = |names: &[&str]| names.iter().any(|n| std::env::var(n).is_ok());
    if in_env(&["KITTY_WINDOW_ID"]) || term_program == "kitty" || term_program == "ghostty" || term.contains("ghostty")
        || in_env(&["WEZTERM_PANE"]) || term_program == "wezterm"
        || term_program == "warpterminal" || in_env(&["WARP_SESSION_ID", "WARP_TERMINAL_SESSION_UUID"])
    {
        return TerminalCapabilities { images: Some(ImageProtocol::Kitty), true_color: true, hyperlinks: true };
    }
    if in_env(&["ITERM_SESSION_ID"]) || term_program == "iterm.app" {
        return TerminalCapabilities { images: Some(ImageProtocol::ITerm2), true_color: true, hyperlinks: true };
    }
    if in_env(&["WT_SESSION"]) || term_program == "vscode" || term_program == "alacritty" {
        return TerminalCapabilities { images: None, true_color: true, hyperlinks: true };
    }
    if term_emulator.contains("jetbrains") {
        return TerminalCapabilities { images: None, true_color: true, hyperlinks: false };
    }
    TerminalCapabilities { images: None, true_color: has_true_color, hyperlinks: false }
}

pub fn get_capabilities() -> TerminalCapabilities {
    let cached = CAPS.read().unwrap_or_else(|e| e.into_inner());
    if let Some(caps) = *cached {
        return caps;
    }
    let caps = detect_capabilities();
    set_capabilities(caps);
    caps
}

pub fn set_capabilities(caps: TerminalCapabilities) {
    let mut guard = CAPS.write().unwrap_or_else(|e| e.into_inner());
    *guard = Some(caps);
}

/// Wrap text in an OSC 8 hyperlink sequence.
pub fn hyperlink(text: &str, url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// Whether a rendered line contains a kitty/iTerm2 inline image sequence.
pub fn is_image_line(line: &str) -> bool {
    line.starts_with("\x1b_G") || line.starts_with("\x1b]1337;File=") || line.contains("\x1b_G") || line.contains("\x1b]1337;File=")
}

/// Default cell dimensions (used by image sizing).
pub fn get_cell_dimensions() -> (u32, u32) {
    (9, 18)
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
            let width = ((bytes[24] as u32) | ((bytes[25] as u32) << 8) | ((bytes[26] as u32) << 16)) + 1;
            let height = ((bytes[27] as u32) | ((bytes[28] as u32) << 8) | ((bytes[29] as u32) << 16)) + 1;
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
pub fn image_fallback(mime_type: &str, dimensions: Option<(u32, u32)>, filename: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(filename) = filename {
        let home = std::env::var("HOME").unwrap_or_default();
        let display = if !home.is_empty() && (filename == home || filename.starts_with(&format!("{home}/"))) {
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

pub fn encode_kitty(base64_data: &str, columns: usize, rows: usize, image_id: Option<u32>, move_cursor: bool) -> String {
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
    let size = base64_data.len();
    let mut params = vec![format!("inline=1"), format!("size={size}")];
    if columns > 0 {
        params.push(format!("width={columns}"));
    }
    params.push("height=auto".to_string());
    if !preserve_aspect_ratio {
        params.push("preserveAspectRatio=0".to_string());
    }
    format!("\x1b]1337;File={};{base64_data}\x07", params.join(";"))
}
