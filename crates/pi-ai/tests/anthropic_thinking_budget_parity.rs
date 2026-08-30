#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Deterministic Anthropic thinking-budget edge parity.

use pi_ai::api::anthropic_messages::{build_params, AnthropicOptions};
use pi_ai::model::Model;
use pi_ai::types::{Context, ModelThinkingLevel};

fn reasoning_model() -> Model {
    let mut model = Model::new(
        "MiniMax-M2.7",
        "MiniMax M2.7",
        "anthropic-messages",
        "minimax",
    );
    model.reasoning = true;
    model.thinking_level_map = Some(std::collections::BTreeMap::from([(
        ModelThinkingLevel::Off,
        Some("off".to_string()),
    )]));
    model
}

#[test]
fn explicit_zero_thinking_budget_uses_upstream_default() {
    let params = build_params(
        &reasoning_model(),
        &Context::default(),
        &AnthropicOptions {
            thinking_enabled: Some(true),
            thinking_budget_tokens: Some(0),
            ..Default::default()
        },
    )
    .expect("thinking parameters should serialize");

    assert_eq!(params["thinking"]["budget_tokens"], 1024);
}
