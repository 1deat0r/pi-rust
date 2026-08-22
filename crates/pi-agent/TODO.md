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

## Not yet ported (upstream mapping)
- Agent loop + harness (agent.ts / agent-loop.ts, harness/* beyond the session
  layer): events, reducer, prompt templates, system prompt, skills,
  compaction, branch-summarization, image tool, file-mutation-queue,
  tool-context, telemetry wiring, stream-fn, proxy.
- Coding-agent extended messages wiring (packages/coding-agent/src/core/
  messages.ts) — AgentMessage custom variants are in pi-agent; the coding
  agent's use of BashExecutionMessage/CustomMessage is P4/P8.
- Migration v1 (linear) and v2 (parentId) to v3/v4 (codec handles v3 linear;
  the v1/v2/v3 migration lives in jsonl/repo.ts).
- Session tree/navigation (session/tree-*), compaction + branch summary —
  P3/P8.
