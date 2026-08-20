//! Validating client/server message codec — port of
//! `packages/protocol/src/codec.ts`.

use crate::cbor::{decode_cbor, encode_cbor, CborOptions, Value};
use crate::error::{CborError, ProtocolValidationError};
use crate::framing::{
    encode_frame, assert_complete_frame, FrameDecoder, FrameDecoderOptions, DEFAULT_MAX_FRAME_LENGTH,
};
use crate::schemas::{ClientMessage, ServerMessage};

fn bounded_error_message(error: &str) -> String {
    if error.len() <= 500 {
        error.to_string()
    } else {
        format!("{}...", &error[..497])
    }
}

/// Validate and parse a client message from an arbitrary JSON value.
pub fn parse_client_message(value: &serde_json::Value) -> Result<ClientMessage, ProtocolValidationError> {
    serde_json::from_value(value.clone())
        .map_err(|_| ProtocolValidationError::new("Invalid client protocol message"))
}

/// Validate and parse a server message from an arbitrary JSON value.
pub fn parse_server_message(value: &serde_json::Value) -> Result<ServerMessage, ProtocolValidationError> {
    serde_json::from_value(value.clone())
        .map_err(|_| ProtocolValidationError::new("Invalid server protocol message"))
}

fn encode_protocol_message<T>(
    value: &T,
    parse: impl Fn(&serde_json::Value) -> Result<T, ProtocolValidationError>,
    kind: &str,
    options: &FrameDecoderOptions,
) -> Result<Vec<u8>, ProtocolValidationError>
where
    T: Clone + serde::Serialize,
{
    // Validate first (mirrors upstream: parse then encode).
    let json_value = serde_json::to_value(value)
        .map_err(|e| ProtocolValidationError::new(format!("Unable to encode {kind} protocol message: {e}")))?;
    parse(&json_value).map_err(|e| e)?;
    let max_frame_length = options.max_frame_length.unwrap_or(DEFAULT_MAX_FRAME_LENGTH);
    let cbor = encode_cbor(&Value::from(json_value), &CborOptions {
        max_byte_length: Some(max_frame_length),
        ..Default::default()
    })
    .map_err(|e| ProtocolValidationError::new(format!("Unable to encode {kind} protocol message: {}", bounded_error_message(&e.to_string()))))?;
    let frame = encode_frame(&cbor)
        .map_err(|e| ProtocolValidationError::new(format!("Unable to encode {kind} protocol message: {e}")))?;
    assert_complete_frame(&frame, options)
        .map_err(|e| ProtocolValidationError::new(format!("Unable to encode {kind} protocol message: {e}")))?;
    Ok(frame)
}

/// Validates and encodes one complete length-prefixed client message.
pub fn encode_client_message(
    message: &ClientMessage,
    options: &FrameDecoderOptions,
) -> Result<Vec<u8>, ProtocolValidationError> {
    encode_protocol_message(
        message,
        |v| parse_client_message(v),
        "client",
        options,
    )
}

/// Validates and encodes one complete length-prefixed server message.
pub fn encode_server_message(
    message: &ServerMessage,
    options: &FrameDecoderOptions,
) -> Result<Vec<u8>, ProtocolValidationError> {
    encode_protocol_message(
        message,
        |v| parse_server_message(v),
        "server",
        options,
    )
}

pub struct ValidatedMessageDecoder<T> {
    failed: bool,
    frames: FrameDecoder,
    kind: &'static str,
    max_frame_length: usize,
    parse: fn(&serde_json::Value) -> Result<T, ProtocolValidationError>,
}

impl<T> ValidatedMessageDecoder<T> {
    pub fn new(kind: &'static str, parse: fn(&serde_json::Value) -> Result<T, ProtocolValidationError>, options: &FrameDecoderOptions) -> Result<Self, ProtocolValidationError> {
        let frames = FrameDecoder::new(options)
            .map_err(|e| ProtocolValidationError::new(format!("Invalid {kind} protocol framing: {e}")))?;
        Ok(Self {
            failed: false,
            frames,
            kind,
            max_frame_length: options.max_frame_length.unwrap_or(DEFAULT_MAX_FRAME_LENGTH),
            parse,
        })
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<T>, ProtocolValidationError> {
        if self.failed {
            return Err(ProtocolValidationError::new(format!(
                "{} message decoder has failed",
                self.kind
            )));
        }
        let frames = self.frames.push(chunk).map_err(|e| {
            self.failed = true;
            ProtocolValidationError::new(format!(
                "Invalid {} protocol frame: {}",
                self.kind,
                bounded_error_message(&e.to_string())
            ))
        })?;
        let mut messages = Vec::with_capacity(frames.len());
        for frame in &frames {
            let value = decode_cbor(frame, &CborOptions {
                max_byte_length: Some(self.max_frame_length),
                ..Default::default()
            })
            .map_err(|e| {
                self.failed = true;
                ProtocolValidationError::new(format!(
                    "Invalid {} protocol frame: {}",
                    self.kind,
                    bounded_error_message(&e.to_string())
                ))
            })?;
            let json = value_to_json(value).map_err(|e| {
                self.failed = true;
                ProtocolValidationError::new(format!(
                    "Invalid {} protocol frame: {}",
                    self.kind,
                    bounded_error_message(&e.to_string())
                ))
            })?;
            let message = (self.parse)(&json).map_err(|e| {
                self.failed = true;
                e
            })?;
            messages.push(message);
        }
        Ok(messages)
    }

    pub fn end(&mut self) -> Result<(), ProtocolValidationError> {
        if self.failed {
            return Err(ProtocolValidationError::new(format!(
                "{} message decoder has failed",
                self.kind
            )));
        }
        self.frames.end().map_err(|e| {
            self.failed = true;
            ProtocolValidationError::new(format!(
                "Invalid {} protocol framing: {}",
                self.kind,
                bounded_error_message(&e.to_string())
            ))
        })
    }
}

/// Incrementally decodes and validates framed client messages.
pub struct ClientMessageDecoder(ValidatedMessageDecoder<ClientMessage>);

impl ClientMessageDecoder {
    pub fn new(options: &FrameDecoderOptions) -> Result<Self, ProtocolValidationError> {
        Ok(Self(ValidatedMessageDecoder::new("client", parse_client_message, options)?))
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<ClientMessage>, ProtocolValidationError> {
        self.0.push(chunk)
    }

    pub fn end(&mut self) -> Result<(), ProtocolValidationError> {
        self.0.end()
    }
}

/// Incrementally decodes and validates framed server messages.
pub struct ServerMessageDecoder(ValidatedMessageDecoder<ServerMessage>);

impl ServerMessageDecoder {
    pub fn new(options: &FrameDecoderOptions) -> Result<Self, ProtocolValidationError> {
        Ok(Self(ValidatedMessageDecoder::new("server", parse_server_message, options)?))
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<ServerMessage>, ProtocolValidationError> {
        self.0.push(chunk)
    }

    pub fn end(&mut self) -> Result<(), ProtocolValidationError> {
        self.0.end()
    }
}

/// Converts a decoded protocol value back into a JSON value (infallible for
/// decoder output because the decoder never produces `Undefined`).
fn value_to_json(value: Value) -> Result<serde_json::Value, CborError> {
    Ok(match value {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(b),
        Value::Int(i) => serde_json::Value::from(i),
        Value::Float(f) => serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .ok_or_else(|| CborError::new("Decoded CBOR number must be finite".to_string()))?,
        Value::Text(s) => serde_json::Value::String(s),
        Value::Bytes(_) => {
            return Err(CborError::new("CBOR byte string is not valid JSON".to_string()));
        }
        Value::Array(items) => serde_json::Value::Array(
            items
                .into_iter()
                .map(value_to_json)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Value::Map(entries) => {
            let mut map = serde_json::Map::new();
            for (k, v) in entries {
                map.insert(k, value_to_json(v)?);
            }
            serde_json::Value::Object(map)
        }
        Value::Undefined => return Err(CborError::new("undefined is not valid JSON".to_string())),
    })
}
