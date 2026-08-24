# Pi in Rust — 1:1 Rewrite Plan

Target: https://github.com/earendil-works/pi (Pi Agent Harness, v0.84.2, commit 5cd93f6)
Goal: Functional 1:1 port to idiomatic Rust. Same CLI surface, same data formats on disk and on the wire, same behavior — different implementation language.

**Conversion progress: 63.86% (106/166 exhaustive ledger tasks complete).** The
percentage is `checked / (checked + open)` over the full
[CONVERSION-LEDGER.md](CONVERSION-LEDGER.md), including its supplemental
source-audit tasks. It is not capped at the original 100-item work queue;
update it whenever the ledger changes. The denominator remains provisional
until the ledger-freezing audit task is complete.

## 0. Governance — standing process rules (operator directive 2026-08-21)

1. **Reassess the plan after every phase.** No phase is "done" until this file
   is updated: phase status, criterion evidence, and issues found. A stale plan
   is a process failure.
2. **Line-by-line expert assessment.** Each phase's code is assessed line by
   line as an expert software engineer would review a landing PR: correctness
   against upstream, error paths, resource/lifecycle handling, and test
   quality — not just "does it compile".
3. **Independent reviewer sign-off gate.** After the plan update, an
   independent expert reviewer (fresh session, not the implementing agent)
   must review the updated plan and the code state and **sign off** before any
   continuation. No code work proceeds past a phase without explicit sign-off.
4. **Evidence tiers, never blurred.** All criterion claims carry a tier:
   `unit` | `mock` | `live`. A claim like "it works" is worthless without the
   tier and the exact command that produced it.
5. **Parity oracles.** Any port of upstream behavior with observable semantics
   (partial-json, SSE, thinking-level clamping, session JSONL) is pinned by a
   golden test generated from the upstream artifact (vendored where possible —
   see §8). Guessing at upstream behavior is a defect, not a shortcut.

## 1. What Pi is

Pi is a self-extensible coding agent monorepo. Nine published packages, ~105k LOC TypeScript source, ~456 test files.

| Package | ~LOC src | Responsibility |
|---|---|---|
| protocol | 1,236 | Strict CBOR codec (RFC 8949 definite-length subset), 4-byte length framing, ClientMessage/ServerMessage codec, TypeBox schemas |
| telemetry | 935 | Vendor-neutral telemetry contracts, in-memory adapter, noop adapter |
| ai | 23,555 | Unified multi-provider LLM API: ~45 providers, model catalogs, OAuth/auth, images API, partial JSON, SSE/WS transports, API adaptors (anthropic-messages, openai-completions/responses, google-generative-ai, bedrock-converse, mistral-conversations) |
| agent | 12,635 | Agent runtime: agent loop, harness (compaction, branch summarization, session JSONL v4, memory, skills), built-in tools (bash/read/write/edit/edit-diff/image), prompt templates, system prompt, events/telemetry |
| client | 1,225 | Protocol client over the server transport |
| server | 2,299 | Server: connections, sessions, snapshots, unix transport, testing harness |
| session-backends | 2,566 | SQLite session backend (+ index shim) |
| tui | 16,772 | Terminal UI library: differential renderer, layout system (VStack/HStack/ScrollView), components (editor, markdown, image, input, select-list, settings-list, loader, box, text), alt-screen, fuzzy, kill-ring, undo stack, keybinding system |
| coding-agent | 59,900 | The `pi` CLI: args, settings/config (global + project), model resolution/registry/catalog, auth storage, bash executor, exec, HTTP dispatcher, session manager (tree, resume), project trust, slash commands, prompts (.pi/prompts), skills loader, extensions, package manager (install/remove/update), compaction, export-html, event bus, footer, usage totals, provider attribution/composer, RPC mode (JSONL over stdio), server integration, migrations, interactive TUI mode, bun packaging |
| evals | 1,277 | Eval harness |

Total src: **104,800 LOC**. Plus runtime data files (models.generated.ts from live provider catalogs).

## 2. Fidelity model — what "1:1" means

Not a line-for-line transpile (TS class/duck-typing to Rust requires different idioms). Fidelity is defined at the *observable contract* level:

1. **CLI surface**: same binary name `pi`, same commands (install/remove/uninstall/update/list/config/auth/run/rpc), same flags (`--provider`, `--model`, `--api-key`, `--system-prompt`, `--mode text|json|rpc`, `--print/-p`, `--continue/-c`, `--resume/-r`, `--session`, `--session-id`, `--fork`, `--session-dir`, `--no-session`, `--name/-n`, `--models`, `--no-tools/-nt`, `--no-builtin-tools/-nbt`, `--tools/-t`, `--exclude-tools/-xt`, `--thinking`, `--extension/-e`, `--no-extensions/-ne`, `--skill`, `--no-skills/-ns`, `--prompt-template`, `--no-prompt-templates/-np`, `--theme`, `--use-theme`, `--no-themes`, `--no-context-files/-nc`, `--export`, `--list-models`, `--verbose`, `--tui-mode`, `--approve/-a`, `--no-approve/-na`, `--offline`, `--help/-h`, `--version/-v`), `@file` argument expansion.
2. **Environment surface**: `PI_MODEL`, `PI_PROVIDER`, `PI_KEY`, `PI_SESSION_ID`, `PI_SESSION_FILE`, `PI_CODING_AGENT_DIR`, `PI_CODING_AGENT_SESSION_DIR`, `PI_OFFLINE`, `PI_REASONING_LEVEL`, `PI_TELEMETRY`, `PI_SKIP_VERSION_CHECK`, `PI_SHARE_VIEWER_URL`, `PI_CACHE_RETENTION`, `PI_TUI_ESC_TIMEOUT`, etc.
3. **On-disk formats**: `~/.pi/agent/settings.json` (global) + `./.pi/settings.json` (project), `~/.pi/agent/sessions/--<path>--/<ts>_<uuid>.jsonl` session files (JSONL v4: header + entries + lane records; v3 auto-migration), `~/.pi/agent/auth.json`, `~/.pi/agent/models.json`, skill/prompt/extension resources under `.pi/`.
4. **Wire/E2E protocols**: RPC mode JSONL over stdio (rpc-types: prompt/steer/follow_up/abort/new_session/get_state/set_model/cycle_model/get_available_models/set_thinking_level/get_session_stats/get_entries/get_tree/get_messages/get_commands/bash + events); server protocol: CBOR + 4-byte length framing, PROTOCOL_VERSION=1, ClientMessage/ServerMessage schemas.
5. **LLM provider behavior**: same providers/APIs (anthropic-messages, openai-completions, openai-responses, google, bedrock-converse, ...), same streaming event semantics (AssistantMessageEventStream: text delta, thinking delta, tool call deltas, usage, stop reasons), same model catalog semantics (glob/fuzzy matching, `provider/model:thinking` patterns).
6. **Tool behavior**: same tool names/inputs/outputs as model-facing (read, write, edit, edit-diff, bash, ls, find, grep, image), same execution semantics (cwd, env, timeouts, output truncation).

**Non-goals**: reproducing npm/bun packaging, TypeBox metaprogramming, JS duck typing, the exact TUI pixel rendering.

## 3. Crate architecture

Cargo workspace at `pi-rust/`. One crate per upstream package, same dependency direction:

```
pi-rust/
  Cargo.toml                 # workspace
  crates/
    pi-protocol/             # packages/protocol      — CBOR, framing, codec, schemas (pure)
    pi-telemetry/            # packages/telemetry     — contracts, memory, noop (pure)
    pi-ai/                   # packages/ai            — providers, catalog, transports, images
    pi-agent/                # packages/agent         — runtime, harness, session-jsonl, tools
    pi-client/               # packages/client        — client over protocol
    pi-server/               # packages/server        — server, unix transport
    pi-session-backends/     # packages/session-backends — sqlite backend
    pi-tui/                  # packages/tui           — terminal UI library
    pi-coding-agent/         # packages/coding-agent  — bin `pi`
    pi-evals/                # packages/evals
  scripts/                   # build/parity helpers
  upstream_pi/               # read-only reference clone (NOT in workspace)
```

Dependencies (crates): protocol ← client/server; telemetry ← all; ai ← agent, server, coding-agent; agent ← coding-agent; tui ← coding-agent; client+protocol ← coding-agent (rpc/stdio); session-backends ← coding-agent optional.

Key Rust dep choices:
- async: `tokio`; HTTP: `reqwest` (rustls) with SSE streaming via `futures-util`; ws for websocket transports later
- serialization: `serde`/`serde_json`; CBOR hand-ported (see pi-protocol) — `ciborium`/`serde_cbor` diverge from the strict subset semantics
- ordered JSON maps: `indexmap` (CBOR map byte-parity and JSON.stringify insertion order)
- TUI: `crossterm` + custom differential renderer port (no ratatui — ratatui semantics differ; port the pi-tui component/layout model instead)
- misc: `uuid`, `base64`, `regex`, `ignore` (gitignore), `dirs`, `thiserror`, `tracing`, `async-trait`, `sha2`/`hex`, `futures`, `tokio-util`, `bytes`, `chrono` (timestamps are unix ms integers; use `std::time` where possible)

## 4. Data model parity (must match byte-for-byte)

### 4.1 Message content blocks (pi-ai types.rs)
```rust
enum ContentBlock {
  Text { text: String },                     // {"type":"text","text":...}
  Thinking { thinking: String, redacted: bool }, // {"type":"thinking",...}
  Image { data: String, mime_type: String }, // {"type":"image","data":base64,...}
  ToolCall { id, name, arguments: JsonValue, /* + optional provider fields */ },
  // ToolResult carries tool_call_id, name, output, is_error, details
}
```
JsonValue mirrors `string|number|boolean|null|JsonValue[]|{...}` — serde_json::Value is 1:1.

### 4.2 Messages
`UserMessage { role, content: String|Vec<Text|Image>, timestamp }`
`AssistantMessage { role, content: Vec<Text|Thinking|ToolCall>, api, provider, model, usage, stop_reason, deferred, error_message, response_id, timestamp }`
`ToolResultMessage { role, content: Vec<Text|Image>, tool_call_id, name, output, is_error, details, timestamp }`
AgentMessage = Message | CustomAgentMessages (hookMessage/custom — role "custom").

StopReason: `pending|stop|length|toolUse|error|aborted|deferred`.
ThinkingLevel: `off|minimal|low|medium|high|xhigh|max`.
Usage: `{ input, output, cache_read, cache_write, reasoning?, total_tokens, cost{input,output,cache_read,cache_write,total} }`.

### 4.3 Session JSONL v4 (agent/src/harness/session/jsonl)
Line 1 header: `{"kind":"header","version":4,"id":...,"createdAt":...,"cwd":...,"parentSessionId"?:...,"metadata"?:...}`
Then one JSON object per line, each an Entry or LaneRecord:
- Entries: `message{type,id,seq,parentId,timestamp,message,terminate?}`, `model_change`, `thinking_level_change`, `active_tools_change`, `compaction{summary,retainedTail,tokensBefore,details?,usage?}`, `branch_summary`, `custom{customType,data?}`
- LaneRecords: `operation_started{id,seq,lane,timestamp,type,intent{kind:run|compaction|navigation,...}}`, `abort_requested`, `operation_finished`, `step_attempt`, `tool_started`, `queue_enqueued`, `queue_cancelled`, `write_deferred`, `usage`
- Storage assigns seq (shared counter), parentId (leaf of appending lane), timestamp.
- v3 files (no header, linear, `hookMessage` role) auto-migrate to v4 on load.
- Session file path: `sessionsRoot/--<cwd with / -> ->--/<unix_ms>_<uuid>.jsonl`; sessionsRoot = `PI_CODING_AGENT_SESSION_DIR` or `~/.pi/agent/sessions/`.

### 4.4 RPC mode (JSONL over stdio) — packages/coding-agent/src/modes/rpc/rpc-types.ts
Commands on stdin: prompt, steer, follow_up, abort, new_session, get_state, set_model, cycle_model, get_available_models, set_thinking_level, cycle_thinking_level, get_available_thinking_levels, set_steering_mode, set_follow_up_mode, compact, set_auto_compaction, set_auto_retry, abort_retry, bash, abort_bash, get_session_stats, export_html, switch_session, fork, clone, get_fork_messages, get_entries, get_tree, get_last_assistant_text, set_session_name, get_messages, get_commands.
Responses/events on stdout incl. `response{id,command,success,error?}`, `event{type:message|tool|...}`, `error`.
Exact event taxonomy ported from rpc-*.ts.

### 4.5 Server protocol
Framing: 4-byte big-endian u32 payload length prefix; max frame 16 MiB. Payload: CBOR (strict subset above). Messages per schemas.ts: hello/handshake with PROTOCOL_VERSION=1, client/server messages with ids, session list/snapshot/update operations.

### 4.6 Settings (settings.json)
Global `~/.pi/agent/settings.json` + project `./.pi/settings.json`, merged (project wins). Keys (schema from settings-manager.ts): compaction{enabled,reserveTokens,keepRecentTokens}, branchSummary, providerRetry, retry{enabled,maxRetries,baseDelayMs,provider}, terminal{showImages,imageWidthCells,clearOnShrink,showTerminalProgress}, image{autoResize,blockImages}, thinkingBudgets, markdown{codeBlockIndent,mermaid}, warning, defaultProjectTrust, transport, model, provider, apiKey, systemPrompt, keybindings, extensions, skills, tools, initialMessage, etc. Port the full interface; unknown keys preserved (forward compat), unknown extension blocks kept.

## 5. Crate module maps

### pi-protocol
- `cbor/`: Value enum (ordered map via indexmap), encode/decode with strict subset semantics: definite lengths only, i53 ints, f64 for non-integers, null/false/true simple values, cycle guards, depth/container/length limits, `skip undefined map values`, no undefined in arrays. Max 16 MiB by default, max depth 64, max container 1M.
- `framing.rs`: encode_frame, FrameDecoder incremental.
- `codec.rs`: ClientMessage/ServerMessage, encode/decode/validate, protocol version check.
- `schemas.rs`: Rust types + serde for every schema in schemas.ts (ThinkingLevel, SessionPhase, ModelRef, ModelCost, ModelMetadata, contents, Usage, transcript items, sessions).

### pi-telemetry
- contracts (traceable spans, events, counters, histograms, gauges), `Memory` adapter, `Noop` adapter, `Telemetry` facade. Ports telemetry/index.ts + memory.ts + noop.ts + testing/conformance.

### pi-ai
- `types.rs`: KnownApi, Api, KnownProvider, ProviderId, ThinkingLevel, ToolChoice, ContentBlock, Message, Usage, StopReason, Model<T>/Provider, ProviderResponse, StreamOptions, ProviderStreams, images types, TextSignature.
- `model_catalog.rs`: models from `models.generated.ts` (data-driven; generate from upstream catalog at build time via scripts/generate-models — first cut: parse upstream `models.generated.ts` into a `models.json` resource; runtime loads `~/.pi/agent/models.json` merged over bundled).
- `models_store.rs`, `models.rs` (createProvider + registry), `provider` registry: all.ts → static dispatch per provider id with per-provider modules (anthropic, openai, google, faux, ...). Each provider: Model init from spec, stream fn implementing SSE/WS/HTTP with api-specific payloads.
- `transports.rs`: SSE stream reader (parse `data:` lines, `[DONE]`), WebSocket, chunked JSON partial parser.
- `partial_json.rs`: tolerant incremental parser mirroring `partial-json` semantics (used for streaming tool-call arguments).
- `oauth.rs`/`auth.rs`, `env_api_keys.rs`, `session_resources.rs`, `images.rs`: port.
- `event_stream.rs`: `AssistantMessageEventStream` — the streaming event type (on_chunk, on_text_delta, on_thinking_delta, on_tool_call_delta, on_usage, completion).
- `api/`: anthropic_messages, openai_completions, openai_responses, google_generative_ai (via genai REST), bedrock_converse, mistral_conversations, cloudflare, github_copilot, azure_openai_responses, google_vertex, pi_messages (server side) + lazy variants.

### pi-agent
- `types.rs`: stream-fn types, ToolExecutionMode, QueueMode, AgentMessage union, AgentState, AgentTool, AgentContext, AgentEvent.
- `agent.rs`/`agent_loop.rs`: run loop with before/after tool hooks, stop conditions, turn lifecycle, events.
- `harness/`: env (FileSystem abstraction — port NodeFs to tokio::fs), events, prompt_templates, reducer (message reduction), session (context, state, memory, jsonl codec + repo, migrations v1→v3→v4), compaction (compaction + branch summary), skills, system_prompt, telemetry, tools (bash/read/write/edit/edit-diff/image with file-mutation-queue + path-utils), search (grep-like, via `ignore`).
- `stream_fn.rs` / `proxy.rs` / `node.ts` equivalents.

### pi-server / pi-client
- server: listener (unix), connection state machine, sessions, snapshots, protocol dispatch, errors; client: connect, send/receive typed messages, snapshot sync.
- transport unix: socket path resolution + preset.

### pi-tui
- terminal backend (crossterm), differential renderer + cell buffer, layout system (VStack/HStack/ScrollView + constrained boxes), components (Text, Box, Image, Input, Editor, Markdown, SelectList, SettingsList, Loader, TruncatedText, Spacer, Stack, CancellableLoader), alt-screen + main-screen modes, fuzzy, keys + keybindings, kill-ring, undo-stack, word-navigation, autocomplete, terminal-image (sixel/iTerm/kitty), latex render subset, stdin-buffer, native-modifiers.

### pi-coding-agent (bin: pi)
- `cli/args.rs`: full flag surface, `@file` args, unknown-flag diagnostics, print_help.
- `config.rs`: APP_NAME/TITLE, VERSION, CONFIG_DIR_NAME, env var names, getAgentDir paths (sessions, auth, settings, models, tools, bin, themes, extensions, prompts, skills), expandTildePath.
- `core/settings.rs` (manager), `core/session_manager.rs` (tree, resume, delete via trash), `core/model_resolver.rs`+`model_registry.rs`+`model_runtime.rs`, `core/auth_storage.rs`, `core/bash_executor.rs`, `core/exec.rs`, `core/http_dispatcher.rs`, `core/project_trust.rs`+`trust_manager.rs`, `core/system_prompt.rs`, `core/prompt_templates.rs`, `core/skills.rs`, `core/extensions/*` (types, loader, runner, wrapper), `core/package_manager.rs`, `core/compaction/*`, `core/export_html/*`, `core/event_bus.rs`, `core/usage_totals.rs`, `core/timings.rs`, `core/provider_attribution.rs`, `core/provider_composer.rs`, `core/messages.rs` (extended messages: BashExecutionMessage, CustomMessage), `core/slash_commands.rs`, `core/keybindings.rs`, `core/footer_data_provider.rs`, `core/session_cwd.rs`, `core/agent_session*` (services/runtime/session), `core/tools/*` (bash, read, write, edit, edit-diff, ls, find, grep, output-accumulator, truncate, file-mutation-queue, tool-definition-wrapper, render-utils).
- `modes/interactive.rs` (TUI mode) + `modes/rpc.rs` (JSONL RPC).
- `main.rs`: parse args → build services → dispatch (interactive | rpc | one-shot print | commands).

## 6. Phased roadmap

P0 — Research & mapping. **DONE** (this doc; source inventory above).
P1 — Workspace + foundations: pi-protocol (CBOR/framing/codec/schemas, tests), pi-telemetry (contracts/memory/noop, tests). Criterion: cbor round-trips match upstream test vectors; frame decoder matches; protocol messages validate.
P2 — pi-ai core: types, model catalog model, transports (SSE), partial-json, faux provider, anthropic+openai providers, event stream. Criterion: faux provider E2E streaming; recorded SSE fixtures decode; partial-json cases.
P3 — pi-agent data + harness core: AgentMessage/AgentState, session JSONL v4 codec + repo (read/write/append/migrate v3), memory, env abstraction, tools (read/write/edit/edit-diff/bash) with mutation queue. Criterion: JSONL round-trip incl. v3 migration; tool tests over tmp dirs.
P4 — pi-coding-agent core: args, config/env, settings manager, session manager, project trust, auth storage, model resolver/runtime, system prompt, slash commands, skills loader, bash executor. Criterion: `pi --version`, `pi --help`, `pi run -p` with faux provider completes an agent loop and writes a session file; settings round-trip.
P5 — RPC mode: full rpc-types JSONL protocol. Criterion: rpc transcript tests match upstream golden transcripts (recorded from real `pi rpc` runs).
P6 — pi-client/pi-server + protocol link. Criterion: client↔server over unix socket; snapshot sync.
P7 — pi-tui: backend + layout + core components (Text/Box/Input/Editor/Markdown/SelectList). Criterion: component snapshot tests; interactive mode usable in tmux.
P8 — coding-agent parity completion: compaction, export-html, extensions, package manager (install/remove/update/list), themes, provider attribution/composer, usage totals, telemetry wiring, migrations, auth commands, list-models, config TUI, update mechanism.
P9 — session-backends sqlite, evals, packaging, parity suite: golden transcripts, session-file fixtures, CLI matrix test against upstream behavior.

## 7. Session ledger

### Session 1 — 2026-08-21 — workspace, foundations, ai-core skeleton
Agent: pi (Claude)   HEAD: no git yet (see Risk R-1)

P0 (this doc) + workspace + P1 foundations + the start of P2 landed. Real
state, not intent:

- pi-protocol — **single suite green (unit, 46 tests)** — CBOR subset codec,
  framing, codec, schemas; `TODO.md` records the only divergences (items that
  cannot occur in Rust), and the conformance source is the upstream protocol
  test suite.
- pi-telemetry — **green (unit, 3 tests)**.
- pi-ai — **P2 core complete (unit, ~2,500 LOC, 24 lib + 2 integration tests,
  all green; 0 warnings)**. The five P2 issues below were root-caused (all
  verified), fixed, and regression-tested in the same session; the P2 sign-off
  gate passed with four additive reviewer conditions, all folded in (ledger
  recount, P2-D isolation wording, oracle repair-path rows, sse finish order).
  Original failure inventory for the record:

| ID | Symptom | Root cause (evidence) | Fix |
|----|---------|----------------------|-----|
| P2-A | `partial_json::tolerates_partial_strings` (expects `{"a": null}` for `{"a`, got `{}`) and `tolerates_partial_keywords_and_numbers` (expects `0` for `-`) | Both the Rust parser **and** its tests diverge from the real upstream contract. Upstream observable behavior is `parseStreamingJson` (JSON.parse → partial-parse → partial-parse(repair) → `{}`); npm `partial-json@0.1.7` verified oracle: `{"a`→`{}`, `tru`→`true`, `{"a": tru`→`{"a":true}`, `-`→`{}`, `12.`→`{}`, `""`→`{}` (`node scripts/oracle_partial_json.mjs`). The current Rust parser returns null for partial keywords and falls back to numbers for `-`/`12.` — neither matches. | Port `parseStreamingJson` + `repairJson` semantics; align the partial parser to the npm oracle; rewrite tests from the golden table. **RESOLVED** — `partial_json.rs` rewritten: `parse_streaming_json` (exact upstream chain incl. `parseJsonWithRepair`), `repair_json` port, Result-based partial parser mirroring npm@0.1.7; tests assert the 28-row golden table.** |
| P2-B | `sse::handles_utf8_split_across_chunks` (split multibyte seq decodes to replacement chars); latent `finish()` event-**reorder** bug (`events.remove(0); events.push(last)` rotates the first pending event to the back — harmless in current tests, wrong on any EOF-with-buffered-data path) | `push_bytes` does `String::from_utf8_lossy` per chunk; an incomplete multibyte sequence at a chunk boundary becomes U+FFFD and corrupts the buffer. | Byte-accumulating buffer; split lines on the `\n` byte; decode complete lines only; on `finish`, decode the remainder (lossy only as a final fallback); **remove the `remove(0)`/re-push rotation in `finish()`**. **RESOLVED** — `sse.rs` rewritten on a byte buffer with line-boundary UTF-8 decode; `finish()` rotation removed; old UTF-8 case + finish-order + EOF-data regressions pass.** |
| P2-C | `model::thinking_level_clamp` (expects Medium→Low, got Medium) | The port diverges from upstream `getSupportedThinkingLevels`/`clampThinkingLevel` (pinned 5cd93f6) in three ways: (1) upstream returns `["off"]` when `!model.reasoning`, the Rust `Model::new` defaults `reasoning=false` and the gate is missing; (2) upstream map semantics: missing keys are supported except `xhigh`/`max` (which require an explicit entry); (3) upstream clamps UP first then DOWN; the Rust port clamps DOWN only. With the test's map `{off,low,high}` upstream gives Medium→Medium, Xhigh→High. The test's Medium→Low matches no upstream code path. | Port the upstream function exactly (reasoning gate + map semantics + up-then-down clamp). Fix the test (set `reasoning=true`; assert Medium→Medium, Xhigh→High). **RESOLVED** — `model.rs` ports upstream `getSupportedThinkingLevels`/`clampThinkingLevel` exactly (reasoning gate, null-key semantics, xhigh/max explicit-entry rule, up-then-down clamp); test corrected to upstream-verified expectations.** |
| P2-D | `faux::usage_estimate_counts_prompt_once` — infinite hang | Root-caused by experiment: `split_by_token_size`'s deterministic RNG uses non-wrapping u64 arithmetic on a **global static counter** → integer overflow at seed 3 (verified: `6364136223846793005*3` overflows u64) → **panic** under debug overflow-checks. The panic happens inside the `tokio::spawn`'d producer task whose `JoinHandle` is `std::mem::forget`'d → swallowed. `collect()` waits on `rx.recv()` while the returned stream still holds a live `UnboundedSender` → never returns. Reproduced: a single 400-char text forces seed≥3 in ONE stream → probe times out IN ISOLATION (verified: `tests/hang_probe.rs` 5s internal timeout fires). The order-dependence is real but scoped to the short recorded test `usage_estimate_counts_prompt_once`: in isolation it completes fast (seed < 3, no panic); it only hangs in the full binary after other faux tests advance the shared static counter past 3. Root cause is the global static + non-wrapping arithmetic; test-order determines *which* test trips, not *whether* one does. | (a) `wrapping_mul`/`wrapping_add` in the LCG; (b) move RNG state off the global static (per-core or thread-local) so tests are order-independent; (c) **close the panic-hang hole — REQUIRED to guarantee stream termination on producer panic**: wrapping the producer body in `catch_unwind` and emitting a terminal `Error` event (or completing the oneshot result); merely dropping the producer's sender is NOT sufficient because the returned stream itself holds a live `UnboundedSender` inside `collect()`, so the channel never closes from the consumer side. Prefer instance-local (per-core) RNG state over thread-local so tests stay order-independent. | **RESOLVED** — per-core `Arc<AtomicU64>` RNG with wrapping LCG (no overflow panic); producer wrapped in `catch_unwind` that downcasts the payload and emits a terminal `Error`; two regressions: long-text bounded termination + panicking factory surfaces as Error (never hangs).** |
| P2-E | 17 compiler warnings in pi-ai | Unused `create_error_stream` import, unnecessary `mut`, etc. | Clean before P2 sign-off (`cargo fix` + manual review). **RESOLVED** — 0 warnings across pi-ai and pi-telemetry (removed unused imports, irrefutable if-lets, no-op drop); `cargo fix` unavailable pre-git so cleanups are manual.** |

### Session 2 — 2026-08-22 — settings manager (P4 criterion) + HOUSEKEEPING FIX
Agent: pi (Claude)   HEAD: 6bf2cf8 → (this session)

- **Repair**: previous session left HEAD ab4f181 unbuildable — `tools/mod.rs`
  references `pi_ai::types::json_tool` which was never committed, and
  Cargo.lock lacked the `base64` dep declared in pi-agent/Cargo.toml.
  Folded both into commit 6bf2cf8; also restored
  `scripts/oracle_partial_json.mjs` which had been truncated to 0 bytes in
  the working tree (parity oracle per §8 must stay regenerable).
- **Settings manager ported 1:1** — `crates/pi-coding-agent/src/core/settings.rs`
  from upstream `settings-manager.ts` (1,347 LOC): deep merge (project wins),
  modified-field tracking with external-key preservation, key-removal
  semantics for `Option` setters, async flush write queue, reload,
  drainErrors with file paths, project trust state machine, lazy `.pi` dir
  creation on write only, migrations (queueMode→steeringMode,
  websockets→transport, skills object→array, retry.maxDelayMs→
  retry.provider.maxRetryDelayMs), PackageSource untagged enum, full
  accessor surface. FileSettingsStorage (`.lock` retry 10x/20ms, released on
  drop) + InMemorySettingsStorage.
- **Tests (TDD, oracle-ported)**: 71 new settings tests — 23 lib unit tests
  (deep_merge/migrate/timeout/strip_bom) + 48 integration tests ported from
  upstream `settings-manager.test.ts` (605 LOC oracle) in
  `tests/settings_sm.rs`, plus regressions for two review findings
  (provider-retry read depth; key-removal persistence).
- **Review findings fixed pre-commit** (port-review stage): (1)
  `get_provider_retry_settings` read `retry.{timeoutMs,maxRetries}` instead
  of `retry.provider.*`; (2) `setShellPath/None` etc. wrote `null` instead of
  removing the key (upstream drops `undefined` in JSON.stringify) — persist
  now removes modified-but-absent fields.
- Workspace: **210 tests passing** (was 142), 0 lib warnings in
  pi-coding-agent; clippy clean for the new module.
- P4 criterion status: `pi --version`/`--help`/`run -p` faux E2E met
  (session 1); **settings round-trip now met** by the module suite. P4
  remaining: model registry/catalog, openai/google providers + auth,
  remaining tools (ls/find/grep/edit-diff/image), project-trust wiring into
  the CLI. P5 (RPC) not started.
- Docs: TODO.md updated; PLAN.md ledger updated (this entry).

### Session 3 — 2026-08-22 — settings wired into the run path (P4 follow-up)
Agent: pi (Claude)   HEAD: 8e52bf8 → (this session)

- `pi -p` now reads settings.json (global + project merge) for
  provider/model defaults: CLI → `PI_PROVIDER`/`PI_MODEL` env → settings →
  `google`/provider default, mirroring upstream `findInitialModel` for the
  one-shot path. Regression caught by the binary-level tests: a settings
  `defaultModel` must NOT leak into an explicitly-selected CLI provider's
  scope (upstream pairs defaultProvider+defaultModel; scoped models win once
  a provider source is explicit) — resolution gate `has_explicit_provider`.
- TDD: 3 binary-level E2E tests spawn the real `pi` binary with a sandboxed
  `$HOME` (global settings default; project-overrides-global; CLI beats
  settings) + 3 resolver unit tests. The 2 settings-dependent tests were red
  before the wiring.
- Cleanup in run.rs: `StreamFn` type alias (3x type_complexity), a redundant
  guard, unwrap_or_default; clippy clean for run.rs. 0 lib warnings.
- Workspace: **219 tests passing**; pi-coding-agent 85.
- P4 status: settings round-trip criterion met at module AND binary level.
  Next: model registry/catalog, openai/google providers + auth, remaining
  harness tools, project-trust CLI wiring.

### Session 4 — 2026-08-22 — harness tools: ls, find, grep + REMOTE PUSH RULE
Agent: pi (Claude)   HEAD: cab9abb → (this session)

- **Process**: operator directive — every local commit must be pushed to the
  remote immediately, every single time; persisted as a global harness prompt
  note. All commits this session pushed in the same step.
- `ls`/`find`/`grep` ported 1:1 (packages/coding-agent/src/core/tools/) into
  `crates/pi-coding-agent/src/core/tools/`. Model-facing text output is the
  contract; TUI theme rendering deferred until pi-tui. `find` spawns `fd`
  with the exact upstream args (`--glob --color=never --hidden
  [--no-require-git] --max-results N [--full-path] -- PATTERN PATH`) and
  `grep` spawns `rg` (`--json --line-number --color=never --hidden
  [--ignore-case] [--fixed-strings] [--glob G] -- PATTERN PATH`) — same
  binaries upstream uses (env has fd 10.4.2 / rg 15.2.0). Notices, relativize
  (trailing '/' preserved), fd full-path `**/` prefixing, representative
  upstream behaviors verified by probes (fd emits absolute paths for
  absolute search paths; rg ignores .gitignore outside git repos).
- TDD: 24 tests (6 ls / 7 find / 11 grep) over temp trees; oracle-derived
  expectations; 3 expectations corrected mid-cycle to match verified
  upstream behavior (dotfiles sort first; rg need .git to honor .gitignore;
  truncate_head_with usize::MAX overflow avoided with finite bound).
- Registered ls/find/grep in `run.rs` (7 built-in tools, --no-tools gate).
- Workspace: **243 tests passing** (was 219); pi-coding-agent 109; 0 lib
  warnings; clippy clean for tools.

### Session 5 — 2026-08-22 — edit tool fidelity: edit-diff + fuzzy matching
Agent: pi (Claude)   HEAD: 03c11e5 → (this session)

- **edit tool upgraded to 1:1 upstream behavior** (agent edit.ts + the entire
  edit-diff.ts machinery): multiple disjoint `edits[{oldText,newText}]`
  matched against the original (not incrementally), exact-then-fuzzy matching
  (NFKC, smart quotes/dashes/spaces + trailing-whitespace normalization),
  duplicate/missing/empty/no-change/overlap errors with exact upstream
  messages, BOM + CRLF/LF preservation, and `details` carrying the display
  diff + unified patch + firstChangedLine. prepareArguments variants
  (edits-as-array / JSON string / single object / legacy top-level
  oldText+newText) and schema/description match upstream. The previous
  naive single-string replaceAll tool is gone.
- New deps: `similar` (line diff; upstream uses npm `diff`) and
  `unicode-normalization` (NFKC for fuzzy normalization).
- TDD: 27 tests (20 edit_diff unit: fuzzy find/normalize/apply/errors/diff/
  patch-apply-back; 7 tool: disjoint edits + file updated, overlap leaves
  file unchanged, missing/duplicate, BOM+CRLF, symlink, fuzzy smart quote,
  prepare-args variants). Two expectations corrected against verified code
  semantics: fuzzy path rewrites touched lines from the normalized base
  (curly quotes become straight *on touched lines only*), and npm-style line
  counts drop the trailing empty split element (patch hunk counts match
  createTwoFilesPatch).
- TDD red-green discipline: seam 1 = pure edit_diff functions (byte-consistent
  offsets), seam 2 = execute_edit over temp files. Mutation-queue env tests
  (blocking writes, concurrency serialization) deferred — they need the env
  abstraction seam, tracked in pi-agent TODO.
- Workspace: **270 tests passing** (was 243); pi-agent 71; clippy clean for
  the new code; 0 lib warnings.

### Session 6 — 2026-08-22 — session search (SessionSearch scanning port)
Agent: pi (Claude)   HEAD: e1cef36 → (this session)

- `ScanningSessionSearch` ported (crates/pi-agent/src/search.rs) from
  packages/agent/src/search/ (scanning.ts + index.ts, 176+32 LOC): scan
  sessions via the Session facade readables (getMetadata/findEntries/
  getLabel), page entries oldest-first (100/page), project each entry as
  JSON.stringify(entry) + label, match case-insensitive substring, emit
  {sessionId, entryId, timestamp, snippet} hits. entryTypes filter, limit,
  duplicate-sessionId guard, abort flag (upstream AbortSignal — sync flag
  until async iteration infra lands).
- Deferred design notes: the upstream lazy source-function form is deferred;
  the JSONL-on-disk case is covered directly through JsonlSessionRepo in
  tests.
- TDD: 5 tests ported from search.test.ts (memory array-source two sessions
  + missing + trim/case, labels in projection, entry-type filter + abort,
  duplicate-session rejection, JSONL sessions on disk via the repo). All
  passed on first implementation run.
- Workspace: **275 tests passing** (was 270); 0 lib warnings; clippy clean.
- P3 data layer now: codec, storage, state, repo, Session facade, search
  all present. Remaining read-side: context.ts and memory.ts backend
  conformance (InMemorySessionStorage/Repo) — the session-backend
  conformance harness is a separate testing piece.

### Session 7 — 2026-08-22 — session facade + in-memory backend + backend conformance (P3 read-side close)
Agent: pi (Claude)   HEAD: c00742f → (this session)

- **P3 read-side + backend conformance landed**, in four pieces, all TDD
  against upstream 5cd93f6:
  1. **`session/messages.rs`** — port of `harness/messages.ts`: full
     `CustomAgentMessage` surface (bashExecution/custom/branchSummary/
     compactionSummary), `bashExecutionToText`, the three message creators,
     `convertToLlm`. Extended `AgentMessage` with a `role()` accessor.
  2. **`session/context.rs`** — port of `context.ts`: `buildSessionContext`
     (messages + derived thinkingLevel/model/activeToolNames), the default
     compaction-boundary transform, caller transforms, custom-type
     projectors, deferred-assistant omission. 4 tests ported from
     `context.test.ts`.
  3. **`session/memory.rs`** — port of `memory.ts`: `InMemorySessionStorage` +
     `InMemorySessionRepo` with Arc<Mutex> sharing so opened sessions
     observe repo state (mirrors upstream shared references).
  4. **Backend conformance harness** — the full 30-case `conformance.ts`
     ported to `tests/conformance.rs` and run against BOTH backends
     (in-memory + JSONL-on-MemoryFs), 60 executions. This was the sharpest
     tool: it surfaced **seven real contract divergences** in the existing
     port, all fixed with regression evidence:
     | ID | Divergence found | Fix |
     |----|-----------------|-----|
     | C-1 | `validateUnusedId`/`validateNewLane` returned InvalidEntry/InvalidLane instead of `already_exists`; `validateTarget` returned InvalidTarget instead of `not_found` | Error codes aligned to upstream (`Session id already exists`, `Lane already exists`, `Entry not found`) |
     | C-2 | `find_entries` cursor applied `seq > afterSeq` for every order; upstream keeps `seq < afterSeq` for newestFirst; no limit/cursor validation | Order-dependent cursor via `matchesEntryQuery`; `invalid_query` validation for limit/cursor |
     | C-3 | `find_entries_on_branch` was a minimal newest-first walk with no order/filters/cursor/bounds/cycle guard and silently empty on a missing start | Full upstream port: walkToRoot with bounds, cycle detection (`invalid_entry`), `not_found` on missing start, order-dependent bound semantics (oldest-first breaks AFTER the bound entry; newest-first stops AT it) |
     | C-4 | `findOpenOperations` returned oldest-first ids with no limit validation; conformance needs full records newest-first | `find_open_operations` returns `OperationStartedRecord`s newest-first with validated limit; enforcement uses an internal `open_operation_ids` |
     | C-5 | `getLog` had no afterSeq/limit and lanes/facts were never pushed to the log | Full `LogItem` union (Entry/Record/Lane/Fact); lane + fact mutations now log like upstream; `LogOptions{afterSeq,limit}` with validation |
     | C-6 | Usage records did not accumulate stats (cached/uncached/total/cost) | Record-mutation stats update in `apply_mutation`, matching the upstream formulas |
     | C-7 | Fork target validation used InvalidTarget/InvalidEntry and the JSONL repo folded `ForkError::Session` into generic Storage, losing the code | `invalid_fork_target` for both missing and non-message targets; repo fork now preserves `ForkError::Session` verbatim |
     Plus: insertion-order lanes (BTreeMap → IndexMap) for `getLanes`/fork-lane byte parity, and `parentId`-exists/cycle validation in `apply_mutation`.
- **Session facade restructured** (`session.rs`): backend enum
  `SessionStorageKind<F>` (Jsonl | InMemory), full upstream SessionTree
  surface: `view(lane)` → `SessionView` (lane-bound append/query),
  `appendMessage`/`appendCustomEntry` → id, `getLeafId`,
  `findEntry`/`findEntryOnBranch` with upstream result-limit=1 propagation,
  `findOpenOperations`, `getLog(options)`, and the `operationKind requires
  type "operation_started"` query guard. `run.rs` switches to the facade
  `append_entry` (drop `Session::storage_mut`).
- **Divergence documented (not fixed in this session):** upstream permits
  *negative* token adjustments in `usage` records (adjustment records with
  input −2 etc.). The pi-ai port types token counts as u64, so negative
  adjustments are unrepresentable; the conformance stats case drops that
  record. Flagged for a future decision (would ripple through pi-ai Usage).
- Workspace: **309 tests passing** (was 275: +30 conformance ×2 backends = 60
  effective cases counted per test fn, +4 context); 0 lib warnings in all
  crates; new files clippy-clean (2 pre-existing state.rs findings remain);
  test-suite warning count 0.
- P3 status: data layer COMPLETE (codec, storage, state, repo, Session
  facade, view, memory backend, context, search, backend conformance).
  Remaining P3-adjacent harness work now: migration v1/v2, compaction +
  branch-summarization, remaining harness env/tools, agent loop.

### Session 8 — 2026-08-22 — compaction + branch-summarization, migration, run-path wiring, P4 auth start
Agent: pi (Claude)   HEAD: 34b539d → (this session; 5 commits: 802c099→a3611d0)

- **pi-ai utils/retry.rs** — `retryAssistantCall` + `isRetryableAssistantError`
  port (bounded exponential backoff, abort normalization, quota/billing
  non-retryable gate, exact upstream pattern sets). 16 tests from retry.test.ts.
- **harness/compaction/** — full port of `packages/agent/src/harness/compaction/`
  (utils.ts + compaction.ts + branch-summarization.ts) onto the session layer.
  includes: file-op extraction/formatting + serializeConversation (2k tool-
  result truncation), token estimation per role, cut-point/turn-start
  selection with split-turn semantics, prepareCompaction (previous-summary
  carry, virtual retained-tail entries, split-turn slicing, prior-compaction
  file-op details), generateSummary[WithUsage] (maxTokens clamp, reasoning
  gate, previous-summary + custom-instruction prompts), completeSimpleWith
  Retries (cacheRetention none + fresh sessionId), compact with turn-prefix
  usage combination, collectEntriesForBranchSummary (newest-first default
  branch walk proven against facade semantics), prepareBranchEntries,
  generateBranchSummary. LLM paths run through a minimal `SimpleModels` seam
  (harness/models.rs) standing in for pi-ai's Models facade (P4
  model-runtime will replace). 53 lib + 20 integration tests ported from
  upstream compaction.test.ts / branch-summarization.test.ts.
- **Migration v1/v2/v3** — ported from packages/coding-agent/src/core/
  session-manager.ts into crates/pi-coding-agent/src/core/session_migration.rs
  (NOT jsonl/repo.ts — corrected the pi-agent TODO's upstream mapping; the
  JSONL codec is v4-only). migrateSessionEntries (v1→id/parentId tree +
  compaction firstKeptEntryIndex→Id; v2→hookMessage→custom; idempotent),
  parseSessionEntries (malformed-line skip), assertValidSessionId. 6 tests
  from migration.test.ts + probes.
- **convertToLlm wired into the run path** — `stream_assistant_response` now
  converts AgentMessages through harness/messages.rs convertToLlm; custom
  messages (bash execution, custom, compaction/branch summaries) reach the
  provider as rendered user messages instead of being dropped;
  excludeFromContext suppression works. 2 lib tests.
- **P4 auth slice** — ported resolve-config-value.ts (!command cached exec,
  $/$$/$! template interpolation, env var classification) and auth-storage.ts
  (file .lock backend with sync 10x/20ms + async exponential backoff in 30s
  stale window; InMemory backend; AuthStorage with revision-batched reads,
  read-modify-write modify resolving configured keys, delete, list;
  ReadOnlyAuthStorage with upstream validation; readStoredCredential;
  getFileRevision parity). 6 + 8 tests. Divergences documented: configured-
  shell command path (default /bin/sh -c used), reload coalescing simplified.
- Reviewer conditions carried: upstream mapping corrections (migration
  location; findEntriesOnBranch default order) written into the code/docs.
- Workspace: **384 tests passing** (was 309); 0 lib warnings in touched
  crates; clippy clean for new modules (1 pre-existing faux.rs type_complexity
  remains).
- P4 status: auth storage/pre-requisites done. Remaining: model registry/
  catalog (models-store, model-resolver, registry over the Models facade),
  openai/google providers + adaptors, project-trust CLI wiring, remaining
  `pi` commands (config/auth/list-models), wiring compaction +
  buildSessionContext into the coding-agent run path. P5 (RPC) not started.
- Docs: PLAN.md updated (this entry); pi-ai/pi-agent/pi-coding-agent TODO.md
  updated. Repo pushed after every commit per Session-4 rule.

### Session 9 — 2026-08-22 — model catalog + Models facade + provider registry (P4 core)
Agent: pi (Claude)   HEAD: 291d8ec → (this session)

- **Model catalog vendored + ported (pi-ai)** — the entire generated catalog
  (39 providers, 1267 models) is now bundled. Upstream gitignores
  `providers/data/*.json` (generated from models.dev), so the vendored source
  is the published `@earendil-works/pi-ai@0.84.2` npm tarball, copied into
  `crates/pi-ai/data/` with `.manifest.json` (generatedAt 2026-08-14).
  `model_catalog.rs` ports model-catalog.ts flatten + models.generated.ts
  (`MODELS` table) + providers/all.ts catalog read side:
  `get_builtin_model/get_builtin_models/get_builtin_providers/
  get_builtin_model_data_generated_at`. `Model` struct gained camelCase serde
  + the `compat` field (anthropic/OpenAI compat overrides, present in the
  catalog). 8 tests.
- **auth.rs (pi-ai)** — port of auth/types.ts + auth/helpers.ts: Credential
  union, AuthContext (env/fileExists), ModelAuth/AuthResult/AuthCheck,
  ApiKeyAuth/OAuthAuth/ProviderAuth traits, CredentialStore trait +
  InMemoryCredentialStore, envApiKeyAuth helper.
- **models.rs (pi-ai)** — the Models facade (models.ts + models-store.ts):
  `Provider` struct with single/by-api stream dispatch (a model whose api has
  no implementation streams the exact upstream "no API implementation"
  error), `create_provider`, `merge_headers` (case-insensitive override),
  `ModelsStore` + `InMemoryModelsStore`, `create_models` with
  setProvider/delete/clear/getProviders/getProvider/getModels/getModel,
  checkAuth/getAvailable/getAuth/applyAuth (auth application with apiKey/
  headers/env/baseUrl override + model-static header merge), and
  stream/complete/streamSimple/completeSimple with lazy auth (auth failures
  terminate the stream with an error event, matching upstream lazyStream).
  9 tests incl. streaming dispatch and auth gating.
- **providers/all.rs (pi-ai)** — all 39 builtin provider factories registered
  with vendored catalogs, upstream baseUrls, and env-key auth. `anthropic` is
  wired to the real anthropic_messages adaptor; the rest stream the upstream
  no-API-implementation error until their api adaptor is ported
  (openai-completions + openai-responses next unlock most providers).
  `builtin_models()` builds the full registry collection. 7 tests.
- **`pi --list-models [search]` (coding-agent)** — args flag + list_models.rs
  port of cli/list-models.ts: auth-gated availability via the facade,
  upstream table columns (provider/model/context/max-out/thinking/images),
  formatTokenCount. Verified live: `pi --list-models` with GEMINI_API_KEY +
  OPENAI_API_KEY + AI_GATEWAY_API_KEY renders the google/openai/vercel
  tables in upstream format. 3 tests.
- Workspace: **411 tests passing** (was 384); pi-ai 80; coding-agent 88;
  0 lib warnings in touched crates; new modules clippy-clean (pi-ai lib has
  0 warnings from new files; the 15 existing clippy findings are all
  pre-session).
- P4 status: model registry/catalog **LANDED at the pi-ai layer**
  (catalog + facade + provider registry + --list-models). Remaining P4:
  openai/google providers + api adaptors, model-config/models-store
  (file-backed models.json merge), model-runtime wiring into the run path,
  project-trust CLI wiring, remaining `pi` commands (config/auth).
  P5 (RPC) not started.
- Docs: PLAN.md updated (this entry); pi-ai + pi-coding-agent TODO.md
  updated. Repo pushed after every commit per Session-4 rule.

### Session 11 — 2026-08-22 — parallel completion: all provider adaptors, TUI surface, coding-agent parity, P9, agent-harness
Agent: pi (Claude) + 6 RLM subagents (A1/A2/B/C/D/E) in isolated worktrees; each branch merged to main after completion. HEAD: e6ce100 → 8c6fa30.

- **pi-ai adaptor completion (A1+A2)**: mistral-conversations (native), openai-codex-responses (SSE),
  bedrock-converse (SigV4 + aws-eventstream), google-vertex (api-key/ADC JWT), cloudflare (workers-ai/
  ai-gateway auth + placeholder base URLs), github-copilot dynamic headers, pi-messages broker,
  openrouter-images + images facade (45-model vendored catalog). All 39 catalog providers now have real
  stream dispatch (previously: anthropic/google/openai/azure/codex real, the rest no-API-implementation).
  ~113 new pi-ai tests (265 total).
- **pi-tui full surface (B)**: fuzzy, kill-ring, undo-stack, word-navigation, terminal-colors,
  native-modifiers, keybindings, stdin-buffer, CombinedAutocompleteProvider, LaTeX (91 parity), SelectList,
  Editor (28 tests), Markdown renderer (22 tests), Image/terminal-image, SettingsList, CancellableLoader,
  alt-screen flash/search. Interactive mode wired: slash registry+dispatch, selectors, footer, streaming
  markdown, tmux-verified E2E. pi-tui 176 lib tests.
- **pi-coding-agent parity (C)**: extensions (loader/runner/wrapper), package manager, CLI commands
  (install/remove/uninstall/update/list/config/auth), event bus, usage totals, provider attribution,
  slash-commands registry, model config/registry/resolver/stores, provider composer. 384 tests incl. 28
  binary-level CLI tests.
- **P9 (D)**: SqliteSessionRepository + storage (30/30 conformance), migrations/sql/facts/writer-leases/
  repository/search suites (85 tests), pi-evals harness + CLI runner (20), scripts/parity-suite.mjs (6/6).
- **pi-agent harness (E + parent)**: events, frontmatter/prompt-templates/system-prompt, skills, reducer
  (12 corruption reasons), image mime utils, file-mutation-queue, result/stream-fn, telemetry schemas,
  ExecutionEnv/StdExecutionEnv, proxy streamProxy, shell-output capture, rich agent loop + Agent class,
  agent-harness scaffold. pi-agent 244 tests.
- **RPC compact divergence closed**: faux registered in the runtime models facade.
- Workspace: **1236 tests passing** (was 529); 0 warnings; clippy-clean for all new files.
- Divergences carried as TODO comments: codex WS transport, OAuth device-code flows, DeferredHandles,
  images retries, several interactive slash commands pending core plumbing,
  models.json runtime merge seam, AWS profile-file chain, vertex ADC scope.

### Session 12 — 2026-08-22 — SessionHandle API + per-session snapshot events (P6)
- ClientConnection/PiClient made Clone (Arc-internal halves); close() now &self.
- New pi-client/src/session_handle.rs: SessionHandle (id, client, attached,
  forwarder, snapshot/event listener slots), SessionLeaseMode (Shared/Exclusive),
  AcquireSessionOptions, subscribe/on_event, prompt/steer/abort/set_model/
  set_thinking/detach/dispose; PiClient::start_session/acquire_session/attach_session.
- Server: ServerSnapshotPublisher::broadcast_session_event (per-session
  ServerEvent::SessionSnapshot fanout after create/attach/prompt/steer/abort/
  set_model/set_thinking via session_snapshot_of) — matches upstream
  Snapshots.publishSessionSnapshot semantics.
- Client notes attach snapshot synchronously so handle.snapshot() is immediately
  correct before the event fanout round-trips.
- E2E: pi-server/tests/session_handle_e2e.rs (lifecycle + subscribe/on_event)
  — both pass. Workspace: 1240 tests (was 1236), 0 warnings. Commits d221714,
  dc32ad9 (resume-picker WIP from TUI-surface left uncommitted on main).

### Session 12 addendum — 2026-08-22 (late) — interactive slash-command completion
Agent: pi (Claude), post-merge integration on main (commit 1df851c). HEAD verified from a clean clone: **1240 tests, 0 warnings**.

Interactive `/` commands now wired end-to-end (tmux-verified where noted): settings, model, thinking,
theme, session, compact, clear, hotkeys, help, quit, export (writes HTML), new, resume (picker +
transcript rehydrate), name, fork (repo.fork), clone, import <jsonl>, reload, trust, copy (clipboard or
banner), login (credential list), logout <provider>, tree (entry-tree banner). A `persisted_until`
watermark in the interactive loop guarantees messages are neither lost nor duplicated across
session-switch operations. Only `/share` remains a banner — it requires the GitHub gist OAuth flow,
which is part of the provider OAuth gap below.

### Session 12 addendum (2) — divergence closure: AWS profile chain, images retry, DeferredHandles
Agent: pi (Claude), on main. Commits 4f9f14d(e)-ae71edd (HEAD).

- Bedrock: shared AWS credentials file chain (`AWS_SHARED_CREDENTIALS_FILE` or
  `~/.aws/credentials`; default/named profiles, session tokens; env keys keep
  precedence) — closes the AWS-profile-file divergence. 4 tests.
- OpenRouter images: full `retryProviderRequest` semantics (retryable statuses
  incl. `x-should-retry`, retry-after-ms/retry-after with 60s server-delay cap,
  exponential backoff 0.5*2^i capped at 8s with jitter; fresh request per
  attempt; transport-error retry) — closes the images-retry divergence. 3 tests
  incl. a 429→200 local-server integration test.
- DeferredHandles: FauxProviderCore.fetch_deferred/cancel_deferred (pending-
  fetches re-emission, cached final resolution, factory steps, unknown/
  cancelled errors), ProviderStreams deferred slots + DeferredFetchOptions,
  Models.facade fetch_deferred/cancel_deferred through lazy auth — closes the
  DeferredHandles divergence. 2 tests.
- Shipped state verified from clean clones: 260 pi-ai lib tests, full workspace
  green across repeated clone runs (known pre-existing env-race flake in the
  azure/cloudflare env-key tests may transiently fail one binary; serialize
  env-mutating tests in a future pass if it recurs).

Remaining documented gaps (all optional/edge surfaces, mostly OAuth/infra
gated): `/share` GitHub-gist OAuth, provider OAuth device-code flows, codex
WebSocket transport (SSE fallback), ConfigSelector full TUI component,
`update --models` pi.dev fetch seam, pi-ai Usage u64 negative-adjustment
decision, TUI alt-screen full swap + ICU word segmentation.

### Session 12 addendum (3) — /share gist + Usage decision
- Interactive `/share` wired (gh auth -> export HTML -> `gh gist create --public=false`
  -> viewer URL; spawn_blocking + timeouts so gh never blocks the UI). E2E verified:
  a real secret gist was created and the viewer URL rendered. With this, ALL
  interactive slash commands are functional (no remaining "not wired" banners).
- Editor autocomplete fix: Enter applies the completion and closes the popup so the
  line submits instead of re-applying the same item.
- Superseded decision: the earlier choice to keep pi-ai Usage token counts as
  u64 was reversed in Session 16. The upstream negative-adjustment case is now
  represented with signed i64 counts and preserved in ledger totals.
- Remaining documented gaps: provider OAuth device-code flows (openai-codex/
  github-copilot/radius), codex WebSocket transport (SSE fallback), ConfigSelector
  full TUI component, and TUI alt-screen full swap.

### Session 13 (planning) — 2026-08-23 — NEXT-100 tracker authored
Agent: pi (Claude)   HEAD: 83e55cb (planning only, no code)

- Authored `NEXT-100.md`: the full 100-micro-task tracker for the remaining
  1:1 conversion (T0 land in-flight /share+--export diff — working tree is RED:
  `cli_export.rs:15` unterminated char literal; T1 close 9 documented
  divergences incl. OAuth device-code + WS transports; T2 AgentTool contract +
  validateToolArguments; T3 coding-agent run-path parity; T4 server/client
  concurrency; T5 TUI completion; T6 remaining core module audit; T7 data
  model/session tree/RPC parity; T8 evals/parity suite; T9 final verification).
- Audit-found gaps not previously tracked: missing CLI flags (--fork, -a/-na,
  -nbt, -e/-ne, --skill/-ns/-np, --theme group, -nc), no auto-compaction in
  run.rs, no --mode json (json-event.ts), image tool not registered (7 vs 8),
  unported core modules (bash-executor, exec, project-trust/trust-manager,
  system-prompt/skills/prompt-templates loaders, http-dispatcher, session-cwd,
  cache-stats, timings, auth-guidance, diagnostics, messages-extended).
- Next session: T0 task 1 (fix cli_export.rs) then land the /share + --export
  diff, then T0.4 finish the real /share flow.

### Session 14 — 2026-08-23 — land in-flight JSON mode + full CLI flag surface (T3 #44–#46)
Agent: pi (Claude)   HEAD: f3ae75f

- **Landed the in-flight working tree**: committed `--mode json` event stream
  (`modes/json_event.rs` from `modes/json-event.ts`, T3 #44) + its binary tests
  (`tests/cli_json_mode.rs`, T3 #45) + test-flake hardening (shared
  `crate::utils::env_lock` for env-mutating ai tests; poisoning-resistant env
  lock; extension-loader + grep test expectation fixes). JSON events reuse the
  RPC `to_json_message_update` envelope (the Rust port of upstream
  `toJsonEvent`). 2 binary tests pass; workspace +8 tests over the last clean
  revision.
- **Closed the CLI flag-surface gap (T3 #46)**: `args.rs` now parses the full
  upstream `args.ts` surface — added `--fork`, `--no-builtin-tools/-nbt`,
  `--extension/-e`, `--no-extensions/-ne`, `--skill`, `--no-skills/-ns`,
  `--prompt-template`, `--no-prompt-templates/-np`, `--theme`, `--use-theme`,
  `--no-themes`, `--no-context-files/-nc`, **plus** `--append-system-prompt`,
  `--models`, `--tui-mode`. Repeatable flags accumulate into Vecs; `--use-theme`
  and `--tui-mode` validate their value token; `--thinking` validation now
  produces an upstream-shaped `Args.diagnostics` entry instead of being silently
  stored. `Diagnostics` carries an Error/Warning kind, and `main.rs` surfaces it
  per upstream main.ts (error → `Error:` + exit 1; warning → `Warning:` +
  continue). 8 new unit tests (20 total in the args module); live-verified the
  error, warning, and clean-parse paths against the built binary.
- **Flag-matrix golden test (T3 #47)**: `tests/cli_flag_matrix.rs` fires the
  full upstream `args.ts` flag surface against the built binary — recognized
  flags produce no "unknown flags" diagnostic, `--help` lists the complete
  surface, error-valued diagnostics (missing `--use-theme` value) exit nonzero
  with an `Error:` line, and invalid `--thinking` warns-but-continues. 5 tests.
- **Scope note**: `--fork`, `-e`, `--skill`, `--prompt-template`, `--theme`
  parsing is done; run-path *honoring* of these flags lands with the T6 loaders
  (#73/#74) and the session-tree fork parity (#83/#88) — not half-wired here.
- **Telemetry gate wired (T3 #48)**: ported `core/telemetry.ts` →
  `core/telemetry.rs` (`isInstallTelemetryEnabled` honoring the `PI_TELEMETRY`
  env override — `1`/`true`/`yes` enable, `0`/`false`/`no` disable, unset defers
  to the `enableInstallTelemetry` setting) and wired it into
  `provider_attribution.rs`, which previously checked only the setting and
  ignored the env. Emits the observable §2.2 env-surface contract.
- **Print-mode output parity (T3 #42/#43)**: `run.rs` now prompts each
  positional message as its own sequential turn (upstream `runPrintMode`
  `for message of messages { session.prompt(message) }`), folds prior turns
  into the agent context, surfaces a terminal Error/Aborted stop-reason to
  stderr with `errorMessage || "Request {stopReason}"` and exits nonzero, and
  joins text content blocks with `\n`. The faux path queues one response per
  prompt so sequential turns work. `tests/cli_print_parity.rs` (2 tests).
  Audit note: `--steer`/`--follow-up`/`--compact` are RPC commands, not
  print-mode flags.
- Workspace: **1330 tests passing** (was 1310); 0 lib warnings; clippy-clean
  for the touched files.
- Docs: NEXT-100.md (#42/#43/#46/#47/#48 done, #44/#45 already done last
  commit), pi-coding-agent/TODO.md updated. **T3 (coding-agent run-path
  parity) is now complete.** Repo pushed after every commit.

### Session 14 addendum — independent reviewer gate (T1 #22)
Agent: independent reviewer session (fresh context, not the implementing agent)

- Ran the PLAN.md §0.3 reviewer gate over the T0+T1/T3 completion increment
  (f3ae75f..5c3f64c). Verdict: **APPROVE WITH CONDITIONS**.
- **C1 (blocking)**: json mode must not exit nonzero / write stderr on a
  streamed model error — upstream `runPrintMode` only turns Error/Aborted into
  a nonzero exit in text mode; json mode emits the error as an event and exits
  0. Fix: removed the `Err("model error: …")` path from json_event.rs and
  corrected the codifying test. **RESOLVED (commit b42050c).**
- **C2 (blocking)**: `--tui-mode` diagnostics diverged — invalid values now
  carry the quoted value (`Invalid TUI mode "bogus". …`), and a flag-like or
  missing value reports `--tui-mode requires regular or fullscreen` without
  swallowing the token. **RESOLVED (b42050c).**
- **C3 (minor)**: main.rs exits inside the diagnostic loop; upstream prints
  all diagnostics first then exits once if any is an error. **RESOLVED
  (b42050c).**
- Non-blocking notes: N1 provider attribution env override has no production
  caller yet (real providers land T6); N3 json events buffered after the loop
  (matches the codebase RPC pattern); N4 json_event.rs duplicates run.rs setup
  (consolidation follow-up). **N2 resolved post-review** — `-v` now maps to
  version per upstream args.ts (was verbose); `--verbose` remains long-form
  only; unit test `short_v_is_version_not_verbose` added.

### Session 15 — 2026-08-23 — T6 audit-driven core wiring, T4 #49–51, T5 word-nav/footer/ConfigSelector
Scope: contiguous work after the Session 14 reviewer gate (commit `1e35f72`),
executed from the NEXT-100 tracker against the T6 verify-then-port recut.

- **T6 remaining core modules** (verify-then-port; most loaders already existed —
  the real deliverable was wiring into the run path):
  - `#71` bash-executor/exec audit — already covered by `pi_agent` bash tool
    (BashCapture + `run_bash`, truncate-tail + `[Showing lines X-Y of N...]`
    messages). Session 25 later closed the live `onUpdate` callback path and
    basic throttled bash progress; S-018 still tracks full harness
    truncation/detail fixture parity.
  - `#72` system-prompt assembly (`--system-prompt` base + skills block +
    `--append-system-prompt`); `#73` skills loader (`--skill`/`-ns`/
    `.pi/skills`/`<agentDir>/skills`/settings key) + `<available_skills>`
    block; `#74` prompt-templates + resource-loader (`/template` expansion)
    + `core/context_files.rs` `<project_context>` injection with `-nc`;
    `#76` session-cwd resume guard (refuses resumed sessions whose stored cwd
    vanished); `#78` auth-guidance messages in list-models; `#79`
    settings-diagnostics + diagnostics ResourceDiagnostic kinds; `#80`
    extended-messages/provider-composer edge audits. Commits
    cb89e93/a1c9675/ee7ef97/84f8693.
- **T4 #49–51** (LiveSessionManager completion, auxiliary subsystem):
  sync adaptation, runtime-event fan-out + terminal-close, subscription
  segment control + dispose-on-idle. Commits 6831f49/71bcce6/fe54def.
- **T5 TUI completion:**
  - `#63/#64` ICU-style word segmentation — each CJK ideograph steps as its
    own word-like segment (`word_navigation.rs`), matching upstream
    `Intl.Segmenter`; editor Ctrl+arrow word nav adopts it. commit 6262abc.
  - `#67/#68` footer token-total reads — `formatTokens` + `render_usage_stats`
    (`↑input ↓output Rcache Wcache CH{rate}% $cost`) wired from transcript
    usage aggregation. commit f3f4d8b.
  - `#59` ConfigSelector — data layer (model + `build_groups`, 5 tests) then
    the `packageManager.resolve()` → `ResolvedPaths` **producer** (on-disk
    collection, pattern filtering, precedence-ranked collision dedup,
    package dedupe, install-on-missing seam; 9 tests), wired into
    `commands/config.rs`. Commits 38f9428 + e8f5b3a. The interactive
    render/`handleInput` component remains deferred (PTY-bound).
- Evidence: full `cargo test --workspace` green at 1405 tests / 0 failures
  (baseline 1240 → 1405 across this working revision); new-code regions of
  `package_manager.rs`, config.rs, model_resolver.rs clippy-clean (`cargo
  clippy -p pi-coding-agent --no-deps`). Docs updated in NEXT-100.md (#59/#60)
  and pi-coding-agent/TODO.md.
- **Addendum (T6 #77):** `core/cache_stats.rs` landed — prompt-cache waste
  accounting (`compute_cache_waste`/`collect_cache_misses`/`detect_cache_miss`
  over session entries, `ModelPriceSource`, TTL/noise-floor/compaction-reset/
  model-change semantics), 7 fixture tests; workspace green at 1412. The
  interactive consumers (cache-miss notices + "Cache Re-billed" stats line,
  gated by the wired `showCacheMissNotices` setting) are PTY-bound; `timings.ts`
  is a deliberate non-port (no Rust startup-timing namespace). Recorded in
  NEXT-100 #77.
- **Addendum (T7 #83/#84):** `modes/rpc.rs` `build_tree` parity fixed vs
  upstream `SessionManager.getTree()` — nodes carry `label?` (via
  `JsonlSession::get_label`), children sorted by entry timestamp, and
  self-parent/orphan entries treated as roots (the prior revision emitted no
  label, left children unsorted, and took stale clone snapshots). 3 tree
  build tests; workspace green at 1415. The interactive entry-tree banner
  remains PTY-bound. Recorded in NEXT-100 #83/#84.
- **Addendum (T7 #85/#86):** export-html parity audited (mermaid/search are
  client-side template features covered by the byte-identical oracle goldens;
  no tmp divergence in the file path; extension tool pre-rendering remains a
  documented no-op seam). The export parity fixture was expanded to cover
  tool-call + thinking blocks, a `compaction` entry, and a `branch_summary`
  entry; goldens regenerated (still byte-identical) + a coverage test. 1 new
  test; workspace green at 1416. Recorded in NEXT-100 #85/#86.

### Session 16 — 2026-08-23 — signed usage and negative-adjustment conformance
Scope: NEXT-100 #81/#82, closing the remaining data-model divergence after the
T7 export fixture work.

- Widened pi-ai `Usage` token fields (`input`, `output`, `cacheRead`,
  `cacheWrite`, `cacheWrite1h`, `reasoning`, `totalTokens`) to signed `i64`.
  Provider usage parsers now preserve signed JSON integers; model cost
  accounting no longer saturates cache-write subtraction, so correction rows
  retain their negative cost deltas.
- Widened `SessionStats` and coding-agent `UsageTotals` token counters to
  signed values. JSONL, in-memory, and SQLite stats accumulation now preserves
  negative adjustment records; context/cache-window derived estimates clamp
  correction-only values to zero because they are not live prompt context.
- Re-enabled the upstream C-neg conformance row in both agent backends:
  input/total `-2`, cost `-0.5`, yielding cached `3`, uncached `10`, total `18`,
  cost `9.5`. Added signed `Usage` JSON round-trip coverage.
- Evidence: `cargo test --workspace` — 1417 tests, 0 failures; `cargo check
  --workspace` — clean. The repository-wide `cargo clippy --workspace
  --all-targets -- -D warnings` remains blocked by pre-existing lint debt in
  pi-ai/pi-protocol/pi-tui outside this change.
- NEXT-100 #81/#82 marked done. The remaining T7 edge work is `pi update` and
  the RPC shape/runtime audits.

### Session 17 — 2026-08-23 — RPC session-query audit
Scope: NEXT-100 #87, the remaining RPC read/query shapes after the tree parity
fixes.

- Audited `get_entries`, `get_tree`, `get_messages`, and
  `get_last_assistant_text` against the upstream RPC mode. RPC session loads
  now read the supplied path directly, restore the active branch context, and
  preserve the loaded session id/name; forked sessions likewise restore their
  context and use the actual new id.
- `get_last_assistant_text` now skips empty aborted messages and trims text;
  `get_fork_messages` reads persisted user entries and returns their real
  entry ids. Added command-level coverage for entries/since/leaf, tree/leaf,
  reloaded messages, last-assistant text, and fork-message ids.
- Evidence: `cargo test -p pi-coding-agent --lib
  modes::rpc::tests::session_queries_use_reloaded_branch_context` — passed;
  workspace test inventory — 1418 tests; `cargo check --workspace` — clean.
- NEXT-100 #87 marked done. RPC mutation/runtime behavior remains #88.

### Session 18 — 2026-08-24 — RPC golden command/event conformance
Scope: S-042, the RPC command/event wire contract and error lifecycle.

- Added deterministic command and event fixtures under
  `crates/pi-coding-agent/tests/fixtures/rpc/`. They cover every RPC command,
  core and session lifecycle event, queue modes, compaction, export,
  switch/fork/clone, malformed input, and failure responses; dynamic ids and
  paths are normalized in the fixture signature.
- Added queue, thinking-level, session-name, compaction lifecycle,
  summarization retry, settlement, and incremental bash execution events, and
  changed dispatcher/task failures into RPC error responses instead of loop
  termination. Updated compaction result types to the upstream shape.
- Evidence: focused RPC module — 37 tests passed; command/event golden tests
  and malformed-line test passed; live `--mode rpc` bash smoke emitted
  `bash_execution_update` before the final response; `cargo test --workspace
  --offline` passed.
- S-042 marked complete. The next image/read audit is recorded in Session 19.

### Session 19 — 2026-08-24 — image/read processing and prompt attachments
Scope: #32, the model-facing image path used by `read` and `@file` prompts.

- Audited the pinned source and corrected the stale “8th image tool” premise:
  upstream `harness/tools/image.ts` is a shared detector/encoder, not a
  separately registered `AgentTool`.
- Added the provider-facing image pipeline: content sniffing, BMP→PNG
  normalization, 2000x2000 and 4.5MB limits, JPEG quality fallback,
  conversion/dimension hints, and graceful omission errors. `read` uses the
  configured auto-resize setting in one-shot, interactive, and RPC modes.
- Implemented `@file` processing for the one-shot path, including tagged text
  references and image content blocks, and applied upstream `blockImages`
  filtering at the provider boundary while preserving the transcript.
- Evidence: `cargo test -p pi-agent --offline tools::image` (6 passed),
  `cargo test -p pi-coding-agent --offline
  run::tests::file_arguments_attach_images_and_tag_text_references`, the
  binary image attachment test in `cli_print_parity`, `cargo check
  --workspace --offline`, and `git diff --check`.
- #32 marked done. Next planned work is one-shot auto-compaction (#33–34 /
  S-025).

### Session 20 — 2026-08-24 — one-shot auto-compaction
Scope: #33–34 and S-025, settings-driven compaction in print mode.

- The one-shot run path now provisions an in-memory session path while turns
  execute, evaluates the existing harness token estimate against model window
  and `compaction` settings, runs the harness summary compactor, rebuilds the
  provider context from the compaction entry plus retained tail, and appends
  the same compaction entry to JSONL persistence.
- Faux summary completions use an isolated scripted queue, so compaction never
  consumes a later normal print response. This gives the binary parity test a
  deterministic provider boundary while real providers reuse the configured
  stream path.
- Evidence (mock): `cargo test -p pi-coding-agent --offline --test
  cli_print_parity` (4 passed), including
  `print_mode_auto_compaction_persists_and_continues`; `cargo test -p
  pi-coding-agent --offline run::tests` (8 passed); `cargo check --workspace
  --offline` and `git diff --check`.
- #33, #34, and S-025 marked done. Next planned work is client
  reconnect/timeouts and the remaining TUI/config-selector interactive gaps.

### Session 21 — 2026-08-24 — client reconnect, timeout, and disposal hardening
Scope: optional/deferred T4 client library hardening (#54 and #56), without
claiming the remaining lease, transport-factory, or full conformance work.

- `pi-client` now has explicit `Disconnected`/`Connecting`/`Connected` state,
  connection-state listeners with unsubscribe handles, reconnect epochs, fresh
  handshake snapshots, and session-handle invalidation after disconnect.
- Handshake and request operations accept deterministic timeout bounds. A
  timed-out request is removed from the pending map and retains a tombstone so
  a late response is ignored rather than misclassified as a protocol error.
- Added permanent `PiClient::dispose()` alongside reconnectable `close()`;
  disposal releases listeners/snapshots and prevents future requests or
  reconnects.
- Evidence (mock): `cargo test -p pi-client --offline` (4 tests, including
  fake-Unix reconnect/lifecycle, handshake timeout, request timeout with late
  response, and disposal); `cargo test -p pi-server --offline`; `cargo check
  --workspace --offline`; `git diff --check`.
- Line-by-line audit against `upstream_pi/packages/client/src/{connection,client,
  state}.ts` leaves #55 lease reconciliation, #57 transport factories, and
  #58 lease-churn E2E open. Supplemental S-045/S-047 remain open because
  reconnect backoff/replay, transport-factory options, and the full upstream
  error/conformance matrix are not implemented.
- #54 and #56 marked done. T4 remains optional/deferred; the next user-facing
  work is the ConfigSelector snapshot/PTY coverage and remaining TUI gaps.

### Session 22 — 2026-08-24 — ConfigSelector interactive behavior
Scope: T5 #59–60 interactive selector behavior and deterministic snapshots;
PTY lifecycle coverage remains separately tracked by S-056.

- Completed the Rust `ConfigSelectorComponent` behavior over the existing
  resolved-resource producer: search input/filtering by name/path/type,
  circular item navigation, page movement, scope switching, global toggles,
  project inherit/load/unload cycling, inherited-resource indicators, and
  resource/package override persistence.
- Settings writes are flushed synchronously after a toggle and on selector
  close, preventing queued settings changes from being lost when `pi config`
  exits.
- Evidence (mock): `cargo test -p pi-coding-agent --offline
  interactive::config_selector` (8 passed, including global/project render
  snapshots); `cargo test -p pi-coding-agent
  --offline` (436 unit tests plus all integration targets); `cargo test -p
  pi-tui --offline` (186 passed). #59 and #60 are marked done; the PTY
  selector exercise is now recorded in S-035 and the full matrix remains in
  S-056.

### Session 23 — 2026-08-24 — ConfigSelector PTY and resize lifecycle
Scope: T5 S-035 ConfigSelector terminal evidence; the full interactive slash-
command matrix remains S-056.

- Added `tests/config_selector_pty.rs`, a deterministic tmux harness around the
  real `pi config --approve` binary. It captures a global render snapshot,
  checks Unicode footer glyphs, resizes the pane, navigates and toggles global
  and project rows, verifies synchronous settings writes, and checks raw
  alternate-screen/cursor entry and cleanup sequences.
- Fixed a real resize lifecycle gap: `pi-tui::Tree` now exposes invalidation,
  and both config and interactive loops invalidate differential render state
  on `TerminalEvent::Resize` before redrawing.
- Evidence (PTY/mock): `cargo test -p pi-coding-agent --offline --test
  config_selector_pty` (1 passed), plus the existing selector snapshot suite.
  S-035 is complete; #61/#62 and S-056 remain open.

### Session 24 — 2026-08-24 — Alt-screen redraw invalidation
Scope: the safe screen-restoration seam inside T5 #61/#62; the full
regular/fullscreen renderer swap remains open.

- `TerminalBackend` now exposes a monotonic screen epoch that changes on
  alternate-screen entry/exit. `pi-tui::Tree` records that epoch and forces a
  complete differential redraw when an overlay or external prompt has replaced
  and restored the active screen.
- Evidence (unit): `cargo test -p pi-tui --offline
  terminal::tests::mode_state_is_idempotent_before_terminal_activation` (1 passed);
  the ConfigSelector tmux PTY test also remains green.
- This closes a concrete stale-frame seam but does not mark #61/#62 complete:
  regular/main-screen rendering, mode switching, and the dedicated tmux swap
  probe still need implementation.

### Session 25 — 2026-08-24 — AgentTool preparation and streamed updates
Scope: T2 #25–27, including the remaining runtime seams behind the already
landed AgentTool shape and schema validator.

- Audited the pinned upstream harness tools and wired `edit`'s
  `prepareArguments` normalization before validation. JSON-string, single-edit
  object, and legacy top-level `oldText`/`newText` calls now reach the same
  validated array shape; the other built-ins have identity/no prepare shims in
  the oracle.
- The rich loop now passes a scoped update callback to every tool execution.
  A channel-backed sink forwards updates while sequential or parallel tools
  are still running, drains queued updates before each end event, and ignores
  callbacks after settlement. Bash emits an initial update, throttled output
  progress, and a final snapshot.
- Terminate hints are retained in `AgentToolResult` and stop a batch only when
  every finalized result opts in; mixed parallel batches continue normally.
- Evidence (unit): `cargo test -p pi-agent --offline
  rich_loop_executes_tool_batch_and_emits_execution_events`,
  `cargo test -p pi-agent --offline
  terminate_hints_require_every_parallel_tool_to_opt_in`, and the two focused
  integration filters `bash_tool_streams_partial_updates_through_agent_contract`
  and `edit_tool_registers_prepare_arguments_before_validation` under
  `cargo test -p pi-agent --offline --test tools`.
- #25–27 are complete. Supplemental S-018 remains open for full harness
  truncation/detail parity and S-020 remains open for the malformed-call
  fixture matrix.

### Session 26 — 2026-08-24 — Bash harness capture integration
Scope: follow-up hardening for T2 #26 / S-018; no new ledger checkbox is
claimed in this checkpoint.

- The registered bash AgentTool now executes through the existing
  `StdExecutionEnv` + `execute_shell_with_capture` seam. Live updates carry
  structured truncation metadata, and large output is persisted to a temp log
  with `fullOutputPath` in the final result/details. The legacy direct
  `run_bash` API remains available for compatibility and lower-level tests.
- Extended the Rust truncation snapshot with the upstream output counts,
  partial-line, first-line, and applied-limit fields so RPC/UI consumers can
  serialize the same detail shape.
- Evidence (unit): `cargo test -p pi-agent --offline --test tools
  bash_tool_preserves_full_output_details_when_truncated`,
  `cargo test -p pi-agent --offline harness::shell_output`, and
  `cargo test -p pi-agent --offline rich_loop_abort_cancels_inflight_bash_tool`.
- S-018 remains open for exact scheduled-timer/write-chain fixtures and the
  broader built-in malformed-call/update matrix; the next RPC audit raises the
  ledger to 51.20%.

### Session 27 — 2026-08-24 — RPC runtime control audit
Scope: T7 #88, closing the remaining direct audit item for auto-compaction,
auto-retry, steering mode, and follow-up mode commands.

- Added a direct RPC runtime test that sends all four control commands and
  verifies live flags, persisted settings, queue modes, and the `get_state`
  wire response. Existing stream/compaction/retry/provider-setting and queue
  drain tests cover the downstream effects.
- Evidence (unit/mock):
  `cargo test -p pi-coding-agent --offline
  rpc_runtime_control_commands_update_settings_and_state`, the focused RPC
  suite, and `cargo test --workspace --offline`.
- #88 is complete. #89–90 and the supplemental eval/fixture tasks remain open.

### Session 28 — 2026-08-24 — update/version and model-catalog parity
Scope: T7 #89–90, closing the visible `pi update` version and model-catalog
fetch contract.

- `pi update` now follows the upstream latest-release plan: three-attempt
  retrying transport checks for transient failures, normalized release metadata,
  semver prerelease/build precedence, current-version/`--force` decisions, and
  a truthful compiled-binary self-update fallback.
- `pi update --models` now refreshes built-in provider catalogs concurrently
  under the upstream 15-second total bound, retries transient HTTP statuses,
  handles 304/404/501 and ETag/Last-Modified persistence, and reports the
  upstream success/error lines. The selected-provider seam keeps this behavior
  mock-testable without contacting every provider.
- Evidence (unit/mock): `cargo test -p pi-coding-agent --offline --lib
  core::version_check::tests`, `cargo test -p pi-coding-agent --offline --lib
  core::remote_catalog_provider::tests`, `cargo test -p pi-coding-agent
  --offline --test cli_commands update_`, `cargo check --workspace --offline`,
  `cargo test --workspace --offline`, and `cargo fmt --all -- --check`.
- #89 and #90 are complete. S-016/S-017 remain open for atomic-write and
  provider-shape/runtime-merge fixture expansion beyond this update command.

### Session 29 — 2026-08-24 — AgentTool harness fixture and lifecycle parity
Scope: supplemental S-018/S-020 closure for built-in update payloads, tool
execution ordering, mutation serialization, and malformed-call behavior.

- Normal `AgentToolResult` text output now omits optional `details`; error text
  retains the upstream empty-object details shape. `read`, `write`, `edit`,
  `bash`, `ls`, `find`, and `grep` are covered through the registered tool
  contract, with abort checks and canonical-path file-mutation serialization.
- The rich loop now runs prepared parallel tools through a completion-aware
  update sink: `tool_execution_end` follows actual completion order while
  model-facing result messages remain in source order. Immediate preparation
  failures are emitted, mutable before-hooks can replace validated arguments,
  after-hooks can override results, and callbacks after settlement are ignored.
- Bash fixtures cover coalesced progress, final truncation/full-output detail,
  and timeout after output. The coding-agent integration fixture invokes all
  seven registered built-ins with malformed arguments and verifies error
  payloads, event coverage, source-order results, and no file mutation.
- Evidence (unit/mock/integration):
  `cargo test -p pi-agent --offline --quiet`,
  `cargo test -p pi-coding-agent --offline --test tool_contract -- --nocapture`,
  `cargo test --workspace --offline --quiet`,
  `cargo fmt --all -- --check`, and `git diff --check`.
- S-018, S-019, S-020, and S-024 are complete. The next open harness/runtime
  work is S-021/S-022/S-023; S-026 and the remaining provider/runtime audits
  continue in their respective ledger sections.

### Session 30 — 2026-08-24 — Panic-safe telemetry settlement
Scope: supplemental S-023 closure for callback unwind settlement and a
deterministic workspace gate.

- The in-memory telemetry adapter now wraps callback admission in an unwind
  boundary. Normal callbacks settle as before; panics settle the current span
  as an automatic error unless an explicit status was recorded, preserve the
  original panic payload by resuming unwinding, and leave late recordings
  inert. Nested callback panics settle inner spans before their parents.
- The TUI image fallback and Kitty capability fixtures now share their module
  lock, preventing concurrent global capability mutation from making the
  workspace gate flaky.
- Evidence (unit):
  `/home/mustbearnold/.cargo/bin/cargo test -p pi-telemetry --offline --quiet`
  (6 passed),
  `/home/mustbearnold/.cargo/bin/cargo test -p pi-agent --offline --quiet`,
  `/home/mustbearnold/.cargo/bin/cargo test -p pi-tui --offline --quiet`
  (186 passed),
  `/home/mustbearnold/.cargo/bin/cargo test --workspace --offline --quiet`,
  `/home/mustbearnold/.cargo/bin/cargo fmt --all -- --check`, and
  `git diff --check`.
- S-023 is complete. The next open harness/runtime work is S-021/S-022;
  S-026 and the remaining provider/runtime audits continue in their ledger
  sections.

### Session 31 — 2026-08-24 — Print-path harness ownership (partial S-021)
Scope: route the one-shot coding-agent path through a configured,
stateful `AgentHarness` while preserving its existing compaction and JSONL
session behavior.

- `AgentHarness` now accepts the provider stream, model, system prompt,
  image policy, and registered tool preparation callbacks; configured harnesses
  own a rich `Agent` plus an in-memory main-lane transcript. Harness-created
  tools preserve `prepareArguments` semantics instead of dropping them at the
  adapter boundary.
- The one-shot `run.rs` path now prompts the harness, reads its chronological
  transcript for compaction decisions, updates the harness Agent after a
  compaction boundary, and replays the harness transcript into the durable
  JSONL session. Agent prompt messages are retained exactly once across turns.
- Evidence (unit/integration): `/home/mustbearnold/.cargo/bin/cargo test
  -p pi-agent --offline --quiet` (175 library tests plus integration targets),
  `/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline
  --quiet` (445 library tests plus integration targets),
  `/home/mustbearnold/.cargo/bin/cargo test --workspace --offline --quiet`,
  `/home/mustbearnold/.cargo/bin/cargo fmt --all -- --check`, and
  `git diff --check`.
- S-021 remains open: interactive, JSONL, and RPC modes still use their
  direct loop paths, and secondary lane operations plus complete harness event
  wiring remain for the next slice. JSON mode now uses a configured stateful
  harness, while S-022 remains open.

### Session 32 — 2026-08-24 — Harness run lifecycle and async span settlement (partial S-022)
Scope: make the integrated harness-owned print run observable with the
upstream run lifecycle and telemetry contract.

- `HarnessTelemetryContext` now has an async span boundary that keeps
  `pi.harness.run` open across provider/session awaits and preserves panic
  settlement semantics. The harness consumes its configured telemetry context
  instead of silently discarding it.
- Configured `AgentHarness::run_prompt` emits ordered `run_start` and
  `run_end` events, records required operation/session/lane attributes, adds
  matching span events, marks failed session writes as telemetry errors, and
  retains the existing durable transcript behavior. The upstream scaffolded
  `events.on` registry remains unavailable; this integration seam is explicit
  and testable.
- Evidence (unit/integration): `/home/mustbearnold/.cargo/bin/cargo test
  -p pi-agent --offline configured_harness_runs_agent_and_persists_lane_messages
  -- --nocapture`, `/home/mustbearnold/.cargo/bin/cargo test -p pi-telemetry
  --offline --quiet` (6 passed), `/home/mustbearnold/.cargo/bin/cargo test
  -p pi-agent --offline --quiet` (175 library tests plus integration targets),
  `/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline
  --test cli_print_parity -- --nocapture`, `/home/mustbearnold/.cargo/bin/cargo
  test --workspace --offline --quiet`, `/home/mustbearnold/.cargo/bin/cargo
  fmt --all -- --check`, and `git diff --check`.
- S-022 remains open: interactive, JSON, JSONL, and RPC adapters still need
  the same complete lifecycle/event bridge and golden wire assertions.

### Session 33 — 2026-08-24 — Shared lifecycle bridge across mode adapters (partial S-022)
Scope: apply the harness lifecycle boundary to the remaining mode-owned agent
loops without changing their established UI or wire payloads.

- A reusable `run_with_harness_lifecycle` adapter now wraps an arbitrary
  mode-owned async loop with ordered run events and a settled
  `pi.harness.run` span. The JSON mode, interactive turn path, detached RPC
  prompt worker, and synchronous RPC prompt path all use the same adapter.
- Existing mode-specific agent events remain unchanged and are emitted inside
  the lifecycle boundary; the adapter exposes the live span for nested
  operation events. A focused golden fixture asserts event order, nested span
  event order, required attributes, and completed outcome.
- Evidence (unit/integration): `/home/mustbearnold/.cargo/bin/cargo test
  -p pi-agent --offline mode_lifecycle_adapter_preserves_event_and_span_order
  -- --nocapture`, `/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent
  --offline modes::rpc::tests --quiet` (39 passed), `/home/mustbearnold/.cargo/bin/cargo
  test -p pi-coding-agent --offline --test cli_print_parity --quiet`,
  `/home/mustbearnold/.cargo/bin/cargo check -p pi-coding-agent --offline`,
  `/home/mustbearnold/.cargo/bin/cargo test --workspace --offline --quiet`
  (176 pi-agent tests, 286 pi-ai tests, 445 pi-coding-agent tests, and 186
  pi-tui tests plus integration targets),
  `/home/mustbearnold/.cargo/bin/cargo fmt --all -- --check`, and
  `git diff --check`.
- S-022 remains open: mode-specific lifecycle events are not yet exposed as
  complete golden JSON/JSONL/RPC envelopes, and interactive/JSON persistence
  and secondary lane telemetry still need end-to-end assertions.

### Session 34 — 2026-08-24 — JSON mode harness ownership (partial S-021/S-022)
Scope: route `--mode json` through the configured stateful `AgentHarness` while
preserving its JSON event contract, including provider terminal errors.

- JSON mode now builds its registered tools as `HarnessTool`s, creates a
  memory-backed main-lane session, configures the provider/model/system prompt
  through `AgentHarnessOptions`, and emits the rich stream updates captured by
  `AgentHarness::run_prompt_with_events`. The harness retains the prompt and
  assistant transcript exactly once for this invocation.
- The rich loop now forwards terminal provider `Error` events as message
  updates. Terminal `Done` remains intentionally omitted from the existing
  RPC golden envelope, preserving successful RPC wire parity while allowing
  JSON mode to reproduce its error-event contract.
- Evidence (unit/integration): `/home/mustbearnold/.cargo/bin/cargo test -p
  pi-agent --offline --quiet` (176 tests plus integration targets),
  `/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline
  --test cli_json_mode --quiet` (2 passed),
  `/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline
  --lib modes::rpc::tests::rpc_command_golden_transcript_matches_fixture`,
  `/home/mustbearnold/.cargo/bin/cargo test --workspace --offline --quiet`,
  `/home/mustbearnold/.cargo/bin/cargo fmt --all`, and `git diff --check`.
- Commits: `dd7a568` (`feat(harness): route json mode through stateful
  agent`) and `3fb8049` (`fix(rich-agent): preserve rpc terminal event
  parity`). Those historical pushes were initially blocked; the accumulated
  branch is now pushed and verified at parity after `gh auth setup-git`.
- S-021/S-022 remain open: interactive, JSONL, RPC full harness ownership,
  mode-specific lifecycle golden envelopes, persistence, and secondary lanes
  still need implementation and evidence.

### Session 35 — 2026-08-24 — Interactive turn harness ownership (partial S-021/S-022)
Scope: route each interactive turn through a configured harness while keeping
the existing TUI transcript, session-switch, and stream-update surfaces.

- Interactive turns now build `HarnessTool`s with the same tool preparation
  callbacks as print/JSON mode, create a memory-backed main-lane harness for
  the invocation, seed its Agent from the current in-memory transcript, and
  feed captured rich stream updates into the existing TUI text callback.
  Session persistence remains owned by the interactive runtime, so resume,
  fork, clone, and `/share` behavior keep their existing ordering.
- A focused regression exercises the real faux stream through `stream_turn`,
  asserting prompt/assistant transcript order, assistant text, and streamed
  deltas. The direct-loop implementation is removed from this turn path;
  RPC/JSONL full harness ownership and secondary lanes remain open.
- Evidence (unit/integration): `/home/mustbearnold/.cargo/bin/cargo test -p
  pi-coding-agent --offline interactive_stream_turn_uses_harness_transcript_and_events
  -- --nocapture` (1 passed), `/home/mustbearnold/.cargo/bin/cargo test -p
  pi-coding-agent --offline --quiet` (446 library tests plus integration
  targets), `/home/mustbearnold/.cargo/bin/cargo check -p pi-coding-agent
  --offline`, `/home/mustbearnold/.cargo/bin/cargo test --workspace --offline
  --quiet` (176 pi-agent, 286 pi-ai, 446 pi-coding-agent, and 186 pi-tui
  tests plus integration/doctest targets), `/home/mustbearnold/.cargo/bin/cargo
  fmt --all`, and `git diff --check`.
- S-021/S-022 remain open: JSONL/RPC full AgentHarness ownership,
  mode-specific lifecycle golden envelopes, persistence, and secondary lanes
  still need implementation and evidence.

### Session 36 — 2026-08-24 — Compiled-binary self-update contract (S-028)
Scope: document and test the supported replacement behavior for the Rust
distribution where the running executable cannot replace itself.

- `pi update --self` retains the upstream latest-release lookup, semver
  decision, `--force` handling, and failure exit code. Once a newer release is
  selected, the Rust command emits one stable replacement instruction naming
  the detected package/version and explicitly directs the user to the package
  manager, wrapper, or source checkout that owns the installation.
- README.md now documents the source-checkout replacement workflow:
  `cargo build --release -p pi-coding-agent`, followed by replacement through
  the installation mechanism. The GitHub repository description source records
  this intentional compiled-binary fallback.
- Evidence (unit/mock):
  `cargo test -p pi-coding-agent --offline
  commands::package::tests::self_update_fallback_instruction_matches_distribution_contract`,
  `cargo test -p pi-coding-agent --offline --test cli_commands update_`,
  `cargo check --workspace --offline`, `cargo test --workspace --offline
  --quiet`, `cargo fmt --all`, and `git diff --check`.
- S-028 is complete with an explicit distribution-level divergence; S-026,
  S-027, and the remaining S-021/S-022 harness ownership work remain open.

### Session 37 — 2026-08-24 — Startup timing compatibility (S-031)
Scope: preserve the upstream `PI_TIMING=1` gate while documenting the Rust
distribution's intentional lack of startup timing namespaces.

- Added `core::timings`, which recognizes only the exact upstream value `1`.
  The binary emits a visible warning for that request and points to
  `/usr/bin/time -p` as the supported process-level timing fallback. It does
  not pretend to provide the upstream per-namespace profiler.
- Evidence (unit):
  `cargo test -p pi-coding-agent --offline
  core::timings::tests::matches_upstream_exact_one_gate_and_fallback_text`.
  `PI_TIMING=1 ./target/debug/pi --version` (mock binary smoke) prints the
  warning before `pi 0.84.2`; the full workspace check/test and documentation
  gate are green before the checkpoint commit (448 coding-agent tests).
- S-031 is complete as an explicit, user-visible non-port. At this point in
  the sequence S-029, session migration integration, and the remaining
  harness ownership work remained open; S-029 is closed in Session 40.

### Session 38 — 2026-08-24 — Legacy session integration (partial S-026)
Scope: connect the existing v1/v2/v3-to-v4 converter to session inventory and
direct switch paths without overstating the still-missing CLI routing.

- Added an atomic `migrate_legacy_session_file` bridge and a session-root
  scanner. Interactive startup and `/resume` refresh the root before listing
  sessions, so legacy files become visible as v4 sessions. RPC startup scans
  the root, and `switch_session` migrates an explicitly supplied legacy path
  before loading it.
- Fork/clone now inherit the converted v4 source when reached through these
  paths. `/import` retains its existing copy-and-convert behavior. Missing
  RPC paths preserve the established error wording.
- Evidence (unit/integration):
  `cargo test -p pi-coding-agent --offline
  core::session_migration::filesystem_migration_tests` (3 passed),
  `cargo test -p pi-coding-agent --offline
  modes::rpc::tests::rpc_load_session_migrates_legacy_v3_file` (1 passed),
  `cargo test -p pi-coding-agent --offline
  modes::rpc::tests::rpc_command_golden_transcript_matches_fixture` (1 passed),
  the interactive harness regression (1 passed), and
  `cargo test --workspace --offline --quiet` (176 pi-agent, 286 pi-ai, 451
  pi-coding-agent, and 186 pi-tui tests plus integration/doctest targets).
- S-026 remains open for CLI `--continue`/`--resume`/`--fork` routing and the
  complete resume/switch/fork/import audit.

### Session 39 — 2026-08-24 — Interactive cache notices and re-billing (S-030)
Scope: connect the already-ported prompt-cache accounting to the interactive
transcript, footer/session usage, settings selector, and context reset paths.

- Added a serialized shadow of interactive session entries because the local
  TUI defers JSONL persistence until exit. Cache misses are re-derived from
  those entries and injected after the matching assistant timestamp, so the
  notices are not persisted and remain correctly placed after compaction.
- Exposed and wired the upstream-off `showCacheMissNotices` setting. The
  notice formatter preserves the upstream 20k-token/$0.10 thresholds, model
  switch/idle labels, and compact token/cost text. `/session` now includes the
  cumulative `Cache Re-billed` tokens/cost/miss-count line.
- Footer usage now reads the shadow entries, preserving assistant,
  tool-result, and compaction/summary usage across post-compaction context
  replacement. Auto-compaction and `/clear` add reset markers; new session,
  resume, and import clear/reload the shadow state.
- Evidence (unit/integration): `cargo test -p pi-coding-agent --offline
  --lib interactive::` (33 passed), `cargo test -p pi-coding-agent --offline
  --quiet` (455 coding-agent unit tests plus integration targets),
  `cargo check --workspace --offline`, `cargo test --workspace --offline
  --quiet`, `cargo fmt --all`, and `git diff --check`.
- S-030 is complete. The remaining S-026 CLI routing audit and
  S-021/S-022 harness ownership work remain open.

### Session 40 — 2026-08-24 — Install telemetry transport and startup gate (S-029)
Scope: finish the upstream interactive install/update ping with explicit
offline, opt-out, retry, timeout, and settings-selector behavior.

- Added `core::telemetry::report_install_telemetry`, which sends the
  `/api/report-install?version=` request with a Rust Pi user-agent. The
  background report uses a five-second overall bound and retries transient
  transport/429/5xx failures without delaying or surfacing errors in the TUI.
- Interactive startup persists the last shipped version and launches the
  report only on a fresh or version-changed install boundary. `PI_OFFLINE`
  short-circuits transport; `PI_TELEMETRY` overrides the default-on
  `enableInstallTelemetry` setting; and `/settings` now exposes the setting.
  `PI_INSTALL_TELEMETRY_URL` is a test-only endpoint seam.
- Evidence (unit/mock): `cargo test -p pi-coding-agent --offline --lib
  core::telemetry::` (7 passed), `cargo test -p pi-coding-agent --offline
  --quiet` (458 coding-agent unit tests plus integration targets),
  `cargo check --workspace --offline`, `cargo test --workspace --offline
  --quiet`, `cargo fmt --all`, and `git diff --check`.
- S-029 is complete. The remaining S-026 CLI routing audit and S-021/S-022
  harness ownership work remain open.

### Session 41 — 2026-08-24 — Complete CLI session routing and legacy import audit (S-026)

Scope: finish the v1/v2/v3-to-v4 integration at every CLI, interactive, RPC,
fork, resume, switch, and import boundary.

- The one-shot path now migrates the configured session root before inventory,
  resolves `--continue`/`--resume`/`--session` by newest, exact, path, or
  unambiguous id prefix, and opens the selected session before constructing the
  stateful harness. `--fork` creates a tree child with parent metadata; new
  prompts append directly to the selected durable file, and the existing
  branch is rebuilt into provider context.
- Interactive and RPC startup now apply the same durable selector behavior.
  Interactive `/import` migrates legacy files into the configured session
  directory and derives metadata from the resulting v4 header; direct RPC
  `switch_session`, startup scans, and fork/clone paths retain the atomic
  migration boundary.
- Added binary regressions for continue, resume, and fork path behavior and
  updated the flag matrix for the now-semantic fork-target diagnostic.
- Evidence (unit/integration): `cargo test -p pi-coding-agent --offline --test
  cli_print_parity --quiet` (7 passed), `cargo test -p pi-coding-agent
  --offline --test cli_flag_matrix --quiet` (5 passed), `cargo test -p
  pi-coding-agent --offline --lib interactive:: --quiet` (33 passed), `cargo
  test -p pi-coding-agent --offline --lib modes::rpc::tests --quiet` (40
  passed), `cargo test --workspace --offline --quiet`, `cargo fmt --all --
  --check`, and `git diff --check`.
- Checkpoint: commit `711a25e`; `git rev-parse HEAD` and
  `git ls-remote origin refs/heads/main` both resolve to
  `711a25e7878c6f403214611651128d90734c09a0`.
- S-026 is complete. S-021/S-022 harness ownership, S-027 extension runtime,
  and the remaining provider/TUI/client/server/evaluation audits remain open.

### Session 42 — 2026-08-24 — Provider auth guidance across mode boundaries (S-032)
Scope: finish the upstream provider-specific no-key/auth guidance at every
user-visible print, JSON, interactive, and RPC error boundary.

- Added a shared auth-failure classifier and formatter that preserves the
  upstream `/login` plus docs message for API-key failures, emits the
  provider-specific re-authentication instruction for OAuth-capable providers,
  and leaves ordinary network/provider errors unchanged. Assistant terminal
  messages are rewritten in place so usage, model, and stop-reason fields are
  retained.
- Wired the formatter through print-mode terminal errors, JSON message-update
  envelopes, interactive turn events/transcripts, and both detached and
  synchronous RPC event paths. Added the RPC wire regression and formatter
  unit cases.
- Evidence (unit/mock): `cargo test -p pi-coding-agent --offline --lib
  core::auth_guidance::tests --quiet` (4 passed), the RPC auth-envelope
  regression (1 passed), `cargo test -p pi-coding-agent --offline --lib
  interactive:: --quiet` (33 passed), `cargo test -p pi-coding-agent
  --offline --lib modes::rpc::tests --quiet` (41 passed), `cargo test -p
  pi-coding-agent --offline --test cli_json_mode --quiet` (2 passed),
  `cargo test -p pi-coding-agent --offline --test cli_print_parity --quiet`
  (7 passed), `cargo check -p pi-coding-agent --offline`, `cargo fmt --all
  -- --check`, and `git diff --check`.
- The full workspace retry was blocked by host resource failures while an
  unrelated OpenHuman release build ran: rustc was SIGKILLed, then rust-lld
  reported SIGBUS, and the isolated retry reached `Disk quota exceeded`.
  Re-run the full workspace gate after the host build/cache pressure clears.
- Checkpoint: commit `50c2103`; `git rev-parse HEAD` and
  `git ls-remote origin refs/heads/main` both resolve to
  `50c2103e3471f302fba5a4a015138af43c9cc400`.
- S-032 is complete. S-021/S-022 harness ownership, S-027 extension runtime,
  and the remaining provider/TUI/client/server/evaluation audits remain open.

### Session 43 — 2026-08-24 — Shared secondary AgentHarness lanes (partial S-021/S-022)
Scope: replace the harness lane/session stubs with shared durable session-tree
views while preserving the existing main-lane print, JSON, and interactive
callers.

- `AgentHarness` now shares its session and lifecycle bus through async-safe
  handles. `lane`, `create_lane`, and `lanes` expose durable main/secondary
  lane metadata, reject invalid/reserved names with stable harness errors, and
  preserve one shared session file/tree.
- Secondary lane views build an independent stateful `Agent` from the same
  model/tools/provider stream, seed it from the selected branch, and implement
  text/message prompts. Prompt deltas append to the selected lane, advance only
  that lane pointer, and return `RunResultValue` outcomes. Run lifecycle events
  and `pi.harness.run` spans are shared and carry the concrete lane name.
- The in-memory regression creates a lane at the main leaf, verifies inherited
  branch context, lane-local persistence/pointer movement, event order, and
  telemetry attributes. `Session::get_metadata` now clones synchronously under
  the in-memory mutex so lane futures remain `Send`.
- Evidence (unit/mock): `/home/mustbearnold/.cargo/bin/cargo test -p
  pi-agent --offline harness::agent_harness::tests::secondary_lane_has_branch_context_and_shared_lifecycle
  -- --nocapture` (1 passed), `/home/mustbearnold/.cargo/bin/cargo test -p
  pi-agent --offline --quiet` (177 library tests plus integration targets),
  `/home/mustbearnold/.cargo/bin/cargo check -p pi-coding-agent --offline`,
  `/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline
  --lib modes::rpc::tests --quiet` (41 passed), `/home/mustbearnold/.cargo/bin/cargo
  test -p pi-coding-agent --offline --lib interactive:: --quiet` (33 passed),
  `/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --test
  cli_print_parity --quiet` (7 passed), `/home/mustbearnold/.cargo/bin/cargo
  fmt --all`, and `git diff --check`.
- This is a partial S-021/S-022 checkpoint, not closure: JSONL/RPC full
  AgentHarness ownership, mode-specific golden envelopes, secondary-lane
  persistence across all adapters, queue/control operations, and complete
  event registry wiring remain open.
- Checkpoint: commit `d8b589f`; `git rev-parse HEAD` and
  `git ls-remote origin refs/heads/main` both resolve to
  `d8b589f3532847042405c2a1a474b0e761c943a7a`.

### Session 44 — 2026-08-24 — ConfigSelector package/path parity (S-034)

Scope: finish the remaining upstream ConfigSelector inheritance and package
pattern audit after the existing search, navigation, persistence, and PTY
coverage.

- Project package overrides now match equivalent local sources resolved from
  different global/project settings bases, write project-relative sources for
  newly created local overrides, and clean empty `autoload: false` override
  objects when cycling back to inherit. Package resource state now preserves
  the upstream distinction between an absent type filter and an explicit empty
  filter.
- Top-level project override matching now includes the resource metadata base
  directory, and inherited items use their absolute path for the force-load /
  force-unload pair. Resource identity uses canonical paths when available,
  matching the upstream inherited-resource map behavior.
- Evidence (unit/mock/live):
  `/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib
  interactive::config_selector --quiet` (11 passed),
  `/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib
  interactive:: --quiet` (36 passed),
  `/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline
  --test config_selector_pty --quiet` (1 passed),
  `/home/mustbearnold/.cargo/bin/cargo check -p pi-coding-agent --offline`,
  `/home/mustbearnold/.cargo/bin/cargo fmt --all`, and `git diff --check`.
- S-034 is complete. S-033 slash-command audits, S-021/S-022 full JSONL/RPC
  harness ownership, and the remaining provider/TUI/client/server/evaluation
  audits remain open.
- Implementation commit `974bd1b` was committed and pushed immediately after
  this entry; the documentation refresh is the follow-up checkpoint.

### Session 45 — 2026-08-24 — Interactive manual compaction slash command (partial S-033)

Scope: replace the interactive `/compact` divergence banner with the existing
agent compaction machinery, while leaving the broader slash-command terminal
matrix open.

- Refactored interactive automatic compaction into a shared helper that can
  either observe the context threshold or force a manual run. `/compact` now
  accepts optional instructions, prepares the current session path, summarizes
  through the configured models facade, persists a compaction entry, replaces
  the in-memory context, resets cache accounting, and reports stable success,
  no-op, or error banners.
- The no-history manual path is covered so an empty session does not invoke a
  provider or mutate the transcript. The existing threshold-triggered
  compaction test exercises the shared persistence/context replacement path.
- Evidence (unit/mock):
  `/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib
  modes::interactive --quiet` (13 passed),
  `/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib
  interactive:: --quiet` (37 passed),
  `/home/mustbearnold/.cargo/bin/cargo check -p pi-coding-agent --offline`,
  `/home/mustbearnold/.cargo/bin/cargo fmt --all -- --check`, and
  `git diff --check`.
- S-033 remains open for the real-terminal/fixture audits of export, import,
  share, trust, login/logout, new/resume, fork/clone, tree, and reload.
- Implementation commit `514cca9` was committed and pushed immediately after
  this entry; the documentation refresh is the follow-up checkpoint.

### Session 46 — 2026-08-24 — Interactive slash-command PTY fixture checkpoint (S-033)

Scope: exercise the real interactive binary through a tmux PTY and complete
the S-033 command-behavior audit while leaving the broader S-056 matrix open.

- Added a fixture-driven transcript for `/help`, `/export`, `/import`,
  `/share`, `/trust`, `/login`, `/logout`, `/name`, `/copy`, `/new`, `/fork`,
  `/clone`, `/tree`, `/reload`, and `/resume`. The fixture seeds a second
  session, selects it with real picker keys, and verifies the rehydrated faux
  transcript; it substitutes temporary paths, verifies the exported HTML,
  checks that trust survives a reload, and asserts alternate-screen/cursor
  cleanup in the raw terminal log.
- The live PTY exposed a first-use deadlock in the terminal image capability
  cache: the read guard was held while detection acquired the write lock. The
  cache now releases the read guard before detection/storage, with a focused
  regression test.
- `/help` derives its command banner from the built-in command registry, so the
  interactive surface cannot silently advertise only the original core subset.
- Evidence (live/unit/check):
  `/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline
  --test interactive_slash_pty --quiet` (1 passed),
  `/home/mustbearnold/.cargo/bin/cargo test -p pi-coding-agent --offline --lib
  interactive:: --quiet` (37 passed),
  `/home/mustbearnold/.cargo/bin/cargo test -p pi-tui --offline
  terminal_image::tests::uncached_capability_detection_releases_read_lock_before_write
  --quiet` (1 passed),
  `/home/mustbearnold/.cargo/bin/cargo check -p pi-coding-agent --offline`,
  `/home/mustbearnold/.cargo/bin/cargo fmt --all -- --check`,
  `git diff --check`, and `node scripts/conversion-progress.mjs` reporting
  `60.24% (100/166; 66 open)`.
- S-033 is complete with live fixture evidence. The broader S-056 terminal
  matrix, including cross-terminal capability and full interaction coverage,
  remains open.
- Implementation commit `3b4d350` was committed and pushed immediately after
  this entry; the documentation-only hash refresh follows it.

### Session 47 — 2026-08-24 — Project-trust safety matrix (S-036)

Scope: align project-trust resolution and persistence across every coding-agent
entry point and verify saved/default/override/prompt behavior against the
upstream trust manager contract.

- Added one shared settings-construction gate for print, JSON, RPC, and
  interactive startup. Config and package commands now use the same precedence
  rather than forcing project trust false or loading project settings by
  default. Explicit `--approve/-a` and `--no-approve/-na` remain highest
  priority; saved directory trust and global `defaultProjectTrust` follow.
- Interactive `ask` now presents the project-trust prompt before raw mode and
  saves the selected decision. Headless modes deny unresolved `ask` decisions.
- Added an exclusive sidecar lock around trust-store reads and writes and
  expanded resource detection coverage for every `.pi` marker plus ancestor
  `.agents/skills`; concurrent decision writes are regression-tested.
- Evidence (live/unit/check):
  `cargo test -p pi-coding-agent --offline --test cli_trust --quiet` (7
  passed), including the live tmux prompt;
  `cargo test -p pi-coding-agent --offline --test cli_commands --quiet` (28
  passed); `cargo test -p pi-coding-agent --offline --lib
  core::project_trust --quiet` (7 passed); `cargo check -p pi-coding-agent
  --offline`; `cargo fmt --all -- --check`; and `git diff --check`.
- S-036 is complete. Remaining trust-adjacent work is limited to the broader
  extension/resource parity and final clean-room audits already tracked by
  S-027, S-065, and S-066.

### Session 48 — 2026-08-24 — Deferred-response runtime and lazy capability parity (S-005/S-006)

Scope: carry deferred fetch/cancel through the shared coding-agent runtime,
provider-composer overlays, and every faux mode registration, then preserve
lazy capability declarations and models-store overrides.

- `ModelRuntime` now owns auth-applied stream, simple-stream, deferred-fetch,
  and deferred-cancel dispatch. The faux provider is registered through the
  same `Models` facade in print, interactive, JSON, and RPC modes; RPC keeps a
  separate summary core so its existing compaction golden remains unchanged.
- Provider composition replaces only the model catalog, retaining provider
  stream/deferred hooks and the shared models store. `api::lazy` loads only
  declared deferred capabilities and preserves the exact upstream diagnostics
  for unsupported fetch and cancellation.
- Evidence (unit/mock/live fixture): deferred runtime (1), provider composer
  (15), mode wiring (1), interactive mode (13), RPC mode (45), lazy API (2),
  full pi-ai unit suite (288), model registry (8), and the real
  `interactive_slash_pty` fixture (1) all pass offline; formatting, diff, and
  progress checks are recorded in `GATES.md`.
- S-005 and S-006 are complete. Next provider parity item is S-007 image
  retry/error classification; broader harness, provider, TUI, client/server,
  evaluation, and final-audit work remains open.
- Checkpoint: commit `56ea6f3`; local `HEAD` and `origin/main` were verified
  equal immediately after the implementation push. This documentation refresh
  follows that push.

### Session 49 — 2026-08-24 — Image retry and cancellation parity (S-007)

Scope: complete the pinned OpenRouter image-generation retry loop and its
abort/quota/error classification rather than leaving the earlier partial retry
implementation as an unverified divergence.

- The image adapter now uses the upstream zero-based exponential retry index,
  parses both numeric and IMF-fixdate `Retry-After`, validates the 60-second
  server-delay cap, and rebuilds each request per attempt.
- `ImagesOptions` carries a shared cancellation flag. Request sending, response
  body reads, and retry backoff all race cancellation and return an aborted
  `AssistantImages` result without issuing another attempt.
- The existing assistant retry classifier is covered alongside the image path:
  quota/billing exhaustion is terminal, while transient provider, transport,
  websocket, stream, and explicit retry-guidance errors remain retryable.
- Evidence (unit/mock): OpenRouter adapter (10), retry submodule (5), shared
  retry classifier (16), image facade (19), full pi-ai library (290), coding-
  agent check, unlazy G21–G25, formatting, diff, and progress gates all pass.
- S-007 is complete. Next provider item is S-008 constrained-sampling/
  grammar support; broader harness, extension, TUI, server/client, evaluation,
  and final-audit work remains open.
- Checkpoint: commit `2b92195`; local `HEAD` and `origin/main` were verified
  equal immediately after the implementation push. This documentation refresh
  follows that push.

### Session 50 — 2026-08-24 — Telemetry async lock cleanup and strict verification

Scope: remove the `pi-telemetry` mutex guard held across an async callback and
restore the crate-level strict clippy baseline before continuing the larger
`pi-ai` warning cleanup.

- `InMemoryChildSpan::start_chapter_async` now snapshots the parent id under
  the lock, releases the guard, and only then awaits callback admission or
  child-span creation. Settled parents still receive the noop span callback.
- Evidence (unit): `cargo test -p pi-telemetry --offline --quiet` (6 passed),
  `cargo clippy -p pi-telemetry --offline --all-targets -- -D warnings`,
  `cargo fmt --all -- --check`, `git diff --check`, and the conversion progress
  checker all pass.
- The full `pi-ai` all-target clippy gate remains open and currently reports
  52 diagnostics; those findings are the next cleanup leaf. The conversion
  ledger remains 62.65% (104/166) because this is verification cleanup, not a
  newly closed parity task.
- Checkpoint: telemetry implementation commit `45e6d64` and the follow-up
  metadata sync `788f9c5` are pushed; local `HEAD` and `origin/main` matched
  after each push. This documentation refresh follows those checkpoints.

### Session 51 — 2026-08-24 — pi-ai adapter strict-clippy cleanup

Scope: remove low-risk strict-clippy findings across the Anthropic, OpenAI,
Azure, Google, faux, partial-JSON, event-stream, and core content helpers.

- Replaced manual defaults with derives, converted `map().flatten()` to
  `and_then`, collapsed single-pattern matches, removed copy-type clones and
  needless borrows, used `clamp`, and removed a no-op response-slot loop.
- Evidence (unit/build): `cargo test -p pi-ai --offline --lib --quiet` (290
  passed), `cargo check -p pi-coding-agent --offline`, `cargo fmt --all --
  --check`, `git diff --check`, and the conversion progress checker pass.
- The strict all-target clippy run now reports 23 diagnostics (down from 52),
  all in the remaining structural/test-placement group. The ledger remains
  62.65% (104/166); this is warning cleanup rather than a new parity task.
- The next leaf covers provider helper signatures, SSE loop shape, faux and
  message enum layout, test-module placement, and test fixture initializers.
- Checkpoint: adapter cleanup commit `8aba4db` is pushed and hash-verified;
  this documentation refresh records that synchronized checkpoint.

### Session 52 — 2026-08-24 — Full pi-ai strict-clippy restoration

Scope: finish the remaining structural and test-target clippy findings after
the adapter cleanup.

- Added a named faux factory type, retained the public unboxed message-step
  API with documented `large_enum_variant` compatibility exceptions, and
  consolidated Anthropic model pricing into one `ModelCost` argument.
- Reordered the stream sink adapter, corrected SSE fixture strings and loops,
  initialized test contexts directly, scoped provider environment locks before
  awaits, and fixed the remaining integration-test lint.
- Evidence (unit/build): `cargo clippy -p pi-ai --offline --all-targets -- -D
  warnings`, `cargo test -p pi-ai --offline --quiet` (290 library, 4 + 8 + 2
  integration tests), `cargo check -p pi-coding-agent --offline`, telemetry
  clippy, formatting, diff, and progress gates all pass.
- G28 is now met. The ledger remains 62.65% (104/166) because this closes a
  verification baseline rather than a new parity task.
- Checkpoint: strict-clippy restoration commit `7b3db53` is pushed and
  hash-verified; this documentation refresh follows that implementation push.

### Session 53 — 2026-08-24 — S-008 constrained sampling and grammar tools

Scope: complete strict JSON-schema and OpenAI grammar custom-tool parity for
all pi-ai adaptors that advertise those capabilities, including request
conversion, message replay, streaming deltas, and exact required-constraint
errors.

- Added `api/constrained_sampling.rs` as the shared strict-schema/grammar
  resolver. It clones schemas before strictification, rewrites optional fields
  as nullable required properties, rejects the upstream unsupported schema
  subset, selects non-empty Lark before regex definitions, infers the one
  required string input property, and emits monotonic JSON deltas for custom
  tool streams.
- Integrated strict and grammar paths into OpenAI Completions, Responses,
  Azure, and Codex; strict schema conversion into Anthropic Messages, Bedrock
  Converse, Google Generative/Vertex, and Mistral. Required unsupported
  schemas now return the upstream diagnostic instead of silently dropping or
  downgrading a tool. Responses-family custom tool events are assembled and
  replayed through `custom_tool_call` / `custom_tool_call_output` wire shapes.
- Added focused schema, exact-diagnostic, grammar-wire, message-replay, and
  SSE fixture tests across the adaptors. Independent reviewer sign-off against
  upstream commit `5cd93f688aaab89dbb6dfa4aca535f21796ae185`: APPROVE, no
  correctness or parity blockers.
- Evidence (unit/mock): `cargo test -p pi-ai --offline --quiet` (307 library,
  4 + 9 + 2 integration tests), `cargo clippy -p pi-ai --offline
  --all-targets -- -D warnings`, `cargo check --workspace --offline`,
  `cargo fmt --all -- --check`, and `git diff --check` pass. The full workspace
  test link attempt was resource-blocked by `ld` receiving SIGKILL 9 during
  the `pi-coding-agent` `export_html_parity` test binary link; the focused
  pi-ai suite is green.
- S-008 is complete. Implementation commit
  `7a72f2fe104cf660f946f29a822c88da556a37d1` was pushed to `origin/main`, and
  local/remote hashes matched immediately after the push. Next
  dependency-safe work is S-009 Codex WebSocket session caching/reuse; S-010
  through S-017 and the broader harness/TUI/server/client/final-audit tasks
  remain open.

### Session 54 — 2026-08-24 — S-009 Codex WebSocket session caching/reuse

Scope: complete the Codex WebSocket session cache, cached-context delta
requests, `websocket-cached` transport behavior, eviction, and close/error
recovery against the upstream `openai-codex-responses.ts` implementation.

- Added process-global session/account WebSocket cache state with busy-entry
  isolation, 5-minute idle eviction, 55-minute max-age eviction, and
  generation-safe timer cleanup. `cacheRetention: "none"` bypasses the cache.
- Added continuation state and request-body delta construction for `auto` and
  `websocket-cached`, preserving full-body behavior for fresh or incompatible
  continuations. Plain `websocket` reuses sockets without enabling the cached
  context delta body, matching upstream transport selection.
- Added cleanup on send/read/parse/map/process/output failures and one retry for
  `previous_response_not_found`; `auto` retains SSE fallback while explicit
  `websocket` returns the WebSocket error. Account-scoped mock fixtures verify
  that sessions do not cross authentication boundaries.
- Independent reviewer bat compared the Rust implementation, tests, and
  `upstream_pi/packages/ai/src/api/openai-codex-responses.ts` line-by-line and
  returned **APPROVE** with no blockers. The only follow-up was correcting a
  stale module comment, now synchronized.
- Evidence (mock/unit): `cargo test -p pi-ai --offline --lib
  api::openai_codex_responses --quiet` (34 passed),
  `cargo test -p pi-ai --offline --quiet` (313 library, 4 + 9 + 2 integration
  tests), `cargo check -p pi-ai --offline`, `cargo clippy -p pi-ai --offline
  --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and
  `git diff --check` pass. The authoritative checker reports exactly
  `Conversion progress: 63.86% (106/166; 60 open)`.
- S-009 is complete. The next dependency-safe action is S-010 AWS
  credential/profile-file and region resolution parity for Bedrock.

### Open (carry-forward)
- P2 phase COMPLETE (evidence above). P3 data layer COMPLETE (Session 7);
  harness compaction + branch-summarization + legacy v1/v2/v3 migration
  LANDED (Session 8); the remaining P3/P4 harness work (image tool,
  file-mutation-queue, tool-context, agent loop, harness env, telemetry
  wiring) continues per §6 without a phase gate (the P3 criterion is met).
- Core coding-agent run path + CLI surface is complete (skills, prompt
  templates, context files, session-cwd, auth-guidance, model registry/
  resolver/runtime, config/auth/list-models commands, RPC/json/jsonl modes,
  `/share`, `--export`). Tracked forward in NEXT-100.md, not here.
- **T5 remaining (tracked in NEXT-100):** full interactive slash-command PTY
  coverage (S-056), alt-screen full swap (#61/62), terminal feature probes
  (#65/66), editor IME edge (#69), and interactive E2E tmux coverage (#70).
- Signed usage adjustments are closed in Session 16: pi-ai `Usage` and session
  ledger stats preserve negative token/cost corrections, with C-neg conformance
  coverage in both backends.
- Governance §0(3): before the next MAJOR phase, a fresh independent
  reviewer session must sign off on this increment.

### Docs
- PLAN.md updated: yes (this revision).
- Repo git-init pending operator confirmation (R-1) — now WAIVED in practice:
  the repo is under git with a standing push-after-commit rule (Session 4).


## 8. Parity oracle & upstream references

- Upstream reference: `upstream_pi/` clone pinned at
  `5cd93f688aaab89dbb6dfa4aca535f21796ae185` (v0.84.2). All parity claims are
  made against this commit, never against memory.
- `scripts/oracle_partial_json.mjs` — runnable oracle for the streaming-JSON
  contract (`parseStreamingJson` chain), with the exact npm `partial-json@0.1.7`
  vendored at `scripts/partial-json-0.1.7/` so oracle runs are network-free and
  reproducible. Golden table (28 rows) regenerates with
  `node scripts/oracle_partial_json.mjs`; P2-A tests assert it directly
  (`oracle_core_cases` + `oracle_repair_path_cases`). **The table must cover the `repairJson` branches**
  (raw control-char escaping, invalid-escape doubling, trailing-backslash doubling) so P2-A
  cannot pass while shipping a broken repair path — cases were added 2026-08-21 per reviewer
  condition 2 (see `scripts/oracle_partial_json.mjs` `cases` list).
- Faux-provider parity reference: `upstream_pi/packages/ai/src/providers/faux.ts`
  (usage-estimation + token chunking semantics; the Rust port must keep the
  deterministic-chunk behavioral contract and never panic/hang).

## 9. Risk register

- R-1 **No version control.** The workspace is not a git repo. For a rewrite
  targeting parity with a moving upstream, this is a process hazard (no
  bisect, no rollback, no blame). Operator decision: `git init` at an agreed
  point (suggest: after P2 sign-off, before P3) and commit per phase.
- R-2 **Debug-only hang vs release masking.** RESOLVED with P2-D (wrapping
  LCG + catch_unwind); the release-masking concern is gone because the
  arithmetic cannot panic by construction and any future panic still
  terminates the stream.
- R-3 **Test ordering dependency.** Global statics shared across tests make
  the suite order-sensitive; P2-D b removes the known instance. Any future
  global state should be flagged in review.
- R-4 **Fidelity drift risk.** The "same CLI surface / same data formats"
  contract is enforced only by the parity oracles + golden transcripts listed
  in §6; every phase must update its oracle set before claiming its criterion.

### Session 10 — 2026-08-22 — google responses/azure adaptors, model runtime, RPC, server+client, TUI core
Agent: pi (Claude)   HEAD: 291d8ec → (this session, 37ca48c)

- **pi-ai provider adaptor completion (P4/P2 follow-up)**: google-generative-ai
  (REST :streamGenerateContent?alt=sse, flattened GenerateContentRequest, SSE
  chunk assembly with text/thinking/tool-call deltas + thought signatures,
  usage/cost, thinking-level config by model family + budget tables,
  streamSimple reasoning resolution). openai-responses (+shared) with the
  full SSE event loop: slots map, partial-streaming JSON, reasoning
  signature persistence + terminal backfill, service-tier pricing, all
  terminal events. azure-openai-responses (deployment + resource config,
  azure host normalization). transform-messages (cross-model thinking/
  redaction/signature/ID rules). Provider registry live-dispatch fixes:
  google → google adaptor; openai + opencode + opencode-go → responses /
  ByApi; vercel-ai-gateway → anthropic (live Vercel 401 proved the wire).
- **Model runtime (P4)**: coding-agent core/model_runtime.rs — upstream
  defaultModelPerProvider table (39 providers), provider/model:thinking
  hint parsing, exact→substring→default→first resolution over the facade.
  run.rs routes real providers through the pi-ai Models facade (catalog +
  applyAuth + lazy stream), terminal assistant errors surface as nonzero
  exits. Live E2E: vercel-ai-gateway request + auth-error parsing; faux
  regressions green.
- **RPC mode (P5 — MILESTONE)**: modes/jsonl.rs (strict LF framing),
  rpc_types.rs (command parse + success/failure builders + camelCase
  RpcSessionState), modes/rpc.rs — full RpcRuntime: prompt/steer/follow_up
  (agent loop + message_update streaming via collect_with_observer +
  agent_settled + JSONL persistence), state/model/thinking/queue-mode and
  bash/session/messages commands; --mode rpc dispatch. Live binary
  round-trips (get_state, prompt events, get_messages, abort).
- **Server + client (P6 — MILESTONE)**: pi-server (UnixSocketListener with
  stale-socket liveness probe + private bind symlink, PiServer handshake/
  dispatch/error mapping, Command execution over PiServerService, snapshot
  publisher with revision + broadcast), pi-client (UnixStream transport,
  hello handshake, request correlation, ServerEvent fanout, snapshot state),
  InMemoryService test service. E2E over a real socket incl. bad-version
  hello_error; codec framing probe.
- **TUI core + interactive mode (P7 core)**: pi-tui crate (crossterm
  terminal backend, differential line renderer + Scene, Component trait,
  flex layout, keys model, Text/Spacer/VStack/HStack/Box/Loader/SelectList/
  ScrollView/TruncatedText/Input with unicode editing). coding-agent
  interactive mode: real-TTY loop (You:/π: transcript, Boxed input bar,
  inline editing, Enter streams the turn live, Ctrl-C exit, JSONL session
  persistence). tmux smoke test verified end-to-end.
- Remaining P7 (not ported): full TUI surface (Editor, Markdown renderer,
  Image, SettingsList, alt-screen overlays, terminal-image, fuzzy), the
  interactive components library, and the interactive mode's full features
  (slash commands, model/thinking selectors, footer). Remaining P8: extensions,
  package manager, export-html, themes, provider attribution/composer,
  usage totals/event bus, config/auth CLI commands, compaction wiring into
  the run/RPC paths, telegram/JSON event modes. P9: session-backends sqlite,
  evals, packaging/parity suite.
- Workspace: **529 tests passing** (was 411); 0 lib warnings; clippy clean
  for new modules.
- Docs: PLAN.md updated (this entry); pi-ai/pi-agent/pi-coding-agent/pi-tui
  TODO.md updated. Repo pushed after every commit.

### Session 11 — 2026-08-22 — parallel completion: all provider adaptors, TUI surface, coding-agent parity, P9, agent-harness
Agent: pi (Claude) + 6 RLM subagents (A1/A2/B/C/D/E) in isolated worktrees; each branch merged to main after completion. HEAD: e6ce100 → 8c6fa30.

- **pi-ai adaptor completion (A1+A2)**: mistral-conversations (native), openai-codex-responses (SSE),
  bedrock-converse (SigV4 + aws-eventstream), google-vertex (api-key/ADC JWT), cloudflare (workers-ai/
  ai-gateway auth + placeholder base URLs), github-copilot dynamic headers, pi-messages broker,
  openrouter-images + images facade (45-model vendored catalog). All 39 catalog providers now have real
  stream dispatch (previously: anthropic/google/openai/azure/codex real, the rest no-API-implementation).
  ~113 new pi-ai tests (265 total).
- **pi-tui full surface (B)**: fuzzy, kill-ring, undo-stack, word-navigation, terminal-colors,
  native-modifiers, keybindings, stdin-buffer, CombinedAutocompleteProvider, LaTeX (91 parity), SelectList,
  Editor (28 tests), Markdown renderer (22 tests), Image/terminal-image, SettingsList, CancellableLoader,
  alt-screen flash/search. Interactive mode wired: slash registry+dispatch, selectors, footer, streaming
  markdown, tmux-verified E2E. pi-tui 176 lib tests.
- **pi-coding-agent parity (C)**: extensions (loader/runner/wrapper), package manager, CLI commands
  (install/remove/uninstall/update/list/config/auth), event bus, usage totals, provider attribution,
  slash-commands registry, model config/registry/resolver/stores, provider composer. 384 tests incl. 28
  binary-level CLI tests.
- **P9 (D)**: SqliteSessionRepository + storage (30/30 conformance), migrations/sql/facts/writer-leases/
  repository/search suites (85 tests), pi-evals harness + CLI runner (20), scripts/parity-suite.mjs (6/6).
- **pi-agent harness (E + parent)**: events, frontmatter/prompt-templates/system-prompt, skills, reducer
  (12 corruption reasons), image mime utils, file-mutation-queue, result/stream-fn, telemetry schemas,
  ExecutionEnv/StdExecutionEnv, proxy streamProxy, shell-output capture, rich agent loop + Agent class,
  agent-harness scaffold. pi-agent 244 tests.
- **RPC compact divergence closed**: faux registered in the runtime models facade.
- Workspace: **1236 tests passing** (was 529); 0 warnings; clippy-clean for all new files.
- Divergences carried as TODO comments: codex WS transport, OAuth device-code flows, DeferredHandles,
  images retries, several interactive slash commands pending core plumbing,
  models.json runtime merge seam, AWS profile-file chain, vertex ADC scope.
