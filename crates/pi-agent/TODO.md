# pi-agent — port status

P3 data layer committed (fcfd19c .. c00742f): session JSONL v4 codec + storage
+ state + repo + facade + search. Session 7 (2026-08-22) added the full
SessionTree facade, memory.ts port, context.ts port, harness/messages.ts port,
and the upstream session-backend conformance suite (30 cases × 2 backends).

## Done
- edit tool upgraded to the upstream contract (agent edit.ts + edit-diff.ts):
  multiple disjoint edits, exact-then-fuzzy matching (NFKC + smart
  quotes/dashes/spaces + trailing-whitespace normalization), overlap/
  duplicate/missing/empty/no-change errors, BOM and CRLF/LF preservation,
  display diff + unified patch details, prepare-args variants. 27 tests.
- jsonl codec: header + mutations (entry/record/lane/fact) with upstream
  validation (syntax/schema kinds, torn-tail tolerance).
- session state (state.rs): upstream-parity contract — already_exists /
  not_found / invalid_query / invalid_fork_target error codes, order-dependent
  cursors, validated limits, full findEntriesOnBranch (stopAtId/stopAtType,
  cycle detection, not_found on missing start), usage-record stats
  accumulation, insertion-ordered lanes (IndexMap), lane/fact log items,
  full getLog (afterSeq/limit).
- storage: create/load/append/lanes/facts with atomic torn-tail repair;
  findOpenOperations returns records (newest-first) with limit validation;
  fork-error propagation preserves invalid_fork_target.
- session facade (session.rs): full SessionTree surface — view(lane)
  (SessionView with lane-bound append/query), appendMessage/appendCustomEntry
  → id, getLeafId, findEntry/findEntryOnBranch (limit=1 propagation),
  findOpenOperations, getLog(options), query guards (operationKind requires
  type operation_started).
- memory.rs: InMemorySessionStorage + InMemorySessionRepo (Arc<Mutex> sharing
  so opened sessions observe repo state, mirroring upstream references).
- context.rs: SessionContext (messages/thinkingLevel/model/activeToolNames),
  default compaction-boundary transform, per-customType projectors,
  deferred-assistant omission, compaction/branch summary materialization.
  Ported upstream context.test.ts (4 tests).
- messages.rs: bashExecutionToText, createBranchSummaryMessage,
  createCompactionSummaryMessage, createCustomMessage, convertToLlm; full
  CustomAgentMessage surface (bashExecution/custom/branchSummary/
  compactionSummary).
- session-backend conformance harness (tests/conformance.rs): the full 30-case
  upstream conformance.ts ported and executed against BOTH the in-memory and
  JSONL backends (60 executions). Covers entries/lanes (parents, seq, lanes,
  views, duplicate ids, provisioned ids, tool-result terminate, linearization),
  queries/facts (validation, cursors, branch bounds, facts, stats, name
  durability), records/log (lane filters, runId semantics incl.
  OperationStartedRecord.id match, operationKind, open-op enforcement,
  queue cancellation), repository/forks (create/list/open/delete idempotency,
  branch/tree/before/at forks, invalid-fork-target, default-target
  validation), immutability of reads.
- fs: FileSystem trait + StdFileSystem + MemoryFs.
- search.rs: ScanningSessionSearch (session search port).

## Done (Session 8 — harness compaction)
- harness/compaction/: utils.ts (file-ops extraction/formatting +
  serializeConversation), compaction.ts (settings, context-token accounting,
  estimateTokens, cut-point/turn-start selection, prepareCompaction incl.
  previous-summary + virtual retained-tail + split-turn, generateSummary
  [+WithUsage], completeSimpleWithRetries, compact with turn-prefix handling
  and usage combination), branch-summarization.ts (collectEntriesForBranch
  Summary, prepareBranchEntries, generateBranchSummary). LLM calls run through
  a minimal `SimpleModels` seam (harness/models.rs) that stands in for the
  pi-ai `Models` facade until P4 lands the real one; summarization requests
  isolate routing exactly like upstream (cacheRetention none + fresh
  sessionId). 53 lib tests + 20 integration tests ported from upstream
  compaction.test.ts / branch-summarization.test.ts.
- pi-ai utils/retry.rs: retryAssistantCall + isRetryableAssistantError
  (16 tests ported from retry.test.ts).

- harness/telemetry.rs: telemetry span taxonomy (11 harness spans, AI_TELEMETRY/
  HARNESS_TELEMETRY schemas) over pi-telemetry contexts, HOOK_NAMES/EVENT_TYPES. 11 tests.
- harness/env.rs: FileSystem/Shell/ExecutionEnv async traits + StdExecutionEnv (resolve ~/file://,
  timeout validation, kill-on-timeout, stream callbacks). 25 tests.
- proxy.rs: streamProxy — ProxyAssistantMessageEvent wire protocol + client-side reconstruction. 7 tests.
- harness/shell_output.rs: sanitizeBinaryOutput/trimToLastUtf8Bytes/executeShellWithCapture with tail
  truncation + full-output spill. 5 tests.
- agent.rs (+rich loop): Agent class + rich_agent loop (agent.ts/agent-loop.ts additive port) with
  harness events wiring.
- harness/agent_harness.rs: AgentHarness scaffold — 14 tagged-error constructors, Run/Compaction/
  Navigation/Resume outcome unions, ActionInfo, LaneSnapshot/SessionSnapshot, UnavailableRegistry,
  AgentLane async-trait, create() (rejects record-bearing sessions). 4 tests.
- pi-agent workspace: 244 tests passing.
## Done (Session 11 — harness surfaces)
- harness/events.rs: HarnessEventBus (run_start/run_end), per-type on()
  subscriptions, watch handles with buffer-until-start semantics. 4 tests.
- harness/frontmatter.rs + prompt_templates.rs + system_prompt.rs: shared YAML
  frontmatter parser; loadPromptTemplates (dir/file/sourced), parseCommandArgs
  naive quote toggling, substituteArgs ($1/$@/$ARGUMENTS/${@:N}/${@:N:L}),
  formatPromptTemplateInvocation; formatSkillsForSystemPrompt with XML
  escaping + disableModelInvocation filtering. 10 tests.
- harness/tools/image.rs: detectSupportedImageMimeType + manual base64
  encoder with upstream padding (PNG animation scan, BMP checks). 4 tests.
- harness/reducer.rs: validateRecordLog + reduceLaneState port (all 12
  corruption reasons, full lane state reconstruction, effective configuration,
  terminal failure detection). Adds runId to queue_cancelled records
  (upstream QueueCancelledRecord carries optional runId) across codec/storage/
  memory. 7 tests.
- harness/skills.rs: recursive SKILL.md + root-inline discovery, per-dir
  ignore files with gitignore-style matcher, name/description validation,
  formatSkillInvocation, sourced variant. 8 tests.
- harness/tools.rs: withFileMutationQueue (per-canonical-key serialization
  with global registration chain) + ExecutionToolContext. 3 tests.
- harness/result.rs: TaggedError factory (stable `_tag` + toJSON projection)
  and matchError dispatcher, mirroring harness/result.ts. 2 tests.
- stream_fn.rs: default stream-function registry (setDefaultStreamFn /
  getDefaultStreamFn) with the upstream panic message. 2 tests.
- pi-agent workspace tests: 183 passing, 0 warnings.

## Done (Session 11+ — port/agent-harness lane)
- proxy.rs (packages/agent/src/proxy.ts): streamProxy over the proxy-server
  SSE endpoint; ProxyAssistantMessageEvent wire types, partial message
  reconstruction incl. streaming tool-JSON accumulation per content index,
  error/abort finalization. 7 tests.
- harness/telemetry.rs (harness/telemetry.ts): AI_TELEMETRY_SCHEMA +
  HARNESS_TELEMETRY_SCHEMA as JSON data (spread-inlined, HOOK_NAMES /
  EVENT_TYPES references resolved), agent_telemetry_schemas(),
  start_ai_span/start_harness_span. 11 tests.
- harness/env.rs (harness/types.ts + harness/env/nodejs.ts): Outcome/Result
  helpers, FileError/ExecutionError/CompactionError/BranchSummaryError with
  upstream codes, FileSystem/Shell/ExecutionEnv traits, StdExecutionEnv
  (resolve ~/file://, timeout validation + kill, inherit-env replacement,
  chunk callbacks). 25 tests.
- rich_agent.rs (agent.ts + agent-loop.ts additive): RichAgentEvent full
  event stream, QueueMode/PendingMessageQueue/ToolExecutionMode,
  run_rich_agent_loop (steering/follow-up, truncated-batch failing, seq/
  parallel tool batches, before/afterToolCall, shouldStopAfterTurn,
  transformContext), Agent class (subscribe/prompt/continue/steer/followUp/
  abort). 5 tests. Divergence notes: tool_execution_update + terminate hints
  await the AgentTool contract upgrade.
- harness/shell_output.rs + harness/agent_harness.rs landed via the shared
  worktree (see the harness lane's own commits).

## Not yet ported (upstream mapping)
- AgentTool contract upgrade to the upstream shape (label, prepareArguments,
  execute(toolCallId, params, signal, onUpdate) -> AgentToolResult) so the
  rich loop can emit tool_execution_update and terminate hints; requires
  touching every tool constructor + run.rs call sites.
- pi-ai validateToolArguments port (tool-args JSON-schema validation) and
  wire it into prepare_tool_call.
- Coding-agent extended messages wiring (packages/coding-agent/src/core/
  messages.ts) — AgentMessage custom variants are in pi-agent; the coding
  agent's use of BashExecutionMessage/CustomMessage is P4/P8.
- Legacy session migration v1/v2/v3: the upstream functions live in
  packages/coding-agent/src/core/session-manager.ts
  (migrateSessionEntries / parseSessionEntries), NOT jsonl/repo.ts (the
  JSONL codec only reads version-4 files). Port tracked in
  crates/pi-coding-agent/TODO.md.
- Session tree/navigation (session/tree-*), branch summary wiring into the
  coding-agent run path — P3/P8.
