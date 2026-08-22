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

## Not yet ported (upstream mapping)
- Agent loop + harness (agent.ts / agent-loop.ts, harness/* beyond the
  session/compaction layers): events, reducer, prompt templates, system
  prompt, skills, image tool, file-mutation-queue, tool-context, telemetry
  wiring, stream-fn, proxy.
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
