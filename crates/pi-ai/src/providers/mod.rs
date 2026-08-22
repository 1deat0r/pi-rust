//! Provider implementations — port of `packages/ai/src/providers/`.
//!
//! Providers currently registered: `faux` (scripted test provider).
//! Remaining providers (anthropic, openai, google, bedrock, etc.) are
//! tracked in `crates/pi-ai/TODO.md`.

pub mod all;
pub mod anthropic;
pub mod faux;

pub use all::{
    anthropic_streams_for,
    builtin_images_provider,
    google_streams,
    google_provider_real,
    openai_responses_streams,
    amazon_bedrock_provider, anthropic_provider, ant_ling_provider, azure_openai_responses_provider,
    baseten_provider, builtin_models, builtin_providers, catalog_models, cerebras_provider,
    cloudflare_ai_gateway_provider, cloudflare_workers_ai_provider, deepseek_provider,
    fireworks_provider, github_copilot_provider, google_provider, google_vertex_provider,
    groq_provider, huggingface_provider, kimi_coding_provider, minimax_cn_provider, minimax_provider,
    mistral_provider, moonshotai_cn_provider, moonshotai_provider, nvidia_provider, openai_codex_provider,
    openai_provider, opencode_go_provider, opencode_provider, openrouter_provider,
    qwen_token_plan_cn_provider, qwen_token_plan_individual_provider, qwen_token_plan_provider,
    together_provider, vercel_ai_gateway_provider, xai_provider, xiaomi_provider,
    xiaomi_token_plan_ams_provider, xiaomi_token_plan_cn_provider, xiaomi_token_plan_sgp_provider,
    zai_coding_cn_provider, zai_provider,
};
pub use anthropic::{anthropic_models, AnthropicProvider};
pub use faux::{
    faux_assistant_message, faux_text, faux_thinking, faux_tool_call, FauxAssistantOptions,
    FauxModelDefinition, FauxProviderCore, FauxProviderState, FauxResponseStep, FauxTokenSize,
    RegisterFauxProviderOptions, DEFAULT_PROVIDER,
};
