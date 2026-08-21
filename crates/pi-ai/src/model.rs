//! Model and provider types plus selection helpers — port of
//! `packages/ai/src/models.ts` and the typing from `packages/ai/src/types.ts`.

use std::collections::BTreeMap;

use crate::types::{Api, Cost, JsonValue, ModelThinkingLevel, ProviderId, ThinkingLevel, ThinkingLevelMap};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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

/// Accounts usage against a model's per-million-token rates. Applies tiered
/// pricing when the model declares tiers.
pub fn calculate_cost(model: &Model, usage: &crate::types::Usage) -> Cost {
    let max_tier = model
        .cost
        .tiers
        .as_ref()
        .map(|tiers| {
            tiers
                .iter()
                .filter(|t| usage.input > t.input_tokens_above)
                .max_by_key(|t| t.input_tokens_above)
        })
        .flatten();
    let rates = if let Some(tier) = max_tier {
        RateRates {
            input: tier.input,
            output: tier.output,
            cache_read: tier.cache_read,
            cache_write: tier.cache_write,
        }
    } else {
        RateRates {
            input: model.cost.input,
            output: model.cost.output,
            cache_read: model.cost.cache_read,
            cache_write: model.cost.cache_write,
        }
    };
    let input_cost = (usage.input as f64 * rates.input) / 1_000_000.0;
    let output_cost = (usage.output as f64 * rates.output) / 1_000_000.0;
    let cache_read_cost = (usage.cache_read as f64 * rates.cache_read) / 1_000_000.0;
    let cache_write_cost = (usage.cache_write as f64 * rates.cache_write) / 1_000_000.0;
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
    pub cache_read: f64,
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
    pub cache_read: f64,
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
pub fn clamp_thinking_level(model: &Model, level: ThinkingLevel) -> ModelThinkingLevel {
    let available = get_supported_thinking_levels(model);
    let requested = ModelThinkingLevel::from(level);
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
        assert_eq!(clamp_thinking_level(&model, ThinkingLevel::Low), ModelThinkingLevel::Low);
        assert_eq!(clamp_thinking_level(&model, ThinkingLevel::Medium), ModelThinkingLevel::Medium);
        assert_eq!(clamp_thinking_level(&model, ThinkingLevel::Xhigh), ModelThinkingLevel::High); // up fails -> down to high
        assert_eq!(clamp_thinking_level(&model, ThinkingLevel::Max), ModelThinkingLevel::High);

        // Reasoning off => only "off" is supported (upstream gate).
        model.reasoning = false;
        assert_eq!(get_supported_thinking_levels(&model), vec![ModelThinkingLevel::Off]);
        assert_eq!(clamp_thinking_level(&model, ThinkingLevel::Low), ModelThinkingLevel::Off);
    }
}
