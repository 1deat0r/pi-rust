//! Port of `packages/evals/test/pi-harness.test.ts`.

use pi_evals::harness::{resolve_model_selection, ModelSelection};

#[test]
fn prefers_an_explicit_harness_model_over_environment_defaults() {
    let selection = resolve_model_selection(
        Some(&ModelSelection {
            provider: "anthropic".into(),
            id: "claude-opus-4-6".into(),
        }),
        Some(("openai-codex", "gpt-5.6-sol")),
    )
    .unwrap();
    assert_eq!(
        selection,
        ModelSelection {
            provider: "anthropic".into(),
            id: "claude-opus-4-6".into()
        }
    );
}

#[test]
fn uses_trimmed_environment_defaults_when_the_harness_has_no_explicit_model() {
    let selection =
        resolve_model_selection(None, Some((" openai-codex ", " gpt-5.6-sol "))).unwrap();
    assert_eq!(
        selection,
        ModelSelection {
            provider: "openai-codex".into(),
            id: "gpt-5.6-sol".into()
        }
    );
}

#[test]
fn rejects_an_incomplete_model_selection() {
    for case in [
        (None, None),
        (None, Some(("openai-codex", ""))),
        (None, Some(("", "gpt-5.6-sol"))),
        (Some(""), Some(("gpt-5.6-sol", "x"))),
    ] {
        let explicit = case.0.map(|p| ModelSelection {
            provider: p.to_string(),
            id: "x".into(),
        });
        let env = case.1;
        match resolve_model_selection(explicit.as_ref(), env) {
            Ok(selection) => panic!("expected rejection, got {selection:?}"),
            Err(message) => assert_eq!(
                message,
                "Select a harness model explicitly or set both PI_PROVIDER and PI_MODEL as defaults."
            ),
        }
    }
}
