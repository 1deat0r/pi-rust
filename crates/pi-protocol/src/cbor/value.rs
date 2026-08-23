//! Protocol CBOR value model.
//!
//! Mirrors the domain of plain JavaScript values the upstream codec
//! operates on (see `packages/protocol/src/cbor/encoder.ts`). Plain-object
//! maps preserve insertion order — the encoder writes map entries in
//! insertion order and the decoder preserves the order it read.

use std::fmt;

/// Maximum safe integer magnitude (`Number.MAX_SAFE_INTEGER`).
pub const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991; // 2^53 - 1
/// Minimum safe integer magnitude (exclusive lower bound of the safe range).
pub const MIN_SAFE_INTEGER: i64 = -9_007_199_254_740_991; // -(2^53 - 1)
/// `UINT32_BASE` from `options.ts`.
pub const UINT32_BASE: u64 = 0x1_0000_0000;

/// A value representable in the protocol's CBOR subset.
///
/// `Undefined` models JavaScript `undefined`: map values that are undefined
/// are skipped during encoding; array elements that are undefined are an
/// error. It is never produced by the decoder.
#[derive(Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    Array(Vec<Value>),
    Map(Vec<(String, Value)>),
    Undefined,
}

impl Value {
    pub fn is_safe_int(v: i64) -> bool {
        v >= MIN_SAFE_INTEGER && v <= MAX_SAFE_INTEGER
    }

    pub fn is_safe_int_f64(v: f64) -> bool {
        v >= MIN_SAFE_INTEGER as f64 && v <= MAX_SAFE_INTEGER as f64
    }
}

impl From<serde_json::Value> for Value {
    fn from(v: serde_json::Value) -> Self {
        match v {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::Int(i)
                } else if let Some(u) = n.as_u64() {
                    if u <= MAX_SAFE_INTEGER as u64 {
                        Value::Int(u as i64)
                    } else {
                        Value::Float(u as f64)
                    }
                } else {
                    Value::Float(n.as_f64().unwrap_or(f64::NAN))
                }
            }
            serde_json::Value::String(s) => Value::Text(s),
            serde_json::Value::Array(items) => {
                Value::Array(items.into_iter().map(Value::from).collect())
            }
            serde_json::Value::Object(map) => {
                Value::Map(map.into_iter().map(|(k, v)| (k, Value::from(v))).collect())
            }
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{x:?}"),
            Value::Text(s) => write!(f, "{s:?}"),
            Value::Bytes(b) => write!(f, "Bytes({} bytes)", b.len()),
            Value::Array(items) => f.debug_list().entries(items).finish(),
            Value::Map(entries) => f
                .debug_map()
                .entries(entries.iter().map(|(k, v)| (k, v)))
                .finish(),
            Value::Undefined => write!(f, "undefined"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_int_constants_match_js() {
        assert_eq!(MAX_SAFE_INTEGER, 9_007_199_254_740_991);
        assert_eq!(MIN_SAFE_INTEGER, -9_007_199_254_740_991);
        assert!(Value::is_safe_int(MAX_SAFE_INTEGER));
        assert!(!Value::is_safe_int(MAX_SAFE_INTEGER + 1));
        assert!(Value::is_safe_int(MIN_SAFE_INTEGER));
        assert!(!Value::is_safe_int(MIN_SAFE_INTEGER - 1));
    }
}
