# Independent Expert Review — Session 1 (pi-rust, phase P2 gate)

> Historical P2 review. This report records an earlier gate and is not a
> current full-conversion verdict; current reviewer evidence belongs under the
> active Session-13 gate in PLAN.md and the scoped audit directory.

Reviewer: fresh session, no implementation involvement
Date: 2026-08-21
Scope: PLAN.md governance+ledger (S0/S7/S8/S9), pi-protocol/pi-telemetry green claims,
      pi-ai P2-A..P2-E recorded issues, corrected expectations vs upstream 5cd93f6,
      continuation-gate completeness, workspace build.
Method: I ran every command myself and read every cited source. No claim below is
      taken on faith except the vendor's npm package identity (partial-json@0.1.7,
      cross-checked against upstream package.json, which pins the same version).

Environment: cargo 1.x via $HOME/.cargo/bin, upstream pinned 5cd93f688aaab89dbb6dfa4aca535f21796ae185.

---

## 1. pi-protocol + pi-telemetry green claims — HOLD (verified)

Command: `cargo test -p pi-protocol -p pi-telemetry` (also run separately per crate). Exit 0.

- pi-protocol: 3 suites → 22 + 9 + 15 = **46 passed, 0 failed, 0 ignored**.
  Ledger says "46 tests green." Verified.
- pi-telemetry: **3 passed** (`memory_records_spans`, `memory_records_attributes_and_status`,
  `noop_runs_callback`). Ledger says "3 tests green." Verified.

Minor note: `cargo build -p pi-telemetry` emits 2 warnings (lib.rs:182 `std::mem::drop`
with a reference — a real no-op bug; lib.rs:240 dead fields). Not part of P2-E (which is
scoped to pi-ai) and not blocking, but worth a cleanup pass.

## 2. pi-ai — the five recorded issues, independently re-derived — HOLD (all five real)

Command baseline: `cargo test -p pi-ai -- --test-threads=1` under `timeout 120`.
Observed: 3 failures exported, then the suite **hung** on `usage_estimate_counts_prompt_once`
(timeout killed it, exit 124). Re-running with `--skip usage_estimate_counts_prompt_once`
gives the exact split: **18 passed / 4 failed / 1 hang = 23 total.**

> Ledger correction: the ledger says "23 tests: 19 green / 4 red / 1 hang." 19+4+1=24;
> the true split is **18 green / 4 red / 1 hang**. The 4 red are `thinking_level_clamp`,
> `tolerates_partial_keywords_and_numbers`, `tolerates_partial_strings`,
> `sse::handles_utf8_split_across_chunks`. One-count green overstatement; conclusion unchanged.

### P2-A partial_json vs oracle — CONFIRMED, exactly as recorded
- Rust returns `Null` for `""` (fn start, partial_json.rs), `tru`→`Null`, `{"a": tru`→`{"a":null}`,
  and `parse_number` folds `-`→`0` (not `{}`) and `12.`→`12` (not `{}`) via its
  trim_end_matches fallback. Rust tests `tolerates_partial_strings`/`tolerates_partial_keywords_and_numbers`
  assert those wrong values → they fail.
- `node scripts/oracle_partial_json.mjs` reproduces the recorded golden table exactly:
  `{"a`→`{}`, `tru`→`true`, `{"a": tru`→`{"a":true}`, `-`→`{}`, `12.`→`{}`, `""`→`{}`.
- I additionally cross-ran the **exact** upstream `parseStreamingJson`/`parseJsonWithRepair`
  chain (json-parse.ts @ 5cd93f6) against the vendored `partial-json@0.1.7` on all 20 golden
  cases: **0 diffs** vs the oracle's slightly-simplified chain (the oracle skips the
  `JSON.parse(repairJson)` middle step; proven equivalent on this table). Oracle is sound.

### P2-B SSE UTF-8 split — CONFIRMED, plus an unrecorded bug
- `sse.rs::push_bytes` does `self.buffer.push_str(&String::from_utf8_lossy(chunk))`
  per chunk → an incomplete multibyte sequence at a chunk boundary becomes U+FFFD and
  permanently corrupts the buffer. `handles_utf8_split_across_chunks` fails
  (verified in isolation: sse module 7 passed / 1 failed).
- **Unrecorded latent bug (must fold into P2-B):** `SseParser::finish()` does
  `let last = events.remove(0); events.push(last);` — it rotates the FIRST pending event
  to the back. Harmless when buffer/pending are empty at EOF (the current tests), but on any
  real EOF-with-buffered-data path it **reorders events**. The byte-buffer rewrite must fix this.

### P2-C thinking-level clamp — CONFIRMED, corrected expectations match upstream exactly
- `model.rs Model::new` defaults `reasoning=false` (model.rs:46). Ledger claim correct.
- `get_supported_thinking_levels` (model.rs:159) has **no `!reasoning → ["off"]` gate**.
- Map semantics diverge: Rust includes missing keys for all 7 levels (`None => true`);
  upstream (models.ts:900-911) requires an **explicit non-null entry for `xhigh`/`max`**,
  and only excludes other levels when the map entry is literally `null`. The Rust port's
  `Some(None) => false / None => true` also mishandles map-absent vs map-null for ordinary levels
  (upstream: absent → supported, null → not; Rust: absent → supported, null → not — actually
  Rust matches there, but NOT for xhigh/max which upstream requires be present).
- `clamp_thinking_level` is **down-only** (model.rs:186-211); upstream
  (models.ts:913-931) clamps **up first (from requestedIndex upward), then down**.
- Verified against upstream: with `reasoning=true` + map `{off, low, high}` upstream yields
  `Medium→Medium` (medium is supported: absent key, non-xhigh/max → included) and `Xhigh→High`
  (xhigh not explicit → unsupported; up-loop finds nothing, down-loop lands on high).
  Exactly what the plan records; the Rust test's `Medium→Low` matches **no upstream code path**.

### P2-D faux RNG overflow + swallowed-panic hang — CONFIRMED arithmetic; one record inaccurate
- `split_by_token_size` (faux.rs:587-615): `static COUNTER: AtomicU64` (line 592) and
  `seed * 6364136223846793005 + 1442695040888963407` (line 597) with **non-wrapping** u64.
  Verified by independent arithmetic: `6364136223846793005*3 = 19092408671540379015 > u64::MAX
  (18446744073709551615)` → overflow at seed 3 under debug overflow-checks. Plan's arithmetic claim is correct.
- Defaults `DEFAULT_MIN/MAX_TOKEN_SIZE = 3/5` (faux.rs:23-24) → char_size 12-20, so an
  unremarkable production text (e.g. 400 chars) alone drives far more than 3 counter slots.
- Hang mechanism confirmed: producer is `tokio::spawn`'d (faux.rs:251) and the JoinHandle is
  `std::mem::forget`'d (faux.rs:303) → a panic is swallowed. `AssistantMessageEventStream::collect`
  (event_stream.rs:90-113) awaits `self.rx.recv()` while the stream **itself holds a live
  `tx` sender** → the channel can never close; after the producer panics and drops its copy,
  `collect()` still awaits forever. Confirmed by the full-suite timeout.
- **Record inaccuracy:** the ledger states "Probe passes in isolation → test-order-dependent bug"
  and "Reproduced: single long text (400 chars) forces seed≥3 in one stream → probe times out."
  These contradict each other. Current evidence: `tests/hang_probe.rs` (400-char) **times out in
  isolation** (its internal 5s timeout fires: "TIMEOUT -> hang confirmed"). So the hang is
  **order-INDEPENDENT** for long text. The order-dependence is real but only for the short
  recorded test `usage_estimate_counts_prompt_once`: in isolation it **fails fast** (assertion,
  seed <3 so no panic), and only **hangs** when prior faux tests in the same process have
  advanced the global counter past 3. Root cause analysis is correct; the "probe passes in
  isolation" sentence is wrong and should be corrected in PLAN.md.

### P2-E warnings — CONFIRMED
- `cargo build -p pi-ai` → "pi-ai (lib) generated **17 warnings**". Matches ledger.
  Breakdown includes the recorded `unused import create_error_stream` (faux.rs:10) and 8x
  irrefutable `if let`, none of which are behaviorally dangerous. (The `create_error_stream`
  removal is borderline: P2-D's panic-to-error design may want it, so delete/keep with intent.)

## 3. Corrected expectations stated in the plan — all verified against upstream + oracle

- **parseStreamingJson fallback chain** (json-parse.ts @ 5cd93f6): empty/undefined → `{}`;
  `JSON.parse` (via `parseJsonWithRepair`); on throw → `partialParse(json)`; on throw →
  `partialParse(repairJson(json))`; on throw → `{}`. Plan's description accurate. `repairJson`
  escapes raw control chars in strings, doubles backslashes before invalid escapes, and handles
  dangling trailing backslashes. I proved the vendored 20-case table is identical to this exact
  chain (0/20 diffs).
- **Thinking-level semantics** (models.ts:900-932): reasoning gate (`!reasoning → ["off"]`),
  missing-key map semantics (ordinary levels always supported unless the entry is literally null;
  `xhigh`/`max` require an explicit non-null entry), xhigh/max special case, and up-then-down
  clamp. All four plan statements accurate.
- **SSE fix direction** (byte-accumulating buffer, split on `\n` byte, decode complete lines,
  lossy only at `finish`): correct direction and is the standard fix; adds the `finish()` reorder
  fix (see P2-B).
- **faux RNG wrapping + instance-local state + panic-to-error stream**: sound. Wrapping arithmetic
  removes the debug overflow panic; instance/thread-local RNG removes the cross-test coupling;
  emitting a terminal `Error` (or otherwise guaranteeing sender-drop / oneshot-completion) is the
  only reliable way to close the hang, because `collect()` holds its own sender so "drop the tx"
  alone is insufficient — the producer must actively terminate the stream. (I flag this nuance so
  the implementer prefers "emit terminal Error event" over "just drop the sender.")

## 4. Continuation gate — COMPLETE and SOUND, with 4 conditions to fold in

The P2-A..P2-E plan correctly targets every observed failure and every verified root cause; the
corrected expectations are accurate against pinned upstream. The gate ("implement P2-A..E →
`cargo test -p pi-ai` 23/23 → P2 sign-off") is coherent with §6 P2 scope; nothing material in
scope or sequencing is missing.

Conditions to incorporate (all additive / accuracy, none re-scopes the phase):
1. **Correct PLAN.md S7**: green count 19 → 18; delete the "Probe passes in isolation" sentence
   in P2-D (probe hangs in isolation; order-dependence applies only to the short recorded test).
2. **Extend the oracle table with repair-path cases** so P2-A cannot pass without implementing
   `repairJson`'s observable behavior. The current 20 cases never trigger the repair branches
   (control-char escaping, invalid-escape doubling, trailing-backslash doubling), so a port could
   satisfy the golden table while shipping a broken repair path.
3. **Fold the `sse.rs finish()` event-reorder bug** into P2-B (it is in the same function being
   rewritten).
4. **P2-D fix (c) must guarantee stream termination on producer panic** (emit terminal Error /
   complete the oneshot), not merely "drop the sender," since `collect()` holds its own sender.

Optional (non-blocking): extend warning cleanup to pi-telemetry's 2 warnings; keep-or-delete
`create_error_stream` with intent given P2-D(c).

## 5. Workspace build — HOLD

`cargo check --workspace` (exact command) → exit 0, `Finished dev profile [optimized +
debuginfo] target(s) in 2.88s`, with `pi-ai` emitting the 17 warnings from P2-E. Clean build.

## Evidence tiers
- protocol/telemetry green, workspace check: verified directly (unit, my run).
- All P2-A..E failures/hang: verified directly (unit, my run).
- Oracle golden table: generated from vendored partial-json@0.1.7 (network-free), cross-checked
  against the exact upstream chain (0/20 diffs) — verified, not taken on faith.
- Upstream models.ts / json-parse.ts / faux.ts semantics: read at pinned 5cd93f6 directly.
- Via oracle/vendor only: partial-json@0.1.7 identity (upstream package.json pins "0.1.7").

---

VERDICT: SIGN-OFF
Conditions (all additive, fold into the next implementation phase before P2 sign-off):
1. Correct S7 ledger (green 18, not 19) and remove the "probe passes in isolation" record in P2-D.
2. Add repair-path cases to the partial-json oracle/golden table.
3. Fix the `sse.rs finish()` event-reorder bug as part of P2-B.
4. Make P2-D fix(c) guarantee stream termination on producer panic (terminal Error / oneshot),
   and prefer instance-local (per-core) RNG over thread-local so tests stay order-independent.
