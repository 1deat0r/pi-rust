# pi-rust

An in-progress **1:1 Rust port of the [pi coding agent](https://github.com/earendil-works/pi)** (v0.84.2, pinned upstream commit `5cd93f6`). The project targets the same CLI surface, session formats, provider behavior, tools, and wire contracts in idiomatic Rust.

## Current status

**Conversion progress: 85.54% — 142 of 166 ledger tasks complete; 24 open.**

The denominator includes the full conversion ledger: source audits, provider
edge cases, TUI, RPC, auxiliary client/server, evaluation, documentation, and
final verification work. The original 100-item list is only the historical core
queue. Recalculate the live value with:

```bash
node scripts/conversion-progress.mjs
```

The 2026-08-25 completion run is coordinated through the scoped execution
tree in `.unlazy/full-conversion-20260825/`. The source inventory is complete;
focused OAuth, Anthropic, catalog, proxy, protocol, server, TUI, eval, provider
matrix, client, harness, reconnect, strict-clippy, and live PTY checks are
green, while expanded server, parity, extension-runtime, and final-audit
leaves remain explicitly gated. See [PLAN.md](PLAN.md)
and [HANDOFF.md](HANDOFF.md) for the current checkpoint and next
dependency-safe action.

The port already includes substantial CLI and runtime work, including the
in-process agent loop, stateful harness-backed print, JSON, and interactive
turns, provider/model catalog surfaces, session storage, project trust, tools,
compaction, RPC controls, TUI components, and client/server support. Remaining
work is tracked explicitly rather than treated as complete just because a
similarly named module exists.

The current implementation slice completes constrained JSON-schema and
OpenAI grammar custom-tool parity across the advertised pi-ai adaptors. Strict
schemas are cloned and rewritten without mutating caller input; unsupported
required schemas return the upstream diagnostics, and grammar tool input is
assembled monotonically through streaming Responses/Completions events. The
S-008 implementation commit `7a72f2fe104cf660f946f29a822c88da556a37d1`
is pushed to `origin/main` and hash-verified; the image retry/cancellation and
telemetry checkpoints below remain part of the pushed baseline.

The S-011 Google Vertex checkpoint now covers file-based Application Default
Credentials for service-account JWT and authorized-user refresh flows,
configured token URIs and scopes, explicit credential-path precedence, and
API-key requests that do not require project or location. Deterministic local
fixtures cover the token exchanges and provider-auth precedence; metadata
server, workload identity, and external-account discovery remain open.

The S-012 Cloudflare checkpoint now covers AI Gateway binding-prefix
validation with WHATWG-compatible dot/empty-segment handling, JSON POST/query
translation, header precedence and filtering, runtime-neutral cancellation
forwarding, stored-field credential precedence, scoped account/gateway
environment, inline upstream authorization, and gateway base-URL resolution.
Deterministic local fixtures provide mock evidence; no live Cloudflare account
or Workers runtime was used, and the host binding response remains
runtime-owned.

The shared `AgentHarness` now exposes durable main and secondary lane views:
lanes branch from session leaves, seed independent provider context, persist
their own prompt turns, and emit lane-attributed lifecycle telemetry. Full
JSONL/RPC harness ownership and mode-specific golden persistence remain open
under S-021/S-022.

Bedrock credential/profile and region-resolution parity now covers explicit and
scoped profile precedence, shared credentials and selected-profile config
regions, ARN/env/option endpoint-region precedence, ECS task-role retrieval,
web-identity STS XML credentials, bearer/skip-auth modes, and exact provider
auth source labels. Deterministic local fixtures cover the profile, region,
ECS, STS, and provider-auth paths. SSO- or process-backed profiles and EC2
metadata remain outside the hand-rolled signer scope.
The S-010 checkpoint is pushed as
`9a8eaee9b8273e7b938075a38ed9659baff02359`.
The public-boundary acceptance checkpoint is pushed as
`feadf6415f663662ff0948b2e29507655fc359bd`.
The whole-result acceptance rerun passed the exported Bedrock `stream` and
`stream_simple` runtime boundaries, provider-auth tests, full pi-ai
compile/lint/test/metadata gates, formatting, and diff checks. `cargo package -p pi-ai --offline
--allow-dirty --no-verify` remains a repository-level P9 blocker because the
internal `pi-telemetry` dependency is not available in the offline crates.io
index.

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

### Constrained sampling and strict verification

The shared pi-ai constrained-sampling resolver now handles strict JSON-schema
rewrites, exact unsupported-schema diagnostics, grammar precedence and input
property inference, and monotonic custom-tool JSON deltas. OpenAI
Completions/Responses, Azure, and Codex support grammar custom tools; Anthropic,
Bedrock, Google/Vertex, and Mistral support strict-schema conversion. The
Responses replay path preserves custom item IDs/namespaces and omits absent IDs
rather than serializing `null`.

The `pi-telemetry` and `pi-ai` strict all-target clippy gates pass. Full `pi-ai`
tests pass (307 library, 4 + 9 + 2 integration tests), and the workspace check
passes offline. S-008 is marked complete in the ledger; implementation commit
`7a72f2fe104cf660f946f29a822c88da556a37d1` is pushed and hash-verified.

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
