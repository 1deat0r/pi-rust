# pi-rust

An in-progress **1:1 Rust port of the [pi coding agent](https://github.com/earendil-works/pi)** (v0.84.2, pinned upstream commit `5cd93f6`). The project targets the same CLI surface, session formats, provider behavior, tools, and wire contracts in idiomatic Rust.

## Current status

**Behavioral parity status: exhaustive audit in progress — not yet 1:1 or
flawless.** The older source/conversion ledger still reports 166/166, but that
is not a behavioral completion metric. The current acceptance index contains
318 unique capability IDs, including real CLI, auth, provider, transport,
agent, session, TUI, RPC, extension, release, and adversarial cases.

The current audit must pass the executable inventory, debug/release workspace
matrices, PTY/TUI matrix, real provider checks, clean-environment checks, and
installed-command check before parity can be claimed. Current results and
residuals are recorded in `docs/EXHAUSTIVE-PARITY-INVENTORY.md` and the
active `.unlazy/parity-20260827/` scope.

The latest serialized checkpoint is green on the current tree. JSON mode now
uses the official v3 session header and durable v3 session format while native
pi-agent v4 storage remains compatible; the JSON event sink writes
incrementally, emits the initial tool-call placeholder, normalizes `toolUse`
stop reasons, and emits `agent_settled`. A real release-binary Qwen tool turn
matched the official Pi envelope for the checked tool/result path. The full
workspace all-target matrix, strict workspace clippy, and optimized workspace
release build pass on the current tree. The latest serialized package rerun
 passes 444 pi-ai, 822 pi-coding-agent, and 386 pi-tui library tests with all
package integration targets, plus strict package check/clippy. This is
strong package/runtime evidence, not a 100% parity claim; row-complete JSON,
TUI visual, live-provider, platform, and recovery boundaries remain open.

The latest 2026-08-29 verification is green for the complete offline
workspace all-targets matrix, strict workspace clippy, optimized release
build, release `--version`/`--help`, offline Qwen Token Plan catalog listing,
Anthropic parity (9/9), the Copilot OAuth suites (5/5 plus 4/4), the focused CLI matrix (69/69), and parity
register/dashboard smoke. The run also corrected stale 1,270-model assertions
and the RPC golden fixture; the embedded catalogs now contain 1,292 built-in
models (runtime provider overlays can produce a larger model list).

The latest 2026-08-30 provider checkpoint confirms the native `zai` and
`zai-coding-cn` registrations, catalogs, scoped API-key auth, request-shape
handling, and real loopback streaming in `zai_provider_parity` (4/4). The
models.json list-models overlay/auth regression is also green (2/2), including
authenticated fuzzy search, unauthenticated filtering, and malformed-config
diagnostics. The full pi-ai all-target suite (444 library tests) and
pi-coding-agent all-target suite (822 library tests) pass with package check
and strict clippy. These fixtures use synthetic credentials and local servers;
live Z.AI vendor traffic remains unverified. The rebuilt release binary also
exposes both registrations through `--list-models glm-5.2` when supplied with
their respective synthetic `ZAI_API_KEY` or `ZAI_CODING_CN_API_KEY` environment
variable.

The 2026-08-31 models.json runtime checkpoint closes the previous
catalog-only custom-provider seam: models.json-only API-key providers now
compose into an isolated native Rust provider registry and dispatch through
the registered API adaptor. Focused evidence passes 16 registry tests, 18
pi-ai model tests, and a real local HTTP streaming fixture that verifies
configured Bearer/header behavior without persisting credentials. Strict
pi-ai/pi-coding-agent check and all-target clippy pass. A synthetic in-memory
stored-OAuth regression also proves configured request headers/authHeader are
applied without replacing login, refresh, or subscription behavior.
A three-launch isolated process fixture proves overlay appearance, deletion on
restart, and malformed-config warning/fallback without stale model retention.
The active interactive runtime now also recomposes on `/reload`, extension
reload, and model-catalog selection; a real PTY changes the model metadata and
completes a subsequent turn without restarting. MODEL-002 and MODEL-003 are
PASS in all three dimensions. Existing runtime facades also observe external
auth.json and models-store.json add/replace/remove operations without restart.
MODEL-004 is PASS/PASS/PASS after real text/JSON process coverage exposed and
fixed case-sensitive provider startup and faux thinking-suffix bypasses.

The latest session-runtime gate also passes all five focused
`core::agent_session_runtime` tests, coding-agent check, strict all-target
clippy, stable formatting, and scoped diff checks. Session switching and JSONL
import now reject a missing stored cwd before tearing down the active runtime,
and replacement propagates `previous_session_file`; the broader
session/restart/process acceptance rows remain partial.
The current post-wave TUI gate passes 386 pi-tui library tests plus every
integration target, with strict clippy, stable formatting, and scoped diff
checks. The latest provider gate passes 444 pi-ai library tests plus every
integration target, including the Anthropic thinking-budget edge case. The
coding-agent gate passes 822 library tests plus every integration target,
including real cross-project session cancel/fork PTYs. The latest trust matrix
also passes project-trust 13/13 and real `cli_trust` 9/9; emulator-specific
visual comparison, full trust lifecycle, and live provider behavior remain
open.

The latest residual source wave corrected two upstream edge cases: changelog
link targets beginning with a digit are repository paths rather than URL
schemes, and non-file `SKILL.md` markers no longer hide valid skills. The
focused regressions pass 6/6 changelog and 12/12 skills tests; coding-agent
all-target tests and strict clippy remain green.

The native llama.cpp/local-provider checkpoint is also green: its real
loopback HTTP catalog, auth, OpenAI-compatible stream, load/unload/download
progress, cancellation/timeout, and failure matrix passes 11/11 with strict
coding-agent clippy. A real external llama.cpp installation and live
platform/restart behavior remain open.

The environment/config checkpoint also verified the exact upstream boolean
truth set and empty agent/session-root fallback (`config::tests` 18/18);
ENV-004, ENV-005, and ENV-006 now have conservative implementation/evidence
PARTIAL credit, with clean-process and runtime precedence still open.

The OpenCode/OpenCode-Go/OpenRouter wave is parent-verified: provider units
31/31, the provider matrix 7/7, pi-ai all-targets 419 library tests plus
integration targets, downstream coding-agent check/clippy, and strict/static
gates pass. PROV-025..027 now have implementation/evidence PARTIAL credit;
live vendor and complete stream/error/retry boundaries remain open.

The subsequent xAI checkpoint is parent-verified: xAI provider tests 33/33,
the auth-flow suite 8/8, provider matrix 7/7, and pi-ai all-targets 425
library tests plus every integration target pass with strict clippy,
coding-agent check, and static gates. PROV-033 now has implementation/evidence
PARTIAL credit; live xAI traffic, device authorization, and complete external
stream/error/retry boundaries remain open.

The latest serialized follow-up is also green: the repaired pi-tui tree passes
370 library tests plus every integration target and strict all-target clippy;
the D1 session-environment change passes all 4 focused tests, coding-agent
check, and strict all-target clippy. These checks strengthen selector/overlay,
key-release, and child-environment coverage without completing a full row;
TUI visual/manual evidence and the remaining ENV/process boundaries stay open.

The latest provider recheck is also green: Together and Vercel AI Gateway
catalog/API changes pass the complete pi-ai all-target suite (427 library
tests plus every integration target), strict clippy, JSON/static validation,
and downstream source gates. PROV-031 and PROV-032 are PARTIAL for
implementation and deterministic evidence; live vendor traffic and complete
stream/error/retry/abort boundaries remain OPEN. The authoritative register
is 49 PASS/194 PARTIAL/23 OPEN for implementation and 36 PASS/207 PARTIAL/23
OPEN for deterministic evidence.

The latest CLI-044/047 leaf is parent-verified: real print/JSON signal probes,
interactive signal PTYs, broken-pipe help/version probes, RPC child-failure
evidence, experimental strict-policy tests, package check, and strict clippy
all pass. Optimized release-binary signal probes independently confirm print/
JSON SIGTERM=143 and SIGHUP=129 with empty output. The follow-up CLI-005..011
leaf also passes args/run/print/JSON
focused suites, release BOM/Unicode and missing-`@file` process checks, and
signal-aware print/JSON cancellation. Live provider, Windows, and exhaustive
file/input boundaries remain open.
The latest verified source wave added terminal/image/scrollbar protocol coverage
in pi-tui, provider-independent SSE/event-stream/abort coverage across seven
AI adaptors, and the upstream HOME/USERPROFILE environment fix. The next
disjoint source wave is active: B1 is taking another pi-tui-only parity slice,
B2 is taking a non-TUI adapter/runtime slice outside JSON and session-v3 paths,
and D1 is taking a separate non-TUI acceptance slice. Cargo verification
remains serialized by the parent.

The older source-ledger result remains historical context:

**Source/conversion ledger: 100.00% — 166 of 166 ledger tasks complete; 0
open.**

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
Non-TUI implementation parity: 39.85% (106/266 PASS; 160 PARTIAL; 0 OPEN)
Non-TUI deterministic evidence parity: 36.09% (96/266 PASS; 170 PARTIAL; 0 OPEN)
Non-TUI runtime-boundary parity: 19.17% (51/266 PASS; 164 PARTIAL; 51 OPEN)
Non-TUI overall parity: 18.05% (48/266)
Whole-product behavioral parity: 15.09% (48/318)

See [`docs/PARITY-DASHBOARD.md`](docs/PARITY-DASHBOARD.md) for the
definitions and the machine-validated current checkpoint.

The latest verified runtime checkpoint is stronger than the historical ledger
but is not a blanket 1:1/flawless claim: the debug and optimized workspace
all-targets test gates and strict workspace clippy pass, and the release binary
launches from both
`pi-rust` (with the official `pi` kept independent for side-by-side use), root gates R1–R8 pass, `/login openai-codex` reaches the real
browser OAuth URL and cancels cleanly, and a real stored OpenAI Codex OAuth
credential completed two print turns and two interactive PTY turns. The
submenu Kitty CSI-u release regression and the direct `!!` Bash-completion
regression are release-tested, and the complete workspace release suite is
green. The five-case release authentication PTY suite now also proves that a
bracketed-pasted Qwen API key is masked, persisted, and removable through
`/logout`. The international Qwen Token Plan catalog is embedded and
selectable; an authenticated Qwen turn still requires the operator's real API
key. The
exhaustive 318-ID acceptance campaign remains open for per-capability evidence,
the 52-row visual/interaction reviews, and live-provider/restart/error-recovery
boundaries. The root visual review is closed only for the explicitly recorded
startup/settings/theme/warnings/model-thinking matrix. Official Pi 0.84.3 and
pi-rust startup captures matched after version normalization at 100x30 and
80x24 in both regular and fullscreen modes. The latest serialized
checkpoint also parent-verified the provider, agent-runtime, and transcript/TUI
residual leaves; their evidence does not change the row-based percentages
until the corresponding capability rows are fully scored.

The latest closure checkpoint also parent-verified the session/tree/auth/
clipboard component fixtures, live settings paths, and the immediate
cached-scene composer repaint. Real PTY typing evidence measured 20
per-keystroke samples at p95/max 3.98 ms, and the rapid Unicode/multiline paste
case passed. The optimized workspace release gate and final executable smoke
checks also passed. The conservative TUI percentages remain unchanged because
the full row-level and visual contracts are still open.

The current serialized provider/session gate also passes Anthropic parity (9/9),
Copilot OAuth/provider parity (5/5 plus 4/4 coding-agent cases), Bedrock (38
unit plus 7 transport cases), Mistral (20 unit plus 4 adaptor cases), the
explicit `--session-id`/`--no-session` regression (1/1), and the real CLI
session restart matrix (5/5). The complete workspace all-target test matrix,
strict clippy, release build, and installed `pi-rust` smoke pass. These results
promote PROV-001, PROV-003, PROV-004, PROV-007, PROV-008, PROV-011, PROV-019,
PROV-023, and PROV-024 to PARTIAL in the register; they do not close live
vendor or whole-row parity requirements.

The latest settings integration recheck passed the deterministic panel suite
(9/9), real settings PTY matrix (2/2), core-settings tests (27/27),
interactive-mode tests (50/50), parity-audit validator (8/8), full workspace
tests, strict workspace clippy, formatting/diff checks, and the optimized
release build. The exhaustive SI4 row contract remains open until all 29/31
capability-gated settings have explicit persistence/live/cancel/restart proof.

The latest CLI-modes checkpoint also passed the real exhaustive CLI process
suite (6/6), experimental-policy unit suite (4/4), main CLI unit suite (4/4),
package check, strict clippy, formatting, and diff checks. CLI-035 and CLI-039
are now conservatively partial; interactive context-difference and verbose
startup/signal boundaries remain open.

The latest provider/catalog checkpoint passed seven model-catalog tests, pi-ai
check, strict clippy, JSON validation, formatting, and diff checks. PROV-020,
PROV-021, and PROV-022 are now conservatively partial; authenticated transport
and live vendor boundaries remain open.

The latest TUI controller checkpoint passed 360 pi-tui library tests plus
every integration target, strict clippy, stable formatting, and diff checks.
Controller coverage includes deferred/coalesced repaint, cursor placement,
overlay lifecycle, scrollback-preserving stop, shrink/resize repaint, and
fullscreen restoration; TUI row completion and manual visual comparison
remain open.

The latest CLI leaf also routes normal print-mode final text through the
shared guarded stdout writer and passes the pi unit suite 5/5, experimental
tests 4/4, the real CLI process suite 6/6, package check, strict clippy, and
static gates. Signal, broken-pipe, child-failure, and visual boundaries remain
open. The next source wave is auditing pi-tui renderer/editor/input residuals,
the three Qwen Token Plan providers, and session resume/fork callers.

That source wave is now parent-verified: pi-tui passed 362 library tests plus
all integration targets, strict clippy, stable formatting, and diff checks;
the Qwen provider matrix passed 7/7 with pi-ai check/clippy and static gates;
and session caller tests passed restart 6/6, concurrency 2/2, run 28/28, and
interactive-mode 51/51. The three Qwen Token Plan rows remain conservatively
partial with live vendor behavior open. The follow-up trust audit found that
`/trust` still needs the upstream project-scoped modal integration, so no
trust row was promoted from source-only review. The next wave covers remaining
TUI rendering/tool/animation surfaces, Xiaomi and token-plan providers, and
that trust integration.

The current model-scope checkpoint also passes the run resolver suite (22/22),
CLI print parity (10/10), JSON mode (7/7), and RPC multi-turn (2/2). CLI-over-
settings model scopes are resolved after native provider registration in the
interactive, JSON, and RPC startup paths. The current-tree workspace rerun,
strict clippy, release build/smoke, and root R1-R7 recheck are green; these
results do not close the remaining row-specific, live-provider, or visual
parity boundaries.

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
`pi 0.84.2`; this confirms build/install health, not full behavioral or
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
