# pi-coding-agent — port status

P4 milestone committed: working `pi` binary (args, config/env, run path).
Settings manager (full upstream surface) landed.

## Done
- core/session_migration.rs: legacy .session migration surface ported from
  packages/coding-agent/src/core/session-manager.ts (migrateSessionEntries /
  parseSessionEntries / assertValidSessionId / CURRENT_SESSION_VERSION):
  v1→v2 (id/parentId tree + compaction firstKeptEntryIndex→firstKeptEntryId)
  and v2→v3 (hookMessage role→custom), idempotent (+ 4 extra probe tests:
  hookMessage rename, compaction index conversion, malformed-line skip,
  id-pattern validation). NOTE: the pi-agent TODO's old pointer to
  "jsonl/repo.ts" was wrong; the JSONL codec is v4-only and the v3→v4 import
  path lives in the coding-agent session runtime (P4/P8).
- core/tools: ls, find, grep ported (packages/coding-agent/src/core/tools/) —
  model-facing text output 1:1, spawned via the exact upstream binaries and
  argument sets (fd for find, rg for grep; fd is a harness-tool dependency
  like upstream). ls sorts case-insensitively with directory suffix and
  dotfiles; find relativizes to the search root and preserves trailing '/';
  grep streams rg --json match events with context blocks, line truncation
  (500 chars), and upstream notices. 24 tool tests (6 ls / 7 find / 11 grep).
  Registered in run.rs (7 built-in tools total; gated on --no-tools).
- run.rs settings wiring: `pi -p` now resolves provider/model
  CLI -> PI_PROVIDER/PI_MODEL env -> settings.json defaultProvider/defaultModel
  (project merged over global) -> google/default. Settings default model only
  applies when no explicit provider source exists (upstream pairs the settings
  default pair; a CLI/env provider resolves models from its own scope).
  3 binary-level E2E tests (spawn the real `pi`) + 3 resolver unit tests.
- args.rs: CLI parser with upstream flag surface (commands via flags; value
  flags support `--flag value` and `--flag=value` incl. short aliases;
  positional messages, `@file` args, `--` terminator, unknown-flag capture,
  --help/--version). 7 unit tests.
- config.rs: APP_NAME/TITLE/VERSION, config dir name, env var names
  (PI_CODING_AGENT_DIR/SESSION_DIR/MODEL/PROVIDER/KEY/SESSION_ID/...),
  expandTildePath, getAgentDir/getSessionDir/settings/auth paths, provider
  + model resolution defaults (google), offline flag.
- core/settings.rs: SettingsManager ported 1:1 from
  packages/coding-agent/src/core/settings-manager.ts — deep merge (project
  wins, nested objects merge), modified-field tracking with external-key
  preservation, key-removal semantics for Option setters, flush write queue,
  reload, drainErrors with paths, project trust state machine, lazy `.pi`
  dir creation on write only, full migration set (queueMode→steeringMode,
  websockets→transport, skills object→array, retry.maxDelayMs→
  retry.provider.maxRetryDelayMs), PackageSource untagged enum, full
  accessor surface (theme incl. slash auto-themes, compaction/retry/
  branchSummary/terminal/images/markdown/warnings/thinkingBudgets, external
  editor precedence, sessionDir/shellPath ~ expansion, defaultProjectTrust
  global-only read, analytics trackingId, packages/extensions/skills/
  prompts/themes project variants). FileSettingsStorage (`.lock` sibling,
  retry 10x/20ms, lazy dir create) + InMemorySettingsStorage.
  71 settings tests (23 lib helpers + 48 integration incl. upstream oracle
  port at tests/settings_sm.rs).
- run.rs: `pi -p` non-interactive path — provider/model resolution (faux
  only today; clear error otherwise), scripted faux responses, agent loop
  (pi-agent), session persistence through JsonlSessionRepo (cwd-encoded dir,
  v4 header + message entries, seq chains), optional --name.
- main.rs: dispatch help/version/run; session path printed with --verbose.
- E2E verified: `pi --version` -> `pi 0.84.2`; `pi -p --provider faux "hello"`
  prints the faux reply and persists to
  `--<cwd-with-dashes>--/<iso-ts>_<id>.jsonl` with a v4 header and chained
  message entries.


## Done (Session 9 — model registry + --list-models)
- `--list-models [search]` flag + `cli/list-models.ts` port
  (src/list_models.rs): auth-gated model table over the pi-ai Models facade
  (builtin registry), upstream column format, formatTokenCount. 3 tests;
  `pi --list-models` verified with env keys.
- Model registry surface now available through pi-ai: full 39-provider
  catalog (vendored), createProvider/createModels, checkAuth/getAvailable/
  getAuth/applyAuth, stream dispatch. Coding-agent model-runtime/
  model-config/models-store (file-backed) integration is the next step that
  replaces the run.rs per-provider match.

## Done (Session 10 — RPC + interactive + model runtime)
- core/model_runtime.rs: defaultModelPerProvider + hint parsing + catalog
  resolution over the facade; run.rs routes real providers through the
  facade (auth + dispatch), terminal errors surface as nonzero exits.
- modes/rpc.rs + jsonl.rs + rpc_types.rs: full RPC mode (prompt/steer/
  follow_up with message_update streaming + agent_settled, state/model/
  thinking/queue-mode, bash, session, messages commands) — live-verified.
- modes/interactive.rs: interactive TUI loop over pi-tui (transcript +
  input bar + inline editing + stream turn + session persistence) —
  tmux-verified.

## Done (Session 11 — RPC compact regression)
- modes/rpc.rs: register a scripted faux provider (single stream/simple pair
  over FauxProviderCore + always-resolving FauxApiKeyAuth) in the runtime
  models facade when --provider faux, closing the documented divergence where
  RPC compact failed without a facade-registered provider. 1 regression test.

## Remaining (upstream mapping — big items first)
- Wire SettingsManager into run/main (currently standalone; P4 criterion
  "settings round-trip" is satisfied by the module tests, session wiring is
  part of settings/config TUI integration in P8).
- core/model-registry + model-resolver + models-store + model catalog data
  (generate from upstream models.generated.ts)
- Real providers in pi-ai (openai/google next after anthropic), auth
  storage, http dispatcher/proxy
- core/tools: ls, find, grep, edit-diff, image (bash/read/write/edit done)
- Interactive TUI mode (pi-tui) + RPC JSONL mode
- Skill/prompt/extension loaders, slash commands, system prompt builder
- Session tree manager (resume picker), project trust wiring, compaction,
  export-html, usage totals, telemetry wiring
- Commands: install/remove/update/list/config/auth, list-models
