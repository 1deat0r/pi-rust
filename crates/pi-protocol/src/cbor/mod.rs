//! Strict CBOR codec (RFC 8949 definite-length subset) — port of
//! `packages/protocol/src/cbor/`.

mod decoder;
mod encoder;
mod options;
mod value;

pub use decoder::decode_cbor;
pub use encoder::encode_cbor;
pub use options::{
    CborOptions, DEFAULT_MAX_CBOR_BYTE_LENGTH, DEFAULT_MAX_CBOR_CONTAINER_LENGTH,
    DEFAULT_MAX_CBOR_DEPTH,
};
pub use value::{Value, MAX_SAFE_INTEGER, MIN_SAFE_INTEGER, UINT32_BASE};
