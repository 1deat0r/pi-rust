//! Eval scenario definitions — port of `packages/evals/src/*.eval.ts`.

pub mod extensions;
pub mod smoke;

use std::collections::BTreeMap;

use serde_json::json;

use crate::harness::{Harness, HarnessContext, HarnessResult, HarnessUsage, PiCliRunnerOptions};
use crate::harness_table::{derive_eval_group_key, EVAL_HARNESS_ITERATION_ARTIFACT};
use crate::summary::{HarnessObservation, Outcome};

/// One eval-set declaration consumed by the CLI runner.
pub struct EvalSet {
    pub name: &'static str,
    pub file: &'static str,
    /// Baseline + candidate harnesses (empty candidates => single-harness
    /// eval that reports pass/fail directly, mirroring the upstream smoke
    /// eval which has no comparison table).
    pub baseline: Harness<serde_json::Value>,
    pub candidates: Vec<Harness<serde_json::Value>>,
    /// Inputs exercised by every harness/repetition.
    pub inputs: Vec<EvalInput>,
}

pub struct EvalInput {
    pub id: String,
    pub value: serde_json::Value,
}

/// Builds the observation for one test case (port of the reporter's
/// `collectHarnessObservations` per-run mapping).
#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
pub fn observation_from_run(
    eval_set: &str,
    file: &str,
    test_name: &str,
    input: &serde_json::Value,
    harness: &str,
    baseline: &str,
    candidates: &[String],
    repetition: u32,
    result: &HarnessResult<serde_json::Value>,
    score: Option<f64>,
) -> HarnessObservation {
    let group_key = derive_eval_group_key(input, repetition).unwrap_or_default();
    let outcome = if !result.errors.is_empty() && score.is_none() {
        Outcome::Errored
    } else if score.is_some() {
        Outcome::Scored
    } else {
        Outcome::Unscored
    };
    let total_ms = result.timings.as_ref().map(|timings| timings.total_ms);
    HarnessObservation {
        eval_set: eval_set.to_string(),
        group_key,
        test_name: test_name.to_string(),
        file: file.to_string(),
        harness: harness.to_string(),
        baseline: baseline.to_string(),
        candidates: candidates.iter().map(|s| s.to_string()).collect(),
        repetition,
        outcome,
        score,
        total_tokens: Some(result.usage.total_tokens as f64),
        total_ms,
        estimated_cost_usd: result
            .usage
            .metadata
            .get("estimatedCostUsd")
            .and_then(|v| v.as_f64()),
    }
}

/// Runs one harness on a prompt via the real `pi` binary and packages a
/// harness result (used by the eval definitions).
pub fn run_pi_case(
    runner: &PiCliRunnerOptions,
    _input: &serde_json::Value,
    steps: &[serde_json::Value],
    assert: &dyn Fn(&str) -> Result<Option<f64>, String>,
    set_artifact: &mut dyn FnMut(&str, serde_json::Value),
) -> HarnessResult<serde_json::Value> {
    let started = std::time::Instant::now();
    let mut errors = Vec::new();
    let mut final_output = String::new();
    let cwd = crate::harness::create_eval_root().join("workspace");
    std::fs::create_dir_all(&cwd).ok();

    for step in steps {
        let prompt = step
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let output = crate::harness::run_pi_binary(runner, &cwd, prompt);
        match output {
            Ok(output) => {
                if output.exit_code != 0 {
                    errors.push(format!(
                        "pi exited with code {}: {}",
                        output.exit_code,
                        output.stderr.trim()
                    ));
                    break;
                }
                final_output = crate::harness::extract_response_text(&output.stdout);
            }
            Err(error) => {
                errors.push(error);
                break;
            }
        }
    }

    let _score = if errors.is_empty() {
        match assert(&final_output) {
            Ok(score) => score,
            Err(error) => {
                errors.push(error);
                None
            }
        }
    } else {
        None
    };

    // Expose the final response for assertions and artifacts.
    let mut artifacts = BTreeMap::new();
    artifacts.insert("response".to_string(), json!(final_output));
    set_artifact("runId", json!(crate::harness_table::short_id()));

    let mut metadata = BTreeMap::new();
    // The subprocess runner cannot observe model usage; only providers that
    // report a price for the selected model would have an estimate here.
    if runner.provider != "faux" {
        metadata.insert("estimatedCostUsd".to_string(), json!(0.0));
    }

    HarnessResult {
        output: json!({ "response": final_output }),
        errors,
        events: vec![
            crate::harness::TranscriptEvent::Message {
                role: "user".into(),
                content: "prompt".into(),
            },
            crate::harness::TranscriptEvent::Message {
                role: "assistant".into(),
                content: final_output.clone(),
            },
        ],
        usage: HarnessUsage {
            provider: runner.provider.clone(),
            model: runner.model.clone(),
            total_tokens: 0,
            metadata,
            ..Default::default()
        },
        artifacts,
        timings: Some(crate::harness::HarnessTimings {
            total_ms: started.elapsed().as_secs_f64() * 1000.0,
        }),
    }
}

/// Default harness context used by the runner.
pub fn empty_context() -> HarnessContext {
    HarnessContext::default()
}

/// Iteration artifact key re-export for the runner.
pub fn iteration_artifact_name() -> &'static str {
    EVAL_HARNESS_ITERATION_ARTIFACT
}
