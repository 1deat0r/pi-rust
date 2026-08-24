# Pi → pi-rust conversion handoff

Date: 2026-08-24 (Pacific/Auckland)

## Where the work stopped

The requested progress percentage is now based on the exhaustive conversion
ledger, not the original 100-item queue:

```text
48.80% = 81 completed / 166 total tasks
85 tasks remain open
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

The latest local checkpoint is the current `HEAD` (`test(config): add PTY
selector lifecycle coverage`) after the client reconnect/timeout hardening.
The one-shot auto-compaction, covered client criteria, selector behavior, and
selector PTY/resize lifecycle are implemented, verified, and committed locally.
Pushes are blocked because the HTTPS remote requires GitHub credentials.
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

Current status at pause: branch `main`, no cargo/rustc process still running,
progress checker reports `48.80% (81/166; 85 open)`, with the local client,
selector, and PTY checkpoints ahead of the remote and no cargo/rustc process running.

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
git diff --check
node scripts/conversion-progress.mjs
```

A full `cargo test --workspace --offline` passed after the image/read changes,
including 162 `pi-agent` unit tests, the coding-agent integration targets, 186
`pi-tui` unit tests, and all workspace doctests.

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

1. Retry `git push origin main` after credentials are available; the current
   client/selector/PTY checkpoint is local and is not remote yet.
2. Continue with #61/#62 alt-screen swap work and the broader S-056
   interactive slash-command matrix.
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
