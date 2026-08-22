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

impl TuiKey {
    pub fn simple(base: impl Into<String>) -> Self {
        Self { base: base.into(), ctrl: false, shift: false, alt: false }
    }
    pub fn ctrl(base: impl Into<String>) -> Self {
        Self { base: base.into(), ctrl: true, shift: false, alt: false }
    }
    pub fn shift(base: impl Into<String>) -> Self {
        Self { base: base.into(), ctrl: false, shift: true, alt: false }
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
        parts.push(if self.ctrl && matches!(base.as_str(), "c" | "d" | "z" | "a" | "l" | "u" | "w" | "e" | "x" | "y" | "r" | "t" | "n" | "p" | "k" | "b" | "f" | "h" | "g" | "i" | "o" | "s" | "m" | "j" | "q" | "v") {
            Box::leak(base.into_boxed_str())
        } else {
            Box::leak(base.into_boxed_str())
        });
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
    if raw.starts_with("ctrl+") {
        TuiKey { base: raw.trim_start_matches("ctrl+").to_string(), ctrl: true, shift: false, alt: false }
    } else if raw.starts_with("shift+") {
        TuiKey { base: raw.trim_start_matches("shift+").to_string(), ctrl: false, shift: true, alt: false }
    } else if raw.starts_with("alt+") {
        TuiKey { base: raw.trim_start_matches("alt+").to_string(), ctrl: false, shift: false, alt: true }
    } else {
        TuiKey::simple(raw)
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
}
