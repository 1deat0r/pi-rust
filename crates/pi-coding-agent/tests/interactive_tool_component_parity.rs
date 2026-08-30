#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Deterministic black-box coverage for the interactive tool-execution
//! component boundary.
//!
//! The pinned Pi implementation keeps a single ToolExecutionComponent alive
//! from call start through partial updates and the final result. Rust exposes
//! the same presentation through transcript render helpers, so these tests
//! assert the observable text contract: compact calls, lifecycle markers,
//! previews, notices, redaction, and stable block ordering.

use pi_agent::rich_agent::RichAgentEvent;
use pi_agent::tools::AgentToolResult;
use pi_agent::types::{AgentMessage, CustomAgentMessage};
use pi_ai::types::{AssistantMessage, ContentBlock, Message, ToolResultMessage};
use pi_coding_agent::interactive::messages::{
    render_message_with_options, render_tool_execution_event, TranscriptRenderOptions,
};
use pi_tui::components::markdown::plain_markdown_theme;
use pi_tui::components::Markdown;
use pi_tui::tui::Component;

const ONE_BY_ONE_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

fn options(expanded: bool) -> TranscriptRenderOptions {
    TranscriptRenderOptions {
        show_images: false,
        expand_tool_output: expanded,
        ..Default::default()
    }
}

fn assistant_with(content: Vec<ContentBlock>) -> AgentMessage {
    let mut message = AssistantMessage::new();
    message.set_content(content);
    AgentMessage::Core(Message::Assistant(message))
}

fn render(message: AgentMessage, expanded: bool) -> String {
    render_message_with_options(&message, options(expanded))
        .map(|(_, text)| text)
        .expect("message should have visible content")
}

fn tool_result(tool_call_id: &str, tool_name: &str, output: &str, is_error: bool) -> AgentMessage {
    AgentMessage::Core(Message::ToolResult(ToolResultMessage::text(
        tool_call_id,
        tool_name,
        output,
        is_error,
    )))
}

#[test]
fn built_in_calls_and_generic_fallback_are_compact_and_json_free() {
    let rendered = render(
        assistant_with(vec![
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
            ContentBlock::tool_call(
                "custom-1",
                "custom_tool",
                serde_json::json!({
                    "query": "needle",
                    "api_key": "sk-do-not-display",
                    "nested": {"password": "also-do-not-display", "mode": "fast"}
                }),
            ),
        ]),
        false,
    );

    for expected in [
        "⏳ **$ printf hi** (timeout 5s)",
        "⏳ **read** `src/main.rs`:3-5",
        "⏳ **write** `out.txt`",
        "⏳ **edit** `src/lib.rs` (2 replacements)",
        "⏳ **grep** /TODO/ in `src` (`*.rs`) limit 10",
        "⏳ **find**",
        "⏳ **ls** `src` (limit 20)",
        "⏳ **custom_tool** query=`needle`",
    ] {
        assert!(
            rendered.contains(expected),
            "missing {expected:?}: {rendered:?}"
        );
    }
    assert!(rendered.contains("api_key=[redacted]"));
    assert!(rendered.contains("password: [redacted]"));
    assert!(rendered.contains("mode: `fast`"));
    assert!(!rendered.contains("sk-do-not-display"));
    assert!(!rendered.contains("also-do-not-display"));
    assert!(!rendered.contains("\"command\""));
    assert!(!rendered.contains("\"file_path\""));
    assert!(!rendered.contains("{\"query\""));
    assert!(!rendered.contains("```json"));
}

#[test]
fn lifecycle_start_update_end_keeps_running_state_and_human_result_details() {
    let start = RichAgentEvent::ToolExecutionStart {
        tool_call_id: "call-1".to_string(),
        tool_name: "bash".to_string(),
        args: serde_json::json!({"command": "printf hi"}),
    };
    assert_eq!(
        render_tool_execution_event(&start, options(false)).as_deref(),
        Some("⏳ **$ printf hi**")
    );

    let update = RichAgentEvent::ToolExecutionUpdate {
        tool_call_id: "call-1".to_string(),
        tool_name: "bash".to_string(),
        args: serde_json::json!({"command": "printf hi"}),
        partial_result: serde_json::json!({
            "content": [{"type": "text", "text": "partial output"}],
            "details": {"phase": "running"},
            "addedToolNames": ["custom_tool"]
        }),
    };
    let update_rendered =
        render_tool_execution_event(&update, options(false)).expect("partial update should render");
    assert!(update_rendered.starts_with("⏳ **$ printf hi**"));
    assert!(update_rendered.contains("partial output"));
    assert!(update_rendered.contains("Details: phase=`running`"));
    assert!(update_rendered.contains("**Tools added:** custom_tool"));
    assert!(!update_rendered.contains("\"content\""));
    assert!(!update_rendered.contains("```json"));

    let mut success = AgentToolResult::output("final output");
    success.details = Some(serde_json::json!({
        "fullOutputPath": "/tmp/full-output.log",
        "truncation": {
            "truncated": true,
            "truncatedBy": "lines",
            "outputLines": 5,
            "totalLines": 9
        }
    }));
    let end = RichAgentEvent::ToolExecutionEnd {
        tool_call_id: "call-1".to_string(),
        tool_name: "bash".to_string(),
        result: success,
        is_error: false,
    };
    let end_rendered =
        render_tool_execution_event(&end, options(false)).expect("successful end should render");
    assert!(end_rendered.starts_with("✓ **bash**"));
    assert!(end_rendered.contains("final output"));
    assert!(end_rendered.contains("Full output: /tmp/full-output.log"));
    assert!(end_rendered.contains("Truncated: showing 5 of 9 lines"));
    assert!(!end_rendered.contains("\"truncation\""));

    let failed = RichAgentEvent::ToolExecutionEnd {
        tool_call_id: "call-2".to_string(),
        tool_name: "read".to_string(),
        result: AgentToolResult::text("permission denied"),
        is_error: true,
    };
    let failed_rendered =
        render_tool_execution_event(&failed, options(false)).expect("error end should render");
    assert!(failed_rendered.starts_with("✗ **read**"));
    assert!(failed_rendered.contains("permission denied"));
    assert!(render_tool_execution_event(&RichAgentEvent::TurnStart, options(false)).is_none());
}

#[test]
fn collapsed_and_expanded_previews_follow_pi_tool_limits() {
    let lines = (0..22)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");

    let collapsed_bash = render(tool_result("bash-1", "bash", &lines, false), false);
    assert!(collapsed_bash.contains("line 17"));
    assert!(collapsed_bash.contains("line 21"));
    assert!(!collapsed_bash.contains("line 16"));
    assert!(collapsed_bash.contains("17 earlier lines; expand to show all"));

    let expanded_bash = render(tool_result("bash-1", "bash", &lines, false), true);
    assert!(expanded_bash.contains("line 0"));
    assert!(expanded_bash.contains("line 21"));
    assert!(!expanded_bash.contains("expand to show all"));

    let collapsed_grep = render(tool_result("grep-1", "grep", &lines, false), false);
    assert!(collapsed_grep.contains("line 0"));
    assert!(collapsed_grep.contains("line 14"));
    assert!(!collapsed_grep.contains("line 15"));
    assert!(collapsed_grep.contains("7 more lines; expand to show all"));

    let collapsed_read = render(tool_result("read-1", "read", "file contents", false), false);
    assert!(collapsed_read.starts_with("✓ **read**"));
    assert!(!collapsed_read.contains("file contents"));
    let expanded_read = render(tool_result("read-1", "read", "file contents", false), true);
    assert!(expanded_read.contains("file contents"));

    let write_lines = (0..12)
        .map(|index| format!("write line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let collapsed_write = render(
        assistant_with(vec![ContentBlock::tool_call(
            "write-1",
            "write",
            serde_json::json!({"path": "out.txt", "content": write_lines}),
        )]),
        false,
    );
    assert!(collapsed_write.contains("write line 0"));
    assert!(collapsed_write.contains("write line 9"));
    assert!(!collapsed_write.contains("write line 10"));
    assert!(collapsed_write.contains("2 more lines, 12 total, expand to show all"));

    let generic = render(tool_result("custom-1", "custom_tool", &lines, false), false);
    assert!(generic.contains("line 0"));
    assert!(generic.contains("line 9"));
    assert!(!generic.contains("line 10"));
    assert!(generic.contains("12 more lines; expand to show all"));
}

#[test]
fn details_cover_tool_limits_full_output_and_truncation_without_json() {
    let grep = ToolResultMessage::new("grep-1", "grep", vec![ContentBlock::text("match")], false)
        .with_details_usage_timestamp(
            None,
            Some(serde_json::json!({
                "matchLimitReached": 12,
                "linesTruncated": true,
                "truncation": {"truncated": true, "maxBytes": 2048}
            })),
            1,
        );
    let grep_rendered = render(AgentMessage::Core(Message::ToolResult(grep)), true);
    assert!(grep_rendered.contains("12 matches limit"));
    assert!(grep_rendered.contains("some lines truncated"));
    assert!(grep_rendered.contains("Truncated"));
    assert!(!grep_rendered.contains("\"matchLimitReached\""));

    let find = ToolResultMessage::new(
        "find-1",
        "find",
        vec![ContentBlock::text("src/main.rs")],
        false,
    )
    .with_details_usage_timestamp(
        None,
        Some(serde_json::json!({
            "resultLimitReached": 20,
            "truncation": {"truncated": true, "maxBytes": 50 * 1024}
        })),
        1,
    );
    let find_rendered = render(AgentMessage::Core(Message::ToolResult(find)), true);
    assert!(find_rendered.contains("20 results limit"));
    assert!(find_rendered.contains("50.0KB limit"));

    let ls = ToolResultMessage::new("ls-1", "ls", vec![ContentBlock::text("entry")], false)
        .with_details_usage_timestamp(None, Some(serde_json::json!({"entryLimitReached": 20})), 1);
    let ls_rendered = render(AgentMessage::Core(Message::ToolResult(ls)), true);
    assert!(ls_rendered.contains("20 entries limit"));
}

#[test]
fn image_blocks_follow_all_text_blocks_in_stable_order() {
    let result = ToolResultMessage::new(
        "call-1",
        "custom_tool",
        vec![
            ContentBlock::text("first text"),
            ContentBlock::image(ONE_BY_ONE_PNG, "image/png"),
            ContentBlock::text("second text"),
        ],
        false,
    );
    let rendered = render(AgentMessage::Core(Message::ToolResult(result)), true);
    let first = rendered.find("first text").expect("first text visible");
    let second = rendered.find("second text").expect("second text visible");
    let image = rendered
        .find("[Image: [image/png] 1x1]")
        .expect("image fallback visible");
    assert!(first < second, "text blocks must preserve their order");
    assert!(second < image, "images must follow the text section");

    // The source is consumed by the Markdown transcript renderer. Verify
    // escaped tool output still displays literal punctuation rather than
    // becoming Markdown structure.
    let markdown = Markdown::new(rendered, 0, 0, plain_markdown_theme(), None, None);
    let visible = markdown.render(120).join("\n");
    assert!(visible.contains("first text"));
    assert!(visible.contains("second text"));
    assert!(visible.contains("[Image: [image/png] 1x1]"));
}

#[test]
fn bash_execution_custom_messages_expose_running_success_error_and_truncation() {
    let running = AgentMessage::Custom(CustomAgentMessage::BashExecution {
        command: "printf hi".to_string(),
        output: "hi".to_string(),
        exit_code: None,
        cancelled: false,
        truncated: false,
        full_output_path: None,
        timestamp: 1,
        exclude_from_context: None,
    });
    let running_rendered = render(running, true);
    assert!(running_rendered.starts_with("**$ printf hi**"));
    assert!(running_rendered.contains("⏳ Running... (Esc to cancel)"));

    let success = AgentMessage::Custom(CustomAgentMessage::BashExecution {
        command: "printf hi".to_string(),
        output: "hi".to_string(),
        exit_code: Some(0),
        cancelled: false,
        truncated: true,
        full_output_path: Some("/tmp/full.log".to_string()),
        timestamp: 2,
        exclude_from_context: None,
    });
    let success_rendered = render(success, true);
    assert!(success_rendered.starts_with("**$ printf hi**"));
    assert!(success_rendered.contains("Output truncated. Full output: /tmp/full.log"));

    let error = AgentMessage::Custom(CustomAgentMessage::BashExecution {
        command: "false".to_string(),
        output: "stderr".to_string(),
        exit_code: Some(2),
        cancelled: false,
        truncated: false,
        full_output_path: None,
        timestamp: 3,
        exclude_from_context: None,
    });
    let error_rendered = render(error, true);
    assert!(error_rendered.starts_with("**$ false**"));
    assert!(error_rendered.contains("(exit 2)"));
}
