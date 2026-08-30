//! StdinBuffer buffers input and emits complete sequences — port of
//! `packages/tui/src/stdin-buffer.ts`.
//!
//! Stdin data events can arrive in partial chunks, especially for escape
//! sequences like mouse events. Without buffering, partial sequences can be
//! misinterpreted as regular keypresses. `process` accumulates input until a
//! complete sequence is detected and returns the emitted sequences; `flush`
//! forces out any buffered remainder (the upstream component uses timers to
//! call this — the port exposes it explicitly for the event loop to drive).
//!
//! Based on code from OpenTUI (https://github.com/anomalyco/opentui)
//! MIT License - Copyright (c) 2025 opentui

const ESC: &str = "\x1b";

/// Maximum time the upstream Pi buffer waits for a non-ESC sequence
/// fragment before flushing it as input.
pub const DEFAULT_SEQUENCE_TIMEOUT_MS: u64 = 50;
/// Maximum time the upstream Pi buffer waits after a lone ESC. This is
/// overridden by the terminal backend for high-latency SSH sessions.
pub const DEFAULT_ESCAPE_TIMEOUT_MS: u64 = 10;

const BRACKETED_PASTE_START: &str = "\x1b[200~";
const BRACKETED_PASTE_END: &str = "\x1b[201~";

/// Sequence completeness status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceStatus {
    Complete,
    Incomplete,
    NotEscape,
}

fn is_complete_sequence(data: &str) -> SequenceStatus {
    if !data.starts_with(ESC) {
        return SequenceStatus::NotEscape;
    }
    if data.len() == 1 {
        return SequenceStatus::Incomplete;
    }

    let after_esc = &data[1..];

    // CSI sequences: ESC [
    if let Some(rest) = after_esc.strip_prefix('[') {
        // Old-style mouse: ESC[M + 3 bytes = 6 total
        if let Some(mouse) = rest.strip_prefix('M') {
            let _ = mouse;
            return if data.len() >= 6 {
                SequenceStatus::Complete
            } else {
                SequenceStatus::Incomplete
            };
        }
        return is_complete_csi_sequence(data);
    }

    // OSC sequences: ESC ]
    if after_esc.starts_with(']') {
        return is_complete_osc_sequence(data);
    }

    // DCS sequences: ESC P ... ESC \
    if after_esc.starts_with('P') {
        return is_complete_dcs_sequence(data);
    }

    // APC sequences: ESC _ ... ESC \
    if after_esc.starts_with('_') {
        return is_complete_apc_sequence(data);
    }

    // SS3 sequences: ESC O
    if after_esc.starts_with('O') {
        return if after_esc.len() >= 2 {
            SequenceStatus::Complete
        } else {
            SequenceStatus::Incomplete
        };
    }

    // Meta key sequences: ESC followed by a single character
    if after_esc.chars().count() == 1 {
        return SequenceStatus::Complete;
    }

    // Unknown escape sequence - treat as complete
    SequenceStatus::Complete
}

/// CSI sequences end with a byte in 0x40-0x7E.
fn is_complete_csi_sequence(data: &str) -> SequenceStatus {
    if !data.starts_with("\x1b[") {
        return SequenceStatus::Complete;
    }
    if data.len() < 3 {
        return SequenceStatus::Incomplete;
    }
    let payload = &data[2..];
    let last_char = payload.chars().last().unwrap_or_default();
    let last_code = last_char as u32;

    if (0x40..=0x7e).contains(&last_code) {
        // Special handling for SGR mouse sequences: ESC[<B;X;Ym / ESC[<B;X;YM
        if let Some(mouse) = payload.strip_prefix('<') {
            // Only M/m terminate an SGR mouse report. Other CSI final bytes
            // such as A must remain buffered, matching upstream's strict
            // mouse grammar instead of being dispatched as a mouse event.
            let Some(without_last) = mouse.strip_suffix('M').or_else(|| mouse.strip_suffix('m'))
            else {
                return SequenceStatus::Incomplete;
            };
            let parts: Vec<&str> = without_last.split(';').collect();
            if parts.len() == 3
                && parts
                    .iter()
                    .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
            {
                return SequenceStatus::Complete;
            }
            return SequenceStatus::Incomplete;
        }
        return SequenceStatus::Complete;
    }

    SequenceStatus::Incomplete
}

/// OSC sequences end with ST (ESC \) or BEL.
fn is_complete_osc_sequence(data: &str) -> SequenceStatus {
    if !data.starts_with("\x1b]") {
        return SequenceStatus::Complete;
    }
    if data.ends_with("\x1b\\") || data.ends_with('\x07') {
        SequenceStatus::Complete
    } else {
        SequenceStatus::Incomplete
    }
}

/// DCS sequences end with ST (ESC \).
fn is_complete_dcs_sequence(data: &str) -> SequenceStatus {
    if !data.starts_with("\x1bP") {
        return SequenceStatus::Complete;
    }
    if data.ends_with("\x1b\\") {
        SequenceStatus::Complete
    } else {
        SequenceStatus::Incomplete
    }
}

/// APC sequences end with ST (ESC \).
fn is_complete_apc_sequence(data: &str) -> SequenceStatus {
    if !data.starts_with("\x1b_") {
        return SequenceStatus::Complete;
    }
    if data.ends_with("\x1b\\") {
        SequenceStatus::Complete
    } else {
        SequenceStatus::Incomplete
    }
}

/// Parse an unmodified Kitty printable codepoint (`ESC [ <n> u`, with optional
/// modifier/event-type args) returning the codepoint when simple.
fn parse_unmodified_kitty_printable_codepoint(sequence: &str) -> Option<u32> {
    // /^\x1b\[(\d+)(?::\d*)?(?::\d+)?u$/
    let rest = sequence.strip_prefix("\x1b[")?;
    let rest = rest.strip_suffix('u')?;
    let mut parts = rest.split(':');
    let digits = parts.next()?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    for extra in parts {
        if !extra.is_empty() && !extra.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
    }
    let codepoint: u32 = digits.parse().ok()?;
    if codepoint >= 32 {
        Some(codepoint)
    } else {
        None
    }
}

fn extract_complete_sequences(buffer: &str) -> (Vec<String>, String) {
    let mut sequences: Vec<String> = Vec::new();
    let bytes = buffer.as_bytes();
    let mut pos = 0usize;

    while pos < bytes.len() {
        let remaining = &bytes[pos..];
        if !remaining.starts_with(b"\x1b") {
            // Not an escape sequence - take a single character.
            let ch = std::str::from_utf8(remaining)
                .ok()
                .and_then(|s| s.chars().next());
            match ch {
                Some(c) => {
                    sequences.push(c.to_string());
                    pos += c.len_utf8();
                }
                None => {
                    // Invalid UTF-8: synthesize one byte forward to avoid hanging.
                    sequences.push("\u{fffd}".to_string());
                    pos += 1;
                }
            }
            continue;
        }

        let remaining_str = std::str::from_utf8(remaining).unwrap_or("");
        // We are at an ESC; walk forward looking for a complete sequence.
        let mut seq_end = 1;
        let mut found = false;
        while seq_end <= remaining_str.len() {
            // candidate must end at a char boundary
            if !remaining_str.is_char_boundary(seq_end) {
                seq_end += 1;
                continue;
            }
            let candidate = &remaining_str[..seq_end];
            let status = is_complete_sequence(candidate);

            if status == SequenceStatus::Complete {
                // WezTerm Escape-key regression: '\x1b\x1b' followed by the
                // start of a new escape sequence emits a lone ESC.
                if candidate == "\x1b\x1b" {
                    let next_start = seq_end;
                    let next = remaining_str[next_start..].chars().next();
                    if matches!(
                        next,
                        Some('[') | Some(']') | Some('O') | Some('P') | Some('_')
                    ) {
                        sequences.push(ESC.to_string());
                        pos += 1;
                        found = true;
                        break;
                    }
                }
                sequences.push(candidate.to_string());
                pos += seq_end;
                found = true;
                break;
            } else if status == SequenceStatus::Incomplete {
                seq_end += 1;
                continue;
            } else {
                // NotEscape (shouldn't happen when starting with ESC)
                sequences.push(candidate.to_string());
                pos += seq_end;
                found = true;
                break;
            }
        }

        if !found {
            return (sequences, buffer[pos..].to_string());
        }
    }

    (sequences, String::new())
}

/// Buffers raw terminal input and extracts complete sequences.
#[derive(Debug, Default, Clone)]
pub struct StdinBuffer {
    buffer: String,
    paste_mode: bool,
    paste_buffer: String,
    pending_kitty_printable_codepoint: Option<u32>,
}

impl StdinBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw input data. Returns the sequences that became complete.
    pub fn process(&mut self, data: &str) -> Vec<String> {
        let mut emitted: Vec<String> = Vec::new();

        let high_byte = data.len() == 1 && data.as_bytes()[0] > 127;
        if data.is_empty() && self.buffer.is_empty() {
            emitted.push(String::new());
            return emitted;
        }

        if high_byte {
            // High-byte conversion: single byte > 127 -> ESC + (byte - 128).
            let byte = data.as_bytes()[0] - 128;
            self.buffer.push('\x1b');
            self.buffer.push(char::from(byte));
        } else {
            // Keep the common one-keystroke path allocation-free. The old
            // implementation cloned every input fragment before appending it.
            self.buffer.push_str(data);
        }

        if self.paste_mode {
            self.paste_buffer.push_str(&self.buffer);
            self.buffer.clear();
            let (content, remaining) = Self::consume_paste_end(&self.paste_buffer);
            if let Some(content) = content {
                self.paste_mode = false;
                self.paste_buffer.clear();
                self.pending_kitty_printable_codepoint = None;
                // Keep the event shape expected by Editor. The upstream
                // terminal layer emits a dedicated paste event and then
                // re-wraps its content with bracketed-paste markers before
                // forwarding it to components; this buffer has one string
                // event channel, so preserve that same boundary here.
                emitted.push(format!(
                    "{BRACKETED_PASTE_START}{content}{BRACKETED_PASTE_END}"
                ));
                if !remaining.is_empty() {
                    emitted.extend(self.process(&remaining));
                }
            }
            return emitted;
        }

        if let Some(start_index) = self.buffer.find(BRACKETED_PASTE_START) {
            if start_index > 0 {
                let before_paste = &self.buffer[..start_index];
                let (seqs, _) = extract_complete_sequences(before_paste);
                emitted.extend(seqs);
            }
            self.pending_kitty_printable_codepoint = None;
            self.paste_buffer = self
                .buffer
                .split_off(start_index + BRACKETED_PASTE_START.len());
            self.buffer.clear();
            self.paste_mode = true;

            let (content, remaining) = Self::consume_paste_end(&self.paste_buffer);
            if let Some(content) = content {
                self.paste_mode = false;
                self.paste_buffer.clear();
                self.pending_kitty_printable_codepoint = None;
                emitted.push(format!(
                    "{BRACKETED_PASTE_START}{content}{BRACKETED_PASTE_END}"
                ));
                if !remaining.is_empty() {
                    emitted.extend(self.process(&remaining));
                }
            }
            return emitted;
        }

        let (sequences, remainder) = extract_complete_sequences(&self.buffer);
        self.buffer = remainder;
        for sequence in sequences {
            if self.emit_data_sequence(&sequence) {
                emitted.push(sequence);
            }
        }
        emitted
    }

    fn consume_paste_end(paste_buffer: &str) -> (Option<String>, String) {
        match paste_buffer.find(BRACKETED_PASTE_END) {
            Some(end_index) => {
                let pasted = paste_buffer[..end_index].to_string();
                let remaining = paste_buffer[end_index + BRACKETED_PASTE_END.len()..].to_string();
                (Some(pasted), remaining)
            }
            None => (None, String::new()),
        }
    }

    /// Emit a sequence, deduplicating Kitty printable-codepoint duplicates.
    /// Returns whether the sequence should be emitted.
    fn emit_data_sequence(&mut self, sequence: &str) -> bool {
        let raw_codepoint = if sequence.chars().count() == 1 {
            sequence.chars().next().map(|c| c as u32)
        } else {
            None
        };
        if let Some(cp) = raw_codepoint {
            if Some(cp) == self.pending_kitty_printable_codepoint {
                self.pending_kitty_printable_codepoint = None;
                return false;
            }
        }
        self.pending_kitty_printable_codepoint =
            parse_unmodified_kitty_printable_codepoint(sequence);
        true
    }

    /// Force-flush the buffered remainder as a single sequence.
    pub fn flush(&mut self) -> Vec<String> {
        if self.buffer.is_empty() {
            return Vec::new();
        }
        let sequences = vec![self.buffer.clone()];
        self.buffer.clear();
        self.pending_kitty_printable_codepoint = None;
        sequences
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.paste_mode = false;
        self.paste_buffer.clear();
        self.pending_kitty_printable_codepoint = None;
    }

    /// Release all buffered input.  The event-loop implementation has no
    /// timer handle to cancel here; its timeout owner calls [`flush`] first,
    /// while this method preserves the upstream `destroy()` lifecycle API.
    pub fn destroy(&mut self) {
        self.clear();
    }

    pub fn get_buffer(&self) -> &str {
        &self.buffer
    }

    /// Whether the pending bytes are exactly a lone Escape key. Pi gives this
    /// case a shorter timeout so Alt/meta input stays responsive.
    pub fn is_lone_escape_pending(&self) -> bool {
        self.buffer == ESC
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn passes_through_regular_characters_immediately() {
        let mut buffer = StdinBuffer::new();
        assert_eq!(buffer.process("a"), vec!["a"]);
    }

    #[test]
    fn passes_through_multiple_regular_characters() {
        let mut buffer = StdinBuffer::new();
        assert_eq!(buffer.process("abc"), vec!["a", "b", "c"]);
    }

    #[test]
    fn handles_unicode_characters() {
        let mut buffer = StdinBuffer::new();
        assert_eq!(
            buffer.process("hello 世界"),
            vec!["h", "e", "l", "l", "o", " ", "世", "界"]
        );
    }

    #[test]
    fn passes_through_complete_escape_sequences() {
        let mut buffer = StdinBuffer::new();
        assert_eq!(buffer.process("\x1b[<35;20;5m"), vec!["\x1b[<35;20;5m"]);
        let mut buffer = StdinBuffer::new();
        assert_eq!(buffer.process("\x1b[A"), vec!["\x1b[A"]);
        let mut buffer = StdinBuffer::new();
        assert_eq!(buffer.process("\x1b[11~"), vec!["\x1b[11~"]);
        let mut buffer = StdinBuffer::new();
        assert_eq!(buffer.process("\x1ba"), vec!["\x1ba"]);
        let mut buffer = StdinBuffer::new();
        assert_eq!(buffer.process("\x1bOA"), vec!["\x1bOA"]);
    }

    #[test]
    fn buffers_incomplete_mouse_sgr_sequence() {
        let mut buffer = StdinBuffer::new();
        assert!(buffer.process("\x1b").is_empty());
        assert_eq!(buffer.get_buffer(), "\x1b");
        assert!(buffer.process("[<35").is_empty());
        assert_eq!(buffer.get_buffer(), "\x1b[<35");
        assert_eq!(buffer.process(";20;5m"), vec!["\x1b[<35;20;5m"]);
        assert_eq!(buffer.get_buffer(), "");
    }

    #[test]
    fn keeps_non_mouse_csi_final_bytes_buffered() {
        let mut buffer = StdinBuffer::new();
        let malformed = "\x1b[<35;20;5A";

        assert!(buffer.process(malformed).is_empty());
        assert_eq!(buffer.get_buffer(), malformed);
        assert_eq!(buffer.flush(), vec![malformed]);
    }

    #[test]
    fn buffers_incomplete_csi_sequence() {
        let mut buffer = StdinBuffer::new();
        assert!(buffer.process("\x1b[").is_empty());
        assert!(buffer.process("1;").is_empty());
        assert_eq!(buffer.process("5H"), vec!["\x1b[1;5H"]);
    }

    #[test]
    fn buffers_split_across_many_chunks() {
        let mut buffer = StdinBuffer::new();
        for piece in ["\x1b", "[", "<", "3", "5", ";", "2", "0", ";", "5"] {
            assert!(buffer.process(piece).is_empty());
        }
        // The final byte completes the sequence.
        assert_eq!(buffer.process("m"), vec!["\x1b[<35;20;5m"]);
        assert_eq!(buffer.get_buffer(), "");
    }

    #[test]
    fn flushes_incomplete_sequence_after_timeout() {
        let mut buffer = StdinBuffer::new();
        assert!(buffer.process("\x1b[<35").is_empty());
        // Upstream waits for the timeout then flushes; the Rust port exposes
        // the flush explicitly.
        assert_eq!(buffer.flush(), vec!["\x1b[<35"]);
    }

    #[test]
    fn flushes_lone_esc_as_escape_when_cr_arrives_after_timeout() {
        let mut buffer = StdinBuffer::new();
        assert!(buffer.process("\x1b").is_empty());
        assert_eq!(buffer.flush(), vec!["\x1b"]);
        assert_eq!(buffer.process("\r"), vec!["\r"]);
    }

    #[test]
    fn merges_esc_plus_cr_split_across_chunks() {
        let mut buffer = StdinBuffer::new();
        assert!(buffer.process("\x1b").is_empty());
        assert_eq!(buffer.process("\r"), vec!["\x1b\r"]);
    }

    #[test]
    fn does_not_apply_sequence_timeout_to_lone_esc() {
        let mut buffer = StdinBuffer::new();
        assert!(buffer.process("\x1b").is_empty());
        assert_eq!(buffer.flush(), vec!["\x1b"]);
        assert_eq!(buffer.process("\r"), vec!["\r"]);
    }

    #[test]
    fn keeps_fragmented_mouse_sequences_buffered() {
        let mut buffer = StdinBuffer::new();
        assert!(buffer.process("\x1b[").is_empty());
        assert_eq!(buffer.process("<65;48;39M"), vec!["\x1b[<65;48;39M"]);
    }

    #[test]
    fn handles_characters_before_escape_sequence() {
        let mut buffer = StdinBuffer::new();
        assert_eq!(buffer.process("abc\x1b[A"), vec!["a", "b", "c", "\x1b[A"]);
    }

    #[test]
    fn handles_escape_sequence_before_characters() {
        let mut buffer = StdinBuffer::new();
        assert_eq!(buffer.process("\x1b[Aabc"), vec!["\x1b[A", "a", "b", "c"]);
    }

    #[test]
    fn handles_multiple_complete_sequences() {
        let mut buffer = StdinBuffer::new();
        assert_eq!(
            buffer.process("\x1b[A\x1b[B\x1b[C"),
            vec!["\x1b[A", "\x1b[B", "\x1b[C"]
        );
    }

    #[test]
    fn handles_partial_sequence_with_preceding_characters() {
        let mut buffer = StdinBuffer::new();
        assert_eq!(buffer.process("abc\x1b[<35"), vec!["a", "b", "c"]);
        assert_eq!(buffer.get_buffer(), "\x1b[<35");
        assert_eq!(buffer.process(";20;5m"), vec!["\x1b[<35;20;5m"]);
    }

    #[test]
    fn handles_kitty_press_and_release_events() {
        let mut buffer = StdinBuffer::new();
        assert_eq!(buffer.process("\x1b[97u"), vec!["\x1b[97u"]);
        let mut buffer = StdinBuffer::new();
        assert_eq!(buffer.process("\x1b[97;1:3u"), vec!["\x1b[97;1:3u"]);
        let mut buffer = StdinBuffer::new();
        assert_eq!(
            buffer.process("\x1b[97u\x1b[97;1:3u"),
            vec!["\x1b[97u", "\x1b[97;1:3u"]
        );
        let mut buffer = StdinBuffer::new();
        assert_eq!(
            buffer.process("\x1b[97u\x1b[97;1:3u\x1b[98u\x1b[98;1:3u"),
            vec!["\x1b[97u", "\x1b[97;1:3u", "\x1b[98u", "\x1b[98;1:3u"]
        );
        let mut buffer = StdinBuffer::new();
        assert_eq!(buffer.process("\x1b[1;1:1A"), vec!["\x1b[1;1:1A"]);
        let mut buffer = StdinBuffer::new();
        assert_eq!(buffer.process("\x1b[3;1:3~"), vec!["\x1b[3;1:3~"]);
    }

    #[test]
    fn splits_esc_esc_csi_into_standalone_esc_and_csi() {
        let mut buffer = StdinBuffer::new();
        assert_eq!(
            buffer.process("\x1b\x1b[27;129:3u"),
            vec!["\x1b", "\x1b[27;129:3u"]
        );
        let mut buffer = StdinBuffer::new();
        assert_eq!(
            buffer.process("\x1b\x1b[27;1:3u"),
            vec!["\x1b", "\x1b[27;1:3u"]
        );
    }

    #[test]
    fn still_emits_esc_esc_as_single_sequence_when_not_followed_by_escape() {
        let mut buffer = StdinBuffer::new();
        assert_eq!(buffer.process("\x1b\x1b"), vec!["\x1b\x1b"]);
    }

    #[test]
    fn bracketed_paste_is_emitted_as_paste() {
        let mut buffer = StdinBuffer::new();
        assert_eq!(
            buffer.process("\x1b[200~hello world\x1b[201~"),
            vec!["\x1b[200~hello world\x1b[201~"]
        );
        // Paste with surrounding input.
        let mut buffer = StdinBuffer::new();
        assert_eq!(
            buffer.process("ab\x1b[200~pasted\x1b[201~cd"),
            vec!["a", "b", "\x1b[200~pasted\x1b[201~", "c", "d"]
        );
    }

    #[test]
    fn handles_old_style_mouse_sequences_and_buffers_their_three_payload_bytes() {
        let mut buffer = StdinBuffer::new();
        assert_eq!(buffer.process("\x1b[M abc"), vec!["\x1b[M ab", "c"]);

        let mut buffer = StdinBuffer::new();
        assert!(buffer.process("\x1b[M").is_empty());
        assert_eq!(buffer.get_buffer(), "\x1b[M");
        assert!(buffer.process(" a").is_empty());
        assert_eq!(buffer.get_buffer(), "\x1b[M a");
        assert_eq!(buffer.process("b"), vec!["\x1b[M ab"]);
    }

    #[test]
    fn handles_empty_and_very_long_input_without_losing_boundaries() {
        let mut buffer = StdinBuffer::new();
        assert_eq!(buffer.process(""), vec![""]);

        let long_sequence = format!("\x1b[{}H", "1;".repeat(50));
        assert_eq!(buffer.process(&long_sequence), vec![long_sequence]);
    }

    #[test]
    fn deduplicates_kitty_printable_echoes_including_fragmented_input() {
        let mut buffer = StdinBuffer::new();
        assert_eq!(buffer.process("\x1b[224uà"), vec!["\x1b[224u"]);

        let mut buffer = StdinBuffer::new();
        assert_eq!(buffer.process("\x1b[64u"), vec!["\x1b[64u"]);
        assert!(buffer.process("@").is_empty());

        let mut buffer = StdinBuffer::new();
        assert_eq!(buffer.process("\x1b[97ub"), vec!["\x1b[97u", "b"]);

        let mut buffer = StdinBuffer::new();
        assert_eq!(buffer.process("\x1b[64;3u@"), vec!["\x1b[64;3u", "@"]);
    }

    #[test]
    fn buffers_and_releases_osc_dcs_and_apc_until_string_terminators() {
        for (prefix, payload, suffix) in [
            ("\x1b]", "0;title", "\x1b\\"),
            ("\x1bP", ">|version", "\x1b\\"),
            ("\x1b_", "Gpayload", "\x1b\\"),
        ] {
            let mut buffer = StdinBuffer::new();
            assert!(buffer.process(&format!("{prefix}{payload}")).is_empty());
            assert_eq!(
                buffer.process(suffix),
                vec![format!("{prefix}{payload}{suffix}")]
            );
        }

        let mut buffer = StdinBuffer::new();
        assert_eq!(buffer.process("\x1b]0;title\x07"), vec!["\x1b]0;title\x07"]);
    }

    #[test]
    fn clear_and_destroy_drop_paste_and_escape_state_without_emitting() {
        let mut buffer = StdinBuffer::new();
        assert!(buffer.process("\x1b[200~partial").is_empty());
        buffer.clear();
        assert_eq!(buffer.get_buffer(), "");
        assert_eq!(buffer.process("a"), vec!["a"]);

        assert!(buffer.process("\x1b[<35").is_empty());
        buffer.destroy();
        assert_eq!(buffer.get_buffer(), "");
        assert!(buffer.flush().is_empty());
    }
}

#[test]
fn eof_on_empty_buffer_emits_the_eof_marker_once() {
    let mut buffer = StdinBuffer::new();
    assert_eq!(buffer.process(""), vec![""]);
    // A second EOF with nothing buffered must still signal EOF.
    assert_eq!(buffer.process(""), vec![""]);
    // Regular input keeps flowing afterwards.
    assert_eq!(buffer.process("a"), vec!["a"]);
}

#[test]
fn eof_with_a_pending_incomplete_sequence_keeps_it_flushable() {
    let mut buffer = StdinBuffer::new();
    // An EOF read while an escape sequence is still being assembled must
    // not emit the partial sequence, must not lose it, and the buffered
    // bytes stay recoverable through the normal timeout flush.
    assert_eq!(buffer.process("\x1b[1;5"), Vec::<String>::new());
    assert_eq!(buffer.process(""), Vec::<String>::new());
    assert_eq!(buffer.get_buffer(), "\x1b[1;5");
    assert_eq!(buffer.flush(), vec!["\x1b[1;5"]);
    assert_eq!(buffer.get_buffer(), "");
}
