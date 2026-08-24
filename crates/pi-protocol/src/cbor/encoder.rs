//! Strict, definite-length RFC 8949 subset encoder.
//! Direct port of `packages/protocol/src/cbor/encoder.ts`.

use super::options::{resolve_options, CborOptions, ResolvedCborOptions, MAX_UINT32, UINT32_BASE};
use super::value::{Value, MAX_SAFE_INTEGER};
use crate::error::CborError;

struct CborWriter {
    buffer: Vec<u8>,
    max_byte_length: usize,
}

impl CborWriter {
    fn new(max_byte_length: usize) -> Self {
        let capacity = max_byte_length.min(256);
        Self {
            buffer: Vec::with_capacity(capacity),
            max_byte_length,
        }
    }

    fn ensure_capacity(&mut self, additional: usize) -> Result<(), CborError> {
        let required = self.buffer.len() + additional;
        if required > self.max_byte_length {
            return Err(CborError::new(format!(
                "CBOR byte length exceeds configured limit of {}",
                self.max_byte_length
            )));
        }
        self.buffer
            .reserve(required.saturating_sub(self.buffer.len()));
        Ok(())
    }

    fn write_byte(&mut self, value: u8) -> Result<(), CborError> {
        self.ensure_capacity(1)?;
        self.buffer.push(value);
        Ok(())
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), CborError> {
        self.ensure_capacity(bytes.len())?;
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    fn write_uint16(&mut self, value: u16) -> Result<(), CborError> {
        self.ensure_capacity(2)?;
        self.buffer.push((value >> 8) as u8);
        self.buffer.push(value as u8);
        Ok(())
    }

    fn write_uint32(&mut self, value: u32) -> Result<(), CborError> {
        self.ensure_capacity(4)?;
        self.buffer.push((value >> 24) as u8);
        self.buffer.push((value >> 16) as u8);
        self.buffer.push((value >> 8) as u8);
        self.buffer.push(value as u8);
        Ok(())
    }

    fn write_uint64(&mut self, value: u64) -> Result<(), CborError> {
        // Port of writeUint64: high = floor(value / 2^32) as u32, low = value - high*2^32
        let high = (value / UINT32_BASE) as u32;
        let low = (value - (high as u64) * UINT32_BASE) as u32;
        self.write_uint32(high)?;
        self.write_uint32(low)
    }

    fn write_float64(&mut self, value: f64) -> Result<(), CborError> {
        self.ensure_capacity(9)?;
        self.buffer.push(0xfb);
        self.buffer.extend_from_slice(&value.to_be_bytes());
        Ok(())
    }
}

fn write_argument(writer: &mut CborWriter, major_type: u8, value: u64) -> Result<(), CborError> {
    let prefix = major_type << 5;
    if value < 24 {
        writer.write_byte(prefix | value as u8)
    } else if value <= 0xff {
        writer.write_byte(prefix | 24)?;
        writer.write_byte(value as u8)
    } else if value <= 0xffff {
        writer.write_byte(prefix | 25)?;
        writer.write_uint16(value as u16)
    } else if value <= MAX_UINT32 {
        writer.write_byte(prefix | 26)?;
        writer.write_uint32(value as u32)
    } else {
        writer.write_byte(prefix | 27)?;
        writer.write_uint64(value)
    }
}

fn encode_text(
    writer: &mut CborWriter,
    value: &str,
    options: &ResolvedCborOptions,
) -> Result<(), CborError> {
    let bytes = value.as_bytes();
    if bytes.len() > options.max_byte_length {
        return Err(CborError::new(format!(
            "CBOR text string length exceeds configured limit of {}",
            options.max_byte_length
        )));
    }
    write_argument(writer, 3, bytes.len() as u64)?;
    writer.write_bytes(bytes)
}

fn encode_value(
    writer: &mut CborWriter,
    value: &Value,
    options: &ResolvedCborOptions,
    depth: usize,
) -> Result<(), CborError> {
    if depth > options.max_depth {
        return Err(CborError::new(format!(
            "CBOR nesting depth exceeds configured limit of {}",
            options.max_depth
        )));
    }

    match value {
        Value::Null => writer.write_byte(0xf6)?,
        Value::Bool(true) => writer.write_byte(0xf5)?,
        Value::Bool(false) => writer.write_byte(0xf4)?,
        Value::Int(v) => {
            if *v < -MAX_SAFE_INTEGER || *v > MAX_SAFE_INTEGER {
                return Err(CborError::new(
                    "CBOR integers must be safe JavaScript integers".to_string(),
                ));
            }
            if *v >= 0 {
                write_argument(writer, 0, *v as u64)?;
            } else {
                let encoded = (-1i64 - *v) as u64; // -1 - value (value negative)
                write_argument(writer, 1, encoded)?;
            }
        }
        Value::Float(v) => {
            if !v.is_finite() {
                return Err(CborError::new("CBOR numbers must be finite".to_string()));
            }
            let is_integer = v.fract() == 0.0;
            let is_negative_zero = *v == 0.0 && v.is_sign_negative();
            if is_integer && !is_negative_zero {
                if *v < MIN_SAFE_INTEGER_F64 || *v > MAX_SAFE_INTEGER_F64 {
                    return Err(CborError::new(
                        "CBOR integers must be safe JavaScript integers".to_string(),
                    ));
                }
                if *v >= 0.0 {
                    write_argument(writer, 0, *v as u64)?;
                } else {
                    let encoded = (-1i64 - (*v as i64)) as u64;
                    write_argument(writer, 1, encoded)?;
                }
            } else {
                writer.write_float64(*v)?;
            }
        }
        Value::Text(s) => encode_text(writer, s, options)?,
        Value::Bytes(bytes) => {
            if bytes.len() > options.max_byte_length {
                return Err(CborError::new(format!(
                    "CBOR byte string length exceeds configured limit of {}",
                    options.max_byte_length
                )));
            }
            write_argument(writer, 2, bytes.len() as u64)?;
            writer.write_bytes(bytes)?;
        }
        Value::Array(items) => {
            if items.len() > options.max_container_length {
                return Err(CborError::new(format!(
                    "CBOR array length exceeds configured limit of {}",
                    options.max_container_length
                )));
            }
            write_argument(writer, 4, items.len() as u64)?;
            for item in items {
                if matches!(item, Value::Undefined) {
                    return Err(CborError::new(
                        "CBOR arrays must not contain holes or undefined values".to_string(),
                    ));
                }
                encode_value(writer, item, options, depth + 1)?;
            }
        }
        Value::Map(entries) => {
            let mut kept: Vec<&(String, Value)> = Vec::with_capacity(entries.len());
            for entry in entries {
                if !matches!(entry.1, Value::Undefined) {
                    kept.push(entry);
                }
            }
            if kept.len() > options.max_container_length {
                return Err(CborError::new(format!(
                    "CBOR map length exceeds configured limit of {}",
                    options.max_container_length
                )));
            }
            write_argument(writer, 5, kept.len() as u64)?;
            for (key, entry_value) in kept {
                encode_text(writer, key, options)?;
                encode_value(writer, entry_value, options, depth + 1)?;
            }
        }
        Value::Undefined => {
            return Err(CborError::new(
                "Unsupported CBOR value type: undefined".to_string(),
            ));
        }
    }
    Ok(())
}

const MAX_SAFE_INTEGER_F64: f64 = 9_007_199_254_740_991.0;
const MIN_SAFE_INTEGER_F64: f64 = -9_007_199_254_740_991.0;

/// Encodes the protocol's strict, definite-length RFC 8949 subset.
pub fn encode_cbor(value: &Value, options: &CborOptions) -> Result<Vec<u8>, CborError> {
    let resolved = resolve_options(options);
    let mut writer = CborWriter::new(resolved.max_byte_length);
    encode_value(&mut writer, value, &resolved, 0)?;
    Ok(writer.buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn encodes_known_vectors() {
        let cases: Vec<(Value, &str)> = vec![
            (Value::Null, "f6"),
            (Value::Bool(false), "f4"),
            (Value::Bool(true), "f5"),
            (Value::Int(0), "00"),
            (Value::Int(1), "01"),
            (Value::Int(10), "0a"),
            (Value::Int(23), "17"),
            (Value::Int(24), "1818"),
            (Value::Int(25), "1819"),
            (Value::Int(100), "1864"),
            (Value::Int(1000), "1903e8"),
            (Value::Int(1_000_000), "1a000f4240"),
            (Value::Int(1_000_000_000_000), "1b000000e8d4a51000"),
            (Value::Int(9_007_199_254_740_991), "1b001fffffffffffff"),
            (Value::Int(-1), "20"),
            (Value::Int(-10), "29"),
            (Value::Int(-24), "37"),
            (Value::Int(-25), "3818"),
            (Value::Int(-100), "3863"),
            (Value::Int(-1000), "3903e7"),
            (Value::Int(-1_000_000), "3a000f423f"),
            (Value::Int(-9_007_199_254_740_991), "3b001ffffffffffffe"),
            (Value::Float(1.1), "fb3ff199999999999a"),
            (Value::Float(-0.0), "fb8000000000000000"),
            (Value::Bytes(vec![1, 2, 3, 4]), "4401020304"),
            (Value::Text("".into()), "60"),
            (Value::Text("IETF".into()), "6449455446"),
            (Value::Text("ü".into()), "62c3bc"),
            (Value::Text("水".into()), "63e6b0b4"),
            (Value::Text("𐅑".into()), "64f0908591"),
            (Value::Array(vec![]), "80"),
            (
                Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
                "83010203",
            ),
            (
                Value::Array(vec![
                    Value::Int(1),
                    Value::Array(vec![Value::Int(2), Value::Int(3)]),
                    Value::Array(vec![Value::Int(4), Value::Int(5)]),
                ]),
                "8301820203820405",
            ),
            (
                Value::Map(vec![
                    ("a".into(), Value::Int(1)),
                    ("b".into(), Value::Array(vec![Value::Int(2), Value::Int(3)])),
                ]),
                "a26161016162820203",
            ),
        ];
        for (value, expected) in cases {
            assert_eq!(
                hex(&encode_cbor(&value, &CborOptions::default()).unwrap()),
                expected
            );
        }
    }

    #[test]
    fn rejects_unsafe_integer() {
        let err =
            encode_cbor(&Value::Int(MAX_SAFE_INTEGER + 1), &CborOptions::default()).unwrap_err();
        assert!(err.0.contains("safe JavaScript integers"));
    }

    #[test]
    fn rejects_nan_and_infinity() {
        for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(encode_cbor(&Value::Float(v), &CborOptions::default()).is_err());
        }
    }

    #[test]
    fn skips_undefined_map_values() {
        let value = Value::Map(vec![
            ("a".into(), Value::Int(1)),
            ("b".into(), Value::Undefined),
            ("c".into(), Value::Text("x".into())),
        ]);
        assert_eq!(
            hex(&encode_cbor(&value, &CborOptions::default()).unwrap()),
            "a261610161636178"
        );
    }

    #[test]
    fn rejects_undefined_array_element() {
        let value = Value::Array(vec![Value::Int(1), Value::Undefined]);
        let err = encode_cbor(&value, &CborOptions::default()).unwrap_err();
        assert!(err.0.contains("holes or undefined"));
    }

    #[test]
    fn float_zero_encodes_as_integer_but_negative_zero_as_float() {
        assert_eq!(
            hex(&encode_cbor(&Value::Float(0.0), &CborOptions::default()).unwrap()),
            "00"
        );
        assert_eq!(
            hex(&encode_cbor(&Value::Float(-0.0), &CborOptions::default()).unwrap()),
            "fb8000000000000000"
        );
    }

    #[test]
    fn enforces_limits() {
        let opts = CborOptions {
            max_byte_length: Some(4),
            ..Default::default()
        };
        assert!(encode_cbor(&Value::Text("hello".into()), &opts).is_err());
        let opts = CborOptions {
            max_depth: Some(1),
            ..Default::default()
        };
        let deep = Value::Array(vec![Value::Array(vec![Value::Int(1)])]);
        assert!(encode_cbor(&deep, &opts).is_err());
    }
}
