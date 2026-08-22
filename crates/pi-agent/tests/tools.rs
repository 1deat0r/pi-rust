//! Tool behavior tests: bash capture, read truncation messages, write/edit,
//! path normalization, and an agent-loop tool-call round trip.

use pi_agent::tools::AgentTool;
use pi_ai::types::ContentBlock;

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pi-agent-tools-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn bash_runs_command_and_reports_exit_code() {
    let rt = rt();
    rt.block_on(async {
        let dir = tmpdir("bash");
        let result = pi_agent::tools::bash::execute_bash("echo hello && exit 0", None, &dir.to_string_lossy()).await.unwrap();
        assert!(!result.is_error());
        let text = pi_ai::types::ToolResultMessage::content(&result);
        assert!(text.iter().any(|b| matches!(b, ContentBlock::Text { text, .. } if text == "hello")));
    });
}

#[test]
fn bash_reports_nonzero_exit_with_status() {
    let rt = rt();
    rt.block_on(async {
        let dir = tmpdir("bash-fail");
        let err = pi_agent::tools::bash::execute_bash("echo boom; exit 7", None, &dir.to_string_lossy()).await.unwrap_err();
        assert!(err.contains("boom"));
        assert!(err.contains("Command exited with code 7"));
    });
}

#[test]
fn bash_timeout_kills_command_and_reports() {
    let rt = rt();
    rt.block_on(async {
        let dir = tmpdir("bash-timeout");
        let err = pi_agent::tools::bash::execute_bash("sleep 10", Some(0.2), &dir.to_string_lossy()).await.unwrap_err();
        assert!(err.contains("Command timed out after 0.2 seconds"), "got {err}");
    });
}

#[test]
fn bash_validates_timeout() {
    assert!(pi_agent::tools::bash::validate_timeout(Some(0.0)).is_err());
    assert!(pi_agent::tools::bash::validate_timeout(Some(f64::NAN)).is_err());
    assert!(pi_agent::tools::bash::validate_timeout(Some(1.0)).is_ok());
}

#[test]
fn read_reports_truncation_messages() {
    let rt = rt();
    rt.block_on(async {
        let dir = tmpdir("read");
        let path = dir.join("big.txt");
        let content = (0..2200).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        std::fs::write(&path, &content).unwrap();

        let result = pi_agent::tools::read::execute_read("r", &path.to_string_lossy(), None, None, &dir.to_string_lossy()).await.unwrap();
        let text: String = pi_ai::types::ToolResultMessage::content(&result)
            .iter()
            .filter_map(|b| match b { ContentBlock::Text { text, .. } => Some(text.clone()), _ => None })
            .collect();
        assert!(text.contains("[Showing lines 1-2000 of 2200. Use offset=2001 to continue.]"), "got prefix {:.80}", text);

        // Offset beyond EOF errors.
        let err = pi_agent::tools::read::execute_read("r", &path.to_string_lossy(), Some(5000.0), None, &dir.to_string_lossy()).await.unwrap_err();
        assert!(err.contains("beyond end of file"));
    });
}

#[test]
fn read_honors_offset_and_limit() {
    let rt = rt();
    rt.block_on(async {
        let dir = tmpdir("read-range");
        let path = dir.join("nums.txt");
        std::fs::write(&path, "zero\none\ntwo\nthree\nfour\n").unwrap();
        let result = pi_agent::tools::read::execute_read("r", &path.to_string_lossy(), Some(3.0), Some(2.0), &dir.to_string_lossy()).await.unwrap();
        let text: String = pi_ai::types::ToolResultMessage::content(&result)
            .iter()
            .filter_map(|b| match b { ContentBlock::Text { text, .. } => Some(text.clone()), _ => None })
            .collect();
        assert!(text.contains("two\nthree"), "got {text:?}");
        // The trailing newline yields an empty 6th line, matching upstream's
        // `split("\n")` behavior.
        assert!(
            text.contains("2 more lines in file. Use offset=5 to continue."),
            "missing continuation hint; got {text:?}"
        );
    });
}

#[test]
fn write_creates_parent_dirs_and_reports_bytes() {
    let rt = rt();
    rt.block_on(async {
        let dir = tmpdir("write");
        let result = pi_agent::tools::write::execute_write("w", "a/b/c.txt", "hello", &dir.to_string_lossy()).await.unwrap();
        let text: String = pi_ai::types::ToolResultMessage::content(&result)
            .iter()
            .filter_map(|b| match b { ContentBlock::Text { text, .. } => Some(text.clone()), _ => None })
            .collect();
        assert!(text.contains("Successfully wrote 5 bytes to a/b/c.txt"));
        assert_eq!(std::fs::read_to_string(dir.join("a/b/c.txt")).unwrap(), "hello");
    });
}

#[test]
fn edit_rejects_duplicates_and_applies_disjoint_edits() {
    let rt = rt();
    rt.block_on(async {
        let dir = tmpdir("edit");
        let path = dir.join("f.txt");
        std::fs::write(&path, "one one two").unwrap();
        // duplicate without disambiguation -> error
        let err = pi_agent::tools::edit::execute_edit(
            "e",
            &path.to_string_lossy(),
            vec![pi_agent::tools::edit_diff::Edit { old_text: "one".to_string(), new_text: "x".to_string() }],
            &dir.to_string_lossy(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("Found 2 occurrences"));
        // disjoint edits apply
        let ok = pi_agent::tools::edit::execute_edit(
            "e",
            &path.to_string_lossy(),
            vec![
                pi_agent::tools::edit_diff::Edit { old_text: "two".to_string(), new_text: "TWO".to_string() },
            ],
            &dir.to_string_lossy(),
        )
        .await
        .unwrap();
        assert!(!ok.is_error());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one one TWO");
    });
}

#[test]
fn path_normalization_handles_unicode_and_at() {
    let dir = tmpdir("path");
    let cwd = dir.to_string_lossy().to_string();
    // @ prefix strips (models use @ to disambiguate leading-dash paths)
    assert_eq!(
        pi_agent::tools::path_utils::normalize_tool_path("@-file.txt"),
        "-file.txt"
    );
    // unicode NBSP becomes regular space
    assert_eq!(pi_agent::tools::path_utils::normalize_tool_path("a\u{00A0}b"), "a b");
    // relative resolves under cwd; absolute passes through
    let resolved = pi_agent::tools::path_utils::resolve_tool_path(&cwd, "x.txt");
    assert_eq!(std::path::Path::new(&resolved).parent().unwrap().to_string_lossy(), cwd);
    let abs = pi_agent::tools::path_utils::resolve_tool_path(&cwd, "/tmp/x.txt");
    assert_eq!(abs, "/tmp/x.txt");
}

#[test]
fn agent_loop_executes_tool_calls_and_feeds_results_back() {
    let rt = rt();
    rt.block_on(async {
        let dir = tmpdir("loop");
        let cwd = dir.to_string_lossy().to_string();

        // Faux provider: script a two-step conversation: assistant tool_call
        // (after the prompt) then tool result.
        let core = pi_ai::providers::FauxProviderCore::new(&pi_ai::providers::RegisterFauxProviderOptions::default());
        // First response: issue a bash tool call.
        core.set_responses(vec![
            pi_ai::providers::FauxResponseStep::Message(pi_ai::providers::faux_assistant_message(
                vec![pi_ai::types::ContentBlock::tool_call(
                    "tool-1",
                    "bash",
                    serde_json::json!({"command": "echo from-tool"}),
                )],
                pi_ai::providers::FauxAssistantOptions {
                    stop_reason: Some(pi_ai::types::StopReason::ToolUse),
                    ..Default::default()
                },
            )),
            // Second response: final answer after the tool result.
            pi_ai::providers::FauxResponseStep::Message(pi_ai::providers::faux_assistant_message(
                vec![pi_ai::types::ContentBlock::text("tool output seen")],
                pi_ai::providers::FauxAssistantOptions::default(),
            )),
        ]);

        let model = core.get_model(None).unwrap().clone();
        let stream_fn = Arc::new(move |model: &pi_ai::model::Model, ctx: &pi_ai::types::Context| core.stream(model, ctx, None));
        let tools: Vec<AgentTool> = vec![pi_agent::tools::bash_tool(cwd.clone())];

        let mut context = pi_agent::AgentContext::new(Some("test".into()), tools);
        let cfg = pi_agent::AgentLoopConfig {
            model,
            stream_fn,
            signal: None,
            stop_after_turn: false,
            on_stream_event: None,
        };
        let prompts = vec![pi_agent::agent::user_text_prompt("run the tool", 1)];
        let mut events = Vec::new();
        let messages = pi_agent::run_agent_loop(prompts, &mut context, &cfg, &mut |e| events.push(e)).await;

        // Conversation: user, assistant(tool call), tool result, assistant(final)
        assert_eq!(messages.len(), 4, "expected user -> toolCall -> result -> final, got {messages:?}");
        let tool_result = messages
            .iter()
            .find_map(|m| match m {
                pi_agent::types::AgentMessage::Core(pi_ai::types::Message::ToolResult(t)) => Some(t.clone()),
                _ => None,
            })
            .expect("tool result message");
        assert_eq!(tool_result.tool_name(), "bash");
        assert!(!tool_result.is_error());
        let text: String = pi_ai::types::ToolResultMessage::content(&tool_result)
            .iter()
            .filter_map(|b| match b { pi_ai::types::ContentBlock::Text { text, .. } => Some(text.clone()), _ => None })
            .collect();
        assert!(text.contains("from-tool"), "got {text:?}");

        // Final assistant message is the second scripted response.
        let last = messages.last().unwrap();
        assert!(matches!(last, pi_agent::types::AgentMessage::Core(pi_ai::types::Message::Assistant(_))));
    });
}

use std::sync::Arc;
