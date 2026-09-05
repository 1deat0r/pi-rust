//! Provider implementations — port of `packages/ai/src/providers/`.
//!
//! All bundled providers are registered through `all`; `faux` remains an
//! explicit deterministic test provider and is never used for production
//! authentication or live inference.

pub mod all;
pub mod anthropic;
pub mod faux;
pub mod radius;

pub use all::{
    amazon_bedrock_provider, ant_ling_provider, anthropic_provider, anthropic_streams_for,
    azure_openai_responses_provider, baseten_provider, builtin_images_provider, builtin_models,
    builtin_providers, catalog_models, cerebras_provider, cloudflare_ai_gateway_provider,
    cloudflare_workers_ai_provider, deepseek_provider, fireworks_provider, github_copilot_provider,
    google_provider, google_provider_real, google_streams, google_vertex_provider, groq_provider,
    huggingface_provider, kimi_coding_provider, minimax_cn_provider, minimax_provider,
    mistral_provider, moonshotai_cn_provider, moonshotai_provider, nvidia_provider,
    openai_codex_provider, openai_codex_provider_with_oauth, openai_provider,
    openai_responses_streams, opencode_go_provider, opencode_provider, openrouter_provider,
    provider_streams_for_api, qwen_token_plan_cn_provider, qwen_token_plan_individual_provider,
    qwen_token_plan_provider, radius_provider, radius_provider_with_options, together_provider,
    vercel_ai_gateway_provider, xai_provider, xiaomi_provider, xiaomi_token_plan_ams_provider,
    xiaomi_token_plan_cn_provider, xiaomi_token_plan_sgp_provider, zai_coding_cn_provider,
    zai_provider,
};
pub use anthropic::{anthropic_models, AnthropicProvider};
pub use faux::{
    faux_assistant_message, faux_text, faux_thinking, faux_tool_call, FauxAssistantOptions,
    FauxDeferredOptions, FauxModelDefinition, FauxProviderCore, FauxProviderState,
    FauxResponseStep, FauxTokenSize, RegisterFauxProviderOptions, DEFAULT_PROVIDER,
};
pub use radius::{
    get_radius_credential_config, get_radius_models, get_radius_models_from_config,
    load_radius_gateway_config, normalize_radius_gateway_url, RadiusGatewayConfig,
    RadiusGatewayModel, RadiusOAuth, RadiusProviderOptions, DEFAULT_RADIUS_GATEWAY,
};
