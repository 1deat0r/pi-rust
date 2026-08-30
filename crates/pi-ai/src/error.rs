//! Typed errors for the pi-ai auth/OAuth surface.
//!
//! Every variant's `Display` output is byte-identical to the message string
//! it replaces, so provider diagnostics, auth banners, and parity tests that
//! assert message text are unchanged. Callers can now match on categories
//! (cancelled vs. timeout vs. HTTP failure) instead of parsing text.

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PiAiError {
    /// The user cancelled or aborted the login flow.
    #[error("Login cancelled")]
    LoginCancelled,

    /// The OAuth callback state parameter did not match the request.
    #[error("OAuth state mismatch")]
    StateMismatch,

    /// A provider response was missing/invalid fields, was not valid JSON,
    /// or carried an untrusted URI. `message` carries the exact diagnostic.
    #[error("{message}")]
    InvalidResponse { message: String },

    /// A JWT (access/ID token) could not be parsed.
    #[error("{message}")]
    Jwt { message: String },

    /// An HTTP request failed or returned a non-success status.
    /// `message` carries the exact status/detail diagnostic.
    #[error("{message}")]
    Http { message: String },

    /// A callback/poll deadline elapsed.
    #[error("{message}")]
    Timeout { message: String },

    /// Any remaining dynamic provider diagnostic whose text is generated
    /// from response data. Prefer a structured variant when a message is
    /// a fixed string.
    #[error("{message}")]
    Other { message: String },
}

impl From<&str> for PiAiError {
    fn from(message: &str) -> Self {
        Self::Other {
            message: message.to_string(),
        }
    }
}

impl From<String> for PiAiError {
    fn from(message: String) -> Self {
        Self::Other { message }
    }
}

impl PiAiError {
    pub fn invalid_response(message: impl Into<String>) -> Self {
        Self::InvalidResponse {
            message: message.into(),
        }
    }

    pub fn jwt(message: impl Into<String>) -> Self {
        Self::Jwt {
            message: message.into(),
        }
    }

    pub fn http(message: impl Into<String>) -> Self {
        Self::Http {
            message: message.into(),
        }
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::Timeout {
            message: message.into(),
        }
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self::Other {
            message: message.into(),
        }
    }

    /// The exact user-visible diagnostic text (identical to `Display`).
    pub fn message(&self) -> &str {
        match self {
            Self::LoginCancelled => "Login cancelled",
            Self::StateMismatch => "OAuth state mismatch",
            Self::InvalidResponse { message }
            | Self::Jwt { message }
            | Self::Http { message }
            | Self::Timeout { message }
            | Self::Other { message } => message,
        }
    }
}
