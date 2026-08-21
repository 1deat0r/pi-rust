//! Provider implementations — port of `packages/ai/src/providers/`.
//!
//! Providers currently registered: `faux` (scripted test provider).
//! Remaining providers (anthropic, openai, google, bedrock, etc.) are
//! tracked in `crates/pi-ai/TODO.md`.

pub mod anthropic;
pub mod faux;

pub use anthropic::{anthropic_models, AnthropicProvider};
pub use faux::{
    faux_assistant_message, faux_text, faux_thinking, faux_tool_call, FauxAssistantOptions,
    FauxModelDefinition, FauxProviderCore, FauxProviderState, FauxResponseStep, FauxTokenSize,
    RegisterFauxProviderOptions, DEFAULT_PROVIDER,
};
