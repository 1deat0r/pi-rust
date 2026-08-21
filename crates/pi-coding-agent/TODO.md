# pi-coding-agent — port status

P4 milestone committed: working `pi` binary (args, config/env, run path).

## Done
- args.rs: CLI parser with upstream flag surface (commands via flags; value
  flags support `--flag value` and `--flag=value` incl. short aliases;
  positional messages, `@file` args, `--` terminator, unknown-flag capture,
  --help/--version). 7 unit tests.
- config.rs: APP_NAME/TITLE/VERSION, config dir name, env var names
  (PI_CODING_AGENT_DIR/SESSION_DIR/MODEL/PROVIDER/KEY/SESSION_ID/...),
  expandTildePath, getAgentDir/getSessionDir/settings/auth paths, provider
  + model resolution defaults (google), offline flag.
- run.rs: `pi -p` non-interactive path — provider/model resolution (faux
  only today; clear error otherwise), scripted faux responses, agent loop
  (pi-agent), session persistence through JsonlSessionRepo (cwd-encoded dir,
  v4 header + message entries, seq chains), optional --name.
- main.rs: dispatch help/version/run; session path printed with --verbose.
- E2E verified: `pi --version` -> `pi 0.84.2`; `pi -p --provider faux "hello"`
  prints the faux reply and persists to
  `--<cwd-with-dashes>--/<iso-ts>_<id>.jsonl` with a v4 header and chained
  message entries.

## Remaining (upstream mapping — big items first)
- core/settings-manager + settings.json (global + project merge, full schema)
- core/model-registry + model-resolver + models-store + model catalog data
  (generate from upstream models.generated.ts)
- Real providers in pi-ai (anthropic/openai/google first), auth storage,
  http dispatcher/proxy
- core/tools: bash, read, write, edit, edit-diff, ls, find, grep
- Interactive TUI mode (pi-tui) + RPC JSONL mode
- Skill/prompt/extension loaders, slash commands, system prompt builder
- Session tree manager (resume picker), project trust, compaction,
  export-html, usage totals, telemetry wiring
- Commands: install/remove/update/list/config/auth, list-models
