# pi-rust

An in-progress **1:1 Rust port of the [pi coding agent](https://github.com/earendil-works/pi)** (v0.84.2, pinned upstream commit `5cd93f6`). The project targets the same CLI surface, session formats, provider behavior, tools, and wire contracts in idiomatic Rust.

## Current status

**Conversion progress: 58.43% — 97 of 166 ledger tasks complete; 69 open.**

The denominator includes the full conversion ledger: source audits, provider
edge cases, TUI, RPC, auxiliary client/server, evaluation, documentation, and
final verification work. The original 100-item list is only the historical core
queue. Recalculate the live value with:

```bash
node scripts/conversion-progress.mjs
```

The port already includes substantial CLI and runtime work, including the
in-process agent loop, stateful harness-backed print, JSON, and interactive
turns, provider/model catalog surfaces, session storage, project trust, tools,
compaction, RPC controls, TUI components, and client/server support. Remaining
work is tracked explicitly rather than treated as complete just because a
similarly named module exists.

The last verified local and GitHub `main` checkpoint is `eaa36ba`. GitHub CLI
authentication is configured for the HTTPS remote, so implementation
checkpoints are pushed and hash-verified immediately.

Legacy v1/v2/v3 session files are atomically migrated before session inventory,
CLI continue/resume/session/fork selection, interactive startup and `/import`,
and direct RPC switches. Selected sessions restore their branch context and
append in place; forks preserve parent metadata. The complete routing audit is
tracked as S-026 in the conversion ledger.

### Session routing

`--continue` and `--resume` reopen the newest session for the current working
directory, `--session` accepts a session path or unambiguous id prefix, and
`--fork` creates a durable child from a path or id. These selectors run before
the print, interactive, and RPC harnesses are created, so resumed context and
new messages share one JSONL file. Legacy v1/v2/v3 files are converted
atomically at the inventory or explicit-path boundary.

### Updating compiled Rust installs

`pi update --self` performs the same latest-release and `--force` decision as
upstream. A running compiled Rust executable cannot safely replace itself, so
when an update is available it exits non-zero and prints the exact replacement
instruction for the detected release, for example:

```text
Update @earendil-works/pi-coding-agent@<version> using the package manager, wrapper, or source checkout that provides this installation.
```

For a source checkout, rebuild with `cargo build --release -p pi-coding-agent`
and replace the installed `pi` binary using the mechanism that installed it.
This is the supported replacement behavior for the Rust distribution; the
command does not claim that the running executable was updated.

### Prompt-cache notices

Interactive mode keeps cache accounting live even though it defers JSONL
persistence until exit. Enable `Cache miss notices` in `/settings` to inject
significant cache-miss notices into the transcript; `/session` reports
cumulative `Cache Re-billed` tokens, cost, and miss count. Auto-compaction,
`/clear`, new sessions, resume, and import boundaries reset or reload the
cache segment so notices stay attached to the correct prompt history.

### Install telemetry

Interactive startup sends the anonymous version/update ping only on a fresh or
version-changed install boundary. It is best-effort and backgrounded, with a
bounded retry/timeout policy. Set `PI_TELEMETRY=0` (or disable `Install
telemetry` in `/settings`) to opt out; `PI_OFFLINE=1` disables the transport
before any network request.

### Startup timing

The upstream `PI_TIMING=1` startup namespaces are intentionally not exposed by
the Rust distribution. When that exact value is requested, `pi` prints a
warning and points to `/usr/bin/time -p pi ...` for supported process-level
startup timing; other values remain silent, matching upstream's exact-one
enable gate.

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
