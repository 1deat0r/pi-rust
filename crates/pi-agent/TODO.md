# pi-agent — port status

P3 data layer committed (fcfd19c): session JSONL v4 codec + storage + state.
19 tests ported from upstream jsonl-codec.test.ts / jsonl-storage.test.ts.

## Done
- jsonl codec: header + mutations (entry/record/lane/fact) with upstream
  validation (syntax/schema kinds, customType requirement, intent
  requirement, operation_finished runId, torn-tail syntax tolerance).
- session state: sequence, lanes, id guards, one-open-operation-per-lane
  guard, entry/record queries (order/cursor/limit/type/customType/runId/
  operationKind/lane/toolCallId), branch walk with stopAtType, log, facts
  (name/label), message-count stats.
- storage: create/load/append/appendCustom/lanes/facts with atomic
  torn-tail repair + unterminated-tail repair; MemoryFs test backend.
- fs: FileSystem trait + StdFileSystem + MemoryFs.

## Not yet ported (upstream mapping)
- Agent loop + harness (agent.ts / agent-loop.ts, harness/*): events,
  reducer, prompt templates, system prompt, skills, compaction,
  branch-summarization, tools (bash/read/write/edit/edit-diff/image +
  file-mutation-queue + path-utils), search (grep), telemetry wiring,
  stream-fn, proxy.
- Session facade (session/session.ts): Session wrapper over storage with
  entry-id generation, fork/clone, navigation; tree (session/tree-*);
  context (session/context.ts); memory (session/memory.ts).
- Session repo (jsonl/repo.ts): JsonlSessionRepo header/session discovery,
  cwd-encoded directory layout `--<path>--/<ts>_<uuid>.jsonl`, v3
  migration, list/open/resume, delete via trash.
- Coding-agent extended messages (packages/coding-agent/src/core/messages.ts:
  BashExecutionMessage, CustomMessage) — AgentMessage custom variants.
- Migration v1 (linear) and v2 (parentId) to v3/v4 (codec.ts handles v3
  linear; the v1/v2/v3 migration lives in jsonl/repo.ts).
