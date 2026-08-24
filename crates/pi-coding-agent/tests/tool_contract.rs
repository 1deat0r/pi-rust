//! Malformed-call contract coverage for every built-in coding-agent tool.

use std::sync::Arc;

use pi_agent::agent::user_text_prompt;
use pi_agent::rich_agent::{run_rich_agent_loop, RichAgentEvent, RichAgentLoopConfig};
use pi_agent::tools::AgentTool;
use pi_agent::types::AgentMessage;
use pi_ai::providers::{
    faux_assistant_message, FauxAssistantOptions, FauxProviderCore, FauxResponseStep,
    RegisterFauxProviderOptions,
};
use pi_ai::types::{ContentBlock, Message, StopReason};

fn scripted_stream(core: FauxProviderCore) -> pi_agent::agent::StreamFn {
    Arc::new(move |model, context| core.stream(model, context, None))
}

#[tokio::test]
async fn malformed_calls_fail_before_builtin_execution() {
    let cwd = std::env::temp_dir().join(format!(
        "pi-coding-agent-tool-contract-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&cwd).unwrap();

    let tools: Vec<AgentTool> = vec![
        pi_agent::tools::read_tool(cwd.display().to_string()),
        pi_agent::tools::write_tool(cwd.display().to_string()),
        pi_agent::tools::edit_tool(cwd.display().to_string()),
        pi_agent::tools::bash_tool(cwd.display().to_string()),
        pi_coding_agent::core::tools::ls_tool(cwd.display().to_string()),
        pi_coding_agent::core::tools::find_tool(cwd.display().to_string()),
        pi_coding_agent::core::tools::grep_tool(cwd.display().to_string()),
    ];

    let tool_calls = vec![
        ContentBlock::tool_call("read-invalid", "read", serde_json::json!({})),
        ContentBlock::tool_call(
            "write-invalid",
            "write",
            serde_json::json!({"path": "created.txt"}),
        ),
        ContentBlock::tool_call(
            "edit-invalid",
            "edit",
            serde_json::json!({"path": "missing.txt", "edits": []}),
        ),
        ContentBlock::tool_call("bash-invalid", "bash", serde_json::json!({})),
        ContentBlock::tool_call(
            "ls-invalid",
            "ls",
            serde_json::json!({"path": {"not": "a string"}}),
        ),
        ContentBlock::tool_call("find-invalid", "find", serde_json::json!({})),
        ContentBlock::tool_call(
            "grep-invalid",
            "grep",
            serde_json::json!({"pattern": {"not": "a string"}}),
        ),
    ];

    let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
    core.set_responses(vec![
        FauxResponseStep::Message(faux_assistant_message(
            tool_calls,
            FauxAssistantOptions {
                stop_reason: Some(StopReason::ToolUse),
                ..Default::default()
            },
        )),
        FauxResponseStep::Message(faux_assistant_message(
            vec![ContentBlock::text("done")],
            FauxAssistantOptions::default(),
        )),
    ]);
    let model = core.get_model(None).unwrap().clone();
    let config = RichAgentLoopConfig::new(model, scripted_stream(core), None);
    let mut context = pi_agent::agent::AgentContext::new(Some("test".into()), tools);
    let mut events = Vec::new();
    let messages = run_rich_agent_loop(
        vec![user_text_prompt("exercise malformed calls", 1)],
        &mut context,
        &config,
        &mut |event| events.push(event),
    )
    .await;

    let errors: Vec<String> = messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::Core(Message::ToolResult(result)) if result.is_error() => {
                Some(result.tool_name().to_string())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        errors,
        vec!["read", "write", "edit", "bash", "ls", "find", "grep"]
    );
    assert!(messages.iter().any(|message| {
        matches!(
            message,
            AgentMessage::Core(Message::Assistant(assistant))
                if assistant.content().iter().any(|content| {
                    matches!(content, ContentBlock::Text { text, .. } if text == "done")
                })
        )
    }));

    let error_end_names: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            RichAgentEvent::ToolExecutionEnd {
                tool_name,
                is_error: true,
                ..
            } => Some(tool_name.clone()),
            _ => None,
        })
        .collect();
    let mut sorted_error_end_names = error_end_names;
    let mut sorted_errors = errors.clone();
    sorted_error_end_names.sort();
    sorted_errors.sort();
    assert_eq!(sorted_error_end_names, sorted_errors);
    assert!(!cwd.join("created.txt").exists());
    let _ = std::fs::remove_dir_all(cwd);
}
