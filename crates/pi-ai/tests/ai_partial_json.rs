#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use pi_ai::partial_json::{parse_partial_json, parse_streaming_json, repair_json};
use serde_json::{json, Value};

#[test]
fn pinned_upstream_oracle_covers_nested_escapes_unicode_and_numbers() {
    let rows: &[(&str, Value)] = &[
        (
            r#"{"outer":{"items":[1,{"text":"A\nB"#,
            json!({"outer":{"items":[1,{"text":"A\nB"}]}}),
        ),
        (
            r#"{"outer":{"items":[1,{"emoji":"\u2603"#,
            json!({"outer":{"items":[1,{"emoji":"☃"}]}}),
        ),
        (
            r#"{"outer":{"items":[1,{"emoji":"\uD83D\uDE80"#,
            json!({"outer":{"items":[1,{"emoji":"🚀"}]}}),
        ),
        (
            r#"{"outer":{"items":[true,nul"#,
            json!({"outer":{"items":[true,null]}}),
        ),
        (
            r#"{"outer":{"tail":-12.5e"#,
            json!({"outer":{"tail":-12.5}}),
        ),
        (
            r#"{"outer":{"tail":-12.5e+2"#,
            json!({"outer":{"tail":-1250.0}}),
        ),
        ("{\"raw\":\"x\u{0001}y\"}", json!({"raw":"x\u{0001}y"})),
        (r#"{"path":"c:\tmp\q"}"#, json!({"path":"c:\tmp\\q"})),
        (
            r#"[{"a":[{"b":"quoted \" text"#,
            json!([{"a":[{"b":"quoted \" text"}]}]),
        ),
    ];

    for (input, expected) in rows {
        assert_eq!(
            parse_streaming_json(input),
            *expected,
            "pinned partial-json@0.1.7 oracle mismatch for {input:?}"
        );
    }
}

#[test]
fn every_utf8_truncation_boundary_is_bounded_and_final_value_is_exact() {
    let samples = [
        r#"{"outer":{"items":[1,{"text":"A\nB","snow":"\u2603"},true,null],"tail":-12.5e+2}}"#,
        r#"[{"a":[{"b":"quoted \" text","literal":"雪🚀"}]}]"#,
        "{\"raw\":\"x\u{0001}y\",\"badEscape\":\"c:\\\\tmp\\q\"}",
    ];

    for sample in samples {
        for (index, _) in sample
            .char_indices()
            .chain(std::iter::once((sample.len(), '\0')))
        {
            let parsed = std::panic::catch_unwind(|| parse_streaming_json(&sample[..index]));
            let value =
                parsed.unwrap_or_else(|_| panic!("parser panicked at byte {index}: {sample:?}"));
            serde_json::to_string(&value).expect("every partial result remains serializable JSON");
        }
        assert_eq!(
            parse_streaming_json(sample),
            serde_json::from_str::<Value>(&repair_json(sample)).expect("repaired final JSON")
        );
    }
}

#[test]
fn provider_shaped_tool_argument_deltas_progress_and_final_exact_json_wins() {
    let deltas = [
        r#"{"path":"README.md""#,
        r#", "content":"line 1\n"#,
        r#"line 2", "options":{"dryRun":tru"#,
        r#"e, "tags":["雪","🚀"]}}"#,
    ];
    let expected = [
        json!({"path":"README.md"}),
        json!({"path":"README.md","content":"line 1\n"}),
        json!({"path":"README.md","content":"line 1\nline 2","options":{"dryRun":true}}),
        json!({"path":"README.md","content":"line 1\nline 2","options":{"dryRun":true,"tags":["雪","🚀"]}}),
    ];

    let mut accumulated = String::new();
    for (delta, expected) in deltas.into_iter().zip(expected) {
        accumulated.push_str(delta);
        assert_eq!(parse_streaming_json(&accumulated), expected);
    }

    let authoritative =
        r#"{"path":"README.md","content":"server corrected","options":{"dryRun":false,"tags":[]}}"#;
    assert_eq!(
        parse_streaming_json(authoritative),
        json!({"path":"README.md","content":"server corrected","options":{"dryRun":false,"tags":[]}})
    );
}

#[test]
fn malformed_fragments_fail_closed_without_accepting_trailing_junk() {
    for fragment in [
        "-",
        "12.",
        "tru\"e",
        "{unquoted:1}",
        "[1 2]",
        r#"{"a":1} trailing"#,
    ] {
        assert_eq!(parse_streaming_json(fragment), json!({}), "{fragment:?}");
    }
    assert_eq!(
        parse_streaming_json(r#""bad\xescape""#),
        json!("bad\\xescape")
    );
    assert!(parse_partial_json("12.").is_err());
    assert!(parse_partial_json(r#"{"a":1} trailing"#).is_err());
}
