//! Keyboard model — port of `packages/tui/src/keys.ts` (the key-string
//! surface pi's interactive mode uses for keybindings).

/// A parsed key: base + modifiers (ctrl/shift/alt/meta).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiKey {
    pub base: String,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

/// Event kind emitted by the Kitty keyboard protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEventType {
    Press,
    Repeat,
    Release,
}

impl TuiKey {
    pub fn simple(base: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            ctrl: false,
            shift: false,
            alt: false,
        }
    }
    pub fn ctrl(base: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            ctrl: true,
            shift: false,
            alt: false,
        }
    }
    pub fn shift(base: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            ctrl: false,
            shift: true,
            alt: false,
        }
    }
    /// Canonical form like `"ctrl+c"`, `"enter"`, `"shift+tab"`.
    pub fn canonical(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.ctrl {
            parts.push("ctrl");
        }
        if self.alt {
            parts.push("alt");
        }
        if self.shift && !self.ctrl && !self.alt {
            parts.push("shift");
        }
        if self.base == "ctrl+c" {
            return "ctrl+c".to_string();
        }
        let base = self.base.clone();
        parts.push(Box::leak(base.into_boxed_str()));
        parts.join("+")
    }
}

/// Match a key string like `"ctrl+c"` against a parsed key (upstream
/// `matchesKey`).
pub fn match_key(key: &TuiKey, pattern: &str) -> bool {
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut base: &str = pattern;
    for part in pattern.split('+') {
        match part {
            "ctrl" => ctrl = true,
            "alt" => alt = true,
            "shift" => shift = true,
            other => base = other,
        }
    }
    let base_match = key.base == base
        || (key.base == "esc" && base == "escape")
        || (key.base == "escape" && base == "esc");
    key.ctrl == ctrl && key.alt == alt && key.shift == shift && base_match
}

/// Parse a raw key string (from the terminal backend) into a key.
pub fn parse_key(raw: &str) -> TuiKey {
    if let Some(key) = parse_raw_terminal_key(raw) {
        return key;
    }
    if raw.starts_with("ctrl+") {
        TuiKey {
            base: raw.trim_start_matches("ctrl+").to_string(),
            ctrl: true,
            shift: false,
            alt: false,
        }
    } else if raw.starts_with("shift+") {
        TuiKey {
            base: raw.trim_start_matches("shift+").to_string(),
            ctrl: false,
            shift: true,
            alt: false,
        }
    } else if raw.starts_with("alt+") {
        TuiKey {
            base: raw.trim_start_matches("alt+").to_string(),
            ctrl: false,
            shift: false,
            alt: true,
        }
    } else {
        TuiKey::simple(raw)
    }
}

fn parse_raw_terminal_key(raw: &str) -> Option<TuiKey> {
    if raw.is_empty() {
        return None;
    }

    // Kitty CSI-u and xterm modifyOtherKeys can carry printable text that is
    // not present as a literal UTF-8 character. Decode it before the legacy
    // CSI parser, while leaving Ctrl/Alt combinations to key matching.
    if let Some(printable) = decode_printable_key(raw) {
        let shift = printable.1 & 1 != 0;
        return Some(with_modifiers(printable.0, false, false, shift));
    }

    match raw {
        "\x1b" => return Some(TuiKey::simple("esc")),
        "\r" | "\n" => return Some(TuiKey::simple("enter")),
        "\t" => return Some(TuiKey::simple("tab")),
        "\x7f" => return Some(TuiKey::simple("backspace")),
        "\0" => return Some(TuiKey::ctrl(" ")),
        _ => {}
    }

    if raw.len() == 1 {
        let byte = raw.as_bytes()[0];
        if (1..=26).contains(&byte) {
            return Some(TuiKey::ctrl(((b'a' + byte - 1) as char).to_string()));
        }
        if (28..=31).contains(&byte) {
            return Some(TuiKey::ctrl(((b'4' + byte - 28) as char).to_string()));
        }
        if let Some(ch) = raw.chars().next() {
            return Some(TuiKey::simple(ch.to_string()));
        }
    }

    let escape_tail = raw.strip_prefix('\x1b')?;

    // Legacy Alt+key input is ESC followed by one printable character.
    if escape_tail.chars().count() == 1 {
        let ch = escape_tail.chars().next()?;
        let base = match ch {
            '\r' => "enter",
            '\t' => "tab",
            ' ' => " ",
            _ => return Some(with_modifiers(ch.to_string(), false, false, true)),
        };
        return Some(with_modifiers(base.to_string(), false, false, true));
    }

    if let Some(ss3) = escape_tail.strip_prefix('O') {
        let key = match ss3 {
            "A" => "up",
            "B" => "down",
            "C" => "right",
            "D" => "left",
            "H" => "home",
            "F" => "end",
            "P" => "f1",
            "Q" => "f2",
            "R" => "f3",
            "S" => "f4",
            _ => return None,
        };
        return Some(TuiKey::simple(key));
    }

    let csi = escape_tail.strip_prefix('[')?;
    let final_byte = csi.as_bytes().last().copied()? as char;
    let parameters = &csi[..csi.len() - final_byte.len_utf8()];

    let (modifier, first_parameter) = parse_csi_parameters(parameters);
    let (ctrl, alt, shift) = csi_modifiers(modifier);

    let base = match final_byte {
        'A' => "up".to_string(),
        'B' => "down".to_string(),
        'C' => "right".to_string(),
        'D' => "left".to_string(),
        'F' => "end".to_string(),
        'H' => "home".to_string(),
        'Z' => "tab".to_string(),
        'P' => "f1".to_string(),
        'Q' => "f2".to_string(),
        'R' => "f3".to_string(),
        'S' => "f4".to_string(),
        '~' => special_csi_key(first_parameter?)?,
        'u' => return parse_csi_u_key(parameters),
        _ => return None,
    };

    let shift = shift || final_byte == 'Z';
    Some(with_modifiers(base, ctrl, alt, shift))
}

fn parse_csi_parameters(parameters: &str) -> (Option<u8>, Option<u16>) {
    let mut fields = parameters.split(';');
    let first = fields.next().and_then(|value| {
        if value.is_empty() || value == "?" {
            None
        } else {
            value.parse().ok()
        }
    });
    let modifier = fields.next().and_then(|value| {
        let value = value.split(':').next().unwrap_or(value);
        value.parse().ok()
    });
    (modifier, first)
}

fn csi_modifiers(modifier: Option<u8>) -> (bool, bool, bool) {
    let mask = modifier.unwrap_or(1).saturating_sub(1);
    (mask & 4 != 0, mask & 2 != 0, mask & 1 != 0)
}

fn special_csi_key(code: u16) -> Option<String> {
    Some(
        match code {
            1 | 7 => "home",
            2 => "insert",
            3 => "delete",
            4 | 8 => "end",
            5 => "pageup",
            6 => "pagedown",
            11..=15 => return Some(format!("f{}", code - 10)),
            17..=21 => return Some(format!("f{}", code - 11)),
            23..=26 => return Some(format!("f{}", code - 12)),
            28..=29 => return Some(format!("f{}", code - 15)),
            31..=34 => return Some(format!("f{}", code - 17)),
            _ => return None,
        }
        .to_string(),
    )
}

fn parse_csi_u_key(parameters: &str) -> Option<TuiKey> {
    let mut fields = parameters.split(';');
    let codepoint = fields.next()?.split(':').next()?.parse::<u32>().ok()?;
    let modifier = fields
        .next()
        .and_then(|value| value.split(':').next().unwrap_or(value).parse().ok());
    let (ctrl, alt, shift) = csi_modifiers(modifier);

    let base = match codepoint {
        0x1b => "esc".to_string(),
        0x0d => "enter".to_string(),
        0x09 => return Some(with_modifiers("tab".to_string(), ctrl, alt, shift)),
        0x7f => "backspace".to_string(),
        57399..=57408 => ((b'0' + (codepoint - 57399) as u8) as char).to_string(),
        57409 => ".".to_string(),
        57410 => "/".to_string(),
        57411 => "*".to_string(),
        57412 => "-".to_string(),
        57413 => "+".to_string(),
        57414 => "enter".to_string(),
        57415 => "=".to_string(),
        57416 => ",".to_string(),
        57417 => "left".to_string(),
        57418 => "right".to_string(),
        57419 => "up".to_string(),
        57420 => "down".to_string(),
        57421 => "pageup".to_string(),
        57422 => "pagedown".to_string(),
        57423 => "home".to_string(),
        57424 => "end".to_string(),
        57425 => "insert".to_string(),
        57426 => "delete".to_string(),
        57376..=57398 => format!("f{}", codepoint - 57363),
        _ => char::from_u32(codepoint)?.to_string(),
    };
    Some(with_modifiers(base, ctrl, alt, shift))
}

/// Return the Kitty CSI-u codepoint, normalized modifier mask, and event.
fn parse_kitty_sequence(raw: &str) -> Option<(u32, Option<u32>, u8, KeyEventType)> {
    let body = raw.strip_prefix("\x1b[")?.strip_suffix('u')?;
    let (codepoint_part, modifier_part) = body.split_once(';').unwrap_or((body, "1"));
    let mut codepoint_fields = codepoint_part.split(':');
    let codepoint = codepoint_fields.next()?.parse::<u32>().ok()?;
    let shifted = codepoint_fields
        .next()
        .filter(|field| !field.is_empty())
        .and_then(|field| field.parse::<u32>().ok());
    let mut modifier_fields = modifier_part.split(':');
    let modifier_value = modifier_fields.next()?.parse::<u16>().ok()?;
    let event = match modifier_fields.next().and_then(|field| field.parse().ok()) {
        Some(2) => KeyEventType::Repeat,
        Some(3) => KeyEventType::Release,
        _ => KeyEventType::Press,
    };
    Some((
        codepoint,
        shifted,
        modifier_value.saturating_sub(1).min(u8::MAX as u16) as u8,
        event,
    ))
}

fn parse_modify_other_keys(raw: &str) -> Option<(u32, u8)> {
    let body = raw.strip_prefix("\x1b[27;")?.strip_suffix('~')?;
    let mut fields = body.split(';');
    let modifier = fields.next()?.parse::<u16>().ok()?.saturating_sub(1);
    let codepoint = fields.next()?.parse::<u32>().ok()?;
    Some((codepoint, modifier.min(u8::MAX as u16) as u8))
}

fn is_printable_codepoint(codepoint: u32) -> Option<String> {
    if codepoint < 32 || codepoint == 127 {
        return None;
    }
    char::from_u32(codepoint)
        .filter(|character| !character.is_control())
        .map(|character| character.to_string())
}

/// Decode a printable Kitty CSI-u or xterm modifyOtherKeys sequence.
///
/// Only plain and Shift-modified text is returned. Ctrl, Alt, Super, and
/// other modifier combinations remain keybinding events instead of becoming
/// accidental text insertion. The returned mask is the normalized Kitty
/// modifier bitset (Shift=1, Alt=2, Ctrl=4, Super=8).
pub fn decode_printable_key(raw: &str) -> Option<(String, u8)> {
    const SHIFT: u8 = 1;
    const ALT: u8 = 2;
    const CTRL: u8 = 4;
    const SUPER: u8 = 8;
    const LOCKS: u8 = 64 | 128;

    if let Some((codepoint, shifted, modifier, _event)) = parse_kitty_sequence(raw) {
        let effective_modifier = modifier & !LOCKS;
        if effective_modifier & (ALT | CTRL | SUPER) != 0 {
            return None;
        }
        let effective_codepoint = if effective_modifier & SHIFT != 0 {
            shifted.unwrap_or(codepoint)
        } else {
            codepoint
        };
        return is_printable_codepoint(effective_codepoint).map(|text| (text, modifier));
    }

    let (codepoint, modifier) = parse_modify_other_keys(raw)?;
    let effective_modifier = modifier & !LOCKS;
    if effective_modifier & (ALT | CTRL | SUPER) != 0 {
        return None;
    }
    is_printable_codepoint(codepoint).map(|text| (text, modifier))
}

/// Classify the event suffix used by Kitty's flag-2 keyboard protocol.
/// Bracketed paste is content, not a release/repeat event, even when pasted
/// text happens to contain a matching `:2` or `:3` substring.
pub fn key_event_type(raw: &str) -> KeyEventType {
    if raw.contains("\x1b[200~") {
        return KeyEventType::Press;
    }
    if let Some(body) = raw.strip_prefix("\x1b[") {
        let body = body.strip_suffix(['u', '~', 'A', 'B', 'C', 'D', 'H', 'F']);
        if let Some(body) = body {
            if let Some(last) = body.rsplit(':').next() {
                return match last {
                    "2" => KeyEventType::Repeat,
                    "3" => KeyEventType::Release,
                    _ => KeyEventType::Press,
                };
            }
        }
    }
    KeyEventType::Press
}

pub fn is_key_release(raw: &str) -> bool {
    key_event_type(raw) == KeyEventType::Release
}

pub fn is_key_repeat(raw: &str) -> bool {
    key_event_type(raw) == KeyEventType::Repeat
}

fn with_modifiers(base: String, ctrl: bool, alt: bool, shift: bool) -> TuiKey {
    TuiKey {
        base,
        ctrl,
        shift,
        alt,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_basic() {
        assert!(match_key(&TuiKey::simple("enter"), "enter"));
        assert!(!match_key(&TuiKey::simple("a"), "enter"));
        assert!(match_key(&TuiKey::ctrl("c"), "ctrl+c"));
        assert!(match_key(&TuiKey::ctrl("a"), "ctrl+a"));
        assert!(!match_key(&TuiKey::simple("c"), "ctrl+c"));
    }

    #[test]
    fn parse_roundtrip() {
        let k = parse_key("ctrl+d");
        assert!(k.ctrl);
        assert_eq!(k.base, "d");
        assert!(match_key(&k, "ctrl+d"));
        let k = parse_key("shift+tab");
        assert!(k.shift);
        assert_eq!(k.base, "tab");
    }

    #[test]
    fn parses_raw_terminal_sequences() {
        assert_eq!(parse_key("\x1b[A"), TuiKey::simple("up"));
        assert_eq!(parse_key("\x1b[1;5C"), TuiKey::ctrl("right"));
        assert_eq!(parse_key("\x1b[Z"), TuiKey::shift("tab"));
        assert_eq!(parse_key("\x1b[97u"), TuiKey::simple("a"));
        assert_eq!(parse_key("\x1b[99;5u"), TuiKey::ctrl("c"));
        assert_eq!(
            parse_key("\x1b\r"),
            with_modifiers("enter".to_string(), false, false, true)
        );
    }

    #[test]
    fn decodes_shifted_kitty_and_modify_other_keys_printables() {
        assert_eq!(
            decode_printable_key("\x1b[97:65;2u"),
            Some(("A".to_string(), 1))
        );
        assert_eq!(
            decode_printable_key("\x1b[27;2;65~"),
            Some(("A".to_string(), 1))
        );
        assert_eq!(decode_printable_key("\x1b[97;5u"), None);
        assert_eq!(decode_printable_key("\x1b[97;3u"), None);
    }

    #[test]
    fn identifies_kitty_event_types_without_misclassifying_paste() {
        assert_eq!(key_event_type("\x1b[97;1:2u"), KeyEventType::Repeat);
        assert!(is_key_release("\x1b[97;1:3u"));
        assert!(!is_key_release("\x1b[200~90:62:3F:A5\x1b[201~"));
        assert!(!is_key_repeat("ordinary text :2F"));
    }
}
