//! Eval scenario definitions — port of `packages/evals/src/*.eval.ts`.

pub mod extensions;
pub mod smoke;

use std::collections::BTreeMap;

use serde_json::json;

use crate::error::EvalFailures;
use crate::harness::{Harness, HarnessContext, HarnessResult, HarnessUsage, PiCliRunnerOptions};
use crate::harness_table::{derive_eval_group_key, EVAL_HARNESS_ITERATION_ARTIFACT};
use crate::session_usage::SessionUsage;
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
    assert: &dyn Fn(&str) -> Result<Option<f64>, EvalFailures>,
    set_artifact: &mut dyn FnMut(&str, serde_json::Value),
) -> HarnessResult<serde_json::Value> {
    let started = std::time::Instant::now();
    let mut errors = Vec::new();
    let mut final_output = String::new();
    let mut session_usage = SessionUsage::default();
    let mut latest_session_jsonl = None;
    let eval_root = match crate::harness::create_eval_root() {
        Ok(root) => root,
        Err(error) => {
            errors.push(error.to_string());
            let result = HarnessResult {
                output: json!({ "response": final_output }),
                errors,
                events: Vec::new(),
                usage: HarnessUsage::from_session_usage(
                    &session_usage,
                    &runner.provider,
                    &runner.model,
                ),
                artifacts: BTreeMap::new(),
                timings: Some(crate::harness::HarnessTimings {
                    total_ms: started.elapsed().as_secs_f64() * 1000.0,
                }),
            };
            return result;
        }
    };
    let cwd = eval_root.join("workspace");
    std::fs::create_dir_all(&cwd).ok();
    let session_root = match crate::harness::create_session_root(&cwd) {
        Ok(path) => path,
        Err(error) => {
            errors.push(error.to_string());
            let result = HarnessResult {
                output: json!({ "response": final_output }),
                errors,
                events: Vec::new(),
                usage: HarnessUsage::from_session_usage(
                    &session_usage,
                    &runner.provider,
                    &runner.model,
                ),
                artifacts: BTreeMap::new(),
                timings: Some(crate::harness::HarnessTimings {
                    total_ms: started.elapsed().as_secs_f64() * 1000.0,
                }),
            };
            let _ = std::fs::remove_dir_all(&eval_root);
            return result;
        }
    };
    let mut has_prompt = false;
    let mut prompt_count = 0usize;

    for step in steps {
        if step.get("type").and_then(|v| v.as_str()) == Some("reload") {
            continue;
        }
        let Some(prompt) = step.get("content").and_then(|v| v.as_str()) else {
            errors.push("Pi eval prompt steps must contain string content.".to_string());
            break;
        };
        prompt_count += 1;
        let output = crate::harness::run_pi_binary_in_session(
            runner,
            &cwd,
            prompt,
            &session_root,
            has_prompt,
        );
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
                // A continued invocation returns the cumulative session
                // snapshot. Replacing, rather than summing, avoids billing
                // the first prompt twice.
                session_usage = output.usage;
                latest_session_jsonl = output.session_jsonl;
                has_prompt = true;
            }
            Err(error) => {
                errors.push(error.to_string());
                break;
            }
        }
    }

    if prompt_count == 0 && errors.is_empty() {
        errors.push("Pi eval input must include at least one prompt step.".to_string());
    }

    let _score = if errors.is_empty() {
        match assert(&final_output) {
            Ok(score) => score,
            Err(error) => {
                errors.push(error.to_string());
                None
            }
        }
    } else {
        None
    };

    let run_id = latest_session_jsonl
        .as_deref()
        .and_then(crate::harness::session_id_from_session_jsonl)
        .unwrap_or_else(crate::harness_table::short_id);
    let events = if let Some(session_jsonl) = latest_session_jsonl.as_deref() {
        match crate::harness::transcript_events_from_session_jsonl(session_jsonl) {
            Ok(events) => events,
            Err(error) => {
                errors.push(error.to_string());
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let mut artifacts = BTreeMap::new();
    artifacts.insert("response".to_string(), json!(final_output));
    artifacts.insert("runId".to_string(), json!(run_id.clone()));
    set_artifact("runId", json!(run_id));
    if let Some(session_jsonl) = latest_session_jsonl {
        set_artifact(
            crate::artifacts::PI_SESSION_SNAPSHOT_ARTIFACT,
            json!(session_jsonl.clone()),
        );
        artifacts.insert(
            crate::artifacts::PI_SESSION_SNAPSHOT_ARTIFACT.to_string(),
            json!(session_jsonl),
        );
    }

    let mut result = HarnessResult {
        output: json!({ "response": final_output }),
        errors,
        events,
        usage: HarnessUsage::from_session_usage(&session_usage, &runner.provider, &runner.model),
        artifacts,
        timings: Some(crate::harness::HarnessTimings {
            total_ms: started.elapsed().as_secs_f64() * 1000.0,
        }),
    };
    if let Err(error) = std::fs::remove_dir_all(&eval_root) {
        result.errors.push(format!(
            "failed to clean up eval root {}: {error}",
            eval_root.display()
        ));
    }
    result
}

/// Default harness context used by the runner.
pub fn empty_context() -> HarnessContext {
    HarnessContext::default()
}

/// Iteration artifact key re-export for the runner.
pub fn iteration_artifact_name() -> &'static str {
    EVAL_HARNESS_ITERATION_ARTIFACT
}
