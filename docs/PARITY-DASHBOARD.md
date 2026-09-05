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

## Current measured checkpoint — 2026-08-31

<!-- PARITY_DASHBOARD_METRICS:START -->
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
Non-TUI deterministic evidence parity: 36.47% (97/266 PASS; 169 PARTIAL; 0 OPEN)
Non-TUI runtime-boundary parity: 19.17% (51/266 PASS; 164 PARTIAL; 51 OPEN)
Non-TUI overall parity: 18.42% (49/266)
Whole-product behavioral parity: 15.41% (49/318)

Latest evidence note (2026-08-31): SES-008 durable reopen now proves message,
model/provider, thinking, active-tool, operation, and queue projection across
the storage and public harness layers. Status and percentages are intentionally
unchanged pending one aggregate caller-level resume/footer PTY or process gate.

X-001/X-002 evidence note (2026-08-31): an environment-cleared real RPC
fixture proves complex Unicode through valid events, durable JSONL, and reopen,
plus omitted/null/empty field semantics, post-error recovery, and no-session
no-write behavior. Both rows are PARTIAL/PARTIAL/PARTIAL; invalid bytes, PTY
width, and the complete cross-schema matrix remain open. The independent gate
is 4/4.

X-003/X-004 evidence note (2026-08-31): a clean-process fixture proves
malformed settings/models/auth/session preservation and recovery, failed-export
no-write, symlinked-root mutation, read-only rejection, unrelated-file safety,
and no staging residue. Both rows are PARTIAL/PARTIAL/PARTIAL; malformed
resource/manifest breadth, traversal/races/crash injection, and non-Unix
semantics remain open. The independent gate is 4/4.

X-005/X-006 evidence note (2026-08-31): a local synthetic clean-process fixture
proves request-scoped API-key non-disclosure in output/persistence and
barrier-released shared-root process isolation with valid sessions and no
staging residue. Both rows are PARTIAL/PARTIAL/PARTIAL; complete credential
surfaces and same-runtime/reconnect/shutdown/platform stress remain open. The
independent gate is 4/4; no live credential/provider claim is made.

X-007/X-008 evidence note (2026-08-31): a real RPC process proves repeated
abort correlation, exactly-once child cancellation, post-cancel reuse, and one
durable entry per command; shared retry/backoff and detached-retry suites are
green. Both rows are PARTIAL/PARTIAL/PARTIAL; complete async-operation,
provider/reconnect/crash, signal, and platform matrices remain open. The
independent gate is 4/4.

X-009/X-010 evidence note (2026-08-31): a real RPC process survives delayed
consumption of more than 200 KiB with valid framing, bounded/full output, deep
JSON recovery, and reuse; a clean Unix no-display/browser, hostile-proxy,
offline, non-TTY path has no side effects. Both rows are
PARTIAL/PARTIAL/PARTIAL; broader resource and cross-platform matrices remain
open. The independent gate is 4/4, including real tmux resize/restoration.

SES-013 evidence note (2026-08-31): interactive manual compaction now honors
the upstream pre-hook/custom-result boundary and exact-once persistence. Status
and percentages remain unchanged pending post-lifecycle and real active-turn,
restart/crash, and live extension/provider gates.

SES-014 audit note (2026-08-31): shared automatic-compaction machinery covers
threshold/reserve/retry/abort/persistence/queue behavior, but the interactive
caller still lacks overflow-reason, retry-state, cancellation, and interrupted
turn/queued-prompt continuation threading. Status and percentages remain
unchanged.

SES-015 audit note (2026-08-31): branch-summary core and RPC integration are
source-aligned for collection, configuration, abort/failure handling,
exact-once persistence, and replay. Status remains unchanged pending interactive
tree navigation and provider/auth/restart/live-extension gates.

SES-016 evidence note (2026-08-31): RPC session statistics now count messages
without tool-call duplication and aggregate all persisted assistant/tool/summary
usage buckets plus cost. The focused gate is 3/3; status and percentages remain
unchanged pending context/footer/restart/process/platform closure.

SES-017 audit note (2026-08-31): HTML export source covers the required content
classes and safety transforms. Status remains unchanged pending the complete
adversarial fixture matrix and browser/visual/asset-loading evidence.

SES-018 audit note (2026-08-31): native v4 and explicit coding-agent v3 JSONL
paths are source-aligned for valid records, metadata, validation, and reopen.
Status remains unchanged pending malformed-byte, crash/concurrency, process
round-trip, and platform evidence.

SES-019 audit note (2026-08-31): remote client/protocol/server/backend/RPC
primitives are source-aligned for ownership, snapshots, progress, reconnect,
and disposal. Status remains unchanged pending the real multi-process socket,
concurrent update, disconnect/shutdown, and platform/network matrix.

MODE-001 audit note (2026-08-31): text/print source is aligned for prompt,
retained turns, final output, terminal errors, retry, and compaction. Status is
unchanged pending process signal/backpressure/flush, child cleanup, and complete
retry/compaction visibility evidence.

MODE-002 audit note (2026-08-31): JSON event source covers the audited lifecycle,
tool delta, usage, compaction, error, and session shapes. Status remains
unchanged pending exhaustive provider/process and consumer-boundary evidence.

MODE-003 audit note (2026-08-31): JSON/RPC writers are source-aligned for
serialized complete-line output and flushing. Status remains unchanged pending
real pipe, signal-during-flush, and cross-mode process stress.

RPC-001 audit note (2026-08-31): prompt validation, correlation, detached
settlement, and retained multi-turn context are source-aligned. Status remains
unchanged pending concurrent real-client, pipe/signal, and live-provider gates.

RPC-002 audit note (2026-08-31): steering/follow-up queue modes, ordered drains,
updates, cancellation, and correlation are source-aligned. Status remains
unchanged pending external concurrency, disconnect/EOF, and live timing.

RPC-003 audit note (2026-08-31): agent/retry/bash abort targeting,
idempotence, response ordering, and shutdown wakeup are source-aligned. Status
remains unchanged pending real race/process/flush/platform evidence.

RPC-004 audit note (2026-08-31): new/switch/fork/clone persistence, linkage,
guards, rebinding, and errors are source-aligned. Status remains unchanged
pending process-wire, race, crash-durability, and platform evidence.

RPC-005 audit note (2026-08-31): state/messages/entries/tree schemas,
pagination, errors, labels, and selection are source-aligned. Status remains
unchanged pending process/pipe ordering and concurrent mutation stress.

RPC-006 evidence note (2026-08-31): scoped model cycling now retains scope and
thinking overrides, filters availability, persists selection, reports the
correct scope state, and falls back to the full catalog. The focused gate is
3/3; status remains unchanged pending live refresh and process-wire evidence.

RPC-007 evidence note (2026-08-31): non-reasoning models now distinguish the
available `[off]` level from unsupported cycling, which returns null. The
focused gate is 3/3; status remains unchanged pending live capability catalogs
and process-wire evidence.

RPC-008 audit note (2026-08-31): queue-mode validation, live mutation,
persistence, and response/error schemas are source-aligned. Status remains
unchanged pending concurrent process timing and settings-flush integration.

RPC-009 evidence note (2026-08-31): manual compaction now aborts active
detached prompt/retry work and executes only after the existing exact-once
prompt settlement barrier has persisted terminal messages, released the run
lock, and emitted `agent_settled`. The focused gate is 3/3; status and metrics
remain unchanged pending real process timing, provider cancellation,
persistence/restart, and platform evidence.

RPC-010 evidence note (2026-08-31): standalone RPC bash now uses the selected
session cwd, configured prefix/shell, correlated chunk updates, shared
process-group-safe capture, and full-output spill metadata. Focused and adjacent
bash regressions pass and the independent gate is 3/3; status and metrics remain
unchanged pending extension custom operations, external stress, and platform
shell evidence.

RPC-011 evidence note (2026-08-31): RPC HTML export now uses a valid configured
theme, falls back from an invalid configured name, and normalizes explicit
tilde/file-URL output paths. Two focused regressions pass and the independent
gate is 3/3; status and metrics remain unchanged pending custom-tool rendering,
complete file/process matrices, browser/visual output, and platform paths.

RPC-012 evidence note (2026-08-31): malformed and unknown commands now have one
aggregate same-stream fixture proving deterministic errors, correlation, UI
diagnostics, and subsequent valid dispatch. Implementation and deterministic
evidence are PASS; runtime remains PARTIAL for real pipe/backpressure recovery
and untyped non-string IDs.

RPC-013 evidence note (2026-08-31): the source-aligned shutdown path now has a
permanent clean-process fixture for ordinary EOF, SIGTERM=143, and SIGHUP=129
after correlated dispatch. Implementation is PASS; deterministic/runtime remain
PARTIAL for broken pipe, descendant cleanup/leaks, and cross-platform behavior.

MODE-004 evidence note (2026-08-31): the experimental Unix server now routes
Ctrl-C, SIGTERM, and SIGHUP through graceful listener close. Its full real CLI
suite passes 7/7, including TERM/HUP socket removal, and the independent gate is
3/3. Status and metrics remain unchanged because the CLI client is still
list/snapshot-only and lacks prompt/steer/abort/pending-work lifecycle; platform
and live-provider boundaries remain.

EXT-001..EXT-012 audit note (2026-08-31): the full native-extension block was
re-audited without a status change. Core native contracts are implemented, but
aggregate process/reload proof remains absent and interactive mode still does
not project the broker's complete custom UI state into pi-tui. All twelve rows
therefore remain PARTIAL/PARTIAL/OPEN; metrics are unchanged.

PKG-004 evidence note (2026-08-31): npm/npx/bun source spellings now share a
case-insensitive pre-resolution Rust-only guard. Focused unit and real CLI
matrices prove install/remove/update rejection and unchanged settings; the
independent gate is 3/3. Implementation and deterministic evidence are PASS;
runtime remains PARTIAL for broader platform/process boundaries.

Package/eval/distribution audit note (2026-08-31): PKG-005, EVAL-001..002, and
DIST-001..005 were re-audited without a status change. Remaining evidence needs
live callers, artifact provenance/installers, platform launch, and a disposable
external upgrade/rollback transaction; DIST-004 therefore remains OPEN.

X-001..X-012 audit note (2026-08-31): the complete cross-cutting block was
re-audited. Aggregate process gates now move X-001..X-010 to PARTIAL across all
dimensions; X-011..X-012 remain OPEN/OPEN/OPEN. Distributed feature evidence is
extensive, but complete adversarial/platform combinations remain open.

X-011/X-012 evidence note (2026-09-05): a real-process aggregate proves
failure diagnostics carry the action and offending value with exact recovery
text plus secret absence, and permanently re-tests malformed/unknown/deep
wire input, malformed-session switch, failed-export no-write, and abort
with post-failure reuse. Both rows are PARTIAL/PARTIAL/PARTIAL; broader
crash/platform matrices remain open. The independent gate is 4/4.

ENV-001/ENV-002 evidence note (2026-09-05): deterministic resolver tests
prove CLI/env/default/empty precedence and key precedence; a real-process
fixture proves env-only selection, CLI override, empty fallthrough,
value-naming invalid diagnostics, and PI_KEY/dual-key redaction. Both rows
are PARTIAL/PARTIAL/PARTIAL; footer/request selection, per-vendor breadth,
and live/platform evidence remain open. Startup PI_PROVIDER/PI_MODEL/PI_KEY
defaults are an intentional divergence: pinned upstream only propagates
them to tool children and eval config.

ENV-007/ENV-009 evidence note (2026-09-05): `PI_REASONING_LEVEL` now
resolves CLI > scope > env > settings > builtin with CLI-shaped invalid
warnings, closing a default-fill shadowing bug and wiring per-turn
reasoning into print-path provider requests; a loopback `openai-responses`
fixture proves env/CLI/invalid levels reach the wire `"effort"`. Version
output and no-banner behavior are proven with/without the skip flag and
with `PI_VERSION` ignored. Both rows are PARTIAL/PARTIAL/PARTIAL;
per-mode caller wiring is now closed (2026-09-05): JSON resolves the same
precedence and is loopback-proven, RPC carries loop reasoning plus stream
options, interactive threads the level into every turn with harness rebuild
on change, and SDK/experimental set with-options. Footer selection,
per-vendor breadth, and live/platform evidence remain open.

ENV-011 evidence note (2026-09-05): provider resolvers already match pinned
upstream exactly (explicit option wins, per-request env then process env,
only exact `long` maps to long, silent short fallback), so no source change
was needed. pi-ai precedence unit tests prove explicit-beats-env-long plus
invalid/empty/case-variant fallback on `openai-responses`; a real-process
loopback fixture (4/4, print + JSON) proves `PI_CACHE_RETENTION=long`
reaches the wire as `"prompt_cache_retention":"24h"` while unset/invalid
sends no retention field. The row is PARTIAL/PARTIAL/PARTIAL; live-vendor
breadth, cross-provider wire breadth, and platform evidence remain open.

ENV-013 evidence note (2026-09-05): the settings→env proxy bridge already
matches pinned upstream nullish semantics (explicit env, including empty,
wins; blank settings ignored). Three new `http_dispatcher::tests` pin
empty-env preservation, settings-file bootstrap, and malformed-JSON
diagnostics; real-process loopback fixture
`tests/env_proxy.rs` (4/4, print) proves dead-proxy failure with
absolute-URI interception, `Proxy-Authorization` forwarding, and the
provider untouched, `NO_PROXY` bypass, `settings.json httpProxy`
interception, and fail-closed startup on unroutable values (including
`socks://`, unsupported by this build and upstream alike). (Correction
2026-09-06: an earlier note claimed reqwest drops env-proxy userinfo; that
was a case-sensitive test-string bug.) Follow-up 2026-09-06:
`validate_proxy_env` fails startup on values the client would silently
ignore (upstream throws lazily per request instead). The row is
PARTIAL/PARTIAL/PARTIAL; per-request env-map override (needs per-request
client construction; deferred) and live/platform evidence remain open.

ENV-012 evidence note (2026-09-05): escape-timeout resolution already matches
pinned upstream exactly with pi-tui unit pins, and hardware-cursor/shrink
already follow setting-wins-then-strict-`"1"`-env through constructors,
settings chain, and startup wiring. New settings precedence pins close the
row to PARTIAL/PARTIAL/PARTIAL; live PTY, multi-terminal, and platform
evidence remain open.

ENV-014 evidence note (2026-09-05): home resolution already implements
host-platform precedence with fallback and unit pins. New HuggingFace XDG
search pins plus real-process fixture `tests/env_home.rs` (2/2: home-derived
catalog, homeless fallback) close the row to PARTIAL/PARTIAL/PARTIAL;
Windows/macOS runs remain open.

ENV-015 evidence note (2026-09-05): the editor chain already matches upstream
(setting > `VISUAL` > `EDITOR` > platform default) with Ctrl+G wiring and
launch/input/failure coverage. New precedence pins plus a SIGINT→Cancelled
pin close the row to PARTIAL/PARTIAL/PARTIAL; live editor and platform
evidence remain open.

ENV-016 evidence note (2026-09-05): llama server resolution already matches
upstream (stored env > context env, identical normalization, process-env
login default). New normalization, precedence, and HF path pins close the
row to PARTIAL/PARTIAL/PARTIAL; live traffic, restart breadth, and platform
evidence remain open.
DIST-004 evidence note (2026-09-05): compiled installs cannot self-update,
so the disposable-installer fixture `tests/dist_upgrade.rs` (2/2) proves
binary replacement preserves sessions/settings/auth byte-identical and a
failed `pi update` touches nothing. Git extension updates mirror the
upstream fetch/reset/marker lifecycle (fetch failure leaves no marker,
reset failure marks with the old checkout intact, next success heals).
The row is PARTIAL/PARTIAL/PARTIAL; live installer provenance and platform
breadth remain open.
<!-- PARITY_DASHBOARD_METRICS:END -->

Latest 2026-08-31 operation-record verification: SES-004 implementation and
deterministic evidence are PASS. The 8-case durable JSONL suite round-trips
every record family across two lanes with gapless ordering, settled operations,
and usage totals; the 30-case conformance suite proves shared backend
invariants. Runtime remains PARTIAL for crash/platform durability and live
caller/provider propagation.
The independent SES-004 acceptance gate is reverified 4/4.

Latest 2026-08-31 session-state verification: SES-003 implementation and
deterministic evidence are PASS. The 8-case durable JSONL suite and 4-case
context suite prove ordered model/thinking/active-tool transitions, empty and
repeated changes, tool-call/tool-result messages, reopen, and latest-state
projection. Runtime remains PARTIAL for concurrent/crash/platform durability,
provider-specific tool payloads, and live interactive selector integration.
The independent SES-003 acceptance gate is reverified 4/4.

Latest 2026-08-31 session-entry verification: SES-002 implementation and
deterministic evidence are PASS. Typed append/session entries accept unknown
forward-compatible fields without weakening required IDs, types, sequence, or
parent validation. The 17-case codec suite and 15-case repository suite prove
message/custom entries, termination, nested unknown fields, replay/tree links,
and unchanged raw JSONL. Runtime remains PARTIAL for concurrent/crash/platform
durability and extension-specific rewrite semantics.
The independent SES-002 acceptance gate is reverified 4/4.

Latest 2026-08-31 trust verification: TRUST-001/002 implementation and
deterministic evidence are PASS. The untrusted bootstrap excludes project
extensions while retaining global/explicit trust callbacks, and the final
runtime loads only after callback/saved/default/TUI resolution. Two discovery
tests, four precedence/error/remember tests, and the 10-case real process/
tmux trust suite pass, including startup approve/cancel, `/trust` parent save,
cancel, and subsequent startup. Runtime remains PARTIAL for platform,
readonly/hostile storage, visual, and live external native-extension breadth.
The independently reverified TRUST gate is 5/5 and includes package check,
strict all-target clippy, stable formatting, both parity audits, and the
repository diff check.

Latest 2026-08-31 models.json runtime verification: custom API-key providers
now compose into an isolated native Rust provider registry and dispatch
through the registered API adaptor. Pi-ai model tests pass 18/18,
coding-agent registry tests pass 16/16, a real local HTTP stream passes 1/1
with synthetic configured Bearer/header auth, and a synthetic stored-OAuth
regression proves request-header decoration without replacing the delegated
OAuth lifecycle. Package check/strict all-target clippy/static gates pass.
An isolated real-process fixture passes 3/3 for overlay visibility, deletion
on restart, and malformed replacement fallback without stale retention.
The active interactive facade now recomposes on explicit Pi refresh boundaries;
a real PTY observes changed model metadata and completes a subsequent persisted
turn without restart. MODEL-002 and MODEL-003 are PASS/PASS/PASS; existing
runtime facades observe external auth/catalog file changes without restart.
MODEL-004 is PASS/PASS/PASS after real text/JSON process coverage of exact,
case, provider-scoped, thinking, glob, unavailable, and ambiguous resolution.

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

Phase 3 campaign closure does not move any dashboard dimension.

Protocol-layer checkpoint (2026-08-30): the pi-protocol parity wave landed
alongside the dashboard; all dimensions above are unchanged by that work.
