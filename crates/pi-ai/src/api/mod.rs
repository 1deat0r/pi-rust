//! API adaptors — port of `packages/ai/src/api/`.
//!
//! Implemented: `anthropic_messages`, `openai_completions`,
//! `openai_responses` (+ `openai_responses_shared`), `azure_openai_responses`,
//! `google_generative_ai` (+ `google_shared`), `transform_messages`,
//! `mistral_conversations` (native Mistral Chat Completions), and
//! `openai_codex_responses` (ChatGPT Codex over SSE; the WebSocket transport
//! is a documented divergence). Remaining adaptors (bedrock, cloudflare,
//! vertex, pi-messages, openrouter-images, github-copilot headers) are
//! tracked in `crates/pi-ai/TODO.md`.

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
