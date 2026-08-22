//! Model and provider types plus selection helpers — port of
//! `packages/ai/src/models.ts` and the typing from `packages/ai/src/types.ts`.

use std::collections::BTreeMap;

use crate::types::{Api, Cost, JsonValue, ModelThinkingLevel, ProviderId, ThinkingLevelMap};
use crate::types::{ProviderResponse};

/// `onResponse` callback type: invoked after an HTTP response is received and
/// before its body stream is consumed (mirrors upstream
/// `ProviderRequestOptions.onResponse`).
pub type OnResponseFn = std::sync::Arc<dyn Fn(&ProviderResponse, &Model) + Send + Sync>;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub name: String,
    pub api: Api,
    pub provider: ProviderId,
    pub base_url: String,
    pub reasoning: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<ThinkingLevelMap>,
    pub input: Vec<ModelInput>,
    pub cost: ModelCost,
    pub context_window: u64,
    pub max_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<JsonValue>,
    /// Whether provider auth is configured (populated by the models store).
    #[serde(default)]
    pub authenticated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelInput {
    Text,
    Image,
}

impl Model {
    pub fn new(id: impl Into<String>, name: impl Into<String>, api: impl Into<String>, provider: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            api: api.into(),
            provider: provider.into(),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text, ModelInput::Image],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 16_384,
            sampling_params: None,
            headers: None,
            compat: None,
            authenticated: false,
        }
    }
}

impl Default for Model {
    fn default() -> Self {
        Self::new("", "", "", "")
    }
}

pub fn has_api(model: &Model, api: &str) -> bool {
    model.api == api
}

struct RateRates {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
}

/// Accounts usage against a model's per-million-token rates. Mirrors upstream
/// `calculateCost` (`packages/ai/src/models.ts`):
/// - tier matching counts `input + cacheRead + cacheWrite` (not input alone);
/// - the highest matching tier applies to the whole request;
/// - cache write cost splits `cacheWrite1h` (long 1h-retention writes charged
///   at `2 * rates.input`) from short writes charged at `rates.cacheWrite`.
pub fn calculate_cost(model: &Model, usage: &crate::types::Usage) -> Cost {
    let input_tokens = usage.input + usage.cache_read + usage.cache_write;
    let mut rates = RateRates {
        input: model.cost.input,
        output: model.cost.output,
        cache_read: model.cost.cache_read,
        cache_write: model.cost.cache_write,
    };
    let mut matched_threshold = -1i64;
    if let Some(tiers) = &model.cost.tiers {
        for tier in tiers {
            if input_tokens > tier.input_tokens_above && (tier.input_tokens_above as i64) > matched_threshold {
                rates = RateRates {
                    input: tier.input,
                    output: tier.output,
                    cache_read: tier.cache_read,
                    cache_write: tier.cache_write,
                };
                matched_threshold = tier.input_tokens_above as i64;
            }
        }
    }

    // Anthropic charges 2x base input for 1h cache writes.
    let long_write = usage.cache_write_1h.unwrap_or(0);
    let short_write = usage.cache_write.saturating_sub(long_write);

    let input_cost = (usage.input as f64 * rates.input) / 1_000_000.0;
    let output_cost = (usage.output as f64 * rates.output) / 1_000_000.0;
    let cache_read_cost = (usage.cache_read as f64 * rates.cache_read) / 1_000_000.0;
    let cache_write_cost =
        (rates.cache_write * short_write as f64 + rates.input * 2.0 * long_write as f64) / 1_000_000.0;
    Cost {
        input: input_cost,
        output: output_cost,
        cache_read: cache_read_cost,
        cache_write: cache_write_cost,
        total: input_cost + output_cost + cache_read_cost + cache_write_cost,
    }
}

/// Mirrors `ModelCost` with tier support (flat rates when tiers are absent).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    #[serde(rename = "cacheRead")]
    pub cache_read: f64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<ModelCostTier>>,
}

impl Default for ModelCost {
    fn default() -> Self {
        Self { input: 0.0, output: 0.0, cache_read: 0.0, cache_write: 0.0, tiers: None }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelCostTier {
    pub input: f64,
    pub output: f64,
    #[serde(rename = "cacheRead")]
    pub cache_read: f64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: f64,
    #[serde(rename = "inputTokensAbove")]
    pub input_tokens_above: u64,
}

impl From<&ModelCost> for Cost {
    fn from(c: &ModelCost) -> Self {
        Cost {
            input: c.input,
            output: c.output,
            cache_read: c.cache_read,
            cache_write: c.cache_write,
            total: 0.0,
        }
    }
}

const THINKING_LEVEL_ORDER: [ModelThinkingLevel; 7] = [
    ModelThinkingLevel::Off,
    ModelThinkingLevel::Minimal,
    ModelThinkingLevel::Low,
    ModelThinkingLevel::Medium,
    ModelThinkingLevel::High,
    ModelThinkingLevel::Xhigh,
    ModelThinkingLevel::Max,
];

/// Port of upstream `getSupportedThinkingLevels` (models.ts @ 5cd93f6):
/// - models without `reasoning` support only `off`;
/// - a map entry of literal `null` disables that level;
/// - `xhigh`/`max` additionally REQUIRE an explicit map entry (missing means
///   unsupported); all other levels are supported unless explicitly null.
pub fn get_supported_thinking_levels(model: &Model) -> Vec<ModelThinkingLevel> {
    if !model.reasoning {
        return vec![ModelThinkingLevel::Off];
    }
    THINKING_LEVEL_ORDER
        .iter()
        .copied()
        .filter(|level| {
            let mapped = model
                .thinking_level_map
                .as_ref()
                .and_then(|m| m.get(level));
            if mapped == Some(&None) {
                return false; // map entry is literally null -> disabled
            }
            if *level == ModelThinkingLevel::Xhigh || *level == ModelThinkingLevel::Max {
                return mapped.is_some(); // explicit entry required
            }
            true
        })
        .collect()
}

/// Port of upstream `clampThinkingLevel` (models.ts @ 5cd93f6): exact match,
/// else round UP from the requested index, else round DOWN, else first
/// available (off if present). Upstream walks up before down.
/// Clamps a thinking level (including `off`) to the model's supported set,
/// mirroring upstream `clampThinkingLevel`: exact match first, then walk up
/// toward `max`, then down toward `off`, then first available.
pub fn clamp_thinking_level(model: &Model, level: ModelThinkingLevel) -> ModelThinkingLevel {
    let available = get_supported_thinking_levels(model);
    let requested = level;
    if available.contains(&requested) {
        return requested;
    }
    let requested_index = THINKING_LEVEL_ORDER
        .iter()
        .position(|l| *l == requested)
        .unwrap_or(0);
    for candidate in THINKING_LEVEL_ORDER.iter().skip(requested_index) {
        if available.contains(candidate) {
            return *candidate;
        }
    }
    for candidate in THINKING_LEVEL_ORDER.iter().take(requested_index).rev() {
        if available.contains(candidate) {
            return *candidate;
        }
    }
    available.first().copied().unwrap_or(ModelThinkingLevel::Off)
}

pub fn models_are_equal(a: &Model, b: &Model) -> bool {
    a.provider == b.provider && a.id == b.id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Usage;

    #[test]
    fn calc_cost_zero_features() {
        let model = Model::new("m", "M", "faux", "faux");
        let usage = Usage::default();
        let cost = calculate_cost(&model, &usage);
        assert_eq!(cost.total, 0.0);
    }

    #[test]
    fn calc_cost_flat_rates() {
        let mut model = Model::new("m", "M", "faux", "faux");
        model.cost = ModelCost { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75, tiers: None };
        let usage = Usage {
            input: 1_000_000,
            output: 200_000,
            cache_read: 100_000,
            cache_write: 50_000,
            total_tokens: 1_350_000,
            ..Default::default()
        };
        let cost = calculate_cost(&model, &usage);
        assert!((cost.input - 3.0).abs() < 1e-9);
        assert!((cost.output - 3.0).abs() < 1e-9);
        assert!((cost.cache_read - 0.03).abs() < 1e-9);
        assert!((cost.cache_write - 0.1875).abs() < 1e-9);
    }

    #[test]
    fn calc_cost_applies_request_wide_tiers_above_threshold() {
        // Mirrors upstream models-runtime.test.ts: input 200000, cacheRead 72000.
        // Base rates apply while inputTokens (input + cacheRead + cacheWrite) <= 272000.
        let mut model = Model::new("gpt-5.6-sol", "GPT-5.6 sol", "openai-completions", "openai");
        model.cost = ModelCost {
            input: 5.0,
            output: 30.0,
            cache_read: 0.5,
            cache_write: 6.25,
            tiers: Some(vec![ModelCostTier {
                input: 10.0,
                output: 45.0,
                cache_read: 1.0,
                cache_write: 12.5,
                input_tokens_above: 272_000,
            }]),
        };
        let short = Usage {
            input: 200_000,
            output: 100_000,
            cache_read: 72_000,
            cache_write: 0,
            total_tokens: 372_000,
            ..Default::default()
        };
        let cost = calculate_cost(&model, &short);
        assert!((cost.input - 1.0).abs() < 1e-9);
        assert!((cost.output - 3.0).abs() < 1e-9);
        assert!((cost.cache_read - 0.036).abs() < 1e-12);

        // cacheWrite=1 pushes inputTokens to 272001 -> tier rates apply.
        let long = Usage {
            cache_write: 1,
            ..short.clone()
        };
        let cost = calculate_cost(&model, &long);
        assert!((cost.input - 2.0).abs() < 1e-9);
        assert!((cost.output - 4.5).abs() < 1e-9);
        assert!((cost.cache_read - 0.072).abs() < 1e-12);
        assert!((cost.cache_write - 0.0000125).abs() < 1e-15);
    }

    #[test]
    fn calc_cost_prices_1h_cache_write_at_2x_input_rate() {
        // Mirrors upstream anthropic-cache-write-1h-cost.test.ts:
        // claude-opus-4-8: input 5, cacheWrite (5m) 6.25 per Mtok; 1h write = 2x input = 10.
        let mut model = Model::new("claude-opus-4-8", "Claude Opus 4.8", "anthropic-messages", "anthropic");
        model.cost = ModelCost {
            input: 5.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 6.25,
            tiers: None,
        };
        let usage = Usage {
            input: 0,
            output: 5,
            cache_read: 0,
            cache_write: 1_000_000,
            cache_write_1h: Some(400_000),
            total_tokens: 1_000_005,
            ..Default::default()
        };
        let cost = calculate_cost(&model, &usage);
        // 600k * 6.25/Mtok + 400k * 10/Mtok = 3.75 + 4.0 = 7.75
        assert!((cost.cache_write - 7.75).abs() < 1e-9, "got {}", cost.cache_write);
    }

    #[test]
    fn calc_cost_falls_back_to_5m_rate_without_1h_breakdown() {
        let mut model = Model::new("claude-opus-4-8", "Claude Opus 4.8", "anthropic-messages", "anthropic");
        model.cost = ModelCost {
            input: 5.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 6.25,
            tiers: None,
        };
        let usage = Usage {
            cache_write: 1_000_000,
            total_tokens: 1_000_000,
            ..Default::default()
        };
        let cost = calculate_cost(&model, &usage);
        // 1M * 6.25/Mtok = 6.25
        assert!((cost.cache_write - 6.25).abs() < 1e-9);
    }

    #[test]
    fn thinking_level_clamp() {
        // Verified against upstream getSupportedThinkingLevels/clampThinkingLevel
        // at 5cd93f6 with the same map: absent ordinary keys are supported,
        // xhigh/max require an explicit entry, and the clamp rounds UP first.
        let mut model = Model::new("m", "M", "anthropic-messages", "anthropic");
        model.reasoning = true;
        model.thinking_level_map = Some(BTreeMap::from([
            (ModelThinkingLevel::Off, Some("off".into())),
            (ModelThinkingLevel::Low, Some("low".into())),
            (ModelThinkingLevel::High, Some("high".into())),
        ]));
        // With map {off,low,high} and reasoning=true, upstream supports
        // [off, minimal, low, medium, high]; xhigh/max need explicit entries.
        assert_eq!(
            get_supported_thinking_levels(&model),
            vec![
                ModelThinkingLevel::Off,
                ModelThinkingLevel::Minimal,
                ModelThinkingLevel::Low,
                ModelThinkingLevel::Medium,
                ModelThinkingLevel::High,
            ]
        );
        assert_eq!(clamp_thinking_level(&model, ModelThinkingLevel::Low), ModelThinkingLevel::Low);
        assert_eq!(clamp_thinking_level(&model, ModelThinkingLevel::Medium), ModelThinkingLevel::Medium);
        assert_eq!(clamp_thinking_level(&model, ModelThinkingLevel::Xhigh), ModelThinkingLevel::High); // up fails -> down to high
        assert_eq!(clamp_thinking_level(&model, ModelThinkingLevel::Max), ModelThinkingLevel::High);

        // Reasoning off => only "off" is supported (upstream gate).
        model.reasoning = false;
        assert_eq!(get_supported_thinking_levels(&model), vec![ModelThinkingLevel::Off]);
        assert_eq!(clamp_thinking_level(&model, ModelThinkingLevel::Low), ModelThinkingLevel::Off);
        assert_eq!(clamp_thinking_level(&model, ModelThinkingLevel::Off), ModelThinkingLevel::Off);

        // Reasoning on, "off" explicit-nulled in the map => off unsupported;
        // clamping toward off rounds UP to the lowest supported level
        // (upstream walks up from the requested index: minimal), not down.
        model.reasoning = true;
        model.thinking_level_map = Some(BTreeMap::from([
            (ModelThinkingLevel::Off, None),
            (ModelThinkingLevel::Low, Some("low".into())),
        ]));
        assert_eq!(clamp_thinking_level(&model, ModelThinkingLevel::Off), ModelThinkingLevel::Minimal);
    }
}
