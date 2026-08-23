//! Pi Coding Agent smoke eval — port of `packages/evals/src/smoke.eval.ts`.
//!
//! Upstream asserts the model answers exactly "Paris". The Rust port runs
//! the real `pi` binary and asserts the same exact-output pattern, with a
//! `faux`-aware expectation so the suite is runnable without real-model
//! credentials in CI (the parity suite uses the faux provider).

use serde_json::json;

use crate::harness::{PiCliRunnerOptions, PiRunOutput};

pub const EVAL_SET: &str = "Pi Coding Agent smoke";
pub const FILE: &str = "src/smoke.eval.ts";
pub const TEST_NAME: &str = "runs a basic prompt end to end";

/// Expected single-token answer for real providers (upstream assertion).
pub const EXPECTED_ANSWER: &str = "Paris";

pub struct SmokeOutcome {
    pub output: String,
    pub errors: Vec<String>,
    pub exit_code: i32,
}

/// Runs the smoke prompt through the real `pi` binary.
pub fn run_smoke(runner: &PiCliRunnerOptions, cwd: &std::path::Path, prompt: &str) -> SmokeOutcome {
    let output = crate::harness::run_pi_binary(runner, cwd, prompt);
    match output {
        Ok(PiRunOutput {
            stdout,
            stderr,
            exit_code,
        }) => SmokeOutcome {
            output: crate::harness::extract_response_text(&stdout),
            errors: if exit_code == 0 {
                Vec::new()
            } else {
                vec![format!(
                    "pi exited with code {exit_code}: {}",
                    stderr.trim()
                )]
            },
            exit_code,
        },
        Err(error) => SmokeOutcome {
            output: String::new(),
            errors: vec![error],
            exit_code: -1,
        },
    }
}

/// Applies the scenario assertions; returns the parse of the output for the
/// observation.
pub fn assert_smoke_result(
    runner: &PiCliRunnerOptions,
    outcome: &SmokeOutcome,
) -> Result<f64, String> {
    let mut failures = Vec::new();
    if !outcome.errors.is_empty() {
        failures.extend(outcome.errors.iter().cloned());
    }
    let output = outcome.output.trim();
    if runner.provider == "faux" {
        if !output.starts_with("faux response to:") {
            failures.push(format!(
                "faux provider response did not match the scripted pattern: {output:?}"
            ));
        }
    } else if output != EXPECTED_ANSWER {
        failures.push(format!(
            "expected output {EXPECTED_ANSWER:?}, got {output:?}"
        ));
    }
    if failures.is_empty() {
        Ok(1.0)
    } else {
        Err(failures.join("; "))
    }
}

/// Input for the smoke eval (id keeps the group key stable across runs).
pub fn smoke_input() -> serde_json::Value {
    json!({
        "id": "capital-of-france",
        "prompt": "What's the capital of France? Respond with only the city name."
    })
}
