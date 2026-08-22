//! Ring buffer for Emacs-style kill/yank operations — port of
//! `packages/tui/src/kill-ring.ts`.
//!
//! Tracks killed (deleted) text entries. Consecutive kills can accumulate
//! into a single entry. Supports yank (paste most recent) and yank-pop
//! (cycle through older entries).

/// Ring buffer of killed-text entries.
#[derive(Debug, Default, Clone)]
pub struct KillRing {
    ring: Vec<String>,
}

/// Push options controlling accumulation behavior.
#[derive(Debug, Clone, Copy)]
pub struct KillRingPushOptions {
    /// If accumulating, prepend (backward deletion) or append (forward deletion).
    pub prepend: bool,
    /// Merge with the most recent entry instead of creating a new one.
    pub accumulate: bool,
}

impl KillRing {
    pub fn new() -> Self {
        Self { ring: Vec::new() }
    }

    /// Add text to the kill ring.
    pub fn push(&mut self, text: &str, opts: KillRingPushOptions) {
        if text.is_empty() {
            return;
        }
        if opts.accumulate && !self.ring.is_empty() {
            let last = self.ring.pop().unwrap_or_default();
            self.ring.push(if opts.prepend { format!("{text}{last}") } else { format!("{last}{text}") });
        } else {
            self.ring.push(text.to_string());
        }
    }

    /// Get most recent entry without modifying the ring.
    pub fn peek(&self) -> Option<&str> {
        self.ring.last().map(|s| s.as_str())
    }

    /// Move last entry to front (for yank-pop cycling).
    pub fn rotate(&mut self) {
        if self.ring.len() > 1 {
            let last = self.ring.pop().unwrap();
            self.ring.insert(0, last);
        }
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_creates_entries() {
        let mut ring = KillRing::new();
        ring.push("hello", KillRingPushOptions { prepend: false, accumulate: false });
        ring.push("world", KillRingPushOptions { prepend: false, accumulate: false });
        assert_eq!(ring.len(), 2);
        assert_eq!(ring.peek(), Some("world"));
    }

    #[test]
    fn accumulate_appends_forward_deletions() {
        let mut ring = KillRing::new();
        ring.push("ab", KillRingPushOptions { prepend: false, accumulate: false });
        ring.push("cd", KillRingPushOptions { prepend: false, accumulate: true });
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.peek(), Some("abcd"));
    }

    #[test]
    fn accumulate_prepends_backward_deletions() {
        let mut ring = KillRing::new();
        ring.push("cd", KillRingPushOptions { prepend: false, accumulate: false });
        ring.push("ab", KillRingPushOptions { prepend: true, accumulate: true });
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.peek(), Some("abcd"));
    }

    #[test]
    fn empty_text_is_ignored() {
        let mut ring = KillRing::new();
        ring.push("", KillRingPushOptions { prepend: false, accumulate: false });
        assert_eq!(ring.len(), 0);
    }

    #[test]
    fn rotate_cycles_entries() {
        let mut ring = KillRing::new();
        ring.push("a", KillRingPushOptions { prepend: false, accumulate: false });
        ring.push("b", KillRingPushOptions { prepend: false, accumulate: false });
        ring.push("c", KillRingPushOptions { prepend: false, accumulate: false });
        ring.rotate();
        assert_eq!(ring.peek(), Some("b"));
        ring.rotate();
        assert_eq!(ring.peek(), Some("a"));
    }
}
