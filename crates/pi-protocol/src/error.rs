//! Error types mirroring `packages/protocol/src/cbor/options.ts` (CborError),
//! `packages/protocol/src/framing.ts` (FrameError), and
//! `packages/protocol/src/codec.ts` (ProtocolValidationError).

/// CBOR encode/decode errors. Message text mirrors the upstream JS so
/// failure modes are recognizable during the port.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CborError(pub String);

impl CborError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Frame-level errors from `framing.ts`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct FrameError(pub String);

impl FrameError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Raised when a protocol message fails validation or cannot be encoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ProtocolValidationError(pub String);

impl ProtocolValidationError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}
