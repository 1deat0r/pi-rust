#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

use pi_evals::session_usage::{parse_session_usage, SessionUsage};

const V4_FIXTURE: &str = include_str!("fixtures/session-usage-v4.jsonl");
const LEGACY_FIXTURE: &str = include_str!("fixtures/session-usage-legacy.jsonl");

#[test]
fn aggregates_upstream_usage_sources_from_v4_session_jsonl() {
    let usage = parse_session_usage(V4_FIXTURE).expect("v4 fixture parses");

    assert_eq!(usage.provider.as_deref(), Some("anthropic"));
    assert_eq!(usage.model.as_deref(), Some("claude-sonnet-response"));
    assert_eq!(usage.input_tokens, 113);
    assert_eq!(usage.output_tokens, 64);
    assert_eq!(usage.cache_read_tokens, 11);
    assert_eq!(usage.cache_write_tokens, 7);
    assert_eq!(usage.total_tokens, 195);
    assert_eq!(usage.tool_calls, 1);
    assert!((usage.estimated_cost_usd.expect("priced usage") - 0.0048).abs() < 1e-12);
}

#[test]
fn parses_legacy_session_entry_jsonl_and_omits_zero_cost() {
    let usage = parse_session_usage(LEGACY_FIXTURE).expect("legacy fixture parses");

    assert_eq!(usage.provider.as_deref(), Some("openai"));
    assert_eq!(usage.model.as_deref(), Some("gpt-4o"));
    assert_eq!(usage.total_tokens, 18);
    assert_eq!(usage.tool_calls, 0);
    assert_eq!(usage.estimated_cost_usd, None);
}

#[test]
fn accepts_rust_cost_field_names_as_well_as_upstream_names() {
    let usage = parse_session_usage(
        concat!(
            "{\"kind\":\"header\",\"version\":4,\"id\":\"x\",\"createdAt\":0,\"cwd\":\"/tmp\"}\n",
            "{\"kind\":\"entry\",\"type\":\"message\",\"id\":\"a\",\"seq\":1,\"parentId\":null,\"timestamp\":0,\"message\":{\"role\":\"assistant\",\"content\":[],\"provider\":\"faux\",\"model\":\"faux-1\",\"usage\":{\"input\":1,\"output\":2,\"cacheRead\":3,\"cacheWrite\":4,\"cost\":{\"input\":0.1,\"output\":0.2,\"cache_read\":0.3,\"cache_write\":0.4,\"total\":1.0}}}}\n",
        ),
    )
    .expect("rust cost fixture parses");

    assert_eq!(usage.total_tokens, 10);
    assert!((usage.estimated_cost_usd.expect("priced usage") - 1.0).abs() < 1e-12);
}

#[test]
fn rejects_invalid_session_jsonl_with_a_line_number() {
    let error = parse_session_usage(
        "{\"kind\":\"header\",\"version\":4,\"id\":\"x\",\"createdAt\":0,\"cwd\":\"/tmp\"}\nnot-json\n",
    )
    .expect_err("invalid JSONL must not become zero usage");

    assert!(
        error.to_string().contains("session JSONL line 2"),
        "{error}"
    );
}

#[test]
fn preserves_the_default_shape_when_a_session_has_no_billed_usage() {
    let usage = parse_session_usage(
        "{\"kind\":\"header\",\"version\":4,\"id\":\"x\",\"createdAt\":0,\"cwd\":\"/tmp\"}\n",
    )
    .expect("header-only session parses");

    assert_eq!(usage, SessionUsage::default());
}

#[test]
fn faux_extension_boundary_is_a_versioned_fixture_contract() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../src/evals/fixtures/extensions-faux-unsupported.json"
    ))
    .expect("faux extension fixture parses");

    assert_eq!(fixture["schemaVersion"], 1);
    assert_eq!(fixture["provider"], "faux");
    assert_eq!(fixture["scenario"], "extension-authoring");
    assert_eq!(fixture["supported"], false);
    assert_eq!(fixture["expected"]["successfulHelloCalls"], 0);
    assert_eq!(fixture["expected"]["responsePrefix"], "faux response to:");
}
