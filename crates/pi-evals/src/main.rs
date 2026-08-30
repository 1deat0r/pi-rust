//! `pi-evals` runner CLI — port of `packages/evals/scripts/run-evals.mjs`.
//!
//! Runs the eval suite against the real `pi` binary and prints results.

use std::path::PathBuf;

use pi_evals::artifacts::{
    record_eval_session_artifact, record_eval_source_artifact, Attachment, EvalArtifact,
    TestRecord, PI_SESSION_SNAPSHOT_ARTIFACT,
};
use pi_evals::error::EvalError;
use pi_evals::evals::extensions;
use pi_evals::evals::{observation_from_run, EvalSet};
use pi_evals::harness::{
    create_eval_root, resolve_model_selection, HarnessContext, HarnessUsage, ModelSelection,
    PiCliRunnerOptions,
};
use pi_evals::harness_table::eval_harness_table;
use pi_evals::reporter::{append_harness_run_report, ReporterOptions};
use pi_evals::summary::{
    format_harness_comparison_report, summarize_harness_comparisons, HarnessObservation, Outcome,
};

struct CliOptions {
    runner: PiCliRunnerOptions,
    artifact_dir: Option<PathBuf>,
    evals: Vec<String>,
}

fn usage() -> String {
    "Pi evals runner (Rust port of scripts/run-evals.mjs)

USAGE:
  pi-evals [--provider PROVIDER] [--model MODEL] [--faux] [--binary PATH]
           [--eval smoke|extensions|all] [--artifact-dir DIR]

The default model selection reads PI_PROVIDER / PI_MODEL. `--faux` selects
pi's scripted test provider (faux/faux-1) so the suite runs without
credentials. Additional eval names restrict which evals run."
        .to_string()
}

fn parse_args() -> Result<CliOptions, EvalError> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut provider: Option<String> = None;
    let mut model: Option<String> = None;
    let mut binary = "pi".to_string();
    let mut artifact_dir = std::env::var("PI_EVAL_ARTIFACT_DIR")
        .ok()
        .map(PathBuf::from);
    let mut evals = Vec::new();
    let mut faux = false;
    let mut index = 0;

    let take_value =
        |args: &[String], index: &mut usize, flag: &str| -> Result<String, EvalError> {
            let value = args
                .get(*index + 1)
                .ok_or_else(|| EvalError::MissingFlagValue {
                    flag: flag.to_string(),
                })?;
            if value.starts_with('-') {
                return Err(EvalError::MissingFlagValue {
                    flag: flag.to_string(),
                });
            }
            *index += 1;
            Ok(value.clone())
        };

    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{}", usage());
                std::process::exit(0);
            }
            "--provider" => provider = Some(take_value(&args, &mut index, arg)?),
            "--model" => model = Some(take_value(&args, &mut index, arg)?),
            "--binary" => binary = take_value(&args, &mut index, arg)?,
            "--artifact-dir" => {
                artifact_dir = Some(PathBuf::from(take_value(&args, &mut index, arg)?))
            }
            "--faux" => faux = true,
            "--eval" => evals.push(take_value(&args, &mut index, arg)?),
            _ if arg.starts_with("--provider=") => {
                provider = Some(arg["--provider=".len()..].to_string())
            }
            _ if arg.starts_with("--model=") => model = Some(arg["--model=".len()..].to_string()),
            else_value => evals.push(else_value.to_string()),
        }
        index += 1;
    }

    if faux {
        provider = Some("faux".to_string());
        model = Some("faux-1".to_string());
    }

    // CLI --provider/--model must be supplied together and take precedence.
    let explicit = match (provider.clone(), model.clone()) {
        (Some(p), Some(m)) => Some(ModelSelection { provider: p, id: m }),
        (None, None) => None,
        _ => return Err(EvalError::PartialModelSelection),
    };
    let env_pair = std::env::var("PI_PROVIDER")
        .ok()
        .zip(std::env::var("PI_MODEL").ok());
    let selection = resolve_model_selection(
        explicit.as_ref(),
        env_pair.as_ref().map(|(p, m)| (p.as_str(), m.as_str())),
    )?;

    // Resolve a relative binary path against the current directory so the
    // subprocess (which runs in a fresh temp workspace) can find it.
    let binary = if binary.contains('/') || binary.contains('\\') {
        std::path::Path::new(&binary)
            .canonicalize()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or(binary)
    } else {
        binary
    };
    let runner = PiCliRunnerOptions {
        binary,
        provider: selection.provider,
        model: selection.id,
        no_tools: false,
        extra_args: Vec::new(),
        timeout_secs: 300,
    };
    let evals = if evals.is_empty() {
        vec!["all".to_string()]
    } else {
        evals
    };
    Ok(CliOptions {
        runner,
        artifact_dir,
        evals,
    })
}

fn main() {
    let options = match parse_args() {
        Ok(options) => options,
        Err(error) => {
            eprintln!("[eval] {error}");
            eprintln!("{}", usage());
            std::process::exit(1);
        }
    };
    eprintln!(
        "[eval] default-model={}/{}",
        options.runner.provider, options.runner.model
    );
    if let Some(dir) = &options.artifact_dir {
        eprintln!("[eval] artifacts={}", dir.display());
    }

    let mut observations: Vec<HarnessObservation> = Vec::new();
    let mut ran_any = false;
    for eval_name in &options.evals {
        match eval_name.to_lowercase().as_str() {
            "smoke" | "smoke.eval" => {
                run_smoke_eval(&options);
                ran_any = true;
            }
            "extensions" | "extensions.eval" => {
                observations.extend(run_extensions_eval(&options));
                ran_any = true;
            }
            "all" => {
                run_smoke_eval(&options);
                observations.extend(run_extensions_eval(&options));
                ran_any = true;
            }
            name => {
                eprintln!("[eval] unknown eval {name:?} (expected smoke, extensions, or all)");
                std::process::exit(1);
            }
        }
    }

    if !observations.is_empty() {
        let report = summarize_harness_comparisons(&observations);
        let formatted = format_harness_comparison_report(&report);
        if !formatted.is_empty() {
            println!("\n{formatted}");
        } else if ran_any {
            // Only diagnostic-free unpaired runs still need an empty report.
            eprintln!("\nEval comparisons unavailable.");
        }
    }
}

fn run_smoke_eval(options: &CliOptions) {
    let root = match create_eval_root() {
        Ok(root) => root,
        Err(error) => {
            eprintln!("[eval] {error}");
            return;
        }
    };
    let cwd = root.join("workspace");
    if let Err(error) = std::fs::create_dir_all(&cwd) {
        eprintln!("[eval] failed to create smoke workspace: {error}");
        return;
    }

    let prompt = pi_evals::evals::smoke::smoke_input();
    let prompt_text = prompt
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let outcome = pi_evals::evals::smoke::run_smoke(&options.runner, &cwd, prompt_text);
    let result = pi_evals::evals::smoke::assert_smoke_result(&options.runner, &outcome);

    match &result {
        Ok(_) => println!(
            "✓ Pi Coding Agent smoke: runs a basic prompt end to end\n  provider: {}, model: {}\n  output: {}",
            options.runner.provider, options.runner.model, outcome.output.trim()
        ),
        Err(failures) => {
            eprintln!("✗ Pi Coding Agent smoke: runs a basic prompt end to end\n    {failures}");
        }
    }

    record_run(
        options,
        TestRecord {
            id: "smoke".to_string(),
            file: pi_evals::evals::smoke::FILE.to_string(),
            name: pi_evals::evals::smoke::TEST_NAME.to_string(),
            full_name: pi_evals::evals::smoke::TEST_NAME.to_string(),
            status: if result.is_ok() { "passed" } else { "failed" }.to_string(),
        },
        "pi-coding-agent",
        &serde_json::json!({
            "provider": options.runner.provider,
            "model": options.runner.model,
            "inputTokens": outcome.usage.input_tokens,
            "outputTokens": outcome.usage.output_tokens,
            "totalTokens": outcome.usage.total_tokens,
            "toolCalls": outcome.usage.tool_calls,
            "metadata": HarnessUsage::from_session_usage(
                &outcome.usage,
                &options.runner.provider,
                &options.runner.model,
            )
            .metadata,
        }),
        outcome.session_jsonl.as_deref(),
    );
    let _ = std::fs::remove_dir_all(&root);
}

fn run_extensions_eval(options: &CliOptions) -> Vec<HarnessObservation> {
    let Some(runner) = (options.runner.provider != "faux").then_some(&options.runner) else {
        let boundary = extensions::unsupported_boundary(&options.runner)
            .unwrap_or_else(|| "extension authoring is unsupported".to_string());
        eprintln!(
            "— Pi extension authoring system prompt: skipped (unsupported: {boundary}; fixture-backed)"
        );
        // Record skipped observations for both harnesses so the summary
        // reports them honestly as unscorable diagnostics.
        return skipped_extension_observations(options);
    };

    let eval_set = pi_evals::evals::extensions::EVAL_SET;
    let table_inputs = vec![extensions::extension_input()];

    let baseline = extension_harness(extensions::BASELINE_NAME, runner, false);
    let candidate = extension_harness(extensions::CANDIDATE_NAME, runner, true);
    let rows = match eval_harness_table(
        eval_set,
        &pi_evals::harness_table::EvalHarnessTableOptions::pair(baseline, candidate),
    ) {
        Ok(rows) => rows,
        Err(error) => {
            eprintln!("[eval] {error}");
            return Vec::new();
        }
    };

    let mut observations = Vec::new();
    for row in &rows {
        for input_value in &table_inputs {
            let mut context = HarnessContext {
                artifacts: Default::default(),
                artifact_directory: options.artifact_dir.clone(),
            };
            let input_value = input_value.clone();
            let result = row.harness.run(&input_value, &mut context);
            record_extension_run(options, row.name.as_str(), &input_value, &result);
            // The extension harness encodes its judge score inside the
            // output under `score` (set by extension_harness's assert).
            let score = result.output.get("score").and_then(|v| v.as_f64());
            let observation = observation_from_run(
                eval_set,
                extensions::FILE,
                extensions::TEST_NAME,
                &input_value,
                &row.name,
                extensions::BASELINE_NAME,
                &[extensions::CANDIDATE_NAME.to_string()],
                row.repetition,
                &result,
                score,
            );
            observations.push(observation);
        }
    }
    observations
}

fn extension_harness(
    name: &'static str,
    runner: &PiCliRunnerOptions,
    _candidate: bool,
) -> pi_evals::harness::Harness<serde_json::Value> {
    let runner = runner.clone();
    pi_evals::harness::Harness::new(name, move |input, context| {
        let create_prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let use_prompt = input
            .get("usePrompt")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let steps = extensions::ExtensionSteps {
            create_prompt: create_prompt.to_string(),
            use_prompt: use_prompt.to_string(),
        };
        let eval_root = match create_eval_root() {
            Ok(root) => Some(root),
            Err(error) => {
                context.set_artifact(
                    "runId",
                    serde_json::json!(pi_evals::harness_table::short_id()),
                );
                return pi_evals::harness::HarnessResult {
                    output: serde_json::json!({
                        "response": String::new(),
                        "extensionSource": Option::<String>::None,
                        "score": Option::<f64>::None,
                        "rationale": Option::<String>::None,
                    }),
                    errors: vec![error.to_string()],
                    events: Vec::new(),
                    usage: pi_evals::harness::HarnessUsage::from_session_usage(
                        &pi_evals::session_usage::SessionUsage::default(),
                        &runner.provider,
                        &runner.model,
                    ),
                    artifacts: std::collections::BTreeMap::new(),
                    timings: None,
                };
            }
        };
        let cwd = eval_root.as_ref().map(|root| root.join("workspace"));
        if let Some(cwd) = &cwd {
            std::fs::create_dir_all(cwd).ok();
        }
        let outcome = match &cwd {
            Some(cwd) => extensions::run_extension_scenario(&runner, cwd, &steps),
            None => extensions::ExtensionOutcome {
                final_response: String::new(),
                extension_source: None,
                errors: vec!["failed to create the eval root directory".to_string()],
                session_jsonl: None,
                usage: pi_evals::session_usage::SessionUsage::default(),
            },
        };
        let run_id = outcome
            .session_jsonl
            .as_deref()
            .and_then(pi_evals::harness::session_id_from_session_jsonl)
            .unwrap_or_else(pi_evals::harness_table::short_id);
        let transcript = outcome
            .session_jsonl
            .as_deref()
            .map(pi_evals::harness::transcript_events_from_session_jsonl);
        let mut errors = outcome.errors.clone();
        let events = match transcript {
            Some(Ok(events)) => events,
            Some(Err(error)) => {
                errors.push(error.to_string());
                Vec::new()
            }
            None => Vec::new(),
        };
        context.set_artifact("runId", serde_json::json!(run_id));
        if let Some(session_jsonl) = &outcome.session_jsonl {
            context.set_artifact(
                PI_SESSION_SNAPSHOT_ARTIFACT,
                serde_json::json!(session_jsonl),
            );
        }
        let (score, rationale) = if errors.is_empty() {
            extensions::score_extension_result(&runner, &outcome)
        } else {
            (None, None)
        };
        let mut artifacts = std::collections::BTreeMap::new();
        artifacts.insert("runId".to_string(), serde_json::json!(run_id));
        if let Some(session_jsonl) = &outcome.session_jsonl {
            artifacts.insert(
                PI_SESSION_SNAPSHOT_ARTIFACT.to_string(),
                serde_json::json!(session_jsonl),
            );
        }
        let result = pi_evals::harness::HarnessResult {
            output: serde_json::json!({
                "response": outcome.final_response,
                "extensionSource": outcome.extension_source,
                "score": score,
                "rationale": rationale,
            }),
            errors,
            events,
            usage: pi_evals::harness::HarnessUsage::from_session_usage(
                &outcome.usage,
                &runner.provider,
                &runner.model,
            ),
            artifacts,
            timings: None,
        };
        if let Some(eval_root) = eval_root {
            let _ = std::fs::remove_dir_all(eval_root);
        }
        result
    })
}

/// Persist the extension harness's durable evidence using the same artifact
/// and run-report path as the smoke harness.  The source and transcript are
/// intentionally sourced from the returned harness result, not reconstructed
/// from the judge score.
fn record_extension_run(
    options: &CliOptions,
    harness: &str,
    input: &serde_json::Value,
    result: &pi_evals::harness::HarnessResult<serde_json::Value>,
) {
    let Some(artifact_directory) = &options.artifact_dir else {
        return;
    };
    let Some(run_id) = result
        .artifacts
        .get("runId")
        .and_then(|value| value.as_str())
    else {
        eprintln!("[eval] extension run did not produce a durable run ID");
        return;
    };

    let mut artifacts = Vec::new();
    match record_eval_session_artifact(&result.artifacts) {
        Ok(Some(artifact)) => artifacts.push(artifact),
        Ok(None) => {}
        Err(error) => eprintln!("[eval] failed to record extension session artifact: {error}"),
    }
    if let Some(source) = result
        .output
        .get("extensionSource")
        .and_then(|value| value.as_str())
    {
        artifacts.push(record_eval_source_artifact(
            run_id,
            Attachment {
                name: "hello.ts".to_string(),
                content_type: "text/typescript".to_string(),
                body: source.to_string(),
                body_encoding: "utf-8".to_string(),
            },
        ));
    }

    let usage = serde_json::to_value(&result.usage).unwrap_or_else(|_| serde_json::json!({}));
    let timings = result
        .timings
        .as_ref()
        .and_then(|timings| serde_json::to_value(timings).ok());
    let mut metadata = result.artifacts.clone();
    metadata.remove("runId");
    metadata.remove(PI_SESSION_SNAPSHOT_ARTIFACT);
    let group_key = pi_evals::harness_table::derive_eval_group_key(input, 1)
        .unwrap_or_else(|_| pi_evals::harness_table::short_id());
    let test = TestRecord {
        id: group_key,
        file: extensions::FILE.to_string(),
        name: extensions::TEST_NAME.to_string(),
        full_name: extensions::TEST_NAME.to_string(),
        status: if result.errors.is_empty() {
            "passed".to_string()
        } else {
            "failed".to_string()
        },
    };
    let report_options = ReporterOptions {
        artifact_directory: Some(artifact_directory.clone()),
    };
    if let Err(error) = append_harness_run_report(
        &report_options,
        run_id,
        test,
        harness,
        &usage,
        timings.as_ref(),
        &result.errors,
        &artifacts,
        metadata,
    ) {
        eprintln!("[eval] failed to append extension run report: {error}");
    }
}

fn skipped_extension_observations(options: &CliOptions) -> Vec<HarnessObservation> {
    let input = extensions::extension_input();
    let mut observations = Vec::new();
    for harness in [extensions::BASELINE_NAME, extensions::CANDIDATE_NAME] {
        for repetition in 1..=1u32 {
            observations.push(pi_evals::summary::HarnessObservation {
                eval_set: extensions::EVAL_SET.to_string(),
                group_key: pi_evals::harness_table::derive_eval_group_key(&input, repetition)
                    .unwrap_or_default(),
                test_name: extensions::TEST_NAME.to_string(),
                file: extensions::FILE.to_string(),
                harness: harness.to_string(),
                baseline: extensions::BASELINE_NAME.to_string(),
                candidates: vec![extensions::CANDIDATE_NAME.to_string()],
                repetition,
                outcome: Outcome::Skipped,
                score: None,
                total_tokens: None,
                total_ms: None,
                estimated_cost_usd: None,
            });
        }
    }
    let _ = options;
    observations
}

/// Appends a run record for a single-harness eval.
fn record_run(
    options: &CliOptions,
    test: TestRecord,
    harness: &str,
    usage: &serde_json::Value,
    session_body: Option<&str>,
) {
    let Some(artifact_dir) = &options.artifact_dir else {
        return;
    };
    let Some(session_body) = session_body else {
        return;
    };
    // The session header is the harness's durable run identity.  Reusing it
    // keeps the report, persisted attachment directory, and JSONL snapshot
    // correlated across restarts; only malformed/missing headers need the
    // reporter's random fallback.
    let run_id = pi_evals::harness::session_id_from_session_jsonl(session_body)
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(pi_evals::harness_table::short_id);
    let mut artifacts = std::collections::BTreeMap::new();
    artifacts.insert("runId".to_string(), serde_json::json!(run_id));
    artifacts.insert(
        pi_evals::artifacts::PI_SESSION_SNAPSHOT_ARTIFACT.to_string(),
        serde_json::json!(session_body),
    );
    let session_artifact = match record_eval_session_artifact(&artifacts) {
        Ok(Some(artifact)) => artifact,
        _ => return,
    };
    let report_options = ReporterOptions {
        artifact_directory: Some(artifact_dir.clone()),
    };
    let _ = append_harness_run_report(
        &report_options,
        &run_id,
        test,
        harness,
        usage,
        None,
        &[],
        &[EvalArtifact::Session {
            run_id: run_id.clone(),
            attachments: match session_artifact {
                EvalArtifact::Session { attachments, .. } => attachments,
                _ => return,
            },
        }],
        Default::default(),
    );
}

// Silence unused import when evals::EvalSet isn't constructed here.
#[allow(dead_code)]
fn _unused(_: EvalSet) {}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn record_run_reuses_the_persisted_session_id() {
        let root = create_eval_root().expect("create eval root");
        let artifact_dir = root.join("artifacts");
        let options = CliOptions {
            runner: PiCliRunnerOptions::default(),
            artifact_dir: Some(artifact_dir.clone()),
            evals: Vec::new(),
        };
        let session = "{\"kind\":\"header\",\"version\":4,\"id\":\"session-42\"}\n";

        record_run(
            &options,
            TestRecord {
                id: "test-id".to_string(),
                file: "eval.test.rs".to_string(),
                name: "records session".to_string(),
                full_name: "records session".to_string(),
                status: "passed".to_string(),
            },
            "pi-coding-agent",
            &serde_json::json!({ "totalTokens": 1 }),
            Some(session),
        );

        let report = std::fs::read_to_string(artifact_dir.join("runs.jsonl"))
            .expect("record_run writes a report");
        let record: serde_json::Value = serde_json::from_str(report.trim()).unwrap();
        assert_eq!(record["runId"], serde_json::json!("session-42"));
        assert_eq!(
            record["artifacts"][0]["name"],
            serde_json::json!("session.jsonl")
        );
        let attachment_path = record["artifacts"][0]["path"]
            .as_str()
            .expect("report contains the persisted attachment path");
        assert_eq!(
            std::fs::read_to_string(artifact_dir.join(attachment_path)).unwrap(),
            session
        );

        std::fs::remove_dir_all(root).unwrap();
    }
}
