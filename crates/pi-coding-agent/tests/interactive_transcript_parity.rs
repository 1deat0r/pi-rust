#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

use std::sync::{Arc, Mutex};

use pi_agent::rich_agent::RichAgentEvent;
use pi_agent::tools::AgentToolResult;
use pi_agent::types::AgentMessage;
use pi_ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, DoneReason, ErrorReason, Message,
    StopReason, ToolResultMessage, UserContent,
};
use pi_coding_agent::interactive::messages::{
    build_transcript, build_transcript_with_event, render_assistant_event_segments_with_options,
    render_assistant_event_with_options, render_message_with_options, render_tool_execution_event,
    AssistantTranscriptSegment, TranscriptRenderOptions,
};
use pi_coding_agent::interactive::{
    build_interactive_scene_with_loader_and_scroll_view, new_interactive_document_scroll_view,
};
use pi_tui::components::{Editor, EditorOptions, EditorTheme, Markdown, Text};
use pi_tui::{render_layout_frame, SharedComponent};

fn assistant(content: Vec<ContentBlock>) -> AssistantMessage {
    let mut message = AssistantMessage::new();
    message.set_content(content);
    message
}

fn render_options() -> TranscriptRenderOptions {
    TranscriptRenderOptions {
        show_images: false,
        ..Default::default()
    }
}

#[test]
fn assistant_stream_deltas_render_current_snapshot_and_preserve_multiline_text() {
    let start = AssistantMessageEvent::Start {
        partial: assistant(Vec::new()),
    };
    assert_eq!(
        render_assistant_event_with_options(&start, render_options()),
        None
    );

    let first_partial = assistant(vec![ContentBlock::text("  first line\nsecond line  ")]);
    let first = AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: "first".to_string(),
        partial: first_partial,
    };
    assert_eq!(
        render_assistant_event_with_options(&first, render_options()).as_deref(),
        Some("first line\nsecond line")
    );

    let second_partial = assistant(vec![
        ContentBlock::text("first line\nsecond line\nthird line"),
        ContentBlock::thinking("first thought"),
        ContentBlock::thinking("second thought"),
    ]);
    let second = AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: "third".to_string(),
        partial: second_partial,
    };
    let rendered = render_assistant_event_with_options(&second, render_options())
        .expect("streaming snapshot should render");
    assert!(rendered.contains("first line\nsecond line\nthird line"));
    assert!(pi_tui::strip_ansi_codes(&rendered).contains("> first thought\n>\n> second thought"));
    assert!(!rendered.contains("\"content\""));

    let done_message = assistant(vec![ContentBlock::text("final answer")]);
    let done = AssistantMessageEvent::Done {
        reason: DoneReason::Stop,
        message: done_message,
    };
    assert_eq!(
        render_assistant_event_with_options(&done, render_options()).as_deref(),
        Some("final answer")
    );
}

#[test]
fn tool_lifecycle_renders_call_update_result_without_json_envelopes() {
    let start = RichAgentEvent::ToolExecutionStart {
        tool_call_id: "call-1".to_string(),
        tool_name: "bash".to_string(),
        args: serde_json::json!({"command": "printf hi"}),
    };
    assert_eq!(
        render_tool_execution_event(&start, render_options()).as_deref(),
        Some("⏳ **$ printf hi**")
    );

    let update = RichAgentEvent::ToolExecutionUpdate {
        tool_call_id: "call-1".to_string(),
        tool_name: "bash".to_string(),
        args: serde_json::json!({"command": "printf hi"}),
        partial_result: serde_json::json!({
            "content": [{"type": "text", "text": "partial output"}],
            "details": {"phase": "running"}
        }),
    };
    let update_rendered = render_tool_execution_event(&update, render_options())
        .expect("partial tool update should render");
    assert!(update_rendered.starts_with("⏳ **$ printf hi**"));
    assert!(update_rendered.contains("partial output"));
    assert!(update_rendered.contains("Details: phase=`running`"));
    assert!(!update_rendered.contains("\"content\""));
    assert!(!update_rendered.contains("```json"));

    let end = RichAgentEvent::ToolExecutionEnd {
        tool_call_id: "call-1".to_string(),
        tool_name: "bash".to_string(),
        result: AgentToolResult::output("final output"),
        is_error: false,
    };
    let end_rendered = render_tool_execution_event(&end, render_options())
        .expect("completed tool result should render");
    assert!(end_rendered.starts_with("✓ **bash**"));
    assert!(end_rendered.contains("final output"));
    assert!(!end_rendered.contains("\"content\""));
    assert!(render_tool_execution_event(&RichAgentEvent::TurnStart, render_options()).is_none());
}

#[test]
fn assistant_terminal_states_match_upstream_interruption_and_error_rules() {
    let mut aborted = assistant(Vec::new());
    aborted.set_stop_reason(StopReason::Aborted);
    assert_eq!(
        render_message_with_options(
            &AgentMessage::Core(Message::Assistant(aborted)),
            render_options()
        )
        .map(|(_, text)| text),
        Some("Operation aborted".to_string())
    );

    let mut custom_abort = assistant(vec![ContentBlock::text("partial answer")]);
    custom_abort.set_stop_reason(StopReason::Aborted);
    custom_abort.set_error_message("cancelled by provider");
    let custom_abort_rendered = render_message_with_options(
        &AgentMessage::Core(Message::Assistant(custom_abort)),
        render_options(),
    )
    .expect("custom abort should remain visible")
    .1;
    assert!(custom_abort_rendered.ends_with("cancelled by provider"));
    assert!(!custom_abort_rendered.contains("Error:"));
    assert!(!custom_abort_rendered.contains("operation aborted"));

    let mut error = assistant(Vec::new());
    error.set_stop_reason(StopReason::Error);
    error.set_error_message("provider unavailable");
    let error_rendered = render_message_with_options(
        &AgentMessage::Core(Message::Assistant(error)),
        render_options(),
    )
    .expect("empty error should remain visible")
    .1;
    assert_eq!(error_rendered, "Error: provider unavailable");

    let event_message = assistant(Vec::new());
    let event = AssistantMessageEvent::Error {
        reason: ErrorReason::Aborted,
        error_message: event_message,
    };
    assert_eq!(
        render_assistant_event_with_options(&event, render_options()).as_deref(),
        Some("Operation aborted")
    );

    let mut tool_error = assistant(vec![ContentBlock::tool_call(
        "call-2",
        "read",
        serde_json::json!({"path": "file.txt"}),
    )]);
    tool_error.set_stop_reason(StopReason::Error);
    tool_error.set_error_message("tool failed");
    let tool_error_rendered = render_message_with_options(
        &AgentMessage::Core(Message::Assistant(tool_error)),
        render_options(),
    )
    .expect("tool call remains visible")
    .1;
    assert!(tool_error_rendered.contains("⏳ **read**"));
    assert!(!tool_error_rendered.contains("tool failed"));
}

#[test]
fn segmented_tool_turn_keeps_the_length_notice_after_tool_children() {
    let mut message = assistant(vec![
        ContentBlock::text("partial answer"),
        ContentBlock::tool_call("call-1", "read", serde_json::json!({"path": "file.txt"})),
    ]);
    message.set_stop_reason(StopReason::Length);

    let event = AssistantMessageEvent::Done {
        reason: DoneReason::Length,
        message,
    };
    let segments = render_assistant_event_segments_with_options(&event, render_options());

    assert_eq!(
        segments,
        vec![
            AssistantTranscriptSegment::Markdown("partial answer".to_string()),
            AssistantTranscriptSegment::ToolCall("call-1".to_string()),
            AssistantTranscriptSegment::Markdown(
                "Response was truncated before completion.".to_string(),
            ),
        ]
    );
}

#[test]
fn transcript_turn_sequence_keeps_multiline_user_prompt_and_live_assistant_at_tail() {
    let messages = vec![
        AgentMessage::Core(Message::User(UserContent::string(
            "first line\nsecond line",
            1,
        ))),
        AgentMessage::Core(Message::Assistant(assistant(vec![ContentBlock::text(
            "first response",
        )]))),
    ];
    let partial = assistant(vec![ContentBlock::text("second response")]);
    let event = AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: "second".to_string(),
        partial,
    };
    let rendered = build_transcript_with_event(&messages, &event, render_options());
    assert!(rendered.contains("first line\nsecond line"));
    assert!(rendered.contains("first response"));
    assert!(rendered.ends_with("second response\n"));
    assert!(rendered.find("first response").unwrap() < rendered.rfind("second response").unwrap());
    assert!(!rendered.contains("### You"));
}

#[test]
fn interactive_scene_keeps_transcript_above_fixed_composer_dock() {
    let transcript = Arc::new(Mutex::new(Markdown::new(
        "transcript",
        1,
        0,
        pi_coding_agent::interactive::tui_theme::markdown_theme(),
        None,
        None,
    )));
    let (transcript_scroll_view, _) = new_interactive_document_scroll_view(&transcript);
    let editor = Arc::new(Mutex::new(Editor::new(
        24,
        EditorTheme {
            border_color: Arc::new(|line| line.to_string()),
        },
        EditorOptions::default(),
    )));
    editor
        .lock()
        .unwrap()
        .set_text("first prompt line\nsecond prompt line");
    let footer = Arc::new(Mutex::new(Text::new("footer", 0, 0, None)));
    let status = Arc::new(Mutex::new(Text::new("status", 1, 0, None)));
    let loader = Arc::new(Mutex::new(pi_tui::components::Loader::new("")));

    let scene = build_interactive_scene_with_loader_and_scroll_view(
        &transcript_scroll_view,
        &editor,
        &footer,
        Some(&status),
        None,
        &[],
        &loader,
        "",
    );
    let frame = render_layout_frame(scene as SharedComponent, 80, 20);
    assert_eq!(frame.root.children.len(), 2);
    assert!(frame.root.children[0].rect.y < frame.root.children[1].rect.y);
    assert!(frame.root.children[1].rect.height >= 3);
    assert!(frame
        .lines
        .iter()
        .any(|line| line.contains("first prompt line")));
    assert!(frame
        .lines
        .iter()
        .any(|line| line.contains("second prompt line")));
    assert_eq!(
        frame
            .primary_scroll_view
            .as_ref()
            .expect("transcript should be the primary scroll view")
            .viewport_height(),
        frame.root.children[0].rect.height
    );
}

#[test]
fn tool_result_message_constructor_remains_transcript_compatible() {
    let result = ToolResultMessage::text("call-3", "read", "line one\nline two", false);
    let rendered = build_transcript(&[AgentMessage::Core(Message::ToolResult(result))], false);
    assert!(rendered.starts_with("✓ **read**"));
    assert!(rendered.contains("line one"));
    assert!(rendered.contains("line two"));
    assert!(!rendered.contains("\"tool_call_id\""));
}
