# Pi → pi-rust conversion handoff

## Day goal 2026-09-18 — finish 100% 1:1 parity (pi agent ↔ pi-rust)

### Transcript slice L — SystemMessage.replace removed committed + pushed

Upstream 16292398a: `replace` leaves the replay surface; replay
never resets, mid-convo resolution keeps later messages. Old oracle
test kept as ignored archaeology; new accumulation pin, TDD
red-first. Transcript 7/7, pi-ai lib 486/486, pi-ai strict clippy
clean, fmt clean, conversion 100.00% (166/166), parity dashboard OK
(58/318, upstream=d7296c0). No row promoted; metrics unchanged.
Committed `9780fb6`, pushed, hashes match. Next: continue the
new-drift triage.

### Extension slice K — handler unsubscribe committed + pushed

Upstream #9630 (46c9de40): `on()` returns an unsubscribe handle
(index-addressed removal, empty-key drop, safe rerun); snapshot half
assessed-not-ported. New pin, TDD red-first. extensions_parity
10/10, extensions lib 74/74, scoped rustfmt clean, conversion
100.00% (166/166), parity dashboard OK (58/318, upstream=d7296c0).
Pre-existing clippy verified identical on clean HEAD. No row
promoted; metrics unchanged. Committed `ad824b8`, pushed, hashes
match. Next: continue the new-drift triage.

### Provider slice J — Vercel unsigned thinking committed + pushed

Upstream #9676 (3955b27a1): `allowEmptySignature: true` on all 237
Vercel AI Gateway entries. New catalog pin, TDD red-first (0/237
before patch). Catalog 15/15, pi-ai lib 486/486, pi-ai strict clippy
clean, fmt clean, conversion 100.00% (166/166), parity dashboard OK
(58/318, upstream=d7296c0). No row promoted; metrics unchanged.
Committed `32fcc38`, pushed, hashes match. Next: continue the
new-drift triage.

### Tool slice I — signal-terminated shell exit codes committed + pushed

Upstream #9577 (a8b3dd19): shared executor maps signal termination
to 128+signo (KILL→137, TERM→143; neither→1). New Unix-gated pin,
TDD red-first (Some(0) vs Some(137)). Tools 23/23, pi-agent lib
276/276, scoped rustfmt clean, conversion 100.00% (166/166), parity
dashboard OK (58/318, upstream=d7296c0). Pre-existing clippy
verified identical on clean HEAD. No row promoted; metrics
unchanged. Committed `4a070d3`, pushed, hashes match. Next:
continue the new-drift triage.

### Provider slice H — Azure peak-load retry committed + pushed

Upstream #9669 (e98f287ee): peak-load capacity text joins the
retryable patterns with an oracle-mirroring pin. pi-ai lib 486/486,
retry 33/33, pi-ai strict clippy clean, fmt clean, conversion
100.00% (166/166), parity dashboard OK (58/318, upstream=d7296c0).
No row promoted; metrics unchanged. Committed `eacec53`, pushed,
hashes match. Next: continue the new-drift triage.

### Provider slice G — Cloudflare 520 retry committed + pushed

Upstream #9627 (e5d1838): `"520"` joins the retryable patterns; RED
pin for the exact oracle wording was pre-staged, now green. pi-ai
lib 485/485, retry 32/32, pi-ai strict clippy clean, fmt clean,
conversion 100.00% (166/166), parity dashboard OK (58/318,
upstream=d7296c0). No row promoted; metrics unchanged. Committed
`3b8e474`, pushed, hashes match. Drive-by hook fix: the bundle rule
no longer demands unchanged files be staged (staged-file presence
only), and the progress check applies to staged checkpoint docs —
hook exit 0 verified on the real set. Next: triage the 13-commit new
drift (e4c75a732 → 46c9de40).

## Doc-hygiene pass 2026-09-17 — documentation system optimization

Documentation-only (no source, test, or parity row changed):
`.markdownlint-cli2.jsonc` + `scripts/docs-lint.sh` (README.md +
AGENTS.md lint-clean, 0 issues); `.githooks/pre-commit` portabilized
(PATH cargo, infra exemption, living-docs lint; metric checks rescoped
to README/registers/dashboard so archived snapshots are never
rewritten) and verified runnable here — hook exit 0 on the staged set;
README status rewritten (7 stale 2026-08 checkpoints removed, current
counts: pi-ai 484, pi-tui 409, pi-agent 276, catalog 1,351,
pi-coding-agent 29 pre-existing failures disclosed); HANDOFF.md
6077→947 lines and PLAN.md 4414→501 lines via frozen archives;
GATES.md frozen record with the launch/live tail archived; dashboard
retired-narrative banner; stale notes corrected (drift L/N
supersession, hook portability); AGENTS.md doc map added. Validation:
docs-lint 0 issues, hook exit 0, conversion 100.00% (166/166), parity
dashboard OK (58/318, upstream=d7296c0). Metrics unchanged.
Committed `beb0fdf`, pushed, hashes match. Next: continue the parity
sweep.

## Day goal 2026-09-17 — sectioned transcript-replay port

### Session slice N — header-only exact-id lookup committed + pushed

Upstream #9601 (9b791a4cc fixing #9440): new
`JsonlSessionRepo::find_by_id` reads headers only, skips corrupt
files, `None` on missing roots. Pin green (hit/miss/decoy/
missing-root), TDD red-first. jsonl_repo 17/17, pi-agent lib
276/276, scoped rustfmt clean, conversion 100.00% (166/166), parity
dashboard OK (58/318, upstream=d7296c0). Pre-existing clippy
verified identical on clean HEAD. No row promoted; metrics
unchanged. Committed `3909e01`, pushed, hashes match. (Commit predates
the 2026-09-17 hook-portability fix; hook conditions were verified
manually.) Next: continue the sweep.

### Extension slice L — user_bash fail-closed committed + pushed

Upstream #9068 (509ee2bd0): `emit_user_bash` fails closed —
handler errors report + propagate, invalid defined results rejected
with the upstream diagnostic, `None` propagates. Two pins green,
TDD red-first. Extensions lib 74/74, extensions_parity 9/9, scoped
rustfmt clean, conversion 100.00% (166/166), parity dashboard OK
(58/318, upstream=d7296c0). Pre-existing clippy failures verified
identical on clean HEAD. No row promoted; metrics unchanged.
Committed `c4a77d4`, pushed, hashes match. (Commit predates the
2026-09-17 hook-portability fix; hook conditions were verified
manually.) Next: continue
the sweep.

### Resource slice M — symlink-root pin committed + pushed

RES-006 symlink residual closed with a recursive-discovery pin
(evidence-only; no source change; row note extended, held). New pin
1/1 green, scoped rustfmt clean, conversion 100.00% (166/166),
parity dashboard OK (58/318, upstream=d7296c0). Pre-existing
ignore-file pin failure verified identical on clean HEAD via stash.
No row promoted; metrics unchanged. Committed `d6dd13d`, pushed,
hashes match. (Commit predates the 2026-09-17 hook-portability fix;
hook conditions were verified manually.) Next: continue the sweep.

### Extension slice K — UI prompt events committed + pushed

Upstream #8355 (ccfe79ed2): nesting-aware prompt emitter on
handler contexts; 5 dialogs wrapped; failures isolated. Four
pins green. Extensions 72/72, fmt clean, conversion 100.00%
(166/166), parity dashboard OK (58/318, upstream=d7296c0). No row
promoted; metrics unchanged. Committed `94140ee`, pushed, hashes
match. Next: continue the sweep.

### Session slice L — discovery symlink pin committed + pushed

SES-007 symlink residual closed with a real-filesystem pin (no
source change; row note extended, statuses held). jsonl_repo
16/16, pi-agent lib 276/276, fmt clean, conversion 100.00%
(166/166), parity register + dashboard OK (58/318,
upstream=d7296c0). No row promoted; metrics unchanged. Committed
`27d5cba`, pushed, hashes match. Next: continue the sweep.

SES-007 symlink residual closed with a real-filesystem pin (no
source change; row note extended, statuses held). jsonl_repo
16/16, pi-agent lib 276/276, fmt clean, conversion 100.00%
(166/166), parity register + dashboard OK (58/318,
upstream=d7296c0). No row promoted; metrics unchanged. Next:
commit + push, then continue the sweep.

## Day goal 2026-09-17 — sectioned transcript-replay port

Upstream #8512 (80e62761f): optional powershell tool (discovery +
args + UTF8 prefix; shell_args seam; default set untouched).
Three pins green. pi-agent lib 276/276, fmt clean, conversion
100.00% (166/166), parity dashboard OK (58/318, upstream=d7296c0).
No row promoted; metrics unchanged. Committed `63a828f`, pushed,
hashes match. Next: continue the sweep.

## Day goal 2026-09-17 — sectioned transcript-replay port

### Extension slice I — tool schema validation committed + pushed

Upstream #9300 (acaa253cc): `register_tool` rejects non-object
schemas at registration. New 4-shape pin green. Loader 16/16,
fmt clean, conversion 100.00% (166/166), parity dashboard OK
(58/318, upstream=d7296c0). No row promoted; metrics unchanged.
Committed `49e7442`, pushed, hashes match. Next: continue the
sweep.

## Day goal 2026-09-17 — sectioned transcript-replay port

### Resolver slice H — Radius default committed + pushed

Upstream 9767ba275: Radius default `balanced` (fallback existed;
no post-login flow to defer). Pin updated. Scoped 41/41, fmt
clean, conversion 100.00% (166/166), parity dashboard OK (58/318,
upstream=d7296c0). No row promoted; metrics unchanged. Committed
`feb0d07`, pushed, hashes match. Next: continue the sweep.

## Day goal 2026-09-17 — sectioned transcript-replay port

### Interactive slice W — share isolation committed + pushed

Upstream #8613 (6f35de5b5): unique temp dir per concurrent share
(`share_via_gist` extraction). New concurrency pin green. Share
4/4, fmt clean, conversion 100.00% (166/166), parity dashboard OK
(58/318, upstream=d7296c0). No row promoted; metrics unchanged.
Committed `c8c1172`, pushed, hashes match. Next: continue the
sweep.

Upstream #8613 (6f35de5b5): unique temp dir per concurrent share
(`share_via_gist` extraction). New concurrency pin green. Share
4/4, fmt clean, conversion 100.00% (166/166), parity dashboard OK
(58/318, upstream=d7296c0). No row promoted; metrics unchanged.
Next: commit + push, then continue phase 1.

### ENV slice V — cache TTL breadth committed + pushed

ENV-011 cross-provider residual closed with an anthropic TTL unit
pin (no source change; row note extended, held). pi-ai lib
484/484, fmt clean, conversion 100.00% (166/166), parity register
+ dashboard OK (58/318, upstream=d7296c0). No row promoted;
metrics unchanged. Committed `a1c9d80`, pushed, hashes match.
Next: continue phase 1.

## Day goal 2026-09-17 — sectioned transcript-replay port

### CFG slice AA — migration no-rewrite committed + pushed

CFG-004 restart residual closed with a no-rewrite process pin
(TDD red-first, corrected by archaeology; no source change; row
note extended, held). Config 5/5, fmt clean, conversion 100.00%
(166/166), parity register + dashboard OK (58/318,
upstream=d7296c0). No row promoted; metrics unchanged. Committed
`f01e106`, pushed, hashes match. Next: continue phase 1.

CFG-004 restart residual closed with a no-rewrite process pin
(TDD red-first, corrected by archaeology; no source change; row
note extended, held). Config 5/5, fmt clean, conversion 100.00%
(166/166), parity register + dashboard OK (58/318,
upstream=d7296c0). No row promoted; metrics unchanged. Next:
commit + push, then continue phase 1.

### CFG slice Z — unknown-key retention committed + pushed

CFG-001 retention residual closed with a save round-trip pin
(TDD red-first on flush). Settings 35/35, fmt clean, conversion
100.00% (166/166), parity register + dashboard OK (58/318,
upstream=d7296c0). No row promoted; metrics unchanged. Committed
`f8cc9bd`, pushed, hashes match. Next: continue phase 1.

## Day goal 2026-09-17 — sectioned transcript-replay port

### Agent slice Y — truncated summaries committed + pushed

Upstream #7048 (97fa14e39): shared failure helper rejects
length-stop summaries at 3 sites. New helper + branch pin green.
Compaction 24/24, pi-agent lib 276/276, fmt clean, conversion
100.00% (166/166), parity dashboard OK (58/318, upstream=d7296c0).
No row promoted; metrics unchanged. Committed `27b9d7f`, pushed,
hashes match. Next: continue phase 1.

## Day goal 2026-09-17 — sectioned transcript-replay port

### Agent slice X — summary output cap committed + pushed

Upstream #8845 (e44d75c20): branch summary caps at min(4096,
model.maxTokens). New cap pin green. Compaction 23/23, pi-agent
lib 276/276, fmt clean, conversion 100.00% (166/166), parity
dashboard OK (58/318, upstream=d7296c0). No row promoted; metrics
unchanged. Committed `9f5b425`, pushed, hashes match. Next:
continue phase 1.

## Day goal 2026-09-17 — sectioned transcript-replay port

### CLI slice U — unknown-pattern evidence committed + pushed

CLI-036 unknown-pattern residual closed with a real-process pin
(no source change; row note extended, held). List-models 4/4, fmt
clean, conversion 100.00% (166/166), parity register + dashboard
OK (58/318, upstream=d7296c0). No row promoted; metrics unchanged.
Committed `e3b02c0`, pushed, hashes match. Next: continue phase 1.

## Day goal 2026-09-17 — sectioned transcript-replay port

### CLI slice T — template variable evidence committed + pushed

CLI-033 variable residual closed with a real-process pin (no
source change; row note extended, held). Resources 11/11, fmt
clean, conversion 100.00% (166/166), parity register + dashboard
OK (58/318, upstream=d7296c0). No row promoted; metrics unchanged.
Committed `b132c57`, pushed, hashes match. Next: continue phase 1.

## Day goal 2026-09-17 — sectioned transcript-replay port

### CLI slice S — skill prompt-inclusion evidence committed + pushed

CLI-032 prompt-inclusion residual closed with a usage process pin
(no source change; row note extended, held). Resources 10/10, fmt
clean, conversion 100.00% (166/166), parity register + dashboard
OK (58/318, upstream=d7296c0). No row promoted; metrics unchanged.
Committed `5dc4ff4`, pushed, hashes match. Next: continue phase 1.

## Day goal 2026-09-17 — sectioned transcript-replay port

### CLI slice R — no-tools usage evidence committed + pushed

CLI-023/024 payload residual closed with a JSON-usage process pin
(no source change; both row notes extended, held). JSON 9/9, fmt
clean, conversion 100.00% (166/166), parity register + dashboard
OK (58/318, upstream=d7296c0). No row promoted; metrics unchanged.
Committed `7f14a3e`, pushed, hashes match. Next: continue phase 1.

## Day goal 2026-09-17 — sectioned transcript-replay port

### CLI slice Q — exclusion projection evidence committed + pushed

CLI-022 projection residual closed with a JSON-usage process pin
(no source change; row note extended, held). JSON 8/8, fmt clean,
conversion 100.00% (166/166), parity register + dashboard OK
(58/318, upstream=d7296c0). No row promoted; metrics unchanged.
Committed `2de9f88`, pushed, hashes match. Next: continue phase 1.

## Day goal 2026-09-17 — sectioned transcript-replay port

### CLI slice P — export overwrite evidence committed + pushed

CLI-029 overwrite/suffix residual closed with a real-process pin
(no source change; row note extended, held). Export 5/5, fmt
clean, conversion 100.00% (166/166), parity register + dashboard
OK (58/318, upstream=d7296c0). No row promoted; metrics unchanged.
Committed `bfdcb29`, pushed, hashes match. Next: continue phase 1.

## Day goal 2026-09-17 — sectioned transcript-replay port

### CLI slice O — unknown-tool evidence committed + pushed

CLI-021 unknown-tool residual closed with a real-process pin (no
source change; row note extended, held). Flag matrix 7/7, fmt
clean, conversion 100.00% (166/166), parity register + dashboard
OK (58/318, upstream=d7296c0). No row promoted; metrics unchanged.
Committed `c7165a9`, pushed, hashes match. Next: continue phase 1.

## Day goal 2026-09-17 — sectioned transcript-replay port
Next: commit + push, then continue phase 1.

### CLI slice N — invalid-catalog evidence committed + pushed

CLI-020 invalid-catalog residual closed with a real-process pin
(no source change; row note extended, held). Config 4/4, fmt
clean, conversion 100.00% (166/166), parity register + dashboard
OK (58/318, upstream=d7296c0). No row promoted; metrics unchanged.
Committed `00ec7cc`, pushed, hashes match. Next: continue phase 1.

## Day goal 2026-09-17 — sectioned transcript-replay port
Next: commit + push, then continue phase 1.

### CLI slice M — name persistence evidence committed + pushed

CLI-019 newline/restart residual closed with a real-process pin
(no source change; row note extended, held). Restart 12/12, fmt
clean, conversion 100.00% (166/166), parity register + dashboard
OK (58/318, upstream=d7296c0). No row promoted; metrics unchanged.
Committed `d671b11`, pushed, hashes match. Next: continue phase 1.

## Day goal 2026-09-17 — sectioned transcript-replay port
Next: commit + push, then continue phase 1.

### Maintenance — stale catalog pins committed + pushed

Repaired nvidia id list + copilot filter fixture to vendored data
(both red on clean HEAD). Catalog 14/14, copilot 5/5, pi-ai lib
483/483 green, pi-ai strict clippy clean, fmt clean, conversion
100.00% (166/166), parity dashboard OK (58/318, upstream=d7296c0).
No row promoted; metrics unchanged. Committed `5e3ef43`, pushed,
hashes match. Next: continue the sweep or wave (b) migration.

## Day goal 2026-09-17 — sectioned transcript-replay port

### Agent slice G — proxy EOF error committed + pushed

Upstream #8997 (ebc374490): proxy EOF without terminal event
finalizes an error via pusher tracking. New loopback pin green,
true-RED TDD via 10s hang. pi-agent lib 273/273, proxy 13/13, fmt
clean, conversion 100.00% (166/166), parity dashboard OK (58/318,
upstream=d7296c0). No row promoted; metrics unchanged. Committed
`221b782`, pushed, hashes match. Next: continue the sweep.

## Day goal 2026-09-17 — sectioned transcript-replay port

### Provider slice F — Google transient retry committed + pushed

Upstream #7471 (b9d360a2c): generative-ai retries transient
errors/statuses per maxRetries. New loopback pin green, TDD
red-first. pi-ai lib 483/483, exhaustive 18/18, pi-ai strict
clippy clean, fmt clean, conversion 100.00% (166/166), parity
dashboard OK (58/318, upstream=d7296c0). No row promoted; metrics
unchanged. Committed `57ce12b`, pushed, hashes match. Next:
continue the pin-era sweep.

## Day goal 2026-09-17 — sectioned transcript-replay port

### Transcript leaf 4 — initial-message helpers committed + pushed

Upstream initial-message helpers (`createInitialSystemMessage` /
`normalizeContext` / initial/strip): fold/strip pin, no callers
yet. pi-ai lib 483/483, pi-ai strict clippy clean, fmt clean,
conversion 100.00% (166/166), parity dashboard OK (58/318,
upstream=d7296c0). No row promoted; metrics unchanged. Committed
`c2d2593`, pushed, hashes match. Next: wave (b) adapter
signatures (flag-day migration, queued with loop + persistence).

## Day goal 2026-09-17 — close upstream drift (d7296c0 → e4c75a732)

### Transcript wave (a) — capability flags committed + pushed

`supportsMidConvoSystemMessages`/`supportsMidConvoToolChanges`
parsed + model-id gate with 2 pins (dead-code allows note wave
(b)). pi-ai lib 482/482, pi-ai strict clippy clean, fmt clean,
conversion 100.00% (166/166), parity dashboard OK (58/318,
upstream=d7296c0). No row promoted; metrics unchanged. Committed
`44f5679`, pushed, hashes match. Next: wave (b) (TranscriptContext
adapter signatures).

## Day goal 2026-09-17 — close upstream drift (d7296c0 → e4c75a732)

### Transcript leaf 3 — provider adoption assessed (no slice)

Adoption needs `supportsMidConvoSystemMessages` /
`supportsMidConvoToolChanges` compat flags, `Context` →
transcript plumbing across 10+ adapters, agent-loop system
messages, and session persistence — a multi-day migration, not a
day leaf. Leaves 1–2 (pure surface, no callers) stand alone and
green. Queued adoption waves: (a) compat flags + per-provider
capability table, (b) `TranscriptContext` adapter signatures,
(c) loop forced-prompt + persistence, (d) sectioned assembly
cutover with snapshot updates.

### Transcript leaf 2 — section wrapping committed + pushed

### Transcript leaf 2 — section wrapping committed + pushed

Upstream section wrapping (`buildSystemPromptSections` tail): pure
helpers + pin, no callers yet (flat assembly byte-identical).
pi-ai lib 480/480, pi-ai strict clippy clean, fmt clean,
conversion 100.00% (166/166), parity dashboard OK (58/318,
upstream=d7296c0). No row promoted; metrics unchanged. Committed
`803ebd6`, pushed, hashes match. Next: leaf 3 (provider adoption
scoping).

## Day goal 2026-09-17 — close upstream drift (d7296c0 → e4c75a732)

### Transcript leaf 1 — replay utils committed + pushed

Upstream transcript replay surface (`transcript.ts` at e4c75a732):
`SystemMessage`/`ToolReference` types + replay module with 5 pins
mirroring upstream tests. No callers yet (later leaves). pi-ai lib
479/479, pi-ai strict clippy clean, fmt clean, conversion 100.00%
(166/166), parity dashboard OK (58/318, upstream=d7296c0). No row
promoted; metrics unchanged. Committed `ec175eb`, pushed, hashes
match. Next: leaf 2 (sectioned prompt builder).

## Day goal 2026-09-17 — close upstream drift (d7296c0 → e4c75a732)

### Drift slice E — Google thinking maps committed + pushed

Upstream #9455 (aa50fe778): 32 Google/Vertex/OpenCode thinking maps
derive from models.dev effort metadata (Gemma 4 fallback kept). New
pin mirrors upstream providers test, TDD red-first. pi-ai lib
474/474, pi-ai strict clippy clean, fmt clean, conversion 100.00%
(166/166), parity dashboard OK (58/318, upstream=d7296c0). No row
promoted; metrics unchanged. Committed `92c1aa9`, pushed, hashes
match. Next: drift slice F.

## Drift assessment — remaining commits need larger surfaces (2026-09-17 update)

- 509ee2bd0 user-bash fail-closed (#9068): PORTED as Extension slice L
  (`c4a77d4`) — the runner half ports cleanly despite no interactive
  routing caller yet.
- 9b791a4cc header-only session find (#9601): PORTED as Session slice N
  (`3909e01`) — `JsonlSessionRepo::find_by_id` reads headers only.
- 4c2d91339 hook type exports (#9511): TS-barrel concern only.
- 1247476e6 + 9e05370b2 + e4c75a732 (eval markers, mid-conversation
  system messages, prompt replace): all key off the sectioned
  `SystemMessage` transcript-replay surface, which Rust lacks (flat
  system prompt). Wave B-scale port, queued as one item.

Drift goal complete: 7/12 commits ported as slices A–E, L, N; 5/12
assessed (1 TS-only, 4 blocked on larger surfaces with the next item
queued). Prior text claiming L/N blocked is superseded by the two
slices above.

### Drift slice D — image refresh committed + pushed

Post-pin image refresh (bdee230f1): 4 renames + 2 additions,
vendored pretty format preserved (amended once for a single-line
collapse). New catalog pin green, TDD red-first. pi-ai lib 474/474,
pi-ai strict clippy clean, fmt clean, conversion 100.00% (166/166),
parity dashboard OK (58/318, upstream=d7296c0). No row promoted;
metrics unchanged. Committed `d1006d0`, pushed, hashes match. Next:
drift slice E.

## Day summary — 2026-09-16/17 — Wave C provider sweep (13 slices, all pushed)

### Drift slice C — clipboard fail-closed committed + pushed

Upstream #9618 (3349e1db1; renderer half already ported): OSC 52
fallback remote-only, local failures fail closed. New PATH-shimmed
pin green, TDD red-first. Clipboard 6/6, mermaid 3/3, fmt clean,
conversion 100.00% (166/166), parity dashboard OK (58/318,
upstream=d7296c0). No row promoted; metrics unchanged. Committed
`4d63ffc`, pushed, hashes match. Next: drift slice D.

## Day summary — 2026-09-16/17 — Wave C provider sweep (13 slices, all pushed)

### Drift slice B — fallback overrides committed + pushed

Upstream #9294 (b03a367a4): `allowedFallbackModels` shape
validation + merge pins green, TDD red-first (lane + merge already
supported it). Scoped suites 32/32, fmt clean, conversion 100.00%
(166/166), parity dashboard OK (58/318, upstream=d7296c0). No row
promoted; metrics unchanged. Committed `ee723ae`, pushed, hashes
match. Next: drift slice C.

## Day summary — 2026-09-16/17 — Wave C provider sweep (13 slices, all pushed)

### Drift slice A — Baseten affinity committed + pushed

Upstream #9629 (6671c6047): `sendSessionAffinityHeaders` on all 20
Baseten entries. New catalog pin green, TDD red-first. pi-ai lib
474/474, pi-ai strict clippy clean, fmt clean, conversion 100.00%
(166/166), parity dashboard OK (58/318, upstream=d7296c0). No row
promoted; metrics unchanged. Committed `f83dc1e`, pushed, hashes
match. Next: drift slice B.

## Day summary — 2026-09-16/17 — Wave C provider sweep (13 slices, all pushed)

- 146b463 vLLM priority compat flag (#9004)
- 18009a1 Copilot GPT→Responses routing (#9253)
- 7ca3805 Codex unterminated SSE evidence (#9047, already-ported)
- c3666d9 + fa2b7f3 tool_choice gating then pin-correct revert (#8607
  superseded by 6b36eb592 — ledger records the correction honestly)
- 602392a reasoning-detail delta merge (#8605)
- 80da59b Mistral indexed tool-chunk merge (#8387)
- 5019e94 GLM-5.2 reasoning_effort (#9375)
- 9ac9ae9 supportsMaxOutputTokens (#8941)
- 9e7f98e Responses error provider labels (#9298)
- 492eca9 DeepSeek Flash refresh (#9423, counts 1354→1353)
- 0dd1d33 retired GPT-5.4 Codex models (#9394, counts 1353→1351)
- 26a3f6e Fireworks thinking metadata (#9323)
- 5ff99c4 Bedrock 1h cache-write pricing (#9457)

Assessed without slice (already ported or not narrowly expressible):
strict tool-schema conversion, Google thinking maps, xAI routing +
default, Copilot additional-tools compat, Codex end_turn, raw status
preservation, Kimi/Responses user-agent (superseded at pin),
Cloudflare binding-fetch successor (no fetch seam in this
distribution), assistant-frame surface (Wave B/E territory), pending
stop reason (27-file cross-cutting feature).

State: conversion 100.00% (166/166); parity dashboard OK
(upstream=d7296c0); metrics unchanged 58/318 (no row promoted —
all slices deterministic-only; live/vendor evidence stays OPEN).
pi-ai lib 474/474. Known reds, all verified identical on clean HEAD:
offline fd-download tests, session_env parallel flakes, copilot
gpt-4.1 filter, moonshot_and_nvidia list, pi-tui dead code + pi-agent
module-inception clippy.

Wave C provider lanes are now substantially closed. Recommended next:
Wave E visual parity lift-off — requires human eyeballing per row
(skill rule: never bulk-bless baselines), so it is a supervision
checkpoint, not autonomous work.

Upstream #9457 (8a7b0c03d): Bedrock 1h cache-write population from
`cacheDetails`. New usage/cost pin green, TDD red-first. pi-ai lib
474/474, pi-ai strict clippy clean, fmt clean, conversion 100.00%
(166/166), parity dashboard OK (58/318, upstream=d7296c0). No row
promoted; metrics unchanged. Committed `5ff99c4`, pushed, hashes
match. Next: next Wave C remainder or Wave E visual parity lift-off.

## Latest checkpoint — 2026-09-16 — Wave C slice: Bedrock 1h (uncommitted)

Upstream #9457 (8a7b0c03d): Bedrock 1h cache-write population from
`cacheDetails`. New usage/cost pin green, TDD red-first. pi-ai lib
474/474, pi-ai strict clippy clean, fmt clean, conversion 100.00%
(166/166), parity dashboard OK (58/318, upstream=d7296c0). No row
promoted; metrics unchanged. Next: commit + push, then next Wave C
remainder or Wave E visual parity.

Upstream #9323 (6b94ae2ec): Fireworks thinking metadata in vendored
data + pin; lane already honored the flags; Wave E GLM test updated
to corrected map. TDD red-first. pi-ai lib 473/473, catalog pin
green, pi-ai strict clippy clean, fmt clean, conversion 100.00%
(166/166), parity dashboard OK (58/318, upstream=d7296c0). No row
promoted; metrics unchanged. Committed `26a3f6e`, pushed, hashes
match. Next: next Wave C remainder or Wave E visual parity lift-off.

## Latest checkpoint — 2026-09-16 — Wave C slice: Fireworks thinking (uncommitted)

Upstream #9323 (6b94ae2ec): Fireworks thinking metadata in vendored
data + pin; lane already honored the flags; Wave E GLM test updated
to corrected map. TDD red-first. pi-ai lib 473/473, catalog pin
green, pi-ai strict clippy clean, fmt clean, conversion 100.00%
(166/166), parity dashboard OK (58/318, upstream=d7296c0). No row
promoted; metrics unchanged. Next: commit + push, then next Wave C
remainder or Wave E visual parity.

Upstream #9394 (2e6fe2f98): GPT-5.4/5.4-mini removed from the Codex
catalog; count pins 1353→1351. New retirement pin green, TDD
red-first. pi-ai lib 473/473, pi-ai strict clippy clean, fmt clean,
conversion 100.00% (166/166), parity dashboard OK (58/318,
upstream=d7296c0). No row promoted; metrics unchanged. Committed
`0dd1d33`, pushed, hashes match. Next: next Wave C remainder or Wave
E visual parity lift-off.

## Latest checkpoint — 2026-09-16 — Wave C slice: Codex retirement (uncommitted)

Upstream #9394 (2e6fe2f98): GPT-5.4/5.4-mini removed from the Codex
catalog; count pins 1353→1351. New retirement pin green, TDD
red-first. pi-ai lib 473/473, pi-ai strict clippy clean, fmt clean,
conversion 100.00% (166/166), parity dashboard OK (58/318,
upstream=d7296c0). No row promoted; metrics unchanged. Next: commit
+ push, then next Wave C remainder or Wave E visual parity.

Upstream #9423 (12f59336a): canonical `deepseek-flash` replaces
retired aliases + pricing refresh; count pins 1354→1353. New catalog
pin green, TDD red-first. pi-ai lib 473/473, pi-ai strict clippy
clean, fmt clean, conversion 100.00% (166/166), parity dashboard OK
(58/318, upstream=d7296c0). No row promoted; metrics unchanged.
Committed `492eca9`, pushed, hashes match. Next: retired GPT-5.4
Codex models (#9394) or next Wave C remainder / Wave E visual parity.

## Latest checkpoint — 2026-09-16 — Wave C slice: DeepSeek refresh (uncommitted)

Upstream #9423 (12f59336a): canonical `deepseek-flash` replaces
retired aliases + pricing refresh; count pins 1354→1353. New catalog
pin green, TDD red-first. pi-ai lib 473/473, pi-ai strict clippy
clean, fmt clean, conversion 100.00% (166/166), parity dashboard OK
(58/318, upstream=d7296c0). No row promoted; metrics unchanged.
Next: commit + push, then next Wave C remainder or Wave E visual
parity.

Upstream #9298 (0c7bb7c5c): Responses HTTP errors label the actual
provider. New loopback label pin green, TDD red-first. Drive-by
repaired stale pair-count pin (49→50). pi-ai lib 473/473, exhaustive
17/17, pi-ai strict clippy clean, fmt clean, conversion 100.00%
(166/166), parity dashboard OK (58/318, upstream=d7296c0). No row
promoted; metrics unchanged. Committed `9e7f98e`, pushed, hashes
match. Next: next Wave C remainder or Wave E visual parity lift-off.

## Latest checkpoint — 2026-09-16 — Wave C slice: error provider (uncommitted)

Upstream #9298 (0c7bb7c5c): Responses HTTP errors label the actual
provider. New loopback label pin green, TDD red-first. Drive-by
repaired stale pair-count pin (49→50). pi-ai lib 473/473, exhaustive
17/17, pi-ai strict clippy clean, fmt clean, conversion 100.00%
(166/166), parity dashboard OK (58/318, upstream=d7296c0). No row
promoted; metrics unchanged. Next: commit + push, then next Wave C
remainder or Wave E visual parity.

Upstream #8941 (b8b873b98): `supportsMaxOutputTokens` compat flag
(default true) + schema registration on the Responses lane. New wire
+ schema pins green, TDD red-first. pi-ai lib 473/473, model_config
15/15, pi-ai strict clippy clean, fmt clean, conversion 100.00%
(166/166), parity dashboard OK (58/318, upstream=d7296c0). No row
promoted; metrics unchanged. Committed `9ac9ae9`, pushed, hashes
match. Next: next Wave C remainder or Wave E visual parity lift-off.

## Latest checkpoint — 2026-09-16 — Wave C slice: max tokens opt-out (uncommitted)

Upstream #8941 (b8b873b98): `supportsMaxOutputTokens` compat flag
(default true) + schema registration on the Responses lane. New wire
+ schema pins green, TDD red-first. pi-ai lib 473/473, model_config
15/15, pi-ai strict clippy clean, fmt clean, conversion 100.00%
(166/166), parity dashboard OK (58/318, upstream=d7296c0). No row
promoted; metrics unchanged. Next: commit + push, then next Wave C
remainder or Wave E visual parity.

Upstream #9375 (4bd3f48df): `zai-glm-5-2` uses `reasoning_effort` on
the Mistral lane. New pin green, TDD red-first. pi-ai lib 472/472,
pi-ai strict clippy clean, fmt clean, conversion 100.00% (166/166),
parity dashboard OK (58/318, upstream=d7296c0). No row promoted;
metrics unchanged. Committed `5019e94`, pushed, hashes match. Next:
next Wave C remainder or Wave E visual parity lift-off.

## Latest checkpoint — 2026-09-16 — Wave C slice: GLM reasoning (uncommitted)

Upstream #9375 (4bd3f48df): `zai-glm-5-2` uses `reasoning_effort` on
the Mistral lane. New pin green, TDD red-first. pi-ai lib 472/472,
pi-ai strict clippy clean, fmt clean, conversion 100.00% (166/166),
parity dashboard OK (58/318, upstream=d7296c0). No row promoted;
metrics unchanged. Next: commit + push, then next Wave C remainder or
Wave E visual parity.

Upstream #8387 (6c87d9a02): Mistral tool chunks merge by index-first
keying. New fragment-merge pin green, TDD red-first. pi-ai lib
471/471, pi-ai strict clippy clean, fmt clean, conversion 100.00%
(166/166), parity dashboard OK (58/318, upstream=d7296c0). No row
promoted; metrics unchanged. Committed `80da59b`, pushed, hashes
match. Next: next Wave C remainder or Wave E visual parity lift-off.

## Latest checkpoint — 2026-09-16 — Wave C slice: Mistral merge (uncommitted)

Upstream #8387 (6c87d9a02): Mistral tool chunks merge by index-first
keying. New fragment-merge pin green, TDD red-first. pi-ai lib
471/471, pi-ai strict clippy clean, fmt clean, conversion 100.00%
(166/166), parity dashboard OK (58/318, upstream=d7296c0). No row
promoted; metrics unchanged. Next: commit + push, then next Wave C
remainder or Wave E visual parity.

Correction slice: prior gating ported a superseded intermediate;
pin-correct is verbatim forwarding + no summarization override.
Builder reverted with comment; both-shapes + caller pins green;
drive-by fixed pre-existing compaction test compile break. pi-ai lib
470/470, pi-agent lib 272/272, compaction 22/22, pi-ai strict clippy
clean, fmt clean, conversion 100.00% (166/166), parity dashboard OK
(58/318, upstream=d7296c0). No row promoted; metrics unchanged.
Committed `fa2b7f3`, pushed, hashes match. Next: next Wave C
remainder (assistant-frame compat) or Wave E visual parity lift-off.

## Latest checkpoint — 2026-09-16 — Wave C slice: pin-correct tool_choice (uncommitted)

Correction slice: prior gating ported a superseded intermediate;
pin-correct is verbatim forwarding + no summarization override.
Builder reverted with comment; both-shapes + caller pins green;
drive-by fixed pre-existing compaction test compile break. pi-ai lib
470/470, pi-agent lib 272/272, compaction 22/22, pi-ai strict clippy
clean, fmt clean, conversion 100.00% (166/166), parity dashboard OK
(58/318, upstream=d7296c0). No row promoted; metrics unchanged. Next:
commit + push, then next Wave C remainder or Wave E visual parity.

Upstream #8605 (c5ad7c1b0): completions streaming merges consecutive
reasoning text/summary deltas; encrypted stays discrete. New merge pin
green, TDD red-first (plus edition-2021 fixes). pi-ai lib 470/470,
pi-ai strict clippy clean, fmt clean, conversion 100.00% (166/166),
parity dashboard OK (58/318, upstream=d7296c0). No row promoted;
metrics unchanged. Committed `602392a`, pushed, hashes match. Next:
next Wave C remainder (compaction tool-choice removal, assistant-frame
compat) or Wave E visual parity lift-off.

## Latest checkpoint — 2026-09-16 — Wave C slice: reasoning merge (uncommitted)

Upstream #8605 (c5ad7c1b0): completions streaming merges consecutive
reasoning text/summary deltas; encrypted stays discrete. New merge pin
green, TDD red-first (plus edition-2021 fixes). pi-ai lib 470/470,
pi-ai strict clippy clean, fmt clean, conversion 100.00% (166/166),
parity dashboard OK (58/318, upstream=d7296c0). No row promoted;
metrics unchanged. Next: commit + push, then next Wave C remainder or
Wave E visual parity.

Upstream #8607 (fe37e9f9b): completions lane omits `tool_choice`
without tools, forwards with tools. Old test corrected + new pin, TDD
red-first. pi-ai lib 469/469, pi-ai strict clippy clean, fmt clean,
conversion 100.00% (166/166), parity dashboard OK (58/318,
upstream=d7296c0). No row promoted; metrics unchanged. Committed
`c3666d9`, pushed, hashes match. Next: next Wave C remainder or Wave E
visual parity lift-off.

## Latest checkpoint — 2026-09-16 — Wave C slice: tool_choice gating (uncommitted)

Upstream #8607 (fe37e9f9b): completions lane omits `tool_choice`
without tools, forwards with tools. Old test corrected + new pin, TDD
red-first. pi-ai lib 469/469, pi-ai strict clippy clean, fmt clean,
conversion 100.00% (166/166), parity dashboard OK (58/318,
upstream=d7296c0). No row promoted; metrics unchanged. Pre-existing
failures unchanged on clean HEAD. Next: commit + push, then next Wave
C remainder or Wave E visual parity.

Upstream #9047 already ported by construction (shared `SseParser::
finish()` + codex EOF drain). New trimmed-terminal pin green, no
source change. pi-ai lib 468/468, pi-ai strict clippy clean, fmt
clean, conversion 100.00% (166/166), parity dashboard OK (58/318,
upstream=d7296c0). No row promoted; metrics unchanged. Committed
`7ca3805`, pushed, hashes match. Next: next Wave C remainder or Wave E
visual parity lift-off.

## Latest checkpoint — 2026-09-16 — Wave C slice: Codex SSE EOF (uncommitted)

Upstream #9047 already ported by construction (shared `SseParser::
finish()` + codex EOF drain). New trimmed-terminal pin green, no
source change. pi-ai lib 468/468, pi-ai strict clippy clean, fmt
clean, conversion 100.00% (166/166), parity dashboard OK (58/318,
upstream=d7296c0). No row promoted; metrics unchanged. Next: commit +
push, then next Wave C remainder or Wave E visual parity.

Upstream #9253 (fixes #9209): Copilot `gpt-6-astra` moved from
`openai-completions` to `openai-responses` in vendored
`github-copilot.json` (Responses compat + full sibling thinking map).
New `copilot_gpt_models_route_through_responses_api` regression pin
green; pi-ai lib 467/467, pi-ai strict clippy clean, fmt clean,
conversion 100.00% (166/166), parity dashboard OK (58/318,
upstream=d7296c0). No row promoted; metrics unchanged.
`moonshot_and_nvidia` pin red on clean HEAD too (stale nvidia list,
unrelated). Committed `18009a1`, pushed, hashes match. Next: next Wave
C remainder (Cloudflare binding-fetch successor) or Wave E visual
parity lift-off.

## Latest checkpoint — 2026-09-16 — Wave C slice: Copilot GPT routing (uncommitted)

Upstream #9253 (fixes #9209): Copilot `gpt-6-astra` moved from
`openai-completions` to `openai-responses` in vendored
`github-copilot.json` (Responses compat + full sibling thinking map).
New `copilot_gpt_models_route_through_responses_api` regression pin
green; pi-ai lib 467/467, pi-ai strict clippy clean, fmt clean,
conversion 100.00% (166/166), parity dashboard OK (58/318,
upstream=d7296c0). No row promoted; metrics unchanged.
`moonshot_and_nvidia` pin red on clean HEAD too (stale nvidia list,
unrelated). Next: commit + push, then next Wave C remainder or Wave E
visual parity.

Upstream #9004 (`vllmPriority` compat → top-level `priority` on the
shared openai-completions lane): new `vllm_priority: Option<i64>`
(default omit), `getCompat` resolution, `build_params` injection before
thinking params, models.json schema accepts optional numeric
`vllmPriority`. Tests: pi-ai lib 467/467 (2 new pins), model_config
14/14 (1 new schema pin), pi-ai strict clippy clean, fmt clean,
conversion 100.00% (166/166), parity dashboard OK (58/318,
upstream=d7296c0). No row promoted; metrics unchanged. Pre-existing
clippy failures verified identical on clean HEAD (pi-tui dead code,
pi-agent module-inception). Committed `146b463`, pushed, hashes match.
Next: next Wave C remainder (Copilot Responses routing, Cloudflare
binding-fetch successor) or Wave E visual parity lift-off.

## Latest checkpoint — 2026-09-16 — Wave C slice: vLLM priority (uncommitted)

Upstream #9004 (`vllmPriority` compat → top-level `priority` on the
shared openai-completions lane): new `vllm_priority: Option<i64>`
(default omit), `getCompat` resolution, `build_params` injection before
thinking params, models.json schema accepts optional numeric
`vllmPriority`. Tests: pi-ai lib 467/467 (2 new pins), model_config
14/14 (1 new schema pin), pi-ai strict clippy clean, fmt clean,
conversion 100.00% (166/166), parity dashboard OK (58/318,
upstream=d7296c0). No row promoted; metrics unchanged. Pre-existing
clippy failures verified identical on clean HEAD (pi-tui dead code,
pi-agent module-inception). Next: commit + push, then next Wave C
remainder or Wave E visual parity.

Thinking persistence on the anthropic lane (0.85.1): pi-ai lib 465
green, clippy/fmt clean, conversion 100.00%, parity audits OK.
PROV-003/027 notes extended, no status change (58/318). Six
integration failures pre-existing on HEAD. Next: remaining provider
items (mid-conversation diagnostics UI, OAuth lanes) or visual parity
lift-off.

## Latest checkpoint — 2026-09-16 — Wave E slice 2 committed + pushed

Fireworks GLM thinking default + OpenRouter anthropic-lane affinity
(0.85.1): pi-ai lib 464 green, clippy/fmt clean, conversion 100.00%,
parity audits OK. PROV-010/027 notes extended, no status change
(58/318). Six integration failures pre-existing on HEAD. Next: Wave E
slice 3 (thinking persistence / mid-conversation effort) or visual
parity lift-off.

## Latest checkpoint — 2026-09-16 — Wave E slice 1 committed + pushed

Codex Off-effort default (#9191) + Mistral medium reasoning (#8700):
pi-ai lib 463 green, clippy/fmt clean, conversion 100.00%, parity
audits OK. PROV-019/024 notes extended, no status change (58/318).
Committed `cc36005`, pushed, hashes match. Next: Wave E slice 2
(Fireworks/GLM thinking, OpenRouter affinity headers) or visual
parity lift-off.

## Latest checkpoint — 2026-09-16 — Wave D slice 5 committed + pushed

Capability overrides (programmatic + PI_* env) + zed grouping +
settings wiring: pi-tui lib 409 green on rerun (flakes pre-existing),
clippy/fmt clean, conversion 100.00%, parity dashboard OK. TUI-043
note extended, no status change (58/318). Committed `d5e7e79`,
pushed, hashes match. Next: Wave E visual parity or remaining
provider items (thinking persistence).

## Latest checkpoint — 2026-09-16 — Wave D slice 4 committed + pushed

Input click-to-cursor + settings-list mouse (submenu/search routing,
shared visible range): pi-tui lib 408 + integration green, clippy/fmt
clean, conversion 100.00%, parity tui + dashboard OK. TUI-005/019
notes extended, no status change (58/318). Committed `7665ac2`,
pushed, hashes match. Next: Wave D slice 5 (terminal-image caps,
native-platform, layout) or Wave E visual parity.

## Latest checkpoint — 2026-09-16 — Wave D slice 3 committed + pushed

Editor click-to-place + autocomplete mouse routing (mutation-checked):
editor suites green, lib 404-406/406 (flakes pre-existing), clippy/fmt
clean, conversion 100.00%, parity tui + dashboard OK. TUI-006 note
extended, no status change (58/318). Committed `c5fdc52`, pushed,
hashes match. Next: Wave D slice 4 (input click-to-cursor,
settings-list/loader remaining) or Wave E visual parity.

## Latest checkpoint — 2026-09-16 — Wave D slice 2 committed + pushed

Search index cache, placeholder input + search prompt, select-list
mouse, centered editor borders: pi-tui lib 405, integration green
(flakes pre-existing), clippy/fmt clean, conversion 100.00%, parity
tui + dashboard OK. TUI-015/023/042 notes extended, no status change
(58/318). Committed `ca77e94`, pushed, hashes match. Next: Wave D
slice 3 (editor click-to-place, autocomplete-select mouse) or Wave E
visual parity.

## Latest checkpoint — 2026-09-16 — Wave D slice 1 committed + pushed

LaTeX join symbols, prompt-jump key alternates, bg-ansi tracker,
Loader.invalidate, Box mouse forwarding, MouseRegion, scrollbar
track/thumb split, mouse screen-coords + retarget: pi-tui lib 400
(3/4 runs green; flakes pre-existing on HEAD), clippy/fmt clean,
conversion 100.00%, parity tui + dashboard OK. TUI-014/026/045/047
notes extended, no status change (58/318). Committed `070d3bd`,
pushed, hashes match. Next: Wave D slice 2 (alt-screen search,
editor/input/select-list deltas).

## Latest checkpoint — 2026-09-16 — Wave C provider lanes committed + pushed

Explicit-cache TTL (`ttl: 30m`, no retention string on explicit-mode
models) + per-model compaction budgets (`modelOverrides`, wired
print/interactive/RPC): pi-ai lib 461, settings_sm 51, zero new lib
failures vs HEAD, clippy/fmt clean, conversion 100.00%, parity audits
OK. MODEL-008/SES-013 notes extended, no status change (58/318).
Committed `7736129`, pushed, hashes match. Next: Wave D TUI deltas
(or next Wave C remainder: thinking persistence).

## Latest checkpoint — 2026-09-16 — Wave B runtime surface committed + pushed

`RunContext`, tool memos, deferred-window options, `TextLine` splits,
session values, context-gated fs defaults (`crates/pi-agent`): pi-agent
lib 272/272 serial, run_context 4/4, wave_b2 4/4, workspace clippy/fmt
clean, conversion 100.00%, parity register + dashboard OK. AGENT-015
note extended, no status change (58/318). Committed `abdb07b`, pushed,
hashes match. Next: Wave C provider lanes.

## Latest checkpoint — 2026-09-16 — Wave A search service committed + pushed

`SessionSearchService` (0.85.1 contract) implemented scanning-backed in
`crates/pi-agent/src/search.rs` + exports: searchSessions/searchEntries
with top-entry hits, limit, sync/notify/remove/close. TDD with
mutation-checked limit test. pi-agent lib 272/272 serial, clippy/fmt
clean, conversion 100.00%, parity register + dashboard OK. AGENT-011
note extended, no status change (metrics 58/318). Committed `e4904cf`,
pushed, hashes match (`git rev-parse HEAD` == `git ls-remote origin
refs/heads/main`). Next: Wave B session-type reconciliation.

## Latest checkpoint — 2026-09-16 — re-pin board gate PASSED + next goal set

Board of experts (`board-of-expert-agents-review`): round 1 returned
1 REJECT + 4 CONDITIONAL (14 items); every item adjudicated against live
sources (9 CONFIRMED+fixed, 1 PARTLY, 1 refuted-as-blocker, history
restored verbatim); round 2 unanimous BUILD 5/5 with zero new blockers.
Fixes from review: model data refreshed from the 0.85.1 tarball (1354
models), OpenRouter two-lane port, session-header caller preservation,
fixture re-pin, terminal-test gaps, dashboard/history hygiene. Artifact:
`.unlazy/repin-review-20260916/spec.md` (r2). Next long-horizon goal
recorded at `.unlazy/repin-review-20260916/next-goal.md`: waves A–F
(search-service rewrite → session-type reconciliation → provider lanes →
TUI deltas → TUI visual parity → runtime closure) with installed skills
mapped per wave (no `mattpocock` skill exists locally; mapped installed
equivalents per user direction).
Current dashboard metrics:

Target is now `d7296c0` (v0.85.1, 2026-09-15). Both oracle checkouts at new
pin; audit constant + 267 row notes + source pin comments + workspace
version (0.85.1) + CHANGELOG data updated. Ported: `stream_deferred` split,
`maxAgentDelayMs` cap, terminal capability overrides,
`fullscreenCopyOnSelect`, `/tree` order, `x-opencode-session` header. No row
promoted; metrics unchanged (58/318). Green: pi-ai lib 461, pi-agent lib 270
serial, settings_sm 50, workspace check, strict clippy, fmt, `conversion_audit`
100.00% (166/166), `parity_audit` inventory/register/tui/dashboard all OK.
Pre-existing failures unchanged (offline fd-download, session_env flakes —
red on clean HEAD too). Worktree dirty (re-pin churn); no commit requested
yet — commit + push is the next step when authorized.
Current dashboard metrics:

Source/conversion ledger: 100.00% (166/166; 0 open)
Acceptance inventory census: 100.00% (318/318) (318 IDs indexed)
Acceptance scoring coverage: 100.00% (318/318) (318 of 318 IDs scored)
Root acceptance gates: 100.00% (8/8) (8 passed; 0 open)
Rust-only distribution boundary: 100.00% (0 JS/TS executable source files; generated Rustdoc excluded)
TUI functional implementation: 25.00% (13/52)
TUI test/evidence parity: 25.00% (13/52)
TUI visual/interaction parity: 0.00% (0/52)
TUI overall parity: 0.00% (0/52)
Non-TUI implementation parity: 41.73% (111/266 PASS; 155 PARTIAL; 0 OPEN)
Non-TUI deterministic evidence parity: 40.23% (107/266 PASS; 159 PARTIAL; 0 OPEN)
Non-TUI runtime-boundary parity: 22.18% (59/266 PASS; 156 PARTIAL; 51 OPEN)
Non-TUI overall parity: 21.80% (58/266)
Whole-product behavioral parity: 18.24% (58/318)

Next: commit + push, then agent search-service rewrite (S-066 scope),
OpenRouter anthropic-messages lane, Cloudflare gateway binding, GPT-5.6+
cache TTL, per-model compaction budgets. Files touched: `Cargo.toml`,
`parity_audit.rs` (pin), `NON-TUI-PARITY-STATUS.md` (pin + stale metric
repair), `models.rs` (stream_deferred), `retry.rs` (+cap), `settings.rs`
(+overrides/copy-on-select/retry tuple), `slash.rs` (/tree order),
`providers/all.rs` (session header), `agent_harness.rs`/`rich_agent.rs`
(retry field), `model.rs`/`partial_json.rs`/`easter_eggs.rs`/
`model_catalog.rs` (pin comments), `data/CHANGELOG.md` (refresh), tests
(settings_sm, run.rs, provider wrapper unit), README, GATES.md,
repository-description, CONVERSION-LEDGER.md, PLAN.md, this file.

## Upstream-drift checkpoint — 2026-09-15 — 26 days behind, re-pin recommended

Pinned oracle `5cd93f6` (2026-08-20) vs `origin/main` `d7296c0`
(2026-09-15): 4,954 first-parent commits behind across 26 days. Upstream
moved 0.84.2 → 0.85.1. Ported-source diff is 343 files, +37,579/−12,564
lines across `ai`/`agent`/`coding-agent`/`tui`/`session-backends`, including
~2,000 feat/fix commits touching ported src (provider payload changes,
thinking/reasoning mapping, session/fork/conformance, TUI rendering,
extension tool schemas, compaction budgets). No re-pin performed; no source
or parity row changed. Standing recommendation: re-pin the oracle and extend
the 318-row inventory with the new upstream surface before claiming further
1:1 progress, since current parity %s are measured against the August pin.

## Parity-goal confirmation — 2026-09-15 — 1:1 behavioral + Rust improvements

User-confirmed goal: 1:1 behavioral parity with upstream pi, except where a
pure-Rust implementation brings an obvious improvement (matches the standing
"Rust-only distribution boundary" and the S-027/S-066 scope rulings).

Divergence inventory re-audited against that bar (all must stay recorded, not
silently absorbed):
- Superset inputs (ENV-001/002/007: startup `PI_PROVIDER`/`PI_MODEL`/`PI_KEY`/
  `PI_REASONING_LEVEL`): Rust accepts strictly more inputs than upstream while
  preserving upstream behavior when they are unset. Compatible with 1:1 as
  additive surface; must remain pinned as superset, never as changed default.
- Self-update (DIST-004: no in-place binary self-replacement; `pi update`
  exits nonzero naming the rebuild path) and no-JS extension execution
  (S-027: Rust-native factories only): inherent to a compiled Rust
  distribution — qualifies as the "obvious Rust improvement" exception, stays
  recorded.
- Startup proxy validation (ENV-013: fail once at startup vs upstream lazy
  per-request throw): fail-closed timing shift, stricter not looser. Qualifies
  as improvement; stays recorded with the no-network-turn caveat.
- `PI_VERSION` ignored / no release lookup (ENV-009): narrower than 1:1 only
  if upstream's lookup is deemed behavior worth matching; currently recorded
  as divergence, stays open as PARTIAL.
- CLI-038 (tmux-only PTY evidence): evidence-scope limit, not a behavioral
  divergence; row contract itself is 1:1.

No source, test, or parity row changed by this confirmation. Audits rerun:
`conversion_audit -- all` → `Conversion progress: 100.00% (166/166; 0 open)`;
`parity_audit -- dashboard` → `PARITY_DASHBOARD_OK`, whole-product 18.24%
(58/318). Not committed/pushed (no commit requested).

## Parity-query checkpoint — 2026-09-15 — no ledger change

Read-only assessment; no source, test, or ledger item changed.
- `cargo run -p pi-coding-agent --bin conversion_audit -- all` → `Conversion progress: 100.00% (166/166; 0 open)`, audit blockers 0.
- `cargo run -p pi-coding-agent --bin parity_audit -- dashboard` → `PARITY_DASHBOARD_OK`; whole-product 18.24% (58/318), non-TUI overall 21.80% (58/266), TUI overall 0.00% (0/52).
- Requested `mattpocock` skill is not installed (no match in `~/.config/opencode/skills` or `~/.agents/skills`); assessment used repo-native audits only.
- PLAN.md, CONVERSION-LEDGER.md, and this file carry identical dashboard blocks; no metric edits needed. Not committed/pushed (no commit requested).
