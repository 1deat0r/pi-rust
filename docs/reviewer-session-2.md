# Reviewer Session 2 — Independent Expert Review (pi-rust P2 completion gate)

**Reviewer:** independent expert (round 2, fresh session)
**Project:** pi-rust — Rust 1:1 port of the pi agent harness
**Upstream pinned:** `5cd93f688aaab89dbb6dfa4aca535f21796ae185` (v0.84.2)
**Scope:** verify the P2 phase-completion claim and the four round-1 additive
conditions (reviewer-session-1.md), then sign off for P3 or reject.

Evidence tiers used: `ran` = I executed the command myself; `read` = I read the
file/upstream at the pinned commit. Tier is stated per claim.

---

## 1. Verification 1 — `cargo test --workspace` green, 0 warnings — `ran`

Command (exact): `export PATH=$HOME/.cargo/bin:$PATH && cargo test --workspace`

Result: **75 passed, 0 failed, 0 ignored, 0 filtered**. Breakdown of the passed
suite (all `test result: ok`):

| Suite | Count |
|---|---|
| pi_ai lib (unittests) | 24 |
| pi_ai integration `regression_p2d_producer_hang.rs` | 2 |
| pi_protocol lib | 22 |
| pi_protocol integration `cbor_vectors.rs` | 9 |
| pi_protocol integration `protocol.rs` | 15 |
| pi_telemetry lib | 3 |
| **Total** | **75** |

Warnings: I additionally did a fresh full `cargo clean` + `cargo build --workspace`
and grepped the complete output. `grep -c '^warning:'` = **0**; `grep -c '^error'` = 0.
The ledger's "75/75, 0 warnings" claim holds under a from-scratch build.

---

## 2. Verification 2 — Oracle golden table — `ran`

Command: `node scripts/oracle_partial_json.mjs`

Result: exit 0, prints header + **28 data rows** (verified by row count; script
also documents "Cases 0-19: core partial behavior. Cases 20-27: repairJson-path"
per reviewer condition 2). Output matches upstream behavior for every row, e.g.
`{"a` → `{}`, `tru` → `true`, `-` → `{}`, `12.` → `{}`, `""` → `{}`, `Inf` → `null`,
`1e` → `1`.

The P2-A tests in `crates/pi-ai/src/partial_json.rs` assert **exactly** this
table: `oracle_core_cases` (20 rows, lines ~470-489) + `oracle_repair_path_cases`
(8 rows, lines ~493-507). I cross-checked every expected value in each test
against the oracle output — 28/28 row-by-row match (counted). Both tests pass
in the green suite.

---

## 3. Verification 3 — four round-1 conditions

### 3a. S7 ledger accurate; P2 marked COMPLETE; conditions recorded — `read`

`PLAN.md` §7 ledger marks each of P2-A..P2-E **RESOLVED** with the applied fix,
and the carry-forward block states "P2 phase is COMPLETE (evidence above;
`cargo test --workspace` 75/75, 0 warnings)" (PLAN.md:217). The §7 intro records
the round-1 gate: "the P2 sign-off gate passed with four additive reviewer
conditions, all folded in (ledger recount, P2-D isolation wording, oracle
repair-path rows, sse finish order)" (PLAN.md:200). The round-1 condition-1
sentence-fix is present: the P2-D row now says the probe "times out IN
ISOLATION" (PLAN.md:213) and no stale "passes in isolation" claim remains
(grep found none). Green counts are current (24 lib + 2 integration = matches
the live suite).

Minor nit (non-blocking): the §7 entry heading still reads "### Session 1" while
its content is the S7 update; no S7 heading was added. Accuracy unaffected.

### 3b. Oracle covers repairJson branches — `ran` + `read`

The oracle's repair-path rows (1-indexed output rows 21-28 / cases 20-27)
exercise every `repairJson` branch, and the Rust port implements each:
- **raw control chars inside strings** — `{"a": "b\u0001c"}` → `{"a":"b\u0001c"}`,
  `["x\u0001y"]` → `["x\u0001y"]` (oracle rows 21-22; Rust `oracle_repair_path_cases`
  first two asserts; `repair_json_port` asserts `\u0001` → `\\u0001`).
- **invalid escape doubling** — `{"a": "b\xc"}` → `{"a":"b\\xc"}`,
  `{"a": "b\qc"}` → `{"a":"b\\qc"}` (oracle rows 23-24; Rust tests construct the
  literal-backslash JsonValue directly because reparsed text contains `\x`/`\q`,
  a correct approach).
- **trailing-backslash doubling** — `{"a": "b\` → `{"a":"b"}`, `\` → `{}`
  (oracle rows 25-26).
- **partial unicode/control + partial exponent** — `{"a": "b\u0001c` → `{}`,
  `1e` → `1` (oracle rows 27-28).

All 28 rows are asserted in the passing P2-A tests; a broken repair path can no
longer pass the golden table. **(The task text's "rows 20-27" is off by one:
repair rows are 21-28 in 1-indexed oracle output / cases 20-27 0-indexed; the
substance — all repair branches covered — holds.)**

### 3c. `sse.rs` finish() rotation removed + regressions — `read` + `ran`

`sse.rs:105-134` `fn finish(mut self)` now folds any unterminated buffered line
into an event and then returns `events.extend(std::mem::take(&mut self.pending))`
— the old `events.remove(0); events.push(last)` rotation is gone; pending events
are returned in assembly order. Regression tests present and passing:
`finish_keeps_event_order_when_buffered_data_remains` (sse.rs:204) and
`finish_delivers_unterminated_data_line` (sse.rs:220). The byte-buffer /
line-boundary UTF-8 decode is implemented (sse.rs `drain` splits on the `\n`
byte, decodes complete lines only).

### 3d. Producer panic guarantees stream termination — `read` + `ran`

`crates/pi-ai/src/providers/faux.rs:313-332`: the spawned producer is wrapped as
`tokio::spawn(std::panic::AssertUnwindSafe(body).catch_unwind().then(...))`. On
`Err(panic)` the payload is downcast (`&str` then `String`, fallback
"unknown panic payload") and a **terminal `AssistantMessageEvent::Error`** is
pushed through `panic_tx` (a clone of the live channel sender). This is an active
terminal-event emission, **not** a reliance on dropping the producer's sender.
The `collect()`-holds-its-own-sender rationale is real: `AssistantMessageEventStream`
stores `tx: mpsc::UnboundedSender` as a field (event_stream.rs:12) and
`collect(mut self)` holds `self` (and thus its `tx`) across the entire
`while let Some(event) = self.rx.recv().await` loop (event_stream.rs:69-92), so
dropping the producer's copy alone would never close the channel. The
catch_unwind → terminal-Error design is therefore genuinely required and
correctly implemented.

RNG fix: per-core `Arc<AtomicU64>` (faux.rs:150,214,629-630) with
`wrapping_mul`/`wrapping_add` LCG — no overflow panic, order-independent, matches
round-1 condition.

Both regressions run green in `crates/pi-ai/tests/regression_p2d_producer_hang.rs`:
`long_text_stream_terminates_in_bounded_time` (forces ~30 chunks past old seed-3
threshold; 5s internal timeout did not fire — no hang) and
`producer_panic_surfaces_as_terminal_error_not_hang` (forced Factory panic →
terminal Error containing "synthetic producer panic", never hangs). Both passed
in the 75/75 run.

---

## 4. Verification 4 — P2 fixes faithful to pinned upstream

### 4a. `model.rs` thinking levels == upstream `models.ts` — `read`

Upstream `getSupportedThinkingLevels` (models.ts:902-911) =
`["off"]` when `!model.reasoning`; else filter `EXTENDED_THINKING_LEVELS`
(`off,minimal,low,medium,high,xhigh,max`) where a literal-`null` map entry is
excluded, `xhigh`/`max` additionally require an explicit entry, all other levels
supported unless nulled. Rust port (model.rs:172-195) is line-for-line
equivalent including the null-key and xhigh/max explicit-entry rules.

`clampThinkingLevel` (models.ts:913-930): exact match → else walk UP from the
requested index → else DOWN → else first available. Rust port (model.rs:198-232)
iterates `.skip(requested_index)` (up, inclusive) then `.take(requested_index).rev()`
(down), then `available.first()`. The only nominal divergence — upstream returns
first-available on `indexOf === -1` while Rust `unwrap_or(0)` — is unreachable
because `ModelThinkingLevel::from(level)` always lands in `THINKING_LEVEL_ORDER`;
behaviorally equal. Corrected test (model.rs:~259-289) sets `reasoning=true` and
asserts `Medium→Medium`, `Xhigh→High` (up fails → down), `Max→High`, plus the
`reasoning=false` → `[off]` gate — matching upstream's true behavior.

### 4b. `partial_json` chain == upstream `json-parse.ts` — `read`

The oracle's `repairJson` / `parseJsonWithRepair` / `parseStreamingJson`
(scripts/oracle_partial_json.mjs) are byte-for-byte ports of upstream
`packages/ai/src/utils/json-parse.ts` (read at pinned HEAD; compared line-by-line:
VALID_JSON_ESCAPES set, control-char escape table, `\u` 4-hex-digit rule,
`\\` doubling, trailing-backslash doubling, `parseStreamingJson` four-step
fallback chain `JSON.parse → JSON.parse(repair) → partialParse → partialParse(repair) → {}`
including the `!partialJson || trim()==="" → {}` guard). Rust `parse_streaming_json`
(partial_json.rs:118-134) mirrors the chain exactly over `serde_json`.

**`Inf` divergence — documented and bounded.** partial_json.rs module docs
(lines ~16-22) explicitly state npm `Inf`/`-Inf`/`NaN` partials yield JS
`Infinity`/`NaN` (no JSON representation), which `serde_json::Value` cannot hold,
so they map to `Null`; this is asserted by the oracle rows and the comment bounds
it ("cannot occur in tool-call arguments"). This is an inherent JS/JSON type
mismatch, not a porting error; the bound (fragments that can never be valid JSON
numbers in the wire contract) is sound. Certified as a truthful, bounded,
documented divergence.

### 4c. `faux.rs` usage estimation == upstream `faux.ts` — `read`

Rust `with_usage_estimate` (faux.rs:466-501) mirrors upstream
`withUsageEstimate` (faux.ts:233-264): `input = promptTokens` on fresh session,
`cacheRead = estimateTokens(prefix)`, `cacheWrite = estimateTokens(suffix)`,
`input = saturating_sub(promptTokens - cacheRead)`, `totalTokens = input + output +
cacheRead + cacheWrite`, cache keyed by session id, retention `none` bypasses cache.

Corrected test `usage_estimate_counts_prompt_once` (faux.rs:703-736) asserts the
same invariants as upstream's totals semantics: first call `input == cacheWrite`
(whole prompt charged once and fully written), `u2.input == 0`,
`u2.cache_read == u1.cache_write` (full prefix cached on identical context),
and `total == input+output+cacheRead+cacheWrite` decomposes on both calls. This
is exactly what upstream's two-call-with-same-session-and-context behavior
produces. Test passes.

---

## 5. Optional risk review — `ran` + `read`

- **R-1 no version control** — confirmed the only material open process risk.
  `ls -a` shows no `.git` in the workspace, and none in
  `/home/mustbearn/Projects/AI Agents`, `/home/mustbearn/Projects`, or
  `/home/mustbearn`. PLAN.md §9 R-1 and §7 Docs ("Repo git-init pending operator
  confirmation") both flag it; the standing directive settles it at
  "after P2 sign-off, before P3". R-2 (debug hang) is resolved by the wrapping
  LCG + catch_unwind; R-3's known order-dependence instance is removed (per-core
  RNG); R-4 (fidelity drift) is enforced by the parity oracle -- ongoing, not open.
- **P3 scope ready** — PLAN.md §6 P3 (pi-agent data + harness core: session JSONL
  v4 codec + repo, v3 migration, env abstraction, tools read/write/edit/edit-diff/
  bash with mutation queue; criterion JSONL round-trip incl. v3 migration; tool
  tests over tmp dirs) is clearly defined with a measurable criterion and upstream
  fixtures as the oracle. No blocker identified; the phase-completion gate in §0
  rule 3 is satisfied by this review.

---

## Evidence tiers summary

- `ran`: `cargo test --workspace` (75/75, 0 failures), fresh `cargo clean` +
  `cargo build --workspace` (0 warnings), `node scripts/oracle_partial_json.mjs`
  (28 rows, exit 0), regression suite (both P2-D tests green), `ls -a`/parent
  `.git` absence check.
- `read`: sse.rs, faux.rs, model.rs, partial_json.rs + tests, event_stream.rs,
  upstream json-parse.ts / models.ts / faux.ts at pinned 5cd93f6, PLAN.md §6/§7/§8/§9,
  reviewer-session-1.md (four conditions).
- `reasoned` (not run): behavior of `unwrap_or(0)` in clamp (unreachable);
  `Inf`→`Null` bound rationale; `collect()` sender-retention causality
  (confirmed structurally by reading the struct/collect spans).

## Findings summary

All phase-completion claims and all four round-1 conditions verify clean. No
material defect, discrepancy, or unaddressed condition found. The only
observations are cosmetic: the §7 ledger heading is still "Session 1" (content is
S7), and the task brief's "oracle rows 20-27" is an off-by-one for the 1-indexed
28-row table (repair rows are 21-28). Neither affects correctness.

VERDICT: SIGN-OFF
