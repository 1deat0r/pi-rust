//! Tolerant incremental JSON parsing — port of the streaming-JSON contract in
//! upstream `packages/ai/src/utils/json-parse.ts` (pinned 5cd93f6), whose
//! observable behavior bottoms out in the npm `partial-json@0.1.7` parser
//! (vendored alongside the oracle at `scripts/partial-json-0.1.7/`).
//!
//! Two layers:
//! - [`parse_streaming_json`]: the exact upstream fallback chain —
//!   `JSON.parse` → `JSON.parse(repairJson)` → `partialParse` →
//!   `partialParse(repairJson)` → `{}`.
//! - [`parse_partial_json`]: the tolerant middle layer; it mirrors the npm
//!   package's observable behavior on the golden table (unterminated strings
//!   tolerated, partial booleans/null completed, incomplete numbers that the
//!   npm package rejects — `-`, `12.` — are errors, malformed keywords are
//!   errors) but returns `Result` instead of throwing.
//!
//! Divergence (documented): npm's `Inf`/`-Inf`/`NaN` partials produce JS
//! `Infinity`/`NaN`, which have no JSON representation; `serde_json::Value`
//! cannot hold them, so the Rust port yields `Null` for those fragments. This
//! cannot occur in tool-call arguments.

use crate::types::JsonValue;

/// Error type for a partial parse that the npm parser would reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartialJsonError;

pub type PartialJsonResult<T> = Result<T, PartialJsonError>;

const VALID_JSON_ESCAPES: &[char] = &['"', '\\', '/', 'b', 'f', 'n', 'r', 't', 'u'];

fn is_control_char(c: char) -> bool {
    let cp = c as u32;
    cp <= 0x1f
}

fn escape_control_char(c: char) -> String {
    match c {
        '\u{0008}' => "\\b".to_string(),
        '\u{000c}' => "\\f".to_string(),
        '\n' => "\\n".to_string(),
        '\r' => "\\r".to_string(),
        '\t' => "\\t".to_string(),
        _ => format!("\\u{:04x}", c as u32),
    }
}

/// Port of upstream `repairJson` (json-parse.ts @ 5cd93f6): escapes raw
/// control characters inside strings, doubles backslashes before invalid
/// escapes, and doubles a dangling trailing backslash.
pub fn repair_json(json: &str) -> String {
    let mut repaired = String::with_capacity(json.len());
    let mut in_string = false;
    let chars: Vec<char> = json.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if !in_string {
            repaired.push(ch);
            if ch == '"' {
                in_string = true;
            }
            i += 1;
            continue;
        }
        if ch == '"' {
            repaired.push(ch);
            in_string = false;
            i += 1;
            continue;
        }
        if ch == '\\' {
            let next = chars.get(i + 1).copied();
            match next {
                None => {
                    repaired.push_str("\\\\");
                    i += 1;
                    continue;
                }
                Some('u') => {
                    let digits: String = chars.iter().skip(i + 2).take(4).collect();
                    if digits.len() == 4 && digits.chars().all(|c| c.is_ascii_hexdigit()) {
                        repaired.push_str("\\u");
                        repaired.push_str(&digits);
                        i += 6;
                        continue;
                    }
                }
                Some(n) if VALID_JSON_ESCAPES.contains(&n) => {
                    repaired.push('\\');
                    repaired.push(n);
                    i += 2;
                    continue;
                }
                Some(_) => {}
            }
            repaired.push_str("\\\\");
            i += 1;
            continue;
        }
        if is_control_char(ch) {
            repaired.push_str(&escape_control_char(ch));
        } else {
            repaired.push(ch);
        }
        i += 1;
    }
    repaired
}

/// The upstream `parseStreamingJson` chain: empty/whitespace → `{}`;
/// `JSON.parse` → `JSON.parse(repairJson)` → partial parse → partial parse of
/// the repaired string → `{}`.
pub fn parse_streaming_json(input: &str) -> JsonValue {
    let empty_object = || JsonValue::Object(serde_json::Map::new());
    if input.trim().is_empty() {
        return empty_object();
    }
    if let Ok(v) = serde_json::from_str::<JsonValue>(input) {
        return v;
    }
    let repaired = repair_json(input);
    if repaired != input {
        if let Ok(v) = serde_json::from_str::<JsonValue>(&repaired) {
            return v;
        }
    }
    if let Ok(v) = parse_partial_json(input) {
        return v;
    }
    if repaired != input {
        if let Ok(v) = parse_partial_json(&repaired) {
            return v;
        }
    }
    empty_object()
}

/// Tolerant partial parser mirroring npm `partial-json@0.1.7` observable
/// behavior (see module docs). Returns `Err` for fragments the npm package
/// rejects.
pub fn parse_partial_json(input: &str) -> PartialJsonResult<JsonValue> {
    let mut parser = Parser { input, pos: 0, aborted: false };
    if parser.eof() {
        return Err(PartialJsonError); // npm: "is empty"
    }
    parser.skip_ws();
    if parser.eof() {
        return Err(PartialJsonError);
    }
    let value = parser.parse_value()?;
    parser.skip_ws();
    // Trailing junk after the top-level value rejects (npm throws).
    if !parser.eof() {
        return Err(PartialJsonError);
    }
    Ok(value)
}

struct Parser<'a> {
    input: &'a str,
    pos: usize,
    /// npm aborts a tolerant parse on a raw control char inside a string:
    /// what was parsed so far is returned as-is and the rest is ignored.
    aborted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueAbort {
    /// The value token was malformed (raw control char, junk, bad number).
    Malformed,
    /// Input ended mid-token (partial but tolerated by npm).
    Partial,
}

impl<'a> Parser<'a> {
    fn eof(&self) -> bool {
        self.pos >= self.input.len()
    }
    fn peek(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }
    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }
    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.bump();
            } else {
                break;
            }
        }
    }

    fn parse_value(&mut self) -> PartialJsonResult<JsonValue> {
        self.skip_ws();
        match self.peek() {
            None => Err(PartialJsonError),
            Some('{') => self.parse_object(),
            Some('[') => self.parse_array(),
            Some('"') => Ok(JsonValue::String(self.parse_string(ValueAbort::Partial)?)),
            Some('t') | Some('f') | Some('n') => self.parse_keyword(),
            Some(c) if c == '-' || c.is_ascii_digit() => {
                let rest = &self.input[self.pos..];
                if rest == "-Inf" || rest == "-Infinity" {
                    self.pos = self.input.len();
                    Ok(JsonValue::Null)
                } else {
                    self.parse_number()
                }
            }
            Some('I') | Some('N') => {
                let rest = &self.input[self.pos..];
                // Fragment is a prefix of Infinity or NaN at EOF.
                if ("Infinity".starts_with(rest) || "NaN".starts_with(rest)) && !rest.is_empty() {
                    self.pos = self.input.len();
                    Ok(JsonValue::Null)
                } else {
                    Err(PartialJsonError)
                }
            }
            Some(_) => Err(PartialJsonError),
        }
    }

    fn parse_keyword(&mut self) -> PartialJsonResult<JsonValue> {
        let rest = &self.input[self.pos..];
        let (word, value) = if rest.starts_with("true") {
            ("true", JsonValue::Bool(true))
        } else if rest.starts_with("false") {
            ("false", JsonValue::Bool(false))
        } else if rest.starts_with("null") {
            ("null", JsonValue::Null)
        } else if "true".starts_with(rest) || "false".starts_with(rest) || "null".starts_with(rest) {
            // Truncated keyword at EOF: npm completes it (tru -> true).
            self.pos = self.input.len();
            return Ok(if rest.starts_with('f') {
                JsonValue::Bool(false)
            } else if rest.starts_with('n') {
                JsonValue::Null
            } else {
                JsonValue::Bool(true)
            });
        } else {
            // Keyword prefix followed by junk (e.g. `trux`, `tru"e`).
            return Err(PartialJsonError);
        };
        self.pos += word.len();
        Ok(value)
    }

    fn parse_object(&mut self) -> PartialJsonResult<JsonValue> {
        self.bump(); // '{'
        self.skip_ws();
        let mut map = serde_json::Map::new();
        if self.peek() == Some('}') {
            self.bump();
            return Ok(JsonValue::Object(map));
        }
        loop {
            self.skip_ws();
            let key = match self.peek() {
                Some('"') => match self.parse_string(ValueAbort::Malformed) {
                    Ok(k) => k,
                    // unterminated key at EOF (e.g. `{"a`) -> npm returns {}
                    Err(_) => return Ok(JsonValue::Object(map)),
                },
                // `{"a` variants already returned; a non-string key is junk.
                Some(_) => return Err(PartialJsonError),
                None => return Ok(JsonValue::Object(map)),
            };
            self.skip_ws();
            match self.peek() {
                Some(':') => {
                    self.bump();
                }
                // `{"a` without colon: npm drops the key entirely.
                _ => return Ok(JsonValue::Object(map)),
            }
            self.skip_ws();
            if self.eof() {
                // `{"a":` with nothing after: npm drops the key entirely.
                return Ok(JsonValue::Object(map));
            }
            match self.parse_value() {
                Ok(value) => {
                    map.insert(key, value);
                }
                // Raw control char / malformed value (e.g. `{"a": "b\x01c`)
                // makes npm DROP the key and stop: return what was parsed.
                Err(PartialJsonError) => {
                    if self.aborted {
                        self.pos = self.input.len(); // ignore the remainder
                    }
                    return Ok(JsonValue::Object(map));
                }
            }
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.bump();
                    self.skip_ws();
                    if self.eof() {
                        // trailing comma at EOF: npm keeps the object.
                        return Ok(JsonValue::Object(map));
                    }
                }
                Some('}') => {
                    self.bump();
                    return Ok(JsonValue::Object(map));
                }
                None => return Ok(JsonValue::Object(map)),
                Some(_) => return Err(PartialJsonError),
            }
        }
    }

    fn parse_array(&mut self) -> PartialJsonResult<JsonValue> {
        self.bump(); // '['
        self.skip_ws();
        let mut items = Vec::new();
        if self.peek() == Some(']') {
            self.bump();
            return Ok(JsonValue::Array(items));
        }
        loop {
            self.skip_ws();
            match self.peek() {
                None => return Ok(JsonValue::Array(items)),
                Some(']') => {
                    self.bump();
                    return Ok(JsonValue::Array(items));
                }
                _ => {}
            }
            match self.parse_value() {
                Ok(v) => items.push(v),
                Err(PartialJsonError) => {
                    if self.aborted {
                        self.pos = self.input.len();
                    }
                    return Ok(JsonValue::Array(items));
                }
            }
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.bump();
                    self.skip_ws();
                    if self.eof() {
                        return Ok(JsonValue::Array(items));
                    }
                }
                Some(']') => {
                    self.bump();
                    return Ok(JsonValue::Array(items));
                }
                None => return Ok(JsonValue::Array(items)),
                Some(_) => return Err(PartialJsonError),
            }
        }
    }

    /// Parse a JSON string. A raw control character is malformed (npm drops
    /// the value); an unterminated string is tolerated (partial); a dangling
    /// trailing backslash is dropped (npm: `{"a": "b\` -> `{"a":"b"}`).
    fn parse_string(&mut self, _abort: ValueAbort) -> PartialJsonResult<String> {
        self.bump(); // opening quote
        let mut out = String::new();
        let mut escaped = false;
        let mut pending_high_surrogate: Option<u16> = None;
        while let Some(c) = self.bump() {
            if escaped {
                match c {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    '/' => out.push('/'),
                    'b' => out.push('\u{0008}'),
                    'f' => out.push('\u{000c}'),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    'u' => {
                        let mut hex = String::new();
                        for _ in 0..4 {
                            match self.peek() {
                                Some(d) if d.is_ascii_hexdigit() => {
                                    hex.push(d);
                                    self.bump();
                                }
                                _ => break,
                            }
                        }
                        if hex.len() == 4 {
                            if let Ok(code) = u16::from_str_radix(&hex, 16) {
                                if let Some(high) = pending_high_surrogate.take() {
                                    // Complete a low surrogate pair.
                                    if (0xdc00..=0xdfff).contains(&code) {
                                        let combined =
                                            0x10000 + ((high as u32 - 0xd800) << 10) + (code as u32 - 0xdc00);
                                        if let Some(ch) = char::from_u32(combined) {
                                            out.push(ch);
                                        }
                                    } else {
                                        out.push('\u{fffd}');
                                        continue;
                                    }
                                } else if (0xd800..=0xdbff).contains(&code) {
                                    pending_high_surrogate = Some(code);
                                } else if let Some(ch) = char::from_u32(code as u32) {
                                    out.push(ch);
                                }
                            }
                        }
                    }
                    // npm rejects UNKNOWN escape sequences inside a string
                    // (e.g. `\x`), which is how `{"a": "b\xc"}` falls through
                    // to repairJson. Hmm: the raw input there has a literal
                    // backslash before x; when the REPAIRED string is parsed
                    // (`\\x`) the doubling makes it a literal backslash, so a
                    // lone backslash escape here is malformed.
                    _ => return Err(PartialJsonError),
                }
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                break;
            } else if is_control_char(c) {
                // Raw control char inside a string: npm aborts the parse and
                // returns what it had (dropping the current key/value).
                self.aborted = true;
                return Err(PartialJsonError);
            } else {
                out.push(c);
            }
        }
        if pending_high_surrogate.take().is_some() {
            out.push('\u{fffd}');
        }
        // Unterminated or dangling-backslash EOF is tolerated; a dangling
        // backslash is discarded (matches npm's `{"a": "b\` -> {"a":"b"}).
        Ok(out)
    }

    fn parse_number(&mut self) -> PartialJsonResult<JsonValue> {
        let start = self.pos;
        if self.peek() == Some('-') {
            self.bump();
        }
        while matches!(
            self.peek(),
            Some(c) if c.is_ascii_digit() || c == '.' || c == 'e' || c == 'E' || c == '+' || c == '-'
        ) {
            self.bump();
        }
        let text = &self.input[start..self.pos];
        // npm rejects unterminated fractional numbers (`12.`, `1.`); Rust's
        // f64 parser is lenient about the trailing dot, so reject it first.
        if text.ends_with('.') {
            return Err(PartialJsonError);
        }
        // Complete numbers.
        if let Ok(f) = text.parse::<f64>() {
            if let Some(i) = text.parse::<i64>().ok() {
                if i as f64 == f {
                    return Ok(JsonValue::from(i));
                }
            }
            if f.is_finite() {
                return Ok(JsonValue::Number(
                    serde_json::Number::from_f64(f).unwrap_or_else(|| serde_json::Number::from(0)),
                ));
            }
            return Ok(JsonValue::Null); // Inf/-Inf-like: see module docs
        }
        // Incomplete numbers npm rejects (`-`, `12.`, `1.2.3`): error.
        if text == "-" || text.ends_with('.') || text.matches('.').count() > 1 {
            return Err(PartialJsonError);
        }
        // Incomplete exponent (e.g. `1e`, `12e+`): npm returns the mantissa.
        let trimmed = text.trim_end_matches(['e', 'E', '+', '-']);
        if trimmed.is_empty() || trimmed == "-" {
            return Err(PartialJsonError);
        }
        if let Ok(i) = trimmed.parse::<i64>() {
            return Ok(JsonValue::from(i));
        }
        if let Ok(f) = trimmed.parse::<f64>() {
            if f.is_finite() {
                return Ok(JsonValue::Number(
                    serde_json::Number::from_f64(f).unwrap_or_else(|| serde_json::Number::from(0)),
                ));
            }
        }
        Err(PartialJsonError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn val(s: &str) -> JsonValue {
        serde_json::from_str(s).unwrap()
    }

    // Golden table from `node scripts/oracle_partial_json.mjs` (vendored
    // partial-json@0.1.7, upstream chain at 5cd93f6). Every row must match.
    fn assert_oracle(input: &str, expected: JsonValue) {
        let got = parse_streaming_json(input);
        assert_eq!(got, expected, "oracle row diverges for {:?}", input);
    }

    #[test]
    fn oracle_core_cases() {
        assert_oracle("{\"a", val("{}"));
        assert_oracle("{\"a\":", val("{}"));
        assert_oracle("{\"a\": 1", val(r#"{"a":1}"#));
        assert_oracle("{\"a\": \"hel", val(r#"{"a":"hel"}"#));
        assert_oracle("\"hel", val(r#""hel""#));
        assert_oracle("-", val("{}"));
        assert_oracle("12.", val("{}"));
        assert_oracle("12", val("12"));
        assert_oracle("tru", val("true"));
        assert_oracle("{\"a\": tru", val(r#"{"a":true}"#));
        assert_oracle("[1, 2,", val("[1,2]"));
        assert_oracle("", val("{}"));
        assert_oracle("nul", val("null"));
        assert_oracle("{\"a\": 1,", val(r#"{"a":1}"#));
        assert_oracle("{\"a\": {\"b\": 2}", val(r#"{"a":{"b":2}}"#));
        assert_oracle("{\"a\": \"he\\\"", val(r#"{"a":"he\""}"#));
        assert_oracle("tru\"e", val("{}"));
        assert_oracle("Inf", val("null"));
        assert_oracle("-Inf", val("null"));
        assert_oracle("{\"a\": [1, 2", val(r#"{"a":[1,2]}"#));
    }

    #[test]
    fn oracle_repair_path_cases() {
        // repairJson branches: raw control char, invalid escapes, trailing
        // backslash, partial exponent (reviewer condition 2).
        assert_oracle("{\"a\": \"b\u{0001}c\"}", val(r#"{"a":"b\u0001c"}"#));
        assert_oracle("[\"x\u{0001}y\"]", val(r#"["x\u0001y"]"#));
        // Upstream's repair doubles the invalid escape, so the parsed string
        // contains a literal backslash-x / backslash-q (not valid JSON text
        // for `val()`, hence the direct JsonValue construction).
        assert_oracle(
            "{\"a\": \"b\\xc\"}",
            serde_json::json!({"a": "b\\xc"}),
        );
        assert_oracle(
            "{\"a\": \"b\\qc\"}",
            serde_json::json!({"a": "b\\qc"}),
        );
        assert_oracle("{\"a\": \"b\\", val(r#"{"a":"b"}"#));
        assert_oracle("\\", val("{}"));
        assert_oracle("{\"a\": \"b\u{0001}c", val("{}"));
        assert_oracle("1e", val("1"));
    }

    #[test]
    fn repair_json_port() {
        assert_eq!(repair_json("{\"a\": \"b\u{0001}c\"}"), "{\"a\": \"b\\u0001c\"}");
        assert_eq!(repair_json("{\"a\": \"b\\xc\"}"), "{\"a\": \"b\\\\xc\"}");
        assert_eq!(repair_json("{\"a\": \"b\\"), "{\"a\": \"b\\\\");
        // Outside strings backslashes pass through verbatim.
        assert_eq!(repair_json("\\"), "\\");
    }

    #[test]
    fn tolerant_partial_values() {
        assert_eq!(parse_partial_json("true").unwrap(), val("true"));
        assert_eq!(parse_partial_json("tru").unwrap(), val("true"));
        assert_eq!(parse_partial_json("{\"a\": \"hel").unwrap(), val(r#"{"a":"hel"}"#));
        assert_eq!(parse_partial_json("[1, 2,").unwrap(), val("[1,2]"));
        // npm rejects these (partial-json throws -> chain returns {}).
        assert!(parse_partial_json("-").is_err());
        assert!(parse_partial_json("12.").is_err());
        assert!(parse_partial_json("tru\"e").is_err());
    }
}

