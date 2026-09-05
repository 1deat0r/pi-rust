#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_agent::fs::MemoryFs;
use pi_agent::harness::{AgentHarness, AgentHarnessOptions, HarnessTool};
use pi_agent::rich_agent::RichAgentEvent;
use pi_agent::session::memory::{in_memory_metadata, InMemorySessionStorage};
use pi_agent::session::Session;
use pi_agent::tools::{AgentTool, AgentToolResult, ToolExecutionMode};
use pi_agent::types::AgentMessage;
use pi_ai::providers::{
    faux_assistant_message, FauxAssistantOptions, FauxProviderCore, FauxResponseStep,
    RegisterFauxProviderOptions,
};
use pi_ai::types::{ContentBlock, Message, StopReason, UserContent};

fn session(id: &str) -> Session<MemoryFs> {
    let storage = Arc::new(Mutex::new(InMemorySessionStorage::new(in_memory_metadata(
        id, None,
    ))));
    Session::from_in_memory(storage)
}

fn user(text: &str) -> AgentMessage {
    AgentMessage::Core(Message::User(UserContent::string(text, 1)))
}

fn scripted_tool_turn(calls: &[(&str, &str)]) -> FauxResponseStep {
    FauxResponseStep::Message(faux_assistant_message(
        calls
            .iter()
            .map(|(id, value)| {
                ContentBlock::tool_call(*id, "delayed", serde_json::json!({"value": value}))
            })
            .collect(),
        FauxAssistantOptions {
            stop_reason: Some(StopReason::ToolUse),
            ..Default::default()
        },
    ))
}

fn final_turn(text: &str) -> FauxResponseStep {
    FauxResponseStep::Message(faux_assistant_message(
        vec![ContentBlock::text(text)],
        FauxAssistantOptions::default(),
    ))
}

fn delayed_tool(sequential: bool, terminate: bool) -> AgentTool {
    let tool = AgentTool::new(
        pi_ai::types::json_tool(
            "delayed",
            "deterministic delayed tool",
            &serde_json::json!({
                "type": "object",
                "properties": {"value": {"type": "string"}},
                "required": ["value"]
            }),
        ),
        "delayed",
        Arc::new(move |_, arguments, _, _| {
            let value = arguments["value"].as_str().unwrap().to_string();
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(if value == "slow" { 40 } else { 5 }))
                    .await;
                Ok(AgentToolResult {
                    content: vec![ContentBlock::text(format!("done:{value}"))],
                    terminate,
                    ..Default::default()
                })
            })
        }),
    );
    if sequential {
        tool.with_execution_mode(ToolExecutionMode::Sequential)
    } else {
        tool
    }
}

async fn harness_run(
    id: &str,
    tool: AgentTool,
    responses: Vec<FauxResponseStep>,
) -> (
    Vec<AgentMessage>,
    Vec<RichAgentEvent>,
    Vec<pi_ai::types::Context>,
    usize,
) {
    let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
    core.set_responses(responses);
    let model = core.get_model(None).expect("faux model").clone();
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let captured_contexts = contexts.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let stream_calls = calls.clone();
    let stream_fn = Arc::new(
        move |model: &pi_ai::model::Model, context: &pi_ai::types::Context| {
            captured_contexts
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(context.clone());
            stream_calls.fetch_add(1, Ordering::SeqCst);
            core.stream(model, context, None)
        },
    );
    let mut options = AgentHarnessOptions::new(session(id), model);
    options.stream_fn = Some(stream_fn);
    options.tools = Some(vec![HarnessTool::from_agent_tool(&tool)]);
    let (harness, suspended) = AgentHarness::create(options).await.expect("create harness");
    assert!(suspended.is_empty());
    let (messages, events) = harness
        .run_prompt_with_events(vec![user("run tools")])
        .await
        .expect("run tool turn");
    let contexts = contexts
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    (messages, events, contexts, calls.load(Ordering::SeqCst))
}

fn tool_result_ids(messages: &[AgentMessage]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::Core(Message::ToolResult(result)) => {
                Some(result.tool_call_id().to_string())
            }
            _ => None,
        })
        .collect()
}

fn tool_end_ids(events: &[RichAgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            RichAgentEvent::ToolExecutionEnd { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .collect()
}

fn assistant_texts(messages: &[AgentMessage]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::Core(Message::Assistant(assistant)) => Some(
                assistant
                    .content()
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn parallel_tools_finish_concurrently_but_feed_results_to_follow_up_in_source_order() {
    let (messages, events, contexts, calls) = harness_run(
        "parallel-tools",
        delayed_tool(false, false),
        vec![
            scripted_tool_turn(&[("call-slow", "slow"), ("call-fast", "fast")]),
            final_turn("follow-up complete"),
        ],
    )
    .await;

    assert_eq!(calls, 2, "tool results must trigger one follow-up request");
    assert_eq!(tool_end_ids(&events), ["call-fast", "call-slow"]);
    assert_eq!(tool_result_ids(&messages), ["call-slow", "call-fast"]);
    assert_eq!(
        assistant_texts(&messages).last().unwrap(),
        "follow-up complete"
    );
    assert_eq!(contexts.len(), 2);
    assert_eq!(contexts[1].messages.len(), 4);
    let follow_up_result_ids = contexts[1]
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(result.tool_call_id()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(follow_up_result_ids, ["call-slow", "call-fast"]);
}

#[tokio::test(flavor = "current_thread")]
async fn per_tool_sequential_override_survives_the_public_harness_conversion() {
    let (messages, events, _, calls) = harness_run(
        "sequential-tools",
        delayed_tool(true, false),
        vec![
            scripted_tool_turn(&[("call-slow", "slow"), ("call-fast", "fast")]),
            final_turn("sequential follow-up"),
        ],
    )
    .await;

    assert_eq!(calls, 2);
    assert_eq!(tool_end_ids(&events), ["call-slow", "call-fast"]);
    assert_eq!(tool_result_ids(&messages), ["call-slow", "call-fast"]);
    assert_eq!(
        assistant_texts(&messages).last().unwrap(),
        "sequential follow-up"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn all_terminating_tool_results_stop_without_an_extra_provider_turn() {
    let (messages, events, _, calls) = harness_run(
        "terminating-tools",
        delayed_tool(false, true),
        vec![
            scripted_tool_turn(&[("terminate-slow", "slow"), ("terminate-fast", "fast")]),
            final_turn("must not run"),
        ],
    )
    .await;

    assert_eq!(calls, 1);
    assert_eq!(
        tool_result_ids(&messages),
        ["terminate-slow", "terminate-fast"]
    );
    assert_eq!(tool_end_ids(&events), ["terminate-fast", "terminate-slow"]);
    assert!(!assistant_texts(&messages)
        .iter()
        .any(|text| text == "must not run"));
}
