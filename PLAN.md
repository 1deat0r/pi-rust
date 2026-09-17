# Pi in Rust — 1:1 Rewrite Plan

> Active planning window: the 2026-09-18 parity-finish goal below plus
> the 2026-09-17 day goal. Older checkpoints (2026-09-16 re-pin and
> earlier) are frozen in `docs/PLAN-ARCHIVE-2026-08.md` — do not
> duplicate them here; append new slices at the top.

## Day goal 2026-09-18 — finish 100% 1:1 parity (pi agent ↔ pi-rust)

### Tool slice I — signal-terminated shell exit codes

Ported upstream #9577 (a8b3dd19): shared executor maps signal
termination to 128+signo (KILL→137, TERM→143; neither→1) via
`ExitStatusExt`; no caller change needed (rejection path already
carries partial output). New Unix-gated pin, TDD red-first. No row
promoted (tool slice; platform breadth open). Gate green: tools
23/23, pi-agent lib 276/276, scoped rustfmt, conversion 100.00%
(166/166), parity dashboard OK (upstream=d7296c0, 58/318).
Pre-existing clippy verified identical on clean HEAD. Next: commit +
push, then continue the new-drift triage.

### Provider slice H — Azure peak-load retry

Ported upstream #9669 (e98f287ee): `"currently experiencing high
demand"` joins the retryable patterns with an oracle-mirroring pin.
No row promoted (provider deterministic slice). Gate green: pi-ai
lib 486/486, retry 33/33, pi-ai strict clippy, fmt, conversion
100.00% (166/166), parity dashboard OK (upstream=d7296c0, 58/318).
Next: commit + push, then continue the new-drift triage
(remaining: unsubscribe, forced-prompts, changelog-only,
signal-shell, vercel-thinking, thinking-notices, eval-harness x2,
thinking-replay, gemini-levels).

### Provider slice G — Cloudflare 520 retry

Ported upstream #9627 (e5d1838): `"520"` joins the retryable status
patterns (RED pin for the exact oracle wording was pre-staged).
No row promoted (provider deterministic slice). Gate green: pi-ai
lib 485/485, retry 32/32, pi-ai strict clippy, fmt, conversion
100.00% (166/166), parity dashboard OK (upstream=d7296c0, 58/318).
Next: commit + push, then triage the 13-commit new drift
(e4c75a732 → 46c9de40).

### Doc-hygiene pass — documentation system optimization

Documentation-only: lint config + `scripts/docs-lint.sh`, portable
pre-commit hook (exit 0 verified), README status rewrite with current
counts, HANDOFF/PLAN archive sweep (6077→~950 / 4414→~500 lines),
GATES.md frozen record, dashboard retired-narrative banner, stale
note corrections, AGENTS.md doc map. No source/test/row changed.
Gate green: docs-lint 0 issues, conversion 100.00% (166/166), parity
dashboard OK (upstream=d7296c0, 58/318). Next: commit + push, then
continue the parity sweep.

## Day goal 2026-09-17 — sectioned transcript-replay port

### Session slice N — header-only exact-id lookup

Ported upstream #9601 (9b791a4cc fixing #9440): new
`JsonlSessionRepo::find_by_id` reads headers only (first lines),
skips corrupt files, `None` on missing roots. Pin covers hit/miss/
decoy/missing-root, TDD red-first. No row promoted (session slice;
restart evidence open). Gate green: jsonl_repo 17/17, pi-agent lib
276/276, scoped rustfmt, conversion 100.00% (166/166), parity
dashboard OK (upstream=d7296c0, 58/318). Pre-existing clippy
verified identical on clean HEAD. Next: commit + push, then
continue the sweep.

### Extension slice L — user_bash fail-closed

Ported upstream #9068 (509ee2bd0, previously assessed blocked —
runner half is portable): `emit_user_bash` validates results and
fails closed on handler errors (report + propagate, no fallback).
Two pins (error + 6-shape invalid matrix), TDD red-first. No row
promoted (extension slice; routing + live execution open). Gate
green: extensions lib 74/74, extensions_parity 9/9, scoped rustfmt,
conversion 100.00% (166/166), parity dashboard OK
(upstream=d7296c0, 58/318). Pre-existing clippy failures verified
identical on clean HEAD. Next: commit + push, then continue the
sweep.

### Resource slice M — symlink-root pin

Closed the RES-006 symlink residual with a recursive-discovery pin
(evidence-only: discovery already follows symlinks; absent roots
stay empty, never error; row note extended, held). No row promoted
(installed/missing-package breadth open). Gate green: new pin 1/1,
scoped rustfmt, conversion 100.00% (166/166), parity dashboard OK
(upstream=d7296c0, 58/318). Pre-existing ignore-file pin failure
verified identical on clean HEAD. Next: commit + push, then continue
the sweep.

### Extension slice K — UI prompt events

Ported upstream #8355 (ccfe79ed2): nesting-aware prompt emitter
installed on handler contexts; 5 dialogs wrapped; failures
isolated. Four pins. No row promoted (extension slice). Gate
green: extensions 72/72, fmt, conversion 100.00% (166/166),
parity dashboard OK (upstream=d7296c0, 58/318). Next: commit +
push, then continue the sweep.

### Session slice L — discovery symlink pin

Closed the SES-007 symlink residual with a real-filesystem pin
(no source change; row note extended, statuses held). No row
promoted (restart evidence open). Gate green: jsonl_repo 16/16,
pi-agent lib 276/276, fmt, conversion 100.00% (166/166), parity
register + dashboard OK (upstream=d7296c0, 58/318). Next: commit
+ push, then continue the sweep.

### Tool slice J — PowerShell tool

Ported upstream #8512 (80e62761f): optional powershell tool with
discovery + args + UTF8 prefix (shell_args seam, default set
untouched). Three pins. No row promoted (tool slice; Windows
execution offline-unverifiable). Gate green: pi-agent lib 276/276,
fmt, conversion 100.00% (166/166), parity dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then continue
the sweep.

### Extension slice I — tool schema validation

Ported upstream #9300 (acaa253cc): `register_tool` rejects
non-object schemas at registration. New 4-shape pin. No row
promoted (extension slice). Gate green: loader 16/16, fmt,
conversion 100.00% (166/166), parity dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then continue
the sweep.

### Resolver slice H — Radius default

Ported upstream 9767ba275 default (`balanced`; fallback existed;
no post-login flow to defer). Pin updated. No row promoted
(config slice). Gate green: scoped 41/41, fmt, conversion 100.00%
(166/166), parity dashboard OK (upstream=d7296c0, 58/318). Next:
commit + push, then continue the sweep.

### Interactive slice W — share isolation

Ported upstream #8613 (6f35de5b5): unique temp dir per share
(`share_via_gist` extraction, cleanup on all paths). New
concurrent-shares pin. No row promoted (interactive slice). Gate
green: share 4/4, fmt, conversion 100.00% (166/166), parity
dashboard OK (upstream=d7296c0, 58/318). Next: commit + push,
then continue phase 1.

### ENV slice V — cache TTL breadth

Closed the ENV-011 cross-provider residual with an anthropic TTL
unit pin (no source change; row note extended, held). No row
promoted (live breadth open). Gate green: pi-ai lib 484/484, fmt,
conversion 100.00% (166/166), parity register + dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then continue
phase 1.

### CFG slice AA — migration no-rewrite

Closed the CFG-004 restart residual with a no-rewrite process pin
(TDD red-first on wrong expectation, corrected by archaeology; no
source change; row note extended, held). No row promoted
(matrices open). Gate green: config 5/5, fmt, conversion 100.00%
(166/166), parity register + dashboard OK (upstream=d7296c0,
58/318). Next: commit + push, then continue phase 1.

### CFG slice Z — unknown-key retention

Closed the CFG-001 retention residual with a save round-trip pin
(TDD red-first on async flush). No row promoted (permission
breadth open). Gate green: settings 35/35, fmt, conversion
100.00% (166/166), parity register + dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then continue
phase 1.

### Agent slice Y — truncated summaries

Ported upstream #7048 (97fa14e39): shared failure helper
rejects length-stop summaries at 3 sites. New helper + branch
pin. No row promoted (agent slice). Gate green: compaction 24/24,
pi-agent lib 276/276, fmt, conversion 100.00% (166/166), parity
dashboard OK (upstream=d7296c0, 58/318). Next: commit + push,
then continue phase 1.

### Agent slice X — summary output cap

Ported upstream #8845 (e44d75c20): branch summary caps at
min(4096, model.maxTokens). New cap pin. No row promoted (agent
slice). Gate green: compaction 23/23, pi-agent lib 276/276, fmt,
conversion 100.00% (166/166), parity dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then continue
phase 1.

### CLI slice U — unknown-pattern evidence

Closed the CLI-036 unknown-pattern residual with a real-process
pin (no source change; row note extended, held). No row promoted
(refresh breadth open). Gate green: list-models 4/4, fmt,
conversion 100.00% (166/166), parity register + dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then continue
phase 1.

### CLI slice T — template variable evidence

Closed the CLI-033 variable residual with a real-process pin (no
source change; row note extended, held). No row promoted
(precedence breadth open). Gate green: resources 11/11, fmt,
conversion 100.00% (166/166), parity register + dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then continue
phase 1.

### CLI slice S — skill prompt-inclusion evidence

Closed the CLI-032 prompt-inclusion residual with a usage process
pin (no source change; row note extended, held). No row promoted
(precedence breadth open). Gate green: resources 10/10, fmt,
conversion 100.00% (166/166), parity register + dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then continue
phase 1.

### CLI slice R — no-tools usage evidence

Closed the CLI-023/024 payload residual with a JSON-usage process
pin (no source change; both row notes extended, held). No row
promoted (payload breadth open). Gate green: JSON 9/9, fmt,
conversion 100.00% (166/166), parity register + dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then continue
phase 1.

### CLI slice Q — exclusion projection evidence

Closed the CLI-022 projection residual with a JSON-usage process
pin (no source change; row note extended, held). No row promoted
(extension breadth open). Gate green: JSON 8/8, fmt, conversion
100.00% (166/166), parity register + dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then continue
phase 1.

### CLI slice P — export overwrite evidence

Closed the CLI-029 overwrite/suffix residual with a real-process
pin (no source change; row note extended, held). No row promoted
(XSS breadth open). Gate green: export 5/5, fmt, conversion
100.00% (166/166), parity register + dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then continue
phase 1.

### CLI slice O — unknown-tool evidence

Closed the CLI-021 unknown-tool process residual with a real-
process pin (no source change; row note extended, held). No row
promoted (extension breadth open). Gate green: flag matrix 7/7,
fmt, conversion 100.00% (166/166), parity register + dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then continue
phase 1.

### CLI slice N — invalid-catalog evidence

Closed the CLI-020 invalid-catalog process residual with a real-
process pin (no source change; row note extended, held). No row
promoted (collision breadth open). Gate green: config 4/4, fmt,
conversion 100.00% (166/166), parity register + dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then continue
phase 1.

### CLI slice M — name persistence evidence

Closed the CLI-019 newline/restart process residual with a real-
process pin (no source change; row note extended, held). No row
promoted (display breadth open). Gate green: restart 12/12, fmt,
conversion 100.00% (166/166), parity register + dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then continue
phase 1.

### Maintenance — stale catalog pins

Repaired nvidia id list + copilot filter fixture to vendored data
(both red on clean HEAD). Full catalog 14/14, copilot 5/5 green.
No row promoted (test-only). Gate green: pi-ai lib 483/483, pi-ai
strict clippy, fmt, conversion 100.00% (166/166), parity dashboard
OK (upstream=d7296c0, 58/318). Next: commit + push, then continue
the sweep.

### Agent slice G — proxy EOF error

Ported upstream #8997 (ebc374490): proxy EOF without terminal
event finalizes an error (pusher terminal tracking). New loopback
pin, true-RED TDD via hang. No row promoted (agent deterministic
slice). Gate green: pi-agent lib 273/273, proxy 13/13, fmt,
conversion 100.00% (166/166), parity dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then continue
the sweep.

### Provider slice F — Google transient retry

Ported upstream #7471 (b9d360a2c): generative-ai initial request
retries transient errors/statuses per maxRetries (Vertex-mirror
loop). New 503→200 / 503-terminal loopback pin, TDD red-first.
No row promoted (provider deterministic slice). Gate green: pi-ai
lib 483/483, exhaustive 18/18, pi-ai strict clippy, fmt,
conversion 100.00% (166/166), parity dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then continue
the pin-era sweep.

### Transcript leaf 4 — initial-message helpers

Ported upstream `createInitialSystemMessage`/`normalizeContext`/
initial/strip helpers with a fold/strip pin, no callers yet. No
row promoted (new surface). Gate green: pi-ai lib 483/483, pi-ai
strict clippy, fmt, conversion 100.00% (166/166), parity dashboard
OK (upstream=d7296c0, 58/318). Next: commit + push, then wave (b)
adapter signatures (flag-day migration, queued with loop +
persistence).

### Transcript wave (a) — mid-convo capability flags

Parsed `supportsMidConvoSystemMessages`/`supportsMidConvoToolChanges`
+ model-id gate with 2 pins (dead-code allows with wave-(b) notes
per precedent; readers land with plumbing). No row promoted (no
behavior change). Gate green: pi-ai lib 482/482, pi-ai strict
clippy, fmt, conversion 100.00% (166/166), parity dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then wave (b)
(TranscriptContext adapter signatures).

### Transcript leaf 2 — section wrapping

Ported upstream section wrapping (`buildSystemPromptSections`
tail): pure helpers + pin, no callers yet. No row promoted (new
surface). Gate green: pi-ai lib 480/480, pi-ai strict clippy, fmt,
conversion 100.00% (166/166), parity dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then leaf 3
(provider adoption scoping).

### Transcript leaf 1 — replay utils

Ported upstream transcript replay surface (`transcript.ts` at
e4c75a732): `SystemMessage`/`ToolReference` types + full replay
module with 5 pins mirroring upstream tests. No callers yet. No
row promoted (new surface). Gate green: pi-ai lib 479/479, pi-ai
strict clippy, fmt, conversion 100.00% (166/166), parity dashboard
OK (upstream=d7296c0, 58/318). Next: commit + push, then leaf 2
(sectioned prompt builder).

### Drift slice E — Google thinking maps

Ported upstream #9455 (aa50fe778): thinking maps derive from
models.dev effort metadata for 32 Google/Vertex/OpenCode models
(Gemma 4 fallback kept). New pin mirrors upstream providers test,
TDD red-first. No row promoted (data-correction slice). Gate green:
pi-ai lib 474/474, catalog pin, pi-ai strict clippy, fmt,
conversion 100.00% (166/166), parity dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then drift slice F.

### Drift slice D — image refresh

Ported post-pin image refresh (bdee230f1): 4 renames + 2 additions.
New catalog pin, TDD red-first. No row promoted (data-correction
slice). Gate green: pi-ai lib 474/474, catalog pin, pi-ai strict
clippy, fmt, conversion 100.00% (166/166), parity dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then drift slice E.

### Drift slice C — clipboard fail-closed

Ported upstream #9618 (3349e1db1; renderer half already ported):
OSC 52 fallback is remote-only; local failures fail closed. New
PATH-shimmed pin, TDD red-first. No row promoted (TUI deterministic
slice). Gate green: clipboard 6/6, mermaid 3/3, fmt, conversion
100.00% (166/166), parity dashboard OK (upstream=d7296c0, 58/318).
Pre-existing failures unchanged on clean HEAD. Next: commit + push,
then drift slice D.

### Drift slice B — fallback overrides

Ported upstream #9294 (b03a367a4): `allowedFallbackModels` shape
validation (≤3 entries, non-empty provider/model + cost; `[]`
disables). Lane + merge already supported it. New schema + merge
pins, TDD red-first. No row promoted (config deterministic slice).
Gate green: scoped suites 32/32, fmt, conversion 100.00% (166/166),
parity dashboard OK (upstream=d7296c0, 58/318). Pre-existing
failures unchanged on clean HEAD. Next: commit + push, then drift
slice C.

### Drift slice A — Baseten session affinity

Ported upstream #9629 (6671c6047): `sendSessionAffinityHeaders`
on all 20 Baseten entries (lane already honored it). New catalog
pin, TDD red-first. No row promoted (data-correction slice). Gate
green: pi-ai lib 474/474, catalog pin, pi-ai strict clippy, fmt,
conversion 100.00% (166/166), parity dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then drift slice B.

## Active 2026-09-16 checkpoint — Wave C slice: Bedrock 1h

Ported upstream #9457 (8a7b0c03d): Bedrock 1h cache-write
population from `cacheDetails` (cost side already handled). New
usage/cost pin, TDD red-first. No row promoted (provider-lane
deterministic slice). Gate green: pi-ai lib 474/474, pi-ai strict
clippy, fmt, conversion 100.00% (166/166), parity dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then next Wave C
remainder or Wave E visual parity lift-off.

## Active 2026-09-16 checkpoint — Wave C slice: Fireworks thinking

Ported upstream #9323 (6b94ae2ec): Fireworks thinking metadata
(unsigned replay, adaptive scoping, fallback maps, alias collapse)
in vendored data + pin; lane already honored the flags. TDD
red-first. No row promoted (data-correction slice). Gate green:
pi-ai lib 473/473, catalog pin, pi-ai strict clippy, fmt,
conversion 100.00% (166/166), parity dashboard OK (upstream=d7296c0,
58/318). Next: commit + push, then next Wave C remainder or Wave E
visual parity lift-off.

## Active 2026-09-16 checkpoint — Wave C slice: Codex retirement

Ported upstream #9394 (2e6fe2f98): GPT-5.4/5.4-mini removed from the
Codex catalog; count pins 1353→1351. New retirement pin, TDD
red-first. No row promoted (data-correction slice). Gate green:
pi-ai lib 473/473, catalog pins, pi-ai strict clippy, fmt,
conversion 100.00% (166/166), parity dashboard OK (upstream=d7296c0,
58/318). Next: commit + push, then next Wave C remainder or Wave E
visual parity lift-off.

## Active 2026-09-16 checkpoint — Wave C slice: DeepSeek refresh

Ported upstream #9423 (12f59336a): canonical `deepseek-flash`
replaces 2 retired aliases + pricing refresh (flash + v4-pro); count
pins 1354→1353. New catalog pin, TDD red-first. No row promoted
(data-correction slice). Gate green: pi-ai lib 473/473, catalog pin,
pi-ai strict clippy, fmt, conversion 100.00% (166/166), parity
dashboard OK (upstream=d7296c0, 58/318). Next: commit + push, then
next Wave C remainder or Wave E visual parity lift-off.

## Active 2026-09-16 checkpoint — Wave C slice: error provider

Ported upstream #9298 (0c7bb7c5c): Responses HTTP errors label the
actual provider. New opencode/openai loopback label pin, TDD
red-first. Drive-by repaired stale pair-count pin (49→50). No row
promoted (provider-lane deterministic slice). Gate green: pi-ai lib
473/473, exhaustive 17/17, pi-ai strict clippy, fmt, conversion
100.00% (166/166), parity dashboard OK (upstream=d7296c0, 58/318).
Next: commit + push, then next Wave C remainder or Wave E visual
parity lift-off.

## Active 2026-09-16 checkpoint — Wave C slice: max tokens opt-out

Ported upstream #8941 (b8b873b98): `supportsMaxOutputTokens`
compat flag (default true) gates `max_output_tokens` on the
Responses lane + schema registration. New wire + schema pins, TDD
red-first. No row promoted (provider-lane deterministic slice).
Gate green: pi-ai lib 473/473, model_config 15/15, pi-ai strict
clippy, fmt, conversion 100.00% (166/166), parity dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then next Wave C
remainder or Wave E visual parity lift-off.

## Active 2026-09-16 checkpoint — Wave C slice: GLM reasoning

Ported upstream #9375 (4bd3f48df): `zai-glm-5-2` uses
`reasoning_effort` on the Mistral lane. New pin, TDD red-first. No
row promoted (provider-lane deterministic slice). Gate green: pi-ai
lib 472/472, pi-ai strict clippy, fmt, conversion 100.00% (166/166),
parity dashboard OK (upstream=d7296c0, 58/318). Next: commit + push,
then next Wave C remainder or Wave E visual parity lift-off.

## Active 2026-09-16 checkpoint — Wave C slice: Mistral merge

Ported upstream #8387 (6c87d9a02): Mistral tool chunks merge by
`index ?? callId` (Rust: `index:{n}` vs `id:{callId}` keys). New
fragment-merge pin, TDD red-first. No row promoted (provider-lane
deterministic slice). Gate green: pi-ai lib 471/471, pi-ai strict
clippy, fmt, conversion 100.00% (166/166), parity dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then next Wave C
remainder or Wave E visual parity lift-off.

## Active 2026-09-16 checkpoint — Wave C slice: pin-correct tool_choice

Correction: prior gating slice ported superseded intermediate state.
Pin-correct contract (d7296c0) is verbatim `tool_choice` forwarding +
no caller override in summarization. Builder reverted with sequence
comment; both-shapes pin; new pi-agent caller pin; drive-by fix of
pre-existing compaction test compile break. No row promoted. Gate
green: pi-ai lib 470/470, pi-agent lib 272/272, compaction 22/22,
pi-ai strict clippy, fmt, conversion 100.00% (166/166), parity
dashboard OK (upstream=d7296c0, 58/318). Next: commit + push, then
next Wave C remainder or Wave E visual parity lift-off.

## Active 2026-09-16 checkpoint — Wave C slice: reasoning merge

Ported upstream #8605 (c5ad7c1b0): completions streaming merges
consecutive reasoning text/summary deltas (concat payload, first
signature wins, id/format/index fill; encrypted stays discrete). New
merge pin, TDD red-first. No row promoted (shared-lane deterministic
slice). Gate green: pi-ai lib 470/470, pi-ai strict clippy, fmt,
conversion 100.00% (166/166), parity dashboard OK (upstream=d7296c0,
58/318). Next: commit + push, then next Wave C remainder or Wave E
visual parity lift-off.

## Active 2026-09-16 checkpoint — Wave C slice: tool_choice gating

Ported upstream #8607 (fe37e9f9b): completions lane omits
`tool_choice` when the payload has no tools, forwards verbatim with
tools. Old test corrected to the new contract + new omit/forward pin.
TDD red-first. No row promoted (shared-lane deterministic slice).
Gate green: pi-ai lib 469/469, pi-ai strict clippy, fmt, conversion
100.00% (166/166), parity dashboard OK (upstream=d7296c0, 58/318).
Pre-existing failures unchanged on clean HEAD (copilot gpt-4.1 filter,
nvidia list). Next: commit + push, then next Wave C remainder or Wave E
visual parity lift-off.

## Active 2026-09-16 checkpoint — Wave C slice: Codex SSE EOF evidence

Upstream #9047 (64eeb82a4) already ported by construction: the shared
SSE parser flushes residual frames at EOF and the codex path drains
`finish()` after the byte loop. New
`processes_terminal_sse_event_without_trailing_blank_line` pin proves
stop + text on a trimmed terminal fixture; no source change. No row
promoted (evidence-only). Gate green: pi-ai lib 468/468, pi-ai strict
clippy, fmt, conversion 100.00% (166/166), parity dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then next Wave C
remainder or Wave E visual parity lift-off.

## Active 2026-09-16 checkpoint — Wave C slice: Copilot GPT routing

Ported upstream #9253 (fixes #9209): Copilot `gpt-6-astra` moved from
`openai-completions` to `openai-responses` in the vendored catalog with
Responses compat + full sibling thinking map; new
`copilot_gpt_models_route_through_responses_api` regression pin (every
Copilot `gpt-*` on Responses). No row promoted (data-correction slice;
live Copilot traffic offline-unverifiable). Gate green: new pin + pi-ai
lib 467/467, pi-ai strict clippy, fmt, conversion 100.00% (166/166),
parity dashboard OK (upstream=d7296c0, 58/318).
`moonshot_and_nvidia` pin red on clean HEAD too (stale nvidia list,
unrelated). Next: commit + push, then next Wave C remainder
(Cloudflare binding-fetch successor) or Wave E visual parity lift-off.

## Active 2026-09-16 checkpoint — Wave C slice: vLLM priority compat flag

Ported upstream #9004 (`vllmPriority` compat → top-level `priority` on
the shared openai-completions lane): new `vllm_priority: Option<i64>`
compat field (default omit), `getCompat` resolution from model compat,
`build_params` injection before thinking params, models.json schema
accepts optional numeric `vllmPriority`. Tests: 2 pi-ai pins
(resolution + set/omit wire payload), 1 model-config schema pin (accept
numeric / reject string). No row promoted (shared-lane deterministic
slice; live vLLM scheduler behavior offline-unverifiable). Gate green:
pi-ai lib 467/467, model_config 14/14, pi-ai strict clippy, fmt,
conversion 100.00% (166/166), parity dashboard OK (upstream=d7296c0,
58/318). Pre-existing clippy failures unchanged on clean HEAD (pi-tui
dead code, pi-agent module-inception). Next: commit + push, then next
Wave C remainder (Cloudflare binding-fetch successor, Copilot Responses
routing) or Wave E visual parity lift-off.
