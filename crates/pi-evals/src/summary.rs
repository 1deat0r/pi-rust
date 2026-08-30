//! Harness comparison summaries — port of
//! `packages/evals/src/vitest-evals/summary.ts`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Observation outcome (port of `HarnessObservationOutcome`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Scored,
    Unscored,
    Skipped,
    Pending,
    Errored,
}

/// A single harness observation (port of `HarnessObservation`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessObservation {
    pub eval_set: String,
    pub group_key: String,
    pub test_name: String,
    pub file: String,
    pub harness: String,
    pub baseline: String,
    pub candidates: Vec<String>,
    pub repetition: u32,
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
}

/// Paired metric summary (port of `PairedMetricSummary`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedMetricSummary {
    pub total_pairs: usize,
    pub eligible_pairs: usize,
    pub baseline_mean: Option<f64>,
    pub candidate_mean: Option<f64>,
    pub mean_delta: Option<f64>,
}

/// Correctness lift summary (port of `CorrectnessLiftSummary`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CorrectnessLiftSummary {
    pub total_pairs: usize,
    pub eligible_pairs: usize,
    pub baseline_pass_rate: Option<f64>,
    pub candidate_pass_rate: Option<f64>,
    pub lift: Option<f64>,
    pub baseline_wins: usize,
    pub candidate_wins: usize,
    pub ties: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessPairComparison {
    pub baseline: String,
    pub candidate: String,
    pub correctness: CorrectnessLiftSummary,
    pub total_tokens: PairedMetricSummary,
    pub total_ms: PairedMetricSummary,
    pub estimated_cost_usd: PairedMetricSummary,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessComparisonDiagnostic {
    pub eval_set: String,
    pub group_key: String,
    pub test_name: String,
    pub file: String,
    pub repetition: u32,
    pub harness: String,
    pub reason: DiagnosticReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiagnosticReason {
    MissingObservation,
    DuplicateObservation,
    HarnessError,
    MissingScore,
    UnscorableOutcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessEvalSetReport {
    pub eval_set: String,
    pub comparisons: Vec<HarnessPairComparison>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessComparisonReport {
    pub schema_version: u32,
    pub eval_sets: Vec<HarnessEvalSetReport>,
    pub diagnostics: Vec<HarnessComparisonDiagnostic>,
}

#[derive(Debug, Clone)]
struct HarnessDescriptor {
    name: String,
    index: usize,
}

#[derive(Debug, Clone)]
struct ObservationGroup {
    eval_set: String,
    group_key: String,
    test_name: String,
    file: String,
    repetition: u32,
    observations_by_harness: BTreeMap<String, Vec<HarnessObservation>>,
}

#[derive(Debug)]
struct EvalSetData {
    baseline: HarnessDescriptor,
    candidates_by_name: BTreeMap<String, HarnessDescriptor>,
    groups_by_key: BTreeMap<String, ObservationGroup>,
}

fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        None
    } else {
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }
}

/// `Number((left - right).toPrecision(15))` — mirrors upstream's lossy
/// rounding of computed deltas.
fn precise_difference(left: f64, right: f64) -> f64 {
    round_to_precision(left - right, 15)
}

fn round_to_precision(value: f64, sig_digits: i32) -> f64 {
    if value == 0.0 || !value.is_finite() {
        return value;
    }
    let magnitude = value.abs().log10().floor() as i32;
    let factor = 10f64.powi(sig_digits - 1 - magnitude);
    (value * factor).round() / factor
}

fn group_observations(observations: &[HarnessObservation]) -> BTreeMap<String, EvalSetData> {
    let mut eval_sets: BTreeMap<String, EvalSetData> = BTreeMap::new();
    for observation in observations {
        let eval_set = eval_sets
            .entry(observation.eval_set.clone())
            .or_insert_with(|| EvalSetData {
                baseline: HarnessDescriptor {
                    name: observation.baseline.clone(),
                    index: 0,
                },
                candidates_by_name: BTreeMap::new(),
                groups_by_key: BTreeMap::new(),
            });
        for (index, name) in observation.candidates.iter().enumerate() {
            let existing = eval_set.candidates_by_name.get(name);
            if existing
                .map(|candidate| index < candidate.index)
                .unwrap_or(true)
            {
                eval_set.candidates_by_name.insert(
                    name.clone(),
                    HarnessDescriptor {
                        name: name.clone(),
                        index,
                    },
                );
            }
        }
        let key = format!(
            "{}",
            serde_json::json!([
                observation.file,
                observation.test_name,
                observation.group_key
            ])
        );
        let group = eval_set
            .groups_by_key
            .entry(key)
            .or_insert_with(|| ObservationGroup {
                eval_set: observation.eval_set.clone(),
                group_key: observation.group_key.clone(),
                test_name: observation.test_name.clone(),
                file: observation.file.clone(),
                repetition: observation.repetition,
                observations_by_harness: BTreeMap::new(),
            });
        // When the first observation for a group has a different repetition
        // than later members, keep the first (mirror of upstream `||`) which
        // reports the declaration's repetition; the summary is keyed by
        // groupKey+file+testName so duplicates collapse.
        group
            .observations_by_harness
            .entry(observation.harness.clone())
            .or_default()
            .push(observation.clone());
    }
    eval_sets
}

fn ordered_harnesses(eval_set: &EvalSetData) -> Vec<HarnessDescriptor> {
    let mut result = vec![eval_set.baseline.clone()];
    let mut candidates: Vec<HarnessDescriptor> =
        eval_set.candidates_by_name.values().cloned().collect();
    candidates.sort_by(|left, right| {
        left.index
            .cmp(&right.index)
            .then_with(|| left.name.cmp(&right.name))
    });
    result.extend(candidates);
    result
}

fn ordered_candidates(eval_set: &EvalSetData) -> Vec<HarnessDescriptor> {
    let mut candidates: Vec<HarnessDescriptor> =
        eval_set.candidates_by_name.values().cloned().collect();
    candidates.sort_by(|left, right| {
        left.index
            .cmp(&right.index)
            .then_with(|| left.name.cmp(&right.name))
    });
    candidates
}

fn ordered_groups(eval_set: &EvalSetData) -> Vec<ObservationGroup> {
    let mut groups: Vec<ObservationGroup> = eval_set.groups_by_key.values().cloned().collect();
    groups.sort_by(|left, right| {
        left.group_key
            .cmp(&right.group_key)
            .then_with(|| left.repetition.cmp(&right.repetition))
    });
    groups
}

fn collect_diagnostics(
    harnesses: &[HarnessDescriptor],
    groups: &[ObservationGroup],
) -> Vec<HarnessComparisonDiagnostic> {
    let mut diagnostics = Vec::new();
    for group in groups {
        for harness in harnesses {
            let observations = group
                .observations_by_harness
                .get(&harness.name)
                .cloned()
                .unwrap_or_default();
            let reason = if observations.is_empty() {
                Some(DiagnosticReason::MissingObservation)
            } else if observations.len() > 1 {
                Some(DiagnosticReason::DuplicateObservation)
            } else if observations[0].outcome == Outcome::Errored {
                Some(DiagnosticReason::HarnessError)
            } else if observations[0].outcome == Outcome::Unscored {
                Some(DiagnosticReason::MissingScore)
            } else if observations[0].outcome != Outcome::Scored {
                Some(DiagnosticReason::UnscorableOutcome)
            } else {
                None
            };
            if let Some(reason) = reason {
                diagnostics.push(HarnessComparisonDiagnostic {
                    eval_set: group.eval_set.clone(),
                    group_key: group.group_key.clone(),
                    test_name: group.test_name.clone(),
                    file: group.file.clone(),
                    repetition: group.repetition,
                    harness: harness.name.clone(),
                    reason,
                });
            }
        }
    }
    diagnostics
}

#[derive(Clone)]
struct ObservationPair {
    baseline: HarnessObservation,
    candidate: HarnessObservation,
}

fn pair_observations(
    groups: &[ObservationGroup],
    baseline_harness: &str,
    candidate_harness: &str,
) -> Vec<ObservationPair> {
    let mut pairs = Vec::new();
    for group in groups {
        let baseline = group
            .observations_by_harness
            .get(baseline_harness)
            .cloned()
            .unwrap_or_default();
        let candidate = group
            .observations_by_harness
            .get(candidate_harness)
            .cloned()
            .unwrap_or_default();
        if baseline.len() == 1 && candidate.len() == 1 {
            pairs.push(ObservationPair {
                baseline: baseline[0].clone(),
                candidate: candidate[0].clone(),
            });
        }
    }
    pairs
}

fn summarize_metric(
    pairs: &[ObservationPair],
    select: &dyn Fn(&HarnessObservation) -> Option<f64>,
    total_pairs: usize,
) -> PairedMetricSummary {
    let mut baseline_values = Vec::new();
    let mut candidate_values = Vec::new();
    for pair in pairs {
        if pair.baseline.outcome != Outcome::Scored || pair.candidate.outcome != Outcome::Scored {
            continue;
        }
        let baseline_value = select(&pair.baseline);
        let candidate_value = select(&pair.candidate);
        let (Some(baseline_value), Some(candidate_value)) = (baseline_value, candidate_value)
        else {
            continue;
        };
        if !baseline_value.is_finite() || !candidate_value.is_finite() {
            continue;
        }
        baseline_values.push(baseline_value);
        candidate_values.push(candidate_value);
    }
    let baseline_mean = mean(&baseline_values);
    let candidate_mean = mean(&candidate_values);
    PairedMetricSummary {
        total_pairs,
        eligible_pairs: baseline_values.len(),
        baseline_mean,
        candidate_mean,
        mean_delta: match (baseline_mean, candidate_mean) {
            (Some(b), Some(c)) => Some(precise_difference(c, b)),
            _ => None,
        },
    }
}

fn summarize_correctness(pairs: &[ObservationPair], total_pairs: usize) -> CorrectnessLiftSummary {
    let mut eligible_pairs = 0usize;
    let mut baseline_passes = 0usize;
    let mut candidate_passes = 0usize;
    let mut baseline_wins = 0usize;
    let mut candidate_wins = 0usize;
    let mut ties = 0usize;

    for pair in pairs {
        if pair.baseline.outcome != Outcome::Scored || pair.candidate.outcome != Outcome::Scored {
            continue;
        }
        eligible_pairs += 1;
        let baseline_passed = pair.baseline.score.unwrap_or(0.0) >= 1.0;
        let candidate_passed = pair.candidate.score.unwrap_or(0.0) >= 1.0;
        if baseline_passed {
            baseline_passes += 1;
        }
        if candidate_passed {
            candidate_passes += 1;
        }
        if baseline_passed == candidate_passed {
            ties += 1;
        } else if baseline_passed {
            baseline_wins += 1;
        } else {
            candidate_wins += 1;
        }
    }

    let baseline_pass_rate = if eligible_pairs == 0 {
        None
    } else {
        Some(baseline_passes as f64 / eligible_pairs as f64)
    };
    let candidate_pass_rate = if eligible_pairs == 0 {
        None
    } else {
        Some(candidate_passes as f64 / eligible_pairs as f64)
    };
    CorrectnessLiftSummary {
        total_pairs,
        eligible_pairs,
        baseline_pass_rate,
        candidate_pass_rate,
        lift: match (baseline_pass_rate, candidate_pass_rate) {
            (Some(b), Some(c)) => Some(precise_difference(c, b)),
            _ => None,
        },
        baseline_wins,
        candidate_wins,
        ties,
    }
}

fn compare_harnesses(
    baseline: &HarnessDescriptor,
    candidate: &HarnessDescriptor,
    groups: &[ObservationGroup],
) -> HarnessPairComparison {
    let pairs = pair_observations(groups, &baseline.name, &candidate.name);
    HarnessPairComparison {
        baseline: baseline.name.clone(),
        candidate: candidate.name.clone(),
        correctness: summarize_correctness(&pairs, groups.len()),
        total_tokens: summarize_metric(&pairs, &|o| o.total_tokens, groups.len()),
        total_ms: summarize_metric(&pairs, &|o| o.total_ms, groups.len()),
        estimated_cost_usd: summarize_metric(&pairs, &|o| o.estimated_cost_usd, groups.len()),
    }
}

/// `summarizeHarnessComparisons`: groups observations by eval set / input and
/// reports each candidate against the declared baseline.
pub fn summarize_harness_comparisons(
    observations: &[HarnessObservation],
) -> HarnessComparisonReport {
    let mut eval_sets = Vec::new();
    let mut diagnostics: Vec<HarnessComparisonDiagnostic> = Vec::new();
    for (eval_set, data) in group_observations(observations) {
        let harnesses = ordered_harnesses(&data);
        let candidates = ordered_candidates(&data);
        let groups = ordered_groups(&data);
        eval_sets.push(HarnessEvalSetReport {
            eval_set,
            comparisons: candidates
                .iter()
                .map(|candidate| compare_harnesses(&data.baseline, candidate, &groups))
                .collect(),
        });
        diagnostics.extend(collect_diagnostics(&harnesses, &groups));
    }
    diagnostics.sort_by(|left, right| {
        left.eval_set
            .cmp(&right.eval_set)
            .then_with(|| left.file.cmp(&right.file))
            .then_with(|| left.group_key.cmp(&right.group_key))
            .then_with(|| left.repetition.cmp(&right.repetition))
            .then_with(|| left.harness.cmp(&right.harness))
    });
    HarnessComparisonReport {
        schema_version: 1,
        eval_sets,
        diagnostics,
    }
}

// ---------------------------------------------------------------------------
// Formatting (port of formatHarnessComparisonReport with ANSI styling)
// ---------------------------------------------------------------------------

mod ansi {
    pub const GRAY: &str = "\x1b[90m";
    pub const GREEN: &str = "\x1b[32m";
    pub const RED: &str = "\x1b[31m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const BOLD: &str = "\x1b[1m";
    pub const RESET: &str = "\x1b[0m";

    pub fn style(text: &str, color: &str) -> String {
        format!("{color}{text}{RESET}")
    }
    pub fn gray(text: &str) -> String {
        style(text, GRAY)
    }
    pub fn green(text: &str) -> String {
        style(text, GREEN)
    }
    pub fn red(text: &str) -> String {
        style(text, RED)
    }
    pub fn yellow(text: &str) -> String {
        style(text, YELLOW)
    }
    pub fn bold(text: &str) -> String {
        style(text, BOLD)
    }
}

fn format_percentage(value: Option<f64>) -> String {
    match value {
        None => "unavailable".to_string(),
        Some(value) => format!("{:.1}%", value * 100.0),
    }
}

fn format_signed(value: f64, fraction_digits: usize) -> String {
    if value >= 0.0 {
        format!("+{value:.*}", fraction_digits)
    } else {
        format!("{value:.*}", fraction_digits)
    }
}

fn format_coverage(eligible_pairs: usize, total_pairs: usize) -> String {
    ansi::gray(&format!("({eligible_pairs}/{total_pairs} pairs)"))
}

fn format_report_line(label: &str, value: String) -> String {
    let padded = format!("{label:>9}");
    format!("    {}  {value}", ansi::gray(&padded))
}

fn color_delta(value: f64, formatted: String, positive_is_better: bool) -> String {
    if value == 0.0 {
        ansi::gray(&formatted)
    } else {
        let improved = if positive_is_better {
            value > 0.0
        } else {
            value < 0.0
        };
        if improved {
            ansi::green(&formatted)
        } else {
            ansi::red(&formatted)
        }
    }
}

fn format_metric(
    label: &str,
    metric: &PairedMetricSummary,
    format_value: &dyn Fn(f64) -> String,
    format_delta: &dyn Fn(f64) -> String,
    comparison_pairs: usize,
) -> String {
    let coverage = if metric.eligible_pairs == 0 || metric.eligible_pairs == comparison_pairs {
        String::new()
    } else {
        format!(
            " {}",
            format_coverage(metric.eligible_pairs, metric.total_pairs)
        )
    };
    let (Some(baseline_mean), Some(candidate_mean), Some(mean_delta)) = (
        metric.baseline_mean,
        metric.candidate_mean,
        metric.mean_delta,
    ) else {
        return format_report_line(label, format!("{}{coverage}", ansi::yellow("unavailable")));
    };
    let delta = color_delta(mean_delta, format_delta(mean_delta), false);
    let values = ansi::gray(&format!(
        "(candidate {}, baseline {})",
        format_value(candidate_mean),
        format_value(baseline_mean)
    ));
    format_report_line(label, format!("{delta} {values}{coverage}"))
}

/// `formatHarnessComparisonReport`: renders the terminal comparison table.
pub fn format_harness_comparison_report(report: &HarnessComparisonReport) -> String {
    if report
        .eval_sets
        .iter()
        .all(|set| set.comparisons.is_empty())
    {
        return String::new();
    }
    let mut lines = vec![ansi::bold("Eval Comparisons")];
    for eval_set in &report.eval_sets {
        lines.push(format!("  {}", eval_set.eval_set));
        for (index, comparison) in eval_set.comparisons.iter().enumerate() {
            if index > 0 {
                lines.push(String::new());
            }
            let correctness = &comparison.correctness;
            lines.push(format_report_line("Baseline", comparison.baseline.clone()));
            lines.push(format_report_line(
                "Candidate",
                format!(
                    "{} {}",
                    comparison.candidate,
                    format_coverage(correctness.eligible_pairs, correctness.total_pairs)
                ),
            ));
            match correctness.lift {
                None => lines.push(format_report_line("Pass rate", ansi::yellow("unavailable"))),
                Some(lift) => {
                    let delta =
                        color_delta(lift, format!("{} pp", format_signed(lift * 100.0, 1)), true);
                    let values = ansi::gray(&format!(
                        "(candidate {}, baseline {})",
                        format_percentage(correctness.candidate_pass_rate),
                        format_percentage(correctness.baseline_pass_rate)
                    ));
                    lines.push(format_report_line("Pass rate", format!("{delta} {values}")));
                }
            }
            lines.push(format_metric(
                "Tokens",
                &comparison.total_tokens,
                &|value| format!("{value:.1}"),
                &|value| format_signed(value, 1),
                correctness.eligible_pairs,
            ));
            lines.push(format_metric(
                "Latency",
                &comparison.total_ms,
                &|value| format!("{value:.1}ms"),
                &|value| format!("{}ms", format_signed(value, 1)),
                correctness.eligible_pairs,
            ));
            lines.push(format_metric(
                "Est. cost",
                &comparison.estimated_cost_usd,
                &|value| format!("${value:.4}"),
                &|value| {
                    if value >= 0.0 {
                        format!("+${value:.4}")
                    } else {
                        format!("-${:.4}", value.abs())
                    }
                },
                correctness.eligible_pairs,
            ));
        }
    }
    if !report.diagnostics.is_empty() {
        lines.push(format!("  {}", ansi::yellow("Incomplete observations")));
        for diagnostic in &report.diagnostics {
            lines.push(format!(
                "    {}: {}/{} repetition {}, harness {}",
                reason_label(&diagnostic.reason),
                diagnostic.file,
                diagnostic.test_name,
                diagnostic.repetition,
                diagnostic.harness
            ));
        }
    }
    lines.join("\n")
}

fn reason_label(reason: &DiagnosticReason) -> &'static str {
    match reason {
        DiagnosticReason::MissingObservation => "missing-observation",
        DiagnosticReason::DuplicateObservation => "duplicate-observation",
        DiagnosticReason::HarnessError => "harness-error",
        DiagnosticReason::MissingScore => "missing-score",
        DiagnosticReason::UnscorableOutcome => "unscorable-outcome",
    }
}

/// Removes ANSI escape sequences (mirror of `stripVTControlCharacters` for
/// tests).
pub fn strip_ansi(text: &str) -> String {
    // The pattern is a compile-time literal; a compile failure here is a
    // build defect.
    #[allow(clippy::panic)]
    static ANSI_PATTERN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap_or_else(|error| panic!("static regex: {error}"))
    });
    ANSI_PATTERN.replace_all(text, "").into_owned()
}
