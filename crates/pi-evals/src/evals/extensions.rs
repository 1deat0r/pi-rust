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

use serde::Deserialize;
use serde_json::json;

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
pub fn faux_extension_fixture() -> FauxExtensionFixture {
    serde_json::from_str(FAUX_UNSUPPORTED_FIXTURE).expect("valid faux extension fixture")
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

    let run = |prompt: &str| crate::harness::run_pi_binary(runner, cwd, prompt);
    match run(&steps.create_prompt) {
        Ok(first) if first.exit_code == 0 => {
            usage.merge(&first.usage);
            session_jsonl = first.session_jsonl.clone();
            // reload step: a second invocation picks up any authored files
            match run(&steps.use_prompt) {
                Ok(second) if second.exit_code == 0 => {
                    usage.merge(&second.usage);
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
                Err(error) => errors.push(error),
            }
        }
        Ok(first) => {
            errors.push(format!(
                "pi exited with code {} on the create step: {}",
                first.exit_code,
                first.stderr.trim()
            ));
        }
        Err(error) => errors.push(error),
    }

    let extension_path = cwd.join(".pi").join("extensions").join("hello.ts");
    let extension_source = if extension_path.exists() {
        std::fs::read_to_string(&extension_path).ok()
    } else {
        None
    };

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
) -> Result<f64, String> {
    let mut failures = Vec::new();
    if !outcome.errors.is_empty() {
        failures.extend(outcome.errors.iter().cloned());
    }
    match &outcome.extension_source {
        None => failures.push("generated extension source is unavailable".to_string()),
        Some(source) => {
            let mut imports = Vec::new();
            let import_pattern = regex::Regex::new(r#"\b(?:from|import)\s*[\"']([^\"']+)[\"']"#)
                .expect("static extension import pattern");
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

    if failures.is_empty() {
        Ok(1.0)
    } else {
        Err(failures.join("; "))
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
        Err(error) => (Some(0.0), Some(error)),
    }
}

/// True when the runner cannot possibly author extensions (faux provider).
pub fn is_skippable(runner: &PiCliRunnerOptions) -> bool {
    runner.provider == "faux" || runner.provider.is_empty()
}
