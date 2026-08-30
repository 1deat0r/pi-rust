//! Pi extension authoring eval — port of `packages/evals/src/extensions.eval.ts`.
//!
//! Upstream drives an in-process `AgentSession`: create a `hello` extension,
//! reload, then call the tool. The Rust port drives the real `pi` binary with
//! the same three-step input and asserts the observable file-system outcome:
//! a `hello.ts` extension exists in the workspace's `.pi/extensions`
//! directory and the final response matches the tool greeting. The
//! system-prompt-split baseline/candidate pair is retained as the harness
//! table (the two harnesses differ only in the prompts they run, mirroring
//! the upstream intent of comparing prompt configurations).

use std::sync::LazyLock;

use serde::Deserialize;
use serde_json::json;

use crate::error::EvalFailures;
use crate::harness::PiCliRunnerOptions;
use crate::session_usage::SessionUsage;

pub const EVAL_SET: &str = "Pi extension authoring system prompt";
pub const FILE: &str = "src/extensions.eval.ts";
pub const TEST_NAME: &str = "creates, reloads, and uses a hello extension";

pub const BASELINE_NAME: &str = "system-prompt-without-docs";
pub const CANDIDATE_NAME: &str = "default-system-prompt";

const CREATE_PROMPT: &str =
    "Create a Pi extension with a hello tool that takes a name and returns a greeting. For example, passing Bob should return `Hello, Bob!`.";
const USE_PROMPT: &str =
    "Use the hello tool to greet Bob. Respond with exactly the tool's greeting and nothing else.";

pub const FAUX_UNSUPPORTED_FIXTURE: &str =
    include_str!("fixtures/extensions-faux-unsupported.json");

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FauxExtensionFixture {
    pub schema_version: u32,
    pub scenario: String,
    pub provider: String,
    pub supported: bool,
    pub reason: String,
    pub expected: FauxExtensionExpected,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FauxExtensionExpected {
    pub generated_extension_source: Option<String>,
    pub loaded_tools: Vec<String>,
    pub successful_hello_calls: u64,
    pub response_prefix: String,
}

/// The faux provider is intentionally a scripted text fixture. It cannot
/// produce the tool call needed to author and reload an extension, so the
/// extension eval reports this exact versioned boundary instead of pretending
/// that a skipped run is a model score.
pub fn faux_extension_fixture() -> &'static FauxExtensionFixture {
    // The fixture is embedded at compile time; a parse failure is a build
    // defect, so panicking during first use is the honest invariant.
    #[allow(clippy::panic)]
    static FIXTURE: LazyLock<FauxExtensionFixture> = LazyLock::new(|| {
        serde_json::from_str(FAUX_UNSUPPORTED_FIXTURE)
            .unwrap_or_else(|error| panic!("valid faux extension fixture: {error}"))
    });
    &FIXTURE
}

pub fn unsupported_boundary(runner: &PiCliRunnerOptions) -> Option<String> {
    if !is_skippable(runner) {
        return None;
    }
    let fixture = faux_extension_fixture();
    Some(format!(
        "{} (fixture schema {}: {})",
        fixture.reason, fixture.schema_version, fixture.scenario
    ))
}

pub struct ExtensionSteps {
    pub create_prompt: String,
    pub use_prompt: String,
}

/// The three upstream steps: create, reload, use. The runner maps `reload`
/// to a second binary invocation in the same workspace (each run in the eval
/// executes against the same cwd, so authored files persist).
pub fn extension_input() -> serde_json::Value {
    json!({
        "id": "hello-extension",
        "prompt": CREATE_PROMPT,
        "usePrompt": USE_PROMPT,
    })
}

pub struct ExtensionOutcome {
    pub final_response: String,
    pub extension_source: Option<String>,
    pub errors: Vec<String>,
    pub session_jsonl: Option<String>,
    pub usage: SessionUsage,
}

/// Runs the create + use prompts through the real `pi` binary in `cwd` and
/// reads the authored extension source if it exists.
pub fn run_extension_scenario(
    runner: &PiCliRunnerOptions,
    cwd: &std::path::Path,
    steps: &ExtensionSteps,
) -> ExtensionOutcome {
    let mut errors = Vec::new();
    let mut final_response = String::new();
    let mut usage = SessionUsage::default();
    let mut session_jsonl = None;

    let session_root = match crate::harness::create_session_root(cwd) {
        Ok(path) => path,
        Err(error) => {
            errors.push(error.to_string());
            return ExtensionOutcome {
                final_response,
                extension_source: None,
                errors,
                session_jsonl,
                usage,
            };
        }
    };
    let run = |prompt: &str, continue_session: bool| {
        crate::harness::run_pi_binary_in_session(
            runner,
            cwd,
            prompt,
            &session_root,
            continue_session,
        )
    };
    match run(&steps.create_prompt, false) {
        Ok(first) if first.exit_code == 0 => {
            usage = first.usage;
            session_jsonl = first.session_jsonl.clone();
            // reload step: a second invocation picks up any authored files
            match run(&steps.use_prompt, true) {
                Ok(second) if second.exit_code == 0 => {
                    // The continued session snapshot already includes the
                    // create step's usage; summing both snapshots would
                    // double-count the first provider call.
                    usage = second.usage;
                    session_jsonl = second.session_jsonl;
                    final_response = crate::harness::extract_response_text(&second.stdout);
                }
                Ok(second) => {
                    errors.push(format!(
                        "pi exited with code {} on the use step: {}",
                        second.exit_code,
                        second.stderr.trim()
                    ));
                }
                Err(error) => errors.push(error.to_string()),
            }
        }
        Ok(first) => {
            errors.push(format!(
                "pi exited with code {} on the create step: {}",
                first.exit_code,
                first.stderr.trim()
            ));
        }
        Err(error) => errors.push(error.to_string()),
    }

    let extension_path = cwd.join(".pi").join("extensions").join("hello.ts");
    let extension_source = if extension_path.exists() {
        std::fs::read_to_string(&extension_path).ok()
    } else {
        None
    };

    // The subprocess runner owns the retained session just like the upstream
    // in-process harness owns its temporary SessionManager root.  The JSONL
    // snapshot above is already an owned string, so clean up the exact
    // session directory before returning and surface cleanup failure as a
    // harness error rather than leaving a misleading successful result.
    if let Err(error) = std::fs::remove_dir_all(&session_root) {
        errors.push(format!(
            "failed to clean up eval session directory {}: {error}",
            session_root.display()
        ));
    }

    ExtensionOutcome {
        final_response,
        extension_source,
        errors,
        session_jsonl,
        usage,
    }
}

/// Applies the extension-authoring judge assertions (port of
/// `ExtensionAuthoringJudge` narrowed to CLI-observable checks).
pub fn assert_extension_result(
    _runner: &PiCliRunnerOptions,
    outcome: &ExtensionOutcome,
) -> Result<f64, EvalFailures> {
    let mut failures = Vec::new();
    if !outcome.errors.is_empty() {
        failures.extend(outcome.errors.iter().cloned());
    }
    match &outcome.extension_source {
        None => failures.push("generated extension source is unavailable".to_string()),
        Some(source) => {
            // The pattern is a compile-time literal; a compile failure here
            // is a build defect.
            #[allow(clippy::panic)]
            static IMPORT_PATTERN: LazyLock<regex::Regex> = LazyLock::new(|| {
                regex::Regex::new(r#"\b(?:from|import)\s*[\"']([^\"']+)[\"']"#)
                    .unwrap_or_else(|error| panic!("static extension import pattern: {error}"))
            });
            let mut imports = Vec::new();
            let import_pattern = &*IMPORT_PATTERN;
            for captures in import_pattern.captures_iter(source).take(100) {
                if let Some(spec) = captures.get(1) {
                    imports.push(spec.as_str().to_string());
                }
            }
            if !imports
                .iter()
                .any(|spec| spec == "@earendil-works/pi-coding-agent")
            {
                failures.push("extension does not import the canonical @earendil-works/pi-coding-agent package".to_string());
            }
            if imports
                .iter()
                .any(|spec| spec.starts_with("@mariozechner/"))
            {
                failures.push("extension imports a legacy @mariozechner package".to_string());
            }
            if imports
                .iter()
                .any(|spec| spec.starts_with("@sinclair/typebox"))
            {
                failures.push(
                    "extension imports legacy \"@sinclair/typebox\" instead of \"typebox\""
                        .to_string(),
                );
            }
            if !source.contains("hello") {
                failures.push("generated extension does not declare the hello tool".to_string());
            }
        }
    }
    let response = outcome.final_response.trim();
    if response != "Hello, Bob!" {
        failures.push("final response was not exactly \"Hello, Bob!\"".to_string());
    }

    // A matching answer and source file are not sufficient evidence of the
    // extension workflow.  The upstream judge receives tool calls/results
    // from the live AgentSession; require the durable subprocess transcript
    // to prove that hello({ name: "Bob" }) actually ran and returned the
    // greeting.  Missing or malformed persistence is a failed assertion, not
    // a provider-success claim.
    match outcome.session_jsonl.as_deref() {
        None => failures.push("durable session transcript is unavailable".to_string()),
        Some(session_jsonl) => {
            match crate::harness::transcript_events_from_session_jsonl(session_jsonl) {
                Err(error) => failures.push(error.to_string()),
                Ok(events) => {
                    let hello_call = events.iter().find_map(|event| match event {
                        crate::harness::TranscriptEvent::ToolCall {
                            id,
                            name,
                            arguments,
                        } if name == "hello"
                            && arguments
                                .as_ref()
                                .and_then(|arguments| arguments.get("name"))
                                .and_then(serde_json::Value::as_str)
                                == Some("Bob") =>
                        {
                            Some(id.as_str())
                        }
                        _ => None,
                    });
                    let successful_hello = hello_call.is_some_and(|call_id| {
                        events.iter().any(|event| {
                            matches!(
                                event,
                                crate::harness::TranscriptEvent::ToolResult {
                                    tool_call_id,
                                    name,
                                    content,
                                    error,
                                } if tool_call_id == call_id
                                    && name == "hello"
                                    && error.is_empty()
                                    && content == &serde_json::json!("Hello, Bob!")
                            )
                        })
                    });
                    if !successful_hello {
                        failures.push(
                            "no successful hello({ name: \"Bob\" }) call returned \"Hello, Bob!\""
                                .to_string(),
                        );
                    }
                }
            }
        }
    }

    if failures.is_empty() {
        Ok(1.0)
    } else {
        Err(EvalFailures(failures))
    }
}

/// Maps the upstream judge contract to a harness observation. Harness
/// failures are unscorable, while assertion failures are deterministic score
/// zeroes with their rationale retained for the report.
pub fn score_extension_result(
    runner: &PiCliRunnerOptions,
    outcome: &ExtensionOutcome,
) -> (Option<f64>, Option<String>) {
    if !outcome.errors.is_empty() {
        return (None, None);
    }
    match assert_extension_result(runner, outcome) {
        Ok(score) => (Some(score), None),
        Err(error) => (Some(0.0), Some(error.to_string())),
    }
}

/// True when the runner cannot possibly author extensions (faux provider).
pub fn is_skippable(runner: &PiCliRunnerOptions) -> bool {
    runner.provider == "faux" || runner.provider.is_empty()
}
