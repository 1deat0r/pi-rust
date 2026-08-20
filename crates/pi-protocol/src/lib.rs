//! Pi agent harness protocol.
//!
//! Port of [`@earendil-works/pi-protocol`](https://github.com/earendil-works/pi):
//! strict definite-length CBOR codec, 4-byte length framing, validating
//! client/server message codec, and typed protocol schemas.

pub mod cbor;
pub mod codec;
pub mod error;
pub mod framing;
pub mod schemas;

pub use cbor::{
    decode_cbor, encode_cbor, CborOptions, Value, DEFAULT_MAX_CBOR_BYTE_LENGTH,
    DEFAULT_MAX_CBOR_CONTAINER_LENGTH, DEFAULT_MAX_CBOR_DEPTH, MAX_SAFE_INTEGER, MIN_SAFE_INTEGER,
    UINT32_BASE,
};
pub use codec::{
    encode_client_message, encode_server_message, parse_client_message, parse_server_message,
    ClientMessageDecoder, ServerMessageDecoder, ValidatedMessageDecoder,
};
pub use error::{CborError, FrameError, ProtocolValidationError};
pub use framing::{
    encode_frame, assert_complete_frame, FrameDecoder, FrameDecoderOptions,
    DEFAULT_MAX_FRAME_LENGTH, FRAME_HEADER_LENGTH,
};
pub use schemas::*;
