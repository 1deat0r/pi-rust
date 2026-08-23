# Pi → pi-rust conversion handoff

Date: 2026-08-24 (Pacific/Auckland)

## Where the work stopped

The requested progress percentage is now based on the exhaustive conversion
ledger, not the original 100-item queue:

```text
43.98% = 73 completed / 166 total tasks
93 tasks remain open
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

The latest local checkpoint is `13da24c` (`feat(images): align read
processing and file attachments`). The image/read milestone is implemented,
verified, and committed. The immediate push was blocked because the HTTPS
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

Current status at pause: branch `main`, no cargo/rustc process still running,
progress checker reports `43.98% (73/166; 93 open)`, with the image checkpoint
ahead of the remote.

## Verification already completed

These checks passed during the session:

```bash
/home/mustbearnold/.cargo/bin/cargo check --workspace --offline
/home/mustbearnold/.cargo/bin/cargo test --workspace --offline
/home/mustbearnold/.cargo/bin/cargo test -p pi-agent --offline tools::image
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline run::tests
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline image_file_argument_is_attached_and_normalized
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

The image/read parity milestone (#32 / S-020 audit) is complete:

- Source audit confirmed that upstream has no separate model-facing `image`
  tool: `harness/tools/image.ts` is the shared detector/base64 helper used by
  `read`.
- `pi-agent` now ports the shared detector, BMP→PNG normalization, 2000×2000
  and 4.5 MB processing policy, JPEG fallback, conversion/dimension hints,
  and the `blockImages` provider filter.
- `read` uses the processing settings in one-shot, JSON, interactive, and RPC
  paths. `@file` arguments now attach processed image blocks and tagged text
  references in one-shot and JSON modes; RPC rejects them with the upstream
  unsupported-mode diagnostic.
- Focused coverage includes six image tests, run-path helper coverage, and a
  binary CLI session test proving BMP input is persisted as `image/png` with
  its file reference. The full workspace test suite is green.
- The earlier RPC checkpoint remains in `a5e161c`; its fixtures are under
  `crates/pi-coding-agent/tests/fixtures/rpc/`.

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
   image/read checkpoint is `13da24c` locally and is not remote yet.
2. Finish one-shot print-path auto-compaction and its binary/session fixture
   tests (#33–34 / S-025).
3. Audit client reconnect/timeouts and the remaining TUI/config-selector
   interactive behavior.
4. Keep `CONVERSION-LEDGER.md`, `PLAN.md`, and this handoff synchronized;
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

The operator has requested commit + push after each checkpoint. The image/read
commit was made and its push was retried; GitHub rejected it because no HTTPS
username is available. Retry the push whenever credentials are available and
report the blocker honestly. Before continuing, inspect `git status`, read
this handoff, run the progress checker, and treat all existing dirty changes
as user-owned work.
