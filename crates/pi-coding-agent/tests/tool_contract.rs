#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

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

#[tokio::test]
async fn read_tool_executes_a_real_unicode_range_through_the_agent_loop() {
    let cwd = std::env::temp_dir().join(format!(
        "pi-coding-agent-read-contract-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("fixture.txt"), "zero\n界🙂e\u{301}\nlast\n").unwrap();

    let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
    core.set_responses(vec![
        FauxResponseStep::Message(faux_assistant_message(
            vec![ContentBlock::tool_call(
                "read-real",
                "read",
                serde_json::json!({"path": "fixture.txt", "offset": 2, "limit": 1}),
            )],
            FauxAssistantOptions {
                stop_reason: Some(StopReason::ToolUse),
                ..Default::default()
            },
        )),
        FauxResponseStep::Message(faux_assistant_message(
            vec![ContentBlock::text("read complete")],
            FauxAssistantOptions::default(),
        )),
    ]);
    let model = core.get_model(None).unwrap().clone();
    let config = RichAgentLoopConfig::new(model, scripted_stream(core), None);
    let mut context = pi_agent::agent::AgentContext::new(
        Some("test".into()),
        vec![pi_agent::tools::read_tool(cwd.display().to_string())],
    );
    let messages = run_rich_agent_loop(
        vec![user_text_prompt("read the fixture", 1)],
        &mut context,
        &config,
        &mut |_| {},
    )
    .await;

    assert!(messages.iter().any(|message| matches!(
        message,
        AgentMessage::Core(Message::ToolResult(result))
            if result.tool_call_id() == "read-real"
                && !result.is_error()
                && result.content().iter().any(|block| matches!(
                    block,
                    ContentBlock::Text { text, .. }
                        if text == "界🙂e\u{301}\n\n[2 more lines in file. Use offset=3 to continue.]"
                ))
    )));
    assert!(messages.iter().any(|message| matches!(
        message,
        AgentMessage::Core(Message::Assistant(assistant))
            if assistant.content().iter().any(|block| matches!(
                block,
                ContentBlock::Text { text, .. } if text == "read complete"
            ))
    )));
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn ls_tool_uses_locale_order_and_fractional_limit_through_the_agent_loop() {
    let cwd = std::env::temp_dir().join(format!(
        "pi-coding-agent-ls-contract-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&cwd).unwrap();
    for name in ["_under", "-dash", ".hidden", "😀"] {
        std::fs::write(cwd.join(name), "").unwrap();
    }

    let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
    core.set_responses(vec![
        FauxResponseStep::Message(faux_assistant_message(
            vec![ContentBlock::tool_call(
                "ls-real",
                "ls",
                serde_json::json!({"limit": 2.5}),
            )],
            FauxAssistantOptions {
                stop_reason: Some(StopReason::ToolUse),
                ..Default::default()
            },
        )),
        FauxResponseStep::Message(faux_assistant_message(
            vec![ContentBlock::text("ls complete")],
            FauxAssistantOptions::default(),
        )),
    ]);
    let model = core.get_model(None).unwrap().clone();
    let config = RichAgentLoopConfig::new(model, scripted_stream(core), None);
    let mut context = pi_agent::agent::AgentContext::new(
        Some("test".into()),
        vec![pi_coding_agent::core::tools::ls_tool(
            cwd.display().to_string(),
        )],
    );
    let messages = run_rich_agent_loop(
        vec![user_text_prompt("list the fixture", 1)],
        &mut context,
        &config,
        &mut |_| {},
    )
    .await;

    assert!(messages.iter().any(|message| matches!(
        message,
        AgentMessage::Core(Message::ToolResult(result))
            if result.tool_call_id() == "ls-real"
                && !result.is_error()
                && result.content().iter().any(|block| matches!(
                    block,
                    ContentBlock::Text { text, .. }
                        if text == "_under\n-dash\n.hidden\n\n[2.5 entries limit reached. Use limit=5 for more]"
                ))
    )));
    assert!(messages.iter().any(|message| matches!(
        message,
        AgentMessage::Core(Message::Assistant(assistant))
            if assistant.content().iter().any(|block| matches!(
                block,
                ContentBlock::Text { text, .. } if text == "ls complete"
            ))
    )));
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn find_tool_matches_hidden_unicode_nested_paths_through_the_agent_loop() {
    let cwd = std::env::temp_dir().join(format!(
        "pi-coding-agent-find-contract-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(cwd.join("src").join("nested")).unwrap();
    std::fs::write(cwd.join("src").join("nested").join(".secret-界.rs"), "").unwrap();
    std::fs::write(cwd.join("src").join("outside.txt"), "").unwrap();

    let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
    core.set_responses(vec![
        FauxResponseStep::Message(faux_assistant_message(
            vec![ContentBlock::tool_call(
                "find-real",
                "find",
                serde_json::json!({"pattern": "src/**/*.rs", "limit": 10}),
            )],
            FauxAssistantOptions {
                stop_reason: Some(StopReason::ToolUse),
                ..Default::default()
            },
        )),
        FauxResponseStep::Message(faux_assistant_message(
            vec![ContentBlock::text("find complete")],
            FauxAssistantOptions::default(),
        )),
    ]);
    let model = core.get_model(None).unwrap().clone();
    let config = RichAgentLoopConfig::new(model, scripted_stream(core), None);
    let mut context = pi_agent::agent::AgentContext::new(
        Some("test".into()),
        vec![pi_coding_agent::core::tools::find_tool(
            cwd.display().to_string(),
        )],
    );
    let messages = run_rich_agent_loop(
        vec![user_text_prompt("find the fixture", 1)],
        &mut context,
        &config,
        &mut |_| {},
    )
    .await;

    assert!(messages.iter().any(|message| matches!(
        message,
        AgentMessage::Core(Message::ToolResult(result))
            if result.tool_call_id() == "find-real"
                && !result.is_error()
                && result.content().iter().any(|block| matches!(
                    block,
                    ContentBlock::Text { text, .. } if text == "src/nested/.secret-界.rs"
                ))
    )));
    assert!(messages.iter().any(|message| matches!(
        message,
        AgentMessage::Core(Message::Assistant(assistant))
            if assistant.content().iter().any(|block| matches!(
                block,
                ContentBlock::Text { text, .. } if text == "find complete"
            ))
    )));
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn grep_tool_matches_hidden_unicode_context_through_the_agent_loop() {
    let cwd = std::env::temp_dir().join(format!(
        "pi-coding-agent-grep-contract-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join(".秘密.txt"), "before\nneedle 界🙂\nafter\n").unwrap();

    let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
    core.set_responses(vec![
        FauxResponseStep::Message(faux_assistant_message(
            vec![ContentBlock::tool_call(
                "grep-real",
                "grep",
                serde_json::json!({
                    "pattern": "needle 界🙂",
                    "glob": "*.txt",
                    "literal": true,
                    "context": 1
                }),
            )],
            FauxAssistantOptions {
                stop_reason: Some(StopReason::ToolUse),
                ..Default::default()
            },
        )),
        FauxResponseStep::Message(faux_assistant_message(
            vec![ContentBlock::text("grep complete")],
            FauxAssistantOptions::default(),
        )),
    ]);
    let model = core.get_model(None).unwrap().clone();
    let config = RichAgentLoopConfig::new(model, scripted_stream(core), None);
    let mut context = pi_agent::agent::AgentContext::new(
        Some("test".into()),
        vec![pi_coding_agent::core::tools::grep_tool(
            cwd.display().to_string(),
        )],
    );
    let messages = run_rich_agent_loop(
        vec![user_text_prompt("grep the fixture", 1)],
        &mut context,
        &config,
        &mut |_| {},
    )
    .await;

    assert!(messages.iter().any(|message| matches!(
        message,
        AgentMessage::Core(Message::ToolResult(result))
            if result.tool_call_id() == "grep-real"
                && !result.is_error()
                && result.content().iter().any(|block| matches!(
                    block,
                    ContentBlock::Text { text, .. }
                        if text == ".秘密.txt-1- before\n.秘密.txt:2: needle 界🙂\n.秘密.txt-3- after"
                ))
    )));
    assert!(messages.iter().any(|message| matches!(
        message,
        AgentMessage::Core(Message::Assistant(assistant))
            if assistant.content().iter().any(|block| matches!(
                block,
                ContentBlock::Text { text, .. } if text == "grep complete"
            ))
    )));
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn write_tool_creates_and_overwrites_unicode_through_the_agent_loop() {
    let cwd = std::env::temp_dir().join(format!(
        "pi-coding-agent-write-contract-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&cwd).unwrap();

    let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
    core.set_responses(vec![
        FauxResponseStep::Message(faux_assistant_message(
            vec![ContentBlock::tool_call(
                "write-create",
                "write",
                serde_json::json!({"path": "nested/fixture.txt", "content": "old"}),
            )],
            FauxAssistantOptions {
                stop_reason: Some(StopReason::ToolUse),
                ..Default::default()
            },
        )),
        FauxResponseStep::Message(faux_assistant_message(
            vec![ContentBlock::tool_call(
                "write-overwrite",
                "write",
                serde_json::json!({"path": "nested/fixture.txt", "content": "界🙂"}),
            )],
            FauxAssistantOptions {
                stop_reason: Some(StopReason::ToolUse),
                ..Default::default()
            },
        )),
        FauxResponseStep::Message(faux_assistant_message(
            vec![ContentBlock::text("write complete")],
            FauxAssistantOptions::default(),
        )),
    ]);
    let model = core.get_model(None).unwrap().clone();
    let config = RichAgentLoopConfig::new(model, scripted_stream(core), None);
    let mut context = pi_agent::agent::AgentContext::new(
        Some("test".into()),
        vec![pi_agent::tools::write_tool(cwd.display().to_string())],
    );
    let messages = run_rich_agent_loop(
        vec![user_text_prompt("write the fixture", 1)],
        &mut context,
        &config,
        &mut |_| {},
    )
    .await;

    let results: Vec<&pi_ai::types::ToolResultMessage> = messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::Core(Message::ToolResult(result)) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results.iter().any(|result| {
        result.tool_call_id() == "write-create"
            && result.content().iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::Text { text, .. }
                        if text == "Successfully wrote 3 bytes to nested/fixture.txt"
                )
            })
    }));
    assert!(results.iter().any(|result| {
        result.tool_call_id() == "write-overwrite"
            && result.content().iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::Text { text, .. }
                        if text == "Successfully wrote 3 bytes to nested/fixture.txt"
                )
            })
    }));
    assert_eq!(
        std::fs::read_to_string(cwd.join("nested/fixture.txt")).unwrap(),
        "界🙂"
    );
    assert!(messages.iter().any(|message| matches!(
        message,
        AgentMessage::Core(Message::Assistant(assistant))
            if assistant.content().iter().any(|block| matches!(
                block,
                ContentBlock::Text { text, .. } if text == "write complete"
            ))
    )));
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn edit_tool_applies_disjoint_unicode_edits_through_the_agent_loop() {
    let cwd = std::env::temp_dir().join(format!(
        "pi-coding-agent-edit-contract-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("fixture.txt"), "alpha\n界🙂\nomega\n").unwrap();

    let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
    core.set_responses(vec![
        FauxResponseStep::Message(faux_assistant_message(
            vec![ContentBlock::tool_call(
                "edit-real",
                "edit",
                serde_json::json!({
                    "path": "fixture.txt",
                    "edits": [
                        {"oldText": "alpha", "newText": "ALPHA"},
                        {"oldText": "界🙂", "newText": "世界🙂"}
                    ]
                }),
            )],
            FauxAssistantOptions {
                stop_reason: Some(StopReason::ToolUse),
                ..Default::default()
            },
        )),
        FauxResponseStep::Message(faux_assistant_message(
            vec![ContentBlock::text("edit complete")],
            FauxAssistantOptions::default(),
        )),
    ]);
    let model = core.get_model(None).unwrap().clone();
    let config = RichAgentLoopConfig::new(model, scripted_stream(core), None);
    let mut context = pi_agent::agent::AgentContext::new(
        Some("test".into()),
        vec![pi_agent::tools::edit_tool(cwd.display().to_string())],
    );
    let messages = run_rich_agent_loop(
        vec![user_text_prompt("edit the fixture", 1)],
        &mut context,
        &config,
        &mut |_| {},
    )
    .await;

    let result = messages
        .iter()
        .find_map(|message| match message {
            AgentMessage::Core(Message::ToolResult(result))
                if result.tool_call_id() == "edit-real" =>
            {
                Some(result)
            }
            _ => None,
        })
        .expect("edit result");
    assert!(!result.is_error());
    assert!(result.content().iter().any(|block| matches!(
        block,
        ContentBlock::Text { text, .. }
            if text == "Successfully replaced 2 block(s) in fixture.txt."
    )));
    assert_eq!(result.details().unwrap()["firstChangedLine"], 1);
    assert_eq!(
        std::fs::read_to_string(cwd.join("fixture.txt")).unwrap(),
        "ALPHA\n世界🙂\nomega\n"
    );
    assert!(messages.iter().any(|message| matches!(
        message,
        AgentMessage::Core(Message::Assistant(assistant))
            if assistant.content().iter().any(|block| matches!(
                block,
                ContentBlock::Text { text, .. } if text == "edit complete"
            ))
    )));
    let _ = std::fs::remove_dir_all(cwd);
}
