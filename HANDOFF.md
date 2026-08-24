# Pi → pi-rust conversion handoff

Date: 2026-08-24 (Pacific/Auckland)

## Where the work stopped

The requested progress percentage is now based on the exhaustive conversion
ledger, not the original 100-item queue:

```text
55.42% = 92 completed / 166 total tasks
74 tasks remain open
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

The latest committed local checkpoint is the panic-safe telemetry settlement
slice after the AgentTool harness, lifecycle/termination, schema-validator,
RPC runtime, update/version, and model-catalog checkpoints. It includes
synchronized ledger/plan/handoff docs. Pushes remain blocked because the HTTPS
remote requires GitHub credentials.
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
`55.42% (92/166; 74 open)`. Preserve the pre-existing untracked `AGENTS.md`;
all other changes in this checkpoint are intentional local work ahead of the
remote.

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
/home/mustbearnold/.cargo/bin/cargo fmt --all -- --check
git diff --check
node scripts/conversion-progress.mjs
```

A full `cargo test --workspace --offline` passed after the image/read changes,
including 162 `pi-agent` unit tests, the coding-agent integration targets, 186
`pi-tui` unit tests, and all workspace doctests.

The latest full gate after the panic-safe telemetry slice passed: 174
`pi-agent` tests, 286 `pi-ai` unit tests, 445 `pi-coding-agent` unit tests plus
all integration targets (including the malformed-call fixture), 186 `pi-tui`
unit tests, and all workspace doctests.

## Last code change

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

1. Retry `git push origin main` whenever GitHub credentials are available and
   report the HTTPS credential blocker if it persists.
2. Continue with S-021/S-022/S-023, then the full regular/fullscreen #61/#62 swap
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

The operator has requested commit + push after each checkpoint. The
auto-compaction commit was made and its push was retried; GitHub rejected it
because no HTTPS username is available. Retry the push whenever credentials
are available and report the blocker honestly. Before continuing, inspect
`git status`, read this handoff, run the progress checker, and treat all
existing dirty changes as user-owned work.
