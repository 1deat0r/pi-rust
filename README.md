# pi-rust

An in-progress **1:1 Rust port of the [pi coding agent](https://github.com/earendil-works/pi)** (v0.84.2, pinned upstream commit `5cd93f6`). The project targets the same CLI surface, session formats, provider behavior, tools, and wire contracts in idiomatic Rust.

## Current status

**Conversion progress: 62.65% — 104 of 166 ledger tasks complete; 62 open.**

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

The current implementation slice completes OpenRouter image retry,
HTTP-date-delay, and cancellation parity on top of the deferred-response
runtime checkpoint `56ea6f3`. Implementation commit `2b92195` and its push to
`origin/main` are verified. The follow-on telemetry strict-verification fix is
implemented in `45e6d64`, with metadata sync in `788f9c5`; both are pushed and
hash-verified through the authenticated HTTPS remote.

The shared `AgentHarness` now exposes durable main and secondary lane views:
lanes branch from session leaves, seed independent provider context, persist
their own prompt turns, and emit lane-attributed lifecycle telemetry. Full
JSONL/RPC harness ownership and mode-specific golden persistence remain open
under S-021/S-022.

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

### Provider authentication guidance

Provider auth failures in print, JSON, interactive, and RPC modes preserve
the upstream actionable guidance: API-key failures name the provider and point
to `/login` plus the bundled provider/model docs; OAuth-capable failures point
to `/login <provider>`. Network and non-auth errors retain their original
diagnostics.

### Harness lanes

The harness session tree supports `main` plus named secondary lanes. A lane
created at a session leaf inherits that branch as provider context, then
persists new user/assistant messages and advances only its own leaf pointer.
Run lifecycle events and `pi.harness.run` spans include the lane name.

The ConfigSelector now matches global/project package sources across their
settings bases, writes project-relative local overrides, preserves inherited
package filters, recognizes metadata-base resource patterns, and cleans empty
project overrides when returning to inherit. Search, navigation, scope
switching, synchronous writes, close behavior, and the real-terminal PTY
exercise are covered by the selector and ConfigSelector tests.

Interactive `/compact` now uses the same compaction path as automatic context
management, accepts optional summary instructions, persists the compaction
entry, replaces the live context, and resets cache accounting. The remaining
interactive slash-command terminal matrix remains tracked under S-056; S-033
command behavior itself is complete.

S-033 is now complete at the command-behavior audit level. Its real tmux PTY
fixture covers `/resume` picker selection and transcript rehydration alongside
the existing `/help`,
`/export`, `/import`, `/share`, `/trust`, `/login`, `/logout`, `/name`,
`/copy`, `/new`, `/fork`, `/clone`, `/tree`, and `/reload`, including
alternate-screen and cursor cleanup assertions. It also fixes the first-hit
terminal capability probe so an uncached interactive startup cannot deadlock;
the broader S-056 command matrix remains open.

Project trust is resolved before any project settings or resources load across
print, JSON, RPC, interactive, config, and package commands. Saved decisions,
global `defaultProjectTrust`, `--approve/-a`, and `--no-approve/-na` follow the
upstream precedence; interactive `ask` prompts before raw mode and persists the
answer, while headless unresolved prompts remain untrusted.

### Deferred responses

The shared `ModelRuntime` now preserves deferred fetch/cancel dispatch through
print, interactive, JSON, and RPC mode wiring. Provider-composer overlays keep
the selected provider capabilities and shared models store, while the lazy API
surface exposes only declared capabilities and preserves the upstream missing-
capability diagnostics. Faux runtime tests cover submit, poll-to-resolution,
cancellation, and mode registration.

### Image generation

OpenRouter image generation now follows the upstream retry contract: status and
`x-should-retry` classification, numeric/HTTP-date `Retry-After`, server-delay
caps, zero-based exponential backoff, and abort-aware request/body/backoff
handling. Quota/billing errors remain terminal in the shared assistant retry
classifier, and image failures stay encoded as `AssistantImages` results.

### Strict verification

The telemetry async span path now releases its in-memory mutex before invoking
an async callback, preserving settled-parent behavior without holding a guard
across `.await`. The focused telemetry tests and strict all-target clippy gate
pass. The broader `pi-ai` all-target clippy cleanup remains active; the adapter
cleanup reduced its current run to 23 structural/test diagnostics, so no
repository-wide zero-warning claim is made.

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
