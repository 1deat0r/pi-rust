#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_agent::fs::MemoryFs;
use pi_agent::harness::{AgentHarness, AgentHarnessOptions, HarnessTool};
use pi_agent::rich_agent::{AfterToolCallResult, BeforeToolCallResult, RichAgentEvent};
use pi_agent::session::memory::{in_memory_metadata, InMemorySessionStorage};
use pi_agent::session::Session;
use pi_agent::tools::{AgentTool, AgentToolResult};
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

fn tool_turn() -> FauxResponseStep {
    FauxResponseStep::Message(faux_assistant_message(
        vec![ContentBlock::tool_call(
            "hook-call",
            "hooked",
            serde_json::json!({"value": "original"}),
        )],
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

fn content_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_result(messages: &[AgentMessage]) -> &pi_ai::types::ToolResultMessage {
    messages
        .iter()
        .find_map(|message| match message {
            AgentMessage::Core(Message::ToolResult(result)) => Some(result),
            _ => None,
        })
        .expect("tool result")
}

fn hook_tool(order: Arc<Mutex<Vec<String>>>) -> AgentTool {
    AgentTool::new(
        pi_ai::types::json_tool(
            "hooked",
            "hook lifecycle fixture",
            &serde_json::json!({
                "type": "object",
                "properties": {"value": {"type": "string"}},
                "required": ["value"]
            }),
        ),
        "hooked",
        Arc::new(move |_, arguments, _, _| {
            let order = order.clone();
            let value = arguments["value"].as_str().unwrap().to_string();
            Box::pin(async move {
                order.lock().unwrap().push(format!("tool:{value}"));
                Ok(AgentToolResult::output(format!("tool:{value}")))
            })
        }),
    )
}

fn harness_options(
    id: &str,
    responses: Vec<FauxResponseStep>,
    tool: AgentTool,
) -> AgentHarnessOptions<MemoryFs> {
    let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
    core.set_responses(responses);
    let model = core.get_model(None).expect("faux model").clone();
    let stream_fn = Arc::new(
        move |model: &pi_ai::model::Model, context: &pi_ai::types::Context| {
            core.stream(model, context, None)
        },
    );
    let mut options = AgentHarnessOptions::new(session(id), model);
    options.stream_fn = Some(stream_fn);
    options.tools = Some(vec![HarnessTool::from_agent_tool(&tool)]);
    options
}

#[tokio::test(flavor = "current_thread")]
async fn public_harness_hooks_mutate_arguments_replace_results_and_preserve_order() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let tool = hook_tool(order.clone());
    let mut options = harness_options(
        "hook-mutation",
        vec![tool_turn(), final_turn("follow-up")],
        tool,
    );
    let before_order = order.clone();
    options.before_tool_call = Some(Arc::new(move |context, _| {
        before_order.lock().unwrap().push("before".to_string());
        context.args["value"] = serde_json::json!("mutated");
        Box::pin(async { None::<BeforeToolCallResult> })
    }));
    let after_order = order.clone();
    options.after_tool_call = Some(Arc::new(move |context, _| {
        after_order.lock().unwrap().push(format!(
            "after:{}:{}",
            context.args["value"],
            content_text(context.result.content())
        ));
        Box::pin(async {
            Some(AfterToolCallResult {
                content: Some(vec![ContentBlock::text("after replacement")]),
                details: Some(serde_json::json!({"hooked": true})),
                is_error: Some(false),
                ..Default::default()
            })
        })
    }));

    let (harness, suspended) = AgentHarness::create(options).await.expect("create harness");
    assert!(suspended.is_empty());
    let (messages, events) = harness
        .run_prompt_with_events(vec![user("run hook")])
        .await
        .expect("hooked run");

    assert_eq!(
        *order.lock().unwrap(),
        ["before", "tool:mutated", "after:\"mutated\":tool:mutated"]
    );
    let result = tool_result(&messages);
    assert_eq!(content_text(result.content()), "after replacement");
    assert_eq!(result.details(), Some(&serde_json::json!({"hooked": true})));
    assert!(!result.is_error());
    let lifecycle = events
        .iter()
        .filter_map(|event| match event {
            RichAgentEvent::ToolExecutionStart { .. } => Some("start"),
            RichAgentEvent::ToolExecutionEnd { .. } => Some("end"),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(lifecycle, ["start", "end"]);
}

#[tokio::test(flavor = "current_thread")]
async fn aborted_hook_cleans_up_and_the_public_harness_runs_again() {
    let entered = Arc::new(AtomicBool::new(false));
    let tool = hook_tool(Arc::new(Mutex::new(Vec::new())));
    let mut options = harness_options(
        "hook-abort-reuse",
        vec![tool_turn(), final_turn("second run succeeds")],
        tool,
    );
    let hook_entered = entered.clone();
    options.before_tool_call = Some(Arc::new(move |_, signal| {
        let hook_entered = hook_entered.clone();
        Box::pin(async move {
            hook_entered.store(true, Ordering::SeqCst);
            while !signal
                .as_ref()
                .is_some_and(|signal| signal.load(Ordering::SeqCst))
            {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            None
        })
    }));

    let (harness, _) = AgentHarness::create(options).await.expect("create harness");
    let harness = Arc::new(harness);
    let running = harness.clone();
    let first = tokio::spawn(async move {
        running
            .run_prompt_with_events(vec![user("abort hook")])
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while !entered.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("before hook entered");
    harness.agent_handle().expect("agent handle").abort();
    let (first_messages, _) = tokio::time::timeout(Duration::from_secs(1), first)
        .await
        .expect("aborted run settled")
        .expect("run task")
        .expect("aborted run result");
    let first_result = tool_result(&first_messages);
    assert!(first_result.is_error());
    assert_eq!(content_text(first_result.content()), "Operation aborted");

    let second = harness
        .run_prompt(vec![user("reuse after abort")])
        .await
        .expect("subsequent run");
    assert!(second.iter().any(|message| matches!(
        message,
        AgentMessage::Core(Message::Assistant(assistant))
            if content_text(assistant.content()) == "second run succeeds"
    )));
}

#[tokio::test(flavor = "current_thread")]
async fn lane_agents_inherit_hooks_and_contain_hook_panics() {
    let invoked = Arc::new(AtomicBool::new(false));
    let tool = hook_tool(Arc::new(Mutex::new(Vec::new())));
    let mut options = harness_options(
        "lane-hook-panic",
        vec![tool_turn(), final_turn("lane recovered")],
        tool,
    );
    let hook_invoked = invoked.clone();
    options.before_tool_call = Some(Arc::new(move |_, _| {
        hook_invoked.store(true, Ordering::SeqCst);
        panic!("public lane hook panic")
    }));

    let (harness, _) = AgentHarness::create(options).await.expect("create harness");
    let lane = harness
        .create_lane("worker", None)
        .await
        .expect("create lane");
    lane.prompt_messages(&[user("run panicking lane hook")])
        .await
        .expect("hook panic is contained as a tool result");
    assert!(invoked.load(Ordering::SeqCst));
}
