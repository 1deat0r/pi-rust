# Pi → pi-rust conversion handoff

Date: 2026-08-24 (Pacific/Auckland)

## Where the work stopped

The requested progress percentage is now based on the exhaustive conversion
ledger, not the original 100-item queue:

```text
58.43% = 97 completed / 166 total tasks
69 tasks remain open
```

The authoritative ledger is [CONVERSION-LEDGER.md](CONVERSION-LEDGER.md).
[PLAN.md](PLAN.md) displays the same value. Recalculate it after every ledger
edit with:

```bash
node scripts/conversion-progress.mjs
```

The denominator is intentionally provisional until S-001, S-002, and S-066
finish the source inventory, stale-document reconciliation, and final ledger
freeze. Do not claim 100% before the final clean-room audit.

## Important working-tree state

The latest implementation checkpoint is the committed S-026 CLI session-routing
slice `711a25e`, after the parity checkpoint `eaa36ba` and the committed S-029
install-telemetry slice `3d6f1fc` and the committed
S-030 interactive cache-notice slice `7356dd3`,
`ef640ce`,
the partial legacy-session
integration slice, after committed startup timing (`869ae6d`) and
the committed compiled-binary self-update contract (`db97b89`) and interactive
turn harness ownership
checkpoint (`0cd9d03`) and documentation-hook checkpoint (`d56d8ba`), JSON-mode
harness ownership (`3fb8049`), print-path harness,
lifecycle/termination, schema-validator, panic-safe telemetry, RPC runtime,
update/version, and model-catalog checkpoints. The JSON-mode implementation is
`dd7a568`; its follow-up preserves the successful RPC golden envelope while
forwarding terminal JSON provider errors. GitHub authentication is now
configured through `gh`, and the accumulated branch has been pushed.
Preserve existing changes; do not use `git reset --hard`, `git checkout --`, or
broad revert commands.

The worktree is very large because the baseline was already heavily changed
and `cargo fmt --all` reformatted many Rust files. The meaningful current
additions/renames include:

- `CONVERSION-LEDGER.md` replaces the old `NEXT-100.md` tracker.
- `scripts/conversion-progress.mjs` validates task IDs and computes the
  percentage.
- `crates/pi-coding-agent/src/core/version_check.rs` adds the update-check
  seam.
- Runtime/model catalog, RPC, config selector, TUI, provider, session, CLI,
  and parity work is spread across the modified crates.

Current status at pause: branch `main`, progress checker reports
`58.43% (97/166; 69 open)`. The S-026 implementation checkpoint `711a25e` is
synchronized with `origin/main`. Preserve the pre-existing untracked
`AGENTS.md`.
The README now records the legacy-session integration, CLI session routing,
startup-timing, interactive cache-notice, install-telemetry contracts, and the
synchronized-doc workflow. The pre-existing `AGENTS.md` remains untouched.

The compiled self-update checkpoint was checked immediately after its commit:

- `git rev-parse HEAD`: `db97b89c0c7767ece2154b70a886e3f98fb151e5`
- `git ls-remote origin refs/heads/main`:
  `90a5b931591eaeaea20f1fd9c0d10f72d7614a7b`
- Immediate `git push origin main` failed with the exact blocker:
  `fatal: could not read Username for 'https://github.com': No such device or address`.
- `gh auth status` reports no authenticated GitHub host. The pre-commit hook
  therefore could not sync `.github/repository-description.txt` to GitHub.
  That historical push was blocked before GitHub authentication was repaired;
  the accumulated branch was later pushed at the parity checkpoint below.

The S-030 checkpoint was retried immediately after commit and remains blocked:

- `git rev-parse HEAD`: `7356dd37896043b54c554949b7dabec8bd325aae`
- `git ls-remote origin refs/heads/main`:
  `90a5b931591eaeaea20f1fd9c0d10f72d7614a7b`
- `git push origin main` failed with:
  `fatal: could not read Username for 'https://github.com': No such device or address`.
- `gh auth status --hostname github.com`: `You are not logged into any GitHub hosts.`

The startup-timing checkpoint was verified against the remote immediately
after commit:

- `git rev-parse HEAD`: `869ae6de6d451243b511409cf7de545819c55f6b`
- `git ls-remote origin refs/heads/main`:
  `90a5b931591eaeaea20f1fd9c0d10f72d7614a7b`
- Immediate `git push origin main` failed with:
  `fatal: could not read Username for 'https://github.com': No such device or address`.
- `gh auth status` still reports no authenticated GitHub host, so the updated
  repository description remains local only.

## Verification already completed

These checks passed during the session:

```bash
/home/mustbearnold/.cargo/bin/cargo check --workspace --offline
/home/mustbearnold/.cargo/bin/cargo test --workspace --offline
/home/mustbearnold/.cargo/bin/cargo test -p pi-agent --offline tools::image
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline run::tests
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline image_file_argument_is_attached_and_normalized
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test cli_print_parity
/home/mustbearnold/.cargo/bin/cargo test -p pi-client --offline
/home/mustbearnold/.cargo/bin/cargo test -p pi-server --offline
/home/mustbearnold/.cargo/bin/cargo check --workspace --offline
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline interactive::config_selector
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test config_selector_pty
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline
/home/mustbearnold/.cargo/bin/cargo test -p pi-tui --offline
/home/mustbearnold/.cargo/bin/cargo test -p pi-tui terminal_image --offline
/home/mustbearnold/.cargo/bin/cargo test -p pi-tui terminal::tests::cell_size_query_and_response_update_image_dimensions --offline
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent modes::rpc::tests --offline
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent remote_catalog_provider --offline
/home/mustbearnold/.cargo/bin/cargo test -p pi-agent --offline rich_loop_executes_tool_batch_and_emits_execution_events
/home/mustbearnold/.cargo/bin/cargo test -p pi-agent --offline terminate_hints_require_every_parallel_tool_to_opt_in
/home/mustbearnold/.cargo/bin/cargo test -p pi-agent --offline --test tools bash_tool_streams_partial_updates_through_agent_contract
/home/mustbearnold/.cargo/bin/cargo test -p pi-agent --offline --test tools edit_tool_registers_prepare_arguments_before_validation
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline rpc_runtime_control_commands_update_settings_and_state
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib core::version_check::tests
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib core::remote_catalog_provider::tests
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test cli_commands update_
/home/mustbearnold/.cargo/bin/cargo test -p pi-agent --offline --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-telemetry --offline --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-agent --offline tools::validation -- --nocapture
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test tool_contract -- --nocapture
/home/mustbearnold/.cargo/bin/cargo test -p pi-tui --offline --quiet
/home/mustbearnold/.cargo/bin/cargo test --workspace --offline --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test cli_json_mode --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib interactive:: --quiet
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib modes::rpc::tests::rpc_command_golden_transcript_matches_fixture
/home/mustbearnold/.cargo/bin/cargo fmt --all -- --check
git diff --check
node scripts/conversion-progress.mjs
```

The S-030 interactive cache-notice checkpoint is complete in the working tree:

- Interactive mode maintains serialized shadow entries while its JSONL writes
  remain deferred until exit. Cache misses are re-derived and injected after
  matching assistant timestamps, with the upstream 20k-token/$0.10 display
  thresholds and model-switch/idle labels.
- The settings selector now exposes and persists `showCacheMissNotices`.
  Footer usage reads the shadow entries so assistant, tool-result, and
  compaction/summary usage survives context replacement. `/session` now shows
  `Cache Re-billed` tokens, cost, and miss count; auto-compaction, `/clear`,
  new-session, resume, and import reset/reload the cache shadow appropriately.
- Evidence: `cargo test -p pi-coding-agent --offline --lib interactive::`
  (33 passed), `cargo test -p pi-coding-agent --offline --quiet` (455
  coding-agent unit tests plus integration targets),
  `cargo check --workspace --offline`, `cargo test --workspace --offline
  --quiet`, `cargo fmt --all`, and `git diff --check`.

S-030 is closed. The remaining CLI session-routing audit (S-026) and
S-021/S-022 harness ownership work are still open.

The S-029 install-telemetry checkpoint is complete in the working tree:

- `core::telemetry::report_install_telemetry` sends the upstream version query
  and Pi user-agent through a bounded five-second best-effort transport, with
  transient transport/429/5xx retries. `PI_OFFLINE` short-circuits; the
  `PI_TELEMETRY` environment override and default-on `enableInstallTelemetry`
  setting gate the report.
- Interactive startup records the shipped version and launches the report in
  the background only for a fresh/version-changed install boundary. The
  settings selector now exposes `Install telemetry`; the endpoint has a
  `PI_INSTALL_TELEMETRY_URL` test seam.
- Evidence: `cargo test -p pi-coding-agent --offline --lib
  core::telemetry::` (7 passed), `cargo test -p pi-coding-agent --offline
  --quiet` (458 coding-agent unit tests plus integration targets),
  `cargo check --workspace --offline`, and `cargo test --workspace --offline
  --quiet`.

S-029 is closed. The remaining CLI session-routing audit (S-026) and
S-021/S-022 harness ownership work are still open.

The S-029 checkpoint was checked against the remote immediately after commit:

- `git rev-parse HEAD`: `3d6f1fc6dc047e983cdc12d6093b8423cb582441`
- `git ls-remote origin refs/heads/main`:
  `90a5b931591eaeaea20f1fd9c0d10f72d7614a7b`
- Immediate `git push origin main` failed with:
  `fatal: could not read Username for 'https://github.com': No such device or address`.
- `gh auth status --hostname github.com`: `You are not logged into any GitHub hosts.`

Remote parity was restored after GitHub device authentication:

- `gh auth status --hostname github.com`: logged in as `1deat0r` with `repo`
  scope.
- `gh auth setup-git --hostname github.com`: configured the GitHub CLI
  credential helper for the HTTPS remote.
- `git push origin main`: advanced `origin/main` from `90a5b93` to `a1c3e92`.
- `git rev-parse HEAD` and `git ls-remote origin refs/heads/main` both equal
  `a1c3e9268cd74d8992bcbd4c62f995ff20a5382d`.

The next implementation checkpoint must repeat the same commit, push, and
exact-hash verification sequence.

A full `cargo test --workspace --offline` passed after the image/read changes,
including 162 `pi-agent` unit tests, the coding-agent integration targets, 186
`pi-tui` unit tests, and all workspace doctests.

The latest full gate after the partial legacy-session integration checkpoint
passed: 176 `pi-agent` tests, 286 `pi-ai` unit tests, 451 `pi-coding-agent` unit tests plus
all integration targets (including the malformed-call and print-parity
fixtures), 186 `pi-tui` unit tests, and all workspace doctests.

The JSON-mode harness checkpoint also passed the full workspace gate after
restoring the successful RPC golden transcript. The focused JSON integration
test passes both normal faux streaming and the terminal no-key provider error
case, with both cases exiting successfully as required by JSON mode.

The compiled-binary self-update contract checkpoint is now complete locally:

- `pi update --self` retains the upstream latest-release lookup and update
  decision, but a compiled Rust executable reports a non-zero result instead
  of attempting to overwrite itself. The replacement message is centralized
  and unit-tested byte-for-byte, including the package/version target.
- README.md documents rebuilding a source checkout with
  `cargo build --release -p pi-coding-agent` and replacing the installed
  binary through its owning mechanism. S-028 is closed as this intentional,
  user-visible distribution behavior.
- The focused package-command and offline update tests, workspace check/test,
  formatter, diff check, and progress checker are the evidence recorded for
  this checkpoint. GitHub description synchronization is attempted by the
  pre-commit hook when `gh` is authenticated.

The startup-timing compatibility checkpoint is complete in the working tree:

- `core::timings` recognizes the upstream exact `PI_TIMING=1` gate. The binary
  prints a user-facing warning with `/usr/bin/time -p` as the supported
  process-level fallback; the upstream timing namespaces remain an explicit
  Rust distribution non-port.
- The exact-one gate and fallback text are covered by
  `core::timings::tests::matches_upstream_exact_one_gate_and_fallback_text`.
  `PI_TIMING=1 ./target/debug/pi --version` prints the warning before
  `pi 0.84.2`. S-031 is closed; session migration integration, install
  telemetry, cache notices, and harness ownership remain open.

The earlier partial legacy-session integration checkpoint is complete; S-026
was closed by the CLI routing slice `711a25e`:

- Legacy v1/v2/v3 files are atomically converted before interactive session
  inventory and direct RPC switch_session loads. Fork/clone inherit the
  converted v4 source; /import keeps its existing copy-and-convert path.
- The three converter/file-system tests, direct RPC migration test, RPC golden
  transcript, interactive harness regression, CLI continue/resume/fork tests,
  interactive/RPC unit suites, workspace gate, formatter, and diff check are
  the evidence for the completed audit.

The legacy-session checkpoint was verified against the remote immediately
after commit:

- `git rev-parse HEAD`: `ef640ce09d60b158e2062a03bf31e12d7a4e3f74`
- `git ls-remote origin refs/heads/main`:
  `90a5b931591eaeaea20f1fd9c0d10f72d7614a7b`
- Immediate `git push origin main` failed with:
  `fatal: could not read Username for 'https://github.com': No such device or address`.
- `gh auth status` still reports no authenticated GitHub host; local and
  remote parity is not claimed.

## Earlier completed code changes

The one-shot auto-compaction milestone (#33–34 / S-025) is complete in the
working tree:

- The run path now provisions messages and compaction entries in memory,
  evaluates configured thresholds, calls the existing harness summarizer,
  rebuilds provider context from the retained compaction tail, and persists
  the compaction entry in JSONL.
- A binary faux-provider test forces the setting, verifies the second print
  turn continues, and verifies a `"type":"compaction"` JSONL entry. The
  focused print parity file has four passing tests.
- #33, #34, and S-025 are marked complete. The previous image/read checkpoint
  remains in `333ad84`; the earlier RPC fixtures are under
  `crates/pi-coding-agent/tests/fixtures/rpc/`.

The client reconnect/timeout milestone (#54 and #56) is also complete in the
working tree:

- `pi-client` exposes connection lifecycle state/listeners, reconnects through
  a fresh handshake with epochs and snapshot reset, invalidates session handles
  on disconnect, bounds handshake/request waits, ignores late responses for
  timed-out requests, and adds permanent `dispose()` alongside reconnectable
  `close()`.
- Fake Unix-socket tests cover reconnect lifecycle and snapshot refresh,
  handshake timeout, request timeout/late response, and disposal. The focused
  client suite has 4 passing tests; the dependent `pi-server` suite also passes.
- #55 lease reconciliation, #57 transport factories, #58 lease-churn E2E, and
  supplemental S-045/S-047 remain open; this is auxiliary T4 hardening, not a
  claim that the full upstream client library is complete.

The ConfigSelector interactive milestone (#59) is complete locally:

- The selector now supports search/filtering, circular/page navigation, global
  toggles, project inherit/load/unload cycling, inherited-resource indicators,
  package/top-level override persistence, and synchronous settings flushes.
- The focused selector suite has 8 passing tests, including deterministic
  global/project render snapshots; the full coding-agent suite
  has 436 unit tests plus its integration targets, and the full pi-tui suite
  has 186 passing tests. #59/#60 are complete; the focused PTY exercise is
  recorded in S-035, followed by #61/#62 and the remaining terminal probes.

The focused ConfigSelector PTY milestone (S-035) is also complete locally:

- `tests/config_selector_pty.rs` drives the real `pi config --approve` binary
  through tmux, asserts a visible global render snapshot and Unicode footer,
  survives pane resize, navigates/toggles global and project rows, verifies
  both settings files, and checks raw alternate-screen/cursor cleanup.
- A resize event now invalidates `pi-tui::Tree` differential state in both the
  config selector and main interactive loop, fixing the stale-frame behavior
  exposed by the PTY test.
- The focused PTY suite passes one test. The full interactive slash-command
  matrix remains S-056; alt-screen mode switching remains #61/#62.

The next alt-screen hardening checkpoint is also complete locally:

- `TerminalBackend` now exposes a monotonic screen epoch that changes on
  alternate-screen entry/exit, and `Tree` forces a full redraw when the epoch
  changes. This covers overlays or external prompts that temporarily replace
  the active screen without claiming the full regular/fullscreen renderer swap.
- The terminal transition test verifies idempotence and the expected epoch
  sequence; the PTY selector test remains green after the renderer change.

The AgentTool contract/update checkpoint is now complete locally and committed
in the latest checkpoint:

- `edit_tool` registers the upstream `prepareArguments` normalization before
  schema validation. The pinned source audit confirmed the other built-ins do
  not define non-identity prepare shims.
- The rich loop passes a scoped callback into tool execution and forwards
  updates through a channel while sequential/parallel calls run. Callbacks
  are gated after settlement; bash emits an initial update, 100ms-throttled
  output progress, and a final snapshot before `tool_execution_end`.
- Batch termination honors `AgentToolResult.terminate` only when every
  finalized tool opts in. Mixed parallel termination is covered by a focused
  unit test.
- Successful text results omit optional `details`, while error text preserves
  the upstream empty-object details shape. The built-in `ls`, `find`, and
  `grep` paths use the successful shape; read/write/edit/bash preserve their
  existing structured results.
- Parallel completion events are emitted in completion order while durable
  model-facing result messages remain in source order. Immediate preparation
  failures, mutable before-hooks, after-hook overrides, and late callback
  suppression have focused coverage.
- Bash fixtures cover coalesced progress, final truncation/full-output detail,
  and timeout after output. The registered seven-tool coding-agent fixture
  covers malformed read/write/edit/bash/ls/find/grep calls and verifies error
  payloads plus the absence of file mutation. #25–27, S-018, and S-020 are
  complete; S-024 remains open for broader schema-validator parity.

The termination-contract follow-up is included in the latest checkpoint:

- `ToolExecutionEnd` now carries the raw `AgentToolResult`, so lifecycle and
  RPC events preserve `terminate` and all optional result fields. The
  model-facing `ToolResultMessage` remains free of the internal hint.
- RPC prompt persistence correlates tool end events with their later
  tool-result message end and writes `terminate: true` on the JSONL message
  entry. This lets lane recovery reconstruct termination decisions.
- Mixed/all-terminating parallel batches and the RPC/session path are covered
  by the focused rich-loop and RPC suites; S-019 is complete.

The schema-validator parity follow-up is included in the latest checkpoint:

- Tool argument validation now covers local `$ref`, union combinators,
  tuple/constrained arrays, `additionalProperties`/`patternProperties`,
  enum/const, numeric and string bounds, common formats, and nullable optional
  normalization. Primitive coercion remains aligned with the upstream
  `Value.Convert`/plain-schema path.
- The validator fixture set covers these behaviors and the complete workspace
  gate is green; S-024 is complete. Remaining validator work, if discovered by
  future source audits, must be recorded as a new supplemental item rather
  than silently folded into this claim.

The panic-safe telemetry follow-up is included in the latest checkpoint:

- The in-memory telemetry adapter now catches callback panics, settles the
  span as an automatic error unless an explicit status was recorded, resumes
  the original panic, and keeps nested spans’ inner-first settlement order.
  Panic payloads remain opaque and late span operations remain inert.
- The TUI image fallback and Kitty capability fixtures now share their global
  capability lock, removing the workspace-only race seen during the first
  full gate.
- S-023 is complete. The next harness/runtime gaps are S-021 and S-022.

The print-path harness ownership slice is included in the latest checkpoint:

- Configured `AgentHarness` instances now own the rich `Agent`, provider/model
  configuration, tool preparation callbacks, and an in-memory main-lane
  transcript. The one-shot `run.rs` path prompts that harness, performs
  compaction against its transcript, updates Agent state at the compaction
  boundary, and replays the transcript into the durable JSONL session.
- Agent prompt messages are retained exactly once across sequential turns. A
  focused harness fixture proves the configured Agent output is persisted in
  chronological lane order; the four-test print-parity suite and full
  workspace gate remain green.
- This is a partial S-021 checkpoint, not closure: interactive, JSON, JSONL,
  and RPC modes still use their direct loop paths, and secondary lanes plus
  complete event/telemetry lifecycle wiring remain S-021/S-022 work.

The harness lifecycle/telemetry slice is included in the latest checkpoint:

- The configured print-path harness now consumes `HarnessTelemetryContext`
  through an async-safe span boundary, emits `run_start` and `run_end` in
  order, and records a settled `pi.harness.run` span with the required
  session/lane/operation attributes. Session-write failures mark the span
  explicitly as errors.
- The focused harness fixture asserts the exact event sequence, span name,
  required attributes, settled status, and `run_start`/`run_end` span events.
  The full workspace gate remains green; the shared mode bridge described
  below now applies the same boundary to the remaining loops. This is partial
  S-022 because golden wire checks remain.

The shared mode lifecycle bridge is included in the latest checkpoint:

- `run_with_harness_lifecycle` now wraps the JSON mode, interactive turns,
  detached RPC prompt workers, and synchronous RPC prompt execution. Existing
  mode-specific events remain in their established order/payload shape, while
  each run receives the same ordered harness lifecycle and async span boundary.
- The adapter fixture asserts `run_start`, nested event, `run_end` ordering and
  the required operation attributes; RPC’s focused 39-test suite and print
  parity remain green. The full workspace gate passes with 176 pi-agent tests.
  This is still partial S-022: mode-specific golden lifecycle envelopes,
  persistence, and secondary-lane assertions remain.

The JSON-mode harness ownership slice is included in the latest checkpoint:

- `--mode json` now creates a memory-backed `AgentHarness`, configures its
  registered tools/model/system prompt, and emits the harness-captured rich
  message updates. Terminal provider errors are preserved as JSON
  `message_update` events; terminal `done` remains omitted from RPC's existing
  successful golden transcript.
- `cli_json_mode` passes both its faux success case and its no-key terminal
  error case; the full workspace gate remains green. This is partial S-021 and
  S-022: interactive/JSONL/RPC full harness ownership, lifecycle goldens,
  persistence, and secondary lanes remain open.

The interactive turn harness ownership slice is the next checkpoint:

- Interactive `stream_turn` now creates a configured memory-backed
  `AgentHarness`, seeds it from the current transcript, preserves all built-in
  tool preparation callbacks, and forwards rich stream updates to the existing
  TUI callback. The runtime continues to own durable JSONL persistence and
  session-switch behavior.
- The focused interactive harness test and the full 446-test coding-agent
  suite plus the full workspace gate pass. This remains partial S-021/S-022:
  JSONL/RPC full harness
  ownership, lifecycle goldens, persistence, and secondary lanes remain open.

The follow-up bash harness integration is included in the latest checkpoint:

- The registered bash tool now runs through `StdExecutionEnv` and
  `execute_shell_with_capture`, preserving structured truncation metadata and
  full-output temp-file paths while retaining the legacy direct `run_bash`
  API. The focused full-output, shell-capture, abort, coalescing, and timeout
  fixtures all pass; remaining harness work is tracked under S-021/S-022/S-023.

The RPC runtime audit is now complete locally:

- A direct test sends `set_auto_compaction`, `set_auto_retry`,
  `set_steering_mode`, and `set_follow_up_mode`, then verifies live flags,
  persisted settings, queue modes, and the `get_state` response. Existing
  stream/compaction/retry/provider-setting and queue-drain tests cover the
  downstream behavior.
- #88 is marked complete. The update/version and model-catalog slice closes
  #89–90; S-016/S-017 remain open for atomic-write and broader provider-shape
  fixture expansion.

The update/version and model-catalog checkpoint is now complete locally:

- `pi update` now performs the upstream latest-release plan with normalized
  metadata, semver prerelease/build comparison, transient retry behavior,
  current-version/`--force` handling, and a truthful compiled-binary
  self-update fallback.
- `pi update --models` refreshes built-in providers concurrently within the
  upstream 15-second bound, retries transient HTTP statuses, persists
  ETag/Last-Modified/freshness state, handles 304/404/501, and keeps the
  `PI_MODEL_CATALOG_URL` seam for mock tests. The user-facing success/error
  lines now match upstream.
- Version tests cover offline and `PI_SKIP_VERSION_CHECK` short-circuiting,
  normalized release JSON, and malformed endpoint failures. Catalog tests
  cover persisted success and three-attempt transient failure behavior; the
  binary update test covers offline self-update failure.
- #89 and #90 are marked complete. The supplemental S-016/S-017 rows remain
  open deliberately.

## Major parity work already present

The current source includes substantial ports beyond the original baseline:

- exhaustive conversion ledger and progress checker;
- provider/auth/OAuth and Codex WebSocket work;
- remote model catalog refresh, freshness, ETag/304, offline, and runtime
  model merge behavior;
- CLI flags, JSON mode, print-mode sequencing/error behavior, resource
  loading, project trust, diagnostics, telemetry, sessions, export, and
  footer usage totals;
- RPC queue/abort/steer/follow-up/compaction/retry scaffolding and session
  tree queries;
- config-selector data model/resource producer and a partial interactive
  component;
- TUI word navigation, terminal capability probing, image sizing helpers,
  editor/autocomplete, markdown, and alt-screen foundations;
- server/client/session-backend changes and extensive fixture/tests.

The ledger deliberately keeps several items open because “code exists” is not
the same as proven 1:1 behavior. In particular, do not silently check off
items just because a similarly named Rust module exists.

## Recommended next sequence

1. Continue with the remaining S-026 and S-021/S-022 work, then the full
   regular/fullscreen #61/#62 swap
   work and broader S-056 interactive matrix.
3. Keep `CONVERSION-LEDGER.md`, `PLAN.md`, and this handoff synchronized;
   only mark a task complete with an evidence tier and exact command/fixture.

## Useful source references

- Upstream authoritative clone: `upstream_pi/`
- Upstream pinned target: `5cd93f688aaab89dbb6dfa4aca535f21796ae185`
- Rust cargo binary: `/home/mustbearnold/.cargo/bin/cargo`
- Primary RPC implementation: `crates/pi-coding-agent/src/modes/rpc.rs`
- One-shot path: `crates/pi-coding-agent/src/run.rs`
- TUI terminal/image paths: `crates/pi-tui/src/terminal.rs`,
  `crates/pi-tui/src/terminal_image.rs`, `crates/pi-tui/src/tui.rs`

## Session discipline

The operator has requested commit + push after each checkpoint. GitHub device
authentication and the HTTPS credential helper are now configured; the
accumulated branch is verified at parity with `origin/main` at `711a25e`.
Before continuing, inspect
`git status`, read this handoff, run the progress checker, and treat all
existing dirty changes as user-owned work.
