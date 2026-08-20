# pi-telemetry — port status

Port of `packages/telemetry` v0.84.2. Complete for current consumers:
contract types (AttributeValue/SpanAttributes/SpanOptions/SpanStatus),
NOOP context, InMemoryTelemetryContext with settle-once semantics and
detached snapshots.

## Notes / deltas
- Trait design: `TelemetrySpan` is the object-safe recording trait;
  `TelemetryContext::start_span` is generic (not object-safe) — idiomatic
  Rust replacement for the TS interface. Call sites in pi-agent and
  pi-coding-agent use `TelemetryContext` as a generic bound.
- `start_span` returns the callback value directly (upstream wraps in
  Promise). Automatic error status on callback panic parallels upstream's
  `catch` path via `settle_span(failed=true)`; panics in the callback abort
  but still settle the span (not yet wired — add `catch_unwind` if needed).
- Schema definition helpers (`defineTelemetrySchema`, schemas for agent
  spans) live with their consumers in pi-agent; re-export here if they
  become crate-shared.

## Future work
- Port `test/conformance.test.ts` if/when schema conformance checking is
  needed by pi-evals.
