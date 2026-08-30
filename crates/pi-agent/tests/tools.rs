#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Tool behavior tests: bash capture, read truncation messages, write/edit,
//! path normalization, and an agent-loop tool-call round trip.

use pi_agent::tools::{AgentTool, ToolUpdateCallback};
use pi_ai::types::ContentBlock;

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
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
        let result = pi_agent::tools::bash::execute_bash(
            "echo hello && exit 0",
            None,
            &dir.to_string_lossy(),
        )
        .await
        .unwrap();
        assert!(!result.is_error());
        let text = pi_ai::types::ToolResultMessage::content(&result);
        assert!(text
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text, .. } if text == "hello\n")));
    });
}

#[test]
fn bash_reports_nonzero_exit_with_status() {
    let rt = rt();
    rt.block_on(async {
        let dir = tmpdir("bash-fail");
        let err =
            pi_agent::tools::bash::execute_bash("echo boom; exit 7", None, &dir.to_string_lossy())
                .await
                .unwrap_err();
        assert!(err.contains("boom"));
        assert!(err.contains("Command exited with code 7"));
    });
}

#[test]
fn bash_timeout_kills_command_and_reports() {
    let rt = rt();
    rt.block_on(async {
        let dir = tmpdir("bash-timeout");
        let err =
            pi_agent::tools::bash::execute_bash("sleep 10", Some(0.2), &dir.to_string_lossy())
                .await
                .unwrap_err();
        assert!(
            err.contains("Command timed out after 0.2 seconds"),
            "got {err}"
        );
    });
}

#[test]
fn direct_bash_output_stream_preserves_interactive_metadata() {
    let rt = rt();
    rt.block_on(async {
        let dir = tmpdir("bash-direct-output");
        let updates = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let received = updates.clone();
        let callback: pi_agent::tools::bash::BashOutputCallback =
            std::sync::Arc::new(move |output| received.lock().unwrap().push(output));
        let capture = pi_agent::tools::bash::run_bash_with_output(
            "printf hello",
            &dir.to_string_lossy(),
            None,
            None,
            Some(callback),
        )
        .await
        .unwrap();

        assert_eq!(capture.output, "hello");
        assert_eq!(capture.exit_code, Some(0));
        assert!(!capture.aborted);
        assert!(!capture.timed_out);
        assert!(!capture.truncated);
        assert!(capture.full_output_path.is_none());
        assert!(updates
            .lock()
            .unwrap()
            .iter()
            .any(|output| output == "hello"));
    });
}

#[test]
fn direct_bash_output_exposes_full_file_when_truncated() {
    let rt = rt();
    rt.block_on(async {
        let dir = tmpdir("bash-direct-truncated");
        let capture = pi_agent::tools::bash::run_bash_with_output(
            "yes line | head -n 15000",
            &dir.to_string_lossy(),
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let path = capture
            .full_output_path
            .as_deref()
            .expect("direct bash should preserve the full-output path");
        assert!(capture.truncated);
        assert!(std::path::Path::new(path).is_file());
        assert!(std::fs::read_to_string(path).unwrap().contains("line\n"));
    });
}

#[test]
fn run_bash_uses_bounded_capture_and_preserves_full_output() {
    let rt = rt();
    rt.block_on(async {
        let dir = tmpdir("bash-run-bounded");
        let capture = pi_agent::tools::bash::run_bash(
            "yes line | head -n 15000",
            &dir.to_string_lossy(),
            None,
            None,
        )
        .await
        .unwrap();

        let path = capture
            .full_output_path
            .as_deref()
            .expect("run_bash should preserve the full-output path");
        let full_output = std::fs::read_to_string(path).unwrap();
        assert_eq!(capture.exit_code, Some(0));
        assert!(capture.truncated);
        assert!(full_output.lines().count() > 10_000);
        assert!(capture.output.len() < full_output.len());
        assert!(capture.error_message.is_none());
    });
}

#[test]
fn run_bash_returns_spawn_failure_instead_of_fake_success() {
    let rt = rt();
    rt.block_on(async {
        let dir = tmpdir("bash-missing-cwd").join("does-not-exist");
        let error = pi_agent::tools::bash::run_bash(
            "printf should-not-run",
            &dir.to_string_lossy(),
            None,
            None,
        )
        .await
        .expect_err("an invalid working directory must remain an error");
        assert!(!error.to_string().is_empty());
    });
}

#[test]
fn run_bash_preserves_pre_cancelled_state_without_running_command() {
    let rt = rt();
    rt.block_on(async {
        let dir = tmpdir("bash-pre-cancelled");
        let abort = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let capture = pi_agent::tools::bash::run_bash(
            "printf should-not-run",
            &dir.to_string_lossy(),
            None,
            Some(abort),
        )
        .await
        .unwrap();
        assert!(capture.aborted);
        assert!(capture.output.is_empty());
        assert_eq!(capture.exit_code, None);
    });
}

#[test]
fn bash_validates_timeout() {
    assert!(pi_agent::tools::bash::validate_timeout(Some(0.0)).is_err());
    assert!(pi_agent::tools::bash::validate_timeout(Some(f64::NAN)).is_err());
    assert!(pi_agent::tools::bash::validate_timeout(Some(1.0)).is_ok());
}

#[test]
fn bash_tool_streams_partial_updates_through_agent_contract() {
    let rt = rt();
    rt.block_on(async {
        let dir = tmpdir("bash-updates");
        let updates = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let received = updates.clone();
        let on_update: ToolUpdateCallback = std::sync::Arc::new(move |partial| {
            received.lock().unwrap().push(partial.clone());
        });

        let result = pi_agent::tools::bash::execute_bash_with_updates(
            "printf hello",
            None,
            &dir.to_string_lossy(),
            None,
            Some(on_update),
        )
        .await
        .unwrap();

        let updates = updates.lock().unwrap();
        assert!(updates.len() >= 2, "expected initial and output updates");
        assert!(updates.iter().any(|update| {
            update.content.iter().any(|content| {
                matches!(content, ContentBlock::Text { text, .. } if text.contains("hello"))
            })
        }));
        assert!(updates.last().unwrap().content.iter().any(|content| {
            matches!(content, ContentBlock::Text { text, .. } if text == "hello")
        }));
        assert!(result.content.iter().any(|content| {
            matches!(content, ContentBlock::Text { text, .. } if text == "hello")
        }));
    });
}

#[test]
fn bash_tool_preserves_full_output_details_when_truncated() {
    let rt = rt();
    rt.block_on(async {
        let dir = tmpdir("bash-full-output");
        let result = pi_agent::tools::bash::execute_bash_with_updates(
            "yes line | head -n 15000",
            None,
            &dir.to_string_lossy(),
            None,
            None,
        )
        .await
        .unwrap();

        let full_output_path = result.details.as_ref().unwrap()["fullOutputPath"]
            .as_str()
            .expect("full output path in tool details");
        assert!(std::path::Path::new(full_output_path).is_file());
        assert!(result.content.iter().any(|content| {
            matches!(content, ContentBlock::Text { text, .. } if text.contains("Full output:"))
        }));
    });
}

#[test]
fn bash_tool_coalesces_updates_and_keeps_final_truncation_snapshot() {
    let rt = rt();
    rt.block_on(async {
        let dir = tmpdir("bash-coalesced");
        let updates = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let received = updates.clone();
        let on_update: ToolUpdateCallback = std::sync::Arc::new(move |partial| {
            received.lock().unwrap().push(partial.clone());
        });

        let result = pi_agent::tools::bash::execute_bash_with_updates(
            "i=1; while [ $i -le 3000 ]; do echo line-$i; i=$((i + 1)); done",
            None,
            &dir.to_string_lossy(),
            None,
            Some(on_update),
        )
        .await
        .unwrap();

        let updates = updates.lock().unwrap();
        assert!(
            updates.len() < 25,
            "expected throttled updates, got {}",
            updates.len()
        );
        let details = result.details.as_ref().unwrap();
        assert_eq!(details["truncation"]["totalLines"], 3000);
        assert!(result.content.iter().any(|content| {
            matches!(content, ContentBlock::Text { text, .. } if text.contains("line-3000"))
        }));
        let final_update = updates.last().expect("final progress update");
        assert!(final_update.content.iter().any(|content| {
            matches!(content, ContentBlock::Text { text, .. } if text.contains("line-3000"))
        }));
        assert_eq!(
            final_update.details.as_ref().unwrap()["fullOutputPath"],
            details["fullOutputPath"]
        );
        let full_output =
            std::fs::read_to_string(details["fullOutputPath"].as_str().unwrap()).unwrap();
        assert!(full_output.contains("line-1\nline-2"));
        assert!(full_output.contains("line-2999\nline-3000"));
    });
}

#[test]
fn bash_tool_preserves_output_and_full_file_when_timeout_follows_output() {
    let rt = rt();
    rt.block_on(async {
        let dir = tmpdir("bash-timeout-output");
        let error = pi_agent::tools::bash::execute_bash_with_updates(
            "yes line | head -n 15000; sleep 1",
            Some(0.05),
            &dir.to_string_lossy(),
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            error.contains("Command timed out after 0.05 seconds"),
            "got: {error}"
        );
        let full_output_path = error
            .split("Full output: ")
            .nth(1)
            .and_then(|path| path.lines().next())
            .map(|path| path.trim_end_matches(']'))
            .expect("full output path in timeout error");
        let full_output = std::fs::read_to_string(full_output_path)
            .unwrap_or_else(|e| panic!("path={full_output_path:?} error={error:?}: {e}"));
        assert!(full_output.contains("line\n"));
    });
}

#[test]
fn edit_tool_registers_prepare_arguments_before_validation() {
    let tool = pi_agent::tools::edit_tool(std::env::temp_dir().to_string_lossy().into_owned());
    let prepare = tool
        .prepare_arguments
        .as_ref()
        .expect("edit tool prepareArguments");
    let prepared = prepare(serde_json::json!({
        "path": "file.txt",
        "oldText": "before",
        "newText": "after",
    }));
    assert_eq!(prepared["edits"][0]["oldText"], "before");
    assert!(prepared.get("oldText").is_none());
}

#[test]
fn read_reports_truncation_messages() {
    let rt = rt();
    rt.block_on(async {
        let dir = tmpdir("read");
        let path = dir.join("big.txt");
        let content = (0..2200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, &content).unwrap();

        let result = pi_agent::tools::read::execute_read(
            "r",
            &path.to_string_lossy(),
            None,
            None,
            &dir.to_string_lossy(),
        )
        .await
        .unwrap();
        let text: String = pi_ai::types::ToolResultMessage::content(&result)
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            text.contains("[Showing lines 1-2000 of 2200. Use offset=2001 to continue.]"),
            "got prefix {:.80}",
            text
        );

        // Offset beyond EOF errors.
        let err = pi_agent::tools::read::execute_read(
            "r",
            &path.to_string_lossy(),
            Some(5000.0),
            None,
            &dir.to_string_lossy(),
        )
        .await
        .unwrap_err();
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
        let result = pi_agent::tools::read::execute_read(
            "r",
            &path.to_string_lossy(),
            Some(3.0),
            Some(2.0),
            &dir.to_string_lossy(),
        )
        .await
        .unwrap();
        let text: String = pi_ai::types::ToolResultMessage::content(&result)
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
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
        let result = pi_agent::tools::write::execute_write(
            "w",
            "a/b/c.txt",
            "hello",
            &dir.to_string_lossy(),
        )
        .await
        .unwrap();
        let text: String = pi_ai::types::ToolResultMessage::content(&result)
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(text.contains("Successfully wrote 5 bytes to a/b/c.txt"));
        assert_eq!(
            std::fs::read_to_string(dir.join("a/b/c.txt")).unwrap(),
            "hello"
        );
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
            vec![pi_agent::tools::edit_diff::Edit {
                old_text: "one".to_string(),
                new_text: "x".to_string(),
            }],
            &dir.to_string_lossy(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("Found 2 occurrences"));
        // disjoint edits apply
        let ok = pi_agent::tools::edit::execute_edit(
            "e",
            &path.to_string_lossy(),
            vec![pi_agent::tools::edit_diff::Edit {
                old_text: "two".to_string(),
                new_text: "TWO".to_string(),
            }],
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
    assert_eq!(
        pi_agent::tools::path_utils::normalize_tool_path("a\u{00A0}b"),
        "a b"
    );
    // relative resolves under cwd; absolute passes through
    let resolved = pi_agent::tools::path_utils::resolve_tool_path(&cwd, "x.txt");
    assert_eq!(
        std::path::Path::new(&resolved)
            .parent()
            .unwrap()
            .to_string_lossy(),
        cwd
    );
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
        let core = pi_ai::providers::FauxProviderCore::new(
            &pi_ai::providers::RegisterFauxProviderOptions::default(),
        );
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
        let stream_fn = Arc::new(
            move |model: &pi_ai::model::Model, ctx: &pi_ai::types::Context| {
                core.stream(model, ctx, None)
            },
        );
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
        let messages =
            pi_agent::run_agent_loop(prompts, &mut context, &cfg, &mut |e| events.push(e)).await;

        // Conversation: user, assistant(tool call), tool result, assistant(final)
        assert_eq!(
            messages.len(),
            4,
            "expected user -> toolCall -> result -> final, got {messages:?}"
        );
        let tool_result = messages
            .iter()
            .find_map(|m| match m {
                pi_agent::types::AgentMessage::Core(pi_ai::types::Message::ToolResult(t)) => {
                    Some(t.clone())
                }
                _ => None,
            })
            .expect("tool result message");
        assert_eq!(tool_result.tool_name(), "bash");
        assert!(!tool_result.is_error());
        let text: String = pi_ai::types::ToolResultMessage::content(&tool_result)
            .iter()
            .filter_map(|b| match b {
                pi_ai::types::ContentBlock::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(text.contains("from-tool"), "got {text:?}");

        // Final assistant message is the second scripted response.
        let last = messages.last().unwrap();
        assert!(matches!(
            last,
            pi_agent::types::AgentMessage::Core(pi_ai::types::Message::Assistant(_))
        ));
    });
}

use std::sync::Arc;
