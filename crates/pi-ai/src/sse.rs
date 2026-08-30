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
        while let Some(line_end) = self
            .buffer
            .iter()
            .position(|&byte| byte == b'\n' || byte == b'\r')
        {
            let terminator = self.buffer[line_end];
            // A CR at the end of a transport chunk may be the first half of
            // CRLF. Defer it until the next chunk so a split line ending
            // cannot be mistaken for a blank-line event boundary.
            if terminator == b'\r' && line_end + 1 == self.buffer.len() {
                break;
            }
            let consume_through =
                if terminator == b'\r' && self.buffer.get(line_end + 1).copied() == Some(b'\n') {
                    line_end + 2
                } else {
                    line_end + 1
                };
            let line_bytes: Vec<u8> = self.buffer.drain(..consume_through).collect();
            let line = &line_bytes[..line_end];
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
        } else {
            // The SSE grammar treats a field without a colon as a field with
            // an empty value. This matters for `data` lines in malformed but
            // recoverable provider streams and matches the upstream decoder.
            let (field, value) = line
                .split_once(':')
                .map(|(field, value)| (field, value.strip_prefix(' ').unwrap_or(value)))
                .unwrap_or((line, ""));
            match field {
                "data" => match self.pending.last_mut() {
                    Some(e) => {
                        if !e.data.is_empty() {
                            e.data.push('\n');
                        }
                        e.data.push_str(value);
                    }
                    None => self.pending.push(SseEvent {
                        data: value.to_string(),
                        event: None,
                        id: None,
                    }),
                },
                "event" => match self.pending.last_mut() {
                    Some(e) => e.event = Some(value.to_string()),
                    None => self.pending.push(SseEvent {
                        data: String::new(),
                        event: Some(value.to_string()),
                        id: None,
                    }),
                },
                "id" => match self.pending.last_mut() {
                    Some(e) => e.id = Some(value.to_string()),
                    None => self.pending.push(SseEvent {
                        data: String::new(),
                        event: None,
                        id: Some(value.to_string()),
                    }),
                },
                _ => {}
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
            let mut remaining = std::mem::take(&mut self.buffer);
            // `drain` intentionally holds a trailing CR until another chunk
            // disambiguates CRLF. At EOF it is a complete line terminator,
            // so process the line before returning pending event fields.
            let trailing_cr = remaining.last() == Some(&b'\r');
            if trailing_cr {
                remaining.pop();
            }
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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
    fn accepts_all_sse_line_endings_and_split_crlf() {
        let mut parser = SseParser::new();
        let mut events = parser.push_bytes(b"event: message\rdata: one\r\n\r");
        events.extend(parser.push_bytes(b"\ndata: two\n\n"));

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("message"));
        assert_eq!(events[0].data, "one");
        assert_eq!(events[1].data, "two");
    }

    #[test]
    fn split_crlf_does_not_end_a_multiline_event() {
        let mut parser = SseParser::new();
        let mut events = parser.push_bytes(b"data: one\r");
        assert!(events.is_empty());
        events.extend(parser.push_bytes(b"\ndata: two\n\n"));

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "one\ntwo");
    }

    #[test]
    fn accepts_bare_data_field_as_an_empty_data_line() {
        let events = SseParser::parse_text("data\n\n");
        assert_eq!(
            events,
            vec![SseEvent {
                data: String::new(),
                event: None,
                id: None,
            }]
        );
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
