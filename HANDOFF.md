# Pi → pi-rust conversion handoff

Date: 2026-08-24 (Pacific/Auckland)

## Where the work stopped

The requested progress percentage is now based on the exhaustive conversion
ledger, not the original 100-item queue:

```text
42.17% = 70 completed / 166 total tasks
96 tasks remain open
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

The earlier large port is checkpointed locally in commit `7bace4f`. The current
S-040 implementation and its documentation are still uncommitted at this
handoff point. Preserve existing changes; do not use `git reset --hard`,
`git checkout --`, or broad revert commands.

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
progress checker reports `42.17% (70/166; 96 open)`.

## Verification already completed

These checks passed during the session:

```bash
/home/mustbearnold/.cargo/bin/cargo check --workspace --offline
/home/mustbearnold/.cargo/bin/cargo test -p pi-tui terminal_image --offline
/home/mustbearnold/.cargo/bin/cargo test -p pi-tui terminal::tests::cell_size_query_and_response_update_image_dimensions --offline
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent modes::rpc::tests --offline
/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent remote_catalog_provider --offline
node scripts/conversion-progress.mjs
```

A full `cargo test --workspace --offline` passed after the S-040 RPC settings
changes, including all workspace unit, integration, and doctest targets.

## Last code change

The latest change completed RPC settings parity:

- `rpc.rs`: applies configured compaction, retry/provider, transport, thinking,
  model, and queue settings to normal prompt and compaction execution while
  preserving request-local overrides.
- Focused coverage: 30 RPC tests; the full workspace suite also passes
  offline.

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

1. Audit RPC abort vs abort-bash lifecycle, terminal events, and session
   records under simultaneous prompt/tool activity (S-041).
2. Produce golden transcripts for every RPC command and event type (S-042),
   including switch/fork/clone, queue modes, compaction, export, and errors.
3. Complete image/read processing parity and register the model-facing image
   behavior in the run path (#32 / S-020-related audit).
4. Finish one-shot print-path auto-compaction and its binary/session fixture
   tests (#33–34 / S-025).
5. Audit client reconnect/timeouts and the remaining TUI/config-selector
   interactive behavior.
6. Keep `CONVERSION-LEDGER.md` and the percentage in `PLAN.md` synchronized;
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

Do not commit or push unless the operator explicitly requests it. Before
continuing, inspect `git status`, read this handoff, run the progress checker,
and treat all existing dirty changes as user-owned work.
