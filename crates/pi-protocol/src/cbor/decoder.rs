//! Strict, definite-length RFC 8949 subset decoder.
//! Direct port of `packages/protocol/src/cbor/decoder.ts`.

use super::options::{resolve_options, CborOptions, ResolvedCborOptions, UINT32_BASE};
use super::value::Value;
use crate::error::CborError;

struct CborReader<'a> {
    bytes: &'a [u8],
    offset: usize,
    options: &'a ResolvedCborOptions,
}

impl<'a> CborReader<'a> {
    fn decode(&mut self) -> Result<Value, CborError> {
        let value = self.read_item(0)?;
        if self.offset != self.bytes.len() {
            return Err(CborError::new(
                "CBOR payload contains trailing data".to_string(),
            ));
        }
        Ok(value)
    }

    fn read_item(&mut self, depth: usize) -> Result<Value, CborError> {
        if depth > self.options.max_depth {
            return Err(CborError::new(format!(
                "CBOR nesting depth exceeds configured limit of {}",
                self.options.max_depth
            )));
        }
        let initial = self.read_byte()?;
        let major_type = initial >> 5;
        let additional_information = initial & 0x1f;

        match major_type {
            0 => Ok(Value::Int(self.read_argument(additional_information)?)),
            1 => {
                let value = -1 - self.read_argument(additional_information)?;
                if value < -MAX_SAFE || value > MAX_SAFE {
                    return Err(CborError::new(
                        "Decoded CBOR integer is outside the safe range".to_string(),
                    ));
                }
                Ok(Value::Int(value))
            }
            2 => {
                let length = self.read_length(additional_information, "byte string")?;
                let bytes = self.read_bytes(length)?;
                Ok(Value::Bytes(bytes.to_vec()))
            }
            3 => {
                let length = self.read_length(additional_information, "text string")?;
                let bytes = self.read_bytes(length)?;
                match std::str::from_utf8(bytes) {
                    Ok(s) => Ok(Value::Text(s.to_string())),
                    Err(_) => Err(CborError::new(
                        "CBOR text string contains invalid UTF-8".to_string(),
                    )),
                }
            }
            4 => {
                let length = self.read_length(additional_information, "array")?;
                let mut result = Vec::with_capacity(length);
                for _ in 0..length {
                    result.push(self.read_item(depth + 1)?);
                }
                Ok(Value::Array(result))
            }
            5 => {
                let length = self.read_length(additional_information, "map")?;
                let mut entries: Vec<(String, Value)> = Vec::with_capacity(length);
                let mut keys = std::collections::HashSet::new();
                for _ in 0..length {
                    let key = self.read_item(depth + 1)?;
                    let key = match key {
                        Value::Text(s) => s,
                        _ => {
                            return Err(CborError::new("CBOR map keys must be strings".to_string()))
                        }
                    };
                    if !keys.insert(key.clone()) {
                        return Err(CborError::new(
                            "CBOR map contains a duplicate key".to_string(),
                        ));
                    }
                    let value = self.read_item(depth + 1)?;
                    entries.push((key, value));
                }
                Ok(Value::Map(entries))
            }
            6 => Err(CborError::new("CBOR tags are not supported".to_string())),
            7 => self.read_simple(additional_information),
            _ => Err(CborError::new("Malformed CBOR major type".to_string())),
        }
    }

    fn read_simple(&mut self, additional_information: u8) -> Result<Value, CborError> {
        match additional_information {
            20 => Ok(Value::Bool(false)),
            21 => Ok(Value::Bool(true)),
            22 => Ok(Value::Null),
            27 => {
                let bytes = self.read_bytes(8)?;
                let value = f64::from_be_bytes(bytes.try_into().expect("8 bytes"));
                if !value.is_finite() {
                    return Err(CborError::new(
                        "Decoded CBOR number must be finite".to_string(),
                    ));
                }
                if value.fract() == 0.0 && (value < MIN_SAFE_I64_F64 || value > MAX_SAFE_I64_F64) {
                    return Err(CborError::new(
                        "Decoded CBOR integer is outside the safe range".to_string(),
                    ));
                }
                Ok(Value::Float(value))
            }
            31 => Err(CborError::new(
                "CBOR break marker is not supported".to_string(),
            )),
            _ => Err(CborError::new(
                "Unsupported CBOR simple value or floating-point width".to_string(),
            )),
        }
    }

    fn read_length(&mut self, additional_information: u8, kind: &str) -> Result<usize, CborError> {
        if additional_information == 31 {
            return Err(CborError::new(format!(
                "Indefinite-length CBOR {kind}s are not supported"
            )));
        }
        let length = self.read_argument(additional_information)?;
        let limit = if kind == "byte string" || kind == "text string" {
            self.options.max_byte_length
        } else {
            self.options.max_container_length
        };
        if (length as usize) > limit {
            return Err(CborError::new(format!(
                "CBOR {kind} length exceeds configured limit of {limit}"
            )));
        }
        Ok(length as usize)
    }

    fn read_argument(&mut self, additional_information: u8) -> Result<i64, CborError> {
        if additional_information < 24 {
            return Ok(additional_information as i64);
        }
        match additional_information {
            24 => Ok(self.read_byte()? as i64),
            25 => {
                let bytes = self.read_bytes(2)?;
                Ok((bytes[0] as i64) * 0x100 + bytes[1] as i64)
            }
            26 => {
                let bytes = self.read_bytes(4)?;
                Ok((bytes[0] as i64) * 0x1_000_000
                    + (bytes[1] as i64) * 0x1_0000
                    + (bytes[2] as i64) * 0x100
                    + bytes[3] as i64)
            }
            27 => {
                // Port of readArgument(27): two readArgument(26) calls on 8 bytes.
                let high = self.read_argument(26)?;
                let low = self.read_argument(26)?;
                if high > 0x1f_ffff {
                    return Err(CborError::new(
                        "Decoded CBOR integer or length is outside the safe range".to_string(),
                    ));
                }
                Ok(high * UINT32_BASE as i64 + low)
            }
            31 => Err(CborError::new(
                "Indefinite-length CBOR items are not supported".to_string(),
            )),
            _ => Err(CborError::new(
                "Malformed CBOR additional information".to_string(),
            )),
        }
    }

    fn read_byte(&mut self) -> Result<u8, CborError> {
        if self.offset >= self.bytes.len() {
            return Err(CborError::new("Truncated CBOR payload".to_string()));
        }
        let value = self.bytes[self.offset];
        self.offset += 1;
        Ok(value)
    }

    fn read_bytes(&mut self, length: usize) -> Result<&'a [u8], CborError> {
        if length > self.bytes.len() - self.offset {
            return Err(CborError::new("Truncated CBOR payload".to_string()));
        }
        let value = &self.bytes[self.offset..self.offset + length];
        self.offset += length;
        Ok(value)
    }
}

const MAX_SAFE: i64 = 9_007_199_254_740_991; // 2^53 - 1
const MAX_SAFE_I64_F64: f64 = 9_007_199_254_740_991.0;
const MIN_SAFE_I64_F64: f64 = -9_007_199_254_740_991.0;

/// Decodes exactly one item from the protocol's strict RFC 8949 subset.
pub fn decode_cbor(bytes: &[u8], options: &CborOptions) -> Result<Value, CborError> {
    let resolved = resolve_options(options);
    if bytes.len() > resolved.max_byte_length {
        return Err(CborError::new(format!(
            "CBOR byte length exceeds configured limit of {}",
            resolved.max_byte_length
        )));
    }
    let mut reader = CborReader {
        bytes,
        offset: 0,
        options: &resolved,
    };
    reader.decode()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from_hex(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn decodes_known_vectors() {
        let cases: Vec<(&str, Value)> = vec![
            ("f6", Value::Null),
            ("f4", Value::Bool(false)),
            ("f5", Value::Bool(true)),
            ("00", Value::Int(0)),
            ("01", Value::Int(1)),
            ("0a", Value::Int(10)),
            ("17", Value::Int(23)),
            ("1818", Value::Int(24)),
            ("1819", Value::Int(25)),
            ("1864", Value::Int(100)),
            ("1903e8", Value::Int(1000)),
            ("1a000f4240", Value::Int(1_000_000)),
            ("1b000000e8d4a51000", Value::Int(1_000_000_000_000)),
            ("1b001fffffffffffff", Value::Int(9_007_199_254_740_991)),
            ("20", Value::Int(-1)),
            ("29", Value::Int(-10)),
            ("37", Value::Int(-24)),
            ("3818", Value::Int(-25)),
            ("3863", Value::Int(-100)),
            ("3903e7", Value::Int(-1000)),
            ("3a000f423f", Value::Int(-1_000_000)),
            ("3b001ffffffffffffe", Value::Int(-9_007_199_254_740_991)),
            ("fb3ff199999999999a", Value::Float(1.1)),
            ("fb8000000000000000", Value::Float(-0.0)),
            ("4401020304", Value::Bytes(vec![1, 2, 3, 4])),
            ("60", Value::Text("".into())),
            ("6449455446", Value::Text("IETF".into())),
            ("62c3bc", Value::Text("ü".into())),
            ("63e6b0b4", Value::Text("水".into())),
            ("64f0908591", Value::Text("𐅑".into())),
            ("80", Value::Array(vec![])),
            (
                "83010203",
                Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
            ),
            (
                "8301820203820405",
                Value::Array(vec![
                    Value::Int(1),
                    Value::Array(vec![Value::Int(2), Value::Int(3)]),
                    Value::Array(vec![Value::Int(4), Value::Int(5)]),
                ]),
            ),
            (
                "a26161016162820203",
                Value::Map(vec![
                    ("a".into(), Value::Int(1)),
                    ("b".into(), Value::Array(vec![Value::Int(2), Value::Int(3)])),
                ]),
            ),
        ];
        for (hex_str, expected) in cases {
            let got = decode_cbor(&from_hex(hex_str), &CborOptions::default()).unwrap();
            assert_eq!(got, expected, "decoding {hex_str}");
        }
    }

    #[test]
    fn rejects_trailing_data() {
        let bytes = from_hex("0102");
        let err = decode_cbor(&bytes, &CborOptions::default()).unwrap_err();
        assert!(err.0.contains("trailing data"));
    }

    #[test]
    fn rejects_truncated() {
        for hex_str in ["18", "1903", "1a000f42", "43ff"] {
            let bytes = from_hex(hex_str);
            let err = decode_cbor(&bytes, &CborOptions::default()).unwrap_err();
            assert!(err.0.contains("Truncated"), "truncated {hex_str}: {err:?}");
        }
    }

    #[test]
    fn rejects_tags_and_breaks() {
        // Tag 0 wrapping integer 1
        assert_eq!(
            decode_cbor(&from_hex("c001"), &CborOptions::default())
                .unwrap_err()
                .0,
            "CBOR tags are not supported"
        );
        // Break in container position
        assert_eq!(
            decode_cbor(&from_hex("9fff"), &CborOptions::default())
                .unwrap_err()
                .0,
            "Indefinite-length CBOR arrays are not supported"
        );
        // Indefinite-length string
        assert_eq!(
            decode_cbor(&from_hex("7fff"), &CborOptions::default())
                .unwrap_err()
                .0,
            "Indefinite-length CBOR text strings are not supported"
        );
    }

    #[test]
    fn rejects_duplicate_map_keys() {
        let bytes = from_hex("a2616101616102");
        assert_eq!(
            decode_cbor(&bytes, &CborOptions::default()).unwrap_err().0,
            "CBOR map contains a duplicate key"
        );
    }

    #[test]
    fn rejects_unsafe_64bit_integer() {
        // 2^53 encoded as 64-bit positive int: 1b0020000000000000
        let bytes = from_hex("1b0020000000000000");
        assert!(decode_cbor(&bytes, &CborOptions::default()).is_err());
    }

    #[test]
    fn rejects_non_finite_float() {
        // f64 NaN: fb7ff8000000000000
        let bytes = from_hex("fb7ff8000000000000");
        assert_eq!(
            decode_cbor(&bytes, &CborOptions::default()).unwrap_err().0,
            "Decoded CBOR number must be finite"
        );
    }

    #[test]
    fn round_trips_supported_values() {
        let values = vec![
            Value::Null,
            Value::Bool(true),
            Value::Bool(false),
            Value::Int(0),
            Value::Int(1),
            Value::Int(-1),
            Value::Int(9_007_199_254_740_991),
            Value::Int(-9_007_199_254_740_991),
            Value::Float(1.1),
            Value::Float(-0.0),
            Value::Text("hello 世界".into()),
            Value::Bytes(vec![0, 255, 1]),
            Value::Array(vec![Value::Int(1), Value::Text("x".into())]),
            Value::Map(vec![
                ("k1".into(), Value::Int(1)),
                ("k2".into(), Value::Null),
                ("k3".into(), Value::Array(vec![])),
            ]),
        ];
        for v in values {
            let encoded = super::super::encoder::encode_cbor(&v, &CborOptions::default()).unwrap();
            let decoded = decode_cbor(&encoded, &CborOptions::default()).unwrap();
            assert_eq!(decoded, v);
        }
    }
}
