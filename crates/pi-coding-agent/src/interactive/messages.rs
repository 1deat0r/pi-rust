//! Message rendering for the interactive transcript.
//!
//! The upstream interactive mode uses separate TUI components for assistant
//! text/thinking, tool calls/results, images, and custom messages. The Rust
//! interactive mode stores the same scene as one markdown source string, so
//! this module is the component boundary: it preserves markdown source for
//! ordinary text while materializing every supported content block into the
//! transcript stream. Terminal image sequences are kept as their own lines
//! so pi-tui can place, crop, and account for them during rendering.

use pi_agent::rich_agent::RichAgentEvent;
use pi_agent::types::{AgentMessage, CustomAgentMessage, CustomContent};
use pi_ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Message, StopReason, ToolResultMessage,
    UserContentBody,
};
use pi_tui::terminal_image::{
    get_capabilities, get_image_dimensions, image_fallback, render_image, ImageDimensions,
    ImageProtocol, ImageRenderOptions,
};
use serde::Serialize;
use serde_json::Value;

use crate::core::settings::SettingsManager;
use crate::interactive::tui_theme;

const DEFAULT_IMAGE_WIDTH_CELLS: usize = 60;
const DEFAULT_TOOL_PREVIEW_LINES: usize = 10;
const DEFAULT_BASH_PREVIEW_LINES: usize = 20;
const BASH_RESULT_PREVIEW_LINES: usize = 5;
const READ_RESULT_PREVIEW_LINES: usize = 10;
const GREP_RESULT_PREVIEW_LINES: usize = 15;
const FIND_RESULT_PREVIEW_LINES: usize = 20;
const LS_RESULT_PREVIEW_LINES: usize = 20;
const WRITE_CALL_PREVIEW_LINES: usize = 10;
const MAX_GENERIC_ARGUMENTS: usize = 6;
const MAX_GENERIC_ARGUMENT_CHARS: usize = 240;
const MAX_GENERIC_CALL_CHARS: usize = 900;
const TOOL_RUNNING_MARKER: &str = "⏳";

/// Rendering controls shared by persisted transcript and live-event output.
///
/// The legacy helpers below retain their public signatures and default to
/// showing complete tool output. Callers that own interactive settings can
/// opt into the upstream collapsed-tool behavior and image visibility without
/// changing the message model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptRenderOptions {
    pub hide_thinking: bool,
    pub show_images: bool,
    pub image_width_cells: usize,
    /// Horizontal message padding from Pi's `outputPad` setting. The
    /// interactive view applies this to user/assistant/thinking components;
    /// keeping it in the render options lets live and persisted projections
    /// carry the same setting without changing the message model.
    pub output_pad: usize,
    pub expand_tool_output: bool,
}

impl Default for TranscriptRenderOptions {
    fn default() -> Self {
        Self {
            hide_thinking: false,
            show_images: true,
            image_width_cells: DEFAULT_IMAGE_WIDTH_CELLS,
            output_pad: 1,
            // Keep the existing Rust transcript contract lossless. The
            // interactive component can request the upstream preview mode.
            expand_tool_output: true,
        }
    }
}

/// Render one agent message into transcript markdown/source.
pub fn render_message(message: &AgentMessage, hide_thinking: bool) -> Option<(String, String)> {
    render_message_with_options(
        message,
        TranscriptRenderOptions {
            hide_thinking,
            ..Default::default()
        },
    )
}

/// Render one agent message with explicit image and tool-output controls.
pub fn render_message_with_options(
    message: &AgentMessage,
    options: TranscriptRenderOptions,
) -> Option<(String, String)> {
    match message {
        AgentMessage::Core(Message::User(user)) => {
            let text = render_user_content(user.content(), options);
            if text.trim().is_empty() {
                return None;
            }
            Some(("user".to_string(), text))
        }
        AgentMessage::Core(Message::Assistant(assistant)) => {
            render_assistant_message(assistant, options)
        }
        AgentMessage::Core(Message::ToolResult(result)) => {
            Some(("tool".to_string(), render_tool_result(result, options)))
        }
        AgentMessage::Custom(custom) => render_custom_message(custom, options),
    }
}

/// Render an assistant stream event using its current partial/final message.
///
/// The upstream TUI updates one assistant component for every event and adds
/// tool-execution components as tool-call blocks appear. Returning the
/// rendered partial here gives the Rust mode the same complete event view to
/// use when it owns a live transcript buffer, including thinking and partial
/// tool-call arguments.
pub fn render_assistant_event(
    event: &AssistantMessageEvent,
    hide_thinking: bool,
) -> Option<String> {
    render_assistant_event_with_options(
        event,
        TranscriptRenderOptions {
            hide_thinking,
            ..Default::default()
        },
    )
}

/// Render an assistant stream event with explicit display controls.
pub fn render_assistant_event_with_options(
    event: &AssistantMessageEvent,
    options: TranscriptRenderOptions,
) -> Option<String> {
    let message = match event {
        AssistantMessageEvent::Start { partial }
        | AssistantMessageEvent::TextStart { partial, .. }
        | AssistantMessageEvent::TextDelta { partial, .. }
        | AssistantMessageEvent::TextEnd { partial, .. }
        | AssistantMessageEvent::ThinkingStart { partial, .. }
        | AssistantMessageEvent::ThinkingDelta { partial, .. }
        | AssistantMessageEvent::ThinkingEnd { partial, .. }
        | AssistantMessageEvent::ToolCallStart { partial, .. }
        | AssistantMessageEvent::ToolCallDelta { partial, .. }
        | AssistantMessageEvent::ToolCallEnd { partial, .. } => partial,
        AssistantMessageEvent::Done { message, .. } => message,
        AssistantMessageEvent::Error {
            reason,
            error_message,
        } => {
            // Error events carry the authoritative lifecycle reason. Most
            // providers also set stopReason on the message, but direct stream
            // rendering must remain correct when they only set errorMessage.
            if error_message.stop_reason().is_some() {
                return render_assistant_message(error_message, options).map(|(_, text)| text);
            }
            let mut message = error_message.clone();
            message.set_stop_reason(match reason {
                pi_ai::types::ErrorReason::Aborted => StopReason::Aborted,
                pi_ai::types::ErrorReason::Error => StopReason::Error,
            });
            return render_assistant_message(&message, options).map(|(_, text)| text);
        }
    };

    render_assistant_message(message, options).map(|(_, text)| text)
}

/// Render the assistant portion of an interactive turn without tool-call
/// blocks. Pi owns tool calls in separate ToolExecutionComponent instances;
/// keeping them out of the assistant markdown prevents a live bash/read call
/// from being rendered twice while the tool lifecycle is still active.
pub fn render_assistant_event_without_tool_calls_with_options(
    event: &AssistantMessageEvent,
    options: TranscriptRenderOptions,
) -> Option<String> {
    let mut message = match event {
        AssistantMessageEvent::Start { partial }
        | AssistantMessageEvent::TextStart { partial, .. }
        | AssistantMessageEvent::TextDelta { partial, .. }
        | AssistantMessageEvent::TextEnd { partial, .. }
        | AssistantMessageEvent::ThinkingStart { partial, .. }
        | AssistantMessageEvent::ThinkingDelta { partial, .. }
        | AssistantMessageEvent::ThinkingEnd { partial, .. }
        | AssistantMessageEvent::ToolCallStart { partial, .. }
        | AssistantMessageEvent::ToolCallDelta { partial, .. }
        | AssistantMessageEvent::ToolCallEnd { partial, .. } => partial.clone(),
        AssistantMessageEvent::Done { message, .. } => message.clone(),
        AssistantMessageEvent::Error {
            reason,
            error_message,
        } => {
            let mut message = error_message.clone();
            if message.stop_reason().is_none() {
                message.set_stop_reason(match reason {
                    pi_ai::types::ErrorReason::Aborted => StopReason::Aborted,
                    pi_ai::types::ErrorReason::Error => StopReason::Error,
                });
            }
            message
        }
    };
    // The upstream assistant component decides whether an assistant-level
    // terminal notice belongs to this turn before tool-call children are
    // rendered separately. Preserve that fact across the assistant-only
    // projection; filtering the calls first would otherwise turn a failed
    // tool-bearing turn into an apparent no-tool error.
    let had_tool_calls = message
        .content()
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolCall { .. }));
    message.set_content(
        message
            .content()
            .iter()
            .filter(|block| !matches!(block, ContentBlock::ToolCall { .. }))
            .cloned()
            .collect(),
    );
    render_assistant_message_inner(&message, options, true, had_tool_calls).map(|(_, text)| text)
}

/// Ordered projection of one assistant stream snapshot. Pi keeps the
/// assistant component and each tool component as separate retained children;
/// retaining those boundaries here prevents prose after a tool call from
/// moving above the tool while the provider continues streaming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantTranscriptSegment {
    Markdown(String),
    ToolCall(String),
}

/// Render one assistant stream snapshot into Pi's retained child order.
pub fn render_assistant_event_segments_with_options(
    event: &AssistantMessageEvent,
    options: TranscriptRenderOptions,
) -> Vec<AssistantTranscriptSegment> {
    let message = match event {
        AssistantMessageEvent::Start { partial }
        | AssistantMessageEvent::TextStart { partial, .. }
        | AssistantMessageEvent::TextDelta { partial, .. }
        | AssistantMessageEvent::TextEnd { partial, .. }
        | AssistantMessageEvent::ThinkingStart { partial, .. }
        | AssistantMessageEvent::ThinkingDelta { partial, .. }
        | AssistantMessageEvent::ThinkingEnd { partial, .. }
        | AssistantMessageEvent::ToolCallStart { partial, .. }
        | AssistantMessageEvent::ToolCallDelta { partial, .. }
        | AssistantMessageEvent::ToolCallEnd { partial, .. } => partial.clone(),
        AssistantMessageEvent::Done { message, .. } => message.clone(),
        AssistantMessageEvent::Error {
            reason,
            error_message,
        } => {
            let mut message = error_message.clone();
            if message.stop_reason().is_none() {
                message.set_stop_reason(match reason {
                    pi_ai::types::ErrorReason::Aborted => StopReason::Aborted,
                    pi_ai::types::ErrorReason::Error => StopReason::Error,
                });
            }
            message
        }
    };

    let has_tool_calls = message
        .content()
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolCall { .. }));
    if !has_tool_calls {
        return render_assistant_message(&message, options)
            .map(|(_, text)| vec![AssistantTranscriptSegment::Markdown(text)])
            .unwrap_or_default();
    }

    let mut segments = Vec::new();
    let content = message.content().to_vec();
    let mut index = 0;
    while index < content.len() {
        if let ContentBlock::ToolCall { id, .. } = &content[index] {
            segments.push(AssistantTranscriptSegment::ToolCall(id.clone()));
            index += 1;
            continue;
        }

        let start = index;
        while index < content.len() && !matches!(content[index], ContentBlock::ToolCall { .. }) {
            index += 1;
        }
        let mut run_message = message.clone();
        run_message.set_content(content[start..index].to_vec());
        if let Some((_, text)) =
            render_assistant_message_without_terminal_notice(&run_message, options)
        {
            segments.push(AssistantTranscriptSegment::Markdown(text));
        }
    }
    // Tool calls are retained as separate children in the live transcript, so
    // this path cannot use `render_assistant_message`'s single joined string.
    // Pi still renders the provider's length stop notice after the complete
    // assistant turn, including turns that contain tool calls. Keep it as a
    // final markdown child so it is not lost when the stream is segmented.
    if message.stop_reason() == Some(StopReason::Length) {
        segments.push(AssistantTranscriptSegment::Markdown(
            "Response was truncated before completion.".to_string(),
        ));
    }
    segments
}

fn render_assistant_message_without_terminal_notice(
    assistant: &AssistantMessage,
    options: TranscriptRenderOptions,
) -> Option<(String, String)> {
    render_assistant_message_inner(assistant, options, false, false)
}

fn render_assistant_message_inner(
    assistant: &AssistantMessage,
    options: TranscriptRenderOptions,
    include_terminal_notice: bool,
    has_tool_calls: bool,
) -> Option<(String, String)> {
    let mut parts = Vec::new();
    let content = assistant.content();
    let mut index = 0usize;

    while index < content.len() {
        match &content[index] {
            ContentBlock::Text { text, .. } => {
                if !text.trim().is_empty() {
                    parts.push(text.trim().to_string());
                }
                index += 1;
            }
            ContentBlock::Thinking { .. } => {
                let mut thinking_blocks = Vec::new();
                while let Some(ContentBlock::Thinking { thinking, .. }) = content.get(index) {
                    if !thinking.trim().is_empty() {
                        thinking_blocks.push(thinking.trim().to_string());
                    }
                    index += 1;
                }
                if thinking_blocks.is_empty() {
                    continue;
                }
                if options.hide_thinking {
                    // The upstream component uses a styled Text child for a
                    // collapsed thinking run, not a Markdown blockquote.
                    parts.push(render_hidden_thinking_label());
                } else {
                    parts.push(render_thinking_block(thinking_blocks.join("\n\n")));
                }
            }
            ContentBlock::Image { .. } => {
                parts.push(render_image_block(&content[index], options));
                index += 1;
            }
            ContentBlock::ToolCall { .. } => {
                index += 1;
            }
        }
    }

    let terminal_notice = if include_terminal_notice {
        match assistant.stop_reason() {
            Some(StopReason::Length) => {
                Some("Response was truncated before completion.".to_string())
            }
            Some(StopReason::Aborted) if !has_tool_calls => Some(
                assistant
                    .error_message()
                    .filter(|error| *error != "Request was aborted")
                    .unwrap_or("Operation aborted")
                    .to_string(),
            ),
            Some(StopReason::Error) if !has_tool_calls => Some(
                assistant
                    .error_message()
                    .unwrap_or("Unknown error")
                    .to_string(),
            ),
            _ => None,
        }
    } else {
        None
    };

    if parts.is_empty() && terminal_notice.is_none() {
        return None;
    }

    let mut text = parts.join("\n");
    if let Some(notice) = terminal_notice {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&render_terminal_notice(assistant.stop_reason(), notice));
    }

    if let Some(diagnostics) = assistant.diagnostics().filter(|items| !items.is_empty()) {
        text.push_str("\n\n");
        text.push_str(&render_display_details("Diagnostics", diagnostics));
    }

    Some(("assistant".to_string(), text))
}

/// Render one live tool lifecycle event in the same compact form used by the
/// interactive transcript.
///
/// `RichAgentEvent::ToolExecutionStart` and `ToolExecutionUpdate` return a
/// running component (the call summary plus any partial output). The end
/// event returns the completed or error component. Non-tool events return
/// `None`, allowing the interactive loop to pass its full event stream here
/// without a second dispatch table. Partial results are decoded into content
/// blocks and never displayed as their serialized JSON envelope.
pub fn render_tool_execution_event(
    event: &RichAgentEvent,
    options: TranscriptRenderOptions,
) -> Option<String> {
    match event {
        RichAgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => Some(render_tool_execution_snapshot(
            ToolExecutionSnapshot {
                tool_call_id,
                tool_name,
                arguments: args,
                content: &[],
                details: None,
                is_error: false,
                marker: TOOL_RUNNING_MARKER,
                added_tool_names: &[],
            },
            options,
        )),
        RichAgentEvent::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            args,
            partial_result,
        } => {
            let content = decode_content_blocks(partial_result.get("content"));
            let details = partial_result.get("details");
            let added_tool_names = decode_added_tool_names(
                partial_result
                    .get("addedToolNames")
                    .or_else(|| partial_result.get("added_tool_names")),
            );
            Some(render_tool_execution_snapshot(
                ToolExecutionSnapshot {
                    tool_call_id,
                    tool_name,
                    arguments: args,
                    content: &content,
                    details,
                    is_error: false,
                    marker: TOOL_RUNNING_MARKER,
                    added_tool_names: &added_tool_names,
                },
                options,
            ))
        }
        RichAgentEvent::ToolExecutionEnd {
            tool_call_id: _,
            tool_name,
            result,
            is_error,
        } => Some(render_tool_execution_result(
            tool_name,
            &result.content,
            result.details.as_ref(),
            *is_error,
            &result.added_tool_names,
            options,
        )),
        _ => None,
    }
}

struct ToolExecutionSnapshot<'a> {
    tool_call_id: &'a str,
    tool_name: &'a str,
    arguments: &'a Value,
    content: &'a [ContentBlock],
    details: Option<&'a Value>,
    is_error: bool,
    marker: &'a str,
    added_tool_names: &'a [String],
}

fn render_tool_execution_snapshot(
    snapshot: ToolExecutionSnapshot<'_>,
    options: TranscriptRenderOptions,
) -> String {
    let call = ContentBlock::tool_call(
        snapshot.tool_call_id,
        snapshot.tool_name,
        snapshot.arguments.clone(),
    );
    let mut rendered = render_tool_call_with_marker(&call, options, snapshot.marker);
    append_tool_execution_result(
        &mut rendered,
        snapshot.tool_name,
        snapshot.content,
        snapshot.details,
        snapshot.is_error,
        snapshot.added_tool_names,
        options,
    );
    rendered
}

fn render_tool_execution_result(
    tool_name: &str,
    content: &[ContentBlock],
    details: Option<&Value>,
    is_error: bool,
    added_tool_names: &[String],
    options: TranscriptRenderOptions,
) -> String {
    let marker = if is_error { "✗" } else { "✓" };
    let mut rendered = format!("{marker} **{}**", escape_markdown_text(tool_name));
    append_tool_execution_result(
        &mut rendered,
        tool_name,
        content,
        details,
        is_error,
        added_tool_names,
        options,
    );
    rendered
}

fn append_tool_execution_result(
    rendered: &mut String,
    tool_name: &str,
    content: &[ContentBlock],
    details: Option<&Value>,
    is_error: bool,
    added_tool_names: &[String],
    options: TranscriptRenderOptions,
) {
    let output = render_tool_result_content(tool_name, content, details, is_error, options);
    if !output.is_empty() {
        rendered.push('\n');
        rendered.push_str(&output);
    }
    if let Some(details) = details {
        if let Some(summary) = render_tool_details(tool_name, details, is_error) {
            rendered.push_str("\n\n");
            rendered.push_str(&summary);
        }
    }
    if !added_tool_names.is_empty() {
        rendered.push_str("\n\n**Tools added:** ");
        rendered.push_str(
            &added_tool_names
                .iter()
                .map(|name| escape_markdown_text(name))
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
}

fn decode_content_blocks(value: Option<&Value>) -> Vec<ContentBlock> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|block| serde_json::from_value(block.clone()).ok())
        .collect()
}

fn decode_added_tool_names(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// Build a transcript and append the current assistant stream view.
pub fn build_transcript_with_event(
    messages: &[AgentMessage],
    event: &AssistantMessageEvent,
    options: TranscriptRenderOptions,
) -> String {
    let mut output = build_transcript_with_options(messages, options);
    if let Some(live) = render_assistant_event_with_options(event, options) {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&live);
        output.push('\n');
    }
    output
}

fn render_user_content(content: &UserContentBody, options: TranscriptRenderOptions) -> String {
    match content {
        UserContentBody::String(text) => text.clone(),
        UserContentBody::Blocks(blocks) => render_ordered_blocks(blocks, options),
    }
}

fn render_assistant_message(
    assistant: &AssistantMessage,
    options: TranscriptRenderOptions,
) -> Option<(String, String)> {
    let mut parts = Vec::new();
    let content = assistant.content();
    let mut index = 0usize;

    while index < content.len() {
        match &content[index] {
            ContentBlock::Text { text, .. } => {
                if !text.trim().is_empty() {
                    // AssistantMessageComponent trims each text block before
                    // handing it to Markdown, while preserving internal
                    // newlines and markdown source.
                    parts.push(text.trim().to_string());
                }
                index += 1;
            }
            ContentBlock::Thinking { .. } => {
                let mut thinking_blocks = Vec::new();
                while let Some(ContentBlock::Thinking { thinking, .. }) = content.get(index) {
                    if !thinking.trim().is_empty() {
                        thinking_blocks.push(thinking.trim().to_string());
                    }
                    index += 1;
                }
                if thinking_blocks.is_empty() {
                    continue;
                }
                if options.hide_thinking {
                    // One label per contiguous thinking run, matching the
                    // upstream component's collapsed rendering.
                    parts.push(render_hidden_thinking_label());
                } else {
                    // Upstream renders one Markdown section per contiguous
                    // thinking run, with blank lines between provider blocks.
                    parts.push(render_thinking_block(thinking_blocks.join("\n\n")));
                }
            }
            ContentBlock::Image { .. } => {
                parts.push(render_image_block(&content[index], options));
                index += 1;
            }
            ContentBlock::ToolCall { .. } => {
                parts.push(render_tool_call(&content[index], options));
                index += 1;
            }
        }
    }

    let has_tool_calls = content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolCall { .. }));
    let terminal_notice = match assistant.stop_reason() {
        Some(StopReason::Length) => Some("Response was truncated before completion.".to_string()),
        // Tool execution components own errors for assistant messages that
        // contain tool calls. This prevents the assistant row from printing a
        // second error beside the tool's failed result.
        Some(StopReason::Aborted) if !has_tool_calls => Some(
            assistant
                .error_message()
                .filter(|error| *error != "Request was aborted")
                .unwrap_or("Operation aborted")
                .to_string(),
        ),
        Some(StopReason::Error) if !has_tool_calls => Some(
            assistant
                .error_message()
                .unwrap_or("Unknown error")
                .to_string(),
        ),
        _ => None,
    };

    if parts.is_empty() && terminal_notice.is_none() {
        return None;
    }

    let mut text = parts.join("\n");
    if let Some(notice) = terminal_notice {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&render_terminal_notice(assistant.stop_reason(), notice));
    }

    if let Some(diagnostics) = assistant.diagnostics().filter(|items| !items.is_empty()) {
        text.push_str("\n\n");
        text.push_str(&render_display_details("Diagnostics", diagnostics));
    }

    Some(("assistant".to_string(), text))
}

fn render_thinking_block(thinking: String) -> String {
    // Prefix every physical line so multiline reasoning remains a real
    // markdown blockquote rather than leaking unquoted lines into the chat.
    thinking
        .split('\n')
        .map(|line| {
            if line.is_empty() {
                ">".to_string()
            } else {
                format!(
                    "> {}",
                    tui_theme::italic(tui_theme::fg("thinkingText", line))
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_hidden_thinking_label() -> String {
    tui_theme::italic(tui_theme::fg("thinkingText", "Thinking..."))
}

fn render_terminal_notice(stop_reason: Option<StopReason>, notice: String) -> String {
    let notice = if stop_reason == Some(StopReason::Error) {
        format!("Error: {notice}")
    } else {
        notice
    };
    tui_theme::fg("error", notice)
}

fn render_tool_call(block: &ContentBlock, options: TranscriptRenderOptions) -> String {
    render_tool_call_with_marker(block, options, TOOL_RUNNING_MARKER)
}

fn render_tool_call_with_marker(
    block: &ContentBlock,
    options: TranscriptRenderOptions,
    marker: &str,
) -> String {
    let ContentBlock::ToolCall {
        name,
        arguments,
        namespace,
        ..
    } = block
    else {
        return String::new();
    };

    let display_name = namespace
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|prefix| format!("{prefix}/{name}"))
        .unwrap_or_else(|| name.clone());

    let rendered = match name.as_str() {
        "bash" => render_bash_call(arguments),
        "read" => render_read_call(arguments, options),
        "write" => render_write_call(arguments, options),
        "edit" => render_edit_call(arguments),
        "grep" => render_grep_call(arguments),
        "find" => render_find_call(arguments),
        "ls" => render_ls_call(arguments),
        _ => render_generic_tool_call(&display_name, arguments),
    };
    if marker == TOOL_RUNNING_MARKER {
        rendered
    } else {
        rendered
            .strip_prefix(TOOL_RUNNING_MARKER)
            .map(|rest| format!("{marker}{rest}"))
            .unwrap_or(rendered)
    }
}

fn render_bash_call(arguments: &Value) -> String {
    let command = match string_argument(arguments, "command") {
        Ok(Some(command)) if !command.is_empty() => sanitize_inline_text(&command),
        Ok(_) => "...".to_string(),
        Err(()) => "[invalid arg]".to_string(),
    };
    let mut rendered = format!(
        "{TOOL_RUNNING_MARKER} **$ {}**",
        escape_markdown_text(&command)
    );
    if let Some(timeout) = numeric_argument(arguments, "timeout") {
        if timeout != "0" {
            rendered.push_str(&format!(" (timeout {timeout}s)"));
        }
    } else if argument_is_invalid(arguments, "timeout") {
        rendered.push_str(" (timeout [invalid arg])");
    }
    rendered
}

fn render_read_call(arguments: &Value, options: TranscriptRenderOptions) -> String {
    let raw_path = match first_string_argument(arguments, &["file_path", "path"]) {
        Ok(path) => path,
        Err(()) => return format!("{TOOL_RUNNING_MARKER} **read** [invalid arg]"),
    };
    let path = render_path_value(raw_path.as_deref(), "...");
    let range = render_line_range(arguments);
    let path_text = (!options.expand_tool_output)
        .then(|| raw_path.as_deref().and_then(compact_read_label))
        .flatten()
        .map(|label| {
            let kind = if label.starts_with("[skill] ") {
                "[skill]"
            } else if label.starts_with("read docs ") {
                "read docs"
            } else {
                "read resource"
            };
            let name = label
                .strip_prefix("[skill] ")
                .or_else(|| label.strip_prefix("read docs "))
                .or_else(|| label.strip_prefix("read resource "))
                .unwrap_or(label.as_str());
            format!("**{kind}** {}", inline_value(name))
        })
        .unwrap_or_else(|| format!("**read** {path}"));
    format!("{TOOL_RUNNING_MARKER} {path_text}{range}")
}

fn render_write_call(arguments: &Value, options: TranscriptRenderOptions) -> String {
    let path = match first_string_argument(arguments, &["file_path", "path"]) {
        Ok(raw_path) => render_path_value(raw_path.as_deref(), "..."),
        Err(()) => "[invalid arg]".to_string(),
    };
    let mut rendered = format!("{TOOL_RUNNING_MARKER} **write** {path}");
    match string_argument(arguments, "content") {
        Err(()) => rendered.push_str("\n\n[invalid content arg - expected string]"),
        Ok(Some(content)) if !content.is_empty() => {
            let content = normalize_argument_text(&content);
            rendered.push_str("\n\n");
            rendered.push_str(&preview_write_content(&content, options.expand_tool_output));
        }
        Ok(_) => {}
    }
    rendered
}

fn render_edit_call(arguments: &Value) -> String {
    let path = match first_string_argument(arguments, &["file_path", "path"]) {
        Ok(path) => render_path_value(path.as_deref(), "..."),
        Err(()) => "[invalid arg]".to_string(),
    };
    let replacement_count = edit_replacement_count(arguments);
    let suffix = replacement_count
        .filter(|count| *count > 0)
        .map(|count| {
            format!(
                " ({} replacement{})",
                count,
                if count == 1 { "" } else { "s" }
            )
        })
        .unwrap_or_default();
    format!("{TOOL_RUNNING_MARKER} **edit** {path}{suffix}")
}

fn render_grep_call(arguments: &Value) -> String {
    let pattern = match string_argument(arguments, "pattern") {
        Ok(Some(pattern)) => format!(
            "/{}/",
            escape_markdown_text(&sanitize_inline_text(&pattern))
        ),
        Ok(_) => "//".to_string(),
        Err(()) => "[invalid arg]".to_string(),
    };
    let path = match string_argument(arguments, "path") {
        Ok(Some(path)) if !path.is_empty() => render_path_value(Some(&path), "."),
        Ok(_) => inline_value("."),
        Err(()) => "[invalid arg]".to_string(),
    };
    let mut rendered = format!("{TOOL_RUNNING_MARKER} **grep** {pattern} in {path}");
    if let Some(glob) = string_argument(arguments, "glob").ok().flatten() {
        if !glob.is_empty() {
            rendered.push_str(&format!(" ({})", inline_value(&glob)));
        }
    }
    if let Some(limit) = numeric_argument(arguments, "limit") {
        rendered.push_str(&format!(" limit {limit}"));
    }
    rendered
}

fn render_find_call(arguments: &Value) -> String {
    let pattern = match string_argument(arguments, "pattern") {
        Ok(Some(pattern)) => escape_markdown_text(&sanitize_inline_text(&pattern)),
        Ok(_) => String::new(),
        Err(()) => "[invalid arg]".to_string(),
    };
    let path = match string_argument(arguments, "path") {
        Ok(Some(path)) if !path.is_empty() => render_path_value(Some(&path), "."),
        Ok(_) => inline_value("."),
        Err(()) => "[invalid arg]".to_string(),
    };
    let mut rendered = format!("{TOOL_RUNNING_MARKER} **find** {pattern} in {path}");
    if let Some(limit) = numeric_argument(arguments, "limit") {
        rendered.push_str(&format!(" (limit {limit})"));
    }
    rendered
}

fn render_ls_call(arguments: &Value) -> String {
    let path = match string_argument(arguments, "path") {
        Ok(Some(path)) if !path.is_empty() => render_path_value(Some(&path), "."),
        Ok(_) => inline_value("."),
        Err(()) => "[invalid arg]".to_string(),
    };
    let mut rendered = format!("{TOOL_RUNNING_MARKER} **ls** {path}");
    if let Some(limit) = numeric_argument(arguments, "limit") {
        rendered.push_str(&format!(" (limit {limit})"));
    }
    rendered
}

fn render_generic_tool_call(name: &str, arguments: &Value) -> String {
    let mut rendered = format!("{TOOL_RUNNING_MARKER} **{}**", escape_markdown_text(name));
    let Some(object) = arguments.as_object() else {
        if !arguments.is_null() {
            rendered.push(' ');
            rendered.push_str(&safe_argument_value(arguments, None));
        }
        return truncate_display(&rendered, MAX_GENERIC_CALL_CHARS);
    };

    let mut fields = Vec::new();
    for (key, value) in object.iter().take(MAX_GENERIC_ARGUMENTS) {
        fields.push(format!(
            "{}={}",
            escape_markdown_text(key),
            safe_argument_value(value, Some(key))
        ));
    }
    if object.len() > MAX_GENERIC_ARGUMENTS {
        fields.push("…".to_string());
    }
    if !fields.is_empty() {
        rendered.push(' ');
        rendered.push_str(&fields.join(" "));
    }
    truncate_display(&rendered, MAX_GENERIC_CALL_CHARS)
}

fn string_argument(arguments: &Value, key: &str) -> Result<Option<String>, ()> {
    let Some(object) = arguments.as_object() else {
        return if arguments.is_null() {
            Ok(None)
        } else {
            Err(())
        };
    };
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(()),
    }
}

fn first_string_argument(arguments: &Value, keys: &[&str]) -> Result<Option<String>, ()> {
    let Some(object) = arguments.as_object() else {
        return if arguments.is_null() {
            Ok(None)
        } else {
            Err(())
        };
    };
    for key in keys {
        if let Some(value) = object.get(*key) {
            if value.is_null() {
                continue;
            }
            return match value {
                Value::String(value) => Ok(Some(value.clone())),
                _ => Err(()),
            };
        }
    }
    Ok(None)
}

fn numeric_argument(arguments: &Value, key: &str) -> Option<String> {
    arguments
        .as_object()
        .and_then(|object| object.get(key))
        .and_then(Value::as_number)
        .map(ToString::to_string)
}

fn argument_is_invalid(arguments: &Value, key: &str) -> bool {
    let Some(object) = arguments.as_object() else {
        return !arguments.is_null();
    };
    object
        .get(key)
        .is_some_and(|value| !value.is_null() && !value.is_number())
}

fn render_path_value(raw_path: Option<&str>, empty_fallback: &str) -> String {
    match raw_path {
        Some(path) if !path.is_empty() => inline_value(&shorten_path(path)),
        _ => inline_value(empty_fallback),
    }
}

fn shorten_path(path: &str) -> String {
    let Some(home) = crate::config::home_dir() else {
        return path.to_string();
    };
    let home = home.to_string_lossy();
    if path == home {
        return "~".to_string();
    }
    path.strip_prefix(home.as_ref())
        .and_then(|suffix| suffix.strip_prefix('/'))
        .map(|suffix| format!("~/{suffix}"))
        .unwrap_or_else(|| path.to_string())
}

fn compact_read_label(path: &str) -> Option<String> {
    let normalized = path.replace('\\', "/");
    let file_name = normalized.rsplit('/').next().unwrap_or(normalized.as_str());
    let parent = normalized
        .rsplit_once('/')
        .map(|(_, parent)| parent.rsplit('/').next().unwrap_or(parent))
        .unwrap_or("SKILL");

    if file_name == "SKILL.md" {
        return Some(format!("[skill] {parent}"));
    }
    if matches!(
        file_name,
        "AGENTS.md" | "AGENTS.override.md" | "AGENTS.MD" | "CLAUDE.md" | "CLAUDE.MD"
    ) {
        return Some(format!("read resource {}", shorten_path(&normalized)));
    }
    if normalized == "README.md"
        || normalized.starts_with("docs/")
        || normalized.starts_with("examples/")
    {
        return Some(format!("read docs {}", shorten_path(&normalized)));
    }
    None
}

fn render_line_range(arguments: &Value) -> String {
    let Some(object) = arguments.as_object() else {
        return String::new();
    };
    let has_offset = object.get("offset").is_some_and(|value| !value.is_null());
    let has_limit = object.get("limit").is_some_and(|value| !value.is_null());
    if !has_offset && !has_limit {
        return String::new();
    }

    let start = match object.get("offset") {
        None | Some(Value::Null) => Some(1_i64),
        Some(value) => value.as_i64(),
    };
    let limit = match object.get("limit") {
        None | Some(Value::Null) => None,
        Some(value) => value.as_i64(),
    };
    let (Some(start), limit_valid) = (
        start,
        object
            .get("limit")
            .is_none_or(|value| value.is_null() || value.as_i64().is_some()),
    ) else {
        return ": [invalid arg]".to_string();
    };
    if !limit_valid {
        return ": [invalid arg]".to_string();
    }
    let end = limit.map(|count| start.saturating_add(count).saturating_sub(1));
    match end {
        Some(end) => format!(":{start}-{end}"),
        None => format!(":{start}"),
    }
}

fn edit_replacement_count(arguments: &Value) -> Option<usize> {
    let object = arguments.as_object()?;
    if let Some(edits) = object.get("edits") {
        match edits {
            Value::Array(edits) => return Some(edits.len()),
            Value::Object(edit) if edit.contains_key("oldText") && edit.contains_key("newText") => {
                return Some(1)
            }
            Value::String(edits) => {
                if let Ok(parsed) = serde_json::from_str::<Value>(edits) {
                    if let Value::Array(parsed) = parsed {
                        return Some(parsed.len());
                    }
                    if parsed.as_object().is_some_and(|edit| {
                        edit.contains_key("oldText") && edit.contains_key("newText")
                    }) {
                        return Some(1);
                    }
                }
            }
            _ => {}
        }
    }
    if object.get("oldText").is_some() && object.get("newText").is_some() {
        Some(1)
    } else {
        None
    }
}

fn safe_argument_value(value: &Value, key: Option<&str>) -> String {
    if key.is_some_and(is_sensitive_key) {
        return "[redacted]".to_string();
    }
    let rendered = match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => inline_value(value),
        Value::Array(values) => {
            let mut items = values
                .iter()
                .take(MAX_GENERIC_ARGUMENTS)
                .map(|item| safe_argument_value(item, None))
                .collect::<Vec<_>>();
            if values.len() > MAX_GENERIC_ARGUMENTS {
                items.push("…".to_string());
            }
            format!("[{}]", items.join(", "))
        }
        Value::Object(values) => {
            let mut fields = values
                .iter()
                .take(MAX_GENERIC_ARGUMENTS)
                .map(|(key, value)| {
                    format!(
                        "{}: {}",
                        escape_markdown_text(key),
                        safe_argument_value(value, Some(key))
                    )
                })
                .collect::<Vec<_>>();
            if values.len() > MAX_GENERIC_ARGUMENTS {
                fields.push("…".to_string());
            }
            format!("{{{}}}", fields.join(", "))
        }
    };
    truncate_display(&rendered, MAX_GENERIC_ARGUMENT_CHARS)
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "password",
        "passwd",
        "secret",
        "token",
        "api_key",
        "apikey",
        "authorization",
        "credential",
        "private_key",
        "access_key",
    ]
    .iter()
    .any(|needle| key.contains(needle))
}

fn sanitize_inline_text(text: &str) -> String {
    let text = pi_tui::strip_ansi_codes(text);
    let mut sanitized = String::new();
    for character in text.chars() {
        match character {
            '\n' | '\r' => sanitized.push('↵'),
            '\t' => sanitized.push_str("   "),
            character if character.is_control() => sanitized.push('�'),
            character => sanitized.push(character),
        }
    }
    sanitized
}

fn normalize_argument_text(text: &str) -> String {
    pi_tui::strip_ansi_codes(text)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "   ")
}

fn escape_markdown_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(character, '\\' | '`' | '*' | '[' | ']') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn inline_value(value: &str) -> String {
    format!("`{}`", sanitize_inline_text(value).replace('`', "'"))
}

fn truncate_display(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated = text
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

fn preview_write_content(text: &str, expanded: bool) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    if expanded || lines.len() <= WRITE_CALL_PREVIEW_LINES {
        return escape_literal_lines(&lines);
    }
    let mut preview = escape_literal_lines(&lines[..WRITE_CALL_PREVIEW_LINES]);
    preview.push_str(&format!(
        "\n... ({} more lines, {} total, expand to show all)",
        lines.len() - WRITE_CALL_PREVIEW_LINES,
        lines.len()
    ));
    preview
}

fn escape_literal_lines(lines: &[&str]) -> String {
    lines
        .iter()
        .map(|line| escape_markdown_output(line))
        .collect::<Vec<_>>()
        .join("  \n")
}

fn preview_tail_lines(text: &str, max_lines: usize, expanded: bool, noun: &str) -> String {
    if expanded || max_lines == 0 {
        return text.to_string();
    }
    let lines = text.lines().collect::<Vec<_>>();
    if lines.len() <= max_lines {
        return text.to_string();
    }
    let skipped = lines.len() - max_lines;
    format!(
        "... ({skipped} {noun} lines; expand to show all)\n{}",
        lines[skipped..].join("\n")
    )
}

fn render_tool_details(tool_name: &str, details: &Value, _is_error: bool) -> Option<String> {
    let Some(object) = details.as_object() else {
        return (!details.is_null())
            .then(|| format!("Details: {}", safe_argument_value(details, None)));
    };
    if object.is_empty() {
        return None;
    }

    let truncation = object.get("truncation");
    let full_output_path = object
        .get("fullOutputPath")
        .or_else(|| object.get("full_output_path"))
        .and_then(Value::as_str)
        .map(sanitize_inline_text);
    let mut notices = Vec::new();
    let mut truncated_heading = false;

    match tool_name {
        "bash" => {
            if let Some(path) = full_output_path.as_deref() {
                notices.push(format!("Full output: {path}"));
            }
            if let Some(truncation) = truncation.and_then(render_bash_truncation_detail) {
                notices.push(truncation);
            }
        }
        "read" => {
            if let Some(truncation) = truncation.and_then(render_read_truncation_detail) {
                notices.push(truncation);
            }
        }
        "grep" => {
            if let Some(count) = detail_number(object, &["matchLimitReached"]) {
                notices.push(format!("{count} matches limit"));
            }
            if truncation.is_some_and(is_truncated_detail) {
                let size = truncation
                    .and_then(truncation_max_bytes)
                    .unwrap_or(pi_agent::tools::truncate::DEFAULT_MAX_BYTES as u64);
                notices.push(format!("{} limit", format_detail_size(size)));
            }
            if detail_bool(object, &["linesTruncated"]) {
                notices.push("some lines truncated".to_string());
            }
            truncated_heading = !notices.is_empty();
        }
        "find" => {
            if let Some(count) = detail_number(object, &["resultLimitReached"]) {
                notices.push(format!("{count} results limit"));
            }
            if truncation.is_some_and(is_truncated_detail) {
                let size = truncation
                    .and_then(truncation_max_bytes)
                    .unwrap_or(pi_agent::tools::truncate::DEFAULT_MAX_BYTES as u64);
                notices.push(format!("{} limit", format_detail_size(size)));
            }
            truncated_heading = !notices.is_empty();
        }
        "ls" => {
            if let Some(count) = detail_number(object, &["entryLimitReached"]) {
                notices.push(format!("{count} entries limit"));
            }
            if truncation.is_some_and(is_truncated_detail) {
                let size = truncation
                    .and_then(truncation_max_bytes)
                    .unwrap_or(pi_agent::tools::truncate::DEFAULT_MAX_BYTES as u64);
                notices.push(format!("{} limit", format_detail_size(size)));
            }
            truncated_heading = !notices.is_empty();
        }
        _ => {
            if let Some(truncation) = truncation.and_then(render_truncation_detail) {
                notices.push(truncation);
            }
            if let Some(path) = full_output_path.as_deref() {
                notices.push(format!("Full output: {path}"));
            }
        }
    }

    let handled = [
        "truncation",
        "fullOutputPath",
        "full_output_path",
        "diff",
        "patch",
        "matchLimitReached",
        "linesTruncated",
        "resultLimitReached",
        "entryLimitReached",
    ];
    let extras = object
        .iter()
        .filter(|(key, _)| !handled.contains(&key.as_str()))
        .map(|(key, value)| {
            format!(
                "{}={}",
                escape_markdown_text(key),
                safe_argument_value(value, Some(key))
            )
        })
        .collect::<Vec<_>>();

    if !notices.is_empty() {
        let separator = if truncated_heading { ", " } else { ". " };
        let heading = if truncated_heading { "Truncated: " } else { "" };
        let mut rendered = format!("[{heading}{}]", notices.join(separator));
        if !extras.is_empty() {
            rendered.push_str(&format!("\nDetails: {}", extras.join(" ")));
        }
        Some(rendered)
    } else if !extras.is_empty() {
        Some(format!("Details: {}", extras.join(" ")))
    } else {
        None
    }
}

/// Escape a live display value before it is placed in transcript markdown.
/// This is intentionally a display-only helper; model-facing tool arguments
/// remain untouched and are persisted through the normal agent/session path.
pub fn escape_display_text(text: &str) -> String {
    escape_markdown_text(&sanitize_inline_text(text))
}

/// Expose the human-readable detail formatter to the live interactive owner.
/// Unknown detail keys remain bounded and redacted by the same renderer used
/// for settled tool messages.
pub fn render_tool_details_for_display(
    tool_name: &str,
    details: &Value,
    is_error: bool,
) -> Option<String> {
    render_tool_details(tool_name, details, is_error)
}

fn is_truncated_detail(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|object| detail_bool(object, &["truncated"]))
}

fn truncation_max_bytes(value: &Value) -> Option<u64> {
    value
        .as_object()
        .and_then(|object| detail_number(object, &["maxBytes", "max_bytes"]))
}

fn render_bash_truncation_detail(value: &Value) -> Option<String> {
    let object = value.as_object()?;
    if !detail_bool(object, &["truncated"]) {
        return None;
    }
    let by_lines = object
        .get("truncatedBy")
        .or_else(|| object.get("truncated_by"))
        .and_then(Value::as_str)
        == Some("lines");
    let output_lines = detail_number(object, &["outputLines", "output_lines"]);
    let total_lines = detail_number(object, &["totalLines", "total_lines"]);
    if by_lines {
        if let (Some(output), Some(total)) = (output_lines, total_lines) {
            return Some(format!("Truncated: showing {output} of {total} lines"));
        }
        return Some("Truncated: line limit reached".to_string());
    }
    let size = truncation_max_bytes(value)
        .map(format_detail_size)
        .unwrap_or_else(|| format_detail_size(pi_agent::tools::truncate::DEFAULT_MAX_BYTES as u64));
    Some(format!(
        "Truncated: {} lines shown ({} limit)",
        output_lines.unwrap_or_default(),
        size
    ))
}

fn render_read_truncation_detail(value: &Value) -> Option<String> {
    let object = value.as_object()?;
    if !detail_bool(object, &["truncated"]) {
        return None;
    }
    if detail_bool(
        object,
        &["firstLineExceedsLimit", "first_line_exceeds_limit"],
    ) {
        let size = truncation_max_bytes(value)
            .map(format_detail_size)
            .unwrap_or_else(|| {
                format_detail_size(pi_agent::tools::truncate::DEFAULT_MAX_BYTES as u64)
            });
        return Some(format!("First line exceeds {size} limit"));
    }
    let by_lines = object
        .get("truncatedBy")
        .or_else(|| object.get("truncated_by"))
        .and_then(Value::as_str)
        == Some("lines");
    let output_lines = detail_number(object, &["outputLines", "output_lines"]);
    let total_lines = detail_number(object, &["totalLines", "total_lines"]);
    if by_lines {
        if let (Some(output), Some(total)) = (output_lines, total_lines) {
            let line_limit = detail_number(object, &["maxLines", "max_lines"])
                .map(|limit| format!(" ({limit} line limit)"))
                .unwrap_or_default();
            return Some(format!(
                "Truncated: showing {output} of {total} lines{line_limit}"
            ));
        }
        return Some("Truncated: line limit reached".to_string());
    }
    let size = truncation_max_bytes(value)
        .map(format_detail_size)
        .unwrap_or_else(|| format_detail_size(pi_agent::tools::truncate::DEFAULT_MAX_BYTES as u64));
    Some(format!(
        "Truncated: {} lines shown ({} limit)",
        output_lines.unwrap_or_default(),
        size
    ))
}

fn render_truncation_detail(value: &Value) -> Option<String> {
    let object = value.as_object()?;
    if !detail_bool(object, &["truncated"]) {
        return None;
    }
    if detail_bool(
        object,
        &["firstLineExceedsLimit", "first_line_exceeds_limit"],
    ) {
        let limit = detail_number(object, &["maxBytes", "max_bytes"])
            .map(format_detail_size)
            .unwrap_or_else(|| "byte".to_string());
        return Some(format!("First line exceeds {limit} limit"));
    }
    let by_lines = object
        .get("truncatedBy")
        .or_else(|| object.get("truncated_by"))
        .and_then(Value::as_str)
        == Some("lines");
    let output_lines = detail_number(object, &["outputLines", "output_lines"]);
    let total_lines = detail_number(object, &["totalLines", "total_lines"]);
    if by_lines {
        if let (Some(output), Some(total)) = (output_lines, total_lines) {
            return Some(format!("Truncated: showing {output} of {total} lines"));
        }
        return Some("Truncated: line limit reached".to_string());
    }
    if let Some(max_bytes) = detail_number(object, &["maxBytes", "max_bytes"]) {
        return Some(format!(
            "Truncated: {} lines shown ({} limit)",
            output_lines.unwrap_or_default(),
            format_detail_size(max_bytes)
        ));
    }
    Some("Truncated".to_string())
}

fn detail_number(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| object.get(*key))
        .and_then(Value::as_u64)
}

fn detail_bool(object: &serde_json::Map<String, Value>, keys: &[&str]) -> bool {
    keys.iter()
        .find_map(|key| object.get(*key))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn format_detail_size(bytes: u64) -> String {
    crate::core::tools::format_size(bytes)
}

fn render_tool_result(result: &ToolResultMessage, options: TranscriptRenderOptions) -> String {
    let ToolResultMessage::ToolResult {
        content,
        details,
        added_tool_names,
        ..
    } = result;

    let is_error = result.is_error();
    let status = if is_error { "✗" } else { "✓" };
    let mut rendered = format!("{status} **{}**", escape_markdown_text(result.tool_name()));
    append_tool_execution_result(
        &mut rendered,
        result.tool_name(),
        content,
        details.as_ref(),
        is_error,
        added_tool_names.as_deref().unwrap_or(&[]),
        options,
    );
    rendered
}

fn render_tool_result_content(
    tool_name: &str,
    content: &[ContentBlock],
    details: Option<&Value>,
    is_error: bool,
    options: TranscriptRenderOptions,
) -> String {
    // Upstream's render-utils displays all text blocks first and then either
    // inline images or image fallbacks, even when the model returned them in
    // an interleaved order. Retain that observable ordering here.
    let mut text_blocks = Vec::new();
    let mut image_blocks = Vec::new();
    let mut other_blocks = Vec::new();

    for block in content {
        match block {
            ContentBlock::Text { text, .. } => text_blocks.push(normalize_tool_text(text)),
            ContentBlock::Image { .. } => image_blocks.push(block),
            _ => other_blocks.push(block),
        }
    }

    let mut sections = Vec::new();
    let text = text_blocks
        .into_iter()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let show_text = !(tool_name == "write" && !is_error)
        && !(tool_name == "edit" && !is_error)
        && !(tool_name == "read" && !options.expand_tool_output && !is_error);
    if show_text && !text.is_empty() {
        let rendered = match tool_name {
            "bash" => preview_tail_lines(
                &text,
                BASH_RESULT_PREVIEW_LINES,
                options.expand_tool_output,
                "earlier",
            ),
            "read" => preview_lines(&text, READ_RESULT_PREVIEW_LINES, options.expand_tool_output),
            "grep" => preview_lines(&text, GREP_RESULT_PREVIEW_LINES, options.expand_tool_output),
            "find" => preview_lines(&text, FIND_RESULT_PREVIEW_LINES, options.expand_tool_output),
            "ls" => preview_lines(&text, LS_RESULT_PREVIEW_LINES, options.expand_tool_output),
            _ => preview_lines(
                &text,
                DEFAULT_TOOL_PREVIEW_LINES,
                options.expand_tool_output,
            ),
        };
        if !rendered.is_empty() {
            sections.push(rendered);
        }
    }

    // The edit renderer puts a successful diff beside the call component. In
    // this transcript-only representation the result owns that same readable
    // diff, while the success sentence remains intentionally omitted.
    if tool_name == "edit" && !is_error {
        if let Some(diff) = details
            .and_then(|value| value.get("diff"))
            .and_then(Value::as_str)
        {
            let diff = normalize_tool_text(diff).trim().to_string();
            if !diff.is_empty() {
                sections.push(diff);
            }
        }
    }

    for block in image_blocks {
        sections.push(render_image_block(block, options));
    }
    for block in other_blocks {
        let rendered = match block {
            ContentBlock::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                render_thinking_block(thinking.trim().to_string())
            }
            ContentBlock::ToolCall { .. } => render_tool_call(block, options),
            _ => String::new(),
        };
        if !rendered.is_empty() {
            sections.push(rendered);
        }
    }

    sections.join("\n\n").trim().to_string()
}

fn render_custom_message(
    custom: &CustomAgentMessage,
    options: TranscriptRenderOptions,
) -> Option<(String, String)> {
    match custom {
        CustomAgentMessage::BashExecution {
            command,
            output,
            exit_code,
            cancelled,
            truncated,
            full_output_path,
            ..
        } => {
            let output = normalize_tool_text(output);
            let output = preview_tail_lines(
                output.trim(),
                DEFAULT_BASH_PREVIEW_LINES,
                options.expand_tool_output,
                "earlier",
            );
            let mut rendered = format!(
                "**$ {}**",
                escape_markdown_text(&sanitize_inline_text(command))
            );
            if !output.is_empty() {
                rendered.push_str("\n\n");
                rendered.push_str(&output);
            }

            let mut status = Vec::new();
            if *cancelled {
                status.push("(cancelled)".to_string());
            } else if let Some(code) = exit_code {
                if *code != 0 {
                    status.push(format!("(exit {code})"));
                }
            }
            if *truncated {
                status.push(match full_output_path {
                    Some(path) => format!("Output truncated. Full output: {path}"),
                    None => "Output truncated.".to_string(),
                });
            }
            if !status.is_empty() {
                rendered.push_str("\n\n");
                rendered.push_str(&status.join("\n"));
            } else if exit_code.is_none() {
                rendered.push_str("\n\n⏳ Running... (Esc to cancel)");
            }
            // Keep the direct shell execution on its own component lane. The
            // interactive view uses this discriminator to add Pi's spacer +
            // dynamic bash border instead of treating it as generic tool
            // markdown.
            Some(("bash".to_string(), rendered))
        }
        CustomAgentMessage::CompactionSummary {
            summary,
            tokens_before,
            ..
        } => {
            let tokens = format_tokens(*tokens_before);
            Some((
                "banner".to_string(),
                format!("**[compaction]** Compacted from {tokens} tokens\n\n{summary}"),
            ))
        }
        CustomAgentMessage::BranchSummary { summary, .. } => Some((
            "banner".to_string(),
            format!("**[branch]** Branch summary\n\n{summary}"),
        )),
        CustomAgentMessage::Custom {
            custom_type,
            content,
            display,
            details,
            ..
        } => {
            if !display {
                return None;
            }
            let body = match content {
                CustomContent::String(text) => text.clone(),
                CustomContent::Blocks(blocks) => render_ordered_blocks(blocks, options),
            };
            let mut rendered = format!("**[{custom_type}]**");
            if !body.trim().is_empty() {
                rendered.push_str("\n\n");
                rendered.push_str(body.trim());
            }
            if let Some(details) = details {
                rendered.push_str("\n\n");
                rendered.push_str(&render_display_details("Details", details));
            }
            Some(("banner".to_string(), rendered))
        }
    }
}

fn render_ordered_blocks(blocks: &[ContentBlock], options: TranscriptRenderOptions) -> String {
    let mut sections = Vec::new();
    let mut adjacent_text = String::new();

    for block in blocks {
        if let ContentBlock::Text { text, .. } = block {
            // Upstream user/custom messages concatenate adjacent text blocks;
            // do the same while still keeping images and other block types
            // visually separated.
            adjacent_text.push_str(text);
            continue;
        }

        if !adjacent_text.is_empty() {
            sections.push(std::mem::take(&mut adjacent_text));
        }
        let rendered = match block {
            ContentBlock::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                if options.hide_thinking {
                    render_hidden_thinking_label()
                } else {
                    render_thinking_block(thinking.trim().to_string())
                }
            }
            ContentBlock::Image { .. } => render_image_block(block, options),
            ContentBlock::ToolCall { .. } => render_tool_call(block, options),
            ContentBlock::Text { .. } => unreachable!(),
            ContentBlock::Thinking { .. } => String::new(),
        };
        if !rendered.is_empty() {
            sections.push(rendered);
        }
    }
    if !adjacent_text.is_empty() {
        sections.push(adjacent_text);
    }
    sections.join("\n\n")
}

fn render_image_block(block: &ContentBlock, options: TranscriptRenderOptions) -> String {
    let ContentBlock::Image { data, mime_type } = block else {
        return String::new();
    };
    let mime_type = if mime_type.trim().is_empty() {
        "image/unknown"
    } else {
        mime_type.as_str()
    };
    let dimensions = get_image_dimensions(data, mime_type);
    let fallback = || image_fallback(mime_type, dimensions, None);

    let capabilities = get_capabilities();
    if !options.show_images || capabilities.images.is_none() || data.trim().is_empty() {
        return fallback();
    }

    let (width_px, height_px) = dimensions.unwrap_or((800, 600));
    let Some(rendered) = render_image(
        data,
        ImageDimensions {
            width_px,
            height_px,
        },
        ImageRenderOptions {
            max_width_cells: Some(options.image_width_cells.max(1)),
            move_cursor: Some(false),
            ..Default::default()
        },
    ) else {
        return fallback();
    };

    match capabilities.images {
        Some(ImageProtocol::Kitty) => {
            let mut lines = vec![rendered.sequence];
            lines.extend(std::iter::repeat_n(
                String::new(),
                rendered.rows.saturating_sub(1),
            ));
            lines.join("\n")
        }
        Some(ImageProtocol::ITerm2) => {
            let row_offset = rendered.rows.saturating_sub(1);
            let mut lines = vec![String::new(); row_offset];
            let move_up = if row_offset > 0 {
                format!("\x1b[{row_offset}A")
            } else {
                String::new()
            };
            lines.push(format!("{move_up}{}", rendered.sequence));
            lines.join("\n")
        }
        None => fallback(),
    }
}

fn render_display_details<T: Serialize + ?Sized>(label: &str, value: &T) -> String {
    let value = serde_json::to_value(value).unwrap_or(Value::Null);
    format!("**{label}**\n\n{}", safe_argument_value(&value, None))
}

fn normalize_tool_text(text: &str) -> String {
    let without_ansi = pi_tui::strip_ansi_codes(text);
    let normalized =
        pi_agent::harness::shell_output::sanitize_binary_output(&without_ansi).replace('\r', "");
    // Tool output is rendered inside the transcript's Markdown component,
    // while upstream Pi renders it through plain Text components. Escape
    // Markdown syntax here so literal command output (for example
    // `context_overflow_recovery.rs`, headings, list markers, and table rows)
    // remains byte-for-byte visible instead of being interpreted as styling.
    escape_markdown_output(&normalized)
}

fn escape_markdown_output(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if character == '\n' {
            // The Markdown component otherwise treats paragraph newlines as
            // soft spaces. Two trailing spaces select its hard-break path so
            // tool output keeps the one-row-per-line behavior of Pi's plain
            // Text tool components.
            escaped.push_str("  \n");
            continue;
        }
        if matches!(
            character,
            '\\' | '`' | '*' | '_' | '[' | ']' | '~' | '#' | '>' | '|' | '+' | '-' | '.'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn preview_lines(text: &str, max_lines: usize, expanded: bool) -> String {
    if expanded || max_lines == 0 {
        return text.to_string();
    }
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines {
        return text.to_string();
    }
    let mut preview = lines[..max_lines].join("\n");
    let remaining = lines.len() - max_lines;
    preview.push_str(&format!(
        "\n... ({remaining} more lines; expand to show all)"
    ));
    preview
}

/// Build the full transcript markdown given the message list.
pub fn build_transcript(messages: &[AgentMessage], hide_thinking: bool) -> String {
    build_transcript_with_options(
        messages,
        TranscriptRenderOptions {
            hide_thinking,
            ..Default::default()
        },
    )
}

/// Build a transcript with explicit rendering controls.
pub fn build_transcript_with_options(
    messages: &[AgentMessage],
    options: TranscriptRenderOptions,
) -> String {
    build_transcript_with_cache_notices_and_options(messages, options, &[])
}

/// Build the transcript and re-inject non-persisted cache-miss notices after
/// the assistant message that paid for each miss. Notices are keyed by the
/// assistant timestamp so they survive compaction and session rehydration.
pub fn build_transcript_with_cache_notices(
    messages: &[AgentMessage],
    hide_thinking: bool,
    cache_notices: &[(u64, String)],
) -> String {
    build_transcript_with_cache_notices_and_options(
        messages,
        TranscriptRenderOptions {
            hide_thinking,
            ..Default::default()
        },
        cache_notices,
    )
}

/// Build a transcript with cache notices and explicit rendering controls.
pub fn build_transcript_with_cache_notices_and_options(
    messages: &[AgentMessage],
    options: TranscriptRenderOptions,
    cache_notices: &[(u64, String)],
) -> String {
    let mut out = String::new();
    for message in messages {
        if let Some((kind, text)) = render_message_with_options(message, options) {
            match kind.as_str() {
                "user" => {
                    out.push_str(&text);
                    out.push_str("\n\n");
                }
                "assistant" | "tool" | "bash" | "banner" => {
                    out.push_str(&text);
                    out.push_str("\n\n");
                }
                _ => {}
            }

            if let AgentMessage::Core(Message::Assistant(assistant)) = message {
                if let Some((_, notice)) = cache_notices
                    .iter()
                    .find(|(timestamp, _)| *timestamp == assistant.timestamp())
                {
                    out.push_str("> ");
                    out.push_str(notice);
                    out.push_str("\n\n");
                }
            }
        }
    }
    out
}

/// Format a token count like upstream formatTokens.
pub fn format_tokens(count: u64) -> String {
    if count < 1000 {
        return count.to_string();
    }
    if count < 10_000 {
        return format!("{:.1}k", count as f64 / 1000.0);
    }
    if count < 1_000_000 {
        return format!("{}k", (count as f64 / 1000.0).round() as u64);
    }
    if count < 10_000_000 {
        return format!("{:.1}M", count as f64 / 1_000_000.0);
    }
    format!("{}M", (count as f64 / 1_000_000.0).round() as u64)
}

/// Get the thinking level for the active model from settings.
pub fn current_thinking_level(
    settings: &SettingsManager,
    _provider: &str,
    _model_id: &str,
) -> Option<String> {
    settings.get_default_thinking_level().map(|s| s.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::{Mutex, OnceLock};

    use super::*;
    use pi_ai::types::{AssistantMessageDiagnostic, UserContent};
    use pi_tui::components::markdown::plain_markdown_theme;
    use pi_tui::components::Markdown;
    use pi_tui::terminal_image::{
        reset_capabilities_cache, set_capabilities, TerminalCapabilities,
    };
    use pi_tui::tui::Component;

    const ONE_BY_ONE_PNG: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    fn assistant(content: Vec<ContentBlock>) -> AgentMessage {
        let mut message = AssistantMessage::new();
        message.set_content(content);
        AgentMessage::Core(Message::Assistant(message))
    }

    fn options(show_images: bool) -> TranscriptRenderOptions {
        TranscriptRenderOptions {
            show_images,
            ..Default::default()
        }
    }

    fn image_capability_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct ResetCapabilities;

    impl Drop for ResetCapabilities {
        fn drop(&mut self) {
            reset_capabilities_cache();
        }
    }

    #[test]
    fn assistant_renders_markdown_thinking_tool_calls_images_and_diagnostics() {
        let mut message = AssistantMessage::new();
        message.set_content(vec![
            ContentBlock::text("**answer**"),
            ContentBlock::thinking("check the repository"),
            ContentBlock::tool_call("call-1", "read", serde_json::json!({"path": "src/main.rs"})),
            ContentBlock::image(ONE_BY_ONE_PNG, "image/png"),
        ]);
        let mut diagnostic = AssistantMessageDiagnostic::new("renderer_test");
        diagnostic.details = Some(std::collections::BTreeMap::from([(
            "phase".to_string(),
            serde_json::json!("stream"),
        )]));
        message.append_diagnostic(diagnostic);

        let (_, rendered) = render_message_with_options(
            &AgentMessage::Core(Message::Assistant(message)),
            options(false),
        )
        .expect("assistant content should render");
        let visible = pi_tui::strip_ansi_codes(&rendered);
        assert!(rendered.contains("**answer**"));
        assert!(visible.contains("> check the repository"));
        assert!(rendered.contains("**read**"));
        assert!(rendered.contains("`src/main.rs`"));
        assert!(rendered.contains("⏳"));
        assert!(!rendered.contains("\"path\": \"src/main.rs\""));
        assert!(rendered.contains("[Image: [image/png] 1x1]"));
        assert!(rendered.contains("**Diagnostics**"));
        assert!(rendered.contains("renderer_test"));
    }

    #[test]
    fn tool_result_renders_text_images_details_and_added_tools() {
        let result = ToolResultMessage::new(
            "call-1",
            "read",
            vec![
                ContentBlock::text("line one\nline two"),
                ContentBlock::image(ONE_BY_ONE_PNG, "image/png"),
            ],
            false,
        )
        .with_details_usage_timestamp(None, Some(serde_json::json!({"bytes": 12})), 10);
        let AgentMessage::Core(Message::ToolResult(mut result)) =
            AgentMessage::Core(Message::ToolResult(result))
        else {
            unreachable!();
        };
        let ToolResultMessage::ToolResult {
            added_tool_names, ..
        } = &mut result;
        *added_tool_names = Some(vec!["write".to_string()]);

        let (_, rendered) = render_message_with_options(
            &AgentMessage::Core(Message::ToolResult(result)),
            options(false),
        )
        .expect("tool result should render");
        assert!(rendered.starts_with("✓ **read**"));
        assert!(rendered.contains("line one  \nline two"));
        assert!(rendered.contains("[Image: [image/png] 1x1]"));
        assert!(rendered.contains("Details: bytes=12"));
        assert!(!rendered.contains("```json"));
        assert!(!rendered.contains("\"bytes\": 12"));
        assert!(rendered.contains("**Tools added:** write"));
    }

    #[test]
    fn supported_terminal_renders_real_kitty_image_sequence() {
        let _lock = image_capability_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        set_capabilities(TerminalCapabilities {
            images: Some(ImageProtocol::Kitty),
            true_color: true,
            hyperlinks: true,
        });
        let _reset = ResetCapabilities;

        let (_, rendered) = render_message_with_options(
            &assistant(vec![ContentBlock::image(ONE_BY_ONE_PNG, "image/png")]),
            options(true),
        )
        .expect("image-only assistant should render");
        assert!(rendered.contains("\x1b_G"));
        assert!(pi_tui::terminal_image::is_image_line(&rendered));
    }

    #[test]
    fn custom_blocks_respect_display_and_render_images_and_details() {
        let visible = AgentMessage::Custom(CustomAgentMessage::Custom {
            custom_type: "notice".to_string(),
            content: CustomContent::Blocks(vec![
                ContentBlock::text("custom text"),
                ContentBlock::image(ONE_BY_ONE_PNG, "image/png"),
            ]),
            display: true,
            details: Some(serde_json::json!({"source": "test"})),
            hook_type: None,
            timestamp: 1,
        });
        let (_, rendered) = render_message_with_options(&visible, options(false)).unwrap();
        assert!(rendered.contains("**[notice]**"));
        assert!(rendered.contains("custom text"));
        assert!(rendered.contains("[Image: [image/png] 1x1]"));
        assert!(rendered.contains("**Details**"));
        assert!(rendered.contains("source: `test`"));
        assert!(!rendered.contains("```json"));

        let hidden = AgentMessage::Custom(CustomAgentMessage::Custom {
            custom_type: "hidden".to_string(),
            content: CustomContent::String("not displayed".to_string()),
            display: false,
            details: None,
            hook_type: None,
            timestamp: 1,
        });
        assert!(render_message_with_options(&hidden, options(false)).is_none());
    }

    #[test]
    fn user_image_and_bash_output_are_rendered_without_losing_text() {
        let user = AgentMessage::Core(Message::User(UserContent::blocks(
            vec![
                ContentBlock::text("describe this"),
                ContentBlock::image(ONE_BY_ONE_PNG, "image/png"),
            ],
            1,
        )));
        let (_, rendered) = render_message_with_options(&user, options(false)).unwrap();
        assert!(rendered.starts_with("describe this"));
        assert!(rendered.contains("[Image: [image/png] 1x1]"));

        let bash = AgentMessage::Custom(CustomAgentMessage::BashExecution {
            command: "printf hi".to_string(),
            output: "\u{1b}[31mhi\u{1b}[0m\r\n".to_string(),
            exit_code: Some(0),
            cancelled: false,
            truncated: true,
            full_output_path: Some("/tmp/full-output.log".to_string()),
            timestamp: 2,
            exclude_from_context: None,
        });
        let (_, rendered) = render_message_with_options(&bash, options(false)).unwrap();
        assert!(rendered.contains("**$ printf hi**"));
        assert!(!rendered.contains("✓ **$ printf hi**"));
        assert!(rendered.contains("hi"));
        assert!(!rendered.contains("\u{1b}[31m"));
        assert!(rendered.contains("Output truncated. Full output: /tmp/full-output.log"));
    }

    #[test]
    fn hidden_thinking_is_one_label_per_run_and_live_events_use_partial_message() {
        let mut partial = AssistantMessage::new();
        partial.set_content(vec![
            ContentBlock::thinking("first"),
            ContentBlock::thinking("second"),
            ContentBlock::text("streaming"),
            ContentBlock::tool_call("call-1", "bash", serde_json::json!({"command": "pwd"})),
        ]);
        let event = AssistantMessageEvent::ToolCallDelta {
            content_index: 3,
            delta: "}".to_string(),
            partial,
        };
        let rendered = render_assistant_event(&event, true).expect("partial should render");
        assert_eq!(rendered.matches("Thinking...").count(), 1);
        assert!(rendered.contains("streaming"));
        assert!(rendered.contains("⏳ **$ pwd**"));
        assert!(!rendered.contains("\"command\": \"pwd\""));
        assert!(!rendered.contains("```json"));
    }

    #[test]
    fn assistant_only_projection_does_not_duplicate_tool_terminal_errors() {
        let mut partial = AssistantMessage::new();
        partial.set_content(vec![
            ContentBlock::text("before tool"),
            ContentBlock::tool_call("call-1", "read", serde_json::json!({"path": "file.txt"})),
        ]);
        partial.set_stop_reason(StopReason::Error);
        partial.set_error_message("tool failed");
        let event = AssistantMessageEvent::Done {
            reason: pi_ai::types::DoneReason::ToolUse,
            message: partial,
        };

        let rendered =
            render_assistant_event_without_tool_calls_with_options(&event, options(false))
                .expect("the assistant prose remains visible");
        assert_eq!(rendered, "before tool");
        assert!(!rendered.contains("tool failed"));
        assert!(!rendered.contains("Error:"));
    }

    #[test]
    fn collapsed_tool_output_keeps_preview_and_expansion_hint() {
        let result = ToolResultMessage::text(
            "call-1",
            "grep",
            (0..18)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
            false,
        );
        let rendered = render_message_with_options(
            &AgentMessage::Core(Message::ToolResult(result)),
            TranscriptRenderOptions {
                expand_tool_output: false,
                ..options(false)
            },
        )
        .unwrap()
        .1;
        assert!(rendered.contains("line 0"));
        assert!(rendered.contains("line 9"));
        assert!(rendered.contains("line 14"));
        assert!(!rendered.contains("line 15\n"));
        assert!(rendered.contains("3 more lines; expand to show all"));
    }

    #[test]
    fn built_in_tool_calls_use_pi_style_compact_summaries() {
        let mut message = AssistantMessage::new();
        message.set_content(vec![
            ContentBlock::tool_call(
                "bash-1",
                "bash",
                serde_json::json!({"command": "printf hi", "timeout": 5}),
            ),
            ContentBlock::tool_call(
                "read-1",
                "read",
                serde_json::json!({"path": "src/main.rs", "offset": 3, "limit": 3}),
            ),
            ContentBlock::tool_call(
                "write-1",
                "write",
                serde_json::json!({"path": "out.txt", "content": "one\ntwo"}),
            ),
            ContentBlock::tool_call(
                "edit-1",
                "edit",
                serde_json::json!({
                    "path": "src/lib.rs",
                    "edits": [
                        {"oldText": "old", "newText": "new"},
                        {"oldText": "a", "newText": "b"}
                    ]
                }),
            ),
            ContentBlock::tool_call(
                "grep-1",
                "grep",
                serde_json::json!({
                    "pattern": "TODO",
                    "path": "src",
                    "glob": "*.rs",
                    "limit": 10
                }),
            ),
            ContentBlock::tool_call(
                "find-1",
                "find",
                serde_json::json!({"pattern": "**/*.rs", "path": "src", "limit": 20}),
            ),
            ContentBlock::tool_call(
                "ls-1",
                "ls",
                serde_json::json!({"path": "src", "limit": 20}),
            ),
        ]);

        let rendered = render_message_with_options(
            &AgentMessage::Core(Message::Assistant(message)),
            options(false),
        )
        .expect("tool calls should render")
        .1;

        for expected in [
            "⏳ **$ printf hi** (timeout 5s)",
            "⏳ **read** `src/main.rs`:3-5",
            "⏳ **write** `out.txt`\n\none  \ntwo",
            "⏳ **edit** `src/lib.rs` (2 replacements)",
            "⏳ **grep** /TODO/ in `src` (`*.rs`) limit 10",
            "⏳ **find** \\*\\*/\\*.rs in `src` (limit 20)",
            "⏳ **ls** `src` (limit 20)",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?} in {rendered:?}"
            );
        }
        assert!(!rendered.contains("```json"));
        assert!(!rendered.contains("\"path\": \"src/main.rs\""));
        assert!(!rendered.contains("{\"path\""));
    }

    #[test]
    fn read_compact_classifications_are_collapsed_only() {
        let message = assistant(vec![ContentBlock::tool_call(
            "read-1",
            "read",
            serde_json::json!({"path": "README.md"}),
        )]);

        let collapsed = render_message_with_options(
            &message,
            TranscriptRenderOptions {
                expand_tool_output: false,
                ..options(false)
            },
        )
        .expect("collapsed read call should render")
        .1;
        assert!(collapsed.contains("⏳ **read docs** `README.md`"));

        let expanded = render_message_with_options(&message, options(false))
            .expect("expanded read call should render")
            .1;
        assert!(expanded.contains("⏳ **read** `README.md`"));
        assert!(!expanded.contains("**read docs**"));
    }

    #[test]
    fn unknown_tool_arguments_are_readable_bounded_and_redacted() {
        let rendered = render_message_with_options(
            &assistant(vec![ContentBlock::tool_call(
                "custom-1",
                "custom_tool",
                serde_json::json!({
                    "query": "needle",
                    "api_key": "sk-do-not-display",
                    "nested": {"password": "also-do-not-display", "mode": "fast"}
                }),
            )]),
            options(false),
        )
        .expect("custom tool call should render")
        .1;

        assert!(rendered.contains("⏳ **custom_tool**"));
        assert!(rendered.contains("query=`needle`"));
        assert!(rendered.contains("api_key=[redacted]"));
        assert!(rendered.contains("password: [redacted]"));
        assert!(rendered.contains("mode: `fast`"));
        assert!(!rendered.contains("sk-do-not-display"));
        assert!(!rendered.contains("also-do-not-display"));
        assert!(!rendered.contains("```json"));
        assert!(!rendered.contains("\"query\""));
    }

    #[test]
    fn live_tool_execution_events_share_compact_rendering_without_json_envelopes() {
        let start = RichAgentEvent::ToolExecutionStart {
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            args: serde_json::json!({"command": "printf hi"}),
        };
        let start_rendered =
            render_tool_execution_event(&start, options(false)).expect("start should render");
        assert_eq!(start_rendered, "⏳ **$ printf hi**");
        assert!(!start_rendered.contains("```"));
        assert!(!start_rendered.contains("{\""));

        let update = RichAgentEvent::ToolExecutionUpdate {
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            args: serde_json::json!({"command": "printf hi"}),
            partial_result: serde_json::json!({
                "content": [{"type": "text", "text": "hi"}],
                "details": {"progress": "halfway"}
            }),
        };
        let update_rendered =
            render_tool_execution_event(&update, options(false)).expect("update should render");
        assert!(update_rendered.starts_with("⏳ **$ printf hi**"));
        assert!(update_rendered.contains("hi"));
        assert!(update_rendered.contains("Details: progress=`halfway`"));
        assert!(!update_rendered.contains("\"content\""));
        assert!(!update_rendered.contains("```json"));

        let end = RichAgentEvent::ToolExecutionEnd {
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            result: pi_agent::tools::AgentToolResult::output("done"),
            is_error: false,
        };
        let end_rendered =
            render_tool_execution_event(&end, options(false)).expect("end should render");
        assert!(end_rendered.starts_with("✓ **bash**"));
        assert!(end_rendered.contains("done"));
        assert!(!end_rendered.contains("```json"));
        assert!(!end_rendered.contains("{\""));

        assert!(render_tool_execution_event(&RichAgentEvent::TurnStart, options(false)).is_none());
    }

    #[test]
    fn tool_results_render_status_specific_previews_and_human_details() {
        let bash = ToolResultMessage::new(
            "bash-1",
            "bash",
            vec![ContentBlock::text(
                (0..8)
                    .map(|index| format!("line {index}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )],
            false,
        )
        .with_details_usage_timestamp(
            None,
            Some(serde_json::json!({
                "fullOutputPath": "/tmp/full-output.log",
                "truncation": {
                    "truncated": true,
                    "truncatedBy": "lines",
                    "totalLines": 8,
                    "outputLines": 8
                }
            })),
            1,
        );
        let rendered = render_message_with_options(
            &AgentMessage::Core(Message::ToolResult(bash)),
            TranscriptRenderOptions {
                expand_tool_output: false,
                ..options(false)
            },
        )
        .expect("bash result should render")
        .1;
        assert!(rendered.starts_with("✓ **bash**"));
        assert!(!rendered.contains("line 0"));
        assert!(rendered.contains("line 7"));
        assert!(rendered.contains("3 earlier lines; expand to show all"));
        assert!(rendered.contains("Full output: /tmp/full-output.log"));
        assert!(rendered.contains("Truncated: showing 8 of 8 lines"));
        assert!(!rendered.contains("```json"));

        let read = ToolResultMessage::text("read-1", "read", "file contents", false);
        let collapsed = render_message_with_options(
            &AgentMessage::Core(Message::ToolResult(read.clone())),
            TranscriptRenderOptions {
                expand_tool_output: false,
                ..options(false)
            },
        )
        .unwrap()
        .1;
        assert!(collapsed.starts_with("✓ **read**"));
        assert!(!collapsed.contains("file contents"));
        let expanded = render_message_with_options(
            &AgentMessage::Core(Message::ToolResult(read)),
            options(false),
        )
        .unwrap()
        .1;
        assert!(expanded.contains("file contents"));

        let edit = ToolResultMessage::text(
            "edit-1",
            "edit",
            "Successfully replaced 1 block(s) in src/lib.rs.",
            false,
        )
        .with_details_usage_timestamp(
            None,
            Some(serde_json::json!({"diff": "- old\n+ new", "patch": "raw patch"})),
            1,
        );
        let edit_rendered = render_message_with_options(
            &AgentMessage::Core(Message::ToolResult(edit)),
            options(false),
        )
        .unwrap()
        .1;
        assert!(edit_rendered.starts_with("✓ **edit**"));
        assert!(edit_rendered.contains("\\- old  \n\\+ new"));
        assert!(!edit_rendered.contains("Successfully replaced"));
        assert!(!edit_rendered.contains("```json"));

        let error = ToolResultMessage::text("read-2", "read", "permission denied", true);
        let error_rendered = render_message_with_options(
            &AgentMessage::Core(Message::ToolResult(error)),
            options(false),
        )
        .unwrap()
        .1;
        assert!(error_rendered.starts_with("✗ **read**"));
        assert!(error_rendered.contains("permission denied"));
    }

    #[test]
    fn tool_output_preserves_literal_markdown_punctuation_in_the_tui() {
        let result = ToolResultMessage::text(
            "bash-1",
            "bash",
            "?? crates/pi-agent/tests/context_overflow_recovery.rs\n### literal heading\n- literal list item\n1. literal ordered item",
            false,
        );
        let rendered = render_message_with_options(
            &AgentMessage::Core(Message::ToolResult(result)),
            options(false),
        )
        .expect("tool result should render")
        .1;

        assert!(rendered.contains("context\\_overflow\\_recovery\\.rs"));
        assert!(rendered.contains("\\#\\#\\# literal heading"));

        let markdown = Markdown::new(rendered, 0, 0, plain_markdown_theme(), None, None);
        let visible = markdown.render(120).join("\n");
        assert!(
            visible.contains("context_overflow_recovery.rs"),
            "{visible}"
        );
        assert!(visible.contains("### literal heading"), "{visible}");
        assert!(visible.contains("- literal list item"), "{visible}");
        assert!(visible.contains("1. literal ordered item"), "{visible}");
    }

    #[test]
    fn transcript_builder_preserves_user_messages_and_cache_notices() {
        let messages = vec![
            AgentMessage::Core(Message::User(UserContent::string("hello", 1))),
            assistant(vec![ContentBlock::text("world")]),
        ];
        let assistant_timestamp = match &messages[1] {
            AgentMessage::Core(Message::Assistant(message)) => message.timestamp(),
            _ => unreachable!(),
        };
        let rendered = build_transcript_with_cache_notices(
            &messages,
            false,
            &[(assistant_timestamp, "cache miss".to_string())],
        );
        assert!(rendered.contains("hello"));
        assert!(!rendered.contains("### You"));
        assert!(rendered.contains("world"));
        assert!(rendered.contains("> cache miss"));
    }
}
