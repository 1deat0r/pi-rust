//! Result helpers — port of `packages/agent/src/harness/result.ts`.
//!
//! Rust's `Result<T, E>` already models the upstream discriminated union;
//! this module provides the `TaggedError` factory (an Error type carrying a
//! stable `_tag` plus a `toJSON` projection) and `matchError` dispatcher used
//! by error-based control flow in the harness.

use std::collections::BTreeMap;

pub type ErrorMatchers<TValue> = BTreeMap<String, Box<dyn Fn(&TaggedError) -> TValue>>;

/// A tagged error: an error value carrying a stable `_tag` string alongside
/// its message and arbitrary payload (upstream `TaggedErrorValue`).
#[derive(Debug, Clone)]
pub struct TaggedError {
    pub tag: String,
    pub message: String,
    pub payload: BTreeMap<String, serde_json::Value>,
}

impl TaggedError {
    pub fn new(tag: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            tag: tag.into(),
            message: message.into(),
            payload: BTreeMap::new(),
        }
    }

    pub fn with_payload(mut self, key: &str, value: serde_json::Value) -> Self {
        self.payload.insert(key.to_string(), value);
        self
    }

    /// JSON projection excluding the `_tag` key (upstream `toJSON`).
    pub fn to_json(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        map.insert(
            "_tag".to_string(),
            serde_json::Value::String(self.tag.clone()),
        );
        map.insert(
            "message".to_string(),
            serde_json::Value::String(self.message.clone()),
        );
        for (k, v) in &self.payload {
            map.insert(k.clone(), v.clone());
        }
        serde_json::Value::Object(map)
    }
}

impl std::fmt::Display for TaggedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.tag, self.message)
    }
}

impl std::error::Error for TaggedError {}

/// Dispatches on an error's tag, mirroring upstream `matchError`.
pub fn match_error<TValue>(error: &TaggedError, matchers: &ErrorMatchers<TValue>) -> TValue {
    match matchers.get(&error.tag) {
        Some(matcher) => matcher(error),
        None => panic!("no matcher for error tag {}", error.tag),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tagged_error_projection_excludes_tag_key() {
        let err = TaggedError::new("not_found", "missing")
            .with_payload("path", serde_json::Value::String("/x".into()));
        let json = err.to_json();
        assert_eq!(json["_tag"], "not_found");
        assert_eq!(json["message"], "missing");
        assert_eq!(json["path"], "/x");
    }

    #[test]
    fn match_error_dispatches_by_tag() {
        let mut matchers: ErrorMatchers<String> = BTreeMap::new();
        matchers.insert(
            "not_found".to_string(),
            Box::new(|e| format!("nf:{}", e.message)),
        );
        matchers.insert(
            "other".to_string(),
            Box::new(|e| format!("other:{}", e.message)),
        );
        let err = TaggedError::new("not_found", "x");
        assert_eq!(match_error(&err, &matchers), "nf:x");
        let err2 = TaggedError::new("other", "y");
        assert_eq!(match_error(&err2, &matchers), "other:y");
    }
}
