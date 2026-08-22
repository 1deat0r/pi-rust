//! Strict JSONL framing — port of `packages/coding-agent/src/modes/rpc/jsonl.ts`.
//! LF-only framing: payload strings may contain any other Unicode separators;
//! clients must split records on `\n` only.

use serde::Serialize;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, AsyncReadExt};

/// Serialize a single strict JSONL record (LF-terminated).
pub fn serialize_json_line(value: &impl Serialize) -> String {
    let mut line = serde_json::to_string(value).unwrap_or_else(|_| "null".to_string());
    line.push('\n');
    line
}

/// Incremental LF-only line extractor (mirrors `attachJsonlLineReader`).
pub fn split_jsonl_lines(input: &str, on_line: &mut impl FnMut(&str)) {
    let mut buffer = String::new();
    for chunk in input.split_inclusive('\n') {
        buffer.push_str(chunk);
        while let Some(newline) = buffer.find('\n') {
            let line = &buffer[..newline];
            let line = line.strip_suffix('\r').unwrap_or(line);
            on_line(line);
            buffer = buffer[newline + 1..].to_string();
        }
    }
    if !buffer.is_empty() {
        let line = buffer.strip_suffix('\r').unwrap_or(&buffer);
        on_line(line);
    }
}

/// A line reader over any async reader that yields bytes.
pub struct JsonlLineReader<R> {
    reader: BufReader<R>,
    pending: Vec<u8>,
}

impl<R: AsyncRead + Unpin> JsonlLineReader<R> {
    pub fn new(reader: R) -> Self {
        Self { reader: BufReader::new(reader), pending: Vec::with_capacity(256) }
    }

    /// Read the next LF-terminated line; trailing \r stripped.
    pub async fn next_line(&mut self) -> std::io::Result<Option<String>> {
        loop {
            // Try to extract a complete line from the pending buffer.
            if let Some(pos) = self.pending.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=pos).collect();
                let mut line = line;
                if line.last() == Some(&b'\n') {
                    line.pop();
                }
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
            }
            let mut buf = [0u8; 4096];
            let n = self.reader.read(&mut buf).await?;
            if n == 0 {
                if self.pending.is_empty() {
                    return Ok(None);
                }
                // EOF with a trailing unterminated line.
                let line = std::mem::take(&mut self.pending);
                return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
            }
            self.pending.extend_from_slice(&buf[..n]);
        }
    }
}

/// Write a JSONL record to an async writer and flush.
pub async fn write_json_line<W: AsyncWrite + Unpin>(writer: &mut W, line: String) -> std::io::Result<()> {
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_terminates_with_lf() {
        let line = serialize_json_line(&serde_json::json!({"a": 1}));
        assert!(line.ends_with('\n'));
        assert!(line.contains("\"a\":1") || line.contains("\"a\": 1"));
    }

    #[test]
    fn split_handles_lf_and_crlf() {
        let mut lines = Vec::new();
        split_jsonl_lines("{\"a\":1}\n{\"b\":2}\r\n", &mut |l| lines.push(l.to_string()));
        assert_eq!(lines, vec!["{\"a\":1}", "{\"b\":2}"]);
    }

    #[test]
    fn split_handles_trailing_unterminated() {
        let mut lines = Vec::new();
        split_jsonl_lines("{\"a\":1}\n{\"b\":2", &mut |l| lines.push(l.to_string()));
        assert_eq!(lines, vec!["{\"a\":1}", "{\"b\":2"]);
    }

    #[tokio::test]
    async fn line_reader_framing() {
        let input: &[u8] = b"{\"a\":1}\n{\"b\":2}\r\n{\"c\":3}";
        let mut reader = JsonlLineReader::new(input);
        assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("{\"a\":1}"));
        assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("{\"b\":2}"));
        assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("{\"c\":3}"));
        assert_eq!(reader.next_line().await.unwrap(), None);
    }

    #[tokio::test]
    async fn line_reader_handles_empty_lines() {
        let input: &[u8] = b"{}\n\n\n{}\n";
        let mut reader = JsonlLineReader::new(input);
        assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("{}"));
        assert_eq!(reader.next_line().await.unwrap().as_deref(), Some(""));
        assert_eq!(reader.next_line().await.unwrap().as_deref(), Some(""));
        assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("{}"));
        assert_eq!(reader.next_line().await.unwrap(), None);
    }
}
