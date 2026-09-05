#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use pi_ai::api::anthropic_messages::convert_messages as convert_anthropic_messages;
use pi_ai::api::bedrock_converse::{build_command_input, convert_messages as convert_bedrock};
use pi_ai::api::google_shared::convert_messages as convert_google;
use pi_ai::api::openai_completions::{convert_messages as convert_openai, OpenAiCompletionsCompat};
use pi_ai::api::transform_messages::transform_messages;
use pi_ai::{
    AssistantMessage, ContentBlock, Context, Message, Model, ModelInput, ToolResultMessage,
    UserContent,
};
use serde_json::Value;

const SUPPORTED_MIME: [(&str, &str); 4] = [
    ("image/png", "png"),
    ("image/jpeg", "jpeg"),
    ("image/gif", "gif"),
    ("image/webp", "webp"),
];

fn model(api: &str, provider: &str, vision: bool) -> Model {
    let mut model = Model::new("image-model", "Image Model", api, provider);
    model.input = if vision {
        vec![ModelInput::Text, ModelInput::Image]
    } else {
        vec![ModelInput::Text]
    };
    model
}

fn user_image_context(mime_type: &str, data: &str) -> Context {
    Context {
        messages: vec![Message::User(UserContent::blocks(
            vec![
                ContentBlock::text("before"),
                ContentBlock::image(data, mime_type),
                ContentBlock::text("after"),
            ],
            1,
        ))],
        ..Default::default()
    }
}

#[test]
fn supported_user_image_mimes_preserve_payload_and_provider_wire_shape() {
    for (index, (mime_type, bedrock_format)) in SUPPORTED_MIME.iter().enumerate() {
        let data = format!("BASE64-{index}");
        let context = user_image_context(mime_type, &data);

        let openai_model = model("openai-completions", "openai", true);
        let openai = convert_openai(
            &openai_model,
            &context,
            &OpenAiCompletionsCompat::get(&openai_model),
        )
        .expect("OpenAI image conversion");
        assert_eq!(openai[0]["content"][0]["text"], "before");
        assert_eq!(
            openai[0]["content"][1]["image_url"]["url"],
            format!("data:{mime_type};base64,{data}")
        );
        assert_eq!(openai[0]["content"][2]["text"], "after");

        let google = convert_google(&model("google-generative-ai", "google", true), &context);
        assert_eq!(google[0]["parts"][1]["inlineData"]["mimeType"], *mime_type);
        assert_eq!(google[0]["parts"][1]["inlineData"]["data"], data);

        let bedrock_model = model("bedrock-converse-stream", "amazon-bedrock", true);
        let bedrock = build_command_input(&bedrock_model, &context, &Default::default())
            .expect("Bedrock image conversion");
        assert_eq!(
            bedrock["messages"][0]["content"][1]["image"]["format"],
            *bedrock_format
        );
        assert_eq!(
            bedrock["messages"][0]["content"][1]["image"]["source"]["bytes"],
            data
        );

        let anthropic = convert_anthropic_messages(&context.messages, false);
        assert_eq!(
            anthropic[0]["content"][1]["source"]["media_type"],
            *mime_type
        );
        assert_eq!(anthropic[0]["content"][1]["source"]["data"], data);
    }
}

#[test]
fn text_only_models_replace_user_and_tool_images_without_leaking_image_blocks() {
    let messages = vec![
        Message::User(UserContent::blocks(
            vec![
                ContentBlock::image("ONE", "image/png"),
                ContentBlock::image("TWO", "image/jpeg"),
                ContentBlock::text("middle"),
                ContentBlock::image("THREE", "image/webp"),
            ],
            1,
        )),
        Message::ToolResult(ToolResultMessage::new(
            "call-1",
            "capture",
            vec![ContentBlock::image("TOOL", "image/png")],
            false,
        )),
    ];
    let text_model = model("openai-completions", "openai", false);
    let transformed = transform_messages::<fn(&str, &Model, &AssistantMessage) -> String>(
        &messages,
        &text_model,
        None,
    );

    let encoded = serde_json::to_value(&transformed).expect("transformed messages serialize");
    let encoded = encoded.to_string();
    assert!(!encoded.contains("ONE"));
    assert!(!encoded.contains("TWO"));
    assert!(!encoded.contains("THREE"));
    assert!(!encoded.contains("TOOL"));
    assert_eq!(
        encoded
            .matches("(image omitted: model does not support images)")
            .count(),
        2,
        "adjacent images collapse to one placeholder, separated images do not"
    );
    assert_eq!(
        encoded
            .matches("(tool image omitted: model does not support images)")
            .count(),
        1
    );
}

#[test]
fn bedrock_rejects_unsupported_mime_for_vision_but_text_only_downgrades_it() {
    let context = user_image_context("image/svg+xml", "SVG");
    let vision = model("bedrock-converse-stream", "amazon-bedrock", true);
    assert_eq!(
        build_command_input(&vision, &context, &Default::default())
            .expect_err("unsupported MIME must fail"),
        "Unknown image type: image/svg+xml"
    );

    let text_only = model("bedrock-converse-stream", "amazon-bedrock", false);
    let payload = build_command_input(&text_only, &context, &Default::default())
        .expect("text-only capability downgrade should precede MIME validation");
    let serialized = payload.to_string();
    assert!(serialized.contains("image omitted: model does not support images"));
    assert!(!serialized.contains("SVG"));
    assert!(!serialized.contains("image/svg+xml"));
}

#[test]
fn tool_result_images_keep_pairing_text_and_image_order_for_vision_models() {
    let context = Context {
        messages: vec![Message::ToolResult(ToolResultMessage::new(
            "call-7",
            "capture",
            vec![
                ContentBlock::text("caption"),
                ContentBlock::image("PNGDATA", "image/png"),
            ],
            false,
        ))],
        ..Default::default()
    };

    let anthropic = convert_anthropic_messages(&context.messages, false);
    let anthropic_content = &anthropic[0]["content"][0]["content"];
    assert_eq!(anthropic[0]["content"][0]["tool_use_id"], "call-7");
    assert_eq!(anthropic_content[0]["type"], "text");
    assert_eq!(anthropic_content[1]["type"], "image");

    let bedrock_model = model("bedrock-converse-stream", "amazon-bedrock", true);
    let bedrock = convert_bedrock(&context, &bedrock_model, "none", None);
    let result = &bedrock[0]["content"][0]["toolResult"];
    assert_eq!(result["toolUseId"], "call-7");
    assert_eq!(result["content"][0]["text"], "caption");
    assert_eq!(result["content"][1]["image"]["format"], "png");

    let google = convert_google(&model("google-generative-ai", "google", true), &context);
    let google_json = Value::Array(google).to_string();
    assert!(google_json.contains("caption"));
    assert!(google_json.contains("PNGDATA"));
}
