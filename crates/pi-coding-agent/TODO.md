# pi-coding-agent — current conversion notes

The CLI, settings/configuration, trust, auth commands, model/catalog
composition, provider runtime, print/JSON/RPC/interactive modes, sessions,
compaction, package resources, slash commands, telemetry, export/share, TUI
integration, and release fixtures are owned by the checked rows in
CONVERSION-LEDGER.md and the exact evidence recorded there.

## Current validation

- cargo test -p pi-coding-agent --offline --lib --quiet
- cargo test -p pi-coding-agent --offline --tests --quiet
- cargo test -p pi-coding-agent --offline --test extensions_parity --quiet
- node scripts/parity-suite.mjs

## Explicit residual

S-027 now has a native-provider bridge protocol and fixture for `streamSimple`
callback input plus start/text/done events. The Rust boundary adapter also
converts provider definitions and native events into typed pi-ai provider
streams/models, with the typed `Models::stream_simple` path covered by the
external fixture. The bridge now embeds byte-identical `jiti@2.7.0`/Babel
assets, covers TS/TSX, configured aliases and virtual modules under Node and
Bun 1.4.0, normalizes extension paths like upstream, and re-evaluates the
interactive extension set on `/reload`. Print, JSON, RPC, and interactive
startup register these providers before model lookup; the binary print fixture
covers the production selection path. It remains open for the actual built-in
pi/TypeBox JS module graph, compiled-Bun/Node-SEA virtual-module branches, and
full reload lifecycle/resource/flag evidence. The current context/action
object and ordered mid-execution signal/update path are covered by the focused
integration and external-bridge fixtures; the remaining provider/runtime
leaves are intentionally not repeated as done claims here.
The final clean-room gate #100 is recorded as passed in the ledger. The
remaining process gate is S-066.
