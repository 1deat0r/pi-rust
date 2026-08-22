//! API adaptors — port of `packages/ai/src/api/`.
//!
//! Implemented: `anthropic_messages` (Anthropic Messages API over SSE).
//! Remaining adaptors (openai-completions/responses, google, bedrock,
//! mistral, azure, codex, cloudflare, vertex, pi-messages) are tracked in
//! `crates/pi-ai/TODO.md`.

pub mod anthropic_messages;
pub mod openai_completions;
pub mod mistral_conversations;

pub use anthropic_messages::{stream, AnthropicOptions, AnthropicThinkingDisplay};
pub mod transform_messages;
pub mod google_shared;
pub mod google_generative_ai;
pub mod openai_responses_shared;
pub mod openai_responses;
pub mod azure_openai_responses;
pub mod openai_codex_responses;
