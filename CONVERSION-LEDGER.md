# Full Pi → pi-rust Conversion Ledger

Session date: 2026-08-23 (operator: "going to bed — document or something")
Author: pi (Claude), planning pass grounded in a live repo audit.
Base revision: HEAD 90a5b93 (1416 tests at last clean revision).

## Current status (last updated 2026-08-24)

- The exhaustive checker reports **55.42% (92/166)**. Run
  `node scripts/conversion-progress.mjs` after any ledger change; the same
  value is copied into `PLAN.md`.
- The workspace currently checks and tests successfully offline, including the
  focused tool-contract, RPC, and one-shot compaction/image parity tests. The
  full workspace test result is re-run at verification gates rather than
  inferred from a historical session count.
- The original 100 entries remain the historical work queue. The supplemental
  S1 section is authoritative for residual provider, harness, runtime, TUI,
  RPC, auxiliary client/server, evaluation, and final-audit work.

## Current state (verified 2026-08-24)

- HEAD is the local JSON-mode harness checkpoint on `main`, after
  the print-path harness ownership, AgentTool harness/termination,
  schema-validator, panic-safe telemetry, update/version, and model-catalog
  work; the HTTPS remote is still behind because GitHub credentials are
  unavailable.
- The workspace is green under `cargo test --workspace --offline`; the focused
  tool-contract, RPC, image/read, print-mode compaction, malformed-call, and
  harness-owned print-path suites pass. The one-shot path now owns a
  stateful `AgentHarness` transcript and replays it into durable JSONL while
  retaining compaction behavior. Its configured print run now emits ordered
  lifecycle events and a settled `pi.harness.run` span with required
  attributes. JSON mode now also owns a stateful in-memory `AgentHarness`
  transcript and replays rich stream updates, including terminal provider
  errors, without changing the established successful RPC wire envelope. A
  shared lifecycle adapter wraps the JSON and RPC loop paths, while interactive
  turns now run through a configured harness as well; complete mode-specific
  golden envelopes and persistence/secondary-lane assertions remain open
  under S-021/S-022. Telemetry callback panics now
  settle in-memory spans as automatic errors while preserving explicit
  statuses and panic propagation; the shared TUI image-capability fixtures
  are serialized for deterministic workspace runs.
- Documented remaining gaps (PLAN.md carry-forward + per-crate TODOs): OAuth
  device-code flows, codex WebSocket transport (SSE fallback today),
  `/share` GitHub-gist OAuth (in-progress in the working tree), ConfigSelector
  full TUI component, models.json runtime merge seam, full interactive
  slash-command PTY coverage, TUI alt-screen full swap + terminal feature
  probes, server/client
  concurrency surfaces (leases, reconnect, queuing). Signed usage adjustment
  parity is closed in Session 16.
- The alt-screen hardening now invalidates differential frames after
  alternate-screen transitions; full regular/fullscreen swapping and the
  dedicated tmux probe remain open in #61/#62.
- **Additional gaps found in this audit** (not in any TODO file):
  - Missing CLI flags vs 0.84.2 surface: `--fork`, `--approve/-a`,
    `--no-approve/-na`, `--no-builtin-tools/-nbt`, `--extension/-e`,
    `--no-extensions/-ne`, `--skill`, `--no-skills/-ns`, `--prompt-template`,
    `--no-prompt-templates/-np`, `--theme`, `--use-theme`, `--no-themes`,
    `--no-context-files/-nc` (+ print-mode `--steer/--follow-up/--compact`).
  - No auto-compaction wiring in `run.rs` (RPC `compact` command exists).
  - `image` tool not registered: run.rs exposes 7 tools, upstream has 8
    (bash/read/write/edit/edit-diff/ls/find/grep/image).
  - coding-agent core modules not ported as modules (functionality may exist
    elsewhere — audit first): `bash-executor`, `exec`, `system-prompt` wiring,
    `skills` loader, `prompt-templates` + `resource-loader`, `http-dispatcher`,
    `session-cwd`, `cache-stats`, `timings`, `auth-guidance`,
    `settings-diagnostics`, `diagnostics`, `project-trust`/
    `trust-manager`, `messages` (extended), `footer-data-provider`.
- pi-agent: the core AgentTool shape, prepareArguments seam, optional-details
      payloads, mutable before/after hooks, parallel completion ordering,
      built-in update sink, mutation queue, and malformed-call matrix are now
      covered; broader schema-validator parity remains S-024.
  - pi-client: reconnect state/listeners, handshake/request timeout bounds,
    late-response suppression, and permanent dispose now land with fake-socket
    coverage; lease/reconcile parity remains partial, as do transport-factory
    and full conformance gaps.
  - pi-server: no LiveSessionManager exclusivity, session lock/terminal-close
    semantics, command queuing, subscription segment control,
    testing/service.ts parity harness.

## Standing process rules (binding)

- Commit + push after every commit (session-4 directive; applies to every git
  repo worked on from this agent).
- Evidence tiers on every criterion: `unit` | `mock` | `live` — a claim needs
  the tier + exact command that produced it.
- Line-by-line expert assessment per phase; stale plan = process failure
  (PLAN.md §0).
- Independent reviewer sign-off gate before the next MAJOR phase (PLAN.md §0.3).
- Parity oracles: golden tests generated from upstream 5cd93f6, never memory.
- Keep 0 lib warnings + clippy-clean for new files per established bar.

## Tracks

- T0  Land the in-flight working tree (it is red — fix first)
- T1  Close the 9 documented divergences (audit → port → gate)
- T2  Tool contract + validation
- T3  coding-agent run-path parity (unported core surfaces)
- T4  Server/client completion (P6 concurrency)
- T5  TUI completion
- T6  Remaining coding-agent core modules (audit → port what's absent)
- T7  Data model, session tree, export, RPC edge parity
- T8  Evals, packaging, parity suite
- T9  Final 100% verification pass

## Recut (2026-08-23) — priority + goal framing (by operator request)

Goal: **behavioral 1:1 parity of the `pi` CLI product** (what a user of `pi`
experiences), not crate-for-crate structural copying.

Decisive evidence from a live audit:
- The real binary (`pi-coding-agent`, `bin = "pi"`) does **not** depend on
  `pi-server`/`pi-client`; `run.rs:231` runs `pi_agent::agent::run_agent_loop`
  **in-process**. This is a deliberate, behaviorally-faithful divergence.
- Upstream only uses `@earendil-works/pi-client` from `extensions/llama/*` and
  `client/remote-session.ts` — the Llama extension + remote-session driver,
  **not** the core CLI loop. So pi-rust's in-process loop matches the real CLI.
- Therefore `pi-server`/`pi-client` is an **auxiliary subsystem** (no shipped
  binary links it; only pi-server's own dev-deps test it). `T4` hardens it.

Recut of the remaining work by user impact + risk:
1. **T6 gap-audit first** (highest value): convert to *verify-then-port* — many
   listed modules already exist in `pi-agent`/`pi-coding-agent` (skills, system-
   prompt, prompt-templates, project-trust, settings, rpc). Real deliverable is
   **wiring** them into the run path, not re-porting the loaders.
2. **T5 TUI polish** second: config selector, alt-screen full swap, word-nav,
   feature probes, footer totals, editor IME, slash-command E2E.
3. **T7 product surfaces** third: `pi update`, session tree, export_html.
   RPC-edge audits after the visible surfaces.
4. **T4 #52-58 reclassified: OPTIONAL/DEFERRED auxiliary** — a separate
   "server/client as a library" milestone, clearly off the CLI path. Only
   restart if the goal becomes structural parity (i.e. wire `pi-coding-agent` →
   `pi-client`), which is a distinct prerequisite the plan never listed.
5. **T8/T9 stay last**: ledger, parity-suite, release-build, final 100% audit
   (many of #100's criteria are already true today).

---

## T0 — Land the in-flight working tree (fix first — it's red)

- [x] 1. Fix `crates/pi-coding-agent/tests/cli_export.rs:13-15` — repair the
      `split("data-session="")` string to `"data-session=\""`, making the
      `'"'` char literal parse. (unit)
- [x] 2. Verify `args.rs` `--export` help text compiles/prints one flag per
      line (the diff shows a wrapped literal — make rustfmt clean). (unit)
- [x] 3. Audit `main.rs` `--export` wiring vs upstream `exportFromFile`
      (output path fallback, exit codes, "Exported to:" print). (mock)
- [x] 4. Finish `/share` — remove `SHARE INVOKED (probe)` banner and `dry_run`
      default; implement upstream share.ts (gh auth status → export HTML →
      `gh gist create --public=false` → viewer `PI_SHARE_VIEWER_URL#<id>`).
- [x] 5. Wire `PI_SHARE_DRY_RUN=1` as test switch only; mock-gh integration
      test. (mock)
- [x] 6. Verify persist-before-share ordering (transcript matches exported
      HTML; no lost/duplicated messages). (mock)
- [x] 7. Land editor autocomplete fix (close popup after apply so Enter
      submits) with the two new `editor_tests` cases; full pi-tui suite green.
- [x] 8. Remove duplicated `/// Wrap a modal...` doc comment in interactive.rs.
- [x] 9. `cargo test --workspace` + clippy on touched crates; restore all-green
      0-warning baseline. (unit)
- [x] 10. Commit + push: `feat(interactive): /share gist flow + --export CLI
      wiring`.

## T1 — Close the 9 documented divergences (audit → port → gate)

- [x] 11. Divergence-close audit: for each remaining gap, pin upstream file +
      current Rust behavior + test plan. (unit)
- [x] 12. Port upstream `ai/src/oauth.ts` + `auth/oauth/device-code.ts` state
      machine (device auth endpoint, poll interval, expiry, cancellation).
- [x] 13. GitHub device-code flow.
- [x] 14. (non-gap: upstream v0.84.2 Google provider is API-key only; no OAuth flow exists to port) Google OAuth2 device + refresh flow.
- [x] 15. Anthropic OAuth flow.
- [x] 16. Wire OAuth into `auth_storage.rs`, `commands/auth.rs`, `/login`.
- [x] 17. OAuth tests (mock device endpoints, expiry, cancel). (mock)
- [x] 18. Codex WS audit: upstream websocket transport paths vs current
      SSE-only `openai_codex_responses.rs`.
- [x] 19. Port WebSocket client transport (`pi-ai/src/transports/ws.rs`).
- [x] 20. Route codex to WS when settings `transport: websocket`; SSE fallback.
- [x] 21. WS tests (fixture-driven + local ws echo server). (unit/mock)
- [x] 22. Reviewer gate: multi-session diff review of T0+T1 (and T3
      completion) — independent reviewer session signed off. Verdict:
      **APPROVE WITH CONDITIONS**, all three conditions (C1 json-mode terminal
      error exit-0 parity, C2 `--tui-mode` diagnostics + token consumption,
      C3 main.rs diagnostic-print-then-exit ordering) resolved in commit
      b42050c. Non-blocking notes: N1 attribution not yet wired end-to-end
      (real providers land in T6); N3 json events buffered (matches codebase
      RPC pattern); N4 json_event.rs duplicates run.rs setup (consolidation
      follow-up). **N2 (`-v` verbose-vs-version) resolved** — `-v` now maps to
      version per upstream args.ts (was verbose); `--verbose` remains the long
      form.

## T2 — Tool contract + validation

- [x] 23. Audit upstream AgentTool shape vs current `pi-agent/src/tools/`
      trait; per-tool deltas. (unit)
- [x] 24. Upgrade tool trait to upstream shape.
- [x] 25. `prepareArguments` for bash/read/write/edit/edit-diff/ls/find/grep/
      image. (unit) Audited the pinned upstream constructors: `edit` is the
      only built-in with a non-identity prepare shim; Rust now registers the
      upstream normalization for JSON-string, single-object, and legacy
      top-level edit arguments. The remaining built-ins intentionally have no
      prepare shim in the oracle.
- [x] 26. `execute` upstream signature + `onUpdate` → rich loop emits
      `tool_execution_update`. (unit) The callback is now passed through a
      channel-backed sequential/parallel execution sink; bash emits throttled
      partial output plus a final snapshot. Evidence:
      `cargo test -p pi-agent --offline
      rich_loop_executes_tool_batch_and_emits_execution_events` and
      `cargo test -p pi-agent --offline --test tools
      bash_tool_streams_partial_updates_through_agent_contract`.
- [x] 27. Terminate-hint plumbing in `rich_agent.rs`. (unit) Batch termination
      now requires every finalized parallel result to opt in, with evidence in
      `terminate_hints_require_every_parallel_tool_to_opt_in`.
- [x] 28. Migrate every tool constructor + run.rs call sites.
- [x] 29. Port `validateToolArguments` (tool-args JSON-schema validation).
- [x] 30. Wire validation into `prepare_tool_call` with upstream errors.
- [x] 31. Tool-args validation tests (schema errors, unknown keys,
      partial-JSON args).
- [x] 32. Image/read processing parity and model-facing image behavior in
      `run.rs`. (unit) The pinned upstream source has no separate
      `AgentTool` named `image`: `harness/tools/image.ts` is the shared MIME
      detector/base64 helper used by `read`. Rust now ports the detector,
      BMP→PNG normalization, 2000x2000/4.5MB resize policy, JPEG fallback,
      conversion/dimension hints, `@file` image attachments, and the
      provider-boundary `blockImages` filter. Verified with
      `cargo test -p pi-agent --offline tools::image` (6 passed),
      `cargo test -p pi-coding-agent --offline
      run::tests::file_arguments_attach_images_and_tag_text_references`, and
      `cargo check --workspace --offline`. The existing `/images` setting
      remains a terminal-display toggle; `images.blockImages` controls model
      delivery as upstream.

## T3 — coding-agent run-path parity

- [x] 33. Wire auto-compaction into run path (settings threshold → compact →
      continue; upstream `core/compaction/` loop). (mock) The one-shot path
      now provisions its session entries while running, evaluates the model
      context against `compaction.enabled`, `reserveTokens`, and
      `keepRecentTokens`, invokes the existing harness compactor, rebuilds the
      provider context from the compaction entry plus retained tail, and keeps
      processing later print turns. Verified with
      `cargo test -p pi-coding-agent --offline run::tests` and the binary
      compaction test below.
- [x] 34. Binary-level auto-compaction test (JSONL gains compaction entry).
      (mock) `cargo test -p pi-coding-agent --offline --test
      cli_print_parity` passes the forced-settings test
      `print_mode_auto_compaction_persists_and_continues`, including the
      continued second response and persisted `"type":"compaction"` entry.
- [x] 35. Port `core/messages.ts` extended-message wiring
      (BashExecutionMessage/CustomMessage reach provider in run.rs).
- [x] 36. v3→v4 legacy session import in coding-agent session runtime
      (upstream `session-manager.ts`); `/import` conversion parity.
- [x] 37. v3→v4 migration tests using fixture v3 JSONL files.
- [x] 38. models.json runtime merge: file-backed `models_store.rs` load merged
      over bundled catalog; `applyModelsJson` wiring in run path.
- [x] 39. models.json merge tests (override, compat, bad-json error).
- [x] 40. Port `core/project-trust.ts` + `trust-manager.ts` into CLI path
      (`-a/-na`, defaultProjectTrust, `.pi/trust`).
- [x] 41. Project-trust binary tests (untrusted dir tool gating). (mock)
- [x] 42. Print-mode parity audit (`modes/print-mode.ts`): audit found the
      `--steer`/`--follow-up`/`--compact` items are **RPC commands**, not
      print-mode flags (no such flags exist in upstream `args.ts` /
      `print-mode.ts`); the real print-mode contract is sequential multi-turn
      prompting, `initialMessage` handling, quietStartup, and terminal
      error/abort exit semantics.
- [x] 43. Port print-mode output-formatting parity: `run.rs` now prompts each
      positional message as its own sequential turn (upstream
      `for message of messages { session.prompt(message) }`), folds prior
      turns into the agent context, surfaces a terminal Error/Aborted
      stop-reason on stderr with `errorMessage || "Request {stopReason}"` and
      exits nonzero, and joins text content blocks with `\n` (upstream
      ``writeRawStdout(`${content.text}\n`)``). Faux path queues one response
      per prompt. `tests/cli_print_parity.rs` (2 binary tests).
      `initialMessage` equals the leading positional prompt in the current
      single-shot path; quietStartup is TUI/startup-output scoped.
- [x] 44. Port JSON-event mode `modes/json-event.ts` → `modes/json_event.rs`
      (`--mode json`), event-envelope parity.
- [x] 45. JSON-event tests (fixture transcripts).
- [x] 46. Remaining CLI flags: `-nbt`, `-e/--extension`, `-ne/--no-extensions`,
      `--skill`, `-ns`, `--prompt-template`, `-np`, `--theme`, `--use-theme`,
      `--no-themes`, `-nc`, `-a/--approve`, `-na`, `--fork` — **plus** the
      `--append-system-prompt`, `--models`, `--tui-mode` surface and
      upstream `Args.diagnostics` (error→exit 1, warning→continue). Parsing
      landed; run-path honoring of `--fork` (fork-session-and-continue) sits
      with the session-tree parity (#83/#88) and the
      skill/prompt-template/extension/theme loaders with T6 (#73/#74).
- [x] 47. Flag-matrix golden test: `tests/cli_flag_matrix.rs` fires the full
      upstream `args.ts` flag surface against the built binary — every flag
      parses without an "unknown flags" diagnostic, `--help` lists the full
      surface, error-valued diagnostics (missing `--use-theme` value) exit
      nonzero with an `Error:` line, and invalid `--thinking` warns-but-runs.
      5 binary tests.
- [x] 48. Telemetry wiring: ported `core/telemetry.ts` → `core/telemetry.rs`
      (`isInstallTelemetryEnabled` honoring the `PI_TELEMETRY` env override:
      `1`/`true`/`yes` enable, `0`/`false`/`no` disable, unset defers to the
      `enableInstallTelemetry` setting) and wired it into provider attribution
      (the observable §2.2 env surface). 4 unit tests + 1 attribution
      env-override test. Follow-ups tracked: interactive-mode install-report
      ping to `pi.dev/api/report-install` (network-bound), and any
      tracing-subscriber span instrumentation (orthogonal to upstream's
      actual `PI_TELEMETRY` semantics).

## T4 — Server/client completion (P6 concurrency) — OPTIONAL / DEFERRED auxiliary

> Reclassified 2026-08-23: `pi-server`/`pi-client` is an auxiliary subsystem the
> `pi` binary does not link (run loop is in-process). These harden that path but
> advance no user-facing CLI parity. Defer behind T6/T5/T7 unless the goal
> becomes structural parity (see "Recut" preamble). Items kept as-is for tracking.

- [x] 49. `LiveSessionManager`: acquire/release exclusivity + attach/detach
      validation on server.
- [x] 50. Session lock + terminal-close semantics + command queuing.
- [x] 51. Subscription segment control for prompt/steer concurrency.
- [ ] 52. Port `testing/service.ts` parity harness + conformance suite.
- [ ] 53. Server conformance tests (30+ cases). (unit)
- [x] 54. Client reconnect state machine + connection-state listeners. (mock)
      `PiClient` now exposes `Disconnected`/`Connecting`/`Connected`, reconnects
      through a fresh Unix handshake with connection epochs, invalidates
      attached session handles on disconnect, and returns lifecycle callbacks.
      `cargo test -p pi-client --offline` covers snapshot refresh and the full
      lifecycle sequence over a fake Unix socket.
- [ ] 55. Client lease/exclusive-attach parity (reconcile, detach-on-close).
- [x] 56. Client dispose semantics + promise timeouts. (mock)
      Requests have configurable handshake/request bounds; timed-out request
      ids are tombstoned so late responses do not tear down a healthy client;
      `dispose()` permanently releases state/listeners while `close()` remains
      reconnectable. Covered by `cargo test -p pi-client --offline`.
- [ ] 57. Transport factory abstraction beyond unix (async-trait).
- [ ] 58. Client↔server E2E under reconnect + lease churn. (unit/mock)

## T5 — TUI completion

- [x] 59. ConfigSelector full TUI component (config command interactive).
      (mock) `interactive/config_selector.rs` now covers searchable resource
      rows, circular/page navigation, scope switching, global toggles,
      project inherit/load/unload cycling, inherited-resource rendering, and
      synchronous settings flushes before exit. The component is wired through
      `commands/config.rs`; `cargo test -p pi-coding-agent --offline
      interactive::config_selector` passes 8 tests. The underlying data model
      and `PackageManager::resolve()` producer remain covered by the prior
      5+8+integration tests. PTY coverage is recorded in S-035; the full
      interactive matrix remains S-056.
- [x] 60. ConfigSelector snapshot tests. (mock) Five buildGroups tests, eight
      resolve() producer tests, `resolve_feeds_build_groups` integration, two
      interactive behavior tests, and a deterministic global/project render
      snapshot test now cover the selector surface. Verified with `cargo test
      -p pi-coding-agent --offline interactive::config_selector`; the real
      terminal exercise is recorded in S-035, with the full matrix in S-056.
- [ ] 61. Full alt-screen screen-swap parity (save/restore around overlays).
- [ ] 62. Alt-screen swap tmux probe.
- [x] 63. ICU word segmentation (replace regex word-nav with unicode parity).
      `word_navigation.rs` now segments each CJK ideograph as its own
      word-like segment, matching upstream `Intl.Segmenter(undefined,
      {granularity:"word"})` per-character ideographic stepping (the previous
      port jumped the whole CJK run, a real backward-nav divergence from the
      upstream oracle). Editor Ctrl+arrow word nav (editor.rs
      find_word_backward/forward) picks this up automatically. Locale-aware
      dictionary grouping (zh/ja word sense) remains non-reproducible and is
      documented as such.
- [x] 64. Word-segmentation tests (upstream cases). 18 word_navigation tests,
      incl. the upstream CJK oracle translated to byte offsets: backward steps
      17→13→9→6→3→0 through "你好世界 test"; forward steps per-char
      0→3→6→9→12→17. Full pi-tui suite green.
- [ ] 65. tmux `client_termfeatures` probe (feature detection parity).
- [ ] 66. Terminal feature probe tests.
- [x] 67. Token-total footer reads (usage totals → footer parity). Ported
      upstream `formatTokens` (exact thresholds) into `interactive/footer.rs`,
      added `render_usage_stats` (`↑input ↓output Rcache Wcache CH{rate}%
      $cost`) and an optional `usage`/`cache_hit_rate` on `FooterData`, and
      wired `interactive.rs` to aggregate cumulative usage + latest-turn
      cache-hit-rate from transcript assistant messages (upstream
      `FooterComponent.render`).
- [x] 68. Footer tests. 5 footer.rs tests (formatTokens thresholds, usage-stats
      nonzero/cache/CH/cost rendering, usage-left+model-right layout) + 2
      interactive.rs tests (usage aggregation across turns + hit-rate math,
      empty-when-no-usage). Verified full `cargo test --workspace` green at
      1390. Interactive TUI E2E render not reachable headlessly (first-run
      terminal-feature glyph probes need a real terminal's font judgment).
- [ ] 69. Editor IME/selection edge parity (kitty flags, bracketed paste).
- [ ] 70. Interactive-mode E2E tmux script: full slash-command matrix.

## T6 — Remaining coding-agent core modules (audit → port what's absent)

- [x] 71. Audit `core/bash-executor.ts` + `exec.ts` vs agent bash tool; port
      shell-capture/output-truncation parity where missing. AUDIT: already
      covered — `pi_agent/src/tools/bash.rs` implements `BashCapture` +
      `run_bash` with upstream capture semantics (concurrent stdout/stderr
      drain against the timeout deadline, `truncate_tail`, `[Showing lines
      X-Y of N... Full output truncated]` messages) and is wired into run.rs
      via `bash_tool`. Session 25 subsequently closed the live `onUpdate`
      callback path and basic throttled bash progress; S-018 still tracks full
      harness truncation/detail fixture parity.
- [x] 72. Port `core/system-prompt.ts` wiring into run context — run.rs now
      assembles the system prompt from `--system-prompt` base + the skills
      `<available_skills>` block + `--append-system-prompt` inputs (files read
      verbatim, inline text used as-is).
- [x] 73. Port `core/skills.ts` loader into run path (`--skill`, `-ns`,
      `.pi/skills`, `<agentDir>/skills`, settings `skills` key). New
      `core/skills.rs`: recursive dir loader honoring `.gitignore`/`.ignore`/
      `.fdignore`, name/description validation, name-collision dedup with
      winner/loser diagnostics, `formatSkillsForPrompt` `<available_skills>`
      block (the `-ns`/`-s` flips gate loading).
- [x] 74. Port `core/prompt-templates.ts` + `resource-loader.ts` into run path
      (`--prompt-template`, `-np`, `.pi/prompts`, `<agentDir>/prompts`). New
      `core/prompt_templates.rs`: `loadPromptTemplates`, `parseCommandArgs`,
      `substituteArgs` (`$1`/`$@`/`${N:-d}`/`${@:N[:L]}`), and
      `expandPromptTemplate` — run.rs expands `/template` positional messages.
      Plus new `core/context_files.rs`: port of `loadProjectContextFiles` +
      `findShadowedContextFile` + `footer-data-provider.ts findGitPaths`
      (AGENTS.md/CLAUDE.md + AGENTS.override.md ancestor walk with BOM strip,
      worktree-shadow dedup) — run.rs injects the `<project_context>` section
      and `-nc/--no-context-files` disables it (previously a dead flag).
- [ ] 75. Port `core/http-dispatcher.ts` / proxy behavior if not covered —
      AUDIT: real providers route through the pi-ai `Models` facade
      (`models.stream` in run.rs), which owns HTTP/SSE dispatch + auth
      internally. A dedicated coding-agent http-dispatcher seam is not present;
      revisit only if a proxy/network-option divergence surfaces in the facade.
- [x] 76. Port `core/session-cwd.ts` semantics (interactive + RPC). New
      `core/session_cwd.rs`: `getMissingSessionCwdIssue`,
      `formatMissingSessionCwdError`, `formatMissingSessionCwdPrompt`,
      `MissingSessionCwdError`, `assertSessionCwdExists`. Wired into
      `modes/interactive.rs` resume: a session whose stored cwd no longer
      exists is refused with the upstream error banner instead of resumed.
- [x] 77. Port `core/cache-stats.ts` (cache-waste accounting) into the usage
      surface. New `core/cache_stats.rs`: `compute_cache_waste` /
      `collect_cache_misses` / `detect_cache_miss` over `&[Value]` session
      entries — prompt-cache miss detection (previous-turn prompt tokens
      re-billed as input/cacheWrite vs cacheRead), `NOISE_FLOOR_TOKENS`,
      compaction/branch_summary prev-reset, model-change + idle-TTL flags,
      `ModelPriceSource` (per-million cacheRead price) with `NoPrices`/fn-ptr
      sources. 7 fixture tests (first-turn, counted miss + cost math, noise
      floor, compaction reset, model change, price-source fallback, pending-
      message detect). The interactive consumers (cache-miss transcript
      notices + the "Cache Re-billed" stats line, gated by the already-wired
      `showCacheMissNotices` setting) are PTY/render-bound and land with the
      remaining interactive surface. `timings.ts` (a `PI_TIMING=1`-gated stderr
      startup profiler) is a deliberate non-port — the Rust binary has no
      equivalent startup-timing namespace, so the module would be dead code.
- [x] 78. Port `core/auth-guidance.ts` messages parity. New `core/auth_guidance.rs`:
      `getProviderLoginHelp` (docs providers.md/models.md paths + `/login`),
      `formatNoModelsAvailableMessage`, `formatNoModelSelectedMessage`,
      `formatNoApiKeyFoundMessage(provider)` ("the selected model" when
      unknown). Wired into `list_models.rs`: an empty auth-gated model set now
      surfaces the guidance (previously a bare "No models available."). The
      per-provider no-api-key error surfaces in pi-ai remain a deliberate
      follow-up (the guidance module + formatters are ready to append there).
- [x] 79. Port `core/settings-diagnostics.ts` + `diagnostics.ts` — the
      `ResourceDiagnostic`/`ResourceCollision` types landed in new
      `core/diagnostics.rs` (warning/error/collision kinds); skills +
      prompt-template loaders emit them and run.rs surfaces them as warnings.
- [x] 80. Extended-messages + provider-composer edge audits with tests.
      AUDIT: `provider_composer.rs` (applyModelsJson/applyExtension/override/
      compat) landed with a test suite; extended-message wiring to the provider
      was already landed as T3 #35. No new edge gap found in this pass.

## T7 — Data model, session tree, export, RPC edge parity

- [x] 81. Negative-usage decision: widened pi-ai `Usage` token counts,
      optional cache-write/reasoning counts, session stats, usage totals, and
      model cost accounting to signed `i64`/preserved negative values; normal
      context/cache estimates clamp ledger corrections out of window arithmetic.
      (unit: `cargo test --workspace`)
- [x] 82. Re-enabled the upstream negative-adjustment conformance case (C-neg)
      in both the in-memory/JSONL agent backend and SQLite backend; `-2`
      input/total and `-0.5` cost produce `uncached=10`, `total=18`,
      `costTotal=9.5`. (unit: `cargo test --workspace`)
- [x] 83. Session tree/navigation parity: `get_tree` RPC. `modes/rpc.rs`
      `build_tree` now matches upstream `SessionManager.getTree()`: nodes as
      `{ entry, children, label? }` (label emitted only when `labelsById` has
      the id, sourced from `JsonlSession::get_label`), children sorted by
      entry timestamp ascending, and self-parent / orphan (missing-parent)
      entries treated as roots — fixing the prior revision, which emitted no
      label and left children unsorted in a stale-clone snapshot. The
      interactive "entry-tree banner" render remains PTY-bound with the TUI
      surface.
- [x] 84. Session tree tests. 3 `RpcRuntime::build_tree` tests: nest + child
      timestamp ordering, self-parent/orphan-root handling, label emitted only
      when resolved.
- [x] 85. export_html full parity audit. AUDIT: `core/export_html.rs` already
      ports the upstream `export-html/index.ts` `generateHtml`/`exportFromFile`
      pipeline (color/theme derivation, `js_replace` ES-substitution semantics,
      base64 session payload, `TEMPLATE_RENDERED_TOOLS`). mermaid/search are
      client-side features in the vendored template (template.js/html), covered
      byte-for-byte by the oracle goldens; the file-export path writes directly
      to `outputPath` with no temp files (no tmp-cleanup divergence). Custom
      tool pre-rendering (extension `renderCall`/`renderResult`) is the one
      documented no-op seam pending the extension system.
- [x] 86. export_html fixture expansion. The `export_session.jsonl` parity
      fixture now exercises tool-call + thinking content blocks, a `compaction`
      entry (with `retainedTail` + usage), and a `branch_summary` entry (with
      usage), alongside the us/agent exchange; the oracle goldens (dark/light)
      were regenerated and both remain byte-identical. `export_html_parity.rs`
      gains a `fixture_covers_tool_compaction_and_branch_summary` coverage
      test (8 non-session entries). Workspace green at 1416.
- [x] 87. RPC get_entries/get_tree/get_messages/get_last_assistant_text audit.
      `get_entries` preserves oldest-first order, `since`, and `leafId`;
      `get_tree` returns the labeled tree plus leaf; and the message queries
      now rebuild the active branch after switch/fork, trim the last assistant
      text, and return real entry ids for fork messages. Covered by RPC tests.
      Runtime honoring of set_auto_compaction/retry/steering/follow-up remains
      tracked separately in #88.
- [x] 88. RPC runtime audit: set_auto_compaction/retry/steering/follow-up
      honored. (unit; mock) `RpcRuntime` applies command mutations to its live
      flags, persisted settings, queue modes, prompt configuration, and
      `get_state` response. Evidence: `cargo test -p pi-coding-agent --offline
      rpc_runtime_control_commands_update_settings_and_state`, the existing
      `rpc_applies_settings_to_stream_compaction_retry_and_queues`, queue-mode
      drain tests, and the full RPC suite.
- [x] 89. `pi update` parity: version-plan checks use the upstream latest-release
      endpoint/retry semantics; `--models` refreshes the pi.dev catalogs with
      bounded parallel requests, retries, freshness/ETag handling, persistence,
      and the existing `PI_SKIP_VERSION_CHECK` startup seam. (unit; mock)
- [x] 90. Update tests cover offline/skip-check behavior, normalized release
      payloads, model-catalog success and transient HTTP failure paths, and the
      offline CLI update error. (unit; mock) Evidence: `cargo test -p
      pi-coding-agent --offline --lib core::version_check::tests`, `cargo test
      -p pi-coding-agent --offline --lib core::remote_catalog_provider::tests`,
      and `cargo test -p pi-coding-agent --offline --test cli_commands update_`.

## T8 — Evals, packaging, parity suite

- [ ] 91. pi-evals: capture usage tokens from subprocess runs (parse session
      JSONL usage) so eval metrics match upstream.
- [ ] 92. pi-evals: extension-scenario diagnostics under faux (unscorable →
      scorable).
- [ ] 93. `scripts/parity-suite.mjs`: CLI matrix checks (exit codes/format vs
      upstream).
- [ ] 94. Parity suite: golden RPC transcripts. (fixtures)
- [ ] 95. Parity suite: session-file byte fixtures (v1/v2/v3/v4 goldens).
- [ ] 96. Parity suite: settings/auth/models.json on-disk round-trip goldens.
- [ ] 97. Release-build verification: `cargo build --release` + full binary
      suite in release. (live)
- [ ] 98. PLAN.md session-13 ledger update + reviewer-gate prep (§0.3).

## T9 — Final 100% verification pass

- [ ] 99. Full-surface audit: §2.2 env vars, §2.3 on-disk formats, §4.4 RPC
      taxonomy — each demos against the real binary. (live)
- [ ] 100. Final clean-room check: fresh clone → workspace tests green,
      0 warnings, clippy -D warnings clean, flag/env/tool/provider matrix
      recorded in PLAN.md with tiers, milestone tagged.

---

## Supplemental source-audit tasks (S1)

The first 100 entries were the original work queue. This section is the
additional inventory required to make the ledger exhaustive. These items came
from a second pass over every package TODO, the pinned upstream module map,
the current Rust implementation, and the documented Session 11 divergences.
The denominator is allowed to grow if the source inventory discovers a new
observable contract; the ledger is frozen only by S-001 and the final audit.

### S1-A — inventory and evidence control

- [ ] S-001 Complete a source-to-source inventory of every upstream exported
      runtime surface and record one ledger ID per observable behavior; freeze
      the denominator only after the inventory and all TODO files reconcile.
- [ ] S-002 Reconcile stale `TODO.md`, session reports, README, and PLAN claims
      against the current source and replace every “done” claim lacking an
      exact test or live command with an open task.
- [ ] S-003 Add a reproducible ledger-progress checker that counts only
      checked/unchecked tasks in this file and fails on malformed checklist
      lines or duplicate task IDs.
- [ ] S-004 Run the independent-reviewer gate against this exhaustive ledger,
      including a review of every deferred divergence and evidence tier.

### S1-B — pi-ai residual provider and transport parity

- [ ] S-005 Wire deferred-response fetch/cancel through the coding-agent model
      runtime, interactive mode, RPC mode, and provider-composer path; test a
      deferred response from request through resolution and cancellation.
- [ ] S-006 Complete lazy API capability propagation for deferred fetch/cancel,
      including missing-capability error text and models-store overrides.
- [ ] S-007 Port the upstream image retry loop and its abort/quota/error
      classification for image generation requests.
- [ ] S-008 Complete constrained-sampling/grammar tool support for every
      adaptor that advertises strict or grammar tools; reject unsupported
      schemas with the upstream diagnostics.
- [ ] S-009 Complete Codex WebSocket session caching/reuse and the
      `websocket-cached` transport behavior, including eviction and close/error
      recovery.
- [ ] S-010 Complete AWS credential/profile-file and region resolution parity
      for Bedrock, with environment/config precedence fixtures.
- [ ] S-011 Complete Google Vertex ADC file, token URI, scope, refresh, and
      project/location precedence parity.
- [ ] S-012 Complete Cloudflare AI Gateway account/gateway binding and all
      documented base URL/header precedence cases.
- [ ] S-013 Complete GitHub Copilot OAuth refresh, enterprise-domain, token
      exchange, and expired-credential behavior in the auth store and CLI.
- [ ] S-014 Complete Anthropic OAuth provider-name mapping, adaptive-thinking
      replay, eager beta headers, client injection, deferred tool references,
      and server-side fallback behavior.
- [ ] S-015 Add provider-by-provider request/stream/usage/error fixtures for
      all catalog providers, including each advertised API variant and an
      explicit no-API implementation check where upstream intentionally has
      one.
- [ ] S-016 Finish remote model-catalog HTTP semantics: RFC date parsing,
      freshness, ETag/304 handling, 404/501 handling, atomic persistence, and
      offline behavior.
- [ ] S-017 Add model-catalog refresh and runtime-merge tests for every
      provider shape, including custom providers, malformed payloads, and
      generated-at precedence.

### S1-C — pi-agent contract and harness integration

- [x] S-018 Emit upstream `tool_execution_update` events from built-in tools
      through the `onUpdate` callback, including throttling and final-update
      ordering. Evidence (unit): `cargo test -p pi-agent --offline --test
      tools --quiet` covers coalesced progress, final truncation metadata,
      timeout output/full-file preservation, and normal/error output; rich-loop
      tests cover live parallel updates, final ordering, and late-callback
      suppression.
- [x] S-019 Preserve and apply tool `terminate` hints in the model-facing
      result/session/RPC event contract, including mixed parallel batches.
      (unit; mock) Lifecycle end events now retain the raw upstream
      `AgentToolResult`, RPC serialization preserves `terminate`, and the
      session writer stores the hint as the JSONL message-entry marker while
      leaving the model-facing `ToolResultMessage` unchanged. Mixed and
      all-terminating parallel batches plus RPC/session persistence are covered
      by `cargo test -p pi-agent --offline rich_agent::tests --quiet` and
      `cargo test -p pi-coding-agent --offline modes::rpc::tests --quiet`.
- [x] S-020 Verify exact `AgentTool` prepare/execute/error semantics for every
      built-in and coding-agent tool against upstream malformed-call fixtures;
      close any remaining signature or payload drift. Evidence (unit):
      `cargo test -p pi-agent --offline rich_agent::tests --quiet`,
      `cargo test -p pi-agent --offline --test tools --quiet`, and
      `cargo test -p pi-coding-agent --offline --test tool_contract` cover
      mutable before-hook arguments, edit preparation, optional details,
      parallel immediate errors/after-hook overrides, and malformed read,
      write, edit, bash, ls, find, and grep calls.
- [ ] S-021 Integrate the `AgentHarness` lane/session abstraction into the
      coding-agent run path instead of maintaining a parallel direct-loop
      implementation. (Partial unit/integration slice: configured harnesses
      now own the one-shot print-path and JSON-mode Agents plus their
      in-memory main-lane transcripts, and interactive turns now seed a
      configured harness from the current transcript; JSONL/RPC paths and
      secondary lanes remain open.)
- [ ] S-022 Wire the complete harness event and telemetry lifecycle into print,
      interactive, JSON, JSONL, and RPC modes with span/event golden checks.
      (Partial unit/integration slice: configured print, JSON, and interactive
      harness paths emit ordered run lifecycle events and settled
      `pi.harness.run` spans; the shared adapter covers RPC loops, while
      JSONL, complete mode-specific golden envelopes, and
      persistence/secondary-lane assertions remain open.)
- [x] S-023 Add panic-safe telemetry callback settlement equivalent to the
      upstream `try/catch/finally` span lifecycle. (unit) The in-memory
      adapter catches callback unwinds, settles spans as automatic errors
      without inspecting panic payloads, preserves explicit statuses, resumes
      the original panic, and settles nested spans inner-first. Evidence:
      `cargo test -p pi-telemetry --offline --quiet` (6 passed),
      `cargo test -p pi-agent --offline --quiet`,
      `cargo test -p pi-tui --offline --quiet` (186 passed), and
      `cargo test --workspace --offline --quiet`.
- [x] S-024 Complete JSON-schema validation parity for unions, arrays, numeric
      bounds, formats, additional properties, and partial tool-call arguments;
      compare all diagnostics with upstream. (unit) Validation now resolves
      local refs, validates `allOf`/`anyOf`/`oneOf`/`not`, tuple and constrained
      arrays, enums/consts, object property rules, numeric/string bounds,
      common formats, and nullable optional fields while retaining upstream
      primitive coercion. Evidence: `cargo test -p pi-agent --offline
      tools::validation -- --nocapture` (12 passed),
      `cargo test -p pi-agent --offline --quiet` (174 passed), and
      `cargo test --workspace --offline --quiet`.

### S1-D — coding-agent product/runtime parity

- [x] S-025 Add automatic compaction/context rebuilding to the one-shot print
      path, including settings thresholds, retained-tail entries, and
      continuation after compaction. (mock) Evidence: `cargo test -p
      pi-coding-agent --offline --test cli_print_parity` (4 passed), including
      the forced JSONL compaction/continuation test; `cargo check --workspace
      --offline`.
- [ ] S-026 Complete legacy v1/v2/v3-to-v4 import integration for every resume,
      switch, fork, and `/import` path, not only the standalone converter.
- [ ] S-027 Port TypeScript extension execution semantics or provide a proven
      equivalent embedded runtime; cover extension commands, hooks, renderers,
      and failure isolation.
- [ ] S-028 Port the upstream self-update path (`pi update --self`) or document
      and test the exact supported replacement behavior for this distribution.
- [ ] S-029 Complete install-telemetry report transport, opt-out, retry, and
      offline behavior where the upstream CLI performs the network ping.
- [ ] S-030 Wire cache-miss notices and “cache re-billed” display data into the
      interactive transcript/footer, including setting gates and reset events.
- [ ] S-031 Port the `PI_TIMING=1` startup timing surface or prove/document its
      intentional non-port with a compatibility test and user-facing fallback.
- [ ] S-032 Wire provider-specific no-key/auth guidance into every model
      resolution and provider error path, preserving upstream help text.
- [ ] S-033 Complete interactive slash-command behavior audits for export,
      import, share, trust, login/logout, new/resume, fork/clone, tree, and
      reload; each command needs a real terminal or fixture transcript.
- [ ] S-034 Finish ConfigSelector project/global inheritance, package pattern
      toggles, search/navigation, write-scope persistence, and close behavior
      against the upstream component.
- [x] S-035 Add PTY snapshot/golden tests for ConfigSelector rendering,
      resizing, glyph probes, keyboard navigation, and settings writes. Added
      `crates/pi-coding-agent/tests/config_selector_pty.rs`, which drives the
      real `pi config --approve` binary through tmux, checks the visible global
      snapshot and Unicode footer, resizes the pane, navigates/toggles global
      and project rows, verifies both settings files, and inspects raw
      alternate-screen/cursor cleanup sequences. The resize boundary now
      invalidates `pi-tui::Tree` differential state before the next redraw.
      Evidence: `cargo test -p pi-coding-agent --offline --test
      config_selector_pty` (1 passed). The broader slash-command PTY matrix
      remains S-056.
- [ ] S-036 Complete project-trust safety matrix for all commands and resource
      loaders, including saved trust, default trust, `-a`, `-na`, and prompts.

### S1-E — RPC behavior and concurrency

- [x] S-037 Refactor RPC input handling so abort, steer, follow-up, and state
      requests can be received while a prompt is streaming, matching upstream
      concurrency and response ordering. (unit; mock) Prompts now run in a
      detached Tokio worker with ordered event/completion delivery while the
      JSONL loop continues reading control/state commands. Verified with
      `cargo test -p pi-coding-agent modes::rpc::tests --offline` (25 passed),
      `cargo test --workspace --offline`, and a faux-provider JSONL smoke run
      showing prompt preflight → get_state → stream event/agent_settled →
      abort response.
- [x] S-038 Verify queue drain points, one-at-a-time/all modes, cancellation,
      pending counts, steering precedence, and follow-up precedence with live
      streamed transcripts. (unit; mock) RPC tests cover both queue batch
      modes, turn-boundary precedence, pending-state reporting, detached abort
      cancellation, live mode changes, and post-settlement abort ordering.
      The faux stream supplies multiple deterministic turns for queued
      follow-ups. Verified with `cargo test -p pi-coding-agent
      modes::rpc::tests --offline` (25 passed) and `cargo test --workspace
      --offline`.
- [x] S-039 Make retry preserve first-attempt deltas, retry status events,
      final usage/stop reason, abort-retry, and response persistence exactly.
      (unit; mock) Retry attempts retain first-attempt deltas and terminal
      messages, emit upstream-compatible `auto_retry_start`/
      `auto_retry_end` events, preserve final usage and stop reason, accept
      `abort_retry` while detached, and persist intermediate failures while
      keeping failed retries out of the live context. Verified with
      `cargo test -p pi-agent --offline rich_agent::tests -- --nocapture`
      (7 passed), `cargo test -p pi-coding-agent --offline
      modes::rpc::tests -- --nocapture` (29 passed), and
      `cargo test --workspace --offline`.
- [x] S-040 Apply all settings values (reserve/keep tokens, retry provider
      options, queue modes, transport, model) in RPC runtime behavior instead
      of using only defaults or approximations. (unit; mock) RPC runtime
      construction now derives configured compaction reserve/retention,
      provider timeout/retry/delay settings, HTTP/WebSocket transport
      settings, thinking budgets/levels, model selection, and queue modes;
      prompt and compaction requests receive the same provider defaults while
      preserving request-local overrides. Verified with `cargo test
      -p pi-coding-agent --offline modes::rpc::tests -- --nocapture` (30
      passed) and `cargo test --workspace --offline`.
- [x] S-041 Audit RPC abort vs abort-bash lifecycle, terminal events, and
      session records under simultaneous prompt/tool activity. (unit; mock)
      `abort` now targets only the agent/retry signals, while standalone bash
      tasks run concurrently, `abort_bash` interrupts silent or active
      processes, and bash records defer until agent settlement to preserve
      message ordering. RPC now emits upstream lifecycle, message terminal,
      turn, and tool execution events. The rich loop continues after
      non-terminating tool batches and propagates abort into bash tools.
      Verified with `cargo test -p pi-agent --offline
      rich_agent::tests -- --nocapture` (8 passed), `cargo test
      -p pi-coding-agent --offline modes::rpc::tests -- --nocapture` (34
      passed), and `cargo test --workspace --offline`.
- [x] S-042 Produce golden transcripts for every RPC command and event type,
      including switch/fork/clone, queue modes, compaction, export, and all
      error responses. (unit; mock; live) Added deterministic command and
      event fixtures at `crates/pi-coding-agent/tests/fixtures/rpc/`, covering
      every RPC command, core/session lifecycle event, switch/fork/clone,
      queue modes, compaction, export, malformed commands, and failure
      responses. The dispatcher now converts unexpected command/task errors to
      RPC failures; standalone bash emits incremental `bash_execution_update`
      records. Verified with `cargo test -p pi-coding-agent --offline
      rpc_command_golden_transcript_matches_fixture`, `cargo test
      -p pi-coding-agent --offline rpc_event_golden_transcript_covers_wire_event_types`,
      `cargo test -p pi-coding-agent --offline
      malformed_rpc_lines_emit_parse_failures`, the full RPC test module, and
      a live `--mode rpc` bash smoke showing update-before-response ordering.

### S1-F — pi-server/pi-client auxiliary library parity

- [ ] S-043 Port the upstream `testing/service.ts`, test client, deferred
      helpers, and test-server fixtures.
- [ ] S-044 Run the complete server protocol/service conformance suite,
      including malformed frames, handshake errors, snapshots, and lifecycle
      events.
- [ ] S-045 Port client reconnect/backoff and connection-state listener
      behavior, including in-flight request failure and replay rules.
- [ ] S-046 Complete client session lease acquire/release/reconcile,
      exclusive-attach, snapshot reconciliation, and detach-on-close behavior.
- [ ] S-047 Complete client dispose semantics, request timeouts, and transport
      shutdown/error mapping.
- [ ] S-048 Add the transport-factory abstraction and every upstream transport
      option beyond the Unix implementation.
- [ ] S-049 Add reconnect/lease-churn/session-close end-to-end tests over a
      real socket with deterministic timing seams.

### S1-G — pi-tui and terminal parity

- [x] S-050 Complete cell-dimension querying/updating and use measured
      dimensions in image sizing rather than fixed defaults. (unit) Raw Unix
      stdin now flows through `StdinBuffer`; interactive/config loops pass the
      complete response to `Tree::consume_cell_size_response` before key
      dispatch, preserving following input. Verified with
      `cargo test -p pi-tui --offline` (186 passed) and
      `cargo test --workspace --offline`.
- [ ] S-051 Add capability-matrix tests for Kitty, Ghostty, WezTerm, Warp,
      iTerm2, VS Code, Alacritty, JetBrains, screen, tmux, Windows Terminal,
      and unknown terminals.
- [ ] S-052 Complete Editor IME/selection/kitty-event edge behavior and
      bracketed-paste parity from the upstream fixtures.
- [ ] S-053 Complete autocomplete debounce, cancellation, marked-input,
      slash/path provider, and selection-application parity.
- [ ] S-054 Complete SettingsList callback, disabled-row, selection, and
      persistence semantics.
- [ ] S-055 Complete marked/Markdown edge parity and renderer snapshot coverage
      for all upstream block shapes.
- [ ] S-056 Add PTY end-to-end coverage for the full interactive slash-command
      matrix, resize/raw-mode cleanup, alt-screen restoration, and terminal
      feature probes.
- [ ] S-057 Add cross-platform terminal capability and cleanup checks for
      Windows console, Unix terminals, tmux, and nested alternate screens.

### S1-H — evals, fixtures, packaging, and final evidence

- [ ] S-058 Capture usage/cost tokens from subprocess session JSONL in pi-evals
      so evaluation metrics match the upstream harness.
- [ ] S-059 Make the extension scenario scorable under faux, or provide the
      same deterministic extension fixture/diagnostic contract as upstream.
- [ ] S-060 Add provider/CLI exit-code and output-format matrix checks to the
      parity suite, not only smoke checks.
- [ ] S-061 Add golden RPC transcript fixtures and byte-level session fixtures
      for v1/v2/v3/v4 migration and current writes.
- [ ] S-062 Add settings/auth/models.json and package-resource on-disk golden
      fixtures, including unknown-key preservation and lock/retry behavior.
- [ ] S-063 Add full provider/adaptor fixture execution to the release parity
      suite with network-free mock servers and explicit live smoke cases.
- [ ] S-064 Port telemetry schema conformance tests and include them in the
      release gate.
- [ ] S-065 Synchronize README, per-crate TODOs, session reports, and PLAN with
      the final ledger state and remove stale historical claims.
- [ ] S-066 Freeze the final denominator after S-001, run the full source/TODO
      audit, and record the final 100.00% evidence only when no open or
      unclassified task remains.

---

## Conventions

- Track each task as Done only with evidence: tier + exact command/fixture.
- After each committed task/group: push immediately (standing rule).
- Tasks roughly: ~40 pure ports of pinned upstream files, ~30 audit-then-close,
  ~20 tests/verification, ~10 process/gates.
- When a task's "upstream file" is named, pin it to commit 5cd93f688aaab89dbb6dfa4aca535f21796ae185 (v0.84.2).
