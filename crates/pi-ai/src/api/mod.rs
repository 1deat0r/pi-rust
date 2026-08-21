//! API adaptors — port of `packages/ai/src/api/`.
//!
//! Implemented: `anthropic_messages` (Anthropic Messages API over SSE).
//! Remaining adaptors (openai-completions/responses, google, bedrock,
//! mistral, azure, codex, cloudflare, vertex, pi-messages) are tracked in
//! `crates/pi-ai/TODO.md`.

pub mod anthropic_messages;

pub use anthropic_messages::{stream, AnthropicOptions, AnthropicThinkingDisplay};
