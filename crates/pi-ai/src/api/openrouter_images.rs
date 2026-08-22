//! OpenRouter image generation — port of
//! `packages/ai/src/api/openrouter-images.ts`.
//!
//! Routes the unified `ImagesContext` through the OpenAI-compatible
//! `/chat/completions` endpoint with `modalities` (image/text), then converts
//! the returned text + `data:` URI images into `AssistantImages`.

use serde_json::{json, Value};

use crate::images::{ImagesModel, ImagesOptions};
use crate::types::{AssistantImages, ContentBlock, ImagesContext, ImagesStopReason, Usage};

fn sanitize_surrogates(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if (0xD800..=0xDFFF).contains(&(c as u32)) {
            out.push('\u{FFFD}');
        } else {
            out.push(c);
        }
    }
    out
}

/// Build the `/chat/completions` request body (upstream `buildParams`).
pub fn build_params(model: &ImagesModel, context: &ImagesContext) -> Value {
    let content: Vec<Value> = context
        .input
        .iter()
        .map(|item| match item {
            ContentBlock::Text { text, .. } => json!({ "type": "text", "text": sanitize_surrogates(text) }),
            ContentBlock::Image { mime_type, data } => json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{mime_type};base64,{data}") }
            }),
            _ => Value::Null,
        })
        .filter(|v| !v.is_null())
        .collect();

    let modalities = if model.output.iter().any(|o| o == "text") {
        json!(["image", "text"])
    } else {
        json!(["image"])
    };

    json!({
        "model": model.id,
        "messages": [{ "role": "user", "content": content }],
        "stream": false,
        "modalities": modalities,
    })
}

/// Parse OpenAI usage into the unified Usage with cache/cost accounting
/// (upstream `parseUsage`).
pub fn parse_usage(raw_usage: &Value, model: &ImagesModel) -> Option<Usage> {
    if !raw_usage.is_object() {
        return None;
    }
    let prompt_tokens = raw_usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let reported_cached = raw_usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_write_tokens = raw_usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cache_write_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_read_tokens = if cache_write_tokens > 0 {
        reported_cached.saturating_sub(cache_write_tokens)
    } else {
        reported_cached
    };
    let input = prompt_tokens.saturating_sub(cache_read_tokens + cache_write_tokens);
    let output = raw_usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let input_cost = (model.cost.input / 1_000_000.0) * input as f64;
    let output_cost = (model.cost.output / 1_000_000.0) * output as f64;
    let cache_read_cost = (model.cost.cache_read / 1_000_000.0) * cache_read_tokens as f64;
    let cache_write_cost = (model.cost.cache_write / 1_000_000.0) * cache_write_tokens as f64;
    Some(Usage {
        input,
        output,
        cache_read: cache_read_tokens,
        cache_write: cache_write_tokens,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: input + output + cache_read_tokens + cache_write_tokens,
        cost: crate::types::Cost {
            input: input_cost,
            output: output_cost,
            cache_read: cache_read_cost,
            cache_write: cache_write_cost,
            total: input_cost + output_cost + cache_read_cost + cache_write_cost,
        },
    })
}

/// Generate images against OpenRouter's chat completions surface (upstream
/// `generateImages`). Never throws: failures are encoded on the output.
pub async fn generate_images(
    model: &ImagesModel,
    context: &ImagesContext,
    options: &ImagesOptions,
    client: reqwest::Client,
) -> AssistantImages {
    let model = model.clone();
    let context = context.clone();
    let options = options.clone();
    let mut output = AssistantImages {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        output: Vec::new(),
        response_id: None,
        usage: None,
        stop_reason: ImagesStopReason::Stop,
        error_message: None,
        timestamp: crate::types::now_ms(),
    };

    let api_key = match &options.api_key {
        Some(k) if !k.is_empty() => k.clone(),
        _ => {
            output.stop_reason = ImagesStopReason::Error;
            output.error_message = Some(format!("No API key for provider: {}", model.provider));
            return output;
        }
    };

    if options.aborted {
        output.stop_reason = ImagesStopReason::Aborted;
        output.error_message = Some("Request aborted".to_string());
        return output;
    }

    let params = build_params(&model, &context);
    let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));

    let mut request = client
        .post(&url)
        .header("content-type", "application/json")
        .bearer_auth(&api_key)
        .json(&params);
    if let Some(headers) = &model.headers {
        for (k, v) in headers {
            request = request.header(k.as_str(), v.as_str());
        }
    }
    if let Some(headers) = &options.headers {
        for (name, value) in headers {
            if let Some(value) = value {
                request = request.header(name.as_str(), value.as_str());
            }
        }
    }
    if let Some(timeout) = options.timeout_ms {
        request = request.timeout(std::time::Duration::from_millis(timeout));
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(err) => {
            output.stop_reason = if options.aborted { ImagesStopReason::Aborted } else { ImagesStopReason::Error };
            output.error_message = Some(format!("Request failed: {err}"));
            return output;
        }
    };
    let status = response.status();
    let headers_map = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect::<std::collections::BTreeMap<_, _>>();
    if let Some(on_response) = &options.on_response {
        on_response(
            &crate::types::ProviderResponse { status: status.as_u16(), headers: headers_map },
            &model.as_chat_model(),
        );
    }
    let body = match response.bytes().await {
        Ok(body) => body,
        Err(err) => {
            output.stop_reason = ImagesStopReason::Error;
            output.error_message = Some(format!("Request body failed: {err}"));
            return output;
        }
    };
    if !status.is_success() {
        let body_text = String::from_utf8_lossy(&body).to_string();
        output.stop_reason = ImagesStopReason::Error;
        output.error_message = Some(format!(
            "OpenRouter API error ({}): {}",
            status.as_u16(),
            crate::api::openai_completions::extract_openai_error(&body_text)
        ));
        return output;
    }
    let json: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            output.stop_reason = ImagesStopReason::Error;
            output.error_message = Some("OpenRouter returned a non-JSON image response".to_string());
            return output;
        }
    };
    apply_response(&mut output, &model, &json);
    output
}

/// Apply an OpenAI chat completion response to the output (upstream's
/// response handling).
pub fn apply_response(output: &mut AssistantImages, model: &ImagesModel, response: &Value) {
    if let Some(id) = response.get("id").and_then(|v| v.as_str()) {
        output.response_id = Some(id.to_string());
    }
    if let Some(usage) = response.get("usage").and_then(|u| parse_usage(u, model)) {
        output.usage = Some(usage);
    }
    let Some(choice) = response.get("choices").and_then(|c| c.as_array()).and_then(|c| c.first()) else {
        return;
    };
    let Some(message) = choice.get("message") else { return };
    if let Some(content) = message.get("content").and_then(|v| v.as_str()) {
        if !content.is_empty() {
            output.output.push(ContentBlock::text(content));
        }
    }
    if let Some(images) = message.get("images").and_then(|v| v.as_array()) {
        for image in images {
            let image_url = image
                .get("image_url")
                .and_then(|v| v.as_str())
                .or_else(|| image.get("image_url").and_then(|v| v.get("url")).and_then(|v| v.as_str()));
            let Some(image_url) = image_url else { continue };
            if !image_url.starts_with("data:") {
                continue;
            }
            if let Some(caps) = parse_data_uri(image_url) {
                output.output.push(ContentBlock::image(caps.1, caps.0));
            }
        }
    }
}

fn parse_data_uri(uri: &str) -> Option<(String, String)> {
    let rest = uri.strip_prefix("data:")?;
    let (mime, data) = rest.split_once(";base64,")?;
    Some((mime.to_string(), data.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::catalog_images;

    fn model(id: &str, output: &[&str]) -> ImagesModel {
        let mut m = catalog_images("openrouter").into_iter().find(|m| m.id == id).unwrap_or_else(|| {
            let mut base = catalog_images("openrouter").remove(0);
            base.id = id.to_string();
            base
        });
        m.output = output.iter().map(|s| s.to_string()).collect();
        m
    }

    #[test]
    fn build_params_modalities_match_model_output() {
        let m = model("google/gemini-2.5-flash-image", &["image", "text"]);
        let ctx = ImagesContext { input: vec![ContentBlock::text("Generate a dog")] };
        let params = build_params(&m, &ctx);
        assert_eq!(params["stream"], json!(false));
        assert_eq!(params["modalities"], json!(["image", "text"]));
        assert_eq!(params["messages"][0]["content"][0]["type"], json!("text"));
        assert_eq!(params["messages"][0]["content"][0]["text"], json!("Generate a dog"));
    }

    #[test]
    fn build_params_image_input_becomes_data_uri() {
        let m = model("black-forest-labs/flux.2-pro", &["image"]);
        let ctx = ImagesContext {
            input: vec![ContentBlock::image("aGVsbG8=", "image/png")],
        };
        let params = build_params(&m, &ctx);
        assert_eq!(params["modalities"], json!(["image"]));
        assert_eq!(params["messages"][0]["content"][0]["image_url"]["url"], json!("data:image/png;base64,aGVsbG8="));
    }

    #[test]
    fn parse_usage_accounts_cache_and_cost() {
        let mut m = model("google/gemini-2.5-flash-image", &["image"]);
        m.cost.input = 0.3;
        m.cost.output = 2.5;
        m.cost.cache_read = 0.03;
        let raw = json!({
            "prompt_tokens": 100,
            "completion_tokens": 34,
            "prompt_tokens_details": { "cached_tokens": 40, "cache_write_tokens": 10 }
        });
        let usage = parse_usage(&raw, &m).unwrap();
        // 100 prompt - 30 cacheRead - 10 cacheWrite (upstream formula).
        assert_eq!(usage.input, 60);
        assert_eq!(usage.output, 34);
        assert_eq!(usage.cache_read, 30);
        assert_eq!(usage.cache_write, 10);
        assert_eq!(usage.total_tokens, 134);
        assert!((usage.cost.input - 0.3 * 60.0 / 1_000_000.0).abs() < 1e-12);
        assert!((usage.cost.cache_read - 0.03 * 30.0 / 1_000_000.0).abs() < 1e-12);
    }

    #[test]
    fn apply_response_extracts_text_and_base64_images() {
        let mut out = AssistantImages {
            api: "openrouter-images".to_string(),
            provider: "openrouter".to_string(),
            model: "m".to_string(),
            output: vec![],
            response_id: None,
            usage: None,
            stop_reason: ImagesStopReason::Stop,
            error_message: None,
            timestamp: 1,
        };
        let response = json!({
            "id": "img-1",
            "choices": [{
                "message": {
                    "content": "Here is your image.",
                    "images": [{ "image_url": "data:image/png;base64,ZmFrZS1wbmc=" }]
                }
            }]
        });
        apply_response(&mut out, &model("fake", &["text"]), &response);
        assert_eq!(out.response_id.as_deref(), Some("img-1"));
        assert!(matches!(&out.output[0], ContentBlock::Text { text, .. } if text == "Here is your image."));
        match &out.output[1] {
            ContentBlock::Image { mime_type, data } => {
                assert_eq!(mime_type, "image/png");
                assert_eq!(data, "ZmFrZS1wbmc=");
            }
            other => panic!("expected image, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_api_key_returns_error_images() {
        let m = model("black-forest-labs/flux.2-pro", &["image"]);
        let client = reqwest::Client::new();
        let out = generate_images(&m, &ImagesContext { input: vec![] }, &ImagesOptions::default(), client).await;
        assert_eq!(out.stop_reason, ImagesStopReason::Error);
        assert!(out.error_message.as_deref().unwrap_or("").contains("No API key for provider: openrouter"));
    }
}
