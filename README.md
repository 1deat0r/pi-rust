# pi-rust

A working 1:1 port of the [pi agent harness](https://github.com/earendil-works/pi)
(v0.84.2, commit `5cd93f6`) to idiomatic Rust. Same CLI surface, same data
formats on disk and on the wire, same behavior — different implementation
language.

## Status

| Phase | Scope | State |
|---|---|---|
| P0 | Research & mapping | Done |
| P1 | pi-protocol (CBOR/framing/codec/schemas), pi-telemetry | Done — 49 tests green |
| P2 | pi-ai core (types, SSE, event-stream, partial-json, faux provider) | Done — 26 tests green, review-gated |
| P3+ | pi-agent data layer, client/server, TUI, coding-agent | Planned (see PLAN.md) |

`PLAN.md` is the living roadmap and session ledger. Every phase is reassessed
after completion, reviewed line-by-line, and gated by an independent reviewer
sign-off before continuation.

## Workspace

```
crates/
  pi-protocol/      CBOR subset codec, framing, codec, schemas
  pi-telemetry/     span/event/counter contracts, memory + noop adapters
  pi-ai/            providers, model catalog, transports, event streams
  pi-agent/ …       stubs — planned
```

## Upstream reference

The pinned upstream clone is excluded from this repo (gitignored). To compare
against it:

```bash
git clone https://github.com/earendil-works/pi upstream_pi
git -C upstream_pi checkout 5cd93f6
```

## Parity oracle

`scripts/oracle_partial_json.mjs` reproduces the upstream streaming-JSON
contract against the vendored npm `partial-json@0.1.7` (in
`scripts/partial-json-0.1.7/`), network-free; pi-ai tests assert the same
golden table.

## Build & test

```bash
cargo test --workspace   # 75 tests, 0 warnings (P2 baseline)
cargo check --workspace
```

Requires Rust 1.85+ (`rust-version`).

## License

MIT — see [LICENSE](LICENSE). The port targets the MIT-licensed
[earendil-works/pi](https://github.com/earendil-works/pi).
