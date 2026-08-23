//! `pi-evals` runner CLI — port of `packages/evals/scripts/run-evals.mjs`.
//!
//! Runs the eval suite against the real `pi` binary and prints results.

use std::path::PathBuf;

use pi_evals::artifacts::{record_eval_session_artifact, EvalArtifact, TestRecord};
use pi_evals::evals::extensions;
use pi_evals::evals::{observation_from_run, EvalSet};
use pi_evals::harness::{
    create_eval_root, resolve_model_selection, HarnessContext, ModelSelection, PiCliRunnerOptions,
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

fn parse_args() -> Result<CliOptions, String> {
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

    let take_value = |args: &[String], index: &mut usize, flag: &str| -> Result<String, String> {
        let value = args
            .get(*index + 1)
            .ok_or_else(|| format!("Missing value for {flag}"))?;
        if value.starts_with('-') {
            return Err(format!("Missing value for {flag}"));
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
        _ => {
            return Err("CLI model selection requires both --provider and --model.".to_string());
        }
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
    let root = create_eval_root();
    let cwd = root.join("workspace");
    std::fs::create_dir_all(&cwd).expect("create smoke workspace");

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
        &root,
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
            "totalTokens": 0,
            "toolCalls": 0,
        }),
        &outcome.output,
    );
}

fn run_extensions_eval(options: &CliOptions) -> Vec<HarnessObservation> {
    let Some(runner) = (options.runner.provider != "faux").then_some(&options.runner) else {
        eprintln!("— Pi extension authoring system prompt: skipped (faux provider cannot author extensions)");
        // Record skipped observations for both harnesses so the summary
        // reports them honestly as unscorable diagnostics.
        return skipped_extension_observations(options);
    };

    let eval_set = pi_evals::evals::extensions::EVAL_SET;
    let table_inputs = vec![extensions::extension_input()];

    let baseline = extension_harness(extensions::BASELINE_NAME, runner, false);
    let candidate = extension_harness(extensions::CANDIDATE_NAME, runner, true);
    let rows = eval_harness_table(
        eval_set,
        &pi_evals::harness_table::EvalHarnessTableOptions::pair(baseline, candidate),
    )
    .expect("extension harness table");

    let mut observations = Vec::new();
    for row in &rows {
        for input_value in &table_inputs {
            let mut context = HarnessContext {
                artifacts: Default::default(),
                artifact_directory: options.artifact_dir.clone(),
            };
            let input_value = input_value.clone();
            let result = row.harness.run(&input_value, &mut context);
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
    pi_evals::harness::Harness::new(name, move |input, _context| {
        let create_prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let use_prompt = input
            .get("usePrompt")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let cwd = create_eval_root().join("workspace");
        std::fs::create_dir_all(&cwd).ok();
        let steps = extensions::ExtensionSteps {
            create_prompt: create_prompt.to_string(),
            use_prompt: use_prompt.to_string(),
        };
        let outcome = extensions::run_extension_scenario(&runner, &cwd, &steps);
        let assertion = extensions::assert_extension_result(&runner, &outcome);
        let mut errors = outcome.errors.clone();
        let score = match assertion {
            Ok(score) => Some(score),
            Err(error) => {
                errors.push(error);
                None
            }
        };
        pi_evals::harness::HarnessResult {
            output: serde_json::json!({
                "response": outcome.final_response,
                "extensionSource": outcome.extension_source,
                "score": score,
            }),
            errors,
            events: Vec::new(),
            usage: pi_evals::harness::HarnessUsage {
                provider: runner.provider.clone(),
                model: runner.model.clone(),
                total_tokens: 0,
                ..Default::default()
            },
            artifacts: Default::default(),
            timings: None,
        }
    })
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
#[allow(clippy::too_many_arguments)]
fn record_run(
    options: &CliOptions,
    root: &std::path::Path,
    test: TestRecord,
    harness: &str,
    usage: &serde_json::Value,
    session_body: &str,
) {
    let Some(artifact_dir) = &options.artifact_dir else {
        return;
    };
    let run_id = pi_evals::harness_table::short_id();
    let mut artifacts = std::collections::BTreeMap::new();
    artifacts.insert("runId".to_string(), serde_json::json!(run_id));
    artifacts.insert(
        pi_evals::artifacts::PI_SESSION_SNAPSHOT_ARTIFACT.to_string(),
        serde_json::json!(session_body),
    );
    let session_artifact = match record_eval_session_artifact(&artifacts) {
        Ok(Some(artifact)) => artifact,
        _ => {
            let _ = root;
            return;
        }
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
