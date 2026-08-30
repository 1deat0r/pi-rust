# Full Pi → pi-rust Conversion Ledger

Session date: 2026-08-23 (operator: "going to bed — document or something")
Author: pi (Claude), planning pass grounded in a live repo audit.
Base revision: HEAD 90a5b93 (1416 tests at last clean revision).

## Current status (last updated 2026-08-30)

The `166/166` figure below is the historical source/conversion ledger, not
behavioral parity. The active acceptance campaign is the exhaustive
318-ID inventory in `docs/EXHAUSTIVE-PARITY-INVENTORY.md`; it remains open
until the real debug/release, PTY/TUI, provider, clean-environment, and
installation gates pass. Do not report this ledger percentage as the product's
functional completion percentage.

The pi-rust distribution boundary is explicit: startup does not query the
upstream Pi release service or show an `Update available: pi ...` banner.
`pi update --extensions` and `pi update --models` retain their package/catalog
scopes. `pi update` and `pi update --self` report the pi-rust source-repository
rebuild workflow and never replace the compiled Rust binary.

Latest serialized checkpoint: the JSON/session wave is parent-verified. JSON
mode emits the official v3 session header and durable v3 records while native
pi-agent v4 storage remains compatible; the streamed event sink writes
incrementally, supplies the initial `toolCall` placeholder, normalizes
`toolUse` stop reasons, and emits `agent_settled`. A real optimized-release
Qwen tool turn matched the official Pi envelope on the checked path. The full
workspace all-target matrix passes with 247 pi-agent, 433 pi-ai, 814
pi-coding-agent, and 381 pi-tui library tests plus integration targets; strict
workspace clippy and the optimized release build pass. The latest serialized
package rerun additionally passes 435 pi-ai, 816 pi-coding-agent, and 382
pi-tui library tests with all package integration targets and strict package
check/clippy. These gates strengthen evidence only; the tracked row metrics
below remain intentionally conservative.

- The Rust-native checker reports **100.00% (166/166; 0 open)**. Run
  `/home/mustbearnold/.cargo/bin/cargo run -p pi-coding-agent --offline --bin
  conversion_audit -- all` after any ledger or source-audit change; the same
  value is copied into `PLAN.md` and `HANDOFF.md`.

The source-ledger result remains separate from the behavioral TUI contract. The
current root recheck passes R1–R8; the TUI register remains
`19.23% / 19.23% / 0.00% / 0.00%` for functional, evidence,
visual/interaction, and overall parity. A Kitty CSI-u release-filter
regression now prevents one submenu Up/Down press from being applied twice, and
the direct `!!` Bash-completion race has a ten-run optimized-release
regression. The release auth PTY now also proves bracketed API-key paste,
masking, persistence, and logout for `qwen-token-plan`. The complete workspace
release suite is green; R8 now has exact official-Pi versus Rust release
captures at 100x30 and 80x24, while the per-capability visual register remains
open until each row receives its own complete review.

The latest CLI source leaf also routes normal print-mode final text through the
shared guarded stdout writer and is parent-verified by the pi 5/5 unit,
experimental 4/4, and real CLI 6/6 suites. The subsequent CLI-044/047 leaf
adds real print/JSON signal probes, interactive signal PTYs, broken-pipe
help/version probes, RPC child-failure evidence, and strict-policy checks;
vendor/platform, complete child-lifecycle, and visual boundaries remain open.
The follow-up CLI-005..011 leaf adds release BOM/Unicode `@file` and missing
file process evidence plus signal-aware print/JSON cancellation; live provider,
Windows, and exhaustive input/file boundaries remain open.

The Qwen Token Plan source wave is now parent-verified by the 7/7 provider
matrix, pi-ai check/clippy, JSON, formatting, and diff gates; the three Qwen
rows have implementation/evidence PARTIAL credit and runtime OPEN. The same
wave passed the pi-tui 362-library/all-integration gate and the session
restart/concurrency/run/interactive caller suites. The follow-up TUI source
gate is also green; the project-trust audit confirmed that `/trust` still
needs the upstream project-scoped modal integration, and no new row promotion
is claimed for the current provider turn. The next source wave covers the
remaining TUI rendering/tool/animation surfaces, Xiaomi/token-plan provider
rows, and trust integration.

The latest 2026-08-29 wave is parent-verified: pi-tui passed 382 library
tests plus every integration target and strict clippy; the post-fix Anthropic
provider matrix passed 9/9 and the full pi-ai matrix passed 435 library tests
plus every integration target; the Xiaomi/Token Plan
and Z.AI provider fixtures passed 2/2 and 3/3 with pi-ai check/clippy and
JSON/static gates; and the trust/session caller gates passed project-trust
13/13, `cli_trust` 9/9, session restart 6/6, interactive full matrix 7/7,
real PTY 10/10 plus one intentional live ignore, slash completion 5/5, and
run-unit 33/33 with coding-agent check/clippy/static gates. PROV-034..039 and
TRUST-001/002 now carry conservative PARTIAL credit where applicable; no row
reached PASS. Live vendor, full trust lifecycle, complete session path/error/
restart, and visual boundaries remain open. The next source wave is
dispatched separately below.

The latest residual source wave is parent-verified by changelog 6/6 and skills
12/12 focused tests plus coding-agent all-target check/clippy. Digit-starting
colon links now follow upstream URL-scheme classification, and non-file
`SKILL.md` markers no longer suppress valid skill discovery; no overall row is
promoted without its complete runtime boundary.

The native llama.cpp/local-provider slice is also parent-verified: its
loopback catalog/auth/stream/load-unload/download-progress/cancellation/
timeout/failure fixture passes 11/11 with coding-agent all-target clippy.
PROV-040 now carries implementation and deterministic-evidence PARTIAL credit;
external-server and platform/restart boundaries remain open.

The environment/config checkpoint also passed `config::tests` 18/18,
covering exact upstream `env_flag` truthiness and empty agent/session-root
fallback. ENV-004, ENV-005, and ENV-006 now have implementation/evidence
PARTIAL credit; clean-process and runtime precedence remain open.

The OpenCode/OpenCode-Go/OpenRouter source wave is parent-verified: provider
units 31/31, provider matrix 7/7, pi-ai all-targets 419 library tests plus
every integration target, downstream coding-agent check/clippy, strict/static
gates pass, and PROV-025..027 now have implementation/evidence PARTIAL credit.
Live vendor and complete stream/error/retry boundaries remain open.

The subsequent xAI source/runtime checkpoint is also parent-verified: xAI
provider tests pass (33/33), the xAI OAuth tests pass within the 8-case
auth-flow suite, the provider matrix passes 7/7, and the full pi-ai all-targets
gate passes 425 library tests plus every integration target with strict clippy
and downstream coding-agent check/clippy. PROV-033 now has conservative
implementation/evidence PARTIAL credit; live xAI traffic, device
authorization, and complete external stream/error/retry boundaries remain
open.

The latest serialized follow-up is parent-verified: pi-tui passed 370 library
tests plus every integration target and strict clippy after the selector,
overlay, and key-release test repairs; D1's session-environment change passed
4 focused tests, coding-agent check, and strict clippy. These gates add
evidence but do not promote a row without its complete contract.

The latest provider recheck is parent-verified: Together and Vercel AI Gateway
pass the complete pi-ai all-target suite (427 library tests plus every
integration target), strict clippy, JSON/static validation, and downstream
source gates. PROV-031/032 are PARTIAL for implementation and deterministic
evidence; live vendor traffic and complete stream/error/retry/abort boundaries
remain OPEN. Current non-TUI counts are implementation 49 PASS/194
PARTIAL/23 OPEN and deterministic evidence 36 PASS/207 PARTIAL/23 OPEN.

The latest verified source wave added terminal/image/scrollbar protocol coverage
in pi-tui, provider-independent SSE/event-stream/abort coverage across seven
AI adaptors, and the upstream HOME/USERPROFILE environment fix. The next
disjoint source wave is active: B1 is taking a pi-tui-only parity slice, B2 is
taking a non-TUI adapter/runtime slice outside JSON and session-v3 paths, and
D1 is taking a separate non-TUI acceptance slice. Cargo verification remains
serialized behind each source checkpoint.

The synchronized progress dashboard is:

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

The latest residual verification also passed the provider, agent-runtime,
transcript/TUI, settings, component, and presentation package gates under
`.unlazy/parity-20260827/`, followed by a serialized workspace test, strict
clippy, and release-build recheck. This strengthens the evidence for those
slices without changing the separate row-based percentages above; the live
footer owner seam and terminal visual register remain open.

The 2026-08-28 SI4 parent recheck additionally passed settings-panel 9/9,
real settings PTY 2/2, core-settings 27/27, interactive-mode 50/50, the
parity-audit validator 8/8, the full workspace test matrix, strict workspace
Clippy, formatting/diff checks, and the optimized release build. The
row-by-row settings contract remains open until every 29/31 capability-gated
setting has explicit persistence/live/cancel/restart evidence.

This block is synchronized by the Rust-native `parity_audit dashboard`
command and the pre-commit hook; see `docs/PARITY-DASHBOARD.md` for metric
definitions.
- The final scope is explicitly Rust-only: no JavaScript/TypeScript source,
  Node/Bun runtime, npm dependency execution, or source-language extension
  loading is shipped. Compiled Rust factories provide the extension command,
  hook, renderer, tool, and provider surfaces; filesystem JS/TS extension
  paths are rejected or ignored deterministically. HTML export is rendered by
  Rust as a static document. This is the accepted distribution boundary for
  the user's 100%-Rust requirement.
- S-008 constrained JSON-schema and OpenAI grammar custom-tool parity is
  complete with unit/mock evidence in the pi-ai adaptor suite. The last
  committed workspace/release gate passed its strict clippy, build/test,
  formatting, diff, and Cargo-native audit checks. The current extension
  checkpoint's scoped clippy gate is green; the only unisolated package-clippy
  residual is the unrelated untracked changelog regex recorded below.
- S-009 Codex WebSocket session caching/reuse is complete with mock/unit
  evidence. The implementation covers session/account cache keying, cached
  context deltas, idle/max-age eviction, missing-continuation recovery, and
  explicit WebSocket/SSE transport behavior; see the ledger row below for the
  exact fixtures and commands.
- S-010 Bedrock credential/profile and region-resolution parity is complete with
  unit/mock evidence. The implementation covers explicit/scoped/ambient
  profile precedence, shared credentials and `AWS_CONFIG_FILE` profile regions,
  ARN/env/option endpoint-region precedence, ECS task-role credentials, web
  identity STS credentials, bearer/skip-auth behavior, and exact provider auth
  source labels. SSO- or process-backed profiles and EC2 metadata remain
  outside the hand-rolled signer scope.
- The post-publication whole-result S-010 acceptance rerun covered the public
  Bedrock stream/request boundary, provider-auth boundary, all pi-ai library
  and integration targets, compile/lint/format/diff gates, and offline Cargo
  metadata. Those checks passed. `cargo package -p pi-ai --offline
  --allow-dirty --no-verify` remains blocked by repository-level packaging
  metadata: the path dependency `pi-telemetry` has no crates.io version
  requirement, and it is not available in the offline index. No ledger checkbox
  changed during this rerun; packaging remains P9 work outside S-010 behavior.
- The original 100 entries remain the historical work queue. The supplemental
  S1 section is authoritative for residual provider, harness, runtime, TUI,
  RPC, auxiliary client/server, evaluation, and final-audit work.

## Current exhaustive launch and live-provider checkpoint — 2026-08-26

No numbered source-ledger row changed. The active acceptance inventory now
contains 318 IDs, verified by the Rust-native `parity_audit inventory` command.
The optimized Rust binary is installed as both `pi` and `pi-rust`; both PATH
commands resolve to `target/release/pi` and report `pi 0.84.2`. The full debug
and release workspace suites and strict workspace clippy pass.

The real interactive release binary has also been exercised with the currently
stored OpenAI Codex OAuth credential: two sequential interactive PTY turns
returned the requested exact responses, and the real `/login` flow displayed
the authentication selector, generated the `auth.openai.com` browser OAuth
URL, and cancelled cleanly without replacing the credential. The release
`interactive_auth_pty` suite additionally covers browser callback exchange,
device-code exchange, persisted credentials, logout, cancellation, and a real
loopback llama.cpp API-key validation path. These are current `live`/`local`/
`release` evidence entries; they do not close every one of the 318 IDs.

The final release package revalidation also fixed two real acceptance defects:
the experimental Unix-server PTY/process test now isolates its empty session
root from the operator's existing sessions, and the slash PTY expectation for
an unknown `/login` provider now matches the truthful generic diagnostic. The
focused test and complete release workspace matrix pass after both fixes.

The safe operator commands are documented in `README.md`. The legacy
`node scripts/conversion-progress.mjs` path referenced by older notes is absent
from this Rust-only checkout; the authoritative current source-ledger command
is:

```text
Conversion progress: 100.00% (166/166; 0 open)
```

from `cargo run -p pi-coding-agent --offline --bin conversion_audit -- all`.

## Current tool-display and animation checkpoint — 2026-08-26

The behavioral inventory was expanded to 318 IDs with eight explicit TUI
acceptance rows (`TUI-045`–`TUI-052`) for animation timing, live tool display,
and process coexistence. The Rust interactive loop now consumes the real rich
agent tool lifecycle and renders compact Pi-style call/result blocks; normal
TUI output does not serialize the model-facing JSON envelope. Focused tests,
the release PTY with a real OpenAI Codex tool turn, terminal progress
keepalive checks, and concurrent pi-rust process tests pass.

This is behavioral evidence, not a claim that the 166-item source ledger has
changed or that the whole product is complete. The official Pi source checkout
available beside this repository has no installed dependencies or built
artifacts, and the PATH `pi` command is this Rust release binary. Therefore
official-JS-Pi coexistence and final manual visual side-by-side review remain
unverified until a runnable official binary is supplied.

## Current extension contract checkpoint — 2026-08-26

The EXT-009–011 extension contract slice is implemented in the Rust-native
extension boundary. No numbered conversion checkbox changed: this is
behavioral evidence for the exhaustive inventory IDs.

- EXT-009 exposes the live `ExtensionContext` host handle for session/model
  capabilities, trust/idle/queue/signal state, abort/shutdown, messaging and
  entries, session metadata/labels, tool and command catalogs, model/thinking
  changes, context usage, compaction, system-prompt access, and tool updates.
  Queued lifecycle/model changes retain typed pending outcomes; stale contexts
  reject calls after invalidation.
- EXT-010 uses a correlated native UI broker for select/confirm/input/editor,
  cancellation, bounded waits, unknown/late/malformed diagnostics, and all
  fire-and-forget UI actions. Terminal listeners, custom overlays, widgets,
  header/footer, hidden-thinking labels, autocomplete/editor factories, theme
  state, editor state, and tool expansion are stateful broker surfaces. RPC
  forwards `extension_ui_request` records and routes `extension_ui_response`
  records back to the waiting callback; `extension_ui_input` dispatches real
  listener transformations and consume results.
- EXT-011 retains every upstream tool-definition field in an open Rust form:
  label, prompt metadata, constrained sampling (`false` or JSON), render-shell
  policy, argument preparation, execution mode, execute, render-call, and
  render-result callbacks. Preparation, live updates, metadata publication,
  and renderer callback invocation are covered by permanent tests.

Focused direct-stable offline evidence is green:

```text
cargo test -p pi-coding-agent --offline --lib core::extensions -- --nocapture --test-threads=1
  58 passed; 0 failed
cargo test -p pi-coding-agent --offline --lib core::extensions::integration::tests::default_mode_loader_seeds_parsed_extension_flags_before_lifecycle -- --nocapture --test-threads=1
  1 passed; 0 failed (included in the 58-test suite)
cargo test -p pi-coding-agent --test extensions_parity --offline -- --nocapture --test-threads=1
  9 passed; 0 failed
cargo test -p pi-coding-agent --offline --lib 'modes::rpc::tests::rpc_' -- --nocapture --test-threads=1
  16 passed; 0 failed
cargo check -p pi-coding-agent --tests --offline
  Finished successfully
cargo clippy -p pi-coding-agent --lib --offline -- -D warnings -A clippy::invalid_regex -A unused-imports -A unused-mut
  Finished successfully
```

The strict package clippy command without explicit isolation remains blocked by
unrelated dirty-worktree diagnostics in `crates/pi-coding-agent/src/core/changelog.rs`
(look-ahead regex) and `crates/pi-coding-agent/src/interactive/clipboard.rs`
(unused import/mutability); no extension-scope file is involved. The
renderer-neutral factory contract intentionally stops at JSON/native callback
materialization because `interactive.rs` and `pi-tui` are outside this slice;
the concrete TUI component adapter and raw interactive PTY hook remain that
host-owned boundary. The default-loader flag helper and its last-value-wins,
pre-lifecycle regression pass in the 58-test extension suite. A subsequent
package revalidation is currently blocked by the excluded, actively changing
`pi-tui` lane: `scroll_view.rs` lacks `ScrollbarMode: Default` and calls the
missing `scrollbar_visible_locked`, while its geometry value is a tuple but is
read through `thumb_top`/`thumb_height` fields. No extension-scope file is
implicated.

Scope boundary for duplicate validation lifecycle: `main.rs::validate_extension_flags`
still uses a temporary mode loader to discover flag definitions before the real
mode starts. Removing that extra validation-time lifecycle requires a
definition-only loader call in `main.rs`, which is outside this checkpoint's
allowed files.

## Exhaustive usability-test checkpoint — 2026-08-26

No conversion checklist row changed during this checkpoint. The status remains
**100.00% (166/166; 0 open)**, and the authoritative Cargo-native audit was
rerun after the documentation review:
`cargo run -p pi-coding-agent --offline --bin conversion_audit -- all`
reported `audit blockers: 0` and `workspace JS/TS source files: 0`.

The Rust binary's deterministic user-facing behavior is now covered by the
new CLI/mode/resource/trust matrices and permanent optimized-binary TUI/RPC
multi-turn tests recorded in `PLAN.md` and `HANDOFF.md`. The evidence tiers
remain explicit: offline faux/provider-error and isolated PTY tests are
unit/mock/live-local evidence; credentialed live-provider inference and the
installed PATH command are not claimed. The legacy Node progress script is
absent from this Rust-only checkout, so the Cargo-native audit is the current
progress authority.

The operator's pause was cleared on resume. The combined mode oracle passed,
the isolated release reverify passed after one concurrent-orchestration
demotion, and all 19 unlazy gates are met. The working-tree test and
documentation changes are committed and pushed as
`2a9284b76957d2b4bb3a259511fe8817e864fe13`, with local and remote hashes
matching. The pre-existing untracked `AGENTS.md` remains untouched.

## Bounded RPC/protocol command parity checkpoint — 2026-08-26

No numbered conversion-ledger checkbox changed in this bounded slice; the
status remains **100.00% (166/166; 0 open)**. This checkpoint closes an
implementation gap under the existing RPC evidence rows by comparing the Rust
dispatcher and resource catalog with the pinned upstream RPC mode, RPC types,
JSONL, prompt-semantics, and unknown-command regression fixtures.

`crates/pi-coding-agent/src/modes/rpc.rs` now discovers extension commands,
prompt templates (including settings-configured paths), and skills with
upstream-compatible `sourceInfo`; dispatches extension prompts immediately;
expands skill/template prompts; preserves image blocks; queues prompt turns
according to `streamingBehavior`; rejects queued extension commands; and
consumes `extension_ui_response` envelopes without producing a spurious
unknown-command response. `modes/jsonl.rs` now has explicit U+2028/U+2029 and
LF-only framing regressions. The binary RPC fixture covers project prompt/skill
discovery, a real multi-turn session, and the UI-response envelope. The parity
fixture's follow-up expectation now matches upstream success semantics.

Exact focused evidence, all run offline with the direct stable toolchain:

```text
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTDOC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustdoc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-coding-agent --offline --lib modes::rpc::tests -- --test-threads=1
  48 passed; 0 failed
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTDOC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustdoc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-coding-agent --offline --lib modes::jsonl::tests -- --test-threads=1
  7 passed; 0 failed
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTDOC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustdoc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-coding-agent --offline --lib modes::json_event::tests -- --test-threads=1
  1 passed; 0 failed
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTDOC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustdoc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-coding-agent --offline --lib modes::rpc_types::tests -- --test-threads=1
  4 passed; 0 failed
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTDOC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustdoc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-coding-agent --offline --test rpc_binary_multiturn -- --test-threads=1
  2 passed; 0 failed
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTDOC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustdoc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-protocol --offline -- --test-threads=1
  46 executable tests passed; doctests: 0 passed
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTDOC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustdoc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo run -p pi-coding-agent --offline --bin conversion_audit -- all
  Conversion progress: 100.00% (166/166; 0 open)
  audit blockers: 0
  workspace JS/TS source files: 0
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTDOC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustdoc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo fmt --all -- --check
git diff --check
```

The required legacy `node scripts/conversion-progress.mjs` command was also
run and exits 1 with `MODULE_NOT_FOUND` because that script is absent from the
Rust-only checkout; the Cargo-native audit above is the authoritative result.
Focused coding-agent builds required a temporary, one-line reversible repair
to the unrelated pre-existing untracked `crates/pi-ai/src/providers/radius.rs`
borrow error; it was restored exactly and was not staged. With that unrelated
file restored, a normal coding-agent rebuild remains blocked by its existing
`E0515` error. No `interactive.rs` or `pi-tui` file was changed in this slice.

The former RPC no-broker condition was closed by the current extension
contract checkpoint above: the Rust host now originates correlated requests,
resolves real responses, and diagnoses unknown/late/malformed records. No
`interactive.rs` or `pi-tui` file was changed in this slice. Credentialed
live-provider inference was not used for this protocol-only checkpoint.

This scoped implementation and documentation checkpoint is committed and
pushed as `952256c5c230daf8f204f41d7ffb8d7b20c38696`; local `HEAD` and
`origin/main` were verified equal. The pre-existing untracked and unrelated
working-tree changes remain preserved and outside this commit.

## Bounded pi-agent lifecycle parity checkpoint — 2026-08-26

No numbered conversion-ledger row changed in this bounded slice. It refines
and re-verifies the existing S-018/S-019/S-038 lifecycle and queue contracts
against the upstream oracle at
`../pi-rust-s1-audit.KMw0N2/upstream_pi/packages/agent/src/agent.ts`, with
the corresponding tests under `packages/agent/test/`.

`crates/pi-agent/src/rich_agent.rs` now owns an active run lease and shared
abort signal, rejects concurrent prompt/continue operations with upstream
errors, exposes `signal()` and race-safe `wait_for_idle()`, keeps streaming
state active through awaited listener settlement, and uses shared live queue
closures so steering/follow-up messages enqueued during a run reach the
correct turn boundary. Real delayed push-stream transports cover abort before
a five-second response, panic-safe lease cleanup, async-listener idle
settlement, active `continue()` rejection and validation, assistant-tail queue
draining, one-at-a-time steering/follow-up turns, and `QueueMode::All`.

Exact unit/package evidence for this checkpoint:

```text
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-agent --offline --lib rich_agent::tests -- --test-threads=1
  21 passed; 0 failed
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-agent --offline --lib --quiet -- --test-threads=1
  195 passed; 0 failed
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-agent --offline --tests --quiet -- --test-threads=1
  294 passed; 0 failed across the library and seven integration targets
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo check -p pi-agent --offline --all-targets
  Finished successfully
RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo clippy -p pi-agent --offline --all-targets -- -D warnings
  Finished successfully
rustfmt --edition 2021 --check crates/pi-agent/src/rich_agent.rs crates/pi-agent/src/harness/agent_harness.rs
git diff --check
```

The required `node scripts/conversion-progress.mjs` check remains unavailable
because that legacy script is absent (`MODULE_NOT_FOUND`); the existing
Cargo-native ledger status remains **100.00% (166/166; 0 open)**. No
`pi-ai`, `pi-tui`, or `pi-coding-agent` source was changed for this slice.
Remaining limitation: `Agent::subscribe` callbacks are still replayed after
the low-level loop rather than delivered to subscribers at each event while
the stream is running; they are awaited before `is_streaming()` clears.

## Historical state snapshot (verified 2026-08-24; superseded by the active
2026-08-25 checkpoint below)

- The S-009 Codex WebSocket session-cache checkpoint is committed as
  `c3d6109f32abb2f1a4efbda6eb2c90a35383dd98` on `main` and pushed to
  `origin/main`. The only remaining worktree item is the pre-existing
  untracked `AGENTS.md`, which is preserved and unstaged.
- The S-010 Bedrock credential/profile and region-resolution checkpoint is
  committed as `9a8eaee9b8273e7b938075a38ed9659baff02359` on `main` and pushed
  to `origin/main`. Local and remote hashes matched after the push. The only
  remaining worktree item is the pre-existing untracked `AGENTS.md`, which is
  preserved and unstaged.
- The follow-up public-interface acceptance checkpoint is committed as
  `feadf6415f663662ff0948b2e29507655fc359bd` on `main` and pushed to
  `origin/main`, with local and remote hashes matching. It adds exported
  `stream`/`stream_simple` ECS and web-identity integration fixtures.
- The latest committed S-008 constrained-sampling/grammar checkpoint is
  `7a72f2fe104cf660f946f29a822c88da556a37d1` on `main`, pushed to
  `origin/main` with matching local/remote hashes. It follows the S-007 image
  retry/cancellation checkpoint `2b92195`, the deferred-response
  runtime/lazy-capability checkpoint `56ea6f3`, containing S-005/S-006 parity
  wiring after the interactive slash-command PTY slice `3b4d350`, containing the interactive
  manual-compaction checkpoint `514cca9`, the ConfigSelector implementation checkpoint
  `974bd1b` and the partial S-021/S-022 secondary-lane checkpoint `d8b589f`,
  after the S-026 CLI session-routing checkpoint `711a25e`,
  the startup-timing compatibility and compiled-binary self-update contracts,
  the print-path harness ownership, AgentTool harness/termination,
  schema-validator, panic-safe telemetry, install telemetry, update/version,
  and model-catalog work. The telemetry strict-verification cleanup is
  implemented in `45e6d64`; its metadata/documentation sync is `788f9c5`.
  GitHub authentication is configured through `gh auth setup-git`, and
  `origin/main` matched local `HEAD` after each push. This documentation
  refresh follows those synchronized checkpoints.
- The focused tool-contract, RPC, image/read, print-mode compaction,
  malformed-call, harness-owned print-path, and secondary-lane suites pass.
  The last full workspace gate was green before the resource-constrained
  S-032 retry; the current checkpoint does not claim a fresh workspace-wide
  result. The one-shot path now owns a
  stateful `AgentHarness` transcript and replays it into durable JSONL while
  retaining compaction behavior. Its configured print run now emits ordered
  lifecycle events and a settled `pi.harness.run` span with required
  attributes. JSON mode now also owns a stateful in-memory `AgentHarness`
  transcript and replays rich stream updates, including terminal provider
  errors, without changing the established successful RPC wire envelope.
  Configured harnesses cover print, JSON, and interactive turns, while the
  shared lifecycle adapter covers the remaining RPC loop paths; complete
  mode-specific golden envelopes and persistence/secondary-lane assertions
  remain open under S-021/S-022. Telemetry callback panics now
  settle in-memory spans as automatic errors while preserving explicit
  statuses and panic propagation; the shared TUI image-capability fixtures
  are serialized for deterministic workspace runs.
- Documented remaining gaps (PLAN.md carry-forward + per-crate TODOs): OAuth
  device-code flows, `/share` GitHub-gist OAuth (in-progress in the working
  tree), models.json
  runtime merge seam, full interactive
  slash-command PTY coverage, TUI alt-screen full swap + terminal feature
  probes, server/client
  concurrency surfaces (leases, reconnect, queuing). Signed usage adjustment
  parity is closed in Session 16.
- The alt-screen hardening now invalidates differential frames after
  alternate-screen transitions; full regular/fullscreen swapping and the
  dedicated tmux probe remain open in #61/#62.
- **Historical audit claims superseded by later implementation**: the earlier
  missing-flag, missing-auto-compaction, unregistered-image-tool, and absent-
  core-module bullets were contradicted by later source and fixture evidence.
  The authoritative residuals are the supplemental S-013–S-066 rows and the
  source-to-source audit in `.unlazy/full-conversion-20260825/audit/`.
- pi-agent: the core AgentTool shape, prepareArguments seam, optional-details
      payloads, mutable before/after hooks, parallel completion ordering,
      built-in update sink, mutation queue, and malformed-call matrix are now
      covered; broader schema-validator parity remains S-024.
  - pi-client: reconnect state/listeners, handshake/request timeout bounds,
    late-response suppression, and permanent dispose now land with fake-socket
    coverage; lease/reconcile parity remains partial, as do transport-factory
    and full conformance gaps.
  - pi-server: no LiveSessionManager exclusivity, session lock/terminal-close
    semantics, command queuing, subscription segment control,
    testing/service.ts parity harness.

The follow-on telemetry verification cleanup releases the in-memory span mutex
before async callback admission and restores strict clippy for `pi-telemetry`;
its focused tests and crate gate pass. Full `pi-ai` strict clippy now passes
with zero diagnostics after the adapter and structural cleanup; this cleanup
does not change the ledger count.

The adapter cleanup checkpoint is `8aba4db`, followed by the full strict-clippy
restoration `7b3db53` on `main`; local and remote hashes were verified equal
immediately after each push.

## Standing process rules (binding)

- Commit + push after every commit (session-4 directive; applies to every git
  repo worked on from this agent).
- Evidence tiers on every criterion: `unit` | `mock` | `live` — a claim needs
  the tier + exact command that produced it.
- Line-by-line expert assessment per phase; stale plan = process failure
  (PLAN.md §0).
- Independent reviewer sign-off gate before the next MAJOR phase (PLAN.md §0.3).
- Parity oracles: golden tests generated from upstream 5cd93f6, never memory.
- Keep 0 lib warnings + clippy-clean for new files per established bar.

## Tracks

- T0  Land the in-flight working tree (it is red — fix first)
- T1  Close the 9 documented divergences (audit → port → gate)
- T2  Tool contract + validation
- T3  coding-agent run-path parity (unported core surfaces)
- T4  Server/client completion (P6 concurrency)
- T5  TUI completion
- T6  Remaining coding-agent core modules (audit → port what's absent)
- T7  Data model, session tree, export, RPC edge parity
- T8  Evals, packaging, parity suite
- T9  Final 100% verification pass

## Recut (2026-08-23) — priority + goal framing (by operator request)

Goal: **behavioral 1:1 parity of the `pi` CLI product** (what a user of `pi`
experiences), not crate-for-crate structural copying.

Decisive evidence from a live audit:
- The real binary (`pi-coding-agent`, `bin = "pi"`) does **not** depend on
  `pi-server`/`pi-client`; `run.rs:231` runs `pi_agent::agent::run_agent_loop`
  **in-process**. This is a deliberate, behaviorally-faithful divergence.
- Upstream only uses `@earendil-works/pi-client` from `extensions/llama/*` and
  `client/remote-session.ts` — the Llama extension + remote-session driver,
  **not** the core CLI loop. So pi-rust's in-process loop matches the real CLI.
- Therefore `pi-server`/`pi-client` is an **auxiliary subsystem** (no shipped
  binary links it; only pi-server's own dev-deps test it). `T4` hardens it.

Recut of the remaining work by user impact + risk:
1. **T6 gap-audit first** (highest value): convert to *verify-then-port* — many
   listed modules already exist in `pi-agent`/`pi-coding-agent` (skills, system-
   prompt, prompt-templates, project-trust, settings, rpc). Real deliverable is
   **wiring** them into the run path, not re-porting the loaders.
2. **T5 TUI polish** second: config selector, alt-screen full swap, word-nav,
   feature probes, footer totals, editor IME, slash-command E2E.
3. **T7 product surfaces** third: `pi update`, session tree, export_html.
   RPC-edge audits after the visible surfaces.
4. **T4 #52-58 reclassified: OPTIONAL/DEFERRED auxiliary** — a separate
   "server/client as a library" milestone, clearly off the CLI path. Only
   restart if the goal becomes structural parity (i.e. wire `pi-coding-agent` →
   `pi-client`), which is a distinct prerequisite the plan never listed.
5. **T8/T9 stay last**: ledger, parity-suite, release-build, final 100% audit
   (many of #100's criteria are already true today).

---

## T0 — Land the in-flight working tree (fix first — it's red)

- [x] 1. Fix `crates/pi-coding-agent/tests/cli_export.rs:13-15` — repair the
      `split("data-session="")` string to `"data-session=\""`, making the
      `'"'` char literal parse. (unit)
- [x] 2. Verify `args.rs` `--export` help text compiles/prints one flag per
      line (the diff shows a wrapped literal — make rustfmt clean). (unit)
- [x] 3. Audit `main.rs` `--export` wiring vs upstream `exportFromFile`
      (output path fallback, exit codes, "Exported to:" print). (mock)
- [x] 4. Finish `/share` — remove `SHARE INVOKED (probe)` banner and `dry_run`
      default; implement upstream share.ts (gh auth status → export HTML →
      `gh gist create --public=false` → viewer `PI_SHARE_VIEWER_URL#<id>`).
- [x] 5. Wire `PI_SHARE_DRY_RUN=1` as test switch only; mock-gh integration
      test. (mock)
- [x] 6. Verify persist-before-share ordering (transcript matches exported
      HTML; no lost/duplicated messages). (mock)
- [x] 7. Land editor autocomplete fix (close popup after apply so Enter
      submits) with the two new `editor_tests` cases; full pi-tui suite green.
- [x] 8. Remove duplicated `/// Wrap a modal...` doc comment in interactive.rs.
- [x] 9. `cargo test --workspace` + clippy on touched crates; restore all-green
      0-warning baseline. (unit)
- [x] 10. Commit + push: `feat(interactive): /share gist flow + --export CLI
      wiring`.

## T1 — Close the 9 documented divergences (audit → port → gate)

- [x] 11. Divergence-close audit: for each remaining gap, pin upstream file +
      current Rust behavior + test plan. (unit)
- [x] 12. Port upstream `ai/src/oauth.ts` + `auth/oauth/device-code.ts` state
      machine (device auth endpoint, poll interval, expiry, cancellation).
- [x] 13. GitHub device-code flow.
- [x] 14. (non-gap: upstream v0.84.2 Google provider is API-key only; no OAuth flow exists to port) Google OAuth2 device + refresh flow.
- [x] 15. Anthropic OAuth flow.
- [x] 16. Wire OAuth into `auth_storage.rs`, `commands/auth.rs`, `/login`.
- [x] 17. OAuth tests (mock device endpoints, expiry, cancel). (mock)
- [x] 18. Codex WS audit: upstream websocket transport paths vs current
      SSE-only `openai_codex_responses.rs`.
- [x] 19. Port WebSocket client transport (`pi-ai/src/transports/ws.rs`).
- [x] 20. Route codex to WS when settings `transport: websocket`; SSE fallback.
- [x] 21. WS tests (fixture-driven + local ws echo server). (unit/mock)
- [x] 22. Reviewer gate: multi-session diff review of T0+T1 (and T3
      completion) — independent reviewer session signed off. Verdict:
      **APPROVE WITH CONDITIONS**, all three conditions (C1 json-mode terminal
      error exit-0 parity, C2 `--tui-mode` diagnostics + token consumption,
      C3 main.rs diagnostic-print-then-exit ordering) resolved in commit
      b42050c. Non-blocking notes: N1 attribution not yet wired end-to-end
      (real providers land in T6); N3 json events buffered (matches codebase
      RPC pattern); N4 json_event.rs duplicates run.rs setup (consolidation
      follow-up). **N2 (`-v` verbose-vs-version) resolved** — `-v` now maps to
      version per upstream args.ts (was verbose); `--verbose` remains the long
      form.

## T2 — Tool contract + validation

- [x] 23. Audit upstream AgentTool shape vs current `pi-agent/src/tools/`
      trait; per-tool deltas. (unit)
- [x] 24. Upgrade tool trait to upstream shape.
- [x] 25. `prepareArguments` for bash/read/write/edit/edit-diff/ls/find/grep/
      image. (unit) Audited the pinned upstream constructors: `edit` is the
      only built-in with a non-identity prepare shim; Rust now registers the
      upstream normalization for JSON-string, single-object, and legacy
      top-level edit arguments. The remaining built-ins intentionally have no
      prepare shim in the oracle.
- [x] 26. `execute` upstream signature + `onUpdate` → rich loop emits
      `tool_execution_update`. (unit) The callback is now passed through a
      channel-backed sequential/parallel execution sink; bash emits throttled
      partial output plus a final snapshot. Evidence:
      `cargo test -p pi-agent --offline
      rich_loop_executes_tool_batch_and_emits_execution_events` and
      `cargo test -p pi-agent --offline --test tools
      bash_tool_streams_partial_updates_through_agent_contract`.
- [x] 27. Terminate-hint plumbing in `rich_agent.rs`. (unit) Batch termination
      now requires every finalized parallel result to opt in, with evidence in
      `terminate_hints_require_every_parallel_tool_to_opt_in`.
- [x] 28. Migrate every tool constructor + run.rs call sites.
- [x] 29. Port `validateToolArguments` (tool-args JSON-schema validation).
- [x] 30. Wire validation into `prepare_tool_call` with upstream errors.
- [x] 31. Tool-args validation tests (schema errors, unknown keys,
      partial-JSON args).
- [x] 32. Image/read processing parity and model-facing image behavior in
      `run.rs`. (unit) The pinned upstream source has no separate
      `AgentTool` named `image`: `harness/tools/image.ts` is the shared MIME
      detector/base64 helper used by `read`. Rust now ports the detector,
      BMP→PNG normalization, 2000x2000/4.5MB resize policy, JPEG fallback,
      conversion/dimension hints, `@file` image attachments, and the
      provider-boundary `blockImages` filter. Verified with
      `cargo test -p pi-agent --offline tools::image` (6 passed),
      `cargo test -p pi-coding-agent --offline
      run::tests::file_arguments_attach_images_and_tag_text_references`, and
      `cargo check --workspace --offline`. The existing `/images` setting
      remains a terminal-display toggle; `images.blockImages` controls model
      delivery as upstream.

## T3 — coding-agent run-path parity

- [x] 33. Wire auto-compaction into run path (settings threshold → compact →
      continue; upstream `core/compaction/` loop). (mock) The one-shot path
      now provisions its session entries while running, evaluates the model
      context against `compaction.enabled`, `reserveTokens`, and
      `keepRecentTokens`, invokes the existing harness compactor, rebuilds the
      provider context from the compaction entry plus retained tail, and keeps
      processing later print turns. Verified with
      `cargo test -p pi-coding-agent --offline run::tests` and the binary
      compaction test below.
- [x] 34. Binary-level auto-compaction test (JSONL gains compaction entry).
      (mock) `cargo test -p pi-coding-agent --offline --test
      cli_print_parity` passes the forced-settings test
      `print_mode_auto_compaction_persists_and_continues`, including the
      continued second response and persisted `"type":"compaction"` entry.
- [x] 35. Port `core/messages.ts` extended-message wiring
      (BashExecutionMessage/CustomMessage reach provider in run.rs).
- [x] 36. v3→v4 legacy session import in coding-agent session runtime
      (upstream `session-manager.ts`); `/import` conversion parity.
- [x] 37. v3→v4 migration tests using fixture v3 JSONL files.
- [x] 38. models.json runtime merge: file-backed `models_store.rs` load merged
      over bundled catalog; `applyModelsJson` wiring in run path.
- [x] 39. models.json merge tests (override, compat, bad-json error).
- [x] 40. Port `core/project-trust.ts` + `trust-manager.ts` into CLI path
      (`-a/-na`, defaultProjectTrust, `.pi/trust`).
- [x] 41. Project-trust binary tests (untrusted dir tool gating). (mock)
- [x] 42. Print-mode parity audit (`modes/print-mode.ts`): audit found the
      `--steer`/`--follow-up`/`--compact` items are **RPC commands**, not
      print-mode flags (no such flags exist in upstream `args.ts` /
      `print-mode.ts`); the real print-mode contract is sequential multi-turn
      prompting, `initialMessage` handling, quietStartup, and terminal
      error/abort exit semantics.
- [x] 43. Port print-mode output-formatting parity: `run.rs` now prompts each
      positional message as its own sequential turn (upstream
      `for message of messages { session.prompt(message) }`), folds prior
      turns into the agent context, surfaces a terminal Error/Aborted
      stop-reason on stderr with `errorMessage || "Request {stopReason}"` and
      exits nonzero, and joins text content blocks with `\n` (upstream
      ``writeRawStdout(`${content.text}\n`)``). Faux path queues one response
      per prompt. `tests/cli_print_parity.rs` (2 binary tests).
      `initialMessage` equals the leading positional prompt in the current
      single-shot path; quietStartup is TUI/startup-output scoped.
- [x] 44. Port JSON-event mode `modes/json-event.ts` → `modes/json_event.rs`
      (`--mode json`), event-envelope parity.
- [x] 45. JSON-event tests (fixture transcripts).
- [x] 46. Remaining CLI flags: `-nbt`, `-e/--extension`, `-ne/--no-extensions`,
      `--skill`, `-ns`, `--prompt-template`, `-np`, `--theme`, `--use-theme`,
      `--no-themes`, `-nc`, `-a/--approve`, `-na`, `--fork` — **plus** the
      `--append-system-prompt`, `--models`, `--tui-mode` surface and
      upstream `Args.diagnostics` (error→exit 1, warning→continue). Parsing
      landed; run-path honoring of `--fork` (fork-session-and-continue) sits
      with the session-tree parity (#83/#88) and the
      skill/prompt-template/extension/theme loaders with T6 (#73/#74).
- [x] 47. Flag-matrix golden test: `tests/cli_flag_matrix.rs` fires the full
      upstream `args.ts` flag surface against the built binary — every flag
      parses without an "unknown flags" diagnostic, `--help` lists the full
      surface, error-valued diagnostics (missing `--use-theme` value) exit
      nonzero with an `Error:` line, and invalid `--thinking` warns-but-runs.
      5 binary tests.
- [x] 48. Telemetry wiring: ported `core/telemetry.ts` → `core/telemetry.rs`
      (`isInstallTelemetryEnabled` honoring the `PI_TELEMETRY` env override:
      `1`/`true`/`yes` enable, `0`/`false`/`no` disable, unset defers to the
      `enableInstallTelemetry` setting) and wired it into provider attribution
      (the observable §2.2 env surface). 4 unit tests + 1 attribution
      env-override test. Follow-ups tracked: interactive-mode install-report
      ping to `pi.dev/api/report-install` (network-bound), and any
      tracing-subscriber span instrumentation (orthogonal to upstream's
      actual `PI_TELEMETRY` semantics).

## T4 — Server/client completion (P6 concurrency) — OPTIONAL / DEFERRED auxiliary

> Reclassified 2026-08-23: `pi-server`/`pi-client` is an auxiliary subsystem the
> `pi` binary does not link (run loop is in-process). These harden that path but
> advance no user-facing CLI parity. Defer behind T6/T5/T7 unless the goal
> becomes structural parity (see "Recut" preamble). Items kept as-is for tracking.

- [x] 49. `LiveSessionManager`: acquire/release exclusivity + attach/detach
      validation on server.
- [x] 50. Session lock + terminal-close semantics + command queuing.
- [x] 51. Subscription segment control for prompt/steer concurrency.
- [x] 52. Port `testing/service.ts` parity harness + conformance suite.
      (unit/mock) `cargo test -p pi-server --offline --quiet` passes the
      deferred service/runtime, test-client, snapshot, and lifecycle harness
      cases; the expanded server suite contains 32 conformance cases.
- [x] 53. Server conformance tests (30+ cases). (unit)
      `cargo test -p pi-server --offline --quiet` passes 55 total tests,
      including malformed frames, handshake rejection, snapshots,
      attach/detach/exclusive/dispose, queues, subscriptions, errors, and
      cleanup/order cases.
- [x] 54. Client reconnect state machine + connection-state listeners. (mock)
      `PiClient` now exposes `Disconnected`/`Connecting`/`Connected`, reconnects
      through a fresh Unix handshake with connection epochs, invalidates
      attached session handles on disconnect, and returns lifecycle callbacks.
      `cargo test -p pi-client --offline` covers snapshot refresh and the full
      lifecycle sequence over a fake Unix socket.
- [x] 55. Client lease/exclusive-attach parity (reconcile, detach-on-close).
      (mock/socket) `cargo test -p pi-client --offline --test auxiliary_parity`
      covers shared/exclusive attach, lease reconciliation, invalidation, and
      detach-on-close behavior.
- [x] 56. Client dispose semantics + promise timeouts. (mock)
      Requests have configurable handshake/request bounds; timed-out request
      ids are tombstoned so late responses do not tear down a healthy client;
      `dispose()` permanently releases state/listeners while `close()` remains
      reconnectable. Covered by `cargo test -p pi-client --offline`.
- [x] 57. Transport factory abstraction beyond unix (async-trait). (unit/mock)
      `cargo test -p pi-client --offline --test auxiliary_parity` and strict
      client clippy cover the boxed-future transport factory and fake transport
      variants; the Unix fragmented-frame path remains byte-tested.
- [x] 58. Client↔server E2E under reconnect + lease churn. (live)
      `cargo test -p pi-server --offline --test reconnect_lease_e2e --quiet`
      passed 4 local-Unix socket cases covering reconnect/backoff, shared and
      exclusive lease churn, disconnect/reattach, disposal, and fragmented
      protocol frames.

## T5 — TUI completion

- [x] 59. ConfigSelector full TUI component (config command interactive).
      (mock) `interactive/config_selector.rs` now covers searchable resource
      rows, circular/page navigation, scope switching, global toggles,
      project inherit/load/unload cycling, inherited-resource rendering, and
      synchronous settings flushes before exit. The component is wired through
      `commands/config.rs`; `cargo test -p pi-coding-agent --offline
      interactive::config_selector` passes 8 tests. The underlying data model
      and `PackageManager::resolve()` producer remain covered by the prior
      5+8+integration tests. PTY coverage is recorded in S-035; the full
      interactive matrix remains S-056.
- [x] 60. ConfigSelector snapshot tests. (mock) Five buildGroups tests, eight
      resolve() producer tests, `resolve_feeds_build_groups` integration, two
      interactive behavior tests, and a deterministic global/project render
      snapshot test now cover the selector surface. Verified with `cargo test
      -p pi-coding-agent --offline interactive::config_selector`; the real
      terminal exercise is recorded in S-035, with the full matrix in S-056.
- [x] 61. Full alt-screen screen-swap parity (save/restore around overlays).
      (unit/mock) `cargo test -p pi-tui --offline --quiet` includes nested
      save/restore and one-transition fixtures.
- [x] 62. Alt-screen swap tmux probe. (mock) The same 203-test suite covers
      conservative tmux mouse-mode and terminal transition sequences.
- [x] 63. ICU word segmentation (replace regex word-nav with unicode parity).
      `word_navigation.rs` now segments each CJK ideograph as its own
      word-like segment, matching upstream `Intl.Segmenter(undefined,
      {granularity:"word"})` per-character ideographic stepping (the previous
      port jumped the whole CJK run, a real backward-nav divergence from the
      upstream oracle). Editor Ctrl+arrow word nav (editor.rs
      find_word_backward/forward) picks this up automatically. Locale-aware
      dictionary grouping (zh/ja word sense) remains non-reproducible and is
      documented as such.
- [x] 64. Word-segmentation tests (upstream cases). 18 word_navigation tests,
      incl. the upstream CJK oracle translated to byte offsets: backward steps
      17→13→9→6→3→0 through "你好世界 test"; forward steps per-char
      0→3→6→9→12→17. Full pi-tui suite green.
- [x] 65. tmux `client_termfeatures` probe (feature detection parity). (mock)
      The capability matrix includes tmux forwarding and feature fallback.
- [x] 66. Terminal feature probe tests. (unit/mock) Named terminal families
      and unknown fallback are covered by the capability matrix fixture.
- [x] 67. Token-total footer reads (usage totals → footer parity). Ported
      upstream `formatTokens` (exact thresholds) into `interactive/footer.rs`,
      added `render_usage_stats` (`↑input ↓output Rcache Wcache CH{rate}%
      $cost`) and an optional `usage`/`cache_hit_rate` on `FooterData`, and
      wired `interactive.rs` to aggregate cumulative usage + latest-turn
      cache-hit-rate from transcript assistant messages (upstream
      `FooterComponent.render`).
- [x] 68. Footer tests. 5 footer.rs tests (formatTokens thresholds, usage-stats
      nonzero/cache/CH/cost rendering, usage-left+model-right layout) + 2
      interactive.rs tests (usage aggregation across turns + hit-rate math,
      empty-when-no-usage). Verified full `cargo test --workspace` green at
      1390. Interactive TUI E2E render not reachable headlessly (first-run
      terminal-feature glyph probes need a real terminal's font judgment).
- [x] 69. Editor IME/selection edge parity (kitty flags, bracketed paste).
      (unit) Kitty-release filtering and shifted printable/bracketed-input
      behavior pass in the pi-tui edge fixtures.
- [x] 70. Interactive-mode E2E tmux script: full slash-command matrix.
      (live) `cargo test -p pi-coding-agent --offline --test
      interactive_full_matrix --quiet` — 3 PTY cases passed; the companion
      `interactive_slash_pty` test passed 1 case with raw/cooked stty, ANSI,
      tmux, resize, Ctrl-C/Ctrl-D, and exact diagnostics asserted.

## T6 — Remaining coding-agent core modules (audit → port what's absent)

- [x] 71. Audit `core/bash-executor.ts` + `exec.ts` vs agent bash tool; port
      shell-capture/output-truncation parity where missing. AUDIT: already
      covered — `pi_agent/src/tools/bash.rs` implements `BashCapture` +
      `run_bash` with upstream capture semantics (concurrent stdout/stderr
      drain against the timeout deadline, `truncate_tail`, `[Showing lines
      X-Y of N... Full output truncated]` messages) and is wired into run.rs
      via `bash_tool`. Session 25 subsequently closed the live `onUpdate`
      callback path and basic throttled bash progress; S-018 still tracks full
      harness truncation/detail fixture parity.
- [x] 72. Port `core/system-prompt.ts` wiring into run context — run.rs now
      assembles the system prompt from `--system-prompt` base + the skills
      `<available_skills>` block + `--append-system-prompt` inputs (files read
      verbatim, inline text used as-is).
- [x] 73. Port `core/skills.ts` loader into run path (`--skill`, `-ns`,
      `.pi/skills`, `<agentDir>/skills`, settings `skills` key). New
      `core/skills.rs`: recursive dir loader honoring `.gitignore`/`.ignore`/
      `.fdignore`, name/description validation, name-collision dedup with
      winner/loser diagnostics, `formatSkillsForPrompt` `<available_skills>`
      block (the `-ns`/`-s` flips gate loading).
- [x] 74. Port `core/prompt-templates.ts` + `resource-loader.ts` into run path
      (`--prompt-template`, `-np`, `.pi/prompts`, `<agentDir>/prompts`). New
      `core/prompt_templates.rs`: `loadPromptTemplates`, `parseCommandArgs`,
      `substituteArgs` (`$1`/`$@`/`${N:-d}`/`${@:N[:L]}`), and
      `expandPromptTemplate` — run.rs expands `/template` positional messages.
      Plus new `core/context_files.rs`: port of `loadProjectContextFiles` +
      `findShadowedContextFile` + `footer-data-provider.ts findGitPaths`
      (AGENTS.md/CLAUDE.md + AGENTS.override.md ancestor walk with BOM strip,
      worktree-shadow dedup) — run.rs injects the `<project_context>` section
      and `-nc/--no-context-files` disables it (previously a dead flag).
- [x] 75. Port `core/http-dispatcher.ts` / proxy behavior if not covered.
      Evidence (unit/mock): `crates/pi-coding-agent/src/core/http_dispatcher.rs`
      applies the global `httpProxy` setting before auth/package/mode dispatch,
      preserves explicit `HTTP_PROXY`/`HTTPS_PROXY` values (including empty
      values), strips the settings BOM, reports malformed settings JSON, and
      retains the upstream idle/auto-select timeout defaults. Verified with
      `/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline
      --lib core::http_dispatcher --quiet` (3 passed), focused rustfmt, and
      `git diff --check`.
- [x] 76. Port `core/session-cwd.ts` semantics (interactive + RPC). New
      `core/session_cwd.rs`: `getMissingSessionCwdIssue`,
      `formatMissingSessionCwdError`, `formatMissingSessionCwdPrompt`,
      `MissingSessionCwdError`, `assertSessionCwdExists`. Wired into
      `modes/interactive.rs` resume: a session whose stored cwd no longer
      exists is refused with the upstream error banner instead of resumed.
- [x] 77. Port `core/cache-stats.ts` (cache-waste accounting) into the usage
      surface. New `core/cache_stats.rs`: `compute_cache_waste` /
      `collect_cache_misses` / `detect_cache_miss` over `&[Value]` session
      entries — prompt-cache miss detection (previous-turn prompt tokens
      re-billed as input/cacheWrite vs cacheRead), `NOISE_FLOOR_TOKENS`,
      compaction/branch_summary prev-reset, model-change + idle-TTL flags,
      `ModelPriceSource` (per-million cacheRead price) with `NoPrices`/fn-ptr
      sources. 7 fixture tests (first-turn, counted miss + cost math, noise
      floor, compaction reset, model change, price-source fallback, pending-
      message detect). The interactive consumers are now wired: the settings
      selector persists `showCacheMissNotices`, the transcript re-derives
      timestamp-keyed cache-miss notices, the footer aggregates shadow session
      entries across summary/tool usage, and `/session` renders the
      "Cache Re-billed" line. Compaction, `/clear`, new-session, resume, and
      import boundaries reset/reload the shadow cache ledger. `timings.ts` (a
      `PI_TIMING=1`-gated stderr
      startup profiler) is a deliberate non-port — the Rust binary has no
      equivalent startup-timing namespace, so the module would be dead code.
- [x] 78. Port `core/auth-guidance.ts` messages parity. New `core/auth_guidance.rs`:
      `getProviderLoginHelp` (docs providers.md/models.md paths + `/login`),
      `formatNoModelsAvailableMessage`, `formatNoModelSelectedMessage`,
      `formatNoApiKeyFoundMessage(provider)` ("the selected model" when
      unknown). Wired into `list_models.rs`: an empty auth-gated model set now
      surfaces the guidance (previously a bare "No models available."). The
      per-provider no-api-key error surfaces in pi-ai remain a deliberate
      follow-up (the guidance module + formatters are ready to append there).
- [x] 79. Port `core/settings-diagnostics.ts` + `diagnostics.ts` — the
      `ResourceDiagnostic`/`ResourceCollision` types landed in new
      `core/diagnostics.rs` (warning/error/collision kinds); skills +
      prompt-template loaders emit them and run.rs surfaces them as warnings.
- [x] 80. Extended-messages + provider-composer edge audits with tests.
      AUDIT: `provider_composer.rs` (applyModelsJson/applyExtension/override/
      compat) landed with a test suite; extended-message wiring to the provider
      was already landed as T3 #35. No new edge gap found in this pass.

## T7 — Data model, session tree, export, RPC edge parity

- [x] 81. Negative-usage decision: widened pi-ai `Usage` token counts,
      optional cache-write/reasoning counts, session stats, usage totals, and
      model cost accounting to signed `i64`/preserved negative values; normal
      context/cache estimates clamp ledger corrections out of window arithmetic.
      (unit: `cargo test --workspace`)
- [x] 82. Re-enabled the upstream negative-adjustment conformance case (C-neg)
      in both the in-memory/JSONL agent backend and SQLite backend; `-2`
      input/total and `-0.5` cost produce `uncached=10`, `total=18`,
      `costTotal=9.5`. (unit: `cargo test --workspace`)
- [x] 83. Session tree/navigation parity: `get_tree` RPC. `modes/rpc.rs`
      `build_tree` now matches upstream `SessionManager.getTree()`: nodes as
      `{ entry, children, label? }` (label emitted only when `labelsById` has
      the id, sourced from `JsonlSession::get_label`), children sorted by
      entry timestamp ascending, and self-parent / orphan (missing-parent)
      entries treated as roots — fixing the prior revision, which emitted no
      label and left children unsorted in a stale-clone snapshot. The
      interactive "entry-tree banner" render remains PTY-bound with the TUI
      surface.
- [x] 84. Session tree tests. 3 `RpcRuntime::build_tree` tests: nest + child
      timestamp ordering, self-parent/orphan-root handling, label emitted only
      when resolved.
- [x] 85. export_html full parity audit. AUDIT: `core/export_html.rs` already
      ports the upstream `export-html/index.ts` `generateHtml`/`exportFromFile`
      pipeline (color/theme derivation, `js_replace` ES-substitution semantics,
      base64 session payload, `TEMPLATE_RENDERED_TOOLS`). mermaid/search are
      client-side features in the vendored template (template.js/html), covered
      byte-for-byte by the oracle goldens; the file-export path writes directly
      to `outputPath` with no temp files (no tmp-cleanup divergence). Custom
      tool pre-rendering (extension `renderCall`/`renderResult`) is the one
      documented no-op seam pending the extension system.
- [x] 86. export_html fixture expansion. The `export_session.jsonl` parity
      fixture now exercises tool-call + thinking content blocks, a `compaction`
      entry (with `retainedTail` + usage), and a `branch_summary` entry (with
      usage), alongside the us/agent exchange; the oracle goldens (dark/light)
      were regenerated and both remain byte-identical. `export_html_parity.rs`
      gains a `fixture_covers_tool_compaction_and_branch_summary` coverage
      test (8 non-session entries). Workspace green at 1416.
- [x] 87. RPC get_entries/get_tree/get_messages/get_last_assistant_text audit.
      `get_entries` preserves oldest-first order, `since`, and `leafId`;
      `get_tree` returns the labeled tree plus leaf; and the message queries
      now rebuild the active branch after switch/fork, trim the last assistant
      text, and return real entry ids for fork messages. Covered by RPC tests.
      Runtime honoring of set_auto_compaction/retry/steering/follow-up remains
      tracked separately in #88.
- [x] 88. RPC runtime audit: set_auto_compaction/retry/steering/follow-up
      honored. (unit; mock) `RpcRuntime` applies command mutations to its live
      flags, persisted settings, queue modes, prompt configuration, and
      `get_state` response. Evidence: `cargo test -p pi-coding-agent --offline
      rpc_runtime_control_commands_update_settings_and_state`, the existing
      `rpc_applies_settings_to_stream_compaction_retry_and_queues`, queue-mode
      drain tests, and the full RPC suite.
- [x] 89. `pi update` distribution boundary: pi-rust performs no upstream
      latest-release lookup and has no upstream update banner or self-replace
      path. `--extensions` updates installed extension packages and `--models`
      refreshes the pi.dev catalogs with bounded parallel requests, retries,
      freshness/ETag handling, and persistence. (unit; local)
- [x] 90. Update tests cover the Rust-repository self-update instruction,
      non-zero boundary, absence of the old release lookup, model-catalog
      success/transient HTTP failure paths, and package update behavior. (unit;
      local) Evidence: `cargo test -p pi-coding-agent --offline --lib
      core::version_check::tests`, `cargo test -p pi-coding-agent --offline
      --lib core::remote_catalog_provider::tests`, and `cargo test -p
      pi-coding-agent --offline --test cli_commands update_`.

## T8 — Evals, packaging, parity suite

- [x] 91. pi-evals: capture usage tokens from subprocess runs (parse session
      JSONL usage) so eval metrics match upstream. Evidence: mock/unit —
      `/home/mustbearnold/.cargo/bin/cargo test -p pi-evals --offline --quiet`
      and the session-usage fixture suite; faux smoke JSONL recorded input
      1246, output 20, total 1266.
- [x] 92. pi-evals: extension-scenario diagnostics under faux (unscorable →
      scorable). Evidence: unit/mock —
      `/home/mustbearnold/.cargo/bin/cargo test -p pi-evals --offline --test extensions --quiet`
      and the schema-1 `extension-authoring` faux diagnostic fixture.
- [x] 93. `scripts/parity-suite.mjs`: CLI matrix checks (exit codes/format vs
      upstream). Evidence: unit/mock — 8 CLI cases pass in the complete
      release parity run, including help, unknown-flag, exit-code, and output
      format comparisons.
- [x] 94. Parity suite: golden RPC transcripts. (fixtures) Evidence:
      mock — 2 ordered JSONL transcript fixtures pass, including malformed and
      async lifecycle envelopes.
- [x] 95. Parity suite: session-file byte fixtures (v1/v2/v3/v4 goldens).
      Evidence: unit/mock — 5 legacy/current JSONL fixtures pass canonical
      byte, migration, parent-chain, and current-write checks.
- [x] 96. Parity suite: settings/auth/models.json on-disk round-trip goldens.
      Evidence: unit/mock — storage/settings/auth/models/resource fixtures and
      9 focused round-trip checks pass with unknown-key preservation.
- [x] 97. Release-build verification: `cargo build --release` + full binary
      suite in release. (live) Evidence: `/home/mustbearnold/.cargo/bin/cargo
      build --workspace --release --offline` completed successfully, followed
      by `/home/mustbearnold/.cargo/bin/cargo test --workspace --release
      --offline --quiet -- --test-threads=2`, with every release target green
      (including the 476-test pi-coding-agent library target and the 203-test
      pi-tui target). The bounded test concurrency is required for the live
      tmux/PTY fixtures; the unbounded default parallel run can starve that
      terminal fixture and is not used as acceptance evidence.
- [x] 98. PLAN.md session-13 ledger update + reviewer-gate prep (§0.3).
      Evidence (unit/mock): `PLAN.md` now contains the current Session-13
      reviewer matrix, pinned upstream revision, open-row inventory, evidence
      conditions, and exact final-gate commands; `node scripts/conversion-progress.mjs`
      and the release acceptance command are recorded for reproducibility.

## T9 — Final 100% verification pass

- [x] 99. Full-surface audit: §2.2 env vars, §2.3 on-disk formats, §4.4 RPC
      taxonomy — each demos against the real binary. (live) Evidence (live):
      `.unlazy/full-conversion-20260825/gates/leaf-99-current.md` records the
      isolated release-binary print probe, explicit session-id probe, direct
      RPC lifecycle/taxonomy probe, on-disk session checks, environment
      matrix, and the exact `node scripts/parity-suite.mjs` result:
      `checks: 40 passed, 0 failed, 1 not-run, 41 total`; the credentialed
      network smoke remains explicitly not-run.
- [x] 100. Final clean-room check: fresh clone → workspace tests green,
      0 warnings, clippy -D warnings clean, flag/env/tool/provider matrix
      recorded in PLAN.md with tiers. Milestone tag:
      `conversion-97.59-clean-room`. Evidence (live):
      `.unlazy/full-conversion-20260825/gates/clean-room-current.md` records
      a fresh clone at `07e0623cde0ba5caf18275c773df31e56ee37ad1` with the
      pinned `upstream_pi` oracle at
      `5cd93f688aaab89dbb6dfa4aca535f21796ae185`. The isolated gates pass:
      `cargo fmt --all -- --check`,
      `cargo clippy --workspace --all-targets --offline -- -D warnings`,
      `cargo build --workspace --release --offline`,
      `cargo test --workspace --offline --quiet -- --test-threads=2`,
      `node scripts/conversion-progress.test.mjs` (7 passed), the release
      parity command in the report (40 passed, 0 failed, 1 not-run), and
      `git diff --check`. The credentialed live branch remains explicitly
      not-run; the known fake-node failure was not reproduced.

---

## Supplemental source-audit tasks (S1)

The first 100 entries were the original work queue. This section is the
additional inventory required to make the ledger exhaustive. These items came
from a second pass over every package TODO, the pinned upstream module map,
the current Rust implementation, and the documented Session 11 divergences.
The denominator is allowed to grow if the source inventory discovers a new
observable contract; the ledger is frozen only by S-001 and the final audit.

### S1-A — inventory and evidence control

- [x] S-001 Complete a source-to-source inventory of every upstream exported
      runtime surface and record one ledger ID per observable behavior; freeze
      the denominator only after the inventory and all TODO files reconcile.
      Evidence (unit/mock): `.unlazy/full-conversion-20260825/gates/leaf-S1-current.md`
      records the upstream module/export census, Rust ownership census, direct
      runtime declaration census, public runtime-name census, crate TODO-line
      census, and the explicit module-to-ledger ownership map. The report also
      records the pinned upstream revision and the exact inventory commands;
      `node scripts/conversion-progress.mjs` is the reproducible ledger check.
      The current denominator is 166 and remains subject to the final S-066
      freeze; no final 100% claim is made here.
- [x] S-002 Reconcile stale `TODO.md`, session reports, README, and PLAN claims
      against the current source and replace every “done” claim lacking an
      exact test or live command with an open task. Evidence (unit/mock): all
      ten crate TODO files, the four dated session/reviewer reports, and the
      current `README.md`, `PLAN.md`, and `HANDOFF.md` were reconciled; the
      historical reports are explicitly labeled as snapshots and current
      claims carry exact commands or evidence references. `git diff --check`
      passes, and the targeted stale-claim scan is recorded in
      `.unlazy/full-conversion-20260825/gates/leaf-S2-current.md`.
- [x] S-003 Add a reproducible ledger-progress checker that counts only
      checked/unchecked tasks in this file and fails on malformed checklist
      lines or duplicate task IDs. Evidence (unit/mock):
      `node --test scripts/conversion-progress.test.mjs` (7 passed) covers
      stable positive output, malformed status/IDs, duplicate numeric and
      supplemental IDs, and an empty task set; `node scripts/conversion-progress.mjs`
      reports `Conversion progress: 95.18% (158/166; 8 open)` on the real
      ledger. The checker now validates every checklist-looking line instead
      of silently ignoring malformed task rows.
- [x] S-004 Run the independent-reviewer gate against this exhaustive ledger,
      including a review of every deferred divergence and evidence tier.
      Evidence (mock): the independent report
      `.unlazy/full-conversion-20260825/gates/leaf-S4-current.md` records an
      APPROVE-WITH-CONDITIONS verdict against the pinned
      `earendil-works/pi` oracle at
      `5cd93f688aaab89dbb6dfa4aca535f21796ae185`. It verifies the exact
      current open-row set, `node scripts/conversion-progress.mjs`
      (`Conversion progress: 97.59% (162/166; 4 open)` at review time),
      `node --test scripts/conversion-progress.test.mjs` (7 passed),
      `git diff --check`, matching local/remote HEAD
      `d16642dc043d95616bca024fb32724500e5e2fe5`, and the
      `conversion-97.59-clean-room` tag. The follow-up resolution records
      the S-065 condition as satisfied; the remaining conditions explicitly
      keep S-027 open and defer S-066 until no behavioral or unclassified
      task remains. No final 100% claim is made here.

### S1-B — pi-ai residual provider and transport parity

- [x] S-005 Wire deferred-response fetch/cancel through the coding-agent model
      runtime, interactive mode, RPC mode, and provider-composer path; test a
      deferred response from request through resolution and cancellation.
      Evidence (unit/mock): `ModelRuntime` now owns auth-applied stream,
      simple-stream, deferred-fetch, and deferred-cancel dispatch; the faux
      provider is registered through that facade in print, interactive, JSON,
      and RPC modes. The deferred runtime test submits a deferred response,
      polls it to a final message, then cancels a second response and verifies
      the in-band provider error. Provider-composer and mode-wiring tests
      verify the hooks survive catalog composition. Verified with
      `cargo test -p pi-coding-agent --offline --lib
      core::model_runtime::tests::deferred_runtime --quiet` (1 passed),
      `cargo test -p pi-coding-agent --offline --lib
      core::provider_composer --quiet` (15 passed),
      `cargo test -p pi-coding-agent --offline --lib deferred_mode_wiring
      --quiet` (1 passed), `cargo test -p pi-coding-agent --offline --lib
      modes::interactive --quiet` (13 passed), `cargo test -p pi-coding-agent
      --offline --lib modes::rpc --quiet` (45 passed), and the real
      `interactive_slash_pty` fixture (1 passed).
- [x] S-006 Complete lazy API capability propagation for deferred fetch/cancel,
      including missing-capability error text and models-store overrides.
      Evidence (unit): `pi-ai/src/api/lazy.rs` now exposes only declared
      deferred capabilities, lazily loads implementations, and preserves the
      upstream `API does not support deferred responses` and `API cannot cancel
      deferred responses` diagnostics. Model-registry overlays retain the
      shared credentials/models store and provider deferred hooks. Verified
      with `cargo test -p pi-ai --offline --lib api::lazy --quiet` (2 passed),
      `cargo test -p pi-ai --offline --lib --quiet` (288 passed), and
      `cargo test -p pi-coding-agent --offline --lib core::model_registry
      --quiet` (8 passed).
- [x] S-007 Port the upstream image retry loop and its abort/quota/error
      classification for image generation requests. Evidence (unit/mock): the
      OpenRouter image adapter now uses the correct zero-based retry index,
      parses numeric and IMF-fixdate `Retry-After`, caps server delays, and
      observes a shared abort flag while sending, reading, and backing off.
      The shared assistant retry classifier keeps quota/billing failures
      terminal while retaining transient provider/transport/retry guidance.
      Verified with `cargo test -p pi-ai --offline --lib
      api::openrouter_images --quiet` (10 passed), `cargo test -p pi-ai
      --offline --lib api::openrouter_images::retry_tests --quiet` (5
      passed), `cargo test -p pi-ai --offline --lib utils::retry --quiet`
      (16 passed), `cargo test -p pi-ai --offline --lib images --quiet` (19
      passed), and the full pi-ai library suite (290 passed).
- [x] S-008 Complete constrained-sampling/grammar tool support for every
      adaptor that advertises strict or grammar tools; reject unsupported
      schemas with the upstream diagnostics. Evidence (unit/mock): shared
      strict-schema rewrites, optional-property/null handling, unsupported-key
      diagnostics, grammar precedence/inference, and monotonic streaming JSON
      deltas are covered by `cargo test -p pi-ai --offline --lib
      api::constrained_sampling --quiet`; OpenAI Completions custom-tool wire
      shape and stream replay by `cargo test -p pi-ai --offline --lib
      api::openai_completions --quiet`; Responses/Azure/Codex shared custom
      shapes, replay, and exact errors by `cargo test -p pi-ai --offline --lib
      api::openai_responses_shared --quiet`, the Azure/Codex module tests, and
      the Anthropic/Bedrock/Google adaptor fixtures. The complete adaptor suite
      passes with `cargo test -p pi-ai --offline --quiet` (307 library, 4 + 9
      + 2 integration tests); `cargo clippy -p pi-ai --offline --all-targets
      -- -D warnings`, `cargo check --workspace --offline`, `cargo fmt --all --
      --check`, and `git diff --check` also pass. An independent parity review
      against upstream commit `5cd93f688aaab89dbb6dfa4aca535f21796ae185`
      approved the implementation with no blockers.
- [x] S-009 Complete Codex WebSocket session caching/reuse and the
      `websocket-cached` transport behavior, including eviction and close/error
      recovery. Evidence (mock/unit): session+account cache keying, cached
      context deltas, busy-socket isolation, 5-minute idle and 55-minute
      max-age eviction, `cacheRetention: "none"`, missing-continuation retry,
      explicit WebSocket/SSE fallback, and error cleanup are covered by
      `cargo test -p pi-ai --offline --lib api::openai_codex_responses --quiet`
      (34 passed), including the local mock WebSocket fixtures
      `websocket_cached_reuses_session_socket_and_sends_input_delta`,
      `websocket_cached_reopens_after_missing_previous_response`, and
      `websocket_session_cache_is_scoped_by_authenticated_account`. The
      independent review against upstream
      `upstream_pi/packages/ai/src/api/openai-codex-responses.ts` returned
      APPROVE with no blockers. Supporting gates pass with `cargo check -p
      pi-ai --offline`, `cargo clippy -p pi-ai --offline --all-targets -- -D
      warnings`, `cargo test -p pi-ai --offline --quiet`, `cargo fmt --all --
      --check`, and `git diff --check`.
- [x] S-010 Complete AWS credential/profile-file and region resolution parity
      for Bedrock, with environment/config precedence fixtures. Evidence
      (unit/mock): `cargo test -p pi-ai --offline --lib
      api::bedrock_converse --quiet` (43 passed) covers
      `explicit_profile_ignores_ambient_access_keys_and_loads_profile_credentials`,
      `scoped_profile_ignores_ambient_access_keys`,
      `ambient_profile_preserves_env_key_precedence`,
      `aws_config_file_region_resolves_selected_profile`, and
      `region_precedence_is_arn_then_option_then_env_then_config_then_default`.
      The same suite covers `parses_ecs_credentials_response`,
      `parses_sts_web_identity_response`,
      `resolves_ecs_full_uri_credentials_with_authorization_token`, and
      `resolves_web_identity_credentials_with_mock_sts`. The exported
      `stream` and `stream_simple` boundaries are covered by
      `public_stream_resolves_ecs_credentials_before_bedrock_request` and
      `public_stream_resolves_web_identity_credentials_before_bedrock_request`,
      including local ECS/STS HTTP endpoints, Bedrock eventstream responses,
      credential IDs, form fields, and session-token signing headers. Provider
      auth source labels are covered by `cargo test -p pi-ai --offline --lib
      providers::all::tests::amazon_bedrock_auth_recognizes_ecs_and_web_identity_sources
      --quiet` (1 passed). Supporting evidence: `cargo check -p pi-ai
      --offline`, `cargo clippy -p pi-ai --offline --all-targets -- -D
      warnings`, `cargo test -p pi-ai --offline --quiet` (325 library, 4 + 9 +
      2 integration tests), `cargo fmt --all -- --check`, and `git diff
      --check`. Upstream parity references:
      `upstream_pi/packages/ai/src/api/bedrock-converse-stream.ts:144-205,1165-1204`,
      `upstream_pi/packages/ai/src/providers/amazon-bedrock.ts:54-79`,
      `upstream_pi/packages/ai/src/env-api-keys.ts:167-184`,
      `upstream_pi/packages/ai/test/bedrock-credentials.test.ts:66-115`, and
      `upstream_pi/packages/ai/test/bedrock-endpoint-resolution.test.ts:96-208`.
- [x] S-011 Complete Google Vertex ADC file, token URI, scope, refresh, and
      project/location precedence parity. Evidence (mock): `RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-ai --offline --lib google_vertex --quiet` (18 passed) covers explicit ADC path/default-home selection, service-account JWT exchange with the file token URI and configured scopes, authorized-user refresh-token exchange with file credentials, API-key publisher routing without project/location, and the existing ADC project requirement. `RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-ai --offline --lib google_vertex_provider --quiet` (4 passed) covers stored credential-environment precedence, ambient API-key precedence, ADC project/location requirements, and no fallback from a missing explicit ADC path. Supporting evidence: `RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo check -p pi-ai --offline`, `RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTFMT=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustfmt /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo fmt --all -- --check`, and `git diff --check`. Upstream parity references: `upstream_pi/packages/ai/src/providers/google-vertex.ts`, `upstream_pi/packages/ai/src/api/google-vertex.ts`, `upstream_pi/packages/ai/src/env-api-keys.ts`, and the ADC credential-file/token tests.
- [x] S-012 Complete Cloudflare AI Gateway account/gateway binding and all
      documented base URL/header precedence cases. Evidence (mock):
      `RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-ai --offline --lib cloudflare --quiet && printf 'S012_CLOUDFLARE_BINDING_TESTS_PASS\n'` (18 passed) covers prefix/origin validation, JSON body translation, provider/endpoint/query extraction, lower-cased header forwarding, derived-header stripping, dot-segment normalization, rejection paths, and binding dispatch. `RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo test -p pi-ai --offline --lib cloudflare_provider --quiet && printf 'S012_CLOUDFLARE_PROVIDER_TESTS_PASS\n'` (5 passed) covers stored-field precedence, scoped account/gateway environment, inline upstream authorization, and gateway base URL resolution. Static evidence: `RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTFMT=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustfmt /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo check -p pi-ai --offline && RUSTC=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc RUSTFMT=/home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustfmt /home/mustbearnold/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo fmt --all -- --check && git diff --check && printf 'S012_STATIC_CHECKS_PASS\n'`.
      The binding fixture also proves encoded dot/empty-segment handling and
      forwards the optional runtime-neutral cancellation handle.
- [x] S-013 Complete GitHub Copilot OAuth refresh, enterprise-domain, token
      exchange, and expired-credential behavior in the auth store and CLI.
      Evidence (mock): `/home/mustbearnold/.cargo/bin/cargo test -p pi-ai
      --offline --test copilot_oauth_parity --quiet` (5 passed) and
      `/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline
      --test copilot_oauth_parity --quiet` (4 passed), plus offline checks and
      owned-path formatting. Fixtures cover enterprise URL normalization,
      proxy endpoint precedence, rotated refresh credentials, model filtering,
      expired auth-check refresh, `--no-refresh`, and failure-preserving auth
      storage.
- [x] S-014 Complete Anthropic OAuth provider-name mapping, adaptive-thinking
      replay, eager beta headers, client injection, deferred tool references,
      and server-side fallback behavior. Evidence (mock):
      `/home/mustbearnold/.cargo/bin/cargo test -p pi-ai --offline --test
      anthropic_provider_parity --quiet` (9 passed),
      including preservation of a complete tool input supplied at
      `content_block_start` when no incremental JSON delta follows,
      `/home/mustbearnold/.cargo/bin/cargo test -p pi-ai --offline --test
      anthropic_stream --quiet` (9 passed), the focused module tests, strict
      pi-ai clippy, owned-path formatting, and `git diff --check`.
- [x] S-015 Add provider-by-provider request/stream/usage/error fixtures for
      all catalog providers, including each advertised API variant and an
      explicit no-API implementation check where upstream intentionally has
      one. Evidence: mock — `cargo test -p pi-ai --offline --test
      provider_matrix --quiet` passed 4 matrix tests covering 50 text
      provider/API pairs, OpenRouter images, success/error/usage/request
      assertions, and five negative no-API controls; fixture index records
      upstream oracle paths and evidence tiers.
- [x] S-016 Finish remote model-catalog HTTP semantics: RFC date parsing,
      freshness, ETag/304 handling, 404/501 handling, atomic persistence, and
      offline behavior. Evidence (mock): `/home/mustbearnold/.cargo/bin/cargo
      test -p pi-coding-agent --offline --test model_catalog_parity --quiet`
      (8 passed), including RFC-date parsing, conditional requests, 304,
      unavailable responses, malformed payloads, file locking, and offline
      refusal.
- [x] S-017 Add model-catalog refresh and runtime-merge tests for every
      provider shape, including custom providers, malformed payloads, and
      generated-at precedence. Evidence (unit/mock): `/home/mustbearnold/.cargo/bin/cargo
      test -p pi-ai --offline --test model_catalog_parity --quiet` (4 passed)
      and the coding-agent catalog fixture above; unknown provider fields are
      retained through the typed model/catalog persistence round trip.

### S1-C — pi-agent contract and harness integration

- [x] S-018 Emit upstream `tool_execution_update` events from built-in tools
      through the `onUpdate` callback, including throttling and final-update
      ordering. Evidence (unit): `cargo test -p pi-agent --offline --test
      tools --quiet` covers coalesced progress, final truncation metadata,
      timeout output/full-file preservation, and normal/error output; rich-loop
      tests cover live parallel updates, final ordering, and late-callback
      suppression.
- [x] S-019 Preserve and apply tool `terminate` hints in the model-facing
      result/session/RPC event contract, including mixed parallel batches.
      (unit; mock) Lifecycle end events now retain the raw upstream
      `AgentToolResult`, RPC serialization preserves `terminate`, and the
      session writer stores the hint as the JSONL message-entry marker while
      leaving the model-facing `ToolResultMessage` unchanged. Mixed and
      all-terminating parallel batches plus RPC/session persistence are covered
      by `cargo test -p pi-agent --offline rich_agent::tests --quiet` and
      `cargo test -p pi-coding-agent --offline modes::rpc::tests --quiet`.
- [x] S-020 Verify exact `AgentTool` prepare/execute/error semantics for every
      built-in and coding-agent tool against upstream malformed-call fixtures;
      close any remaining signature or payload drift. Evidence (unit):
      `cargo test -p pi-agent --offline rich_agent::tests --quiet`,
      `cargo test -p pi-agent --offline --test tools --quiet`, and
      `cargo test -p pi-coding-agent --offline --test tool_contract` cover
      mutable before-hook arguments, edit preparation, optional details,
      parallel immediate errors/after-hook overrides, and malformed read,
      write, edit, bash, ls, find, and grep calls.
- [x] S-021 Integrate the `AgentHarness` lane/session abstraction into the
      coding-agent run path instead of maintaining a parallel direct-loop
      implementation. Evidence: unit/mock — `cargo test -p pi-agent --offline
      --lib harness --quiet` passed 100 harness tests; coding-agent library
      tests passed 469, and `harness_modes` passed the JSON/JSONL mode fixture.
      Durable lane/queue state, cancellation, watches, and session persistence
      are covered by the owned harness implementation.
- [x] S-022 Wire the complete harness event and telemetry lifecycle into print,
      interactive, JSON, JSONL, and RPC modes with span/event golden checks.
      Evidence: mock/PTY — RPC tests passed 41/41, the JSON lifecycle ordering
      fixture passed, the print/JSON integration slice passed 10/10, and
      `interactive_slash_pty` passed with raw-mode and alternate-screen
      restoration.
- [x] S-023 Add panic-safe telemetry callback settlement equivalent to the
      upstream `try/catch/finally` span lifecycle. (unit) The in-memory
      adapter catches callback unwinds, settles spans as automatic errors
      without inspecting panic payloads, preserves explicit statuses, resumes
      the original panic, and settles nested spans inner-first. Evidence:
      `cargo test -p pi-telemetry --offline --quiet` (6 passed),
      `cargo test -p pi-agent --offline --quiet`,
      `cargo test -p pi-tui --offline --quiet` (186 passed), and
      `cargo test --workspace --offline --quiet`.
- [x] S-024 Complete JSON-schema validation parity for unions, arrays, numeric
      bounds, formats, additional properties, and partial tool-call arguments;
      compare all diagnostics with upstream. (unit) Validation now resolves
      local refs, validates `allOf`/`anyOf`/`oneOf`/`not`, tuple and constrained
      arrays, enums/consts, object property rules, numeric/string bounds,
      common formats, and nullable optional fields while retaining upstream
      primitive coercion. Evidence: `cargo test -p pi-agent --offline
      tools::validation -- --nocapture` (12 passed),
      `cargo test -p pi-agent --offline --quiet` (174 passed), and
      `cargo test --workspace --offline --quiet`.

### S1-D — coding-agent product/runtime parity

- [x] S-025 Add automatic compaction/context rebuilding to the one-shot print
      path, including settings thresholds, retained-tail entries, and
      continuation after compaction. (mock) Evidence: `cargo test -p
      pi-coding-agent --offline --test cli_print_parity` (4 passed), including
      the forced JSONL compaction/continuation test; `cargo check --workspace
      --offline`.
- [x] S-026 Complete legacy v1/v2/v3-to-v4 import integration for every resume,
      switch, fork, and `/import` path, not only the standalone converter.
      CLI `--continue`/`--resume`/`--session`/`--fork` now select durable
      sessions before harness creation, restore the active branch context, and
      append directly to the selected file. Interactive and RPC startup use
      the same selector behavior; root scans and explicit switch/import paths
      atomically migrate legacy files, and `/import` honors a custom session
      directory while reading the migrated v4 header metadata. Evidence:
      `cargo test -p pi-coding-agent --offline --test cli_print_parity --quiet`
      (7 passed), `cargo test -p pi-coding-agent --offline --test
      cli_flag_matrix --quiet` (5 passed), `cargo test -p pi-coding-agent
      --offline --lib interactive:: --quiet` (33 passed), `cargo test -p
      pi-coding-agent --offline --lib modes::rpc::tests --quiet` (40 passed),
      `cargo test --workspace --offline --quiet`, `cargo fmt --all -- --check`,
      `git diff --check`, and `node scripts/conversion-progress.mjs`.
- [x] S-027 Enforce the Rust-only extension boundary and cover the native
      replacement surfaces. The user explicitly changed acceptance to 100%
      Rust, so the prior Node/Bun bridge and embedded JS runtime assets were
      removed rather than retained as a compatibility path. Compiled Rust
      factories cover commands, hooks, renderers, tools, flags, and provider
      registration; filesystem JS/TS paths are rejected or ignored without
      execution, and npm/Bun package execution is rejected with deterministic
      Rust-native guidance. Evidence (unit/mock):
      `cargo test -p pi-coding-agent --offline --test extensions_parity --
      --test-threads=1` (7 passed),
      `cargo test -p pi-coding-agent --offline --lib
      core::extensions::loader::tests -- --test-threads=1` (11 passed),
      `cargo test -p pi-coding-agent --offline --lib
      core::extensions::integration::tests -- --test-threads=1` (12 passed),
      `cargo test -p pi-coding-agent --offline --lib package_manager --
      --test-threads=1` (22 passed), and the full coding-agent library target
      (507 passed). The accepted limitation is explicit: arbitrary external
      JS/TS extension execution is not part of the Rust-only distribution.
- [x] S-028 Document and test the exact supported replacement behavior for the
      Rust distribution. The compiled binary performs no upstream latest-release
      lookup; `pi update --self` exits non-zero with the pi-rust source-checkout
      rebuild instruction instead of claiming that the running executable was
      replaced. The README documents the source-checkout rebuild command.
      Evidence (unit/local):
      `cargo test -p pi-coding-agent --offline
      commands::package::tests::self_update_fallback_instruction_matches_distribution_contract`,
      `cargo test -p pi-coding-agent --offline --test cli_commands update_`,
      and `cargo test --workspace --offline --quiet`.
- [x] S-029 Complete install-telemetry report transport, opt-out, retry, and
      offline behavior where the upstream CLI performs the network ping.
      `core/telemetry.rs` now sends the separate anonymous install report with
      a Rust Pi user-agent, uses a bounded five-second best-effort transport, retries
      transient network/429/5xx failures, and never surfaces report failure to
      the interactive UI. `PI_OFFLINE` short-circuits before transport;
      `PI_TELEMETRY` overrides the default-on `enableInstallTelemetry` setting;
      the interactive settings selector persists that opt-out; and the
      startup path records the last shipped version and launches the report
      only on a fresh/version-changed interactive install boundary. The
      endpoint has a test-only `PI_INSTALL_TELEMETRY_URL` seam. Evidence
      (unit/mock): `cargo test -p pi-coding-agent --offline --lib
      core::telemetry::` (7 passed), `cargo test -p pi-coding-agent
      --offline --quiet` (458 coding-agent unit tests plus integration
      targets), and the workspace check/test gates.
- [x] S-030 Wire cache-miss notices and “cache re-billed” display data into the
      interactive transcript/footer, including setting gates and reset events.
      `modes/interactive.rs` maintains a serialized shadow of deferred
      interactive entries so `cache_stats` can re-derive notices before exit
      persistence, while cumulative footer usage includes assistant,
      tool-result, and summary usage. The settings selector exposes the
      upstream-off `showCacheMissNotices` gate; `/session` renders the
      `Cache Re-billed` token/cost/miss-count line; auto-compaction, `/clear`,
      new-session, resume, and import reset or reload the cache segment.
      Evidence (unit/integration):
      `cargo test -p pi-coding-agent --offline --lib interactive::` (33
      passed), `cargo test -p pi-coding-agent --offline --quiet` (455
      coding-agent unit tests plus integration targets),
      `cargo check --workspace --offline`, and
      `cargo test --workspace --offline --quiet` (176 pi-agent, 286 pi-ai,
      455 pi-coding-agent, 186 pi-tui, plus integration/doctest targets).
- [x] S-031 Port the `PI_TIMING=1` startup timing surface or prove/document its
      intentional non-port with a compatibility test and user-facing fallback.
      The Rust distribution deliberately does not expose upstream's startup
      timing namespaces. When `PI_TIMING=1` is requested, `pi` emits a warning
      naming the supported `/usr/bin/time -p` process-level fallback; other
      values retain the upstream exact-one gate and remain silent. Evidence
      (unit): `cargo test -p pi-coding-agent --offline
      core::timings::tests::matches_upstream_exact_one_gate_and_fallback_text`,
      `PI_TIMING=1 ./target/debug/pi --version` (mock binary smoke).
- [x] S-032 Wire provider-specific no-key/auth guidance into every model
      resolution and provider error path, preserving upstream help text.
      Print, JSON, interactive, and RPC terminal provider errors now normalize
      pi-ai auth failures to the upstream `/login`/docs guidance, including
      OAuth-capable providers and preserving non-auth/network errors. Evidence
      (unit/mock): `cargo test -p pi-coding-agent --offline --lib
      core::auth_guidance::tests --quiet` (4 passed), the RPC auth-envelope
      regression (1 passed), `cargo test -p pi-coding-agent --offline --lib
      interactive:: --quiet` (33 passed), `cargo test -p pi-coding-agent
      --offline --lib modes::rpc::tests --quiet` (41 passed), `cargo test -p
      pi-coding-agent --offline --test cli_json_mode --quiet` (2 passed),
      `cargo test -p pi-coding-agent --offline --test cli_print_parity
      --quiet` (7 passed), `cargo check -p pi-coding-agent --offline`,
      `cargo fmt --all -- --check`, and `git diff --check`.
- [x] S-033 Complete interactive slash-command behavior audits for export,
      import, share, trust, login/logout, new/resume, fork/clone, tree, and
      reload; each command needs a real terminal or fixture transcript. The
      live tmux fixture covers `/help`, `/export`, successful and missing-file
      `/import`, dry-run `/share`, project `/trust` plus `/reload`, no-OAuth
      `/login`, provider `/logout`, `/name`, `/copy`, `/new`, the `/resume`
      picker with keyboard selection and transcript rehydration, `/fork`,
      `/clone`, `/tree`, and alternate-screen/cursor cleanup. It also exposed
      and fixed the first-hit terminal capability-cache deadlock caused by
      holding a read lock while acquiring the write lock. Evidence (live/unit):
      `cargo test -p pi-coding-agent --offline --test
      interactive_slash_pty --quiet` (1 passed), `cargo test -p
      pi-coding-agent --offline --lib interactive:: --quiet` (37 passed),
      `cargo test -p pi-tui --offline
      terminal_image::tests::uncached_capability_detection_releases_read_lock_before_write
      --quiet` (1 passed), `cargo check -p pi-coding-agent --offline`,
      `cargo fmt --all -- --check`, and `git diff --check`. The broader S-056
      command matrix remains open.
- [x] S-034 Finish ConfigSelector project/global inheritance, package pattern
      toggles, search/navigation, write-scope persistence, and close behavior
      against the upstream component. The final audit aligns local package
      sources across global/project bases, creates project-relative package
      overrides, recognizes metadata-base top-level patterns, preserves the
      upstream `autoload: false` empty-filter inheritance rule, and removes
      empty project override objects when cycling back to inherit. Evidence:
      `cargo test -p pi-coding-agent --offline --lib
      interactive::config_selector --quiet` (11 passed), `cargo test -p
      pi-coding-agent --offline --lib interactive:: --quiet` (36 passed),
      `cargo test -p pi-coding-agent --offline --test config_selector_pty
      --quiet` (1 passed), `cargo check -p pi-coding-agent --offline`,
      `cargo fmt --all`, and `git diff --check`.
- [x] S-035 Add PTY snapshot/golden tests for ConfigSelector rendering,
      resizing, glyph probes, keyboard navigation, and settings writes. Added
      `crates/pi-coding-agent/tests/config_selector_pty.rs`, which drives the
      real `pi config --approve` binary through tmux, checks the visible global
      snapshot and Unicode footer, resizes the pane, navigates/toggles global
      and project rows, verifies both settings files, and inspects raw
      alternate-screen/cursor cleanup sequences. The resize boundary now
      invalidates `pi-tui::Tree` differential state before the next redraw.
      Evidence: `cargo test -p pi-coding-agent --offline --test
      config_selector_pty` (1 passed). The broader slash-command PTY matrix
      remains S-056.
- [x] S-036 Complete project-trust safety matrix for all commands and resource
      loaders, including saved trust, default trust, `-a`, `-na`, and prompts.
      Trust resolution now runs before settings/resource construction in print,
      JSON, RPC, interactive, config, and package entry points, with explicit
      overrides taking precedence over saved decisions and global
      `defaultProjectTrust` values. Interactive `ask` startup prompts before
      raw mode and persists the selected decision; headless `ask` remains
      untrusted. The trust store now uses a sidecar create-exclusive lock for
      read/modify/write operations. Evidence (live/unit): `cargo test -p
      pi-coding-agent --offline --test cli_trust --quiet` (7 passed, including
      saved/default trust, JSON startup, `-a`/`-na`, and a real tmux prompt),
      `cargo test -p pi-coding-agent --offline --test cli_commands --quiet`
      (28 passed), `cargo test -p pi-coding-agent --offline --lib
      core::project_trust --quiet` (7 passed, including all resource markers,
      ancestor lookup, and concurrent writes), `cargo check -p
      pi-coding-agent --offline`, `cargo fmt --all -- --check`, and `git diff
      --check`.

### S1-E — RPC behavior and concurrency

- [x] S-037 Refactor RPC input handling so abort, steer, follow-up, and state
      requests can be received while a prompt is streaming, matching upstream
      concurrency and response ordering. (unit; mock) Prompts now run in a
      detached Tokio worker with ordered event/completion delivery while the
      JSONL loop continues reading control/state commands. Verified with
      `cargo test -p pi-coding-agent modes::rpc::tests --offline` (25 passed),
      `cargo test --workspace --offline`, and a faux-provider JSONL smoke run
      showing prompt preflight → get_state → stream event/agent_settled →
      abort response.
- [x] S-038 Verify queue drain points, one-at-a-time/all modes, cancellation,
      pending counts, steering precedence, and follow-up precedence with live
      streamed transcripts. (unit; mock) RPC tests cover both queue batch
      modes, turn-boundary precedence, pending-state reporting, detached abort
      cancellation, live mode changes, and post-settlement abort ordering.
      The faux stream supplies multiple deterministic turns for queued
      follow-ups. Verified with `cargo test -p pi-coding-agent
      modes::rpc::tests --offline` (25 passed) and `cargo test --workspace
      --offline`.
- [x] S-039 Make retry preserve first-attempt deltas, retry status events,
      final usage/stop reason, abort-retry, and response persistence exactly.
      (unit; mock) Retry attempts retain first-attempt deltas and terminal
      messages, emit upstream-compatible `auto_retry_start`/
      `auto_retry_end` events, preserve final usage and stop reason, accept
      `abort_retry` while detached, and persist intermediate failures while
      keeping failed retries out of the live context. Verified with
      `cargo test -p pi-agent --offline rich_agent::tests -- --nocapture`
      (7 passed), `cargo test -p pi-coding-agent --offline
      modes::rpc::tests -- --nocapture` (29 passed), and
      `cargo test --workspace --offline`.
- [x] S-040 Apply all settings values (reserve/keep tokens, retry provider
      options, queue modes, transport, model) in RPC runtime behavior instead
      of using only defaults or approximations. (unit; mock) RPC runtime
      construction now derives configured compaction reserve/retention,
      provider timeout/retry/delay settings, HTTP/WebSocket transport
      settings, thinking budgets/levels, model selection, and queue modes;
      prompt and compaction requests receive the same provider defaults while
      preserving request-local overrides. Verified with `cargo test
      -p pi-coding-agent --offline modes::rpc::tests -- --nocapture` (30
      passed) and `cargo test --workspace --offline`.
- [x] S-041 Audit RPC abort vs abort-bash lifecycle, terminal events, and
      session records under simultaneous prompt/tool activity. (unit; mock)
      `abort` now targets only the agent/retry signals, while standalone bash
      tasks run concurrently, `abort_bash` interrupts silent or active
      processes, and bash records defer until agent settlement to preserve
      message ordering. RPC now emits upstream lifecycle, message terminal,
      turn, and tool execution events. The rich loop continues after
      non-terminating tool batches and propagates abort into bash tools.
      Verified with `cargo test -p pi-agent --offline
      rich_agent::tests -- --nocapture` (8 passed), `cargo test
      -p pi-coding-agent --offline modes::rpc::tests -- --nocapture` (34
      passed), and `cargo test --workspace --offline`.
- [x] S-042 Produce golden transcripts for every RPC command and event type,
      including switch/fork/clone, queue modes, compaction, export, and all
      error responses. (unit; mock; live) Added deterministic command and
      event fixtures at `crates/pi-coding-agent/tests/fixtures/rpc/`, covering
      every RPC command, core/session lifecycle event, switch/fork/clone,
      queue modes, compaction, export, malformed commands, and failure
      responses. The dispatcher now converts unexpected command/task errors to
      RPC failures; standalone bash emits incremental `bash_execution_update`
      records. Verified with `cargo test -p pi-coding-agent --offline
      rpc_command_golden_transcript_matches_fixture`, `cargo test
      -p pi-coding-agent --offline rpc_event_golden_transcript_covers_wire_event_types`,
      `cargo test -p pi-coding-agent --offline
      malformed_rpc_lines_emit_parse_failures`, the full RPC test module, and
      a live `--mode rpc` bash smoke showing update-before-response ordering.

### S1-F — pi-server/pi-client auxiliary library parity

- [x] S-043 Port the upstream `testing/service.ts`, test client, deferred
      helpers, and test-server fixtures. Evidence: unit/mock — the 55-test
      `pi-server` suite covers deferred operations, test service/runtime,
      snapshots, and the local Unix test client.
- [x] S-044 Run the complete server protocol/service conformance suite,
      including malformed frames, handshake errors, snapshots, and lifecycle
      events. Evidence: unit/live-local — 32 expanded `server_e2e` cases and
      4 reconnect lease cases pass; strict server clippy and formatting also
      pass.
- [x] S-045 Port client reconnect/backoff and connection-state listener
      behavior, including in-flight request failure and replay rules. Evidence:
      mock/socket — `cargo test -p pi-client --offline --test auxiliary_parity`
      passed 7 deterministic lifecycle fixtures.
- [x] S-046 Complete client session lease acquire/release/reconcile,
      exclusive-attach, snapshot reconciliation, and detach-on-close behavior.
      Evidence: mock/socket — the same 7-test auxiliary parity suite.
- [x] S-047 Complete client dispose semantics, request timeouts, and transport
      shutdown/error mapping. Evidence: mock/socket — timeout tombstones,
      cancellation, disposal, and late-response fixtures passed in the same
      suite.
- [x] S-048 Add the transport-factory abstraction and every upstream transport
      option beyond the Unix implementation. Evidence: unit/mock — boxed-future
      transport factory and fake/fragmented Unix transport fixtures passed;
      strict pi-client clippy was clean.
- [x] S-049 Add reconnect/lease-churn/session-close end-to-end tests over a
      real socket with deterministic timing seams. Evidence: live — the same
      4-test `reconnect_lease_e2e` suite drives `PiServer` through
      `UnixListener` and `PiClient`/`UnixTransportFactory`; formatting and diff
      checks passed.

### S1-G — pi-tui and terminal parity

- [x] S-050 Complete cell-dimension querying/updating and use measured
      dimensions in image sizing rather than fixed defaults. (unit) Raw Unix
      stdin now flows through `StdinBuffer`; interactive/config loops pass the
      complete response to `Tree::consume_cell_size_response` before key
      dispatch, preserving following input. Verified with
      `cargo test -p pi-tui --offline` (186 passed) and
      `cargo test --workspace --offline`.
- [x] S-051 Add capability-matrix tests for Kitty, Ghostty, WezTerm, Warp,
      iTerm2, VS Code, Alacritty, JetBrains, screen, tmux, Windows Terminal,
      and unknown terminals. Evidence: unit/mock — pi-tui capability matrix
      fixture passed in the 203-test offline suite.
- [x] S-052 Complete Editor IME/selection/kitty-event edge behavior and
      bracketed-paste parity from the upstream fixtures. Evidence: unit — the
      kitty-release, shifted-printable, and bracketed-input fixtures passed.
- [x] S-053 Complete autocomplete debounce, cancellation, marked-input,
      slash/path provider, and selection-application parity. Evidence: unit —
      deterministic debounce/flush/cancel and selection fixtures passed.
- [x] S-054 Complete SettingsList callback, disabled-row, selection, and
      persistence semantics. Evidence: unit — disabled-row, duplicate-filter,
      submenu callback, selection, and persistence fixtures passed.
- [x] S-055 Complete marked/Markdown edge parity and renderer snapshot coverage
      for all upstream block shapes. Evidence: unit/mock — marked edge,
      streaming-fence/math, autolink/OSC8, and ragged-table fixtures passed.
- [x] S-056 Add PTY end-to-end coverage for the full interactive slash-command
      matrix, resize/raw-mode cleanup, alt-screen restoration, and terminal
      feature probes. Evidence: live — the 3-case `interactive_full_matrix`
      and 1-case `interactive_slash_pty` suites passed under tmux with exact
      `stty` and ANSI assertions.
- [x] S-057 Add cross-platform terminal capability and cleanup checks for
      Windows console, Unix terminals, tmux, and nested alternate screens.
      Evidence: unit/mock — the capability matrix, tmux forwarding, nested
      alt-screen, and conservative cleanup fixtures passed in the 203-test
      suite.

### S1-H — evals, fixtures, packaging, and final evidence

- [x] S-058 Capture usage/cost tokens from subprocess session JSONL in pi-evals
      so evaluation metrics match the upstream harness. Evidence: mock/unit —
      `/home/mustbearnold/.cargo/bin/cargo test -p pi-evals --offline --quiet`,
      `--test session_usage`, and the faux smoke fixture recorded input 1246,
      output 20, total 1266 in `runs.jsonl`.
- [x] S-059 Make the extension scenario scorable under faux, or provide the
      same deterministic extension fixture/diagnostic contract as upstream.
      Evidence: unit/mock —
      `/home/mustbearnold/.cargo/bin/cargo test -p pi-evals --offline --test extensions --quiet`
      and the faux extension fixture test/diagnostic contract (`schema 1:
      extension-authoring`) passed.
- [x] S-060 Add provider/CLI exit-code and output-format matrix checks to the
      parity suite, not only smoke checks. Evidence: unit/mock — all 8 CLI
      cases pass in `node scripts/parity-suite.mjs`.
- [x] S-061 Add golden RPC transcript fixtures and byte-level session fixtures
      for v1/v2/v3/v4 migration and current writes. Evidence: unit/mock — 2
      RPC transcripts and 5 session fixtures pass the release matrix.
- [x] S-062 Add settings/auth/models.json and package-resource on-disk golden
      fixtures, including unknown-key preservation and lock/retry behavior.
      Evidence: unit/mock — storage/resource matrices and 9 focused checks
      pass with declared unknown-key paths.
- [x] S-063 Add full provider/adaptor fixture execution to the release parity
      suite with network-free mock servers and explicit live smoke cases.
      Evidence: mock — 51 provider/API variants pass the offline provider
      matrix; a credentialed live smoke case is explicitly declared not-run,
      with no live pass claimed.
- [x] S-064 Port telemetry schema conformance tests and include them in the
      release gate. Evidence: unit/mock — telemetry schema plus 2 focused
      tests pass, including 1 AI span, 11 harness spans, and 4 mutations.
- [x] S-065 Synchronize README, per-crate TODOs, session reports, and PLAN with
      the final ledger state and remove stale historical claims. Evidence
      (mock): `README.md`, `PLAN.md`, `HANDOFF.md`, all crate `TODO.md` files,
      and the dated session/reviewer reports now distinguish current claims
      from historical snapshots; the synchronization check recorded
      `Conversion progress: 97.59% (162/166; 4 open)` before this row was
      counted, and the current result after it is
      `Conversion progress: 98.19% (163/166; 3 open)`. The stale-claim audit
      command
      `rg -n -i 'not yet (port|wired|implemented)|no dedicated checklist ID|ledger hole|source inventory is complete' README.md PLAN.md HANDOFF.md docs/session-10-report.md docs/session-11-report.md docs/reviewer-session-1.md docs/reviewer-session-2.md crates/*/TODO.md`
      returns only explicitly negated/superseded wording; the historical
      HANDOFF checkpoint sections are explicitly labeled `Historical
      checkpoint`. Verified with `node scripts/conversion-progress.mjs`
      (97.59% at the pre-row check) and `git diff --check`.
- [x] S-066 Freeze the final denominator after S-001, run the full source/TODO
      audit, and record the final 100.00% evidence only when no open or
      unclassified task remains. Evidence (unit/mock): the Rust
      `conversion_audit -- all` binary validates the exact 166-ID universe,
      reports `Conversion progress: 100.00% (166/166; 0 open)`, reports zero
      source-audit blockers, and enforces a zero JS/TS source census outside
      `target/`, `.git/`, and the upstream oracle. `cargo check --workspace
      --offline --all-targets`, formatting, diff, focused extension/package
      tests, and the full 507-test coding-agent library target pass.

---

## Conventions

- Track each task as Done only with evidence: tier + exact command/fixture.
- After each committed task/group: push immediately (standing rule).
- Tasks roughly: ~40 pure ports of pinned upstream files, ~30 audit-then-close,
  ~20 tests/verification, ~10 process/gates.
- When a task's "upstream file" is named, pin it to commit 5cd93f688aaab89dbb6dfa4aca535f21796ae185 (v0.84.2).

## Interactive hidden-command parity checkpoint — 2026-08-26

No numbered conversion row changed: the `166/166` Cargo-native conversion
ledger remains the historical source/conversion measure, while the active
behavioral acceptance rows live in `docs/EXHAUSTIVE-PARITY-INVENTORY.md`.
This checkpoint closes the requested interactive implementation slice in the
working tree.

Implemented in the assigned scope:

- `/debug` now writes `config::get_agent_dir()/pi-debug.log`, matching the
  upstream `getDebugLogPath()` equivalent, with ISO-8601/RFC3339 UTC
  timestamps, terminal dimensions, bounded rendered-line diagnostics, and
  Agent-message JSONL.
- `/arminsayshi` and `/dementedelves` are hidden exact no-argument commands
  with Rust-native, width-safe components. Animation uses render-time bounded
  `Instant` state; no task or interval is spawned.
- OpenCode plus a case-insensitive `kimi-k2.5` model id triggers the Rust
  Daxnuts component. Its embedded `DAX_HEX` is the exact 6,144-character
  payload from pinned upstream `daxnuts.ts`; truecolor half-block rendering
  uses actual ESC bytes and has a non-empty-image regression test.
- `BUILTIN_SLASH_COMMANDS` dispatch is exhaustive over `SlashKind`; the
  defensive `not wired`/`Unsupported` catch-all is absent.

Evidence (unit/live):

```text
/home/mustbearnold/.cargo/bin/cargo check -p pi-coding-agent --offline
  exit 0
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib interactive::easter_eggs -- --test-threads=1
  6 passed; 0 failed
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib modes::interactive::tests::debug_timestamp_matches_upstream_iso_shape -- --exact --test-threads=1
  1 passed; 0 failed
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib interactive::interactive_tests::parse_submit_executes_hidden_commands_without_publishing_them -- --exact --test-threads=1
  1 passed; 0 failed
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test interactive_slash_complete_pty -- --test-threads=1
  4 passed; 0 failed
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test interactive_full_matrix -- --test-threads=1
  7 passed; 0 failed
/home/mustbearnold/.cargo/bin/cargo clippy -p pi-coding-agent --offline --all-targets -- -D warnings -A clippy::invalid_regex -A clippy::needless_update -A clippy::drop_non_drop
  exit 0
/home/mustbearnold/.cargo/bin/rustfmt --edition 2021 --check <five scoped interactive files>
  exit 0
git diff --check
  exit 0
```

The exact payload comparison reports Rust/upstream lengths `6144/6144`, equal
payloads, and SHA-256
`4a1df9e4bdd8ecbf6beb4ddc6c7dfa6b80a16f0ff6e18fb9e0139d415ad59f1d` for both.
The unmodified strict clippy command still reports four unrelated existing
diagnostics in `core/changelog.rs`, `core/extensions/integration.rs`, and
`modes/rpc.rs`; those files were not changed because they are outside the
interactive assignment.

## Current parent verification — 2026-08-29 — session-runtime cwd guard

The parent gate verified the new `core::agent_session_runtime` session
replacement boundary. Five focused tests pass, including rejection of a
missing stored cwd before teardown and propagation of `previous_session_file`,
and the coding-agent package check and strict all-target clippy pass. The
import path also validates its effective cwd before replacing the active
session. This promotes evidence for SES-009 and SES-012 only; complete
session, restart, malformed-input, process, and interactive return matrices
remain open.

## Current parent verification — 2026-08-29 — package-wide parity wave

After the session-runtime gate, the parent serialized the remaining package
matrices. `pi-tui` passed 380 library tests plus every integration target and
strict clippy; `pi-ai` passed 433 library tests plus every integration target,
the model-catalog parity target (7/7), and strict clippy; and
`pi-coding-agent` passed 809 library tests plus every integration target,
package check, and strict clippy. Stable rustfmt, scoped diff checks, and the
trailing-whitespace scan passed over all three package scopes.

These gates strengthen evidence only. No row is promoted from partial/open
without its own complete behavior, cancellation/error, process/live, and
visual evidence. The authoritative dashboard remains whole-product
behavioral 30/318 (9.43%), with TUI overall 0/52.

The follow-on workspace all-target test matrix, strict workspace clippy, and
optimized release build pass on the same tree. The installed `pi-rust`
launcher resolves to the rebuilt `target/release/pi` binary and reports
`pi 0.84.2`. This is release/build evidence only; row-specific live,
process, platform, and visual boundaries remain governed by the acceptance
register.

## Latest serialized verification — 2026-08-29 — provider and harness follow-up

The Qwen Token Plan model-derived base-URL loopback dispatch fixture passed
1/1 with the actual provider closure, expected auth header, and streamed
completion; `pi-ai` check and strict all-target clippy pass. The current
`pi-agent` harness/environment tree passed 366 tests across all targets and
strict clippy. These are evidence gates only; no parity row or percentage was
promoted, and live vendor, platform, recovery, and complete TUI
visual/interaction boundaries remain open.

The same wave passed 17/17 focused alt-screen TUI tests, 35/35
OpenAI-compatible handoff tests plus 5/5 cross-provider fixtures, and the
noninteractive missing-session-CWD regression 1/1. Combined package check,
strict clippy, stable formatting, and scoped diff checks pass; no row is
promoted from focused evidence alone.

## Latest serialized verification — 2026-08-29 — loopback routing and SSE

The SSE parser focused suite passes 13/13. OpenAI Responses model-derived
base-URL routing passes its real loopback stream test, and the CLI-035 process
fixture passes 1/1 after proving the provider-visible `AGENTS.md` difference
under `--no-context-files`. The post-fix package matrices pass 441 pi-ai
library tests and 818 pi-coding-agent library tests plus all integration
targets; package checks and strict clippy pass. These gates strengthen
evidence only: live vendor, platform, recovery, and complete TUI
visual/interaction boundaries remain open.

## Latest serialized verification — 2026-08-29 — TUI autocomplete, Anthropic edge, and cross-project session

The current package gates pass 386 pi-tui library tests, 442 pi-ai library
tests, and 821 pi-coding-agent library tests, with every package integration
target green; package check/clippy and the full workspace all-target,
workspace clippy, and optimized release build also pass. The TUI slice adds
mixed slash-command autocomplete, grapheme-safe cursor clamping, and
cancellable file-search pipe draining. The Anthropic slice covers the
thinking-budget zero/default edge and repair/abort/image-request paths. The
cross-project session PTY passes both cancel-without-fork and explicit-yes
fork-with-parent/transcript cases. This strengthens implementation/evidence
only; visual/emulator, live-vendor, platform, recovery, and row-complete
boundaries remain open.

## Latest serialized verification — 2026-08-30 — OpenRouter images and session-id warning

The latest focused OpenRouter image suite passes 13/13, including payload-hook
mutation through a real loopback HTTP request, header precedence, retry
cancellation, and malformed image-data handling. The interactive
missing-session-id warning regression passes 1/1 with the upstream diagnostic;
the full pi-ai matrix passes 443 library tests and all integration targets,
and pi-coding-agent passes 822 library tests and all integration targets.
Package check and strict clippy pass, as do stable formatting and whitespace
checks. These gates strengthen implementation/evidence only; live-vendor,
visual/emulator, platform, recovery, and row-complete boundaries remain open.

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
open. The acceptance register and dashboard remain unchanged at 318/318
indexed/scored, TUI overall 0/52, non-TUI overall 30/266, and whole-product
behavioral parity 30/318 (9.43%). The rebuilt release binary exposes both
providers through `--list-models glm-5.2` with their respective synthetic
API-key environment variables.

## Rust-idiom campaign note — 2026-08-30

The typed-error campaign (see `PLAN.md`) does not change any ledger row. Its
checkpoints keep the checker green:
`Conversion progress: 100.00% (166/166; 0 open)`, 0 audit blockers, 0
workspace JS/TS source files. Phase 1 converted pi-evals; Phase 2.1 brought
pi-server under the hard lint gate with poison-tolerant locking and no
behavioral change (workspace matrix 2,805 tests passed).

Phase 2.2 brought pi-client under the hard lint gate (poison-tolerant
locking, no production unwrap/expect); ledger rows unchanged.

Phase 2.3a brought pi-ai under the hard lint gate (poison-tolerant locking,
LazyLock regexes, guarded invariants); ledger rows unchanged.

Phase 2.3b introduced the typed PiAiError for the pi-ai auth/OAuth surface;
ledger rows unchanged.

Phase 2.4 brought pi-agent under the hard lint gate; ledger rows unchanged.
