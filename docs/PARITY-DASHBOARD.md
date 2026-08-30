# pi-rust parity progress dashboard

This is the single current checkpoint for the separate progress dimensions.
Run `cargo run -p pi-coding-agent --offline --quiet --bin parity_audit --
dashboard` from the repository root to recompute and validate it. The
pre-commit hook requires the same metrics in this file, `README.md`,
`PLAN.md`, `HANDOFF.md`, and the TUI register.

The percentages are intentionally not collapsed into one misleading score:

- Source conversion measures reconciliation of the 166-row source ledger.
- Inventory census measures whether the 318 behavioral capability IDs are
  indexed; it is coverage, not implementation.
- Scoring coverage measures how many of those IDs have normalized status
  dimensions. All 318 rows now have normalized PASS/PARTIAL/OPEN fields;
  this is schema coverage, not completion.
- Root-gate closure measures completed acceptance gates, not product parity.
- Rust-only distribution boundary measures executable JS/TS source files in
  the product tree; generated Rustdoc assets are excluded from that boundary.
- TUI completion dimensions measure functional behavior, test/evidence proof,
  visual/interaction comparison, and the intersection of all three.
- Whole-product behavioral parity is the intersection of the TUI overall
  count and the non-TUI overall count over all 318 inventory IDs.
- The separate non-TUI register is fully normalized, but most rows remain
  `OPEN`; register coverage is not a completion percentage.

## Current measured checkpoint — 2026-08-30

<!-- PARITY_DASHBOARD_METRICS:START -->
Source/conversion ledger: 100.00% (166/166; 0 open)
Acceptance inventory census: 100.00% (318/318) (318 IDs indexed)
Acceptance scoring coverage: 100.00% (318/318) (318 of 318 IDs scored)
Root acceptance gates: 100.00% (8/8) (8 passed; 0 open)
Rust-only distribution boundary: 100.00% (0 JS/TS executable source files; generated Rustdoc excluded)
TUI functional implementation: 19.23% (10/52)
TUI test/evidence parity: 19.23% (10/52)
TUI visual/interaction parity: 0.00% (0/52)
TUI overall parity: 0.00% (0/52)
Non-TUI implementation parity: 18.42% (49/266 PASS; 194 PARTIAL; 23 OPEN)
Non-TUI deterministic evidence parity: 13.53% (36/266 PASS; 207 PARTIAL; 23 OPEN)
Non-TUI runtime-boundary parity: 13.91% (37/266 PASS; 154 PARTIAL; 75 OPEN)
Non-TUI overall parity: 11.28% (30/266)
Whole-product behavioral parity: 9.43% (30/318)
<!-- PARITY_DASHBOARD_METRICS:END -->

Latest serialized verification on 2026-08-30: JSON mode emits the official v3
session header and durable v3 records while native pi-agent v4 storage remains
compatible. Its streamed event sink writes incrementally, emits the official
initial `toolCall` placeholder, normalizes `toolUse` stop reasons, and emits
`agent_settled`; a real optimized-release Qwen tool turn matched the official
Pi envelope on the checked path. The workspace all-target matrix, strict
workspace clippy, and optimized release build pass on the current tree. The
latest package rerun passes 444 pi-ai, 822 pi-coding-agent, and 386 pi-tui
library tests with all package integration targets, plus strict package
check/clippy. These are evidence-strengthening
gates, not row-complete parity.

The current product-level conclusion is therefore: the source rewrite is
reconciled, the behavioral inventory is fully indexed, and the TUI has a
measured partial implementation/evidence slice, but pi-rust is not yet a
100%-parity or flawless product. The root visual review is closed for its
declared matrix; the 52-row visual register and most non-TUI capability rows
remain open.

The latest CLI-044/047 leaf is parent-verified: real print/JSON signal probes,
interactive signal PTYs, broken-pipe help/version probes, RPC child-failure
evidence, experimental strict-policy tests, package check, and strict clippy
all pass. Optimized release-binary signal probes independently confirm print/
JSON SIGTERM=143 and SIGHUP=129 with empty output. The follow-up CLI-005..011
leaf also passes args/run/print/JSON focused suites, release BOM/Unicode and
missing-`@file` process checks, and signal-aware print/JSON cancellation.
Vendor/platform, live provider, Windows, and exhaustive file/input boundaries
remain open.

The latest source-only residual wave is parent-verified by changelog 6/6 and
skills 12/12 focused tests, coding-agent all-target check/clippy, and static
checks. The fixes are edge-case alignment only; the normalized percentages
remain unchanged because complete row boundaries are still open.
The latest verified source wave added terminal/image/scrollbar protocol coverage
in pi-tui, provider-independent SSE/event-stream/abort coverage across seven
AI adaptors, and the upstream HOME/USERPROFILE environment fix. The next
disjoint source wave is active: B1 is taking a pi-tui-only parity slice, B2 is
taking a non-TUI adapter/runtime slice outside JSON and session-v3 paths, and
D1 is taking a separate non-TUI acceptance slice. Cargo verification remains
serialized.

The latest CLI-modes checkpoint passed the real exhaustive CLI process suite
(6/6), the experimental-policy unit suite (4/4), the main CLI unit suite
(4/4), package check, strict clippy, formatting, and diff checks. CLI-035 and
CLI-039 are now conservatively `PARTIAL`; interactive context-difference and
verbose-startup/signal boundaries remain open.

The provider/catalog checkpoint also passed seven model-catalog tests, pi-ai
check, strict clippy, JSON validation, formatting, and diff checks. PROV-020,
PROV-021, and PROV-022 now have conservative implementation/evidence `PARTIAL`
credit; authenticated transport and live vendor boundaries remain open.

The TUI controller checkpoint passed 360 pi-tui library tests plus every
integration target, strict clippy, full stable formatting, and scoped diff
checks. Controller coverage now includes deferred/coalesced owner repaint,
hardware-cursor placement/toggles, overlay lifecycle, scrollback-preserving
regular stop, shrink clearing, resize repaint, and fullscreen restoration;
the 52-row TUI dimensions remain unchanged pending complete row contracts and
manual visual comparison.

The preceding CLI leaf passed the pi unit suite 5/5, experimental tests 4/4,
and the real CLI process suite 6/6 after routing normal print-mode final text
through the guarded stdout writer. CLI-044/047 now have stronger real process
evidence, but remain PARTIAL for vendor/platform and complete lifecycle
boundaries.

That wave is parent-verified: pi-tui passed 362 library tests plus every
integration target and strict clippy; the Qwen provider matrix passed 7/7
with pi-ai check/clippy and static gates; and session restart 6/6,
concurrency 2/2, run 28/28, and interactive-mode 51/51 passed. PROV-028..030
now have implementation/evidence PARTIAL credit with runtime OPEN. The
selection/overlay/search TUI source pass is now closed at the gate boundary
without a row promotion; the project-trust audit confirms that `/trust` still
needs the upstream project-scoped modal integration. The next source wave
covers remaining TUI rendering/tool/animation surfaces, Xiaomi/token-plan
provider rows, and trust integration.

The latest serialized checkpoint parent-verified the residual provider,
agent-runtime, and transcript/TUI leaves and revalidated root gates R1–R8.
The real PTY matrix also now proves rapid Unicode/multiline bracketed-paste
marker echo under one second and exact payload persistence after submission.
Those results are evidence improvements; the separately verified TUI-052
coexistence row is the current functional/evidence promotion reflected in the
values above.

The current post-session/provider gate also passed Anthropic parity (9/9),
Copilot OAuth/provider parity (5/5 plus 4/4 coding-agent cases), Bedrock (38
unit plus 7 transport cases), Mistral (20 unit plus 4 adaptor cases), the
explicit `--session-id`/`--no-session` regression (1/1), and the real CLI
session restart matrix (5/5). A complete workspace all-target rerun and
strict workspace clippy are green. PROV-001, PROV-003, PROV-004, PROV-007,
PROV-008, PROV-011, PROV-019, PROV-023, and PROV-024 are promoted to PARTIAL
in the non-TUI register; live vendor traffic and the remaining row contracts
are still open.

The 2026-08-29 follow-up wave remains green after the CLI-001..CLI-011 and
terminal/image leaves: 806 pi-coding-agent library tests, 378 pi-tui library
tests plus every pi-tui integration target, 433 pi-ai library tests plus every
pi-ai integration target, 69 focused CLI tests, strict workspace clippy, and
the optimized release build pass. These gates do not
promote a row without its complete required behavior and applicable
live/emulator/visual evidence.

The latest ENV/CLI-044 source slice is also parent-verified: core settings
28/28, telemetry 8/8, and the session-root precedence regression 1/1 pass,
with coding-agent check, strict all-target clippy, stable formatting, and
scoped diff checks green. This improves the ENV-005/006 implementation and
deterministic evidence, while process, signal, broken-pipe, and live network
boundaries remain open.

The OpenCode/OpenCode-Go/OpenRouter source slice is parent-verified as well:
provider units 31/31, provider matrix 7/7, pi-ai all-targets 419 library
tests plus every integration target, downstream coding-agent check/clippy,
strict pi-ai clippy, stable formatting, and scoped diff checks pass.
PROV-025..027 now have implementation/evidence PARTIAL credit; live vendor and
complete stream/error/retry boundaries remain open.

The subsequent xAI checkpoint is parent-verified: xAI provider tests 33/33,
the auth-flow suite 8/8, provider matrix 7/7, and pi-ai all-targets 425
library tests plus every integration target pass with strict clippy,
coding-agent check, and static gates. PROV-033 now has implementation/evidence
PARTIAL credit; live xAI traffic, device authorization, and complete external
stream/error/retry boundaries remain open.

The latest serialized package recheck is green: pi-tui passed 370 library
tests plus every integration target and strict clippy, and D1's session-env
change passed 4 focused tests plus coding-agent check/clippy. This is recorded
as stronger evidence for existing partial rows; no row-level metric changed.

The latest provider recheck is green: Together and Vercel AI Gateway pass the
complete pi-ai all-target suite (427 library tests plus every integration
target), strict clippy, JSON/static validation, and downstream source gates.
PROV-031/032 are PARTIAL for implementation and deterministic evidence, while
live vendor traffic and complete stream/error/retry/abort boundaries remain
OPEN. The machine-checked register reports 49/166/51 for implementation and
36/179/51 for deterministic evidence.

The 2026-08-28 closure checkpoint additionally parent-verified the
session/tree/auth/clipboard component slice and the immediate cached-scene
composer repaint. Real PTY composer evidence measured 20 per-keystroke samples
with p95/max 3.98 ms, and the full R1-R7 root gates passed after the change.
These are scoped evidence improvements; no dashboard dimension is promoted
until its complete row contract and visual/interaction evidence are closed.

The latest 2026-08-29 wave is parent-verified: pi-tui passed 367 library
tests plus every integration target and strict clippy; the Xiaomi/Token Plan
and Z.AI provider fixtures passed 2/2 and 3/3 with pi-ai check/clippy and
JSON/static gates; and the trust/session caller gates passed project-trust
13/13, `cli_trust` 9/9, session restart 6/6, interactive full matrix 7/7,
real PTY 10/10 plus one intentional live ignore, slash completion 5/5, and
run-unit 33/33 with coding-agent check/clippy/static gates. PROV-034..039 and
TRUST-001/002 now carry conservative PARTIAL credit in the applicable
dimensions; CLI-013..019 remain partial pending their complete path/error/
restart matrix. No row reached PASS, and live vendor, full trust lifecycle,
and visual boundaries remain open.

The native llama.cpp/local-provider loopback fixture also passes 11/11 with
coding-agent all-target strict clippy; PROV-040 is PARTIAL for implementation
and deterministic evidence, while external-server and platform/restart
behavior remain open.

The environment/config checkpoint passed `config::tests` 18/18, including
exact upstream `env_flag` truthiness and empty agent/session-root fallback;
ENV-004, ENV-005, and ENV-006 are now PARTIAL for implementation and
deterministic evidence, with runtime precedence still open.

The next source wave is active in disjoint scopes: B1 is closing remaining
pi-tui renderer, overlay/search, scrollback, resize, scheduler, and animation
boundaries (`TUI-001..014`, `TUI-040..047`); B2 is auditing release, install,
upgrade/rollback, and official first-run behavior (`DIST-001/002/004/005`)
within the package/distribution surface; and D1 is auditing the remaining
environment/process and strict-mode boundaries (`ENV-007/009/013/014/015`,
`CLI-044`, `CLI-047`). Cargo verification remains serialized behind each
source checkpoint.

The final 2026-08-28 release checkpoint also passed the optimized workspace
all-targets test gate, strict workspace clippy, the release binary build, and
release executable smoke checks for version/help/catalog discovery. The real
authentication, settings, composer, and PTY suites are included in that gate.
One harmless authenticated international Qwen `qwen3.8-max` request returned
`QWEN_LIVE_OK` without exposing the environment credential. Official Pi 0.84.3
and pi-rust also selected the same Qwen provider/model in isolated 100x30 and
80x24 startup PTYs; normalized captures matched at both sizes in regular and
fullscreen modes after the startup spacing fix. The per-capability visual
register and remaining mostly open product/live-provider boundaries are still
open. The rebuilt release binary exposes both providers through
`--list-models glm-5.2` with their respective synthetic API-key environment
variables.

The latest SI4 parent recheck passed settings-panel 9/9, real settings PTY
2/2, core-settings 27/27, interactive-mode 50/50, parity-audit 8/8, the full
workspace test matrix, strict workspace clippy, formatting/diff checks, and
the optimized release build. SI4 remains open until all 29/31
capability-gated settings rows have explicit persistence/live/cancel/restart
evidence; these aggregate gates do not change the row-based percentages.

The current model-scope caller checkpoint also passed the run resolver suite
(22/22), CLI print parity (10/10), JSON mode (7/7), and RPC multi-turn (2/2).
Interactive, JSON, and RPC startup now apply CLI-over-settings model scopes
after native provider registration. The current-tree workspace rerun, strict
clippy, release build/smoke, parity register, and root R1-R7 reverification
are green; remaining provider/auth/live and row-specific boundaries stay
explicitly partial or open.

The latest session-runtime checkpoint passes the five focused
`core::agent_session_runtime` tests, coding-agent check, strict all-target
clippy, stable rustfmt, and scoped diff checks. The missing-stored-cwd guard is
now applied before switch/import teardown, and `previous_session_file` is
propagated through replacement; SES-009 and SES-012 remain partial until
their complete interactive, restart, malformed-input, and process matrices
are evidenced.

The detailed TUI row register is
[`TUI-PARITY-STATUS.md`](TUI-PARITY-STATUS.md). The exhaustive capability
census and evidence rules are in
[`EXHAUSTIVE-PARITY-INVENTORY.md`](EXHAUSTIVE-PARITY-INVENTORY.md). The
machine-validated non-TUI row register and its categorized residual leaves are
in [`NON-TUI-PARITY-STATUS.md`](NON-TUI-PARITY-STATUS.md); run
`parity_audit register` to validate its 266-row join without changing the
existing dashboard percentages.

## Latest parent verification — 2026-08-29 — package-wide gates

The current serialized package gates pass: `pi-tui` reports 383 library tests
plus every integration target and strict all-target clippy; `pi-ai` reports
441 library tests plus every integration target and strict all-target clippy;
and `pi-coding-agent` reports 818 library tests plus every integration target,
package check, and strict all-target clippy. Full stable rustfmt, scoped diff,
and trailing-whitespace checks also pass for the three package scopes.

No parity dimension is promoted solely from package-wide green gates. The
current machine-checked values remain source/conversion 166/166, inventory
318/318, TUI functional/evidence 10/52, TUI visual/overall 0/52, non-TUI
overall 30/266, and whole-product behavioral 30/318 (9.43%). Live provider,
cross-platform, process, and manual visual/interaction boundaries remain
explicitly open or partial in the row registers.

The follow-on workspace gate also passed: workspace all-target tests at
`--test-threads=1`, strict workspace clippy, and the optimized workspace
release build all completed successfully. The installed launcher resolves to
`target/release/pi` and reports `pi 0.84.2`; dashboard percentages are
unchanged because build health does not close row-specific parity boundaries.

## Latest serialized verification — 2026-08-29 — provider and harness follow-up

The real Qwen Token Plan model-base-URL dispatch fixture passed 1/1 against a
loopback HTTP server, including the actual provider closure, auth header, and
streamed completion; `pi-ai` check and strict all-target clippy pass. The
current `pi-agent` harness/environment tree passed 366 tests across all
targets and strict clippy. Metrics remain unchanged because live vendor,
platform, recovery, and complete TUI visual/interaction boundaries are still
open.

The same serialized wave passed 17/17 focused alt-screen TUI tests, 35/35
OpenAI-compatible handoff tests plus 5/5 cross-provider handoff fixtures, and
the noninteractive missing-session-CWD regression 1/1. Combined package check
and strict clippy pass; no metric is promoted from focused evidence alone.

## Latest serialized verification — 2026-08-29 — loopback routing and SSE

The SSE parser focused suite passes 13/13. OpenAI Responses model-derived
base-URL routing passes a real loopback stream test, and the CLI-035 loopback
process passes 1/1 after capturing the normal versus `--no-context-files`
provider-visible system prompt. The post-fix package matrices pass 441 pi-ai
library tests and 818 pi-coding-agent library tests plus all integration
targets; package checks and strict clippy pass. Metrics remain unchanged:
live vendor, platform, recovery, and complete TUI visual/interaction
boundaries are still open.

## Latest serialized verification — 2026-08-30 — Z.AI providers and models.json listing

The existing native Z.AI registrations are verified for both the international
`zai` provider and the `zai-coding-cn` regional provider. The focused
`zai_provider_parity` target passes 4/4, covering registration, catalogs,
scoped API-key precedence, reasoning/tool/max-token request construction, and
a real local loopback streaming request through the provider closure. The
models.json list-models overlay/auth target passes 2/2, covering authenticated
fuzzy search, unauthenticated filtering, and malformed-config diagnostics.

The pi-ai all-target suite passes 444 library tests and all integration
targets; pi-coding-agent passes 822 library tests and all integration targets;
package check and strict all-target clippy pass. The fixtures use synthetic
credentials and local servers only. No live Z.AI vendor request was made, so
vendor quota/error/retry, platform, and complete parity boundaries remain
open. The rebuilt release binary exposes both providers through
`--list-models glm-5.2` with their respective synthetic API-key environment
variables.

The Rust-idiom campaign checkpoints (typed errors, hard lint gates in
pi-evals and pi-server) do not move any dashboard dimension; all counts above
are unchanged by that work.

Phase 2.2 (pi-client) does not move any dashboard dimension.

Phase 2.3a (pi-ai) does not move any dashboard dimension.

Phase 2.3b (PiAiError) does not move any dashboard dimension.

Phase 2.4 (pi-agent) does not move any dashboard dimension.

Phase 2.5 (pi-coding-agent gate) does not move any dashboard dimension.

Phase 2.7 (workspace-wide gate) does not move any dashboard dimension.

Phase 2.6 (AuthStorageError) does not move any dashboard dimension.
