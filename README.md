# pi — the pi agent harness, rewritten in Rust

A working 1:1 port of the [pi agent harness](https://github.com/earendil-works/pi)
(v0.84.2, commit `5cd93f6`) to idiomatic Rust. Same CLI surface, same on-disk
formats, same wire behavior, same observable semantics — different
implementation language.

```
$ cargo build --release -p pi-coding-agent
$ ./target/release/pi --help
```

## Status

The CLI-observable product surface — `pi` interactive mode, `pi run`,
RPC/json/jsonl modes, `pi config`, `pi auth`, `pi list-models`, `pi /share`,
`pi --export`, the full flag and env surface, project trust, skills /
prompt-templates / context-files loaders, tools (bash/read/write/edit/
edit-diff/ls/find/grep/image), compaction, and session storage — is ported and
working. The workspace test suite is **1418 tests, 0 failures**, with the
new-code surface held to a 0-warning / clippy-clean bar.

The remaining work and its progress are tracked in the exhaustive
[`CONVERSION-LEDGER.md`](CONVERSION-LEDGER.md). The percentage in `PLAN.md` is
derived from every checked/unchecked ledger task, including source-audit,
provider edge cases, TUI, RPC, auxiliary client/server, evaluation, and final
verification work; the original 100-item queue is only the historical core of
that ledger.

## Workspace

```
crates/
  pi-protocol/          strict CBOR codec, framing, client/server message schemas
  pi-telemetry/         vendor-neutral telemetry contracts + reference adapters
  pi-ai/                unified multi-provider LLM API, model catalog, transports
  pi-agent/             agent runtime + harness (tools, compaction, session JSONL)
  pi-client/            client session handle + transport (auxiliary)
  pi-server/            in-process server + live-session manager (auxiliary)
  pi-session-backends/  SQLite session backend
  pi-tui/               TUI primitives: editor, markdown, select lists, word nav
  pi-coding-agent/      the `pi` binary — interactive CLI, config, RPC, run loop
  pi-evals/             eval harness
```

The `pi` binary is built from `pi-coding-agent`; it runs the agent loop
in-process (as the real CLI does). `pi-server`/`pi-client` are an auxiliary
subsystem that no shipped binary links — they harden the client/server surface
without advancing CLI parity (see `PLAN.md §Recut`).

## Roadmap & process

- [`PLAN.md`](PLAN.md) — the living roadmap, fidelity model, module maps, phased
  roadmap, and a **session ledger**. Every phase is reassessed on completion,
  reviewed line-by-line against upstream, and gated by an independent reviewer
  sign-off before continuation.
- [`CONVERSION-LEDGER.md`](CONVERSION-LEDGER.md) — the exhaustive task tracker
  for the full 1:1 conversion, with evidence tiers (`unit` | `mock` | `live`)
  on every criterion.
- Per-crate `TODO.md` files shadow the upstream module maps.

## Parity oracle

`scripts/oracle_partial_json.mjs` reproduces the upstream streaming-JSON
contract against the vendored npm `partial-json@0.1.7` (in
`scripts/partial-json-0.1.7/`), network-free; pi-ai tests assert the same golden
table. The pinned upstream clone is **excluded from this repo** (gitignored).

## Build & test

```bash
cargo test --workspace       # 1418 tests, 0 failures
cargo check --workspace
```

Requires Rust 1.85+ (`rust-version`). The dev profile uses `opt-level = 1`.

## License

MIT — see [LICENSE](LICENSE). The port targets the MIT-licensed
[earendil-works/pi](https://github.com/earendil-works/pi).
