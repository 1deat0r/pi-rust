# Pi in Rust — 1:1 Rewrite Plan

> Active planning window: the 2026-09-18 parity-finish goal below plus
> the 2026-09-17 day goal. Older checkpoints (2026-09-16 re-pin and
> earlier) are frozen in `docs/PLAN-ARCHIVE-2026-08.md` — do not
> duplicate them here; append new slices at the top.

## Day goal 2026-09-18 — finish 100% 1:1 parity (pi agent ↔ pi-rust)

### Session slice AH — pure-sdk v3 bad-ts/missing-cwd open+import via migration

Closed the slice AG follow-up (genuine RED 2/2): pure-sdk
`open_session` / `import_prepared_session` now run
`migrate_legacy_session_file` before `read_session_metadata`, so a
v3 header with a bad `timestamp` or missing `cwd` is accepted the
same way the CLI path already is (oracle type+id only; slice AF
CLI pins stay green). Two new pins in session_import_parity (6/6).
Intentional divergence noted: Rust rewrites v3→v4 on open/import
(match SES-006 migration); oracle leaves valid v3 alone — no oracle
pins version-after-import. Mid-slice gate blocker also fixed: RPC
golden failed on clean HEAD because ancestor `Projects/AGENTS.md`
(mtime today) leaked into faux usage via context-file discovery —
hermeticized `runtime_for_test_with_settings` with
`--no-context-files` and surgically refreshed the fixture numbers
(5498→938 compact, stats 3052/2743/5816→772/463/1256). No row
promoted (SES-012 note extended). Gate green: import 6/6, invalid
12/12, lib 918 (golden ok), restart 13/13, resources 11/11, flag
7/7, clean_home 12/12, print 14/14, json 9/9, exhaustive 6/6,
runtime 3/3, extensions 10/10, file safety 2/2, session_id 1/1,
export 5/5, sb 13 suites, clippy 0, fmt, diff, conversion
100.00% (166/166). Next: commit + push, then continue the parity
sweep.

### Session slice AG — sdk read_session_metadata v4→v3 fallback

Closed the sdk v3-import/open divergence (genuine RED 2/2): oracle
`importFromJsonl` validates type+id only and accepts a v3
header-only file (agent-session-runtime.test.ts:229-245); Rust
`read_session_metadata` was v4-only. Dual-parse fallback (v4 then
v3) matches `JsonlSessionStorage::load`; dual-Err preserves the v4
`invalid session header` diagnostic. Two new pins in
session_import_parity (4/4). Follow-up noted: pure-sdk path does
not migrate v3 bad-ts/missing-cwd (oracle import does not migrate
either — CLI `--session` still migrates via SES-006/slice AF).
No row promoted (SES-012 note extended). Gate green: import 4/4,
invalid 12/12, lib 918, restart 13/13, resources 11/11, flag 7/7,
clean_home 12/12, print 14/14, json 9/9, exhaustive 6/6, runtime
3/3, extensions 10/10, file safety 2/2, session_id 1/1, export
1/1, sb 13 suites, clippy 0, fmt, diff, conversion 100.00% (166/166).
Next: commit + push, then continue the parity sweep (slice AG
committed `e8f275f` + pushed).

### Session slice AF — bad-timestamp / missing-cwd acceptance pins

Closes the slice AE follow-up with pins only: migration's
`iso_timestamp_to_ms` fallback absorbs bad timestamps (rewrites as
v4 before strict parse) and empty-cwd is falsy-guarded upstream, so
the open chain already accepts both. Two characterization pins
(green on first run); session_file_invalid now 12/12. No code
change, no row promoted (CLI-014 note extended). Gate green: all
prior targets, clippy 0, fmt, diff, conversion 100.00% (166/166).
Next: commit + push, then continue the parity sweep.

### Session slice AE — bad-header refusal family

Oracle `loadEntriesFromFile` validates only `type` + string `id`.
Three fixes (RED 3/3): migration skips id-less v3 headers (file
untouched, no migration-error leak); metadata v3 strict-parse
failure → intent refusal; v4 missing-id → intent refusal. Three
new pins (open friendly x2 + fork Cannot-fork); session_file_
invalid now 10/10. Follow-up recorded: v3-bad-timestamp-with-id
is oracle-accepted (needs lenient parse + missing-cwd audit).
No row promoted (CLI-014/CLI-016/SES-006 notes extended). Gate
green: target 10/10, lib 918, all prior targets, clippy 0
warnings (two new doc warnings fixed in-slice), fmt, diff,
conversion 100.00% (166/166). Next: commit + push, then
continue the parity sweep.

### Session slice AD — missing-path create + fork-missing Cannot-fork

Second slice-AA-family misattribution corrected (RED 2/2):
`--session <missing, parent exists>` now creates at the explicit
path per oracle `_setSessionFile` else-branch (old "session not
found" pin was a 2a9284b Rust invention — rewritten with
provenance); `--fork <path-like missing>` now refuses with
oracle `Cannot-fork` early at validate (createSessionManager
order). Open defers missing-path creation to prepare so
no-models fails before any file appears (oracle lazy parity).
No row promoted (CLI-014 + CLI-016 notes extended). Gate green:
cli_resources 11/11, session_file_invalid 7/7, restart 13/13,
flag 7/7, clean_home 12/12, all prior targets green, lib 918,
clippy 0 warnings, fmt, diff, conversion 100.00% (166/166).
Next: commit + push, then continue the parity sweep.

### Session slice AC — first-parseable header scan across the open chain

Closed the slice-Z scan-ahead follow-up (genuine RED 4/4): new
`jsonl::first_parseable_line_index` (skip blank/unparseable, stop
at first parseable — oracle `parseSessionHeaderCandidate`) wired
into all five header-locating surfaces: storage load (with
slice-X missing-header diagnostics preserved on scan-miss),
repo list + find_by_id discovery, `metadata_from_session_path`
(scan-miss → intent refusal per `loadEntriesFromFile`), and the
sdk import chain. Agent-storage strictness pins (slice X torn
header, interior/torn-tail) stay green; body-garbage-after-header
stays rejected (agent-storage oracle contract) — SessionManager
body-skip assessed-not-ported (conflicting layers, no process
pin). No row promoted (SES-001/SES-007/CLI-012/CLI-014 notes
extended). Gate green: storage 18/18, repo 17/17,
session_file_invalid 6/6, restart 13/13, all prior targets green,
agent 276 + all suites, sb 13, coding-agent 918, clippy 0
warnings, fmt, diff, docs-lint, conversion 100.00% (166/166).
Next: commit + push, then continue the parity sweep.

### Session slice AB — session-id read-only pin + chronic-warning cleanup

Ported oracle `session-id-readonly` case 1 (evidence-only, green
first): `--session-id X --help` persists zero session files.
Cases 2–3 already covered (empty-stderr reopen; in-process
fork-conflict pin matching the oracle's in-process test). Hygiene
(user-requested amateur-code pass): fixed the five chronic
"pre-existing" warnings — proxy `header_end` sentinel (never
read → terminator-scan loop), pi-tui parity getter allow,
`session::session` path-parity allow, vestigial SettingsMap
import, `ActivePrompt` type alias — plus `&PathBuf`→`&Path`.
Clippy all-targets now ZERO warnings for pi-agent/pi-tui/
pi-coding-agent. README status block corrected (stale 29-failure
claim, 2026-09-17 counts → current 487/409/276/918, catalog
1,357/41). Gate green: session_id_readonly 1/1, session_file_
invalid 4/4, restart 13/13, extensions 10/10, agent 276 +
proxy 13, tui 409, coding-agent 918, pi-ai 487, sb 13 suites,
clippy 0 warnings, fmt, diff, docs-lint, conversion 100.00%
(166/166). Metrics unchanged. Next: commit + push, then
continue the parity sweep.

### Session slice AA — continueRecent fresh-fallback port + CLI-012 note correction

Closed the slice-Z follow-up as a behavior port + honesty fix
(genuine RED 2/2): provenance showed `--continue` fail-closed was
assumed in early routing (`711a25e`), promoted to PASS with an
unverified oracle claim (`25a3b24`) — oracle `continueRecent`
never fails (null → silent fresh session; per-cwd default dir
explains wrong-cwd misses). Ported fresh-fallback at run
(print+JSON)/interactive/rpc; resume-empty keeps fail-closed
(CLI-013). Tests split to oracle-faithful assertions (success +
fresh counts; malformed-skip + restore-recovers); CLI-012 note
corrected in place, row stays PASS. Gate green: restart 13/13,
flag 7/7, clean_home 12/12, print/json/exhaustive/runtime/
extensions/file-safety/import/session_file_invalid green, lib
918/918, clippy 0 errors, fmt, diff, conversion 100.00%
(166/166). Metrics unchanged. Next: commit + push, then continue
the parity sweep.

### Session slice Z — empty/invalid source by intent (open-init vs Cannot-fork)

Closed slice-Y follow-ups with oracle `file-operations`
setSessionFile + `forkFrom` contracts (genuine RED 3/3): intent-
threaded `SessionFileIntent { Open, Fork }` through
`metadata_from_session_path` and `resolve_session_metadata` (all
call sites). Open initializes empty files in place (native v4
header, stable id) and refuses invalid content with the friendly
diagnostic; Fork refuses both with `Cannot fork: source session
file is empty or invalid` and never touches the source. v4
migration-rewrite landmine verified non-applicable. Adjacent
branches + the suspected CLI-012 continue-empty misattribution
recorded as follow-ups. No row promoted (CLI-014/CLI-016 notes
extended). Gate green: session_file_invalid 4/4 (RED 3/3), import
2/2, flag 7/7, clean_home 12/12, restart 12/12, file safety 2/2,
print/json/exhaustive/runtime/extensions/export green, lib 918/918,
clippy 0 errors, fmt, diff, conversion 100.00% (166/166). Next:
commit + push, then continue the parity sweep.

### Session slice Y — invalid-session diagnostic + open-before-model ordering

Ported oracle `session-file-invalid.test.ts` (genuine RED, two
stacked gaps): friendly `Session file is not a valid pi session`
diagnostic, and print-mode session open hoisted ahead of model
resolution (`validate_explicit_session_file`, read-only, before
provider resolution — mirrors oracle createSessionManager order;
JSON mode already correct). `Error: ` prefix only for this family
at main's top-level print (openSessionOrExit mirror). Adjacent
oracle branches (empty-file init, malformed-line scan, missing-
path create, continue/resume order) recorded as follow-ups. Drive-
by: RES-006 test clippy allow (pre-existing all-targets error).
No row promoted (CLI-014 already PASS). Gate green:
session_file_invalid 1/1, import 2/2, export 1/1, flag 7/7, file
safety 2/2, clean_home 12/12, restart 12/12, print/json/exhaustive/
runtime green, lib 918/918, clippy errors 0, fmt, diff, conversion
100.00% (166/166). Next: commit + push, then continue the parity
sweep.

### Session slice X — torn-tail discriminator fix + 5 remaining pins

Closes slice V's 5-case follow-up with a genuine bug fix (TDD RED
2/5): `load` keyed torn-tail repair on error kind instead of the
oracle's terminator property. Now termination-preserving via the
pre-existing `split_text_lines` port — final line repairs iff
unterminated, terminated lines always reject, torn first line
refuses as missing header. Five pins complete the 7-case oracle
suite. No row promoted (session slice). Gate green:
jsonl_storage 15/15, pi-agent lib 276/276 + 17/17 suites,
session-backends green, fmt, clippy pre-existing only,
conversion 100.00% (166/166). Next: commit + push, then continue
the parity sweep.

### Session slice W — import refusal pins + fork assessment

Two SES-012 pins (missing-file, invalid-header), genuine RED
resolved against oracle copy-then-open semantics. SES-010
assessed-not-portable (no branch-name surface). Jev-routed
(weak confs, each oracle-grounded), stop done 0.64, guardrail
safe 0.54 (residual risk verified as noise). No row promoted
(session slice). Gate green: new target 2/2, lib 918/918, fmt,
diff, conversion 100.00% (166/166). Next: commit + push, then
continue the parity sweep.

### Session slice V — torn-tail repair continuity pins

Two oracle torn-tail cases pinned (repair-then-append seq
continuity, interior-line rejection), green first run,
evidence-only. Theme_state lane investigated first: no
reproducible defect, pivoted per Jev re-route (0.96). Jev
pushback (not_done 0.71) honored via 7-case coverage map;
remainder is follow-up. No row promoted (session slice). Gate
green: jsonl_storage 10/10, pi-agent 429, sb 89, fmt, diff,
conversion 100.00% (166/166). Next: commit + push, then the
remaining torn-tail variants.

### Flake-hunt slice U — parallel-load test stabilization

Fixed three load-dependent flake sources with no behavior
change: test-only env lock for `session_env` (0/6 → 8/8),
synchronous watcher baseline capture, `tokio::sync::Mutex`
registry lock + 3 bisected interferers guarded. Jev-routed
(flake_hunt 0.59, moderate conf compensated with bisection
evidence), stop done 0.91, guardrail safe 0.70. No row promoted
(test-hygiene slice). Gate green: lib 918/918 x3, pi-ai 487,
agent 276, sb 89, fmt, diff, conversion 100.00% (166/166).
Next: commit + push, then continue the parity sweep.

### Extension slice T — ignore-pin host hermeticity

Fixed the `ignore_file` failure: host `~/.agents/skills` content
(`sops-age-secrets`) leaked into a global assertion; discovery
itself was already correct (probe-proven). Scoped assertions to
the fixture dir — test-only change. Jev stop done 0.91,
guardrail safe 0.89. No row promoted (test-hygiene slice). Gate
green: target green, lib 916 passed (theme flakes pre-existing
in isolation-green; pi-tui clippy pre-existing), fmt, diff,
conversion 100.00% (166/166). Next: commit + push, then continue
the parity sweep.

### Tool slice S — find/grep binaries + rpc golden refresh

Environment + stale fixture, no source change: installed
`fd-find`/`ripgrep` (`fd` symlink; upstream `ensureTool`
contract), refreshed the rpc golden's 3 catalog-growth deltas
(field-by-field verified, 51/51 cases otherwise identical).
Jev-routed (tool_flakes 0.55, weak conf compensated with extra
grounding), stop done 0.90, guardrail safe 0.88. No row promoted
(test-hygiene slice). Gate green: find 15/15, grep 19/19, rpc
golden green, lib 914 passed (ignore-file bug → next slice;
timing flakes pre-existing), fmt, diff, conversion 100.00%
(166/166). Next: commit + push, then the ignore-file skill
exclusion bug.

### Session slice R — `provider_thinking_level` test-literal repair

Fixed the pre-existing E0063 workspace breakage: 6 test literals
gain `provider_thinking_level: None` (semantically correct —
unmanaged fixtures). No production change. Jev stop done 0.92,
guardrail safe 0.89. No row promoted (test-hygiene slice). Gate
green: session-backends 89/89, context 4/4, workspace compiles
(2009 passed, 0 introduced), pi-agent clippy errors pre-existing,
fmt, diff, conversion 100.00% (166/166). Next: commit + push,
then continue the parity sweep.

### Provider slice Q — zai base-url `/coding/` segment

Fixed the pre-existing `zai_registrations` mismatch: constructor
base was missing `/coding/` (oracle + catalog + lane default all
agree on `api/coding/paas/v4`). Two-line fix. Jev-routed (fix_zai
0.91), stop done 0.95, guardrail safe 0.92. No row promoted
(provider slice). Gate green: zai 4/4, pi-ai 681 passed / 0
failed, strict clippy, fmt, diff, conversion 100.00% (166/166).
Next: commit + push, then continue the parity sweep.

### Provider slice P — meta-ai provider (intentional divergence)

New `meta-ai` provider (no upstream oracle; spec vendored in
`docs/meta-ai-provider-spec.md`): 5-model catalog, constructor
(`MODEL_API_KEY` + `META_API_KEY`), effort map (off→minimal,
max std-1.3-only), matrix lane, count pins (41 providers, 1356
models, 51 pairs). TDD red-first. Jev guardrail safe 0.71 (zero
introduced failures proven), stop-hook not_done honored via docs
gate. No parity row added (318-universe stays pure-upstream;
divergence recorded in ledger). Gate
green: meta_ai 4/4, lib 487/487, matrix 7/7, exhaustive 18/18,
pi-ai 680 passed (1 pre-existing zai), agent 276/276, tui 463/463,
clippy, fmt, conversion 100.00% (166/166). Next: commit + push,
then the zai provider/base_url mismatch.

### Provider slice O3 — qwen token-plan dimension pin refresh

Fixed the stale dimension pin: oracle #9021 added `qwen3.8-flash`
(in-pin ancestor); catalog already correct at 18/18/9, exact-ID pin
green — only three count literals were stale (17/17/8 → 18/18/9).
Jev-routed (fix_qwen_dims 0.95), guardrail safe 0.90, stop-hook
not_done honored (zai mismatch scoped to next unit, docs gate
completed). No row promoted (provider slice). Gate green: xiaomi
2/2, pi-ai 1 remaining pre-existing failure (zai, identical on
clean HEAD), strict clippy, fmt, diff, conversion 100.00%
(166/166). Next: commit + push, then the zai provider/base_url
mismatch.

### Provider slice O2 — openrouter anthropic-messages matrix variant

Fixed the pre-existing `fixture_index` failure: oracle openrouter
is dual-lane since 0.85.1, index was stale single-lane. Added the
`openrouter/anthropic-messages` text variant (shared fixture,
`by-api`), corrected the completions lane. Fixture-JSON-only; new
variant runs through the existing matrix runner. Jev-routed
(fix_fixture_index 0.93), stop done 0.94, guardrail safe 0.92. No
row promoted (provider slice). Gate green: provider_matrix 7/7,
pi-ai 1 remaining pre-existing failure (qwen dims, identical on
clean HEAD), strict clippy, diff, conversion 100.00% (166/166).
Next: commit + push, then continue the parity sweep.

### Transcript slice O — copilot openai-to-anthropic migration pins

Ported the 4-case oracle migration file
(`transform-messages-copilot-openai-to-anthropic.test.ts`):
evidence-only — thinking→text conversion, thoughtSignature strip,
`|` ID normalization, missing-only synthetic results. Four pins
green on first run. Slice routed by live Jev Choice
(provider 0.67 → transcript_wave_b 0.81 after PROV residuals proved
live-traffic-bound). No row promoted (transcript slice; live traffic
offline-unverifiable). Gate green: migration 4/4, siblings green,
pi-ai 649 passed / 1 pre-existing failure (identical on clean HEAD),
strict clippy, fmt, conversion 100.00% (166/166). Next: commit +
push, then continue the parity sweep.

### Drift triage — remaining 5 commits assessed (no slices)

Read all five oracle diffs at 46c9de40: eval-harness validation
(queued behind pi-evals contract), TUI footer eval (out of scope +
no Rust option counterpart), changelog wording (docs-only),
contributor approval (metadata), thinking-drop notice (no Rust
renderer string; blocked on interactive diagnostics projection).
New-drift score: 8/13 ported (slices G–N), 5/13 assessed. No row
promoted. Next: commit + push, then resume the parity sweep
(transcript wave b, or next residual).

### Provider slice N — renamed-proxy thinking replay

Upstream #9188 (1283afd0d): evidence-only — the lane already keeps
the requested id + `responseModel` with fallback pricing, so signed
thinking survives replay. Two pins green on first run. No row
promoted (provider slice; live proxy traffic offline-unverifiable).
Gate green: anthropic_provider_parity 14/14, pi-ai strict clippy,
fmt, conversion 100.00% (166/166), parity dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then continue the
new-drift triage.

### Provider slice M — Gemini thinking-level gate

Ported upstream 16235fd93: one `uses_google_thinking_level` gate
(versioned Pro/Flash, latest aliases, both Gemma 4 spellings);
verbatim level mapping; MINIMAL disabled fallback; both lanes share
the gate. New gate pin + updated disabled pin, TDD red-first. No
row promoted (provider slice; live traffic offline-unverifiable).
Gate green: google 49/49, vertex 22/22, pi-ai lib 487/487, pi-ai
strict clippy, fmt, conversion 100.00% (166/166), parity dashboard
OK (upstream=d7296c0, 58/318). Next: commit + push, then continue
the new-drift triage.

### Transcript slice L — SystemMessage.replace removed

Ported upstream 16292398a: `replace` leaves the replay surface
(replay never resets; mid-convo resolution keeps later messages).
Old oracle test kept as ignored archaeology; new accumulation pin,
TDD red-first. No row promoted (transcript slice; adoption queued).
Gate green: transcript 7/7, pi-ai lib 486/486, pi-ai strict clippy,
fmt, conversion 100.00% (166/166), parity dashboard OK
(upstream=d7296c0, 58/318). Next: commit + push, then continue the
new-drift triage.

### Extension slice K — handler unsubscribe

Ported upstream #9630 (46c9de40): `on()` returns an unsubscribe
handle (index-addressed slot removal, empty-key drop, safe rerun);
emit-snapshot half assessed-not-ported (no re-entrant mutation path
in the Rust runner). New pin, TDD red-first. No row promoted
(extension slice). Gate green: extensions_parity 10/10, extensions
lib 74/74, scoped rustfmt, conversion 100.00% (166/166), parity
dashboard OK (upstream=d7296c0, 58/318). Pre-existing clippy
verified identical on clean HEAD. Next: commit + push, then continue
the new-drift triage.

### Provider slice J — Vercel unsigned thinking

Ported upstream #9676 (3955b27a1): `allowEmptySignature: true` on
all 237 Vercel AI Gateway entries (lane already honored it). New
catalog pin, TDD red-first. No row promoted (data-correction slice;
live traffic offline-unverifiable). Gate green: catalog 15/15, pi-ai
lib 486/486, pi-ai strict clippy, fmt, conversion 100.00% (166/166),
parity dashboard OK (upstream=d7296c0, 58/318). Next: commit + push,
then continue the new-drift triage.

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
