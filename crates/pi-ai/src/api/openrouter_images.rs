//! OpenRouter image generation — port of
//! `packages/ai/src/api/openrouter-images.ts`.
//!
//! Routes the unified `ImagesContext` through the OpenAI-compatible
//! `/chat/completions` endpoint with `modalities` (image/text), then converts
//! the returned text + `data:` URI images into `AssistantImages`.

use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use crate::images::{ImagesModel, ImagesOptions};
use crate::types::{
    AssistantImages, ContentBlock, ImagesContext, ImagesStopReason, ProviderHeaders, Usage,
};

fn merged_request_headers(
    model_headers: Option<&std::collections::BTreeMap<String, String>>,
    option_headers: Option<&ProviderHeaders>,
) -> Vec<(String, String)> {
    let mut headers = model_headers
        .into_iter()
        .flat_map(|headers| {
            headers
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
        })
        .collect::<Vec<_>>();
    if let Some(option_headers) = option_headers {
        for (name, value) in option_headers {
            headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
            if let Some(value) = value {
                headers.push((name.clone(), value.clone()));
            }
        }
    }
    headers
}

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
            ContentBlock::Text { text, .. } => {
                json!({ "type": "text", "text": sanitize_surrogates(text) })
            }
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
    let prompt_tokens = raw_usage
        .get("prompt_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let reported_cached = raw_usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let cache_write_tokens = raw_usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cache_write_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let cache_read_tokens = if cache_write_tokens > 0 {
        reported_cached.saturating_sub(cache_write_tokens)
    } else {
        reported_cached
    };
    let input = prompt_tokens.saturating_sub(cache_read_tokens + cache_write_tokens);
    let output = raw_usage
        .get("completion_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
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

    if is_aborted(&options) {
        output.stop_reason = ImagesStopReason::Aborted;
        output.error_message = Some("Request aborted".to_string());
        return output;
    }

    let chat_model = model.as_chat_model();
    let params = match crate::api::openai_completions::apply_payload_hook(
        build_params(&model, &context),
        &chat_model,
        options.on_payload.as_ref(),
        options.abort_signal.clone(),
    )
    .await
    {
        Ok(params) => params,
        Err(_) => {
            output.stop_reason = ImagesStopReason::Aborted;
            output.error_message = Some("Request aborted".to_string());
            return output;
        }
    };
    let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));

    // Rebuild the request per attempt (RequestBuilder is not Clone, and
    // upstream's retryProviderRequest issues a fresh SDK request each retry).
    let make_request = || {
        let mut request = client
            .post(&url)
            .header("content-type", "application/json")
            .bearer_auth(&api_key)
            .json(&params);
        for (name, value) in
            merged_request_headers(model.headers.as_ref(), options.headers.as_ref())
        {
            request = request.header(name, value);
        }
        if let Some(timeout) = options.timeout_ms {
            request = request.timeout(std::time::Duration::from_millis(timeout));
        }
        request
    };

    let max_retries = options.max_retries.unwrap_or(0);
    let mut retries_remaining = max_retries;
    let mut response = None;
    let mut response_err = None;
    for attempt in 0.. {
        // Each retry rebuilds the request (fresh per upstream
        // retryProviderRequest, which notes X-Stainless-Retry-Count stays 0).
        let attempt_result = send_request(make_request(), options.abort_signal.clone()).await;
        match attempt_result {
            Ok(resp) => {
                if is_aborted(&options) {
                    output.stop_reason = ImagesStopReason::Aborted;
                    output.error_message = Some("Request aborted".to_string());
                    return output;
                }
                let status = resp.status();
                let should_retry = retryable_provider_status(
                    status.as_u16(),
                    resp.headers()
                        .get("x-should-retry")
                        .and_then(|v| v.to_str().ok()),
                );
                if !should_retry || retries_remaining == 0 {
                    response = Some(resp);
                    break;
                }
                // Retryable status with remaining budget: compute the delay.
                let headers_map = resp
                    .headers()
                    .iter()
                    .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                    .collect::<std::collections::BTreeMap<_, _>>();
                match retry_delay_ms(
                    &headers_map,
                    max_retries - retries_remaining,
                    options.max_retry_delay_ms,
                    status.as_u16(),
                ) {
                    Ok(Some(delay)) => {
                        if !sleep_retry(delay, &options).await {
                            output.stop_reason = ImagesStopReason::Aborted;
                            output.error_message = Some("Request aborted".to_string());
                            return output;
                        }
                    }
                    Ok(None) => {}
                    Err(msg) => {
                        response_err = Some(msg);
                        break;
                    }
                }
                retries_remaining -= 1;
                let _ = attempt;
            }
            Err(AttemptError::Aborted) => {
                output.stop_reason = ImagesStopReason::Aborted;
                output.error_message = Some("Request aborted".to_string());
                return output;
            }
            Err(AttemptError::Transport(err)) => {
                // Transport errors retry only while budget remains (upstream
                // treats undefined status as retryable).
                if retries_remaining == 0 {
                    response_err = Some(format!("Request failed: {err}"));
                    break;
                }
                let delay = exponential_retry_delay(max_retries - retries_remaining);
                if !sleep_retry(delay, &options).await {
                    output.stop_reason = ImagesStopReason::Aborted;
                    output.error_message = Some("Request aborted".to_string());
                    return output;
                }
                retries_remaining -= 1;
            }
        }
    }
    let response = match response {
        Some(resp) => resp,
        None => {
            output.stop_reason = if is_aborted(&options) {
                ImagesStopReason::Aborted
            } else {
                ImagesStopReason::Error
            };
            output.error_message =
                Some(response_err.unwrap_or_else(|| "Request failed".to_string()));
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
            &crate::types::ProviderResponse {
                status: status.as_u16(),
                headers: headers_map.clone(),
            },
            &model.as_chat_model(),
        );
    }
    let body = match read_response_body(response, options.abort_signal.clone()).await {
        Ok(body) => body,
        Err(AttemptError::Aborted) => {
            output.stop_reason = ImagesStopReason::Aborted;
            output.error_message = Some("Request aborted".to_string());
            return output;
        }
        Err(AttemptError::Transport(err)) => {
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
            output.error_message =
                Some("OpenRouter returned a non-JSON image response".to_string());
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
    let Some(choice) = response
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
    else {
        return;
    };
    let Some(message) = choice.get("message") else {
        return;
    };
    if let Some(content) = message.get("content").and_then(|v| v.as_str()) {
        if !content.is_empty() {
            output.output.push(ContentBlock::text(content));
        }
    }
    if let Some(images) = message.get("images").and_then(|v| v.as_array()) {
        for image in images {
            let image_url = image.get("image_url").and_then(|v| v.as_str()).or_else(|| {
                image
                    .get("image_url")
                    .and_then(|v| v.get("url"))
                    .and_then(|v| v.as_str())
            });
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

/// Retryable provider status (upstream `isRetryableProviderError`):
/// `x-should-retry` header wins; otherwise 408/409/429/>=500.
fn retryable_provider_status(status: u16, x_should_retry: Option<&str>) -> bool {
    match x_should_retry {
        Some("true") => return true,
        Some("false") => return false,
        _ => {}
    }
    status == 408 || status == 409 || status == 429 || status >= 500
}

/// Retry delay from headers or exponential backoff (upstream
/// `getRetryDelayMs` + `validateServerRetryDelayMs`). `None` means
/// immediately retry (no server delay guidance).
fn retry_delay_ms(
    headers: &std::collections::BTreeMap<String, String>,
    retry_index: u32,
    max_retry_delay_ms: Option<u64>,
    status: u16,
) -> Result<Option<u64>, String> {
    if let Some(retry_after_ms) = headers.get("retry-after-ms") {
        if let Ok(value) = retry_after_ms.parse::<f64>() {
            return validate_retry_delay(value as u64, max_retry_delay_ms, status);
        }
    }
    if let Some(retry_after) = headers.get("retry-after") {
        if let Ok(seconds) = retry_after.parse::<f64>() {
            let delay = (seconds * 1000.0) as u64;
            return validate_retry_delay(delay, max_retry_delay_ms, status);
        }
        if let Some(retry_at_ms) = parse_http_date_epoch_ms(retry_after) {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis() as i128)
                .unwrap_or(0);
            let delay = retry_at_ms.saturating_sub(now_ms).max(0) as u64;
            return validate_retry_delay(delay, max_retry_delay_ms, status);
        }
    }
    Ok(Some(exponential_retry_delay(retry_index)))
}

/// Parse the IMF-fixdate form required by HTTP `Retry-After` into Unix epoch
/// milliseconds. The upstream uses `Date.parse`; keeping this small parser
/// local avoids a new runtime dependency while accepting the same wire form.
fn parse_http_date_epoch_ms(value: &str) -> Option<i128> {
    let fields: Vec<&str> = value.split_whitespace().collect();
    if fields.len() != 6 || fields[5] != "GMT" {
        return None;
    }
    let day = fields[1].parse::<u32>().ok()?;
    let year = fields[3].parse::<i64>().ok()?;
    let month = match fields[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    if !(1..=31).contains(&day) || year < 1 {
        return None;
    }
    let mut time = fields[4].split(':');
    let hour = time.next()?.parse::<u32>().ok()?;
    let minute = time.next()?.parse::<u32>().ok()?;
    let second = time.next()?.parse::<u32>().ok()?;
    if time.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day)?;
    Some(
        (days as i128 * 86_400 + hour as i128 * 3_600 + minute as i128 * 60 + second as i128)
            * 1000,
    )
}

/// Days from the proleptic Gregorian calendar to 1970-01-01, adapted from
/// the civil-date algorithm used by standard library date implementations.
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    let max_day = match month {
        2 => {
            if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return None,
    };
    if day == 0 || day > max_day {
        return None;
    }
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year / 400
    } else {
        (adjusted_year - 399) / 400
    };
    let year_of_era = adjusted_year - era * 400;
    let month_prime = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

fn validate_retry_delay(
    delay_ms: u64,
    max_retry_delay_ms: Option<u64>,
    status: u16,
) -> Result<Option<u64>, String> {
    let max = max_retry_delay_ms.unwrap_or(60_000);
    if max > 0 && delay_ms > max {
        return Err(format!(
            "Server requested {}s retry delay (max: {}s) (HTTP {status})",
            delay_ms.div_ceil(1000),
            max.div_ceil(1000)
        ));
    }
    Ok(Some(delay_ms))
}

/// Exponential backoff with jitter: min(0.5 * 2^i, 8) seconds * (1 - 0.25r).
fn exponential_retry_delay(retry_index: u32) -> u64 {
    let base = (0.5 * 2f64.powi(retry_index as i32)).min(8.0) * 1000.0;
    (base * (1.0 - rand01() * 0.25)) as u64
}

fn rand01() -> f64 {
    // Deterministic-ish PRNG to keep tests stable without a rand dep.
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (n % 1_000_000) as f64 / 1_000_000.0
}

enum AttemptError {
    Aborted,
    Transport(String),
}

fn is_aborted(options: &ImagesOptions) -> bool {
    options.aborted
        || options
            .abort_signal
            .as_ref()
            .is_some_and(|signal| signal.load(Ordering::SeqCst))
}

async fn wait_for_abort(signal: Arc<AtomicBool>) {
    while !signal.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

async fn send_request(
    request: reqwest::RequestBuilder,
    signal: Option<Arc<AtomicBool>>,
) -> Result<reqwest::Response, AttemptError> {
    if signal
        .as_ref()
        .is_some_and(|value| value.load(Ordering::SeqCst))
    {
        return Err(AttemptError::Aborted);
    }
    if let Some(signal) = signal {
        tokio::select! {
            result = request.send() => result.map_err(|error| AttemptError::Transport(error.to_string())),
            _ = wait_for_abort(signal) => Err(AttemptError::Aborted),
        }
    } else {
        request
            .send()
            .await
            .map_err(|error| AttemptError::Transport(error.to_string()))
    }
}

async fn read_response_body(
    response: reqwest::Response,
    signal: Option<Arc<AtomicBool>>,
) -> Result<Vec<u8>, AttemptError> {
    if let Some(signal) = signal {
        tokio::select! {
            result = response.bytes() => result
                .map(|bytes| bytes.to_vec())
                .map_err(|error| AttemptError::Transport(error.to_string())),
            _ = wait_for_abort(signal) => Err(AttemptError::Aborted),
        }
    } else {
        response
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|error| AttemptError::Transport(error.to_string()))
    }
}

async fn sleep_retry(delay_ms: u64, options: &ImagesOptions) -> bool {
    if is_aborted(options) {
        return false;
    }
    let Some(signal) = options.abort_signal.clone() else {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        return !is_aborted(options);
    };
    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms) ) => !is_aborted(options),
        _ = wait_for_abort(signal) => false,
    }
}

fn parse_data_uri(uri: &str) -> Option<(String, String)> {
    let rest = uri.strip_prefix("data:")?;
    let (mime, data) = rest.split_once(";base64,")?;
    if mime.is_empty() || data.is_empty() {
        return None;
    }
    Some((mime.to_string(), data.to_string()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::images::catalog_images;

    fn model(id: &str, output: &[&str]) -> ImagesModel {
        let mut m = catalog_images("openrouter")
            .into_iter()
            .find(|m| m.id == id)
            .unwrap_or_else(|| {
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
        let ctx = ImagesContext {
            input: vec![ContentBlock::text("Generate a dog")],
        };
        let params = build_params(&m, &ctx);
        assert_eq!(params["stream"], json!(false));
        assert_eq!(params["modalities"], json!(["image", "text"]));
        assert_eq!(params["messages"][0]["content"][0]["type"], json!("text"));
        assert_eq!(
            params["messages"][0]["content"][0]["text"],
            json!("Generate a dog")
        );
    }

    #[test]
    fn build_params_image_input_becomes_data_uri() {
        let m = model("black-forest-labs/flux.2-pro", &["image"]);
        let ctx = ImagesContext {
            input: vec![ContentBlock::image("aGVsbG8=", "image/png")],
        };
        let params = build_params(&m, &ctx);
        assert_eq!(params["modalities"], json!(["image"]));
        assert_eq!(
            params["messages"][0]["content"][0]["image_url"]["url"],
            json!("data:image/png;base64,aGVsbG8=")
        );
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
        assert!(
            matches!(&out.output[0], ContentBlock::Text { text, .. } if text == "Here is your image.")
        );
        match &out.output[1] {
            ContentBlock::Image { mime_type, data } => {
                assert_eq!(mime_type, "image/png");
                assert_eq!(data, "ZmFrZS1wbmc=");
            }
            other => panic!("expected image, got {other:?}"),
        }
    }

    #[test]
    fn apply_response_ignores_malformed_empty_data_images() {
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
            "choices": [{
                "message": {
                    "images": [
                        { "image_url": "data:;base64,ZmFrZQ==" },
                        { "image_url": "data:image/png;base64," },
                        { "image_url": "https://example.test/image.png" }
                    ]
                }
            }]
        });

        apply_response(&mut out, &model("fake", &["image"]), &response);

        assert!(out.output.is_empty());
    }

    #[tokio::test]
    async fn no_api_key_returns_error_images() {
        let m = model("black-forest-labs/flux.2-pro", &["image"]);
        let client = reqwest::Client::new();
        let out = generate_images(
            &m,
            &ImagesContext { input: vec![] },
            &ImagesOptions::default(),
            client,
        )
        .await;
        assert_eq!(out.stop_reason, ImagesStopReason::Error);
        assert!(out
            .error_message
            .as_deref()
            .unwrap_or("")
            .contains("No API key for provider: openrouter"));
    }

    #[tokio::test]
    async fn payload_hook_replaces_image_request_before_transport() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let (header_end, content_length) = loop {
                let mut chunk = [0u8; 4096];
                let count = socket.read(&mut chunk).await.unwrap();
                assert!(count > 0, "client closed before sending request");
                request.extend_from_slice(&chunk[..count]);
                let Some(separator) = request.windows(4).position(|window| window == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..separator]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .expect("request content length");
                if request.len() >= separator + 4 + content_length {
                    break (separator, content_length);
                }
            };
            let body_start = header_end + 4;
            let body: Value =
                serde_json::from_slice(&request[body_start..body_start + content_length]).unwrap();
            let response_body = r#"{"id":"fixture","choices":[{"message":{"content":"ok"}}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            body
        });

        let mut model = model("black-forest-labs/flux.2-pro", &["text"]);
        model.base_url = format!("http://{address}");
        let hook: crate::types::OnPayloadFn = Arc::new(|mut payload, _model| {
            Box::pin(async move {
                payload["fixture_marker"] = json!("payload-hook");
                Some(payload)
            })
        });
        let output = generate_images(
            &model,
            &ImagesContext {
                input: vec![ContentBlock::text("draw a cat")],
            },
            &ImagesOptions {
                api_key: Some("test-key".to_string()),
                on_payload: Some(hook),
                ..Default::default()
            },
            reqwest::Client::new(),
        )
        .await;
        assert!(output.error_message.is_none(), "{:?}", output.error_message);
        assert_eq!(server.await.unwrap()["fixture_marker"], "payload-hook");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod retry_tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn null_option_header_suppresses_model_header() {
        let model_headers = BTreeMap::from([("X-Model-Header".to_string(), "secret".to_string())]);
        let option_headers = BTreeMap::from([("x-model-header".to_string(), None)]);

        let merged = merged_request_headers(Some(&model_headers), Some(&option_headers));

        assert!(!merged
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("x-model-header")));
    }

    #[test]
    fn retryable_status_classification() {
        assert!(!retryable_provider_status(200, None));
        assert!(retryable_provider_status(408, None));
        assert!(retryable_provider_status(409, None));
        assert!(retryable_provider_status(429, None));
        assert!(retryable_provider_status(500, None));
        assert!(!retryable_provider_status(400, None));
        assert!(retryable_provider_status(400, Some("true")));
        assert!(!retryable_provider_status(500, Some("false")));
    }

    #[test]
    fn server_retry_delay_headers_and_cap() {
        let mut headers = BTreeMap::new();
        headers.insert("retry-after-ms".to_string(), "250".to_string());
        assert_eq!(retry_delay_ms(&headers, 0, None, 429).unwrap(), Some(250));
        headers.clear();
        headers.insert("retry-after".to_string(), "1".to_string());
        assert_eq!(retry_delay_ms(&headers, 0, None, 429).unwrap(), Some(1000));
        headers.clear();
        headers.insert("retry-after-ms".to_string(), "120000".to_string());
        let err = retry_delay_ms(&headers, 0, Some(60_000), 429).unwrap_err();
        assert!(err.contains("max"), "{err}");
        // No header -> exponential backoff: 0.5*2^i seconds with jitter
        // (index 0: 375..500ms; index 3: 0.5*2^3=4s -> 3000..4000ms).
        headers.clear();
        let delay = retry_delay_ms(&headers, 0, None, 429).unwrap().unwrap();
        assert!((375..=1000).contains(&delay), "{delay}");
        let delay3 = retry_delay_ms(&headers, 3, None, 429).unwrap().unwrap();
        assert!((3000..=4000).contains(&delay3), "{delay3}");
    }

    #[test]
    fn parses_http_date_retry_after_and_applies_cap() {
        assert_eq!(
            parse_http_date_epoch_ms("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(1_445_412_480_000)
        );
        let mut headers = BTreeMap::new();
        headers.insert(
            "retry-after".to_string(),
            "Tue, 01 Jan 2030 00:00:00 GMT".to_string(),
        );
        let error = retry_delay_ms(&headers, 0, Some(60_000), 429).unwrap_err();
        assert!(error.contains("retry delay"), "{error}");
    }

    #[tokio::test]
    async fn retries_429_then_succeeds() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let serve = tokio::spawn(async move {
            let mut count = 0;
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await;
                count += 1;
                if count == 1 {
                    let resp = "HTTP/1.1 429 Too Many Requests\r\nretry-after-ms: 10\r\ncontent-length: 5\r\n\r\nerror";
                    let _ = socket.write_all(resp.as_bytes()).await;
                } else {
                    let body = "{\"id\":\"img\",\"choices\":[{\"message\":{\"content\":\"gen\",\"images\":[{\"image_url\":\"data:image/png;base64,aGVsbG8=\"}]}}]}";
                    let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len());
                    let _ = socket.write_all(resp.as_bytes()).await;
                    break;
                }
            }
            count
        });
        let model = crate::images::catalog_images("openrouter")
            .into_iter()
            .next()
            .expect("openrouter image model");
        let mut m = model.clone();
        m.base_url = format!("http://{addr}");
        let options = ImagesOptions {
            api_key: Some("test-key".to_string()),
            max_retries: Some(2),
            max_retry_delay_ms: Some(60_000),
            ..Default::default()
        };
        let context = ImagesContext {
            input: vec![ContentBlock::text("draw a frog")],
        };
        let output = generate_images(&m, &context, &options, reqwest::Client::new()).await;
        assert!(output.error_message.is_none(), "{:?}", output.error_message);
        assert!(
            output
                .output
                .iter()
                .any(|b| matches!(b, ContentBlock::Image { .. })),
            "expected an image block"
        );
        assert!(serve.await.unwrap() >= 2, "expected at least one retry");
    }

    #[tokio::test]
    async fn aborts_retry_backoff_without_a_second_request() {
        use std::sync::atomic::AtomicBool;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let response = "HTTP/1.1 429 Too Many Requests\r\nretry-after-ms: 1000\r\ncontent-length: 5\r\n\r\nerror";
            socket.write_all(response.as_bytes()).await.unwrap();
            1usize
        });
        let model = crate::images::catalog_images("openrouter")
            .into_iter()
            .find(|candidate| candidate.id == "black-forest-labs/flux.2-pro")
            .expect("openrouter image model");
        let signal = Arc::new(AtomicBool::new(false));
        let task_signal = signal.clone();
        let options = ImagesOptions {
            api_key: Some("test-key".to_string()),
            max_retries: Some(2),
            abort_signal: Some(task_signal),
            ..Default::default()
        };
        let task = tokio::spawn(async move {
            generate_images(
                &ImagesModel {
                    base_url: format!("http://{addr}"),
                    ..model
                },
                &ImagesContext { input: vec![] },
                &options,
                reqwest::Client::new(),
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        signal.store(true, Ordering::SeqCst);
        let output = task.await.unwrap();
        assert_eq!(output.stop_reason, ImagesStopReason::Aborted);
        assert_eq!(server.await.unwrap(), 1);
    }
}
