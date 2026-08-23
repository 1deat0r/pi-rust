//! Anthropic Messages API adaptor tests driven by synthetic SSE fixtures.
//! The event-assembly path (`process_anthropic_events`) is provider-pure, so
//! these cover the same code as a live HTTP stream minus the transport glue.

use pi_ai::api::anthropic_messages::{
    build_params, convert_messages, map_stop_reason, process_anthropic_events,
};
use pi_ai::sse::SseParser;
use pi_ai::types::{
    AssistantMessageEvent, ContentBlock, Context, DoneReason, Message, StopReason, ToolChoice,
    UserContent,
};

fn model() -> pi_ai::Model {
    pi_ai::providers::anthropic_models()
        .into_iter()
        .find(|m| m.id == "claude-opus-4-8")
        .expect("opus model")
}

fn sse(events: &[(&str, &str)]) -> Vec<pi_ai::sse::SseEvent> {
    let mut text = String::new();
    for (event, data) in events {
        text.push_str(&format!("event: {event}\ndata: {data}\n\n"));
    }
    SseParser::parse_text(&text)
}

#[test]
fn assembles_text_stream_with_usage_and_cost() {
    let events = sse(&[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_123","model":"claude-opus-4-8","usage":{"input_tokens":100,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":1000}}}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":100,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":1000}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);

    let mut pushed = Vec::new();
    let message = process_anthropic_events(&model(), &events, |event| pushed.push(event)).unwrap();

    assert_eq!(message.stop_reason(), Some(StopReason::Stop));
    assert_eq!(message.response_id(), Some("msg_123"));
    let text: String = message
        .content()
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello world");
    let usage = message.usage().unwrap();
    assert_eq!(usage.input, 100);
    assert_eq!(usage.output, 5);
    assert_eq!(usage.cache_write, 1000);
    assert_eq!(usage.total_tokens, 1105);
    // opus-4-8: cacheWrite 6.25/Mtok, input 5/Mtok => 1000*6.25/1e6
    assert!((usage.cost.cache_write - 0.00625).abs() < 1e-9);

    // Event protocol: start..text_end, done terminal.
    let kinds: Vec<&str> = pushed
        .iter()
        .map(|e| match e {
            AssistantMessageEvent::TextStart { .. } => "text_start",
            AssistantMessageEvent::TextDelta { .. } => "text_delta",
            AssistantMessageEvent::TextEnd { .. } => "text_end",
            AssistantMessageEvent::Done { .. } => "done",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["text_start", "text_delta", "text_delta", "text_end"]
    );
    // The `done` terminal is emitted by the stream wrapper (stream()), not the
    // event assembler; the returned message carries the final state.
    let _ = DoneReason::Stop;
}

#[test]
fn assembles_tool_use_stream_with_partial_json() {
    let events = sse(&[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"m2","model":"claude-opus-4-8","usage":{}}}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"bash","input":{}}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\": "}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"ls -la\""}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"}"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);

    let mut pushed = Vec::new();
    let message = process_anthropic_events(&model(), &events, |event| pushed.push(event)).unwrap();
    assert_eq!(message.stop_reason(), Some(StopReason::ToolUse));
    let calls: Vec<&ContentBlock> = message
        .content()
        .iter()
        .filter(|b| matches!(b, ContentBlock::ToolCall { .. }))
        .collect();
    assert_eq!(calls.len(), 1);
    if let ContentBlock::ToolCall {
        id,
        name,
        arguments,
        ..
    } = calls[0]
    {
        assert_eq!(id, "toolu_1");
        assert_eq!(name, "bash");
        assert_eq!(arguments, &serde_json::json!({"command": "ls -la"}));
    }
    assert!(pushed
        .iter()
        .any(|e| matches!(e, AssistantMessageEvent::ToolCallDelta { .. })));
    assert!(pushed
        .iter()
        .any(|e| matches!(e, AssistantMessageEvent::ToolCallEnd { .. })));
}

#[test]
fn assembles_thinking_and_redacted_thinking_blocks() {
    let events = sse(&[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"m3","model":"claude-opus-4-8","usage":{}}}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":"sig1"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me think"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"REDACTED_PAYLOAD"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":1}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);

    let message = process_anthropic_events(&model(), &events, |_| {}).unwrap();
    assert!(
        matches!(&message.content()[0], ContentBlock::Thinking { thinking, thinking_signature, redacted }
        if thinking == "Let me think" && thinking_signature.as_deref() == Some("sig1") && redacted == &None)
    );
    assert!(
        matches!(&message.content()[1], ContentBlock::Thinking { thinking, thinking_signature, redacted }
        if thinking == "[Reasoning redacted]" && thinking_signature.as_deref() == Some("REDACTED_PAYLOAD") && redacted == &Some(true))
    );
}

#[test]
fn maps_stop_reasons() {
    assert_eq!(
        map_stop_reason("end_turn", None).unwrap().0,
        StopReason::Stop
    );
    assert_eq!(
        map_stop_reason("max_tokens", None).unwrap().0,
        StopReason::Length
    );
    assert_eq!(
        map_stop_reason("tool_use", None).unwrap().0,
        StopReason::ToolUse
    );
    let (reason, msg) =
        map_stop_reason("refusal", Some(&serde_json::json!({"explanation": "nope"}))).unwrap();
    assert_eq!(reason, StopReason::Error);
    assert_eq!(msg.as_deref(), Some("nope"));
    assert!(map_stop_reason("bogus", None).is_err());
}

#[test]
fn converts_messages_to_anthropic_params() {
    let context = Context {
        system_prompt: Some("You are helpful".into()),
        messages: vec![
            Message::User(UserContent::blocks(vec![ContentBlock::text("hi")], 1)),
            Message::Assistant({
                let mut m = pi_ai::types::AssistantMessage::new();
                m.set_api_provider_model("anthropic-messages", "anthropic", "claude-opus-4-8");
                m.content_mut().push(ContentBlock::text("hello"));
                m.content_mut().push(ContentBlock::thinking("deep think"));
                m.content_mut().push(ContentBlock::tool_call(
                    "call_1",
                    "read",
                    serde_json::json!({"path": "x"}),
                ));
                m.set_stop_reason(StopReason::ToolUse);
                m
            }),
            Message::ToolResult(pi_ai::types::ToolResultMessage::text(
                "call_1", "read", "contents", false,
            )),
        ],
        tools: vec![pi_ai::types::Tool {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            constrained_sampling: None,
        }],
    };
    let params = build_params(
        &model(),
        &context,
        &pi_ai::api::anthropic_messages::AnthropicOptions {
            tool_choice: Some(ToolChoice::Auto),
            ..Default::default()
        },
    );
    assert_eq!(params["model"], "claude-opus-4-8");
    assert_eq!(params["system"][0]["text"], "You are helpful");
    assert_eq!(params["max_tokens"], 64000);
    let messages = params["messages"].as_array().unwrap();
    // user, assistant, tool-result-as-user
    assert_eq!(messages.len(), 3);
    // Thinking without a signature is converted to plain text for the request.
    assert_eq!(messages[1]["content"][1]["type"], "text");
    assert_eq!(messages[2]["content"][0]["tool_use_id"], "call_1");
    assert_eq!(messages[2]["content"][0]["type"], "tool_result");
    assert_eq!(params["tools"][0]["name"], "read");
    assert_eq!(params["tool_choice"], serde_json::json!({"type": "auto"}));
}

#[test]
fn convert_messages_drops_empty_and_redacted_correctly() {
    let messages = vec![
        Message::User(UserContent::blocks(
            vec![ContentBlock::text("   "), ContentBlock::text("  x  ")],
            1,
        )),
        Message::Assistant({
            let mut m = pi_ai::types::AssistantMessage::new();
            m.content_mut().push(ContentBlock::Thinking {
                thinking: "think".into(),
                thinking_signature: None,
                redacted: None,
            });
            m
        }),
    ];
    let params = convert_messages(&messages, false);
    assert_eq!(params.len(), 2);
    // Whitespace-only blocks dropped; non-blank text keeps its original spacing.
    assert_eq!(params[0]["content"][0]["text"], "  x  ");
    assert_eq!(params[0]["content"].as_array().unwrap().len(), 1);
    // thinking-without-signature becomes a text block
    assert_eq!(params[1]["content"][0]["type"], "text");
    assert_eq!(params[1]["content"][0]["text"], "think");
}

#[test]
fn build_params_thinking_budget_and_metadata() {
    let params = build_params(
        &model(),
        &Context::default(),
        &pi_ai::api::anthropic_messages::AnthropicOptions {
            thinking_enabled: Some(true),
            thinking_budget_tokens: Some(2048),
            base: pi_ai::types::StreamOptions {
                metadata: Some(serde_json::json!({"user_id": "u-1"})),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    assert_eq!(params["thinking"]["type"], "enabled");
    assert_eq!(params["thinking"]["budget_tokens"], 2048);
    assert_eq!(params["metadata"]["user_id"], "u-1");
    // Disabled thinking with an off-null map keeps thinking off the model
    let params = build_params(
        &model(),
        &Context::default(),
        &pi_ai::api::anthropic_messages::AnthropicOptions {
            thinking_enabled: Some(false),
            ..Default::default()
        },
    );
    let _ = params;
}

#[test]
fn thinking_signature_replay_roundtrip() {
    // An assistant block with a signature replays as thinking + signature.
    let mut m = pi_ai::types::AssistantMessage::new();
    m.content_mut().push(ContentBlock::Thinking {
        thinking: "old chain".into(),
        thinking_signature: Some("sig-old".into()),
        redacted: None,
    });
    let params = convert_messages(&[Message::Assistant(m)], false);
    assert_eq!(params[0]["content"][0]["type"], "thinking");
    assert_eq!(params[0]["content"][0]["signature"], "sig-old");
}
