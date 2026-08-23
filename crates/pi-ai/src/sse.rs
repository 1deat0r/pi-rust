//! Server-Sent Events streaming protocol parser.
//!
//! Handles `data:`, `event:`, `id:`, `retry:`, and comments. An empty line
//! dispatches a complete event. `[DONE]`-style non-JSON data is passed
//! through as-is (callers interpret it), mirroring how upstream providers
//! consume raw SSE frames.
//!
//! The parser buffers raw bytes and only decodes UTF-8 at line boundaries, so
//! a multibyte character split across chunks never corrupts the stream (the
//! per-chunk `from_utf8_lossy` approach replaced here did exactly that).

#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub data: String,
    pub event: Option<String>,
    pub id: Option<String>,
}

/// Incremental SSE parser over arbitrary byte chunks.
#[derive(Debug, Default)]
pub struct SseParser {
    /// Raw bytes accumulated since the last consumed newline.
    buffer: Vec<u8>,
    /// Fields of the event being assembled.
    pending: Vec<SseEvent>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes, returns complete events parsed from the chunk.
    pub fn push_bytes(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        self.drain()
    }

    fn drain(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        loop {
            let Some(newline) = self.buffer.iter().position(|&b| b == b'\n') else {
                break; // no complete line yet; a multibyte char may still be split
            };
            let line_bytes: Vec<u8> = self.buffer.drain(..=newline).collect();
            let line = line_bytes.strip_suffix(b"\n").unwrap_or(&line_bytes);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            // The line is byte-complete; decode it. Incomplete multibyte
            // sequences cannot occur here because a line ends at a '\n' byte
            // and a char spanning the boundary would leave its trailing bytes
            // still in `buffer`. Only truly invalid UTF-8 bytes fall back to
            // lossy decoding.
            let line = String::from_utf8(line.to_vec())
                .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
            self.process_line(&line, &mut events);
        }
        events
    }

    fn process_line(&mut self, line: &str, out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            if let Some(event) = self.pending_event() {
                out.push(event);
            }
        } else if line.starts_with(':') {
            // SSE comments are ignored (but terminate nothing)
        } else if let Some(field) = line.strip_prefix("data:") {
            let value = field.strip_prefix(' ').unwrap_or(field).to_string();
            match self.pending.last_mut() {
                Some(e) => {
                    if !e.data.is_empty() {
                        e.data.push('\n');
                    }
                    e.data.push_str(&value);
                }
                None => self.pending.push(SseEvent {
                    data: value,
                    event: None,
                    id: None,
                }),
            }
        } else if let Some(field) = line.strip_prefix("event:") {
            let value = field.strip_prefix(' ').unwrap_or(field).to_string();
            match self.pending.last_mut() {
                Some(e) => e.event = Some(value),
                None => self.pending.push(SseEvent {
                    data: String::new(),
                    event: Some(value),
                    id: None,
                }),
            }
        } else if let Some(field) = line.strip_prefix("id:") {
            let value = field.strip_prefix(' ').unwrap_or(field).to_string();
            match self.pending.last_mut() {
                Some(e) => e.id = Some(value),
                None => self.pending.push(SseEvent {
                    data: String::new(),
                    event: None,
                    id: Some(value),
                }),
            }
        }
        // "retry:" lines and unknown fields are ignored.
    }

    fn pending_event(&mut self) -> Option<SseEvent> {
        if self.pending.is_empty() {
            return None;
        }
        // An event with no data still fires (can show the connection is alive).
        Some(self.pending.remove(0))
    }

    /// Flush any unterminated buffered data as a final event (used on EOF).
    /// Events already accumulated are returned in exactly the order they were
    /// assembled (no rotation).
    pub fn finish(mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            let remaining = std::mem::take(&mut self.buffer);
            let line = String::from_utf8(remaining.clone())
                .unwrap_or_else(|_| String::from_utf8_lossy(&remaining).into_owned());
            if !line.trim().is_empty() {
                // EOF inside a line: treat what we have as a final line so a
                // trailing `data:` fragment is still delivered.
                self.process_line(&line, &mut events);
            }
        }
        events.extend(std::mem::take(&mut self.pending));
        events
    }

    /// Convenience: parse a complete SSE text payload.
    pub fn parse_text(text: &str) -> Vec<SseEvent> {
        let mut parser = SseParser::new();
        let mut events = parser.push_bytes(text.as_bytes());
        events.extend(parser.finish());
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_events() {
        let events = SseParser::parse_text("data: hello\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn parses_multi_line_data() {
        let events = SseParser::parse_text("data: line1\ndata: line2\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line1\nline2");
    }

    #[test]
    fn parses_event_and_id() {
        let events = SseParser::parse_text("id: 42\nevent: message\ndata: {\"a\":1}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id.as_deref(), Some("42"));
        assert_eq!(events[0].event.as_deref(), Some("message"));
        assert_eq!(events[0].data, "{\"a\":1}");
    }

    #[test]
    fn incremental_across_chunk_boundaries() {
        let mut parser = SseParser::new();
        let text = "data: hello\ndata: world\n\n";
        let mut events = Vec::new();
        for byte in text.bytes() {
            events.extend(parser.push_bytes(&[byte]));
        }
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello\nworld");
    }

    #[test]
    fn data_after_colon_without_space() {
        let events = SseParser::parse_text("data:{\"b\":2}\n\n");
        assert_eq!(events[0].data, "{\"b\":2}");
    }

    #[test]
    fn done_sentinel_passthrough() {
        let events = SseParser::parse_text("data: [DONE]\n\n");
        assert_eq!(events[0].data, "[DONE]");
    }

    #[test]
    fn ignores_comments_and_retry() {
        let events = SseParser::parse_text(": ping\nretry: 1000\ndata: x\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "x");
    }

    #[test]
    fn handles_utf8_split_across_chunks() {
        let mut parser = SseParser::new();
        let text = "data: héllo\n\ndata: 世界\n\n";
        let bytes = text.as_bytes();
        let mut events = Vec::new();
        for chunk in bytes.chunks(3) {
            events.extend(parser.push_bytes(chunk));
        }
        events.extend(parser.finish());
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "héllo");
        assert_eq!(events[1].data, "世界");
    }

    #[test]
    fn finish_keeps_event_order_when_buffered_data_remains() {
        // Regression (reviewer condition 3): the old finish() pushed leftover
        // buffer as a NEW raw event and then rotated the first pending event
        // to the back. With an assembled-but-undispatched event plus a
        // buffered data line at EOF, that produced two events in the wrong
        // order with the wrong payload. The corrected finish must fold the
        // buffered data line into the pending event and return one event.
        let mut parser = SseParser::new();
        parser.push_bytes(b"data: line1\n"); // pending: [{data:"line1"}]
        parser.push_bytes(b"data: partial"); // EOF leaves "data: partial" buffered
        let events = parser.finish();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line1\npartial");
    }

    #[test]
    fn finish_delivers_unterminated_data_line() {
        let mut parser = SseParser::new();
        parser.push_bytes(b"data: tail");
        let events = parser.finish();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "tail");
    }
}
