#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Wave B2 RED tests: harness deferred option, text-line reader, session values.

use pi_agent::harness::run_context::RunContext;

#[test]
fn harness_stream_options_carry_deferred_continuation_request() {
    // Upstream 0.85.1 `AgentHarnessStreamOptions.deferred`:
    // `boolean | { window?: "15m" | "1h" | "24h" }`. The Rust harness
    // `StreamOptions` is `pi_ai`'s `SimpleStreamOptions`, whose `deferred`
    // must accept the window form. RED: no harness-level constructor exists.
    let options = pi_agent::harness::stream_options_with_deferred_window("1h");
    assert_eq!(
        options.deferred,
        Some(pi_ai::types::DeferredOption::Window(
            pi_ai::types::DeferredWindow::H1
        ))
    );
    // Unknown windows fall back to the 1h default window.
    let fallback = pi_agent::harness::stream_options_with_deferred_window("9h");
    assert_eq!(
        fallback.deferred,
        Some(pi_ai::types::DeferredOption::Window(
            pi_ai::types::DeferredWindow::H1
        ))
    );
}

#[test]
fn text_line_reader_preserves_final_line_termination() {
    let lines = pi_agent::harness::text_lines::split_text_lines("a\nb");
    assert_eq!(
        lines,
        vec![
            pi_agent::harness::text_lines::TextLine {
                text: "a".to_string(),
                terminated: true,
            },
            pi_agent::harness::text_lines::TextLine {
                text: "b".to_string(),
                terminated: false,
            },
        ]
    );
}

#[test]
fn session_value_addresses_validate_and_build_writes() {
    use pi_agent::session::values::{
        append_list, delete_list, delete_value, list, resolve_list_read_options, set_value, value,
        ListReadOptions,
    };
    let address = value("pi.session.name", "").expect("valid address");
    assert_eq!(address.namespace, "pi.session.name");
    let write = set_value(&address, serde_json::json!("demo"));
    assert_eq!(write.op, "set");
    assert!(delete_value(&address).op == "delete");
    let items = list("pi.pending.assistant_frame", "op:resp").expect("valid address");
    assert_eq!(append_list(&items, serde_json::json!(1)).op, "append");
    assert_eq!(delete_list(&items).op, "delete");
    let resolved = resolve_list_read_options(ListReadOptions {
        limit: Some(5),
        ..Default::default()
    })
    .expect("valid limit");
    assert_eq!(resolved.limit, 5);
    assert_eq!(resolved.order, "asc");
    let _ = RunContext::background();
}

#[test]
fn filesystem_reads_honor_context_cancellation() {
    use pi_agent::fs::{FileSystem, MemoryFs};
    let fs = MemoryFs::new();
    fs.write_file("/a.txt", "hello").unwrap();
    let live = RunContext::background();
    assert_eq!(
        fs.read_text_file_with_context("/a.txt", &live).unwrap(),
        "hello"
    );
    let cancelled = RunContext::background();
    cancelled.cancel();
    let error = fs
        .read_text_file_with_context("/a.txt", &cancelled)
        .expect_err("cancelled context must fail the read");
    assert!(error.to_string().contains("abort"), "got: {error}");
}
