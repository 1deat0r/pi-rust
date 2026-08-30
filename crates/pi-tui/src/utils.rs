//! Terminal-text utilities shared by the TUI components.
//!
//! The upstream TUI treats terminal output as a stream of ANSI controls and
//! grapheme clusters, rather than as a stream of Rust `char`s. Keeping that
//! distinction here is important: a Rust `char` can be half of a displayed
//! cell (CJK/emoji), or part of a zero-width cluster (combining marks), and an
//! escape sequence must never be counted or split.

/// A complete ANSI/control sequence found at a byte offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnsiCode {
    pub code: String,
    pub length: usize,
}

/// A range of terminal cells occupied by one grapheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphemeCellRange {
    pub start: usize,
    pub end: usize,
}

/// The result of slicing a line by terminal columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WidthSlice {
    pub text: String,
    pub width: usize,
}

/// The two pieces used when an overlay replaces a range of a base line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedSegments {
    pub before: String,
    pub before_width: usize,
    pub after: String,
    pub after_width: usize,
}

/// Extract a complete terminal control sequence at `pos`.
///
/// CSI, OSC, APC, DCS, PM and SOS strings are recognized. The parser is
/// bounded by the input length, so malformed or incomplete terminal replies
/// cannot make a renderer loop forever.
pub fn extract_ansi_code(text: &str, pos: usize) -> Option<AnsiCode> {
    let bytes = text.as_bytes();
    if pos >= bytes.len() || bytes[pos] != 0x1b || pos + 1 >= bytes.len() {
        return None;
    }

    match bytes[pos + 1] {
        b'[' => {
            let mut i = pos + 2;
            while i < bytes.len() {
                let byte = bytes[i];
                if (0x40..=0x7e).contains(&byte) {
                    return Some(AnsiCode {
                        code: text[pos..=i].to_string(),
                        length: i + 1 - pos,
                    });
                }
                i += 1;
            }
            None
        }
        b']' | b'_' | b'P' | b'^' | b'X' => {
            let mut i = pos + 2;
            while i < bytes.len() {
                if bytes[i] == 0x07 {
                    return Some(AnsiCode {
                        code: text[pos..=i].to_string(),
                        length: i + 1 - pos,
                    });
                }
                if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                    return Some(AnsiCode {
                        code: text[pos..i + 2].to_string(),
                        length: i + 2 - pos,
                    });
                }
                i += 1;
            }
            None
        }
        // Two-byte ESC sequences (save/restore cursor, keypad mode, etc.)
        // are controls too. Treating them as a unit prevents the second byte
        // from leaking into visible-width calculations.
        // An ESC Fe control has an ASCII second byte. If the next byte starts
        // a UTF-8 character, leave the ESC for the malformed-input recovery
        // path below; slicing it as a two-byte sequence would split that
        // character and panic on otherwise valid Unicode text.
        _ if bytes[pos + 1].is_ascii() => Some(AnsiCode {
            code: text[pos..pos + 2].to_string(),
            length: 2,
        }),
        _ => None,
    }
}

/// Remove ANSI, OSC, APC and related terminal control sequences.
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
pub fn strip_terminal_sequences(text: &str) -> String {
    if !text.contains('\x1b') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    while pos < text.len() {
        if let Some(code) = extract_ansi_code(text, pos) {
            pos += code.length;
        } else if text.as_bytes()[pos] == 0x1b {
            // Keep scanning after malformed/incomplete input. ESC itself is a
            // non-printing control, while following text remains data.
            pos += 1;
        } else {
            let ch = text[pos..].chars().next().expect("valid UTF-8 boundary");
            out.push(ch);
            pos += ch.len_utf8();
        }
    }
    out
}

/// Backwards-compatible name used by the existing Rust components.
pub fn strip_ansi_codes(text: &str) -> String {
    strip_terminal_sequences(text)
}

fn is_variation_selector(cp: u32) -> bool {
    (0xfe00..=0xfe0f).contains(&cp) || (0xe0100..=0xe01ef).contains(&cp)
}

fn is_emoji_modifier(cp: u32) -> bool {
    (0x1f3fb..=0x1f3ff).contains(&cp)
}

fn is_tag_character(cp: u32) -> bool {
    (0xe0020..=0xe007f).contains(&cp)
}

fn is_control(cp: u32) -> bool {
    cp < 0x20 || (0x7f..=0x9f).contains(&cp)
}

fn is_format(cp: u32) -> bool {
    matches!(
        cp,
        0x00ad
            | 0x061c
            | 0x180e
            | 0x200b..=0x200f
            | 0x202a..=0x202e
            | 0x2060..=0x2064
            | 0x2066..=0x206f
            | 0xfeff
            | 0xfff9..=0xfffb
    ) || is_variation_selector(cp)
        || is_tag_character(cp)
}

fn is_combining_mark(cp: u32) -> bool {
    // Unicode Mark ranges used by terminal grapheme segmentation. This is
    // explicit because the crate intentionally has no Unicode-property
    // dependency, and covers the scripts handled by the upstream tests.
    (0x0300..=0x036f).contains(&cp)
        || (0x0483..=0x0489).contains(&cp)
        || (0x0591..=0x05bd).contains(&cp)
        || cp == 0x05bf
        || (0x05c1..=0x05c2).contains(&cp)
        || (0x05c4..=0x05c5).contains(&cp)
        || (0x0610..=0x061a).contains(&cp)
        || (0x064b..=0x065f).contains(&cp)
        || cp == 0x0670
        || (0x06d6..=0x06ed).contains(&cp)
        || cp == 0x0711
        || (0x0730..=0x074a).contains(&cp)
        || (0x07a6..=0x07b0).contains(&cp)
        || (0x07eb..=0x07f3).contains(&cp)
        || (0x0816..=0x0819).contains(&cp)
        || (0x081b..=0x0823).contains(&cp)
        || (0x0825..=0x0827).contains(&cp)
        || (0x0829..=0x082d).contains(&cp)
        || (0x0859..=0x085b).contains(&cp)
        || (0x08d3..=0x08ff).contains(&cp)
        || (0x0900..=0x0903).contains(&cp)
        || (0x093a..=0x093c).contains(&cp)
        || (0x0941..=0x094d).contains(&cp)
        || (0x0951..=0x0957).contains(&cp)
        || (0x0962..=0x0963).contains(&cp)
        || (0x0981..=0x0983).contains(&cp)
        || (0x09bc..=0x09cd).contains(&cp)
        || cp == 0x09d7
        || (0x0a01..=0x0a03).contains(&cp)
        || (0x0a3c..=0x0a4d).contains(&cp)
        || cp == 0x0a51
        || (0x0a70..=0x0a71).contains(&cp)
        || cp == 0x0a75
        || (0x0a81..=0x0a83).contains(&cp)
        || (0x0abc..=0x0acd).contains(&cp)
        || (0x0b01..=0x0b03).contains(&cp)
        || (0x0b3c..=0x0b4d).contains(&cp)
        || (0x0b55..=0x0b57).contains(&cp)
        || (0x0b62..=0x0b63).contains(&cp)
        || cp == 0x0b82
        || (0x0bbe..=0x0bcd).contains(&cp)
        || (0x0c00..=0x0c04).contains(&cp)
        || (0x0c3e..=0x0c4d).contains(&cp)
        || (0x0c55..=0x0c56).contains(&cp)
        || (0x0c62..=0x0c63).contains(&cp)
        || (0x0c81..=0x0c83).contains(&cp)
        || (0x0cbc..=0x0ccd).contains(&cp)
        || (0x0ce2..=0x0ce3).contains(&cp)
        || (0x0d00..=0x0d03).contains(&cp)
        || (0x0d3b..=0x0d4d).contains(&cp)
        || cp == 0x0d57
        || (0x0d62..=0x0d63).contains(&cp)
        || (0x0d81..=0x0d83).contains(&cp)
        || (0x0dbc..=0x0dcd).contains(&cp)
        || (0x0dd0..=0x0dd9).contains(&cp)
        || cp == 0x0e31
        || (0x0e34..=0x0e3a).contains(&cp)
        || (0x0e47..=0x0e4e).contains(&cp)
        || cp == 0x0eb1
        || (0x0eb4..=0x0ebc).contains(&cp)
        || (0x0ec8..=0x0ecd).contains(&cp)
        || (0x0f18..=0x0f19).contains(&cp)
        || (0x0f35..=0x0f39).contains(&cp)
        || (0x0f71..=0x0f84).contains(&cp)
        || (0x0f86..=0x0f87).contains(&cp)
        || (0x0f8d..=0x0fbc).contains(&cp)
        || (0x102b..=0x103e).contains(&cp)
        || (0x1056..=0x1059).contains(&cp)
        || (0x105e..=0x1060).contains(&cp)
        || (0x1712..=0x1714).contains(&cp)
        || (0x1732..=0x1734).contains(&cp)
        || (0x1752..=0x1753).contains(&cp)
        || (0x1772..=0x1773).contains(&cp)
        || (0x17b4..=0x17d3).contains(&cp)
        || (0x180b..=0x180d).contains(&cp)
        || (0x1885..=0x1886).contains(&cp)
        || cp == 0x18a9
        || (0x1ab0..=0x1aff).contains(&cp)
        || (0x1dc0..=0x1dff).contains(&cp)
        || (0x20d0..=0x20ff).contains(&cp)
        || (0x2cef..=0x2cf1).contains(&cp)
        || (0x2de0..=0x2dff).contains(&cp)
        || (0x302a..=0x302f).contains(&cp)
        || (0xa66f..=0xa67d).contains(&cp)
        || (0xa69e..=0xa69f).contains(&cp)
        || cp == 0xa802
        || cp == 0xa806
        || cp == 0xa80b
        || (0xa823..=0xa827).contains(&cp)
        || (0xa880..=0xa881).contains(&cp)
        || (0xa8b4..=0xa8c5).contains(&cp)
        || (0xa8e0..=0xa8f1).contains(&cp)
        || (0xa926..=0xa92f).contains(&cp)
        || (0xa947..=0xa953).contains(&cp)
        || (0xa980..=0xa983).contains(&cp)
        || (0xa9b3..=0xa9c0).contains(&cp)
        || (0xaa29..=0xaa36).contains(&cp)
        || cp == 0xaa43
        || (0xaa4c..=0xaa4d).contains(&cp)
        || (0xaa7b..=0xaa7d).contains(&cp)
        || (0xaab0..=0xaab4).contains(&cp)
        || (0xaab7..=0xaab8).contains(&cp)
        || (0xaabe..=0xaabf).contains(&cp)
        || cp == 0xaac1
        || (0xaaf5..=0xaaf6).contains(&cp)
        || (0xabe3..=0xabe4).contains(&cp)
        || (0xabe6..=0xabe7).contains(&cp)
        || cp == 0xabe9
        || (0xabec..=0xabed).contains(&cp)
        || cp == 0xfb1e
        || (0xfe20..=0xfe2f).contains(&cp)
}

fn is_terminal_spacing_mark(cp: u32) -> bool {
    // Unicode Spacing_Mark plus the legacy wcwidth exceptions used by the
    // upstream TUI. The exceptions are listed because several Myanmar marks
    // are Mn but still occupy a cell in common terminals.
    (0x0903..=0x0903).contains(&cp)
        || (0x093b..=0x0940).contains(&cp)
        || (0x0949..=0x094c).contains(&cp)
        || (0x094e..=0x094f).contains(&cp)
        || (0x0982..=0x0983).contains(&cp)
        || (0x09be..=0x09c0).contains(&cp)
        || (0x09c7..=0x09c8).contains(&cp)
        || (0x09cb..=0x09cc).contains(&cp)
        || cp == 0x09d7
        || cp == 0x0a03
        || (0x0a3e..=0x0a40).contains(&cp)
        || cp == 0x0a83
        || (0x0abe..=0x0ac0).contains(&cp)
        || (0x0ac9..=0x0acc).contains(&cp)
        || (0x0b02..=0x0b03).contains(&cp)
        || (0x0b3e..=0x0b40).contains(&cp)
        || (0x0b47..=0x0b48).contains(&cp)
        || (0x0b4b..=0x0b4c).contains(&cp)
        || (0x0bbe..=0x0bc0).contains(&cp)
        || (0x0bc6..=0x0bc8).contains(&cp)
        || (0x0bca..=0x0bcc).contains(&cp)
        || (0x0c01..=0x0c03).contains(&cp)
        || (0x0c41..=0x0c44).contains(&cp)
        || (0x0c82..=0x0c83).contains(&cp)
        || (0x0cbe..=0x0cc4).contains(&cp)
        || (0x0cc7..=0x0cc8).contains(&cp)
        || (0x0cca..=0x0ccc).contains(&cp)
        || (0x0d02..=0x0d03).contains(&cp)
        || (0x0d3e..=0x0d40).contains(&cp)
        || (0x0d46..=0x0d48).contains(&cp)
        || (0x0d4a..=0x0d4c).contains(&cp)
        || cp == 0x0d57
        || (0x102b..=0x102c).contains(&cp)
        || cp == 0x1031
        || (0x1033..=0x1035).contains(&cp)
        || cp == 0x1038
        || (0x103a..=0x103e).contains(&cp)
        || (0x1056..=0x1059).contains(&cp)
        || (0x17b6..=0x17c8).contains(&cp)
        || (0x1a55..=0x1a5e).contains(&cp)
        || (0x1b35..=0x1b44).contains(&cp)
        || (0x1b6b..=0x1b73).contains(&cp)
        || (0xa823..=0xa827).contains(&cp)
        || (0xa880..=0xa881).contains(&cp)
        || (0xaa7b..=0xaa7d).contains(&cp)
        || cp == 0x065f
        || cp == 0x0f7f
}

fn is_zero_width(cp: u32) -> bool {
    is_control(cp) || is_format(cp) || is_combining_mark(cp)
}

fn is_cjk(cp: u32) -> bool {
    (0x1100..=0x115f).contains(&cp)
        || (0x231a..=0x231b).contains(&cp)
        || (0x2329..=0x232a).contains(&cp)
        || (0x2e80..=0xa4cf).contains(&cp)
        || (0xac00..=0xd7a3).contains(&cp)
        || (0xf900..=0xfaff).contains(&cp)
        || (0xfe10..=0xfe19).contains(&cp)
        || (0xfe30..=0xfe6b).contains(&cp)
        || (0xff01..=0xff60).contains(&cp)
        || (0xffe0..=0xffe6).contains(&cp)
        || (0x3040..=0x30ff).contains(&cp)
        || (0x3100..=0x312f).contains(&cp)
        || (0x3130..=0x318f).contains(&cp)
        || (0x31a0..=0x31ff).contains(&cp)
        || (0x3200..=0x33ff).contains(&cp)
        || (0x3400..=0x4dbf).contains(&cp)
        || (0x4e00..=0x9fff).contains(&cp)
        || (0x20000..=0x3fffd).contains(&cp)
}

fn is_emoji(cp: u32) -> bool {
    (0x1f000..=0x1faff).contains(&cp)
        || (0x2300..=0x23ff).contains(&cp)
        || (0x2600..=0x27bf).contains(&cp)
        || (0x2b50..=0x2b55).contains(&cp)
}

fn is_emoji_cluster(chars: &[char]) -> bool {
    chars.iter().any(|ch| is_emoji(*ch as u32))
        || chars.iter().any(|ch| is_variation_selector(*ch as u32))
        || chars.contains(&'\u{200d}')
        || chars.iter().any(|ch| is_emoji_modifier(*ch as u32))
        || chars
            .iter()
            .any(|ch| (0x1f1e6..=0x1f1ff).contains(&(*ch as u32)))
        || chars.contains(&'\u{20e3}')
}

fn east_asian_width(cp: u32) -> usize {
    if is_cjk(cp) || is_emoji(cp) {
        2
    } else if is_zero_width(cp) {
        0
    } else {
        1
    }
}

fn first_visible_char(chars: &[char]) -> Option<char> {
    chars.iter().copied().find(|ch| !is_zero_width(*ch as u32))
}

fn grapheme_width(grapheme: &str) -> usize {
    if grapheme == "\t" {
        return 3;
    }
    let chars: Vec<char> = grapheme.chars().collect();
    if chars.is_empty() {
        return 0;
    }
    if chars.iter().all(|ch| is_terminal_spacing_mark(*ch as u32)) {
        return chars.len();
    }
    if chars.iter().all(|ch| is_zero_width(*ch as u32)) {
        return 0;
    }
    if is_emoji_cluster(&chars) {
        return 2;
    }

    let Some(base) = first_visible_char(&chars) else {
        return 0;
    };
    let mut width = east_asian_width(base as u32);
    let mut follows_mark = false;
    let mut after_base = false;
    for ch in &chars {
        if !after_base {
            if *ch == base {
                after_base = true;
            }
            continue;
        }
        let cp = *ch as u32;
        if is_terminal_spacing_mark(cp) {
            width += 1;
            follows_mark = false;
        } else if is_combining_mark(cp) {
            follows_mark = true;
        } else if !is_zero_width(cp) {
            if follows_mark || (0xff00..=0xffef).contains(&cp) {
                width += east_asian_width(cp);
            } else if cp == 0x0e33 || cp == 0x0eb3 {
                width += 1;
            }
            follows_mark = false;
        }
    }
    width
}

fn next_char(text: &str, pos: usize) -> Option<(char, usize)> {
    text[pos..]
        .chars()
        .next()
        .map(|ch| (ch, pos + ch.len_utf8()))
}

/// Return the byte end of the next terminal grapheme cluster.
fn next_grapheme_end(text: &str, start: usize) -> usize {
    let Some((first, mut end)) = next_char(text, start) else {
        return start;
    };
    let mut regional_count = if (0x1f1e6..=0x1f1ff).contains(&(first as u32)) {
        1
    } else {
        0
    };
    while let Some((ch, next)) = next_char(text, end) {
        let cp = ch as u32;
        if regional_count == 1 && (0x1f1e6..=0x1f1ff).contains(&cp) {
            end = next;
            regional_count = 2;
            continue;
        }
        if is_combining_mark(cp)
            || is_variation_selector(cp)
            || is_emoji_modifier(cp)
            || is_tag_character(cp)
            || cp == 0x20e3
        {
            end = next;
            continue;
        }
        if cp == 0x200d {
            end = next;
            if let Some((_, after_joined)) = next_char(text, end) {
                end = after_joined;
                continue;
            }
        }
        break;
    }
    end
}

/// Return the UTF-8 byte ranges of terminal grapheme clusters in `text`.
///
/// The TUI stores cursor positions as byte offsets, but editing must move and
/// delete a whole displayed grapheme (including combining marks, emoji ZWJ
/// sequences, and regional-indicator pairs).  Keeping this helper alongside
/// the width implementation makes those two notions of a terminal cell agree.
pub fn grapheme_boundaries(text: &str) -> Vec<(usize, usize)> {
    let mut boundaries = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let end = next_grapheme_end(text, start);
        if end <= start {
            break;
        }
        boundaries.push((start, end));
        start = end;
    }
    boundaries
}

/// Return the byte offset immediately before the grapheme containing `cursor`.
pub fn previous_grapheme_boundary(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    let mut previous_start = 0;
    for (start, end) in grapheme_boundaries(text) {
        if cursor <= start {
            return previous_start;
        }
        if cursor < end {
            return start;
        }
        previous_start = start;
    }
    previous_start
}

/// Return the byte offset immediately after the grapheme at `cursor`.
pub fn next_grapheme_boundary(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    for (start, end) in grapheme_boundaries(text) {
        if cursor == start || (cursor > start && cursor < end) {
            return end;
        }
    }
    text.len()
}

#[derive(Debug, Clone)]
struct TextUnit {
    raw: String,
    width: usize,
}

fn text_units(text: &str) -> Vec<TextUnit> {
    let mut units = Vec::new();
    let mut pos = 0;
    while pos < text.len() {
        if let Some(code) = extract_ansi_code(text, pos) {
            units.push(TextUnit {
                raw: code.code,
                width: 0,
            });
            pos += code.length;
            continue;
        }
        if text.as_bytes()[pos] == 0x1b {
            pos += 1;
            continue;
        }
        let end = next_grapheme_end(text, pos);
        let raw = text[pos..end].to_string();
        let width = grapheme_width(&raw);
        units.push(TextUnit { raw, width });
        pos = end;
    }
    units
}

/// Visible width in terminal cells.
pub fn visible_width(text: &str) -> usize {
    text_units(text).iter().map(|unit| unit.width).sum()
}

#[derive(Debug, Clone, Default)]
struct ActiveHyperlink {
    params: String,
    url: String,
    terminator: String,
}

#[derive(Debug, Clone, Default)]
struct AnsiStyleTracker {
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    blink: bool,
    inverse: bool,
    hidden: bool,
    strike: bool,
    fg: Option<String>,
    bg: Option<String>,
    hyperlink: Option<ActiveHyperlink>,
}

fn parse_osc8(code: &str) -> Option<Option<ActiveHyperlink>> {
    if !code.starts_with("\x1b]8;") {
        return None;
    }
    let terminator = if code.ends_with('\x07') {
        "\x07"
    } else if code.ends_with("\x1b\\") {
        "\x1b\\"
    } else {
        return None;
    };
    let body_end = code.len() - terminator.len();
    let body = &code[4..body_end];
    let separator = body.find(';')?;
    let params = &body[..separator];
    let url = &body[separator + 1..];
    if url.is_empty() {
        Some(None)
    } else {
        Some(Some(ActiveHyperlink {
            params: params.to_string(),
            url: url.to_string(),
            terminator: terminator.to_string(),
        }))
    }
}

fn osc8_open(link: &ActiveHyperlink) -> String {
    format!("\x1b]8;{};{}{}", link.params, link.url, link.terminator)
}

fn osc8_close(link: &ActiveHyperlink) -> String {
    format!("\x1b]8;;{}", link.terminator)
}

impl AnsiStyleTracker {
    fn reset_sgr(&mut self) {
        self.bold = false;
        self.dim = false;
        self.italic = false;
        self.underline = false;
        self.blink = false;
        self.inverse = false;
        self.hidden = false;
        self.strike = false;
        self.fg = None;
        self.bg = None;
    }

    fn process(&mut self, code: &str) {
        if let Some(link) = parse_osc8(code) {
            self.hyperlink = link;
            return;
        }
        if !code.ends_with('m') || !code.starts_with("\x1b[") {
            return;
        }
        let params = &code[2..code.len() - 1];
        if params.is_empty() {
            self.reset_sgr();
            return;
        }
        let values: Vec<&str> = params.split(';').collect();
        let mut i = 0;
        while i < values.len() {
            let value = values[i].parse::<u16>().unwrap_or(0);
            match value {
                0 => self.reset_sgr(),
                1 => self.bold = true,
                2 => self.dim = true,
                3 => self.italic = true,
                4 => self.underline = true,
                5 | 6 => self.blink = true,
                7 => self.inverse = true,
                8 => self.hidden = true,
                9 => self.strike = true,
                21 => self.bold = false,
                22 => {
                    self.bold = false;
                    self.dim = false;
                }
                23 => self.italic = false,
                24 => self.underline = false,
                25 => self.blink = false,
                27 => self.inverse = false,
                28 => self.hidden = false,
                29 => self.strike = false,
                39 => self.fg = None,
                49 => self.bg = None,
                30..=37 | 90..=97 => self.fg = Some(value.to_string()),
                40..=47 | 100..=107 => self.bg = Some(value.to_string()),
                38 | 48 => {
                    let target = if value == 38 {
                        &mut self.fg
                    } else {
                        &mut self.bg
                    };
                    if values.get(i + 1) == Some(&"5") && values.get(i + 2).is_some() {
                        *target =
                            Some(format!("{};{};{}", values[i], values[i + 1], values[i + 2]));
                        i += 2;
                    } else if values.get(i + 1) == Some(&"2") && values.get(i + 4).is_some() {
                        *target = Some(format!(
                            "{};{};{};{};{}",
                            values[i],
                            values[i + 1],
                            values[i + 2],
                            values[i + 3],
                            values[i + 4]
                        ));
                        i += 4;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn active_codes(&self) -> String {
        let mut codes = Vec::new();
        if self.bold {
            codes.push("1".to_string());
        }
        if self.dim {
            codes.push("2".to_string());
        }
        if self.italic {
            codes.push("3".to_string());
        }
        if self.underline {
            codes.push("4".to_string());
        }
        if self.blink {
            codes.push("5".to_string());
        }
        if self.inverse {
            codes.push("7".to_string());
        }
        if self.hidden {
            codes.push("8".to_string());
        }
        if self.strike {
            codes.push("9".to_string());
        }
        if let Some(fg) = &self.fg {
            codes.push(fg.clone());
        }
        if let Some(bg) = &self.bg {
            codes.push(bg.clone());
        }
        let mut result = if codes.is_empty() {
            String::new()
        } else {
            format!("\x1b[{}m", codes.join(";"))
        };
        if let Some(link) = &self.hyperlink {
            result.push_str(&osc8_open(link));
        }
        result
    }

    fn line_end_reset(&self) -> String {
        let mut result = String::new();
        if self.underline {
            result.push_str("\x1b[24m");
        }
        if let Some(link) = &self.hyperlink {
            result.push_str(&osc8_close(link));
        }
        result
    }
}

fn update_tracker(text: &str, tracker: &mut AnsiStyleTracker) {
    for unit in text_units(text) {
        if unit.width == 0 && unit.raw.starts_with('\x1b') {
            tracker.process(&unit.raw);
        }
    }
}

#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
fn split_hard_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut pos = 0;
    while pos < text.len() {
        if let Some(code) = extract_ansi_code(text, pos) {
            current.push_str(&code.code);
            pos += code.length;
            continue;
        }
        let (ch, end) = next_char(text, pos).expect("valid UTF-8 boundary");
        match ch {
            '\n' => lines.push(std::mem::take(&mut current)),
            '\r' => {
                if text[end..].starts_with('\n') {
                    pos = end + 1;
                } else {
                    pos = end;
                }
                lines.push(std::mem::take(&mut current));
                continue;
            }
            _ => current.push(ch),
        }
        pos = end;
    }
    lines.push(current);
    lines
}

#[derive(Debug, Clone)]
struct WrapToken {
    raw: String,
    width: usize,
    whitespace: bool,
}

fn is_cjk_break_grapheme(raw: &str) -> bool {
    raw.chars().any(|ch| is_cjk(ch as u32))
}

fn split_wrap_tokens(text: &str) -> Vec<WrapToken> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    let mut current_whitespace: Option<bool> = None;
    let mut pending_ansi = String::new();

    let flush = |tokens: &mut Vec<WrapToken>,
                 current: &mut String,
                 width: &mut usize,
                 whitespace: &mut Option<bool>| {
        if !current.is_empty() {
            tokens.push(WrapToken {
                raw: std::mem::take(current),
                width: *width,
                whitespace: whitespace.unwrap_or(false),
            });
            *width = 0;
            *whitespace = None;
        }
    };

    for unit in text_units(text) {
        if unit.raw.starts_with('\x1b') && unit.width == 0 {
            pending_ansi.push_str(&unit.raw);
            continue;
        }
        let whitespace = unit.raw == " " || unit.raw == "\t";
        let cjk_break = !whitespace && is_cjk_break_grapheme(&unit.raw);
        if cjk_break {
            flush(
                &mut tokens,
                &mut current,
                &mut current_width,
                &mut current_whitespace,
            );
            let mut raw = std::mem::take(&mut pending_ansi);
            raw.push_str(&unit.raw);
            tokens.push(WrapToken {
                raw,
                width: unit.width,
                whitespace: false,
            });
            continue;
        }
        if current_whitespace.is_some() && current_whitespace != Some(whitespace) {
            flush(
                &mut tokens,
                &mut current,
                &mut current_width,
                &mut current_whitespace,
            );
        }
        if !pending_ansi.is_empty() {
            current.push_str(&pending_ansi);
            pending_ansi.clear();
        }
        current_whitespace = Some(whitespace);
        current.push_str(&unit.raw);
        current_width += unit.width;
    }
    if !pending_ansi.is_empty() {
        current.push_str(&pending_ansi);
    }
    flush(
        &mut tokens,
        &mut current,
        &mut current_width,
        &mut current_whitespace,
    );
    tokens
}

fn break_long_word(word: &str, width: usize, tracker: &mut AnsiStyleTracker) -> Vec<String> {
    if width == 0 {
        return vec![word.to_string()];
    }
    let mut lines = Vec::new();
    let mut current = tracker.active_codes();
    let mut current_width = 0;
    for unit in text_units(word) {
        if unit.raw.starts_with('\x1b') && unit.width == 0 {
            current.push_str(&unit.raw);
            tracker.process(&unit.raw);
            continue;
        }
        if current_width > 0 && current_width + unit.width > width {
            current.push_str(&tracker.line_end_reset());
            lines.push(std::mem::take(&mut current));
            current = tracker.active_codes();
            current_width = 0;
        }
        current.push_str(&unit.raw);
        current_width += unit.width;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn wrap_single_line(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() || width == 0 || visible_width(line) <= width {
        return vec![line.to_string()];
    }
    let mut result = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    let mut tracker = AnsiStyleTracker::default();

    for token in split_wrap_tokens(line) {
        if token.width > width && !token.whitespace {
            if !current.is_empty() {
                current.push_str(&tracker.line_end_reset());
                result.push(current.trim_end_matches(' ').to_string());
            }
            let broken = break_long_word(&token.raw, width, &mut tracker);
            if broken.len() > 1 {
                result.extend(broken[..broken.len() - 1].iter().cloned());
            }
            current = broken.last().cloned().unwrap_or_default();
            current_width = visible_width(&current);
            continue;
        }

        if current_width > 0 && current_width + token.width > width {
            let line_to_push = current.trim_end_matches(' ').to_string();
            let line_end = tracker.line_end_reset();
            result.push(format!("{line_to_push}{line_end}"));
            current = if token.whitespace {
                tracker.active_codes()
            } else {
                format!("{}{}", tracker.active_codes(), token.raw)
            };
            current_width = if token.whitespace { 0 } else { token.width };
        } else {
            current.push_str(&token.raw);
            current_width += token.width;
        }
        update_tracker(&token.raw, &mut tracker);
    }
    if !current.is_empty() {
        // A whitespace token can be wider than the available line after all
        // preceding content has been wrapped. Match upstream's final
        // trimEnd so a narrow terminal never receives an over-wide row.
        result.push(current.trim_end_matches(' ').to_string());
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}

/// Word-wrap text while preserving ANSI styles, OSC8 hyperlinks and grapheme
/// boundaries. LF, CRLF and CR are all hard line breaks.
pub fn wrap_text_with_ansi(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut result = Vec::new();
    let mut tracker = AnsiStyleTracker::default();
    for input_line in split_hard_lines(text) {
        let prefix = if result.is_empty() {
            String::new()
        } else {
            tracker.active_codes()
        };
        let wrapped = wrap_single_line(&format!("{prefix}{input_line}"), width);
        result.extend(wrapped);
        update_tracker(&input_line, &mut tracker);
    }
    result
}

/// Extract a visible-column range, preserving ANSI controls that apply to the
/// selected text. `strict` excludes a wide grapheme if it crosses the right
/// edge of the requested range.
pub fn slice_with_width_info(
    line: &str,
    start_col: usize,
    length: usize,
    strict: bool,
) -> WidthSlice {
    if length == 0 {
        return WidthSlice {
            text: String::new(),
            width: 0,
        };
    }
    let end_col = start_col.saturating_add(length);
    let mut result = String::new();
    let mut result_width = 0;
    let mut current_col = 0;
    let mut pending_ansi = String::new();
    for unit in text_units(line) {
        if unit.raw.starts_with('\x1b') && unit.width == 0 {
            if current_col < start_col {
                pending_ansi.push_str(&unit.raw);
            } else if current_col < end_col {
                result.push_str(&unit.raw);
            }
            continue;
        }
        let in_range = current_col >= start_col && current_col < end_col;
        let fits = !strict || current_col.saturating_add(unit.width) <= end_col;
        if in_range && fits {
            result.push_str(&pending_ansi);
            pending_ansi.clear();
            result.push_str(&unit.raw);
            result_width += unit.width;
        }
        current_col += unit.width;
        if current_col >= end_col {
            break;
        }
    }
    WidthSlice {
        text: result,
        width: result_width,
    }
}

/// Backwards-compatible prefix slice.
pub fn slice_with_width(text: &str, width: usize) -> String {
    slice_with_width_info(text, 0, width, false).text
}

/// Slice a line by visible columns.
pub fn slice_by_column(line: &str, start_col: usize, length: usize) -> String {
    slice_with_width_info(line, start_col, length, false).text
}

/// Strict visible-column slice used by overlay compositing.
pub fn slice_by_column_strict(line: &str, start_col: usize, length: usize) -> String {
    slice_with_width_info(line, start_col, length, true).text
}

/// Return the terminal-cell range occupied by the grapheme at `column`.
pub fn get_grapheme_cell_range(line: &str, column: usize) -> Option<GraphemeCellRange> {
    let mut current = 0;
    for unit in text_units(line) {
        if unit.raw.starts_with('\x1b') && unit.width == 0 {
            continue;
        }
        if unit.width > 0 && column >= current && column < current + unit.width {
            return Some(GraphemeCellRange {
                start: current,
                end: current + unit.width,
            });
        }
        current += unit.width;
    }
    None
}

/// Return the URL of the OSC8 hyperlink covering `column`, if any.
pub fn get_osc8_link_at_column(line: &str, column: usize) -> Option<String> {
    let mut active = None;
    let mut current = 0;
    for unit in text_units(line) {
        if unit.raw.starts_with('\x1b') && unit.width == 0 {
            if let Some(link) = parse_osc8(&unit.raw) {
                active = link;
            }
            continue;
        }
        if column >= current && column < current + unit.width {
            return active.map(|link: ActiveHyperlink| link.url);
        }
        current += unit.width;
    }
    None
}

/// Normalize terminal-only output quirks without mutating logical editor
/// content. Tabs outside controls use the fixed three-cell policy.
pub fn normalize_terminal_output(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for unit in text_units(text) {
        if unit.raw.starts_with('\x1b') && unit.width == 0 {
            out.push_str(&unit.raw);
            continue;
        }
        match unit.raw.as_str() {
            "\t" => out.push_str("   "),
            "\u{0e33}" => out.push_str("\u{0e4d}\u{0e32}"),
            "\u{0eb3}" => out.push_str("\u{0ecd}\u{0eb2}"),
            _ => out.push_str(&unit.raw),
        }
    }
    out
}

fn active_tracker_at(line: &str, end_col: usize) -> AnsiStyleTracker {
    let mut tracker = AnsiStyleTracker::default();
    let mut col = 0;
    for unit in text_units(line) {
        if unit.raw.starts_with('\x1b') && unit.width == 0 {
            tracker.process(&unit.raw);
            continue;
        }
        if col >= end_col {
            break;
        }
        col += unit.width;
    }
    tracker
}

/// Extract the base prefix and post-overlay suffix. The suffix starts with
/// the style active at `after_start`, preventing style or OSC8 leakage across
/// an overlay.
pub fn extract_segments(
    line: &str,
    before_end: usize,
    after_start: usize,
    after_len: usize,
    strict_after: bool,
) -> ExtractedSegments {
    let before = slice_with_width_info(line, 0, before_end, true);
    let mut after = slice_with_width_info(line, after_start, after_len, strict_after);
    if !after.text.is_empty() {
        let tracker = active_tracker_at(line, after_start);
        let inherited = tracker.active_codes();
        if !inherited.is_empty() && !after.text.starts_with(&inherited) {
            after.text = format!("{inherited}{}", after.text);
        }
    }
    ExtractedSegments {
        before: before.text,
        before_width: before.width,
        after: after.text,
        after_width: after.width,
    }
}

fn truncate_impl(text: &str, max_width: usize, ellipsis: &str, pad: bool) -> String {
    if max_width == 0 {
        return String::new();
    }
    let width = visible_width(text);
    if width <= max_width {
        if pad {
            return format!("{}{}", text, " ".repeat(max_width - width));
        }
        return text.to_string();
    }

    let ellipsis_width = visible_width(ellipsis);
    if ellipsis_width >= max_width {
        let clipped = slice_with_width_info(ellipsis, 0, max_width, true);
        if clipped.width == 0 {
            return if pad {
                " ".repeat(max_width)
            } else {
                String::new()
            };
        }
        let mut result = format!("\x1b[0m{}\x1b[0m", clipped.text);
        if pad {
            result.push_str(&" ".repeat(max_width - clipped.width));
        }
        return result;
    }

    let target = max_width - ellipsis_width;
    let mut prefix = String::new();
    let mut prefix_width = 0;
    let mut pending_ansi = String::new();
    for unit in text_units(text) {
        if unit.raw.starts_with('\x1b') && unit.width == 0 {
            pending_ansi.push_str(&unit.raw);
            continue;
        }
        if prefix_width + unit.width > target {
            break;
        }
        prefix.push_str(&pending_ansi);
        pending_ansi.clear();
        prefix.push_str(&unit.raw);
        prefix_width += unit.width;
    }
    let tracker = active_tracker_at(&prefix, prefix_width);
    let mut result = prefix;
    if let Some(link) = tracker.hyperlink {
        result.push_str(&osc8_close(&link));
    }
    result.push_str("\x1b[0m");
    result.push_str(ellipsis);
    result.push_str("\x1b[0m");
    if pad {
        result.push_str(&" ".repeat(max_width - ellipsis_width - prefix_width));
    }
    result
}

/// Truncate to a visible width, appending `ellipsis` only when needed.
pub fn truncate_to_width(text: &str, max_width: usize, ellipsis: &str) -> String {
    truncate_impl(text, max_width, ellipsis, false)
}

/// Upstream-compatible truncation variant that pads to exactly `max_width`
/// terminal cells.
pub fn truncate_to_width_padded(text: &str, max_width: usize, ellipsis: &str) -> String {
    truncate_impl(text, max_width, ellipsis, true)
}

/// Apply a background function after padding a line to a terminal width.
pub fn apply_background_to_line(line: &str, width: usize, bg: &dyn Fn(&str) -> String) -> String {
    let visible = visible_width(line);
    let padded = if visible < width {
        format!("{}{}", line, " ".repeat(width - visible))
    } else {
        line.to_string()
    };
    bg(&padded)
}

pub fn is_whitespace_char(ch: char) -> bool {
    ch.is_whitespace()
}

pub fn is_punctuation_char(ch: char) -> bool {
    matches!(
        ch,
        '(' | ')'
            | '{'
            | '}'
            | '['
            | ']'
            | '<'
            | '>'
            | '.'
            | ','
            | ';'
            | ':'
            | '\''
            | '"'
            | '!'
            | '?'
            | '+'
            | '-'
            | '='
            | '*'
            | '/'
            | '\\'
            | '|'
            | '&'
            | '%'
            | '^'
            | '$'
            | '#'
            | '@'
            | '~'
            | '`'
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn width_matches_terminal_cells_not_scalar_count() {
        assert_eq!(visible_width("\x1b[31mred\x1b[0m"), 3);
        assert_eq!(visible_width("\t\x1b[31m界\x1b[0m"), 5);
        assert_eq!(visible_width("e\u{301}"), 1);
        assert_eq!(visible_width("网络"), 4);
        assert_eq!(visible_width("🙂界"), 4);
    }

    #[test]
    fn width_covers_upstream_script_regressions() {
        for value in ["र्क", "র্ক", "ર્ક", "ର୍କ", "ర్క", "ര്‍ക"]
        {
            assert_eq!(visible_width(value), 2, "{value}");
        }
        assert_eq!(visible_width("नेटवर्क"), 5);
        assert_eq!(visible_width("सर्वाधिकार सुरक्षित। ऑर्डर पर क्लिक करें"), 33);
        for value in ["ကာ", "ကေ", "က်", "ကျ", "ကြ", "ကဳ", "ကဴ", "ကဵ", "ကး"]
        {
            assert_eq!(visible_width(value), 2, "{value}");
        }
        assert_eq!(visible_width("ကို"), 1);
        assert_eq!(visible_width("က္"), 1);
        assert_eq!(visible_width("กำ"), 2);
        assert_eq!(visible_width("ກຳ"), 2);
    }

    #[test]
    fn isolated_regional_indicators_are_wide() {
        assert_eq!(visible_width("🇨"), 2);
        assert_eq!(visible_width("🇨🇳"), 2);
        assert_eq!(visible_width("🇦🇧🇨"), 4);
    }

    #[test]
    fn strip_handles_csi_osc_apc_and_malformed_sequences() {
        assert_eq!(
            strip_terminal_sequences("\x1b]133;A\x07hi\x1b]133;B\x1b\\"),
            "hi"
        );
        assert_eq!(strip_terminal_sequences("\x1b_Gi=1;abc\x07text"), "text");
        assert_eq!(strip_terminal_sequences("a\x1bnot-ansi b"), "aot-ansi b");
        assert_eq!(visible_width("abc\x1b[31"), 6);
    }

    #[test]
    fn malformed_escape_before_unicode_is_width_safe() {
        let value = "\x1b界";
        assert!(extract_ansi_code(value, 0).is_none());
        assert_eq!(strip_terminal_sequences(value), "界");
        assert_eq!(visible_width(value), 2);
    }

    #[test]
    fn wraps_hard_breaks_cjk_and_preserves_style() {
        assert_eq!(
            wrap_text_with_ansi("first\nsecond\r\nthird\rfourth", 80),
            vec!["first", "second", "third", "fourth"]
        );
        let red = "\x1b[31m";
        let reset = "\x1b[0m";
        assert_eq!(
            wrap_text_with_ansi(&format!("{red}first\r\nsecond\rthird{reset}"), 80),
            vec![
                format!("{red}first"),
                format!("{red}second"),
                format!("{red}third{reset}")
            ]
        );
        let text = "This is an example 中文汉字测试段落内容中文汉字测试段落内容.";
        let lines = wrap_text_with_ansi(text, 40);
        assert_eq!(
            lines,
            vec![
                "This is an example 中文汉字测试段落内容",
                "中文汉字测试段落内容."
            ]
        );
        assert!(lines.iter().all(|line| visible_width(line) <= 40));
    }

    #[test]
    fn narrow_wrap_trims_terminal_trailing_spaces() {
        let lines = wrap_text_with_ansi("  ", 1);
        assert_eq!(lines, vec![String::new()]);
        assert!(lines.iter().all(|line| visible_width(line) <= 1));
    }

    #[test]
    fn wraps_hyperlinks_with_original_terminator() {
        let open = "\x1b]8;;https://example.com\x07";
        let close = "\x1b]8;;\x07";
        let lines = wrap_text_with_ansi(&format!("{open}0123456789{close}"), 6);
        assert!(lines.len() > 1);
        assert!(lines[0].ends_with(close));
        assert!(lines[1].starts_with(open));
        assert!(lines.iter().all(|line| visible_width(line) <= 6));
    }

    #[test]
    fn slices_respect_grapheme_and_strict_wide_boundaries() {
        assert_eq!(slice_with_width("hello world", 5), "hello");
        assert_eq!(slice_with_width("🙂界", 2), "🙂");
        assert_eq!(slice_by_column_strict("a界b", 1, 1), "");
        assert_eq!(slice_by_column_strict("a界b", 1, 2), "界");
        assert_eq!(
            get_grapheme_cell_range("a界b", 1),
            Some(GraphemeCellRange { start: 1, end: 3 })
        );
    }

    #[test]
    fn cursor_grapheme_boundaries_do_not_skip_partial_unicode_clusters() {
        let text = "a界b";
        assert_eq!(previous_grapheme_boundary(text, 1), 0);
        assert_eq!(previous_grapheme_boundary(text, 2), 1);
        assert_eq!(next_grapheme_boundary(text, 2), 4);
        assert_eq!(next_grapheme_boundary(text, 4), 5);
    }

    #[test]
    fn slices_and_extracts_osc8_without_style_leaks() {
        let open = "\x1b]8;;https://example.com\x1b\\";
        let close = "\x1b]8;;\x1b\\";
        let line = format!("base {open}link{close} tail");
        assert_eq!(
            get_osc8_link_at_column(&line, 6),
            Some("https://example.com".to_string())
        );
        assert_eq!(get_osc8_link_at_column(&line, 11), None);
        let extracted = extract_segments("\x1b[31m0123456789", 3, 6, 3, true);
        assert_eq!(extracted.before_width, 3);
        assert_eq!(extracted.after_width, 3);
        assert!(extracted.after.starts_with("\x1b[31m"));
    }

    #[test]
    fn truncation_is_contiguous_and_width_safe() {
        assert_eq!(
            truncate_to_width("hello world", 5, "..."),
            "he\x1b[0m...\x1b[0m"
        );
        assert_eq!(truncate_to_width("hello", 8, "..."), "hello");
        assert_eq!(truncate_to_width("abcdef", 1, "🙂"), "");
        assert_eq!(truncate_to_width("abcdef", 2, "🙂"), "\x1b[0m🙂\x1b[0m");
        let styled = truncate_to_width("\x1b[31mhellohellohello", 10, "");
        assert!(styled.ends_with("\x1b[0m"));
        let contiguous = truncate_to_width_padded("🙂\t界 \x1b_abc\x07", 7, "…");
        assert_eq!(contiguous, "🙂\t\x1b[0m…\x1b[0m ");
        assert_eq!(visible_width(&contiguous), 7);
    }

    #[test]
    fn normalize_only_changes_terminal_text() {
        assert_eq!(normalize_terminal_output("ำ"), "ํา");
        assert_eq!(normalize_terminal_output("ຳ"), "ໍາ");
        assert_eq!(normalize_terminal_output("a\tb"), "a   b");
        let osc = "\x1b]8;;a\tb\x07x";
        assert_eq!(normalize_terminal_output(osc), osc);
    }

    #[test]
    fn background_padding_uses_visible_width() {
        let result = apply_background_to_line("界", 4, &|value| format!("<{value}>"));
        assert_eq!(result, "<界  >");
    }
}
