//! Image-model catalog and generation facade — port of
//! `packages/ai/src/image-models.ts`, `images.ts`, and
//! `providers/images/register-builtins.ts`.
//!
//! The image-model catalog is vendored from the upstream
//! `image-models.generated.ts` (`crates/pi-ai/data/openrouter-images.json`).
//! `generate_images` dispatches on `model.api` to the registered API
//! implementation, matching upstream's `imagesApiProviderRegistry`.

use serde_json::Value;

use crate::model::{ModelCost, ModelInput};
use crate::types::{AssistantImages, ContentBlock, ImagesContext, ImagesStopReason};

/// Image-generation model (upstream `ImagesModel`). A slim `Model` analog:
/// carries id/name/api/provider/base url, input/output capabilities, and cost.
#[derive(Debug, Clone, PartialEq)]
pub struct ImagesModel {
    pub id: String,
    pub name: String,
    pub api: crate::types::ImagesApi,
    pub provider: crate::types::ImagesProviderId,
    pub base_url: String,
    pub input: Vec<ModelInput>,
    pub output: Vec<String>,
    pub cost: ModelCost,
    /// Static request headers (e.g. OpenRouter HTTP-Referer).
    pub headers: Option<std::collections::BTreeMap<String, String>>,
}

impl ImagesModel {
    /// A chat-side `Model` view for response callbacks (the unified
    /// `on_response` signature receives a `&Model`).
    pub fn as_chat_model(&self) -> crate::model::Model {
        let mut model = crate::model::Model::new(
            self.id.clone(),
            self.name.clone(),
            self.api.clone(),
            self.provider.clone(),
        );
        model.base_url = self.base_url.clone();
        model.cost = self.cost.clone();
        model.input = self.input.clone();
        model
    }
}

/// Options for image generation (subset of upstream `ImagesOptions`).
#[derive(Clone, Default)]
pub struct ImagesOptions {
    pub api_key: Option<String>,
    pub headers: Option<crate::types::ProviderHeaders>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    pub on_response: Option<crate::model::OnResponseFn>,
    pub aborted: bool,
}

/// API implementation: `(model, context, options) -> AssistantImages`.
pub type ImagesFunction = std::sync::Arc<
    dyn Fn(&ImagesModel, &ImagesContext, &ImagesOptions) -> AssistantImages + Send + Sync,
>;

static REGISTRY: std::sync::OnceLock<std::sync::RwLock<std::collections::BTreeMap<String, ImagesFunction>>> =
    std::sync::OnceLock::new();

fn registry() -> &'static std::sync::RwLock<std::collections::BTreeMap<String, ImagesFunction>> {
    REGISTRY.get_or_init(|| std::sync::RwLock::new(std::collections::BTreeMap::new()))
}

/// Register an image API implementation (upstream `registerImagesApiProvider`).
pub fn register_images_api_provider(api: &str, f: ImagesFunction) {
    registry().write().unwrap().insert(api.to_string(), f);
}

fn parse_model_input(value: &Value) -> Vec<ModelInput> {
    value
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| match v.as_str() {
                    Some("text") => Some(ModelInput::Text),
                    Some("image") => Some(ModelInput::Image),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_cost(value: &Value) -> ModelCost {
    let get = |k: &str| value.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
    ModelCost {
        input: get("input"),
        output: get("output"),
        cache_read: get("cacheRead"),
        cache_write: get("cacheWrite"),
        tiers: None,
    }
}

/// Parse the vendored image catalog (`crates/pi-ai/data/openrouter-images.json`).
pub fn catalog_images(provider_id: &str) -> Vec<ImagesModel> {
    let path = match provider_id {
        "openrouter" => concat!(env!("CARGO_MANIFEST_DIR"), "/data/openrouter-images.json"),
        _ => return Vec::new(),
    };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let Some(obj) = value.as_object() else { return Vec::new() };
    obj.iter()
        .map(|(id, v)| {
            let headers = v
                .get("headers")
                .and_then(|h| h.as_object())
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                });
            ImagesModel {
                id: v.get("id").and_then(|x| x.as_str()).unwrap_or(id).to_string(),
                name: v.get("name").and_then(|x| x.as_str()).unwrap_or(id).to_string(),
                api: v.get("api").and_then(|x| x.as_str()).unwrap_or("openrouter-images").to_string(),
                provider: v.get("provider").and_then(|x| x.as_str()).unwrap_or(provider_id).to_string(),
                base_url: v.get("baseUrl").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                input: v.get("input").map(parse_model_input).unwrap_or_default(),
                output: v
                    .get("output")
                    .and_then(|o| o.as_array())
                    .map(|arr| arr.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                    .unwrap_or_default(),
                cost: v.get("cost").map(parse_cost).unwrap_or_default(),
                headers,
            }
        })
        .collect()
}

/// Generate images through the owning API implementation (upstream
/// `generateImages`). Never returns a Result: failures are encoded on the
/// returned `AssistantImages` (`stopReason: "error"`).
pub fn generate_images(
    model: &ImagesModel,
    context: &ImagesContext,
    options: &ImagesOptions,
) -> AssistantImages {
    let f = registry().read().unwrap().get(&model.api).cloned();
    let Some(f) = f else {
        let mut output = AssistantImages {
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            output: Vec::new(),
            response_id: None,
            usage: None,
            stop_reason: ImagesStopReason::Error,
            error_message: Some(format!("No API provider registered for api: {}", model.api)),
            timestamp: crate::types::now_ms(),
        };
        let _ = &mut output;
        return output;
    };
    f(model, context, options)
}

/// Build the error-encoded `AssistantImages` used by lazy-load failure paths.
pub fn image_error(model: &ImagesModel, message: String) -> AssistantImages {
    AssistantImages {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        output: Vec::new(),
        response_id: None,
        usage: None,
        stop_reason: ImagesStopReason::Error,
        error_message: Some(message),
        timestamp: crate::types::now_ms(),
    }
}

/// Output content helper: text and image `ContentBlock`s for results.
pub fn text_content(text: &str) -> ContentBlock {
    ContentBlock::text(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_vendors_openrouter_image_models() {
        let models = catalog_images("openrouter");
        assert!(models.len() >= 36, "expected 36+ image models, got {}", models.len());
        let nano = models.iter().find(|m| m.id == "google/gemini-2.5-flash-image").unwrap();
        assert_eq!(nano.api, "openrouter-images");
        assert_eq!(nano.provider, "openrouter");
        assert_eq!(nano.base_url, "https://openrouter.ai/api/v1");
        assert!(nano.output.contains(&"image".to_string()));
    }

    #[tokio::test]
    async fn unknown_api_returns_error_images() {
        let model = ImagesModel {
            id: "x".to_string(),
            name: "X".to_string(),
            api: "no-such-images-api".to_string(),
            provider: "unknown".to_string(),
            base_url: "https://example.com".to_string(),
            input: vec![ModelInput::Text],
            output: vec!["image".to_string()],
            cost: ModelCost::default(),
            headers: None,
        };
        let out = generate_images(&model, &ImagesContext { input: vec![] }, &ImagesOptions::default());
        assert_eq!(out.stop_reason, ImagesStopReason::Error);
        assert!(out.error_message.as_deref().unwrap_or("").contains("No API provider registered"));
    }
}
