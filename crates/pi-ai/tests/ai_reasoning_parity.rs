#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use pi_ai::api::transform_messages::transform_messages;
use pi_ai::model_catalog::get_all_builtin_models;
use pi_ai::{
    clamp_thinking_level, get_supported_thinking_levels, AssistantMessage, ContentBlock, Message,
    Model, ModelThinkingLevel,
};

const LEVELS: [ModelThinkingLevel; 7] = [
    ModelThinkingLevel::Off,
    ModelThinkingLevel::Minimal,
    ModelThinkingLevel::Low,
    ModelThinkingLevel::Medium,
    ModelThinkingLevel::High,
    ModelThinkingLevel::Xhigh,
    ModelThinkingLevel::Max,
];

fn target() -> Model {
    let mut model = Model::new("target", "Target", "google-generative-ai", "google");
    model.reasoning = true;
    model
}

fn assistant(model: &Model, content: Vec<ContentBlock>) -> AssistantMessage {
    let mut message = AssistantMessage::new().with_timestamp(7);
    message.set_api_provider_model(&model.api, &model.provider, &model.id);
    message.set_content(content);
    message
}

#[test]
fn every_builtin_model_clamps_only_to_its_declared_supported_levels() {
    let models = get_all_builtin_models();
    assert!(!models.is_empty());

    for model in models {
        let supported = get_supported_thinking_levels(model);
        assert!(!supported.is_empty(), "{}/{}", model.provider, model.id);
        if !model.reasoning {
            assert_eq!(supported, vec![ModelThinkingLevel::Off]);
        }

        for requested in LEVELS {
            let clamped = clamp_thinking_level(model, requested);
            assert!(
                supported.contains(&clamped),
                "{}/{} mapped {requested:?} to unsupported {clamped:?}; supported={supported:?}",
                model.provider,
                model.id
            );
        }

        for extended in [ModelThinkingLevel::Xhigh, ModelThinkingLevel::Max] {
            let explicitly_enabled = model.reasoning
                && model
                    .thinking_level_map
                    .as_ref()
                    .and_then(|mapping| mapping.get(&extended))
                    .is_some_and(Option::is_some);
            assert_eq!(
                supported.contains(&extended),
                explicitly_enabled,
                "{}/{} {extended:?}",
                model.provider,
                model.id
            );
        }
    }
}

#[test]
fn same_model_replays_signed_empty_reasoning_and_cross_model_strips_it() {
    let model = target();
    let signed_empty = ContentBlock::Thinking {
        thinking: String::new(),
        thinking_signature: Some("provider-signature".into()),
        redacted: None,
    };
    let signed_tool = ContentBlock::ToolCall {
        id: "call-1".into(),
        name: "read".into(),
        arguments: serde_json::json!({}),
        thought_signature: Some("tool-signature".into()),
        namespace: Some("provider.namespace".into()),
    };

    let same = transform_messages(
        &[Message::Assistant(assistant(
            &model,
            vec![signed_empty.clone(), signed_tool.clone()],
        ))],
        &model,
        None::<&fn(&str, &Model, &AssistantMessage) -> String>,
    );
    let Message::Assistant(same) = &same[0] else {
        panic!("assistant");
    };
    assert_eq!(same.content()[0], signed_empty);
    assert_eq!(same.content()[1], signed_tool);

    let mut foreign_model = model.clone();
    foreign_model.id = "foreign".into();
    let foreign = transform_messages(
        &[Message::Assistant(assistant(
            &foreign_model,
            vec![signed_empty, signed_tool],
        ))],
        &model,
        None::<&fn(&str, &Model, &AssistantMessage) -> String>,
    );
    let Message::Assistant(foreign) = &foreign[0] else {
        panic!("assistant");
    };
    assert_eq!(foreign.content().len(), 1);
    assert!(matches!(
        &foreign.content()[0],
        ContentBlock::ToolCall {
            thought_signature: None,
            namespace: Some(namespace),
            ..
        } if namespace == "provider.namespace"
    ));
}

#[test]
fn empty_or_redacted_reasoning_obeys_signature_and_model_boundaries() {
    let model = target();
    let empty_signature = ContentBlock::Thinking {
        thinking: String::new(),
        thinking_signature: Some(String::new()),
        redacted: None,
    };
    let redacted = ContentBlock::Thinking {
        thinking: String::new(),
        thinking_signature: Some("ciphertext".into()),
        redacted: Some(true),
    };

    let same = transform_messages(
        &[Message::Assistant(assistant(
            &model,
            vec![empty_signature.clone(), redacted.clone()],
        ))],
        &model,
        None::<&fn(&str, &Model, &AssistantMessage) -> String>,
    );
    let Message::Assistant(same) = &same[0] else {
        panic!("assistant");
    };
    assert_eq!(same.content(), std::slice::from_ref(&redacted));

    let mut foreign_model = model.clone();
    foreign_model.provider = "other".into();
    let foreign = transform_messages(
        &[Message::Assistant(assistant(
            &foreign_model,
            vec![empty_signature, redacted],
        ))],
        &model,
        None::<&fn(&str, &Model, &AssistantMessage) -> String>,
    );
    let Message::Assistant(foreign) = &foreign[0] else {
        panic!("assistant");
    };
    assert!(foreign.content().is_empty());
}
