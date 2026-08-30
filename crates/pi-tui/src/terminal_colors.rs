//! Terminal color parsing — port of `packages/tui/src/terminal-colors.ts`.
//!
//! Parses OSC 11 background-color responses and the DECRQM-style terminal
//! color scheme report (`CSI ? 997 ; N n`).

/// An RGB color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalColorScheme {
    Dark,
    Light,
}

fn hex_to_rgb(hex: &str) -> RgbColor {
    let normalized = hex.strip_prefix('#').unwrap_or(hex);
    let r = u8::from_str_radix(&normalized[0..2], 16).unwrap_or(0);
    let g = u8::from_str_radix(&normalized[2..4], 16).unwrap_or(0);
    let b = u8::from_str_radix(&normalized[4..6], 16).unwrap_or(0);
    RgbColor { r, g, b }
}

fn parse_osc_hex_channel(channel: &str) -> Option<u8> {
    if channel.is_empty() || !channel.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let max = 16u64.pow(channel.len() as u32).saturating_sub(1);
    if max == 0 {
        return None;
    }
    let value = u64::from_str_radix(channel, 16).ok()?;
    Some((((value as f64) / (max as f64)) * 255.0).round() as u8)
}

fn parse_rgb_from_osc_value(value: &str) -> Option<RgbColor> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(hex_to_rgb(value));
        }
        if hex.len() == 12 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            let r = parse_osc_hex_channel(&hex[0..4]);
            let g = parse_osc_hex_channel(&hex[4..8]);
            let b = parse_osc_hex_channel(&hex[8..12]);
            return match (r, g, b) {
                (Some(r), Some(g), Some(b)) => Some(RgbColor { r, g, b }),
                _ => None,
            };
        }
        return None;
    }

    let rgb_value = value
        .strip_prefix("rgba:")
        .or_else(|| value.strip_prefix("rgb:"))
        .unwrap_or(value);
    let parts: Vec<&str> = rgb_value.split('/').collect();
    if parts.len() != 3 {
        return None;
    }
    let r = parse_osc_hex_channel(parts[0]);
    let g = parse_osc_hex_channel(parts[1]);
    let b = parse_osc_hex_channel(parts[2]);
    match (r, g, b) {
        (Some(r), Some(g), Some(b)) => Some(RgbColor { r, g, b }),
        _ => None,
    }
}

fn parse_osc11_payload(data: &str) -> Option<&str> {
    // Strict format: starts with ESC ] 11; and ends with BEL or ST (ESC \).
    let rest = data.strip_prefix("\x1b]11;")?;
    let value = rest
        .strip_suffix('\x07')
        .or_else(|| rest.strip_suffix("\x1b\\"))?;
    // No internal ESC/BEL allowed (strict match).
    if value.contains('\x1b') || value.contains('\x07') {
        return None;
    }
    Some(value)
}

/// True when `data` is a strict OSC 11 background color response.
/// E.g. `\x1b]11;#ffffff\x07` or `\x1b]11;rgb:0000/8000/ffff\x1b\`.
pub fn is_osc11_background_color_response(data: &str) -> bool {
    parse_osc11_background_color(data).is_some()
}

/// Parse an OSC 11 background-color response into an RGB color.
pub fn parse_osc11_background_color(data: &str) -> Option<RgbColor> {
    let value = parse_osc11_payload(data)?;
    parse_rgb_from_osc_value(value)
}

const COLOR_SCHEME_REPORT_PREFIX: &str = "\x1b[?997;";

/// Parse a terminal color-scheme report (`CSI ? 997 ; 1|2 n`). `1` = dark,
/// `2` = light. Multiple reports may be concatenated; the LAST value wins.
pub fn parse_terminal_color_scheme_report(data: &str) -> Option<TerminalColorScheme> {
    if !data.starts_with(COLOR_SCHEME_REPORT_PREFIX) {
        return None;
    }
    let mut bytes = data.as_bytes();
    let mut scheme: Option<TerminalColorScheme> = None;
    while let Some(rest) = bytes.strip_prefix(COLOR_SCHEME_REPORT_PREFIX.as_bytes()) {
        bytes = rest;
        let val = match bytes.first() {
            Some(b'1') => TerminalColorScheme::Dark,
            Some(b'2') => TerminalColorScheme::Light,
            _ => return None,
        };
        bytes = &bytes[1..];
        if bytes.first() != Some(&b'n') {
            return None;
        }
        bytes = &bytes[1..];
        scheme = Some(val);
    }
    if bytes.is_empty() && scheme.is_some() {
        scheme
    } else {
        None
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn parses_16bit_osc11_rgb_responses() {
        assert_eq!(
            parse_osc11_background_color("\x1b]11;rgb:0000/8000/ffff\x07"),
            Some(RgbColor {
                r: 0,
                g: 128,
                b: 255
            })
        );
    }

    #[test]
    fn parses_osc11_hex_responses() {
        assert_eq!(
            parse_osc11_background_color("\x1b]11;#ffffff\x1b\\"),
            Some(RgbColor {
                r: 255,
                g: 255,
                b: 255
            })
        );
        assert_eq!(
            parse_osc11_background_color("\x1b]11;#000000\x07"),
            Some(RgbColor { r: 0, g: 0, b: 0 })
        );
    }

    #[test]
    fn rejects_non_strict_osc11_responses() {
        assert_eq!(parse_osc11_background_color("x\x1b]11;#ffffff\x07"), None);
        assert_eq!(parse_osc11_background_color("\x1b]10;#ffffff\x07"), None);
        assert_eq!(parse_osc11_background_color("\x1b]11;#ffffff\x07x"), None);
    }

    #[test]
    fn parses_color_scheme_reports() {
        assert_eq!(
            parse_terminal_color_scheme_report("\x1b[?997;1n"),
            Some(TerminalColorScheme::Dark)
        );
        assert_eq!(
            parse_terminal_color_scheme_report("\x1b[?997;2n"),
            Some(TerminalColorScheme::Light)
        );
        assert_eq!(
            parse_terminal_color_scheme_report("\x1b[?997;2n\x1b[?997;1n\x1b[?997;1n"),
            Some(TerminalColorScheme::Dark)
        );
        assert_eq!(
            parse_terminal_color_scheme_report("\x1b[?997;1n\x1b[?997;2n\x1b[?997;2n"),
            Some(TerminalColorScheme::Light)
        );
        assert_eq!(parse_terminal_color_scheme_report("\x1b[?997;3n"), None);
        assert_eq!(parse_terminal_color_scheme_report("\x1b[?996n"), None);
        assert_eq!(parse_terminal_color_scheme_report("x\x1b[?997;1n"), None);
    }
}
