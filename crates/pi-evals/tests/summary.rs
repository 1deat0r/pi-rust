#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Port of `packages/evals/test/vitest-evals/summary.test.ts`.

use pi_evals::summary::{
    format_harness_comparison_report, strip_ansi, summarize_harness_comparisons,
    HarnessObservation, Outcome,
};

type ObservationResult = &'static str;

fn observation(
    harness: &str,
    test_name: &str,
    result: ObservationResult,
    metrics: Option<(Option<f64>, Option<f64>, Option<f64>)>,
    baseline: &str,
    candidates: Vec<String>,
) -> HarnessObservation {
    let (total_tokens, total_ms, estimated_cost_usd) = metrics.unwrap_or((None, None, None));
    let base = HarnessObservation {
        eval_set: "tool access".to_string(),
        group_key: format!(r#"[{test_name},1]"#),
        test_name: test_name.to_string(),
        file: "src/tool-access.eval.ts".to_string(),
        harness: harness.to_string(),
        baseline: baseline.to_string(),
        candidates,
        repetition: 1,
        outcome: Outcome::Unscored,
        score: None,
        total_tokens,
        total_ms,
        estimated_cost_usd,
    };
    if result == "passed" || result == "failed" {
        HarnessObservation {
            outcome: Outcome::Scored,
            score: Some(if result == "passed" { 1.0 } else { 0.0 }),
            ..base
        }
    } else {
        let outcome = match result {
            "errored" => Outcome::Errored,
            "unscored" => Outcome::Unscored,
            "skipped" => Outcome::Skipped,
            "pending" => Outcome::Pending,
            _ => panic!("unknown observation result: {result}"),
        };
        HarnessObservation {
            outcome,
            score: None,
            ..base
        }
    }
}

#[test]
fn computes_paired_correctness_lift_separately_from_efficiency_deltas() {
    let report = summarize_harness_comparisons(&[
        observation(
            "without-tools",
            "create",
            "failed",
            Some((Some(100.0), Some(1000.0), Some(0.01))),
            "without-tools",
            vec!["with-tools".into()],
        ),
        observation(
            "with-tools",
            "create",
            "passed",
            Some((Some(120.0), Some(800.0), Some(0.02))),
            "without-tools",
            vec!["with-tools".into()],
        ),
        observation(
            "without-tools",
            "inspect",
            "passed",
            Some((Some(200.0), None, None)),
            "without-tools",
            vec!["with-tools".into()],
        ),
        observation(
            "with-tools",
            "inspect",
            "passed",
            Some((Some(180.0), None, None)),
            "without-tools",
            vec!["with-tools".into()],
        ),
    ]);

    assert_eq!(report.eval_sets.len(), 1);
    let comparison = &report.eval_sets[0].comparisons[0];
    assert_eq!(comparison.baseline, "without-tools");
    assert_eq!(comparison.candidate, "with-tools");
    assert_eq!(
        comparison.correctness,
        pi_evals::summary::CorrectnessLiftSummary {
            total_pairs: 2,
            eligible_pairs: 2,
            baseline_pass_rate: Some(0.5),
            candidate_pass_rate: Some(1.0),
            lift: Some(0.5),
            baseline_wins: 0,
            candidate_wins: 1,
            ties: 1,
        }
    );
    assert_eq!(
        comparison.total_tokens,
        pi_evals::summary::PairedMetricSummary {
            total_pairs: 2,
            eligible_pairs: 2,
            baseline_mean: Some(150.0),
            candidate_mean: Some(150.0),
            mean_delta: Some(0.0),
        }
    );
    assert_eq!(
        comparison.total_ms,
        pi_evals::summary::PairedMetricSummary {
            total_pairs: 2,
            eligible_pairs: 1,
            baseline_mean: Some(1000.0),
            candidate_mean: Some(800.0),
            mean_delta: Some(-200.0),
        }
    );
    assert_eq!(
        comparison.estimated_cost_usd,
        pi_evals::summary::PairedMetricSummary {
            total_pairs: 2,
            eligible_pairs: 1,
            baseline_mean: Some(0.01),
            candidate_mean: Some(0.02),
            mean_delta: Some(0.01),
        }
    );
    assert!(report.diagnostics.is_empty());
}

#[test]
fn reports_missing_observations_without_coercing_them_to_failures_or_zero_telemetry() {
    let report = summarize_harness_comparisons(&[
        observation(
            "without-tools",
            "create",
            "failed",
            None,
            "without-tools",
            vec!["with-tools".into()],
        ),
        observation(
            "with-tools",
            "create",
            "passed",
            None,
            "without-tools",
            vec!["with-tools".into()],
        ),
        observation(
            "without-tools",
            "inspect",
            "passed",
            None,
            "without-tools",
            vec!["with-tools".into()],
        ),
    ]);
    let comparison = &report.eval_sets[0].comparisons[0];

    assert_eq!(
        comparison.correctness,
        pi_evals::summary::CorrectnessLiftSummary {
            total_pairs: 2,
            eligible_pairs: 1,
            baseline_pass_rate: Some(0.0),
            candidate_pass_rate: Some(1.0),
            lift: Some(1.0),
            baseline_wins: 0,
            candidate_wins: 1,
            ties: 0,
        }
    );
    assert_eq!(
        comparison.total_tokens,
        pi_evals::summary::PairedMetricSummary {
            total_pairs: 2,
            eligible_pairs: 0,
            baseline_mean: None,
            candidate_mean: None,
            mean_delta: None,
        }
    );
    assert!(report.diagnostics.iter().any(|d| d.test_name == "inspect"
        && d.harness == "with-tools"
        && d.reason == pi_evals::summary::DiagnosticReason::MissingObservation));
}

#[test]
fn keeps_identical_inputs_in_different_test_files_separate() {
    let report = summarize_harness_comparisons(&[
        observation(
            "without-tools",
            "shared",
            "failed",
            None,
            "without-tools",
            vec!["with-tools".into()],
        ),
        observation(
            "with-tools",
            "shared",
            "passed",
            None,
            "without-tools",
            vec!["with-tools".into()],
        ),
        {
            let mut o = observation(
                "without-tools",
                "shared",
                "passed",
                None,
                "without-tools",
                vec!["with-tools".into()],
            );
            o.file = "src/other.eval.ts".to_string();
            o
        },
        {
            let mut o = observation(
                "with-tools",
                "shared",
                "passed",
                None,
                "without-tools",
                vec!["with-tools".into()],
            );
            o.file = "src/other.eval.ts".to_string();
            o
        },
    ]);
    assert_eq!(
        report.eval_sets[0].comparisons[0].correctness.total_pairs,
        2
    );
    assert_eq!(
        report.eval_sets[0].comparisons[0]
            .correctness
            .eligible_pairs,
        2
    );
    assert!(report.diagnostics.is_empty());
}

#[test]
fn does_not_score_harness_errors_as_correctness_failures() {
    let report = summarize_harness_comparisons(&[
        observation(
            "without-tools",
            "create",
            "errored",
            Some((Some(100.0), None, None)),
            "without-tools",
            vec!["with-tools".into()],
        ),
        observation(
            "with-tools",
            "create",
            "passed",
            Some((Some(100.0), None, None)),
            "without-tools",
            vec!["with-tools".into()],
        ),
    ]);
    let comparison = &report.eval_sets[0].comparisons[0];
    assert_eq!(comparison.correctness.total_pairs, 1);
    assert_eq!(comparison.correctness.eligible_pairs, 0);
    assert_eq!(comparison.total_tokens.eligible_pairs, 0);
    assert!(report
        .diagnostics
        .iter()
        .any(|d| d.harness == "without-tools"
            && d.reason == pi_evals::summary::DiagnosticReason::HarnessError));
}

#[test]
fn does_not_derive_correctness_from_completed_tests_without_judge_scores() {
    let report = summarize_harness_comparisons(&[
        observation(
            "without-tools",
            "create",
            "unscored",
            None,
            "without-tools",
            vec!["with-tools".into()],
        ),
        observation(
            "with-tools",
            "create",
            "unscored",
            None,
            "without-tools",
            vec!["with-tools".into()],
        ),
    ]);
    assert_eq!(
        report.eval_sets[0].comparisons[0]
            .correctness
            .eligible_pairs,
        0
    );
    let reasons: Vec<(&str, pi_evals::summary::DiagnosticReason)> = report
        .diagnostics
        .iter()
        .map(|d| (d.harness.as_str(), d.reason.clone()))
        .collect();
    assert_eq!(
        reasons,
        vec![
            (
                "with-tools",
                pi_evals::summary::DiagnosticReason::MissingScore
            ),
            (
                "without-tools",
                pi_evals::summary::DiagnosticReason::MissingScore
            ),
        ]
    );
}

#[test]
fn compares_each_candidate_with_the_declared_baseline() {
    let candidates = vec!["second".to_string(), "third".to_string()];
    let report = summarize_harness_comparisons(&[
        observation(
            "first",
            "input",
            "passed",
            None,
            "first",
            candidates.clone(),
        ),
        observation(
            "second",
            "input",
            "passed",
            None,
            "first",
            candidates.clone(),
        ),
        observation(
            "third",
            "input",
            "passed",
            None,
            "first",
            candidates.clone(),
        ),
    ]);
    let pairs: Vec<(&str, &str)> = report.eval_sets[0]
        .comparisons
        .iter()
        .map(|c| (c.baseline.as_str(), c.candidate.as_str()))
        .collect();
    assert_eq!(pairs, vec![("first", "second"), ("first", "third")]);
}

#[test]
fn retains_a_declared_harness_with_no_completed_observations() {
    let report = summarize_harness_comparisons(&[observation(
        "without-tools",
        "create",
        "failed",
        None,
        "without-tools",
        vec!["with-tools".into()],
    )]);
    assert_eq!(report.eval_sets[0].comparisons.len(), 1);
    assert_eq!(
        report.eval_sets[0].comparisons[0]
            .correctness
            .eligible_pairs,
        0
    );
    assert!(report.diagnostics.iter().any(|d| d.test_name == "create"
        && d.harness == "with-tools"
        && d.reason == pi_evals::summary::DiagnosticReason::MissingObservation));
}

#[test]
fn reports_duplicate_and_unscorable_observations_once_across_multiple_harness_pairs() {
    let candidates = vec!["second".to_string(), "third".to_string()];
    let report = summarize_harness_comparisons(&[
        observation(
            "first",
            "duplicate",
            "passed",
            None,
            "first",
            candidates.clone(),
        ),
        observation(
            "first",
            "duplicate",
            "failed",
            None,
            "first",
            candidates.clone(),
        ),
        observation(
            "second",
            "duplicate",
            "passed",
            None,
            "first",
            candidates.clone(),
        ),
        observation(
            "third",
            "duplicate",
            "passed",
            None,
            "first",
            candidates.clone(),
        ),
        observation(
            "first",
            "skipped",
            "skipped",
            None,
            "first",
            candidates.clone(),
        ),
        observation(
            "second",
            "skipped",
            "passed",
            None,
            "first",
            candidates.clone(),
        ),
        observation(
            "third",
            "skipped",
            "passed",
            None,
            "first",
            candidates.clone(),
        ),
    ]);
    let duplicates: Vec<&str> = report
        .diagnostics
        .iter()
        .filter(|d| d.reason == pi_evals::summary::DiagnosticReason::DuplicateObservation)
        .map(|d| d.test_name.as_str())
        .collect();
    assert_eq!(duplicates, vec!["duplicate"]);
    let unscorable: Vec<&str> = report
        .diagnostics
        .iter()
        .filter(|d| d.reason == pi_evals::summary::DiagnosticReason::UnscorableOutcome)
        .map(|d| d.test_name.as_str())
        .collect();
    assert_eq!(unscorable, vec!["skipped"]);
}

#[test]
fn formats_lift_and_telemetry_availability_for_the_terminal_report() {
    let report = summarize_harness_comparisons(&[
        observation(
            "without-tools",
            "create",
            "failed",
            Some((None, Some(34853.7), None)),
            "without-tools",
            vec!["with-tools".into()],
        ),
        observation(
            "with-tools",
            "create",
            "passed",
            Some((None, Some(30694.2), None)),
            "without-tools",
            vec!["with-tools".into()],
        ),
    ]);
    let formatted = strip_ansi(&format_harness_comparison_report(&report));
    assert!(formatted.contains("Eval Comparisons"), "got: {formatted}");
    assert!(
        formatted.contains(" Baseline  without-tools"),
        "got: {formatted}"
    );
    assert!(
        formatted.contains("Candidate  with-tools (1/1 pairs)"),
        "got: {formatted}"
    );
    assert!(
        formatted.contains("Pass rate  +100.0 pp (candidate 100.0%, baseline 0.0%)"),
        "got: {formatted}"
    );
    assert!(
        formatted.contains("   Tokens  unavailable"),
        "got: {formatted}"
    );
    assert!(
        formatted.contains("  Latency  -4159.5ms (candidate 30694.2ms, baseline 34853.7ms)"),
        "got: {formatted}"
    );
}
