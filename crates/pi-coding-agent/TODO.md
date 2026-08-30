# pi-coding-agent — current conversion notes

## Active behavioral audit — 2026-08-26

The old conversion rows are source-ledger history. Functional completion is
tracked by the 310-ID acceptance inventory at
`../../docs/EXHAUSTIVE-PARITY-INVENTORY.md`; the exhaustive implementation and
real-test campaign remains open until its root gates pass. In particular,
interactive `/login` and live Codex use must remain real-provider evidence,
while faux/loopback fixtures are only local regression evidence.

The CLI, settings/configuration, trust, auth commands, model/catalog
composition, provider runtime, print/JSON/RPC/interactive modes, sessions,
compaction, package resources, slash commands, telemetry, export/share, TUI
integration, and release fixtures are owned by the checked rows in
CONVERSION-LEDGER.md and the exact evidence recorded there.

## Current validation

- cargo test -p pi-coding-agent --offline --lib --quiet
- cargo test -p pi-coding-agent --offline --tests --quiet
- cargo test -p pi-coding-agent --offline --test extensions_parity --quiet
- cargo run -p pi-coding-agent --offline --bin conversion_audit -- all

## Explicit residual

S-027 is complete under the explicit 100%-Rust distribution scope. Rust
factory extensions cover commands, hooks, renderers, tools, flags, and
providers. The former Node/Bun bridge, embedded JS runtime assets, and JS/TS
fixtures are removed; filesystem JS/TS paths and npm/Bun package execution are
rejected or ignored deterministically. Static HTML export is rendered by Rust.
S-066 freezes the denominator at 166 tasks with zero open or unclassified
records. Arbitrary external JS/TS extension execution is intentionally outside
the Rust-only distribution boundary.
