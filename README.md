# pi-rust

An in-progress **1:1 Rust port of the [pi coding agent](https://github.com/earendil-works/pi)** (v0.85.1, pinned upstream commit `d7296c0`). The project targets the same CLI surface, session formats, provider behavior, tools, and wire contracts in idiomatic Rust.

## Current status

**Behavioral parity status: exhaustive audit in progress — not yet 1:1.**
The acceptance index contains 318 unique capability IDs across CLI, auth,
provider, transport, agent, session, TUI, RPC, extension, release, and
adversarial cases. Whole-product behavioral parity is **18.24% (58/318)**;
per-dimension figures are in the checkpoint below. Row-complete JSON, TUI
visual, live-provider, platform, and recovery boundaries remain open.

Audit scope and residuals live in `docs/EXHAUSTIVE-PARITY-INVENTORY.md`.
Session history lives in `PLAN.md`, `HANDOFF.md`, and
`CONVERSION-LEDGER.md`; the 2026-08 checkpoint narrative that used to fill
this section is archived in `docs/PARITY-DASHBOARD.md`.

<!-- README_STATUS:START (machine-checked; keep in sync with the audits) -->

Library suites (2026-09-22, offline): pi-ai 487, pi-tui 409, pi-agent 276,
pi-coding-agent 918 — every suite green. `pi-agent`, `pi-tui`, and
`pi-coding-agent` are also all-target clippy-clean (zero warnings since the
2026-09-22 hygiene pass retired the former dead-code, module-inception,
header-end, unused-import, and type-complexity findings; the earlier
"29 pre-existing failures" note is obsolete). The embedded catalog holds
1,357 models across 41 providers (runtime provider overlays can report
more). Row-complete JSON, TUI
visual, live-provider, platform, and recovery boundaries remain open;
this is strong package evidence, not a 100% parity claim.

The TUI acceptance tracker measures a separate contract across all 52 TUI
capabilities. Its current generated checkpoint is:

TUI functional implementation: 25.00% (13/52)
TUI test/evidence parity: 25.00% (13/52)
TUI visual/interaction parity: 0.00% (0/52)
TUI overall parity: 0.00% (0/52)

These figures are intentionally conservative: automated tests do not count as
visual parity until the same terminal presentation and interaction are
manually compared with official Pi. The full row register and evidence
boundaries are in [`docs/TUI-PARITY-STATUS.md`](docs/TUI-PARITY-STATUS.md).

The synchronized progress dashboard also tracks coverage separately from
completion:

Source/conversion ledger: 100.00% (166/166; 0 open)
Acceptance inventory census: 100.00% (318/318) (318 IDs indexed)
Acceptance scoring coverage: 100.00% (318/318) (318 of 318 IDs scored)
Root acceptance gates: 100.00% (8/8) (8 passed; 0 open)
Rust-only distribution boundary: 100.00% (0 JS/TS executable source files; generated Rustdoc excluded)
TUI functional implementation: 25.00% (13/52)
TUI test/evidence parity: 25.00% (13/52)
TUI visual/interaction parity: 0.00% (0/52)
TUI overall parity: 0.00% (0/52)
Non-TUI implementation parity: 41.73% (111/266 PASS; 155 PARTIAL; 0 OPEN)
Non-TUI deterministic evidence parity: 40.23% (107/266 PASS; 159 PARTIAL; 0 OPEN)
Non-TUI runtime-boundary parity: 22.18% (59/266 PASS; 156 PARTIAL; 51 OPEN)
Non-TUI overall parity: 21.80% (58/266)
Whole-product behavioral parity: 18.24% (58/318)

See [`docs/PARITY-DASHBOARD.md`](docs/PARITY-DASHBOARD.md) for the
definitions and the machine-validated current checkpoint.
<!-- README_STATUS:END -->

## Launch and test it yourself

From this checkout, `pi-rust` resolves to the optimized Rust binary while the
official `pi` installation remains independently available:

```bash
cd "/run/media/mustbearnold/Projects/AI Agents/pi-rust"
pi-rust --version
pi-rust --help
pi --version       # official Pi, if installed
```

If `pi-rust` is not yet installed on your PATH, build and install only the
Rust-named command; this leaves the official `pi` command untouched:

```bash
/home/mustbearnold/.cargo/bin/cargo build --release --offline
install -Dm755 target/release/pi ~/.local/bin/pi-rust
hash -r
pi-rust --version
```

Start the interactive terminal UI with a real Codex provider and no tools:

```bash
pi-rust --provider openai-codex --model gpt-5.5 --no-tools
```

pi-rust does not perform an upstream Pi release check at startup, so
`PI_SKIP_VERSION_CHECK` is not needed for normal Rust launches.

If the credential is missing or expired, enter `/login openai-codex`, choose
`Browser login (default)`, complete the browser flow, and return to the TUI.
For a headless flow choose `Device code login (headless)`. `/logout` removes
the selected stored credential; it does not remove environment-variable or
models-file configuration.

### Qwen Token Plan (international)

The international provider is built in as `qwen-token-plan`. It uses the
official OpenAI-compatible endpoint and reads `QWEN_TOKEN_PLAN_API_KEY`; no
key is bundled or written to the repository:

```bash
export QWEN_TOKEN_PLAN_API_KEY='your-key-from-qwen-token-plan'
pi-rust --provider qwen-token-plan --model qwen3.8-max --no-tools
```

The catalog includes both requested QwenCloud models plus the international
Qwen, DeepSeek, GLM, Kimi, and MiniMax models. Use
`pi-rust --offline --list-models qwen-token-plan` to inspect the embedded
catalog. The endpoint is
`https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1`.
A harmless authenticated release request using `qwen3.8-max` returned
`QWEN_LIVE_OK`; the key was supplied through the environment and was not
printed or persisted by the test.

For a low-risk local interactive smoke test, keep `--no-tools` and optionally
add `--no-session`. Type a prompt, submit a second prompt to verify a
multi-turn conversation, try `/help`, then use `/quit` and confirm the shell
prompt returns normally.

To exercise the real Pi-style tool display, launch with tools enabled and ask
Codex to use one safe built-in tool:

```text
Use the read tool exactly once to read crates/pi-coding-agent/Cargo.toml.
Then reply with exactly LIVE_TOOL_RENDER_OK and nothing else.
```

During the turn the TUI should show a compact running `read` block with the
path, followed by a settled `read` result and its preview. Normal TUI output
must not show a fenced JSON argument object. Tool output is collapsed in the
live view; the completed session remains available through the normal
transcript/detail views.

To test real provider output without the TUI, use the installed command and a
temporary session directory:

```bash
test_dir="$(mktemp -d /tmp/pi-rust-live.XXXXXX)"
PI_SKIP_VERSION_CHECK=1 PI_CODING_AGENT_SESSION_DIR="$test_dir/sessions" \
  pi-rust --print --provider openai-codex --model gpt-5.5 --no-tools \
  --session-id live-codex "Reply with exactly LIVE_PI_RUST_TURN_1"
PI_SKIP_VERSION_CHECK=1 PI_CODING_AGENT_SESSION_DIR="$test_dir/sessions" \
  pi-rust --print --provider openai-codex --model gpt-5.5 --no-tools \
  --continue "Reply with exactly LIVE_PI_RUST_TURN_2 and nothing else"
```

Check auth without printing a secret:

```bash
pi-rust auth check --provider openai-codex --json
```

Run the exhaustive local verification from the checkout with the direct Cargo
binary used by this machine:

```bash
/home/mustbearnold/.cargo/bin/cargo test --workspace --offline -- --test-threads=1
/home/mustbearnold/.cargo/bin/cargo test --workspace --release --offline -- --test-threads=1
/home/mustbearnold/.cargo/bin/cargo clippy --workspace --offline --all-targets -- -D warnings
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --release --offline \
  --test interactive_auth_pty -- --test-threads=1 --nocapture
```

Never paste `auth.json`, bearer tokens, API keys, or authorization codes into
logs or chat.

The denominator includes the full conversion ledger: source audits, provider
edge cases, TUI, RPC, auxiliary client/server, evaluation, documentation, and
final verification work. The original 100-item list is only the historical core
queue. Recalculate the live value and run the source audit with:

```bash
/home/mustbearnold/.cargo/bin/cargo run -p pi-coding-agent --offline --bin conversion_audit -- all
```

The 2026-08-25 completion run is coordinated through the scoped execution tree
in `.unlazy/full-conversion-20260825/`. The Rust `conversion_audit` binary
passes the exact 166-ID ledger check, source/TODO audit, and hard zero-JS/TS
census. Formatting, diff checks, workspace compilation, focused extension and
package tests, and the full 507-test coding-agent library target are green.
See [PLAN.md](PLAN.md), [CONVERSION-LEDGER.md](CONVERSION-LEDGER.md), and
[HANDOFF.md](HANDOFF.md) for the current checkpoint.

The distribution is intentionally 100% Rust: it ships no JavaScript or
TypeScript source, Node/Bun runtime, npm dependency execution, or source-file
extension loader. Compiled Rust factories provide extension commands, hooks,
renderers, tools, flags, and providers. Filesystem JS/TS extension paths are
rejected or ignored deterministically, while skills, prompts, themes, and Git
resource packages remain supported. HTML export is a static document rendered
by Rust without browser JavaScript.

The port already includes substantial CLI and runtime work, including the
in-process agent loop, stateful harness-backed print, JSON, and interactive
turns, provider/model catalog surfaces, session storage, project trust, tools,
compaction, RPC controls, TUI components, and client/server support. Remaining
work is tracked explicitly rather than treated as complete just because a
similarly named module exists.

### Agent lifecycle parity

The bounded `pi-agent` lifecycle slice now tracks active runs with a
panic-safe lease and abort signal, rejects concurrent prompt/continue calls,
waits for async listener settlement before becoming idle, and drains live
steering/follow-up queues at the upstream turn boundaries. Delayed
deterministic push-stream tests cover abort, continuation validation,
assistant-tail queueing, and both queue modes; the focused lifecycle suite is
21/21 green and the complete `pi-agent` package suite is 294/294 green. The
remaining parity limitation is that subscriber callbacks are replayed after
the low-level loop rather than dispatched live at each event.

The current implementation slice completes constrained JSON-schema and
OpenAI grammar custom-tool parity across the advertised pi-ai adaptors. Strict
schemas are cloned and rewritten without mutating caller input; unsupported
required schemas return the upstream diagnostics, and grammar tool input is
assembled monotonically through streaming Responses/Completions events. The
S-008 implementation commit `7a72f2fe104cf660f946f29a822c88da556a37d1`
is pushed to `origin/main` and hash-verified; the image retry/cancellation and
telemetry checkpoints below remain part of the pushed baseline.

### RPC/protocol command parity

The RPC runtime exposes extension, prompt-template, and skill discovery through
`get_commands`, dispatches extension prompts, expands skills and templates,
preserves image attachments, honors queued `streamingBehavior`, and follows
the upstream JSONL framing and inbound `extension_ui_response` semantics.
Direct stable offline tests cover the RPC runtime, JSONL, JSON-event and RPC
types, a real binary multi-turn session, and the adjacent CBOR protocol. The
Rust extension host still cannot originate or resolve extension UI requests;
credentialed live-provider inference is not implied by these offline checks.

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

### Updating pi-rust

pi-rust is a separately maintained Rust distribution. Interactive startup does
not query `pi.dev` for an upstream Pi release and never displays an
`Update available: pi ...` notice. `pi-rust update` is not a self-updater for
the compiled Rust binary:

- `pi-rust update --extensions` updates installed extension packages.
- `pi-rust update --models` refreshes the model catalogs.
- `pi-rust update`, `pi-rust update --self`, and self-updating `--all` report the Rust
  distribution boundary and exit non-zero; they do not query an upstream Pi
  release or replace the running executable.

Update pi-rust from its source repository, then rebuild and reinstall it using
the mechanism that owns your installation:

```bash
cd "/run/media/mustbearnold/Projects/AI Agents/pi-rust"
git pull --ff-only
/home/mustbearnold/.cargo/bin/cargo build --release -p pi-coding-agent
```

### Prompt-cache notices

Interactive mode keeps cache accounting live even though it defers JSONL
persistence until exit. Enable `Cache miss notices` in `/settings` to inject
significant cache-miss notices into the transcript; `/session` reports
cumulative `Cache Re-billed` tokens, cost, and miss count. Auto-compaction,
`/clear`, new sessions, resume, and import boundaries reset or reload the
cache segment so notices stay attached to the correct prompt history.

### Install telemetry

Interactive startup may send the separate anonymous install-telemetry report
only on a fresh or version-changed install boundary. This report is not an
upstream release check and never drives an update notice. It is best-effort and
backgrounded, with a bounded retry/timeout policy. Set `PI_TELEMETRY=0` (or
disable `Install telemetry` in `/settings`) to opt out; `PI_OFFLINE=1` disables
the transport before any network request.

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

### Exhaustive local user-flow verification

The Rust CLI has deterministic coverage for interactive TUI editing and
slash-command PTYs, sequential argv and piped-stdin prompts, print, JSON, and
RPC modes, session JSONL persistence, commands, resources, trust, error paths,
bracketed paste, terminal restoration, and the optimized release binary. Run
the focused user-flow matrix with:

```bash
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline \
  --test interactive_full_matrix --test interactive_slash_pty \
  --test config_selector_pty --test cli_print_parity --test cli_json_mode \
  --test cli_commands --test cli_resources --test cli_trust \
  --test cli_flag_matrix --test interactive_release_multiturn \
  --test rpc_binary_multiturn -- --test-threads=1
```

The full debug and release workspace suites use bounded concurrency:

```bash
/home/mustbearnold/.cargo/bin/cargo test --workspace --offline --quiet -- --test-threads=2
/home/mustbearnold/.cargo/bin/cargo build --workspace --release --offline
/home/mustbearnold/.cargo/bin/cargo test --workspace --release --offline --quiet -- --test-threads=2
```

These are offline/faux-provider and local PTY/RPC tests. Credentialed live
provider inference, third-party extension behavior, alternate terminal
emulators, and the separate JavaScript/mise `pi` found on PATH are not implied
by this deterministic verification.

For a release build:

```bash
cargo build --release -p pi-coding-agent
./target/release/pi --help
```

The pinned upstream source and its tests are the parity oracle; behavior is not
marked complete without evidence from the relevant test or live command.

Latest parent verification (2026-08-29): the current tree passes the full
offline `pi-tui` matrix (383 library tests plus all integration targets), the
full `pi-ai` matrix (441 library tests plus all integration targets), and the
full `pi-coding-agent` matrix (818 library tests plus all integration
targets), along with package checks, strict clippy, stable formatting, and
scoped diff checks. These gates do not imply complete parity: the machine
dashboard still reports 30/318 (9.43%) whole-product behavioral rows, and TUI
overall remains 0/52 until every functional, evidence, and visual/interaction
row is closed.

The subsequent workspace-level all-target test matrix, strict workspace
clippy gate, and optimized release build also pass on this tree. The installed
`pi-rust` launcher resolves to `target/release/pi` and reports
`pi 0.85.1`; this confirms build/install health, not full behavioral or
visual parity.

Latest serialized provider/harness verification (2026-08-29): the real
loopback Qwen Token Plan dispatch fixture passed 1/1 after switching the
provider to honor each model's base URL, and `pi-ai` check plus strict
all-target clippy passed. The current `pi-agent` harness/environment tree
passed 366 tests across all targets and strict clippy. These checks strengthen
runtime evidence but do not promote a row by themselves; live vendor,
platform, row-complete TUI, and visual/interaction boundaries remain open.

The same wave also passed 17/17 focused alt-screen TUI tests, 35/35
OpenAI-compatible handoff tests plus 5/5 cross-provider handoff fixtures, and
the noninteractive missing-session-CWD regression 1/1. Combined checks and
strict clippy pass across all affected packages; the parity percentages remain
unchanged until their full row contracts are closed.

Latest serialized parity follow-up (2026-08-29): the SSE parser passed 13/13
focused tests, OpenAI Responses model-derived base-URL routing passed its real
loopback test, and the CLI-035 loopback process passed 1/1 after proving the
provider-visible `AGENTS.md` difference under `--no-context-files`. The current
tree then passed 441 pi-ai library tests plus all integration targets and 818
pi-coding-agent library tests plus all integration targets, with strict clippy
and package checks green. These are evidence gates only; live vendor,
cross-platform, and complete TUI visual/interaction boundaries remain open.

## License

MIT — see [LICENSE](LICENSE). The port targets the MIT-licensed
[earendil-works/pi](https://github.com/earendil-works/pi).

## Engineering gates

Production code across the workspace is progressively adopting hard clippy
gates (`[workspace.lints.clippy] unwrap_used/expect_used/panic = "deny"`,
opted into per crate via `[lints] workspace = true`). Converted crates so
far: every workspace crate (`pi-evals`, `pi-server`, `pi-client`, `pi-ai`,
`pi-agent`, `pi-coding-agent`, `pi-tui`, `pi-telemetry`,
`pi-session-backends`).
The async auth-storage surface also uses the typed `AuthStorageError`. Test
code carries scoped `#[allow]`s only.
Campaign closed 2026-08-30: the `let _ =` swallow triage found no hidden
error handling, and the settings/models_store persistence panics are
deliberate upstream-mirroring behavior behind documented allows.
See `PLAN.md` for the Rust-idiom campaign status. The pi-protocol layer
(cbor/codec/framing/schemas) and the exhaustive parity registers carry the
behavioral-parity campaign checkpoints; see `docs/EXHAUSTIVE-PARITY-INVENTORY.md`
and `docs/NON-TUI-PARITY-STATUS.md`.
