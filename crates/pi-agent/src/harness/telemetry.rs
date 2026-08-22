//! Agent telemetry schemas — port of `packages/agent/src/harness/telemetry.ts`.
//!
//! The upstream module defines two typed span vocabularies (AI-request and
//! harness operation spans) plus span-start helpers. TypeScript's compile-time
//! schema types (`AiSpanName`, `HarnessSpanName`, ...) have no runtime
//! counterpart; this port exposes the schema data as `serde_json::Value`
//! (structurally identical to the upstream consts) and provides the
//! `start_*` helpers over `pi_telemetry::TelemetryContext`.

use std::collections::BTreeMap;

/// Hook names traceable under `pi.harness.hook` (upstream `HOOK_NAMES`).
pub const HOOK_NAMES: [&str; 11] = [
    "before_run",
    "before_resume",
    "before_run_end",
    "transform_context",
    "before_request",
    "before_payload",
    "after_response",
    "before_tool",
    "after_tool",
    "before_compaction",
    "before_navigation",
];

/// Harness event types traced under `pi.harness.event_handler` (upstream
/// `EVENT_TYPES`).
pub const EVENT_TYPES: [&str; 29] = [
    "run_start",
    "run_resume",
    "run_suspend",
    "run_abort",
    "run_end",
    "fault",
    "handler_error",
    "turn_start",
    "turn_end",
    "retry_scheduled",
    "retry_start",
    "retry_end",
    "message_start",
    "message_update",
    "message_end",
    "tool_start",
    "tool_update",
    "tool_end",
    "entry_added",
    "write_pending",
    "queue_update",
    "fact_update",
    "config_update",
    "compaction_start",
    "compaction_end",
    "navigation_start",
    "navigation_end",
    "lane_created",
    "usage",
];

/// Combined typed span vocabulary for AI-request and harness telemetry
/// (upstream `AGENT_TELEMETRY_SCHEMAS`).
pub fn agent_telemetry_schemas() -> Vec<serde_json::Value> {
    vec![ai_telemetry_schema(), harness_telemetry_schema()]
}

pub fn ai_telemetry_schema() -> serde_json::Value {
    serde_json::json!({"version": 1, "spans": {"pi.ai.request": {"description": "One logical request to an AI provider", "parents": {"kind": "any"}, "startAttributes": {"pi.ai.operation": {"type": "string", "required": true, "values": ["stream", "fetch_deferred", "cancel_deferred", "generate_images"], "description": "Logical provider operation"}, "pi.ai.provider": {"type": "string", "required": true, "description": "Selected provider id"}, "pi.ai.model": {"type": "string", "required": true, "description": "Requested model id"}, "pi.ai.api": {"type": "string", "required": true, "description": "Provider API id"}, "pi.ai.streaming": {"type": "boolean", "required": true, "description": "Whether this operation returns a stream"}, "pi.ai.deferred": {"type": "boolean", "required": false, "description": "Whether the operation requests or participates in deferred execution"}}, "endAttributes": {"pi.ai.response.model": {"type": "string", "description": "Concrete response model"}, "pi.ai.response.id": {"type": "string", "cardinality": "high", "description": "Provider response id"}, "pi.ai.response.stop_reason": {"type": "string", "values": ["stop", "length", "tool_use", "error", "aborted", "deferred"], "description": "Normalized terminal response reason"}, "pi.ai.http.status_code": {"type": "number", "description": "Final HTTP status"}, "pi.ai.usage.input_tokens": {"type": "number", "description": "Reported input tokens"}, "pi.ai.usage.output_tokens": {"type": "number", "description": "Reported output tokens"}, "pi.ai.usage.cache_read_tokens": {"type": "number", "description": "Reported cache-read tokens"}, "pi.ai.usage.cache_write_tokens": {"type": "number", "description": "Reported cache-write tokens"}, "pi.ai.usage.reasoning_tokens": {"type": "number", "description": "Reported reasoning tokens"}, "pi.ai.usage.total_tokens": {"type": "number", "description": "Reported total tokens"}, "pi.ai.usage.cost": {"type": "number", "description": "Reported total cost"}, "pi.ai.stream.chunk_count": {"type": "number", "description": "Streamed update chunk count"}, "pi.ai.stream.time_to_first_chunk_ms": {"type": "number", "description": "Elapsed milliseconds to first update chunk"}, "pi.ai.error.type": {"type": "string", "cardinality": "low", "description": "Provider or transport error class"}}, "status": {"default": "ok", "errorWhen": "The operation throws or returns an error result"}}}})
}

pub fn harness_telemetry_schema() -> serde_json::Value {
    serde_json::json!({"version": 1, "spans": {"pi.harness.run": {"description": "One admitted in-process run invocation", "parents": {"kind": "root_or_external"}, "startAttributes": {"pi.session.id": {"type": "string", "required": true, "cardinality": "high", "description": "Session id"}, "pi.lane.name": {"type": "string", "required": true, "cardinality": "high", "description": "Lane name"}, "pi.operation.id": {"type": "string", "required": true, "cardinality": "high", "description": "Durable operation id"}, "pi.operation.recovery": {"type": "boolean", "required": true, "description": "Whether this invocation resumes durable work"}, "pi.operation.kind": {"type": "string", "required": true, "values": ["run"], "description": "Run operation kind"}}, "endAttributes": {"pi.operation.outcome": {"type": "string", "values": ["completed", "aborted", "failed", "suspended"], "description": "Run invocation outcome"}, "pi.error.code": {"type": "string", "cardinality": "low", "description": "Stable operation error code"}, "pi.error.type": {"type": "string", "cardinality": "low", "description": "Low-cardinality operation error class"}}, "status": {"default": "ok", "errorWhen": "The run fails or throws"}}, "pi.harness.compaction": {"description": "One admitted in-process manual compaction invocation", "parents": {"kind": "root_or_external"}, "startAttributes": {"pi.session.id": {"type": "string", "required": true, "cardinality": "high", "description": "Session id"}, "pi.lane.name": {"type": "string", "required": true, "cardinality": "high", "description": "Lane name"}, "pi.operation.id": {"type": "string", "required": true, "cardinality": "high", "description": "Durable operation id"}, "pi.operation.recovery": {"type": "boolean", "required": true, "description": "Whether this invocation resumes durable work"}, "pi.operation.kind": {"type": "string", "required": true, "values": ["compaction"], "description": "Compaction operation kind"}}, "endAttributes": {"pi.operation.outcome": {"type": "string", "values": ["completed", "declined", "aborted", "failed"], "description": "Compaction invocation outcome"}, "pi.error.code": {"type": "string", "cardinality": "low", "description": "Stable operation error code"}, "pi.error.type": {"type": "string", "cardinality": "low", "description": "Low-cardinality operation error class"}}, "status": {"default": "ok", "errorWhen": "The compaction fails or throws"}}, "pi.harness.navigation": {"description": "One admitted in-process navigation invocation", "parents": {"kind": "root_or_external"}, "startAttributes": {"pi.session.id": {"type": "string", "required": true, "cardinality": "high", "description": "Session id"}, "pi.lane.name": {"type": "string", "required": true, "cardinality": "high", "description": "Lane name"}, "pi.operation.id": {"type": "string", "required": true, "cardinality": "high", "description": "Durable operation id"}, "pi.operation.recovery": {"type": "boolean", "required": true, "description": "Whether this invocation resumes durable work"}, "pi.operation.kind": {"type": "string", "required": true, "values": ["navigation"], "description": "Navigation operation kind"}}, "endAttributes": {"pi.operation.outcome": {"type": "string", "values": ["completed", "declined", "aborted", "failed"], "description": "Navigation invocation outcome"}, "pi.error.code": {"type": "string", "cardinality": "low", "description": "Stable operation error code"}, "pi.error.type": {"type": "string", "cardinality": "low", "description": "Low-cardinality operation error class"}}, "status": {"default": "ok", "errorWhen": "The navigation fails or throws"}}, "pi.harness.checkpoint": {"description": "One run checkpoint", "parents": {"kind": "spans", "spans": ["pi.harness.run"]}, "startAttributes": {"pi.lane.name": {"type": "string", "required": true, "cardinality": "high", "description": "Lane name"}, "pi.operation.id": {"type": "string", "required": true, "cardinality": "high", "description": "Durable operation id"}, "pi.checkpoint.kind": {"type": "string", "required": true, "values": ["normal", "failure_drain", "abort_reconcile"], "description": "Checkpoint purpose"}}, "endAttributes": {}, "status": {"default": "ok", "errorWhen": "Checkpoint work throws"}}, "pi.harness.turn": {"description": "One assistant response and its tool batch", "parents": {"kind": "spans", "spans": ["pi.harness.run"]}, "startAttributes": {"pi.lane.name": {"type": "string", "required": true, "cardinality": "high", "description": "Lane name"}, "pi.operation.id": {"type": "string", "required": true, "cardinality": "high", "description": "Durable operation id"}, "pi.turn.id": {"type": "string", "required": true, "cardinality": "high", "description": "Invocation-local turn id"}}, "endAttributes": {}, "status": {"default": "ok", "errorWhen": "Turn work throws"}}, "pi.harness.step": {"description": "One durable retry attempt", "parents": {"kind": "spans", "spans": ["pi.harness.turn", "pi.harness.checkpoint", "pi.harness.compaction", "pi.harness.navigation"]}, "startAttributes": {"pi.lane.name": {"type": "string", "required": true, "cardinality": "high", "description": "Lane name"}, "pi.operation.id": {"type": "string", "required": true, "cardinality": "high", "description": "Durable operation id"}, "pi.step.kind": {"type": "string", "required": true, "values": ["assistant", "compaction", "branch_summary"], "description": "Retryable step kind"}, "pi.step.attempt": {"type": "number", "required": true, "description": "One-based durable attempt number"}, "pi.compaction.reason": {"type": "string", "required": false, "values": ["manual", "threshold", "overflow"], "description": "Compaction trigger"}}, "endAttributes": {"pi.step.outcome": {"type": "string", "values": ["succeeded", "retry", "failed", "aborted", "deferred", "overflow"], "description": "Attempt outcome"}}, "status": {"default": "ok", "errorWhen": "The attempt retries, fails, or throws"}}, "pi.harness.tool": {"description": "One raw phase-2 tool execution", "parents": {"kind": "spans", "spans": ["pi.harness.turn", "pi.harness.run"]}, "startAttributes": {"pi.lane.name": {"type": "string", "required": true, "cardinality": "high", "description": "Lane name"}, "pi.operation.id": {"type": "string", "required": true, "cardinality": "high", "description": "Durable operation id"}, "pi.turn.id": {"type": "string", "required": false, "cardinality": "high", "description": "Invocation-local live turn id"}, "pi.tool.name": {"type": "string", "required": true, "description": "Tool name"}, "pi.tool.call_id": {"type": "string", "required": true, "cardinality": "high", "description": "Tool call id"}, "pi.tool.replay": {"type": "string", "required": true, "values": ["never", "safe"], "description": "Declared replay policy"}, "pi.tool.recovery": {"type": "boolean", "required": true, "description": "Whether this is recovery execution"}}, "endAttributes": {"pi.tool.is_error": {"type": "boolean", "description": "Whether raw phase-2 execution returned an error"}}, "status": {"default": "ok", "errorWhen": "Raw phase-2 execution returns an error"}}, "pi.harness.hook": {"description": "One registered hook handler invocation", "parents": {"kind": "any"}, "startAttributes": {"pi.lane.name": {"type": "string", "required": true, "cardinality": "high", "description": "Lane name"}, "pi.operation.id": {"type": "string", "required": false, "cardinality": "high", "description": "Durable operation id when accepted"}, "pi.hook.name": {"type": "string", "required": true, "values": ["before_run", "before_resume", "before_run_end", "transform_context", "before_request", "before_payload", "after_response", "before_tool", "after_tool", "before_compaction", "before_navigation"], "description": "Hook name"}, "pi.hook.registration_id": {"type": "string", "required": false, "description": "Stable hook registration id"}}, "endAttributes": {"pi.hook.outcome": {"type": "string", "values": ["completed", "skipped", "blocked", "failed"], "description": "Handler outcome"}}, "status": {"default": "ok", "errorWhen": "The handler throws"}}, "pi.harness.sleep": {"description": "One retry delay", "parents": {"kind": "spans", "spans": ["pi.harness.step", "pi.harness.run"]}, "startAttributes": {"pi.operation.id": {"type": "string", "required": true, "cardinality": "high", "description": "Durable operation id"}, "pi.sleep.delay_ms": {"type": "number", "required": true, "description": "Requested delay in milliseconds"}}, "endAttributes": {"pi.sleep.outcome": {"type": "string", "values": ["elapsed", "aborted"], "description": "Delay outcome"}}, "status": {"default": "ok", "errorWhen": "Sleep work throws"}}, "pi.harness.event_handler": {"description": "One passive event listener invocation", "parents": {"kind": "any"}, "startAttributes": {"pi.event.type": {"type": "string", "required": true, "cardinality": "low", "values": ["run_start", "run_resume", "run_suspend", "run_abort", "run_end", "fault", "handler_error", "turn_start", "turn_end", "retry_scheduled", "retry_start", "retry_end", "message_start", "message_update", "message_end", "tool_start", "tool_update", "tool_end", "entry_added", "write_pending", "queue_update", "fact_update", "config_update", "compaction_start", "compaction_end", "navigation_start", "navigation_end", "lane_created", "usage"], "description": "Delivered harness event type"}, "pi.lane.name": {"type": "string", "required": false, "cardinality": "high", "description": "Lane name for lane-scoped events"}}, "endAttributes": {}, "status": {"default": "ok", "errorWhen": "The listener throws"}}, "pi.session.write": {"description": "One committed session mutation", "parents": {"kind": "any"}, "startAttributes": {"pi.lane.name": {"type": "string", "required": true, "cardinality": "high", "description": "Lane name"}, "pi.operation.id": {"type": "string", "required": false, "cardinality": "high", "description": "Durable operation id when accepted"}, "pi.session.mutation": {"type": "string", "required": true, "values": ["entry", "record", "lane", "fact"], "description": "Session mutation kind"}, "pi.session.item_type": {"type": "string", "required": false, "description": "Entry, record, lane, or fact subtype"}}, "endAttributes": {"pi.session.seq": {"type": "number", "description": "Committed session sequence when exposed"}}, "status": {"default": "ok", "errorWhen": "Storage rejects the mutation"}}}})
}

/// Start a span scoped to a callback for an AI-request span, mirroring
/// upstream `startAiSpan`.
pub fn start_ai_span<C, F, R>(
    context: &C,
    name: &str,
    attributes: BTreeMap<String, serde_json::Value>,
    callback: F,
) -> R
where
    C: pi_telemetry::TelemetryContext,
    F: FnOnce(&pi_telemetry::SpanHandle) -> R,
{
    context.start_span(pi_telemetry::SpanOptions { name: name.to_string(), attributes: Some(attributes) }, callback)
}

/// Start a span scoped to a callback for a harness span, mirroring upstream
/// `startHarnessSpan`.
pub fn start_harness_span<C, F, R>(
    context: &C,
    name: &str,
    attributes: BTreeMap<String, serde_json::Value>,
    callback: F,
) -> R
where
    C: pi_telemetry::TelemetryContext,
    F: FnOnce(&pi_telemetry::SpanHandle) -> R,
{
    context.start_span(pi_telemetry::SpanOptions { name: name.to_string(), attributes: Some(attributes) }, callback)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attr(name: &str, value: serde_json::Value) -> BTreeMap<String, serde_json::Value> {
        let mut m = BTreeMap::new();
        m.insert(name.to_string(), value);
        m
    }

    #[test]
    fn schemas_report_version_one() {
        assert_eq!(ai_telemetry_schema()["version"], 1);
        assert_eq!(harness_telemetry_schema()["version"], 1);
    }

    #[test]
    fn ai_schema_exposes_request_span_with_required_attributes() {
        let schema = ai_telemetry_schema();
        let request = &schema["spans"]["pi.ai.request"];
        assert_eq!(request["description"], "One logical request to an AI provider");
        assert_eq!(request["parents"]["kind"], "any");
        let start = &request["startAttributes"];
        assert_eq!(start["pi.ai.operation"]["required"], true);
        assert_eq!(
            start["pi.ai.operation"]["values"],
            serde_json::json!(["stream", "fetch_deferred", "cancel_deferred", "generate_images"])
        );
        assert_eq!(start["pi.ai.provider"]["required"], true);
        assert_eq!(start["pi.ai.model"]["required"], true);
        assert_eq!(start["pi.ai.streaming"]["type"], "boolean");
    }

    #[test]
    fn harness_schema_span_names_match_upstream() {
        let schema = harness_telemetry_schema();
        let expected = [
            "pi.harness.run",
            "pi.harness.compaction",
            "pi.harness.navigation",
            "pi.harness.checkpoint",
            "pi.harness.turn",
            "pi.harness.step",
            "pi.harness.tool",
            "pi.harness.hook",
            "pi.harness.sleep",
            "pi.harness.event_handler",
            "pi.session.write",
        ];
        let spans = schema["spans"].as_object().unwrap();
        assert_eq!(spans.len(), expected.len());
        for name in expected {
            assert!(spans.contains_key(name), "missing span {name}");
        }
    }

    #[test]
    fn harness_run_start_attributes_include_operation_kind() {
        let schema = harness_telemetry_schema();
        let start = &schema["spans"]["pi.harness.run"]["startAttributes"];
        for key in ["pi.session.id", "pi.lane.name", "pi.operation.id", "pi.operation.recovery", "pi.operation.kind"] {
            assert!(start.get(key).is_some(), "missing {key}");
        }
        assert_eq!(start["pi.operation.kind"]["required"], true);
        assert_eq!(start["pi.operation.kind"]["values"], serde_json::json!(["run"]));
        assert_eq!(start["pi.operation.recovery"]["type"], "boolean");
    }

    #[test]
    fn step_span_kind_values_match_upstream() {
        let schema = harness_telemetry_schema();
        let start = &schema["spans"]["pi.harness.step"]["startAttributes"];
        assert_eq!(
            start["pi.step.kind"]["values"],
            serde_json::json!(["assistant", "compaction", "branch_summary"])
        );
        assert_eq!(start["pi.step.attempt"]["type"], "number");
    }

    #[test]
    fn hook_span_uses_hook_names() {
        let schema = harness_telemetry_schema();
        let start = &schema["spans"]["pi.harness.hook"]["startAttributes"];
        assert_eq!(start["pi.hook.name"]["values"], serde_json::to_value(HOOK_NAMES).unwrap());
        let end = &schema["spans"]["pi.harness.hook"]["endAttributes"];
        assert_eq!(
            end["pi.hook.outcome"]["values"],
            serde_json::json!(["completed", "skipped", "blocked", "failed"])
        );
    }

    #[test]
    fn event_handler_span_uses_event_types() {
        let schema = harness_telemetry_schema();
        let start = &schema["spans"]["pi.harness.event_handler"]["startAttributes"];
        assert_eq!(start["pi.event.type"]["values"], serde_json::to_value(EVENT_TYPES).unwrap());
    }

    #[test]
    fn session_write_mutation_values() {
        let schema = harness_telemetry_schema();
        let start = &schema["spans"]["pi.session.write"]["startAttributes"];
        assert_eq!(start["pi.session.mutation"]["values"], serde_json::json!(["entry", "record", "lane", "fact"]));
    }

    #[test]
    fn tool_span_replay_and_recovery() {
        let schema = harness_telemetry_schema();
        let start = &schema["spans"]["pi.harness.tool"]["startAttributes"];
        assert_eq!(start["pi.tool.replay"]["values"], serde_json::json!(["never", "safe"]));
        assert_eq!(start["pi.tool.recovery"]["type"], "boolean");
        assert_eq!(start["pi.tool.call_id"]["cardinality"], "high");
    }

    #[test]
    fn start_harness_span_records_attributes_and_events() {
        use pi_telemetry::InMemoryTelemetryContext;
        let ctx = InMemoryTelemetryContext::default();
        start_harness_span(
            &ctx,
            "pi.harness.run",
            attr("pi.operation.kind", serde_json::json!("run")),
            |span| {
                pi_telemetry::TelemetrySpan::add_event(span, "run_start", Some(attr("pi.lane.name", serde_json::json!("default"))));
            },
        );
        let spans = ctx.get_spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, "pi.harness.run");
        assert_eq!(spans[0].attributes.get("pi.operation.kind"), Some(&serde_json::json!("run")));
        assert_eq!(spans[0].events.len(), 1);
        assert_eq!(spans[0].events[0].name, "run_start");
        assert!(spans[0].settled);
        assert_eq!(spans[0].status, pi_telemetry::SpanStatus::Ok);
    }

    #[test]
    fn agent_telemetry_schemas_contains_both() {
        let schemas = agent_telemetry_schemas();
        assert_eq!(schemas.len(), 2);
        assert!(schemas[0]["spans"]["pi.ai.request"].is_object());
        assert!(schemas[1]["spans"]["pi.harness.run"].is_object());
    }
}
