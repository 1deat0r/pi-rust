# pi-agent — current conversion notes

The detailed historical port log is retained in git history. Current
completion is owned by CONVERSION-LEDGER.md; this file contains only
evidence-backed notes and does not independently mark parity.

## Evidence-backed surface

The agent loop, tool contract/validation, session JSONL v4 storage, compaction,
branch summaries, harness events/telemetry, skills, prompt templates, search,
filesystem, and built-in tools are implemented in this crate. The focused
checks are:

- cargo test -p pi-agent --offline --quiet
- cargo test -p pi-agent --offline --test conformance --quiet
- cargo test -p pi-agent --offline rich_agent::tests -- --nocapture

The corresponding behavioral ownership is recorded in ledger rows #23–37,
#83–87, and S-018–S-026. A file existing is not treated as evidence without
one of those rows and its exact command.

## Remaining audit ownership

No stale “not yet ported” claim is kept here. Any residual found by the final
source/export audit is assigned to S-001 or to the narrow behavioral row named
by that audit; S-027 remains the only open extension-runtime implementation
row in the current checker.
