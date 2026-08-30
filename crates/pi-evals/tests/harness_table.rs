#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Deterministic harness-table artifact propagation checks.

use std::collections::BTreeMap;

use pi_evals::harness::{Harness, HarnessContext, HarnessResult, HarnessUsage};
use pi_evals::harness_table::{
    eval_harness_table, parse_eval_harness_iteration_artifact, EvalHarnessTableOptions,
    EVAL_HARNESS_ITERATION_ARTIFACT,
};

#[test]
fn wrapper_merges_context_and_result_artifacts_with_upstream_precedence() {
    let harness = Harness::new("baseline", |_, context| {
        context.set_artifact("contextOnly", serde_json::json!("from-context"));
        context.set_artifact("collision", serde_json::json!("context"));
        let mut artifacts = BTreeMap::new();
        artifacts.insert("collision".to_string(), serde_json::json!("from-result"));
        artifacts.insert("resultOnly".to_string(), serde_json::json!(true));
        HarnessResult {
            output: serde_json::json!("ok"),
            errors: Vec::new(),
            events: Vec::new(),
            usage: HarnessUsage::default(),
            artifacts,
            timings: None,
        }
    });
    let candidate = Harness::new("candidate", |_, _| HarnessResult {
        output: serde_json::json!("candidate"),
        errors: Vec::new(),
        events: Vec::new(),
        usage: HarnessUsage::default(),
        artifacts: BTreeMap::new(),
        timings: None,
    });
    let row = eval_harness_table(
        "artifact propagation",
        &EvalHarnessTableOptions::pair(harness, candidate),
    )
    .unwrap()
    .remove(0);

    let result = row.harness.run(
        &serde_json::json!({ "id": "case-1" }),
        &mut HarnessContext::default(),
    );
    assert_eq!(
        result.artifacts.get("contextOnly"),
        Some(&serde_json::json!("from-context"))
    );
    assert_eq!(
        result.artifacts.get("collision"),
        Some(&serde_json::json!("from-result"))
    );
    assert_eq!(
        result.artifacts.get("resultOnly"),
        Some(&serde_json::json!(true))
    );
    let iteration = parse_eval_harness_iteration_artifact(
        result.artifacts.get(EVAL_HARNESS_ITERATION_ARTIFACT),
    )
    .expect("wrapper always attaches iteration metadata");
    assert_eq!(iteration.eval_set, "artifact propagation");
    assert_eq!(iteration.group_key, "[\"case-1\",1]");
}
