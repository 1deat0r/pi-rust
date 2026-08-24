use pi_evals::evals::extensions::{
    faux_extension_fixture, score_extension_result, unsupported_boundary, ExtensionOutcome,
};
use pi_evals::harness::PiCliRunnerOptions;
use pi_evals::session_usage::SessionUsage;

fn runner(provider: &str) -> PiCliRunnerOptions {
    PiCliRunnerOptions {
        provider: provider.to_string(),
        ..PiCliRunnerOptions::default()
    }
}

fn outcome(source: Option<&str>, response: &str, errors: &[&str]) -> ExtensionOutcome {
    ExtensionOutcome {
        final_response: response.to_string(),
        extension_source: source.map(str::to_string),
        errors: errors.iter().map(|error| (*error).to_string()).collect(),
        session_jsonl: None,
        usage: SessionUsage::default(),
    }
}

#[test]
fn extension_judge_is_scorable_and_deterministic() {
    let source = r#"
        import { registerTool } from "@earendil-works/pi-coding-agent";
        registerTool({ name: "hello", description: "greeting" });
    "#;
    let (score, rationale) = score_extension_result(
        &runner("anthropic"),
        &outcome(Some(source), "Hello, Bob!", &[]),
    );

    assert_eq!(score, Some(1.0));
    assert_eq!(rationale, None);
}

#[test]
fn extension_judge_turns_assertion_failure_into_zero_score() {
    let (score, rationale) = score_extension_result(
        &runner("anthropic"),
        &outcome(Some("export const hello = true;"), "not the greeting", &[]),
    );

    assert_eq!(score, Some(0.0));
    let rationale = rationale.expect("failed assertions have a rationale");
    assert!(rationale.contains("canonical"), "{rationale}");
    assert!(rationale.contains("Hello, Bob!"), "{rationale}");
}

#[test]
fn extension_harness_errors_remain_unscorable() {
    let (score, rationale) = score_extension_result(
        &runner("anthropic"),
        &outcome(None, "", &["subprocess failed"]),
    );

    assert_eq!(score, None);
    assert_eq!(rationale, None);
}

#[test]
fn faux_extension_boundary_comes_from_the_fixture() {
    let boundary = unsupported_boundary(&runner("faux")).expect("faux is unsupported");
    let fixture = faux_extension_fixture();

    assert_eq!(fixture.schema_version, 1);
    assert_eq!(fixture.scenario, "extension-authoring");
    assert!(boundary.contains(&fixture.reason));
    assert!(boundary.contains("fixture schema 1: extension-authoring"));
    assert!(unsupported_boundary(&runner("anthropic")).is_none());
}
