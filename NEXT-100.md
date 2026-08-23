# NEXT-100 — micro-task tracker for the full 1:1 pi → pi-rust conversion

Session date: 2026-08-23 (operator: "going to bed — document or something")
Author: pi (Claude), planning pass grounded in a live repo audit.
Base revision: HEAD 83e55cb (1240 tests at last clean revision).

## Current state (verified 2026-08-23, before these tasks)

- HEAD `83e55cb` on main; last clean revision was 1240 tests / 0 warnings.
- **Working tree is RED and uncommitted**: in-flight `/share` + `--export` +
  editor-autocomplete diff (5 modified files + `tests/cli_export.rs` untracked).
  `cli_export.rs:15` has an unterminated char literal (the `"data-session="`
  string on line 13 is broken) — the test target does not compile. The lib/
  bin target builds.
- Documented remaining gaps (PLAN.md carry-forward + per-crate TODOs): OAuth
  device-code flows, codex WebSocket transport (SSE fallback today),
  `/share` GitHub-gist OAuth (in-progress in the working tree), ConfigSelector
  full TUI component, `update --models` pi.dev fetch seam, models.json runtime
  merge seam, pi-ai Usage u64 negative-adjustment decision, TUI alt-screen full
  swap + ICU word segmentation, server/client concurrency surfaces (leases,
  reconnect, queuing).
- **Additional gaps found in this audit** (not in any TODO file):
  - Missing CLI flags vs 0.84.2 surface: `--fork`, `--approve/-a`,
    `--no-approve/-na`, `--no-builtin-tools/-nbt`, `--extension/-e`,
    `--no-extensions/-ne`, `--skill`, `--no-skills/-ns`, `--prompt-template`,
    `--no-prompt-templates/-np`, `--theme`, `--use-theme`, `--no-themes`,
    `--no-context-files/-nc` (+ print-mode `--steer/--follow-up/--compact`).
  - No auto-compaction wiring in `run.rs` (RPC `compact` command exists).
  - No JSON-event mode (`--mode json`; upstream `modes/json-event.ts`).
  - `image` tool not registered: run.rs exposes 7 tools, upstream has 8
    (bash/read/write/edit/edit-diff/ls/find/grep/image).
  - coding-agent core modules not ported as modules (functionality may exist
    elsewhere — audit first): `bash-executor`, `exec`, `system-prompt` wiring,
    `skills` loader, `prompt-templates` + `resource-loader`, `http-dispatcher`,
    `session-cwd`, `cache-stats`, `timings`, `auth-guidance`,
    `settings-diagnostics`, `diagnostics`, `project-trust`/
    `trust-manager`, `messages` (extended), `footer-data-provider`.
  - pi-agent: AgentTool contract not upgraded to upstream shape
    (label / prepareArguments / execute(toolCallId, params, signal, onUpdate)
    -> AgentToolResult); `validateToolArguments` not ported/wired.
  - pi-client: no reconnect state machine, lease/reconcile parity partial
    (session_handle.rs landed), dispose/promise-timeout/transport-factory gaps.
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
3. **T7 product surfaces** third: `pi update`, session tree, export_html,
   negative-usage edge. RPC-edge audits after the visible surfaces.
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
      image.
- [x] 26. `execute` upstream signature + `onUpdate` → rich loop emits
      `tool_execution_update`.
- [x] 27. Terminate-hint plumbing in `rich_agent.rs`.
- [x] 28. Migrate every tool constructor + run.rs call sites.
- [x] 29. Port `validateToolArguments` (tool-args JSON-schema validation).
- [x] 30. Wire validation into `prepare_tool_call` with upstream errors.
- [x] 31. Tool-args validation tests (schema errors, unknown keys,
      partial-JSON args).
- [x] 32. Image tool parity audit + register model-facing `image` in run.rs
      (7 → 8 built-in tools), match `/images` toggle.

## T3 — coding-agent run-path parity

- [x] 33. Wire auto-compaction into run path (settings threshold → compact →
      continue; upstream `core/compaction/` loop).
- [x] 34. Binary-level auto-compaction test (JSONL gains compaction entry). (mock)
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
- [ ] 54. Client reconnect state machine + connection-state listeners.
- [ ] 55. Client lease/exclusive-attach parity (reconcile, detach-on-close).
- [ ] 56. Client dispose semantics + promise timeouts.
- [ ] 57. Transport factory abstraction beyond unix (async-trait).
- [ ] 58. Client↔server E2E under reconnect + lease churn. (unit/mock)

## T5 — TUI completion

- [ ] 59. ConfigSelector full TUI component (config command interactive).
      DATA LAYER DONE (model + producer + buildGroups): `interactive/
      config_selector.rs` ports the upstream config-selector.ts data model
      (`PathMetadata`/`ResolvedResource`/`ResolvedPaths`, `build_groups`,
      `format_base_dir`) with 5 tests. `core/package_manager.rs` now ports the
      full `resolve()` **producer**: on-disk resource collection (recursive
      `collect_files`, `collect_skill_entries` pi/agents modes, auto
      extension/prompt/theme collectors, ancestor `.agents/skills` discovery),
      include/`!exclude`/`+force`/`-force` pattern filtering + autoload-disabled
      deltas, precedence-ranked first-wins collision resolution with canonical
      dedup, project-over-global package dedupe, and install-on-missing via an
      `on_missing` seam — 8 fixture-based tests (auto/user/project discovery,
      filtering, ignore-file exclusion, precedence ranking, npm missing
      skip/error, resolve→build_groups integration). Wired into
      `commands/config.rs` (`summarize_resources` now renders `resolve()` →
      `build_groups` groups). STILL PENDING: the interactive
      render/`handleInput` component (PTY-bound; needs pi-tui Input/Container
      equivalents and glyph-probe setup).
- [ ] 60. ConfigSelector snapshot tests. PARTIAL: 5 unit tests cover the
      buildGroups ordering/labels/display-name behavior + 8 resolve() producer
      tests (collection/filter/precedence/scopes) + `resolve_feeds_build_groups`
      integration; full interactive-component snapshot tests await the render
      surface.
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
      via `bash_tool`. Only noted gap: live `onUpdate` throttling, a streaming
      concern orthogonal to capture parity.
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

- [ ] 81. Negative-usage decision: widen pi-ai Usage token counts to i64;
      ripple through agent/reducer + session-backends stats.
- [ ] 82. Re-enable negative-adjustment conformance case (C-neg) + regressions.
- [ ] 83. Session tree/navigation parity: `get_tree` RPC + entry-tree banner.
- [ ] 84. Session tree tests.
- [ ] 85. export_html full parity audit (dark/light, mermaid, search, tmp cleanup).
- [ ] 86. export_html fixture expansion (tools/compaction/summary rows).
- [ ] 87. RPC get_entries/get_tree/get_messages/get_last_assistant_text audit.
- [ ] 88. RPC runtime audit: set_auto_compaction/retry/steering/follow-up honored.
- [ ] 89. `pi update` parity: version check + `--models` pi.dev fetch seam;
      `PI_SKIP_VERSION_CHECK`.
- [ ] 90. Update/tests (offline, skip-check, fetch failure paths). (mock)

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

## Conventions

- Track each task as Done only with evidence: tier + exact command/fixture.
- After each committed task/group: push immediately (standing rule).
- Tasks roughly: ~40 pure ports of pinned upstream files, ~30 audit-then-close,
  ~20 tests/verification, ~10 process/gates.
- When a task's "upstream file" is named, pin it to commit 5cd93f688aaab89dbb6dfa4aca535f21796ae185 (v0.84.2).
