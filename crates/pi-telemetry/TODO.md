# pi-telemetry — current conversion notes

Telemetry contracts, the noop and in-memory adapters, settle-once/panic-safe
callback handling, schema definitions, and conformance fixtures are covered
by checked rows #48, S-023, and S-064.

Evidence:

- cargo test -p pi-telemetry --offline --quiet
- cargo clippy -p pi-telemetry --offline --all-targets -- -D warnings
- the telemetry schema/release checks recorded in S-064

The object-safe Rust TelemetrySpan trait and generic context method are the
intentional Rust representation of the TypeScript interfaces. The old note
that panic settlement was “not yet wired” is superseded by S-023 evidence.
