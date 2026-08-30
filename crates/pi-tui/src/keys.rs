//! Keyboard model — port of `packages/tui/src/keys.ts` (the key-string
//! surface pi's interactive mode uses for keybindings).

use std::sync::atomic::{AtomicBool, Ordering};

static KITTY_PROTOCOL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Publish whether the terminal has confirmed Kitty keyboard protocol mode.
/// Legacy escape sequences are intentionally interpreted differently while
/// this flag is set (for example, LF is the configured Shift+Enter mapping).
pub fn set_kitty_protocol_active(active: bool) {
    KITTY_PROTOCOL_ACTIVE.store(active, Ordering::Relaxed);
}

/// Return the current process-wide Kitty keyboard protocol state.
pub fn is_kitty_protocol_active() -> bool {
    KITTY_PROTOCOL_ACTIVE.load(Ordering::Relaxed)
}

/// A parsed key: base + modifiers (ctrl/shift/alt/super).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiKey {
    pub base: String,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub super_key: bool,
}

/// Event kind emitted by the Kitty keyboard protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEventType {
    Press,
    Repeat,
    Release,
}

#[derive(Debug, Clone, Copy)]
struct ParsedKittySequence {
    codepoint: u32,
    shifted_key: Option<u32>,
    modifier: u8,
}

impl TuiKey {
    pub fn simple(base: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            ctrl: false,
            shift: false,
            alt: false,
            super_key: false,
        }
    }
    pub fn ctrl(base: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            ctrl: true,
            shift: false,
            alt: false,
            super_key: false,
        }
    }
    pub fn shift(base: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            ctrl: false,
            shift: true,
            alt: false,
            super_key: false,
        }
    }

    pub fn super_key(base: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            ctrl: false,
            shift: false,
            alt: false,
            super_key: true,
        }
    }

    /// Canonical form like `"ctrl+c"`, `"enter"`, `"shift+tab"`.
    pub fn canonical(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.shift {
            parts.push("shift");
        }
        if self.ctrl {
            parts.push("ctrl");
        }
        if self.alt {
            parts.push("alt");
        }
        if self.super_key {
            parts.push("super");
        }
        parts.push(self.base.as_str());
        parts.join("+")
    }
}

fn parse_key_pattern(pattern: &str) -> (String, bool, bool, bool, bool) {
    let normalized = pattern.to_ascii_lowercase();
    let mut parts: Vec<&str> = normalized.split('+').collect();
    let mut base = parts.pop().unwrap_or_default().to_string();
    if base.is_empty() && !parts.is_empty() {
        // A trailing plus is the literal plus key: `ctrl++` means Ctrl+Plus,
        // while `+` means the unmodified plus key.
        parts.pop();
        base = "+".to_string();
    }
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut super_key = false;
    for part in parts {
        match part {
            "ctrl" => ctrl = true,
            "alt" => alt = true,
            "shift" => shift = true,
            "super" => super_key = true,
            _ => {}
        }
    }
    (base, ctrl, alt, shift, super_key)
}

fn contains_key_modifier(pattern: &str) -> bool {
    pattern.split('+').any(|part| {
        matches!(
            part.to_ascii_lowercase().as_str(),
            "ctrl" | "alt" | "shift" | "super"
        )
    })
}

/// Match a key string like `"ctrl+c"` against a parsed key (upstream
/// `matchesKey`).
pub fn match_key(key: &TuiKey, pattern: &str) -> bool {
    let (base, ctrl, alt, shift, super_key) = parse_key_pattern(pattern);
    let shifted_case_match =
        key.shift && key.base.len() == 1 && base.len() == 1 && key.base.eq_ignore_ascii_case(&base);
    let key_base = key.base.to_ascii_lowercase();
    let single_character_match = if key.base.chars().count() == 1 && base.chars().count() == 1 {
        key.base == base
    } else {
        key_base == base
    };
    let physical_symbol_alias =
        (key_base == "-" && base == "_") || (key_base == "_" && base == "-");
    let base_match = single_character_match
        || (key_base == " " && base == "space")
        || (key_base == "space" && base == " ")
        || (key_base == "esc" && base == "escape")
        || (key_base == "escape" && base == "esc")
        || (key_base == "enter" && base == "return")
        || (key_base == "return" && base == "enter")
        || (physical_symbol_alias && (ctrl || alt))
        || shifted_case_match;
    key.ctrl == ctrl
        && key.alt == alt
        && key.shift == shift
        && key.super_key == super_key
        && base_match
}

/// Match raw terminal input against a key identifier. This is the Rust
/// equivalent of upstream `matchesKey(data, keyId)` and is useful at input
/// boundaries where a parsed `TuiKey` would lose legacy protocol context.
pub fn matches_raw_key(raw: &str, pattern: &str) -> bool {
    let (base, ctrl, alt, shift, super_key) = parse_key_pattern(pattern);

    // A few terminals use the readline-style ESC-letter aliases for Alt
    // arrows.  They are not ordinary Alt+letter input: in particular, b/f
    // are word-left/word-right aliases even though ESC followed by another
    // letter is normally parsed as an Alt printable key.
    if !ctrl && alt && !shift && !super_key {
        if (base == "up" && raw == "\x1bp") || (base == "down" && raw == "\x1bn") {
            return true;
        }
        if (base == "left" && (raw == "\x1bb" || raw == "\x1bB"))
            || (base == "right" && (raw == "\x1bf" || raw == "\x1bF"))
        {
            return !is_kitty_protocol_active() || raw == "\x1bb" || raw == "\x1bf";
        }
    }

    // A raw control byte has more than one logical identity in the legacy
    // protocol. Keep these aliases in the raw matcher instead of losing them
    // when parse_key chooses the terminal's primary interpretation.
    if ctrl && !alt && !shift && !super_key {
        if let Some(control) = raw_ctrl_char(&base) {
            if raw == control {
                return true;
            }
        }
        // Backspace (0x08) is also the legacy Ctrl+H byte.  The terminal
        // cannot identify which logical key produced it, so the upstream
        // matcher intentionally accepts both identities.
        if base == "h" && raw == "\x08" {
            return true;
        }
    }
    if ctrl && alt && !shift && !super_key && !is_kitty_protocol_active() {
        if let Some(control) = raw_ctrl_char(&base) {
            let mut legacy = String::with_capacity(1 + control.len());
            legacy.push('\x1b');
            legacy.push_str(&control);
            if raw == legacy {
                return true;
            }
        }
    }

    // parse_key retains these aliases for compatibility, but upstream
    // matchesKey rejects the ambiguous legacy Ctrl+Alt forms in Kitty mode.
    if is_kitty_protocol_active()
        && ctrl
        && alt
        && !shift
        && !super_key
        && raw.len() == 2
        && raw.starts_with('\x1b')
        && raw.as_bytes()[1] < 32
    {
        return false;
    }

    // Legacy terminals encode Shift+letter as the uppercase byte instead of
    // emitting modifier metadata.
    if shift && !ctrl && !alt && !super_key && raw.chars().count() == 1 {
        if let Some(character) = raw.chars().next() {
            if character.is_ascii_uppercase() && base.len() == 1 {
                return character.to_ascii_lowercase().to_string() == base;
            }
        }
    }

    let key = parse_key(raw);
    match_key(&key, pattern)
}

fn raw_ctrl_char(base: &str) -> Option<String> {
    let bytes = base.as_bytes();
    if bytes.len() != 1 {
        return None;
    }
    let control = match bytes[0] {
        b'a'..=b'z' | b'[' | b'\\' | b']' | b'_' => bytes[0] & 0x1f,
        b'-' => 0x1f,
        _ => return None,
    };
    Some((control as char).to_string())
}

/// Parse a raw key string (from the terminal backend) into a key.
pub fn parse_key(raw: &str) -> TuiKey {
    if let Some(key) = parse_raw_terminal_key(raw) {
        return key;
    }
    if contains_key_modifier(raw) {
        let (base, ctrl, alt, shift, super_key) = parse_key_pattern(raw);
        TuiKey {
            base,
            ctrl,
            shift,
            alt,
            super_key,
        }
    } else {
        TuiKey::simple(raw)
    }
}

fn parse_raw_terminal_key(raw: &str) -> Option<TuiKey> {
    if raw.is_empty() {
        return None;
    }

    match raw {
        "\x1b" => return Some(TuiKey::simple("esc")),
        // Ctrl+symbol bytes are ambiguous in the legacy protocol.  An Alt
        // prefix preserves the exact ctrl+alt binding the upstream matcher
        // exposes for these combinations.
        "\x1b\x1b" => return Some(with_modifiers("[".to_string(), true, true, false)),
        "\x1b\x1c" => return Some(with_modifiers("\\".to_string(), true, true, false)),
        "\x1b\x1d" => return Some(with_modifiers("]".to_string(), true, true, false)),
        "\x1b\x1e" => return Some(with_modifiers("^".to_string(), true, true, false)),
        "\x1b\x1f" => return Some(with_modifiers("-".to_string(), true, true, false)),
        // Some terminals use SS3 for the keypad Enter key.  It is the same
        // logical key as Return, not an unknown escape sequence.
        "\x1bOM" => return Some(TuiKey::simple("enter")),
        "\r" => return Some(TuiKey::simple("enter")),
        "\n" => {
            return Some(if is_kitty_protocol_active() {
                TuiKey::shift("enter")
            } else {
                TuiKey::simple("enter")
            })
        }
        "\t" => return Some(TuiKey::simple("tab")),
        "\x7f" => return Some(TuiKey::simple("backspace")),
        "\x08" => {
            return Some(if is_windows_terminal_session() {
                TuiKey {
                    base: "backspace".to_string(),
                    ctrl: true,
                    shift: false,
                    alt: false,
                    super_key: false,
                }
            } else {
                TuiKey::simple("backspace")
            })
        }
        "\0" => return Some(TuiKey::ctrl(" ")),
        "\x1c" => return Some(TuiKey::ctrl("\\")),
        "\x1d" => return Some(TuiKey::ctrl("]")),
        "\x1e" => return Some(TuiKey::ctrl("^")),
        "\x1f" => return Some(TuiKey::ctrl("-")),
        _ => {}
    }

    if raw.len() == 1 {
        let byte = raw.as_bytes()[0];
        if (1..=26).contains(&byte) {
            return Some(TuiKey::ctrl(((b'a' + byte - 1) as char).to_string()));
        }
        if let Some(ch) = raw.chars().next() {
            return Some(TuiKey::simple(ch.to_string()));
        }
    }

    if let Some(key) = parse_legacy_sequence(raw) {
        return Some(key);
    }

    if let Some((codepoint, modifier)) = parse_modify_other_keys(raw) {
        return parse_codepoint_key(codepoint, modifier, None);
    }

    let escape_tail = raw.strip_prefix('\x1b')?;

    // Legacy Alt+key input is ESC followed by one printable character.
    if escape_tail.chars().count() == 1 {
        let ch = escape_tail.chars().next()?;
        if ch == '\x08' || ch == '\x7f' {
            return Some(with_modifiers("backspace".to_string(), false, true, false));
        }
        if !is_kitty_protocol_active()
            && ch != '\r'
            && ch != '\t'
            && (1..=26).contains(&(ch as u32))
        {
            return Some(with_modifiers(
                ((b'a' + ch as u8 - 1) as char).to_string(),
                true,
                true,
                false,
            ));
        }
        if ch == '\r' {
            return Some(if is_kitty_protocol_active() {
                with_modifiers("enter".to_string(), false, false, true)
            } else {
                with_modifiers("enter".to_string(), false, true, false)
            });
        }
        if ch == ' ' && !is_kitty_protocol_active() {
            return Some(with_modifiers(" ".to_string(), false, true, false));
        }
        // ESC+Tab and ESC+Space in Kitty mode are not Shift+Tab/Space. The
        // former has no legacy key identity, while the latter is an
        // unsupported legacy Alt sequence once Kitty mode is active.
        if is_kitty_protocol_active() || ch == '\t' || ch == ' ' {
            return None;
        }
        return Some(with_modifiers(ch.to_string(), false, true, false));
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

fn parse_legacy_sequence(raw: &str) -> Option<TuiKey> {
    let key = match raw {
        "\x1b[A" | "\x1bOA" => "up",
        "\x1b[B" | "\x1bOB" => "down",
        "\x1b[C" | "\x1bOC" => "right",
        "\x1b[D" | "\x1bOD" => "left",
        "\x1b[H" | "\x1bOH" | "\x1b[1~" | "\x1b[7~" => "home",
        "\x1b[F" | "\x1bOF" | "\x1b[4~" | "\x1b[8~" => "end",
        "\x1b[2~" => "insert",
        "\x1b[3~" => "delete",
        "\x1b[5~" | "\x1b[[5~" => "pageup",
        "\x1b[6~" | "\x1b[[6~" => "pagedown",
        "\x1b[E" | "\x1bOE" => "clear",
        "\x1bOP" | "\x1b[11~" | "\x1b[[A" => "f1",
        "\x1bOQ" | "\x1b[12~" | "\x1b[[B" => "f2",
        "\x1bOR" | "\x1b[13~" | "\x1b[[C" => "f3",
        "\x1bOS" | "\x1b[14~" | "\x1b[[D" => "f4",
        "\x1b[15~" | "\x1b[[E" => "f5",
        "\x1b[17~" => "f6",
        "\x1b[18~" => "f7",
        "\x1b[19~" => "f8",
        "\x1b[20~" => "f9",
        "\x1b[21~" => "f10",
        "\x1b[23~" => "f11",
        "\x1b[24~" => "f12",
        _ => return parse_legacy_modifier_sequence(raw),
    };
    Some(TuiKey::simple(key))
}

fn parse_legacy_modifier_sequence(raw: &str) -> Option<TuiKey> {
    let (base, modifier) = match raw {
        "\x1b[a" => ("up", "shift"),
        "\x1b[b" => ("down", "shift"),
        "\x1b[c" => ("right", "shift"),
        "\x1b[d" => ("left", "shift"),
        "\x1b[e" => ("clear", "shift"),
        "\x1b[2$" => ("insert", "shift"),
        "\x1b[3$" => ("delete", "shift"),
        "\x1b[5$" => ("pageup", "shift"),
        "\x1b[6$" => ("pagedown", "shift"),
        "\x1b[7$" => ("home", "shift"),
        "\x1b[8$" => ("end", "shift"),
        "\x1bOa" => ("up", "ctrl"),
        "\x1bOb" => ("down", "ctrl"),
        "\x1bOc" => ("right", "ctrl"),
        "\x1bOd" => ("left", "ctrl"),
        "\x1bOe" => ("clear", "ctrl"),
        "\x1b[2^" => ("insert", "ctrl"),
        "\x1b[3^" => ("delete", "ctrl"),
        "\x1b[5^" => ("pageup", "ctrl"),
        "\x1b[6^" => ("pagedown", "ctrl"),
        "\x1b[7^" => ("home", "ctrl"),
        "\x1b[8^" => ("end", "ctrl"),
        "\x1bB" if !is_kitty_protocol_active() => ("left", "alt"),
        "\x1bF" if !is_kitty_protocol_active() => ("right", "alt"),
        // Readline-style Alt-arrow aliases remain recognizable in Kitty
        // mode. Uppercase B/F are the ambiguous legacy forms and are only
        // accepted while Kitty mode is inactive.
        "\x1bb" => ("left", "alt"),
        "\x1bf" => ("right", "alt"),
        "\x1bp" => ("up", "alt"),
        "\x1bn" => ("down", "alt"),
        _ => return None,
    };
    Some(match modifier {
        "shift" => TuiKey::shift(base),
        "ctrl" => TuiKey::ctrl(base),
        "alt" => with_modifiers(base.to_string(), false, true, false),
        _ => TuiKey::simple(base),
    })
}

fn parse_codepoint_key(
    codepoint: u32,
    modifier: u8,
    base_layout_key: Option<u32>,
) -> Option<TuiKey> {
    const SHIFT: u8 = 1;
    const ALT: u8 = 2;
    const CTRL: u8 = 4;
    const SUPER: u8 = 8;
    const LOCKS: u8 = 64 | 128;
    const SUPPORTED: u8 = SHIFT | ALT | CTRL | SUPER;
    let modifier = modifier & !LOCKS;
    if modifier & !SUPPORTED != 0 {
        return None;
    }

    let mut effective = codepoint;
    if modifier & SHIFT != 0 && (b'A' as u32..=b'Z' as u32).contains(&effective) {
        effective += 32;
    }
    let recognized = (b'a' as u32..=b'z' as u32).contains(&effective)
        || (b'0' as u32..=b'9' as u32).contains(&effective)
        || is_symbol_codepoint(effective);
    if !recognized {
        if let Some(base) = base_layout_key {
            effective = base;
        }
    }

    let base = match effective {
        0x1b => "esc".to_string(),
        0x09 => "tab".to_string(),
        0x0d | 57414 => "enter".to_string(),
        0x20 => " ".to_string(),
        0x7f => "backspace".to_string(),
        57399..=57408 => ((b'0' + (effective - 57399) as u8) as char).to_string(),
        57409 => ".".to_string(),
        57410 => "/".to_string(),
        57411 => "*".to_string(),
        57412 => "-".to_string(),
        57413 => "+".to_string(),
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
        _ => char::from_u32(effective)
            .filter(|character| !character.is_control())
            .map(|character| character.to_string())?,
    };
    Some(TuiKey {
        base,
        ctrl: modifier & CTRL != 0,
        shift: modifier & SHIFT != 0,
        alt: modifier & ALT != 0,
        super_key: modifier & SUPER != 0,
    })
}

fn is_symbol_codepoint(codepoint: u32) -> bool {
    const SYMBOLS: &[u32] = &[
        0x60, 0x2d, 0x3d, 0x5b, 0x5d, 0x5c, 0x3b, 0x27, 0x2c, 0x2e, 0x2f, 0x21, 0x40, 0x23, 0x24,
        0x25, 0x5e, 0x26, 0x2a, 0x28, 0x29, 0x5f, 0x2b, 0x7c, 0x7e, 0x7b, 0x7d, 0x3a, 0x3c, 0x3e,
        0x3f,
    ];
    SYMBOLS.contains(&codepoint)
}

fn is_windows_terminal_session() -> bool {
    std::env::var_os("WT_SESSION").is_some()
        && std::env::var_os("SSH_CONNECTION").is_none()
        && std::env::var_os("SSH_CLIENT").is_none()
        && std::env::var_os("SSH_TTY").is_none()
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
    let codepoint_part = fields.next()?;
    let mut codepoint_fields = codepoint_part.split(':');
    let codepoint = codepoint_fields.next()?.parse::<u32>().ok()?;
    let shifted = codepoint_fields
        .next()
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<u32>().ok());
    let base_layout = codepoint_fields
        .next()
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<u32>().ok());
    let modifier = fields
        .next()
        .and_then(|value| value.split(':').next().unwrap_or(value).parse::<u16>().ok())
        .unwrap_or(1)
        .saturating_sub(1)
        .min(u8::MAX as u16) as u8;
    let effective_codepoint = if modifier & 1 != 0 {
        shifted.unwrap_or(codepoint)
    } else {
        codepoint
    };
    parse_codepoint_key(effective_codepoint, modifier, base_layout)
}

/// Return the Kitty CSI-u codepoint, normalized modifier mask, and event.
fn parse_kitty_sequence(raw: &str) -> Option<ParsedKittySequence> {
    let body = raw.strip_prefix("\x1b[")?.strip_suffix('u')?;
    let (codepoint_part, modifier_part) = body.split_once(';').unwrap_or((body, "1"));
    let mut codepoint_fields = codepoint_part.split(':');
    let codepoint = codepoint_fields.next()?.parse::<u32>().ok()?;
    let shifted = codepoint_fields
        .next()
        .filter(|field| !field.is_empty())
        .and_then(|field| field.parse::<u32>().ok());
    let _base_layout = codepoint_fields
        .next()
        .filter(|field| !field.is_empty())
        .and_then(|field| field.parse::<u32>().ok());
    let mut modifier_fields = modifier_part.split(':');
    let modifier_value = modifier_fields.next()?.parse::<u16>().ok()?;
    let _event = match modifier_fields.next().and_then(|field| field.parse().ok()) {
        Some(2) => KeyEventType::Repeat,
        Some(3) => KeyEventType::Release,
        _ => KeyEventType::Press,
    };
    Some(ParsedKittySequence {
        codepoint,
        shifted_key: shifted,
        modifier: modifier_value.saturating_sub(1).min(u8::MAX as u16) as u8,
    })
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

/// Kitty reserves a private-use range for keypad keys.  Printable decoding
/// must translate only the keypad digits/symbols; keypad navigation keys are
/// logical controls and must never leak as private-use text.
fn normalize_printable_codepoint(codepoint: u32) -> Option<u32> {
    Some(match codepoint {
        57399..=57408 => b'0' as u32 + (codepoint - 57399),
        57409 => b'.' as u32,
        57410 => b'/' as u32,
        57411 => b'*' as u32,
        57412 => b'-' as u32,
        57413 => b'+' as u32,
        57415 => b'=' as u32,
        57416 => b',' as u32,
        57417..=57426 => return None,
        value => value,
    })
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
    const SUPPORTED: u8 = SHIFT | ALT | CTRL | SUPER;

    if let Some(parsed) = parse_kitty_sequence(raw) {
        let codepoint = parsed.codepoint;
        let shifted = parsed.shifted_key;
        let modifier = parsed.modifier;
        let effective_modifier = modifier & !LOCKS;
        if effective_modifier & !SUPPORTED != 0 || effective_modifier & (ALT | CTRL | SUPER) != 0 {
            return None;
        }
        let effective_codepoint = if effective_modifier & SHIFT != 0 {
            shifted.unwrap_or(codepoint)
        } else {
            codepoint
        };
        return normalize_printable_codepoint(effective_codepoint)
            .and_then(is_printable_codepoint)
            .map(|text| (text, modifier));
    }

    let (codepoint, modifier) = parse_modify_other_keys(raw)?;
    let effective_modifier = modifier & !LOCKS;
    if effective_modifier & !SUPPORTED != 0 || effective_modifier & (ALT | CTRL | SUPER) != 0 {
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
            if let Some((_, event)) = body.rsplit_once(':') {
                let last = event;
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
        super_key: false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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
        set_kitty_protocol_active(true);
        assert_eq!(parse_key("\x1b[A"), TuiKey::simple("up"));
        assert_eq!(parse_key("\x1b[1;5C"), TuiKey::ctrl("right"));
        assert_eq!(parse_key("\x1b[Z"), TuiKey::shift("tab"));
        assert_eq!(parse_key("\x1b[97u"), TuiKey::simple("a"));
        assert_eq!(parse_key("\x1b[99;5u"), TuiKey::ctrl("c"));
        assert_eq!(
            parse_key("\x1b\r"),
            with_modifiers("enter".to_string(), false, false, true)
        );
        set_kitty_protocol_active(false);
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

    #[test]
    fn parses_upstream_legacy_function_and_modifier_sequences() {
        assert_eq!(parse_key("\x1b[[5~"), TuiKey::simple("pageup"));
        assert_eq!(parse_key("\x1b[E"), TuiKey::simple("clear"));
        assert_eq!(parse_key("\x1b[24~"), TuiKey::simple("f12"));
        assert_eq!(parse_key("\x1b[2^"), TuiKey::ctrl("insert"));
        assert_eq!(parse_key("\x1b[a"), TuiKey::shift("up"));
        assert_eq!(parse_key("\x1bOa"), TuiKey::ctrl("up"));
        assert_eq!(
            parse_key("\x1bp"),
            with_modifiers("up".into(), false, true, false)
        );
    }

    #[test]
    fn parses_modify_other_keys_and_kitty_alternate_layouts() {
        assert_eq!(parse_key("\x1b[27;5;99~"), TuiKey::ctrl("c"));
        assert_eq!(parse_key("\x1b[27;2;13~"), TuiKey::shift("enter"));
        set_kitty_protocol_active(true);
        assert_eq!(parse_key("\x1b[1089::99;5u"), TuiKey::ctrl("c"));
        assert!(match_key(&parse_key("\x1b[69;2u"), "shift+e"));
        assert_eq!(parse_key("\n"), TuiKey::shift("enter"));
        set_kitty_protocol_active(false);
    }

    #[test]
    fn raw_matching_preserves_mode_sensitive_legacy_input() {
        set_kitty_protocol_active(false);
        assert!(matches_raw_key("\x03", "ctrl+c"));
        assert!(matches_raw_key("\x1bB", "alt+left"));
        assert!(matches_raw_key("\x1b\x1b", "ctrl+alt+["));
        assert!(matches_raw_key("\x1b\x1c", "ctrl+alt+\\"));
        assert!(matches_raw_key("\x1b\x1f", "ctrl+alt+_"));
        assert!(matches_raw_key("\x1bOM", "return"));
        assert!(matches_raw_key("\x1b[1;5H", "ctrl+home"));
        assert!(matches_raw_key("\x1b[27;7;104~", "ctrl+alt+h"));
        set_kitty_protocol_active(true);
        assert!(!matches_raw_key("\x1bB", "alt+left"));
        set_kitty_protocol_active(false);
    }

    #[test]
    fn parses_kitty_alternate_layouts_and_normalizes_keypad_keys() {
        set_kitty_protocol_active(true);
        assert_eq!(parse_key("\x1b[1089::99;5u"), TuiKey::ctrl("c"));
        assert_eq!(
            parse_key("\x1b[1079::112;6u"),
            TuiKey {
                base: "p".to_string(),
                ctrl: true,
                shift: true,
                alt: false,
                super_key: false,
            }
        );
        assert!(matches_raw_key("\x1b[1089::99;5u", "ctrl+c"));
        assert!(!matches_raw_key("\x1b[1089::99;5u", "ctrl+d"));
        assert_eq!(parse_key("\x1b[57399u"), TuiKey::simple("0"));
        assert_eq!(parse_key("\x1b[57410u"), TuiKey::simple("/"));
        assert_eq!(parse_key("\x1b[57417u"), TuiKey::simple("left"));
        assert_eq!(parse_key("\x1b[57426u"), TuiKey::simple("delete"));
        assert!(matches_raw_key("\x1b[57417u", "left"));
        set_kitty_protocol_active(false);
    }

    #[test]
    fn kitty_and_modify_other_keys_preserve_modifier_identity() {
        set_kitty_protocol_active(true);
        assert!(matches_raw_key("\x1b[107;9u", "super+k"));
        assert!(matches_raw_key("\x1b[107;13u", "ctrl+super+k"));
        assert!(!matches_raw_key("\x1b[107;13u", "super+k"));
        assert_eq!(parse_key("\x1b[69;2u"), TuiKey::shift("e"));
        assert_eq!(
            parse_key("\x1b[1089:1057:99;6:2u"),
            TuiKey {
                base: "c".to_string(),
                ctrl: true,
                shift: true,
                alt: false,
                super_key: false,
            }
        );
        set_kitty_protocol_active(false);

        assert_eq!(parse_key("\x1b[27;5;99~"), TuiKey::ctrl("c"));
        assert_eq!(parse_key("\x1b[27;2;13~"), TuiKey::shift("enter"));
        assert_eq!(
            parse_key("\x1b[27;3;127~"),
            with_modifiers("backspace".into(), false, true, false)
        );
        assert!(matches_raw_key("\x1b[27;7;104~", "ctrl+alt+h"));
        assert!(matches_raw_key("\x1b[27;6;69~", "ctrl+shift+e"));
        assert_eq!(
            decode_printable_key("\x1b[57399u"),
            Some(("0".to_string(), 0))
        );
        assert_eq!(decode_printable_key("\x1b[57417u"), None);
    }

    #[test]
    fn legacy_alt_aliases_and_backspace_ambiguity_match_upstream() {
        set_kitty_protocol_active(false);
        assert!(matches_raw_key("\x1bp", "alt+up"));
        assert!(matches_raw_key("\x1bn", "alt+down"));
        assert!(matches_raw_key("\x1bb", "alt+left"));
        assert!(matches_raw_key("\x1bf", "alt+right"));
        assert!(matches_raw_key("\x1b,", "alt+,"));
        assert!(matches_raw_key("\x1b\x03", "ctrl+alt+c"));
        assert!(matches_raw_key("\x1b\x1f", "ctrl+alt+_"));

        let previous = std::env::var_os("WT_SESSION");
        std::env::remove_var("WT_SESSION");
        assert!(matches_raw_key("\x08", "backspace"));
        assert!(!matches_raw_key("\x08", "ctrl+backspace"));
        assert!(matches_raw_key("\x08", "ctrl+h"));
        if let Some(value) = previous {
            std::env::set_var("WT_SESSION", value);
        }

        set_kitty_protocol_active(true);
        assert!(!matches_raw_key("\x1ba", "alt+a"));
        assert!(!matches_raw_key("\x1bB", "alt+left"));
        assert!(matches_raw_key("\x1bb", "alt+left"));
        assert!(matches_raw_key("\x1bp", "alt+up"));
        set_kitty_protocol_active(false);
    }

    #[test]
    fn decode_printable_maps_every_kitty_keypad_text_equivalent() {
        let cases = [
            (57399, "0"),
            (57400, "1"),
            (57409, "."),
            (57410, "/"),
            (57411, "*"),
            (57412, "-"),
            (57413, "+"),
            (57415, "="),
            (57416, ","),
        ];
        for (codepoint, expected) in cases {
            assert_eq!(
                decode_printable_key(&format!("\x1b[{codepoint}u")),
                Some((expected.to_string(), 0))
            );
        }
        assert_eq!(decode_printable_key("\x1b[57417u"), None);
    }

    #[test]
    fn raw_letter_matching_keeps_shift_identity() {
        assert!(!matches_raw_key("A", "a"));
        assert!(matches_raw_key("A", "shift+a"));
        assert!(!match_key(&TuiKey::simple("A"), "a"));
        assert!(match_key(&TuiKey::shift("a"), "shift+a"));
    }

    #[test]
    fn matches_official_kitty_and_native_key_cases() {
        set_kitty_protocol_active(true);

        for (raw, pattern) in [
            ("\x1b[1089::99;5u", "ctrl+c"),
            ("\x1b[1074::100;5u", "ctrl+d"),
            ("\x1b[1103::122;5u", "ctrl+z"),
            ("\x1b[1079::112;6u", "ctrl+shift+p"),
            ("\x1b[107;9u", "super+k"),
            ("\x1b[13;9u", "super+enter"),
            ("\x1b[107;13u", "ctrl+super+k"),
            ("\x1b[107;14u", "ctrl+shift+super+k"),
            ("\x1b[49u", "1"),
            ("\x1b[49;5u", "ctrl+1"),
            ("\x1b[99:67:99;2u", "shift+c"),
            ("\x1b[1089::99;5:3u", "ctrl+c"),
            ("\x1b[1089:1057:99;6:2u", "ctrl+shift+c"),
            ("\x1b[57400u", "1"),
            ("\x1b[57417u", "left"),
            ("\x1b[57426u", "delete"),
        ] {
            assert!(
                matches_raw_key(raw, pattern),
                "{raw:?} should match {pattern}"
            );
        }
        assert!(!matches_raw_key("\x1b[1089::99;5u", "ctrl+d"));
        assert!(!matches_raw_key("\x1b[1089::99;5u", "ctrl+shift+c"));
        assert!(!matches_raw_key("\x1b[107;13u", "super+k"));
        assert!(!matches_raw_key("\x1b[99;17u", "ctrl+c"));
        assert_eq!(
            parse_key("\x1b[107;14u"),
            TuiKey {
                base: "k".to_string(),
                ctrl: true,
                shift: true,
                alt: false,
                super_key: true,
            }
        );

        // Kitty mode keeps only the unambiguous readline Alt-arrow aliases;
        // the uppercase legacy forms and ordinary Alt+printable sequences
        // are not reinterpreted as editor commands.
        assert!(matches_raw_key("\x1bp", "alt+up"));
        assert!(matches_raw_key("\x1bb", "alt+left"));
        assert!(!matches_raw_key("\x1bB", "alt+left"));
        assert!(!matches_raw_key("\x1ba", "alt+a"));
        assert!(!matches_raw_key("\x1b ", "alt+space"));
        assert!(!matches_raw_key("\x1b ", "shift+space"));
        assert!(!matches_raw_key("\x1b\t", "shift+tab"));
        assert_eq!(
            parse_key("\x1bp"),
            with_modifiers("up".into(), false, true, false)
        );
        set_kitty_protocol_active(false);

        for (raw, pattern) in [
            ("\x1b[27;5;99~", "ctrl+c"),
            ("\x1b[27;2;13~", "shift+enter"),
            ("\x1b[27;3;127~", "alt+backspace"),
            ("\x1b[27;6;69~", "ctrl+shift+e"),
            ("\x1b\x03", "ctrl+alt+c"),
            ("\x1b\x1f", "ctrl+alt+_"),
            ("\x1bB", "alt+left"),
            ("\x1bF", "alt+right"),
            ("\x1b[1;5H", "ctrl+home"),
            ("\x1b[5;5~", "ctrl+pageup"),
            ("\x1b[2^", "ctrl+insert"),
        ] {
            assert!(
                matches_raw_key(raw, pattern),
                "{raw:?} should match {pattern}"
            );
        }
        assert_eq!(parse_key("\x1b[27;5;99~"), TuiKey::ctrl("c"));
        assert_eq!(
            parse_key("\x1b[27;3;127~"),
            with_modifiers("backspace".into(), false, true, false)
        );
        assert_eq!(
            decode_printable_key("\x1b[27;2;196~"),
            Some(("Ä".to_string(), 1))
        );

        // Raw BS has its documented dual identity: plain Backspace outside a
        // local Windows Terminal, and Ctrl+H in all legacy environments.
        let previous = std::env::var_os("WT_SESSION");
        std::env::remove_var("WT_SESSION");
        assert!(matches_raw_key("\x08", "backspace"));
        assert!(!matches_raw_key("\x08", "ctrl+backspace"));
        assert!(matches_raw_key("\x08", "ctrl+h"));
        if let Some(value) = previous {
            std::env::set_var("WT_SESSION", value);
        }
    }
}
