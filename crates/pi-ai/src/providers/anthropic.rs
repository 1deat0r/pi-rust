//! Anthropic provider factory — port of `packages/ai/src/providers/anthropic.ts`
//! (catalog subset; full catalog arrives with the model-catalog port).

use std::collections::BTreeMap;

use crate::api::anthropic_messages::{default_base_url, stream, AnthropicOptions};
use crate::model::ModelInput;
use crate::model::{Model, ModelCost, ModelCostTier};
use crate::types::ModelThinkingLevel;
use crate::AssistantMessageEventStream;

/// Minimal bundled model catalog for first-party Anthropic models. Fields
/// mirror upstream `anthropic.models.ts`; costs are $/M tokens.
pub fn anthropic_models() -> Vec<Model> {
    let mut models = Vec::new();
    models.push(claude_model(
        "claude-opus-4-8",
        "Claude Opus 4.8",
        ModelCost {
            input: 5.0,
            output: 25.0,
            cache_read: 0.3,
            cache_write: 6.25,
            tiers: None,
        },
        200_000,
        64_000,
        true,
    ));
    models.push(claude_model(
        "claude-sonnet-4-6",
        "Claude Sonnet 4.6",
        ModelCost {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
            tiers: None,
        },
        200_000,
        64_000,
        true,
    ));
    models.push(claude_model(
        "claude-haiku-4-5",
        "Claude Haiku 4.5",
        ModelCost {
            input: 0.8,
            output: 4.0,
            cache_read: 0.08,
            cache_write: 1.0,
            tiers: None,
        },
        200_000,
        16_384,
        false,
    ));
    // Tiered model example (inputs above 200k use tier rates).
    let mut opus_5 = claude_model(
        "claude-opus-5",
        "Claude Opus 5",
        ModelCost {
            input: 8.0,
            output: 40.0,
            cache_read: 0.5,
            cache_write: 10.0,
            tiers: None,
        },
        1_000_000,
        128_000,
        true,
    );
    opus_5.cost.tiers = Some(vec![ModelCostTier {
        input: 6.0,
        output: 30.0,
        cache_read: 0.4,
        cache_write: 8.0,
        input_tokens_above: 200_000,
    }]);
    models.push(opus_5);
    models
}

fn claude_model(
    id: &str,
    name: &str,
    cost: ModelCost,
    context_window: u64,
    max_tokens: u64,
    reasoning: bool,
) -> Model {
    let mut model = Model::new(id, name, "anthropic-messages", "anthropic");
    model.base_url = default_base_url();
    model.reasoning = reasoning;
    model.input = vec![ModelInput::Text, ModelInput::Image];
    model.cost = cost;
    model.context_window = context_window;
    model.max_tokens = max_tokens;
    if reasoning {
        model.thinking_level_map = Some(BTreeMap::from([
            (ModelThinkingLevel::Off, Some("off".into())),
            (ModelThinkingLevel::Minimal, Some("low".into())),
            (ModelThinkingLevel::Low, Some("low".into())),
            (ModelThinkingLevel::Medium, Some("medium".into())),
            (ModelThinkingLevel::High, Some("high".into())),
            (ModelThinkingLevel::Xhigh, Some("xhigh".into())),
            (ModelThinkingLevel::Max, Some("max".into())),
        ]));
    }
    model
}

/// Resolves an Anthropic API key from the options, `ANTHROPIC_API_KEY`, or a
/// Bearer-style env var (PORT key support noted in TODO).
pub fn resolve_api_key(api_key: Option<&str>) -> Option<String> {
    api_key
        .map(|s| s.to_string())
        .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
}

/// A provider handle exposing catalog + stream functions (mirrors the
/// upstream `createProvider` surface for consumers).
#[derive(Clone)]
pub struct AnthropicProvider {
    pub models: Vec<Model>,
    client: reqwest::Client,
    base_url: String,
}

impl AnthropicProvider {
    pub fn new() -> Self {
        Self {
            models: anthropic_models(),
            client: reqwest::Client::new(),
            base_url: default_base_url(),
        }
    }

    pub fn get_model(&self, id: &str) -> Option<&Model> {
        self.models.iter().find(|m| m.id == id)
    }

    pub fn stream_with_options(
        &self,
        model: &Model,
        context: &crate::types::Context,
        api_key: Option<&str>,
        options: &AnthropicOptions,
    ) -> AssistantMessageEventStream {
        stream(
            model,
            context,
            self.client.clone(),
            &self.base_url,
            api_key,
            options,
        )
    }
}

impl Default for AnthropicProvider {
    fn default() -> Self {
        Self::new()
    }
}
