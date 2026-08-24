# pi-rust

An in-progress **1:1 Rust port of the [pi coding agent](https://github.com/earendil-works/pi)** (v0.84.2, pinned upstream commit `5cd93f6`). The project targets the same CLI surface, session formats, provider behavior, tools, and wire contracts in idiomatic Rust.

## Current status

**Conversion progress: 55.42% — 92 of 166 ledger tasks complete; 74 open.**

The denominator includes the full conversion ledger: source audits, provider
edge cases, TUI, RPC, auxiliary client/server, evaluation, documentation, and
final verification work. The original 100-item list is only the historical core
queue. Recalculate the live value with:

```bash
node scripts/conversion-progress.mjs
```

The port already includes substantial CLI and runtime work, including the
in-process agent loop, stateful harness-backed print and JSON modes,
provider/model catalog surfaces, session storage, project trust, tools,
compaction, RPC controls, TUI components, and client/server support. Remaining
work is tracked explicitly rather than treated as complete just because a
similarly named module exists.

## Workspace

```text
crates/
  pi-protocol/          CBOR codec, framing, and message schemas
  pi-telemetry/         vendor-neutral telemetry contracts and adapters
  pi-ai/                providers, model catalogs, transports, and images
  pi-agent/             agent runtime, harness, tools, and session JSONL
  pi-client/            auxiliary client session handles and transport
  pi-server/            auxiliary in-process server and live-session manager
  pi-session-backends/  SQLite session backend
  pi-tui/               editor, markdown, select lists, terminal features
  pi-coding-agent/      the `pi` binary, CLI, config, RPC, and run loop
  pi-evals/             evaluation harness
```

The shipped `pi` binary runs the agent loop in-process, matching the upstream
CLI architecture. `pi-server` and `pi-client` are auxiliary surfaces and are
not linked into the shipped CLI binary.

## Project documents

- [`PLAN.md`](PLAN.md) — fidelity model, phase roadmap, parity evidence, and
  next actions.
- [`CONVERSION-LEDGER.md`](CONVERSION-LEDGER.md) — exhaustive task ledger with
  `unit`, `mock`, or `live` evidence.
- [`HANDOFF.md`](HANDOFF.md) — current checkpoint, tests, blockers, progress,
  and resume instructions.
- [`AGENTS.md`](AGENTS.md) — mandatory Codex turn, documentation, and
  local/remote commit-push protocol.

Every Codex task must leave these documents synchronized, commit one focused
checkpoint, push it immediately, and verify the local and remote hashes match.
If remote authentication or network access blocks the push, the blocker is
recorded rather than hidden.

The repository pre-commit hook enforces that implementation commits stage the
README, plan, ledger, and handoff together, validates the conversion progress
checker, and attempts to sync the GitHub repository description when `gh` is
authenticated. Enable it for a clone with:

```bash
git config core.hooksPath .githooks
```

## Build and test

Use the offline commands when working from a restricted environment:

```bash
cargo check --workspace --offline
cargo test --workspace --offline
git diff --check
```

For a release build:

```bash
cargo build --release -p pi-coding-agent
./target/release/pi --help
```

The pinned upstream source and its tests are the parity oracle; behavior is not
marked complete without evidence from the relevant test or live command.

## License

MIT — see [LICENSE](LICENSE). The port targets the MIT-licensed
[earendil-works/pi](https://github.com/earendil-works/pi).
