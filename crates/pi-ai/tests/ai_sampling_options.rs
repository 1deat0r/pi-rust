#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;

use pi_ai::api::anthropic_messages::{self, AnthropicOptions};
use pi_ai::api::bedrock_converse::{self, BedrockOptions};
use pi_ai::api::constrained_sampling::{
    append_grammar_tool_input_json_delta, get_json_schema_tool_parameters,
    resolve_grammar_constrained_sampling, resolve_json_schema_strict_sampling,
    GrammarToolInputJsonBuffer,
};
use pi_ai::api::google_generative_ai::{self, GoogleOptions};
use pi_ai::api::openai_completions::{self, OpenAiCompletionsCompat};
use pi_ai::api::openai_responses::{self, OpenAIResponsesOptions};
use pi_ai::model::Model;
use pi_ai::types::{
    ConstrainedSampling, Context, StreamOptions, StrictPreference, Tool, ToolChoice,
};
use serde_json::json;

fn model(api: &str, provider: &str) -> Model {
    let mut model = Model::new("sampling-model", "Sampling Model", api, provider);
    model.base_url = format!("https://{provider}.example/v1");
    model.max_tokens = 16_384;
    model
}

fn tool(constrained_sampling: Option<ConstrainedSampling>) -> Tool {
    Tool {
        name: "lookup".to_string(),
        description: "Look up a value".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "search query"
                },
                "nested": {
                    "type": "object",
                    "properties": {"limit": {"type": "integer"}}
                }
            },
            "required": ["query"]
        }),
        constrained_sampling,
    }
}

#[test]
fn openai_completions_sampling_params_apply_last_and_preserve_custom_keys() {
    let model = model("openai-completions", "openai");
    let compat = OpenAiCompletionsCompat::get(&model);
    let options = StreamOptions {
        temperature: Some(0.25),
        max_tokens: Some(512),
        sampling_params: Some(json!({
            "temperature": 0.75,
            "max_completion_tokens": 777,
            "top_p": 0.8,
            "stop": ["END"],
            "tool_choice": "required",
            "parallel_tool_calls": false,
            "provider_specific": {"mode": "exact"}
        })),
        ..Default::default()
    };

    let payload = openai_completions::build_params(
        &model,
        &Context::default(),
        Some(&options),
        &compat,
        "short",
    )
    .expect("OpenAI payload should build");

    assert_eq!(payload["temperature"], 0.75);
    assert_eq!(payload["max_completion_tokens"], 777);
    assert_eq!(payload["top_p"], 0.8);
    assert_eq!(payload["stop"], json!(["END"]));
    assert_eq!(payload["tool_choice"], "required");
    assert_eq!(payload["parallel_tool_calls"], false);
    assert_eq!(payload["provider_specific"], json!({"mode": "exact"}));
}

#[test]
fn responses_sampling_params_override_named_fields_last() {
    let model = model("openai-responses", "openai");
    let options = OpenAIResponsesOptions {
        base: StreamOptions {
            temperature: Some(0.2),
            max_tokens: Some(5),
            sampling_params: Some(json!({
                "temperature": 0.6,
                "max_output_tokens": 9,
                "top_p": 0.7,
                "parallel_tool_calls": true,
                "custom_response_option": "kept"
            })),
            ..Default::default()
        },
        tool_choice: Some(json!("required")),
        ..Default::default()
    };

    let payload = openai_responses::build_params(&model, &Context::default(), &options)
        .expect("Responses payload should build");

    assert_eq!(payload["temperature"], 0.6);
    assert_eq!(payload["max_output_tokens"], 9);
    assert_eq!(payload["top_p"], 0.7);
    assert_eq!(payload["tool_choice"], "required");
    assert_eq!(payload["parallel_tool_calls"], true);
    assert_eq!(payload["custom_response_option"], "kept");
}

#[test]
fn named_sampling_and_tool_options_keep_provider_wire_shapes() {
    let context = Context {
        tools: vec![tool(None)],
        ..Default::default()
    };

    let anthropic = anthropic_messages::build_params(
        &model("anthropic-messages", "anthropic"),
        &context,
        &AnthropicOptions {
            max_tokens: Some(90),
            temperature: Some(0.2),
            tool_choice: Some(ToolChoice::Auto),
            ..Default::default()
        },
    )
    .expect("Anthropic payload should build");
    assert_eq!(anthropic["max_tokens"], 90);
    assert_eq!(anthropic["temperature"], 0.2);
    assert_eq!(anthropic["tool_choice"], json!({"type": "auto"}));

    let google = google_generative_ai::build_params(
        &model("google-generative-ai", "google"),
        &context,
        &GoogleOptions {
            base: StreamOptions {
                temperature: Some(0.3),
                max_tokens: Some(91),
                ..Default::default()
            },
            tool_choice: Some("auto".to_string()),
            thinking: None,
        },
    )
    .expect("Google payload should build");
    assert_eq!(google["generationConfig"]["temperature"], 0.3);
    assert_eq!(google["generationConfig"]["maxOutputTokens"], 91);
    assert_eq!(
        google["toolConfig"]["functionCallingConfig"]["mode"],
        "AUTO"
    );

    let bedrock = bedrock_converse::build_command_input(
        &model("bedrock-converse-stream", "amazon-bedrock"),
        &context,
        &BedrockOptions {
            base: StreamOptions {
                temperature: Some(0.4),
                max_tokens: Some(92),
                ..Default::default()
            },
            max_tokens: Some(93),
            tool_choice: Some(json!("any")),
            ..Default::default()
        },
    )
    .expect("Bedrock payload should build");
    assert_eq!(bedrock["inferenceConfig"]["temperature"], 0.4);
    assert_eq!(bedrock["inferenceConfig"]["maxTokens"], 93);
    assert_eq!(bedrock["toolConfig"]["toolChoice"], json!({"any": {}}));
}

#[test]
fn constrained_sampling_covers_strict_fallback_grammar_and_monotonic_deltas() {
    let strict = tool(Some(ConstrainedSampling::JsonSchema {
        strict: StrictPreference::Require,
    }));
    assert_eq!(
        resolve_json_schema_strict_sampling(&strict, true).expect("strict schema should resolve"),
        Some(true)
    );
    let normalized =
        get_json_schema_tool_parameters(&strict, Some(true)).expect("schema should normalize");
    assert_eq!(normalized["additionalProperties"], false);
    assert_eq!(
        normalized["properties"]["nested"]["anyOf"][0]["additionalProperties"],
        false
    );
    assert_eq!(
        resolve_json_schema_strict_sampling(&strict, false)
            .expect_err("required strict mode must fail when unsupported"),
        "Tool \"lookup\" requires JSON-schema constrained sampling, but strict tools are unsupported."
    );

    let prefer = tool(Some(ConstrainedSampling::JsonSchema {
        strict: StrictPreference::Prefer,
    }));
    assert_eq!(
        resolve_json_schema_strict_sampling(&prefer, false)
            .expect("preferred strict mode should fall back"),
        None
    );

    let mut variants = BTreeMap::new();
    variants.insert("openai_regex".to_string(), "[a-z]+".to_string());
    variants.insert("openai_lark".to_string(), "start: WORD".to_string());
    let grammar = tool(Some(ConstrainedSampling::Grammar { variants }));
    let resolved = resolve_grammar_constrained_sampling(&grammar, true)
        .expect("grammar should resolve")
        .expect("grammar should be enabled");
    assert_eq!(resolved.format, "lark");
    assert_eq!(resolved.definition, "start: WORD");
    assert_eq!(resolved.input_property, "query");
    assert_eq!(
        resolve_grammar_constrained_sampling(&grammar, false)
            .expect("unsupported grammar should be omitted"),
        None
    );

    let mut buffer = GrammarToolInputJsonBuffer::default();
    assert_eq!(
        append_grammar_tool_input_json_delta(&mut buffer, "query", "ab", false)
            .expect("first delta"),
        Some("{\"query\":\"ab".to_string())
    );
    assert_eq!(
        append_grammar_tool_input_json_delta(&mut buffer, "query", "abc", true)
            .expect("closing delta"),
        Some("c\"}".to_string())
    );
    assert!(append_grammar_tool_input_json_delta(&mut buffer, "query", "ax", false).is_err());
}
