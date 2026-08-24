//! Server errors — port of `packages/server/src/errors.ts`.

use pi_protocol::ProtocolErrorCode;

pub const INTERNAL_SERVER_ERROR_MESSAGE: &str = "Internal server error";
pub const NOT_IMPLEMENTED_MESSAGE: &str = "Operation is not implemented";

#[derive(Debug, Clone)]
pub struct PiServerError {
    pub code: ProtocolErrorCode,
    pub message: String,
    pub details: Option<serde_json::Value>,
}

impl PiServerError {
    pub fn new(code: ProtocolErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }
    pub fn with_details(
        code: ProtocolErrorCode,
        message: impl Into<String>,
        details: serde_json::Value,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            details: Some(details),
        }
    }
    pub fn into_protocol(&self) -> pi_protocol::ProtocolError {
        pi_protocol::ProtocolError {
            code: self.code.clone(),
            message: self.message.clone(),
            details: self.details.clone(),
        }
    }
}

impl std::fmt::Display for PiServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {}",
            serde_json::to_string(&self.code).unwrap_or_default(),
            self.message
        )
    }
}

impl std::error::Error for PiServerError {}

/// Hard internal error (causes are reported to onError, never leaked).
#[derive(Debug)]
pub struct InternalServerError {
    pub cause: String,
}

/// A not-implemented PiServerError (rendered with the aggregate message).
pub fn not_implemented_error() -> PiServerError {
    PiServerError::new(ProtocolErrorCode::NotImplemented, NOT_IMPLEMENTED_MESSAGE)
}

pub fn internal_server_error(cause: impl Into<String>) -> InternalServerError {
    InternalServerError {
        cause: cause.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_roundtrip_preserves_fields() {
        let err = PiServerError::with_details(
            ProtocolErrorCode::Busy,
            "busy",
            serde_json::json!({"sessionId": "s1"}),
        );
        let p = err.into_protocol();
        assert_eq!(p.code, ProtocolErrorCode::Busy);
        assert_eq!(p.details.as_ref().unwrap()["sessionId"], "s1");
    }
}
