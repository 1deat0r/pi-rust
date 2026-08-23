//! API adaptors — port of `packages/ai/src/api/`.
//!
//! Implemented: `anthropic_messages`, `openai_completions`,
//! `openai_responses` (+ `openai_responses_shared`), `azure_openai_responses`,
//! `google_generative_ai` (+ `google_shared`), `transform_messages`,
//! `mistral_conversations` (native Mistral Chat Completions), and
//! `openai_codex_responses` (ChatGPT Codex over SSE; the WebSocket transport
//! is a documented divergence), `github_copilot_headers`, `cloudflare`,
//! `pi_messages`, `openrouter_images`, `google_vertex`, `bedrock_converse`.

pub mod anthropic_messages;
pub mod mistral_conversations;
pub mod openai_completions;

pub use anthropic_messages::{stream, AnthropicOptions, AnthropicThinkingDisplay};
pub mod azure_openai_responses;
pub mod bedrock_converse;
pub mod cloudflare;
pub mod github_copilot_headers;
pub mod google_generative_ai;
pub mod google_shared;
pub mod google_vertex;
pub mod openai_codex_responses;
pub mod openai_responses;
pub mod openai_responses_shared;
pub mod openrouter_images;
pub mod pi_messages;
pub mod transform_messages;
