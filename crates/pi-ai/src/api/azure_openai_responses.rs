//! Azure OpenAI Responses adaptor — port of
//! `packages/ai/src/api/azure-openai-responses.ts`.
//!
//! Routes the OpenAI Responses request to Azure's deployed
//! `/deployments/<deployment>/responses?api-version=` surface, resolving the
//! resource/deployment name from env or options. Message conversion, tool
//! conversion and the stream processor come from `openai_responses_shared`.

use serde_json::{json, Value};

use crate::model::{clamp_thinking_level, Model};
use crate::types::{
    AssistantMessage, AssistantMessageEvent, Context, DoneReason, ErrorReason,
    ModelThinkingLevel, SimpleStreamOptions, StopReason, StreamOptions, ToolChoice, Usage,
};
use crate::event_stream::{AssistantMessageEventStream, StreamSink};
use crate::sse::SseParser;

use super::openai_responses_shared::*;

const DEFAULT_AZURE_API_VERSION: &str = "v1";
const OPENAI_RESPONSES_MIN_OUTPUT_TOKENS: u64 = 16;
const AZURE_TOOL_CALL_PROVIDERS: [&str; 4] = ["openai", "openai-codex", "opencode", "azure-openai-responses"];

#[derive(Clone)]
pub struct AzureOpenAIResponsesOptions {
    pub base: StreamOptions,
    pub reasoning_effort: Option<String>,
    pub reasoning_summary: Option<String>,
    pub tool_choice: Option<Value>,
    pub azure_api_version: Option<String>,
    pub azure_resource_name: Option<String>,
    pub azure_base_url: Option<String>,
    pub azure_deployment_name: Option<String>,
}

impl Default for AzureOpenAIResponsesOptions {
    fn default() -> Self {
        Self {
            base: StreamOptions::default(),
            reasoning_effort: None,
            reasoning_summary: None,
            tool_choice: None,
            azure_api_version: None,
            azure_resource_name: None,
            azure_base_url: None,
            azure_deployment_name: None,
        }
    }
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn parse_deployment_name_map(value: Option<&str>) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Some(value) = value else { return map };
    for entry in value.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if let Some((model_id, deployment)) = entry.split_once('=') {
            let model_id = model_id.trim();
            let deployment = deployment.trim();
            if !model_id.is_empty() && !deployment.is_empty() {
                map.insert(model_id.to_string(), deployment.to_string());
            }
        }
    }
    map
}

fn resolve_deployment_name(model: &Model, options: &AzureOpenAIResponsesOptions) -> String {
    if let Some(name) = &options.azure_deployment_name {
        return name.clone();
    }
    let map = parse_deployment_name_map(env_value("AZURE_OPENAI_DEPLOYMENT_NAME_MAP").as_deref());
    if let Some(name) = map.get(&model.id) {
        return name.clone();
    }
    model.id.clone()
}

fn normalize_azure_base_url(base_url: &str) -> Result<String, String> {
    let trimmed = base_url.trim().trim_end_matches('/');
    let parsed = url::Url::parse(trimmed).map_err(|_| format!("Invalid Azure OpenAI base URL: {base_url}"))?;
    let is_azure_host = parsed.host_str().is_some_and(|h| {
        h.ends_with(".openai.azure.com")
            || h.ends_with(".cognitiveservices.azure.com")
            || h.ends_with(".ai.azure.com")
    });
    let normalized_path = parsed.path().trim_end_matches('/').to_string();
    let mut url = parsed;
    if is_azure_host
        && matches!(normalized_path.as_str(), "" | "/" | "/openai" | "/openai/v1/responses")
    {
        url.set_path("/openai/v1");
        url.set_query(None);
    }
    let mut out = url.to_string();
    while out.ends_with('/') {
        out.pop();
    }
    Ok(out)
}

fn build_default_base_url(resource_name: &str) -> String {
    format!("https://{resource_name}.openai.azure.com/openai/v1")
}

fn resolve_azure_config(model: &Model, options: &AzureOpenAIResponsesOptions) -> Result<(String, String), String> {
    let api_version = options
        .azure_api_version
        .clone()
        .or_else(|| env_value("AZURE_OPENAI_API_VERSION"))
        .unwrap_or_else(|| DEFAULT_AZURE_API_VERSION.to_string());

    let base_url = options
        .azure_base_url
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| env_value("AZURE_OPENAI_BASE_URL").filter(|s| !s.trim().is_empty()));
    let resource_name = options
        .azure_resource_name
        .clone()
        .or_else(|| env_value("AZURE_OPENAI_RESOURCE_NAME"));

    let resolved = if let Some(base_url) = &base_url {
        Some(base_url.clone())
    } else if let Some(resource_name) = &resource_name {
        Some(build_default_base_url(resource_name))
    } else if !model.base_url.is_empty() {
        Some(model.base_url.clone())
    } else {
        None
    };
    let Some(resolved) = resolved else {
        return Err(
            "Azure OpenAI base URL is required. Set AZURE_OPENAI_BASE_URL or AZURE_OPENAI_RESOURCE_NAME, or pass azureBaseUrl, azureResourceName, or model.baseUrl."
                .to_string(),
        );
    };
    Ok((normalize_azure_base_url(&resolved)?, api_version))
}

fn build_params(
    model: &Model,
    context: &Context,
    options: &AzureOpenAIResponsesOptions,
    deployment_name: &str,
) -> Value {
    let messages = convert_responses_messages(
        model,
        context,
        &AZURE_TOOL_CALL_PROVIDERS,
        &ConvertResponsesMessagesOptions::default(),
    );

    let mut params = json!({
        "model": deployment_name,
        "input": messages,
        "stream": true,
        "prompt_cache_key": options.base.session_id.as_deref().map(|s| {
            s.chars().take(64).collect::<String>()
        }),
        "store": false,
    });

    if let Some(max_tokens) = options.base.max_tokens {
        params["max_output_tokens"] = json!(max_tokens.max(OPENAI_RESPONSES_MIN_OUTPUT_TOKENS));
    }
    if let Some(temperature) = options.base.temperature {
        params["temperature"] = json!(temperature);
    }
    if !context.tools.is_empty() {
        let supports_strict = model
            .compat
            .as_ref()
            .and_then(|c| c.get("supportsStrictMode"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        params["tools"] = json!(convert_responses_tools(
            &context.tools,
            &ConvertResponsesToolsOptions {
                strict: None,
                supports_strict_mode: supports_strict,
                supports_openai_grammar_tools: false,
            },
        ));
    }
    if let Some(tool_choice) = &options.tool_choice {
        params["tool_choice"] = tool_choice.clone();
    }

    if model.reasoning {
        if options.reasoning_effort.is_some() || options.reasoning_summary.is_some() {
            let effort = options
                .reasoning_effort
                .clone()
                .or_else(|| Some("medium".to_string()));
            let effort = model
                .thinking_level_map
                .as_ref()
                .and_then(|m| m.get(&ModelThinkingLevel::from_effort_str(effort.as_deref().unwrap_or("medium"))))
                .cloned()
                .flatten()
                .unwrap_or_else(|| effort.unwrap_or_else(|| "medium".to_string()));
            params["reasoning"] = json!({
                "effort": effort,
                "summary": options.reasoning_summary.clone().unwrap_or_else(|| "auto".to_string()),
            });
            params["include"] = json!(["reasoning.encrypted_content"]);
        } else if model
            .thinking_level_map
            .as_ref()
            .and_then(|m| m.get(&ModelThinkingLevel::Off))
            .is_some()
        {
            let off_entry = model
                .thinking_level_map
                .as_ref()
                .and_then(|m| m.get(&ModelThinkingLevel::Off))
                .cloned()
                .flatten()
                .unwrap_or_else(|| "none".to_string());
            params["reasoning"] = json!({ "effort": off_entry });
        }
    }

    if let Some(sp) = &options.base.sampling_params {
        if let Some(obj) = sp.as_object() {
            for (k, v) in obj {
                params[k] = v.clone();
            }
        }
    }
    params
}

fn new_output(model: &Model) -> AssistantMessage {
    let mut output = AssistantMessage::new();
    output.set_api_provider_model(&model.api, &model.provider, &model.id);
    output.set_stop_reason(StopReason::Pending);
    let AssistantMessage::Assistant { usage, .. } = &mut output;
    *usage = Some(Usage::default());
    output
}

/// Streams a request against the Azure OpenAI Responses API.
pub fn stream(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    api_key: Option<&str>,
    options: &AzureOpenAIResponsesOptions,
) -> AssistantMessageEventStream {
    let stream = AssistantMessageEventStream::new();
    let Some(sender) = stream.sender() else { return stream };
    let model = model.clone();
    let context = context.clone();
    let options = options.clone();
    let Some(api_key) = api_key.map(|s| s.to_string()).or_else(|| env_value("AZURE_OPENAI_API_KEY")) else {
        let mut message = new_output(&model);
        message.set_stop_reason(StopReason::Error);
        super::anthropic_messages::set_error_message(&mut message, format!("No API key for provider: {}", model.provider));
        return crate::event_stream::create_error_stream(
            &model.api,
            &model.provider,
            &model.id,
            message.error_message().unwrap_or("").to_string(),
        );
    };
    let deployment_name = resolve_deployment_name(&model, &options);

    let handle = tokio::spawn(async move {
        let mut pusher = crate::event_stream::StreamSinkAdapter::new(sender);
        let (base_url, api_version) = match resolve_azure_config(&model, &options) {
            Ok(cfg) => cfg,
            Err(err) => {
                let mut message = new_output(&model);
                message.set_stop_reason(StopReason::Error);
                super::anthropic_messages::set_error_message(&mut message, err);
                pusher.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    error_message: message.clone(),
                });
                pusher.end(Some(message));
                return;
            }
        };
        let params = build_params(&model, &context, &options, &deployment_name);
        let endpoint = format!("{base_url}/deployments/{deployment_name}/responses?api-version={api_version}");
        let mut request = client
            .post(&endpoint)
            .header("content-type", "application/json")
            .bearer_auth(&api_key)
            .query(&[("api-version", api_version.as_str())]);
        if let Some(headers) = &model.headers {
            for (name, value) in headers {
                request = request.header(name.as_str(), value.as_str());
            }
        }
        if let Some(headers) = &options.base.base.headers {
            for (name, value) in headers {
                if let Some(value) = value {
                    request = request.header(name.as_str(), value.as_str());
                }
            }
        }
        request = request.json(&params);

        let response = match request.send().await {
            Ok(response) => response,
            Err(err) => {
                let mut message = new_output(&model);
                message.set_stop_reason(StopReason::Error);
                super::anthropic_messages::set_error_message(&mut message, format!("Request failed: {err}"));
                pusher.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    error_message: message.clone(),
                });
                pusher.end(Some(message));
                return;
            }
        };
        let status = response.status();
        let provider_response = crate::types::ProviderResponse { status: status.as_u16(), headers: Default::default() };
        if let Some(on_response) = &options.base.on_response {
            on_response(&provider_response, &model);
        }
        let body = match response.bytes().await {
            Ok(body) => body,
            Err(err) => {
                let mut message = new_output(&model);
                message.set_stop_reason(StopReason::Error);
                super::anthropic_messages::set_error_message(&mut message, format!("Request body failed: {err}"));
                pusher.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    error_message: message.clone(),
                });
                pusher.end(Some(message));
                return;
            }
        };
        if !status.is_success() {
            let body_text = String::from_utf8_lossy(&body).to_string();
            let detail = super::openai_responses::extract_openai_responses_error(&body_text);
            let mut message = new_output(&model);
            message.set_stop_reason(StopReason::Error);
            super::anthropic_messages::set_error_message(
                &mut message,
                format!("Azure OpenAI API error ({}): {}", status.as_u16(), detail),
            );
            pusher.push(AssistantMessageEvent::Error {
                reason: ErrorReason::Error,
                error_message: message.clone(),
            });
            pusher.end(Some(message));
            return;
        }
        let body_text = String::from_utf8_lossy(&body).to_string();
        let events = SseParser::parse_text(&body_text);

        pusher.push(AssistantMessageEvent::Start { partial: new_output(&model) });
        let mut output = new_output(&model);
        match process_responses_stream(
            &events,
            &mut output,
            &mut |event| pusher.push(event),
            &model,
            &ProcessResponsesOptions::default(),
        ) {
            Ok(()) => {
                let reason = match output.stop_reason().unwrap_or(StopReason::Stop) {
                    StopReason::Stop => DoneReason::Stop,
                    StopReason::Length => DoneReason::Length,
                    StopReason::ToolUse => DoneReason::ToolUse,
                    StopReason::Deferred => DoneReason::Deferred,
                    _ => DoneReason::Stop,
                };
                pusher.push(AssistantMessageEvent::Done { reason, message: output.clone() });
                pusher.end(Some(output));
            }
            Err(err) => {
                let mut message = new_output(&model);
                message.set_stop_reason(StopReason::Error);
                super::anthropic_messages::set_error_message(&mut message, err);
                pusher.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    error_message: message.clone(),
                });
                pusher.end(Some(message));
            }
        }
    });
    std::mem::forget(handle);
    stream
}

/// Simple stream for Azure (upstream `streamSimple`).
pub fn stream_simple(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    api_key: Option<&str>,
    options: &SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let reasoning_effort = options
        .reasoning
        .map(|r| {
            let clamped = clamp_thinking_level(model, ModelThinkingLevel::from(r));
            if clamped == ModelThinkingLevel::Off {
                None
            } else {
                Some(clamped.as_str().to_string())
            }
        })
        .flatten();
    let go = AzureOpenAIResponsesOptions {
        base: options.base.clone(),
        reasoning_effort,
        reasoning_summary: None,
        tool_choice: options.tool_choice.as_ref().map(|t| match t {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
        }),
        ..Default::default()
    };
    stream(model, context, client, api_key, &go)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Model, ModelInput};
    use crate::types::*;

    fn model(id: &str) -> Model {
        let mut m = Model::new(id, id, "azure-openai-responses", "azure-openai-responses");
        m.reasoning = true;
        m.input = vec![ModelInput::Text, ModelInput::Image];
        m
    }

    fn ctx() -> Context {
        Context {
            system_prompt: Some("You are helpful.".to_string()),
            messages: vec![Message::User(UserContent::string("hello", 1))],
            tools: vec![json_tool("bash", "run a command", &json!({"type":"object","properties":{}}))],
        }
    }

    #[test]
    fn deployment_name_from_map_env() {
        unsafe { std::env::set_var("AZURE_OPENAI_DEPLOYMENT_NAME_MAP", "gpt-5=my-deploy-1, gpt-5-mini = my-mini"); }
        let m = model("gpt-5");
        let name = resolve_deployment_name(&m, &AzureOpenAIResponsesOptions::default());
        assert_eq!(name, "my-deploy-1");
        unsafe { std::env::remove_var("AZURE_OPENAI_DEPLOYMENT_NAME_MAP"); }
    }

    #[test]
    fn deployment_name_defaults_to_model_id() {
        let m = model("gpt-5");
        let name = resolve_deployment_name(&m, &AzureOpenAIResponsesOptions::default());
        assert_eq!(name, "gpt-5");
    }

    #[test]
    fn azure_host_normalization() {
        assert_eq!(
            normalize_azure_base_url("https://my-resource.openai.azure.com").unwrap(),
            "https://my-resource.openai.azure.com/openai/v1"
        );
        assert_eq!(
            normalize_azure_base_url("https://my-resource.openai.azure.com/openai").unwrap(),
            "https://my-resource.openai.azure.com/openai/v1"
        );
        // Custom hosts untouched.
        assert_eq!(
            normalize_azure_base_url("https://example.com/v1").unwrap(),
            "https://example.com/v1"
        );
    }

    #[test]
    fn params_shape_uses_deployment() {
        let m = model("gpt-5");
        unsafe { std::env::remove_var("AZURE_OPENAI_DEPLOYMENT_NAME_MAP"); }
        let params = build_params(&m, &ctx(), &AzureOpenAIResponsesOptions::default(), "gpt-5");
        assert_eq!(params["model"], "gpt-5");
        assert_eq!(params["stream"], true);
        assert_eq!(params["input"][0]["role"], "developer");
        assert_eq!(params["tools"][0]["name"], "bash");
    }

    #[test]
    fn stream_missing_key_is_terminal_error() {
        let m = model("gpt-5");
        unsafe { std::env::remove_var("AZURE_OPENAI_API_KEY"); }
        let s = stream(&m, &Context::default(), reqwest::Client::new(), None, &AzureOpenAIResponsesOptions::default());
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (events, msg) = rt.block_on(s.collect());
        assert!(matches!(&events[0], AssistantMessageEvent::Error { .. }));
        let err = msg.error_message().unwrap_or("").to_string();
        assert!(err.contains("No API key for provider: azure-openai-responses"), "{err}");
    }
}
