# Pi-rust exhaustive behavioral-parity inventory

Status: audit opened 2026-08-26 (Pacific/Auckland)

This is the acceptance index for the product a user runs as `pi`. The
conversion ledger answers a different question—whether the Rust source/export
scope has been reconciled. It does not prove that every user-visible behavior
works. Every item below remains `AUDIT` until its implementation is inspected,
its negative path is exercised, and the appropriate real runtime evidence is
recorded.

## Evidence rules

Each item needs one or more of these evidence tiers:

- `unit`: deterministic pure behavior, serialization, parsing, or rendering.
- `local`: a real Rust process with loopback HTTP, filesystem, PTY, or a faux
  provider. This proves integration, not a production account or provider.
- `live`: a real provider endpoint/account or real external service. Secrets
  are never recorded in the repository or transcript.
- `release`: the optimized `target/release/pi`, including restart and install
  boundaries where applicable.
- `manual`: a visual or interaction review that cannot be decided by an exit
  code alone. It must name the terminal size, emulator, input sequence, and
  observed result.

An item is not complete because a similarly named Rust module exists, because
one happy-path fixture passes, or because the conversion audit reports 100%.
Every item requires success, failure, cancellation, malformed-input, and
repeat/restart coverage where that state exists.

## A. Product launch, process, and CLI contract

| ID | Capability that must work | Required acceptance |
|---|---|---|
| CLI-001 | `pi` executable name and exit status conventions | Debug and release binaries launch, return `--version`, and use stable non-zero errors. |
| CLI-002 | `--help`, `-h`, `--version`, `-v` | Exact help/version output, stdout/stderr cleanliness, and no startup network are verified. |
| CLI-003 | default interactive mode | No args enters the correct TUI, selects the configured/default provider/model, and restores the terminal on exit. |
| CLI-004 | positional messages | One, multiple, empty, Unicode, whitespace, and leading-dash messages are parsed as upstream. |
| CLI-005 | `@file` message expansion | UTF-8, empty, multiline, missing, unreadable, binary, relative, absolute, and repeated files behave correctly. |
| CLI-006 | `--provider` | Known, unknown, case variation, unavailable, and provider/model conflict diagnostics. |
| CLI-007 | `--model` | Bare model, `provider/model`, glob/fuzzy pattern, thinking suffix, unknown model, and unavailable scoped model. |
| CLI-008 | `--api-key` | Explicit key precedence, empty key, redaction, provider mismatch, and no secret in output/session. |
| CLI-009 | `--system-prompt` | Inline prompt, empty prompt, Unicode, provider request propagation, and conflict with config. |
| CLI-010 | repeated `--append-system-prompt` | Ordering, inline/file content, empty/missing values, and final prompt composition. |
| CLI-011 | `--thinking` | `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, invalid values, provider capability clamping, and request payload. |
| CLI-012 | `--continue`, `-c` | Latest session lookup, no session, wrong cwd, malformed session, and continuation context. |
| CLI-013 | `--resume`, `-r` | Interactive picker, exact path/id, partial id, missing id, deleted file, cancel, and selected-session restoration. |
| CLI-014 | `--session` | Explicit path, id, relative path, read-only path, missing parent, and append behavior. |
| CLI-015 | `--session-id` | New exact ID creation, existing exact ID, invalid ID, and readonly/creation semantics. |
| CLI-016 | `--fork` | Fork by path/id, parent linkage, message selection, missing target, and independent writes. |
| CLI-017 | `--session-dir` | Override precedence, isolation, path creation, symlink, and restart discovery. |
| CLI-018 | `--no-session` | No session file, no accidental directory write, while runtime still completes. |
| CLI-019 | `--name`, `-n` | Startup name, rename normalization, Unicode/newline handling, persistence, and footer/picker display. |
| CLI-020 | `--models` | Multiple catalogs, ordering, duplicate providers, invalid catalog, and merge behavior. |
| CLI-021 | `--tools`, `-t` | Allowlist parsing, whitespace, duplicates, unknown tools, extension tools, and tool-call rejection. |
| CLI-022 | `--exclude-tools`, `-xt` | Exclusion precedence, built-in/extension tools, unknown tools, and next-turn refresh. |
| CLI-023 | `--no-tools`, `-nt` | No built-ins or extension tools, prompt/model payload, and explicit allowlist interaction. |
| CLI-024 | `--no-builtin-tools`, `-nbt` | Extension tools remain available and built-ins are absent. |
| CLI-025 | `--print`, `-p` | One prompt, multiple argv prompts, stdin, empty stdin, mixed argv/stdin, output and exit status. |
| CLI-026 | `--mode text` | Text stream, errors, usage, provider attribution, and stdout cleanliness. |
| CLI-027 | `--mode json` | JSON event stream, one JSON object per line, ordering, errors, usage, and no ANSI. |
| CLI-028 | `--mode rpc` | JSONL command/event protocol, long-lived process, and clean shutdown. |
| CLI-029 | `--export` | HTML default, explicit `.html`, `.jsonl`, invalid suffix, overwrite, missing session, and XSS safety. |
| CLI-030 | `--extension`, `-e` | Repeated sources, native Rust factories, missing/invalid paths, and startup failure boundaries. |
| CLI-031 | `--no-extensions`, `-ne` | Built-ins/explicit extensions precedence and no extension side effects. |
| CLI-032 | `--skill` / `--no-skills`, `-ns` | Explicit skills, disabled discovery, precedence, invalid skills, and prompt inclusion. |
| CLI-033 | `--prompt-template` / `--no-prompt-templates`, `-np` | Explicit templates, disabled discovery, variables, invalid files, and selection. |
| CLI-034 | `--theme`, `--use-theme`, `--no-themes` | Theme sources, named selection, invalid colors, live reload, and disabled discovery. |
| CLI-035 | `--no-context-files`, `-nc` | AGENTS/CLAUDE/context discovery suppression and prompt differences. |
| CLI-036 | `--list-models` | All models, search pattern, unknown pattern, catalog refresh, offline behavior, and formatting. |
| CLI-037 | `--offline` | No network startup or provider/catalog request, deterministic diagnostics, and fixture operation. |
| CLI-038 | `--tui-mode regular/fullscreen` | Both modes, invalid value, terminal resize, and alt-screen boundaries. |
| CLI-039 | `--verbose` | Startup diagnostics only when requested, no secret leakage, and stable stderr/stdout split. |
| CLI-040 | `--approve`, `-a`, `--no-approve`, `-na` | Trust override precedence and first-run behavior. |
| CLI-041 | unknown long flags | Extension flag forwarding, `--key=value`, boolean/value parsing, and unknown diagnostics. |
| CLI-042 | unknown short flags | Exact diagnostic and no accidental prompt submission. |
| CLI-043 | missing option values | Every value-taking flag reports the correct error without panic or hang. |
| CLI-044 | signal/process behavior | SIGINT, SIGTERM, EOF, broken pipe, child failure, and terminal cleanup. |

## B. Environment, paths, settings, and resources

| ID | Capability that must work | Required acceptance |
|---|---|---|
| ENV-001 | `PI_MODEL` / `PI_PROVIDER` | Environment defaults, CLI/config precedence, invalid values, and footer/request selection. |
| ENV-002 | `PI_KEY` / provider key variables | Exact provider env precedence and secret redaction. |
| ENV-003 | `PI_SESSION_ID` / `PI_SESSION_FILE` | Session selection precedence, invalid paths, and restart behavior. |
| ENV-004 | `PI_CODING_AGENT_DIR` | Global root override and every child path. |
| ENV-005 | `PI_CODING_AGENT_SESSION_DIR` | Session root override and discovery isolation. |
| ENV-006 | `PI_OFFLINE` | Environment equivalent of `--offline`. |
| ENV-007 | `PI_REASONING_LEVEL` | Environment equivalent of thinking selection. |
| ENV-008 | `PI_TELEMETRY`, `PI_TIMING`, `PI_STARTUP_BENCHMARK` | Enable/disable behavior and output cleanliness. |
| ENV-009 | `PI_SKIP_VERSION_CHECK`, `PI_VERSION` | Startup/update check suppression and version override behavior. |
| ENV-010 | `PI_SHARE_VIEWER_URL` | Share URL configuration and invalid URL handling. |
| ENV-011 | `PI_CACHE_RETENTION` | Cache retention modes and provider request semantics. |
| ENV-012 | `PI_TUI_ESC_TIMEOUT`, cursor, shrink, and terminal probes | Exact environment-driven TUI behavior. |
| ENV-013 | proxy variables | HTTP/HTTPS proxy resolution, explicit dispatcher override, auth, and failure. |
| ENV-014 | `HOME`, `USERPROFILE`, XDG roots | Platform path resolution and missing-home fallback. |
| ENV-015 | `EDITOR` / `VISUAL` | External editor precedence, launch, input, cancellation, and failure. |
| CFG-001 | global `settings.json` | Read, write, defaults, unknown-key retention, malformed JSON, permissions, and atomicity. |
| CFG-002 | project `.pi/settings.json` | Project-over-global merge, cwd changes, malformed file, and trust boundary. |
| CFG-003 | settings merge schema | Every nested setting: compaction, branch summary, retry, terminal, image, markdown, warning, model/provider/key, system prompt, keybindings, tools, resource paths, and extension blocks. |
| CFG-004 | settings migration | Legacy keys, diagnostics, backup/repair, and no silent data loss. |
| CFG-005 | live settings reload | `/reload` and selector changes update the active runtime without duplicate subscriptions. |
| RES-001 | resource discovery | User/project/package resources for extensions, skills, prompts, themes, and context files. |
| RES-002 | resource precedence | Project, user, package, built-in, collision, disable/enable, and stable ordering. |
| RES-003 | invalid resource handling | Invalid frontmatter, UTF-8, missing descriptions, name mismatch, duplicate names, and warning behavior. |
| RES-004 | native Rust extension boundary | Factories for command, hook, renderer, tool, flag, provider, editor, and UI resources. |
| RES-005 | unsupported JS/TS sources | Deterministic rejection/ignore path with no runtime or Node/Bun dependency. |
| RES-006 | package resource roots | Installed package, local package, symlink, and missing package behavior. |

## C. Authentication, models, and provider runtime

| ID | Capability that must work | Required acceptance |
|---|---|---|
| AUTH-001 | API-key environment auth | Every provider's declared env names, precedence, and missing-key message. |
| AUTH-002 | explicit API key | CLI/config/auth-file precedence and no token in logs/session/errors. |
| AUTH-003 | auth.json schema | API-key and OAuth records, extra fields, unknown fields, malformed records, permissions, and atomic writes. |
| AUTH-004 | auth.json locking | Concurrent login/refresh/logout/read operations preserve all providers and never truncate. |
| AUTH-005 | `/login` provider selection | Provider ID/name, search/filter, configured status, auth method selection, cancel, and keyboard navigation. |
| AUTH-006 | `/login` API-key flow | Secret input masking, empty key, persistence, live model use, and error recovery. |
| AUTH-007 | `/login openai-codex` browser OAuth | PKCE, state, production authorize URL, browser launch/fallback, localhost callback, token exchange, account extraction, persistence. |
| AUTH-008 | Codex manual redirect/code | Full URL, `code#state`, query string, raw code, malformed input, wrong state, wrong route, and cancellation. |
| AUTH-009 | Codex device-code OAuth | User code display, verification URL, browser fallback, pending/slow-down polling, expiry, cancel, token exchange, persistence. |
| AUTH-010 | OAuth refresh | Near-expiry refresh, refresh failure, missing account, malformed JWT, concurrent refresh, and retry once semantics. |
| AUTH-011 | `/logout` | No credentials, selector, provider ID/name, credential type, cancel, deletion, runtime availability update, and restart. |
| AUTH-012 | auth failures | Unauthorized, expired, malformed, network, rate-limit, provider-specific errors and actionable `/login` guidance. |
| MODEL-001 | bundled catalog | Every bundled provider/model has valid API, URL, context, costs, modalities, reasoning, and dates. |
| MODEL-002 | models.json overlay | Add/modify/delete/unknown-field retention, base URL/headers/auth overrides, hot reload, and invalid config. |
| MODEL-003 | runtime model registry | File-backed auth/models stores are live sources; changes affect subsequent turns without restart. |
| MODEL-004 | model resolver | Exact, provider-scoped, fuzzy, glob, case, thinking suffix, unavailable, and ambiguous selection. |
| MODEL-005 | model selector | Search, selection, configured state, provider grouping, empty state, cancel, and keyboard navigation. |
| MODEL-006 | provider availability | Auth and model availability refresh, scoped model ordering, and stale-cache invalidation. |
| MODEL-007 | retries | Retry enablement, max retries, backoff, provider setting, retryable status/body, and abort during wait. |
| MODEL-008 | cache affinity/retention | Request flags, cache key, cache read/write usage, retention opt-out, and session affinity. |
| MODEL-009 | cross-provider handoff | Message transformation, unsupported blocks, tool-call IDs, images, thinking, and errors. |

Provider rows are independently required, not covered by a generic “provider
matrix” claim:

`amazon-bedrock`, `ant-ling`, `anthropic`, `azure-openai-responses`, `baseten`,
`cerebras`, `cloudflare-ai-gateway`, `cloudflare-workers-ai`, `deepseek`,
`fireworks`, `github-copilot`, `google`, `google-vertex`, `groq`, `huggingface`,
`kimi-coding`, `minimax`, `minimax-cn`, `mistral`, `moonshotai`,
`moonshotai-cn`, `nvidia`, `openai`, `openai-codex`, `opencode`, `opencode-go`,
`openrouter`, `qwen-token-plan`, `qwen-token-plan-cn`,
`qwen-token-plan-individual`, `together`, `vercel-ai-gateway`, `xai`, `xiaomi`,
`xiaomi-token-plan-ams`, `xiaomi-token-plan-cn`, `xiaomi-token-plan-sgp`,
`zai`, and `zai-coding-cn`.

For each provider, the runtime must prove: base URL and headers, auth source,
request schema, streaming event translation, text/thinking/tool/image blocks,
stop reasons, usage/cost, response IDs, malformed/empty events, HTTP status and
body errors, retry, abort, timeout, Unicode, and model-specific reasoning or
sampling options. Provider-specific OAuth, AWS credentials/regions, Copilot
device/auth, Cloudflare bindings, and catalog refresh are separate cases.

The provider contract is tracked as separate acceptance items so a green result
for one adaptor cannot hide a missing provider registration or credential path:

| ID | Provider acceptance item |
|---|---|
| PROV-001 | `amazon-bedrock`: AWS credential/profile/region resolution, SigV4, eventstream, abort, and errors. |
| PROV-002 | `ant-ling`: OpenAI-compatible URL, auth, payload, stream, and model-specific options. |
| PROV-003 | `anthropic`: API-key auth, Messages payload, thinking/images/tools, stream, and errors. |
| PROV-004 | `azure-openai-responses`: endpoint/deployment/api-version resolution and Responses stream. |
| PROV-005 | `baseten`: chat-template arguments, auth, payload, stream, and failure paths. |
| PROV-006 | `cerebras`: OpenAI-compatible auth, reasoning mapping, stream, and errors. |
| PROV-007 | `cloudflare-ai-gateway`: gateway credentials, route translation, and stream/error paths. |
| PROV-008 | `cloudflare-workers-ai`: account/key resolution, model route, stream, and errors. |
| PROV-009 | `deepseek`: OpenAI-compatible reasoning/content replay and stream/error paths. |
| PROV-010 | `fireworks`: OpenAI-compatible auth, payload, tool/reasoning stream, and errors. |
| PROV-011 | `github-copilot`: token/header transformation, vision flag, auth, stream, and errors. |
| PROV-012 | `google`: Generative AI key auth, contents/tools/thinking, stream, and errors. |
| PROV-013 | `google-vertex`: API-key and ADC/service-account auth, endpoint, stream, and errors. |
| PROV-014 | `groq`: OpenAI-compatible auth, model options, stream, and errors. |
| PROV-015 | `huggingface`: endpoint/auth, payload, stream, and error behavior. |
| PROV-016 | `kimi-coding`: Anthropic-compatible auth, thinking/signature, stream, and errors. |
| PROV-017 | `minimax`: Anthropic-compatible auth, thinking/tools, stream, and errors. |
| PROV-018 | `minimax-cn`: regional Anthropic-compatible auth, thinking/tools, stream, and errors. |
| PROV-019 | `mistral`: Conversations payload, cache affinity, reasoning, stream, retry, and errors. |
| PROV-020 | `moonshotai`: OpenAI-compatible auth, reasoning/content replay, stream, and errors. |
| PROV-021 | `moonshotai-cn`: regional OpenAI-compatible auth, reasoning, stream, and errors. |
| PROV-022 | `nvidia`: OpenAI-compatible auth, payload, stream, and errors. |
| PROV-023 | `openai`: Responses auth, cache, tools/images/reasoning, stream, and errors. |
| PROV-024 | `openai-codex`: browser/device OAuth, account affinity, SSE/WebSocket, zstd, and recovery. |
| PROV-025 | `opencode`: mixed Responses/Completions/Anthropic dispatch and auth. |
| PROV-026 | `opencode-go`: regional mixed-API dispatch, auth, stream, and errors. |
| PROV-027 | `openrouter`: Completions and image-generation routes, auth, stream, and errors. |
| PROV-028 | `qwen-token-plan`: token-plan auth, OpenAI-compatible payload, stream, and errors. |
| PROV-029 | `qwen-token-plan-cn`: regional token-plan auth, payload, stream, and errors. |
| PROV-030 | `qwen-token-plan-individual`: individual token-plan auth, payload, stream, and errors. |
| PROV-031 | `together`: OpenAI-compatible auth, reasoning/content format, stream, and errors. |
| PROV-032 | `vercel-ai-gateway`: gateway auth, route/payload translation, stream, and errors. |
| PROV-033 | `xai`: OpenAI-compatible auth, reasoning/tool payload, stream, and errors. |
| PROV-034 | `xiaomi`: OpenAI-compatible auth, deepseek reasoning replay, stream, and errors. |
| PROV-035 | `xiaomi-token-plan-ams`: regional token-plan auth, payload, stream, and errors. |
| PROV-036 | `xiaomi-token-plan-cn`: regional token-plan auth, payload, stream, and errors. |
| PROV-037 | `xiaomi-token-plan-sgp`: regional token-plan auth, payload, stream, and errors. |
| PROV-038 | `zai`: OpenAI-compatible auth, reasoning/tool payload, stream, and errors. |
| PROV-039 | `zai-coding-cn`: regional coding auth, reasoning/tool payload, stream, and errors. |

## D. AI transport and message semantics

| ID | Capability that must work | Required acceptance |
|---|---|---|
| AI-001 | context/message serialization | Text, thinking, redacted thinking, images, tool calls/results, custom fields, timestamps, and unknown fields. |
| AI-002 | SSE framing | `data:` lines, comments, multiline data, blank events, `[DONE]`, malformed JSON, UTF-8 splits, and EOF. |
| AI-003 | HTTP chunk boundaries | Headers/body splits, partial UTF-8, content length/chunked, timeout, cancellation, and disconnect. |
| AI-004 | WebSocket transport | connect, reuse, session affinity, continuation, busy isolation, idle/max-age eviction, close/error, and SSE fallback. |
| AI-005 | partial JSON | nested objects/arrays, strings/escapes, Unicode surrogate pairs, truncation, invalid fragments, and monotonic deltas. |
| AI-006 | event stream lifecycle | start, text/thinking/tool starts/deltas/ends, usage, done, error, abort, deferred, and exactly-once settlement. |
| AI-007 | tool-call normalization | IDs, missing results, namespace, provider metadata, duplicate calls, malformed arguments, and cleanup. |
| AI-008 | reasoning/thinking | budgets, xhigh/max support, provider mapping, signature replay, empty signatures, and disable semantics. |
| AI-009 | sampling/tool options | temperature, top-p, max tokens, stop, tool choice, parallel tools, strict schema, and provider compatibility. |
| AI-010 | images | input encoding, resize, unsupported MIME, tool-result images, block-images, and model capability errors. |
| AI-011 | error contract | preserve HTTP/body/provider details, classify retryability, redact secrets, and expose actionable user text. |
| AI-012 | abort/timeout | abort before request, during request, during stream, during retry/backoff, and after terminal event. |
| AI-013 | token/context estimates | exact totals, overflow detection, reserve/compaction threshold, and unknown-token fallback. |

## E. Agent loop, harness, and built-in tools

| ID | Capability that must work | Required acceptance |
|---|---|---|
| AGENT-001 | one turn | Prompt → stream → assistant completion → persistence → usage/event settlement. |
| AGENT-002 | multi-turn | Context includes prior turns exactly once; no duplicate user entries or lost assistant messages. |
| AGENT-003 | tool loop | Multiple sequential and parallel tool calls, tool result ordering, follow-up assistant, and stop conditions. |
| AGENT-004 | before/after hooks | Mutation, cancellation, errors, ordering, and cleanup. |
| AGENT-005 | steering/follow-up queues | Queue modes, queued prompt ordering, cancellation, and prompt-during-response behavior. |
| AGENT-006 | deferred responses | Submit/poll/cancel, pending state, timeout, and runtime events. |
| AGENT-007 | retry/compaction interaction | Retry after transient stream drop, compact on overflow, and no duplicate operations. |
| AGENT-008 | system prompt | Default coding prompt, context files, tools, skills, templates, extensions, and user overrides. |
| AGENT-009 | skills | Discovery, frontmatter, name/description validation, collision precedence, invocation control, nested skills, and disabled skills. |
| AGENT-010 | prompt templates | Discovery, interpolation, arguments, malformed template, and selection. |
| AGENT-011 | memory | Read/write/search/compaction behavior, missing files, and persistence. |
| AGENT-012 | telemetry | Span/event/counter lifecycle, no-op mode, panic/error settlement, and no leaked secrets. |
| TOOL-001 | `read` | File, directory, range, binary, missing, permission, truncation, Unicode, and output format. |
| TOOL-002 | `write` | New/overwrite/atomicity, parent creation, permissions, Unicode, error, and mutation queue. |
| TOOL-003 | `edit` | Exact replacement, multiple matches, no match, ambiguity, diff, Unicode, and conflict. |
| TOOL-004 | `edit-diff` | Patch parsing, add/delete/rename, malformed patch, path safety, and rollback. |
| TOOL-005 | `bash` | cwd, env, timeout, stdin, stdout/stderr, exit code, signal, cancellation, truncation, shell quoting, and late output. |
| TOOL-006 | `ls` | Files/directories, hidden, sorting, symlink, missing, permission, and limit. |
| TOOL-007 | `find` | Glob, root relativization, nested gitignore, symlink, hidden, invalid pattern, and limit. |
| TOOL-008 | `grep` | Regex/literal, binary, Unicode, context, path filters, gitignore, invalid pattern, and output limit. |
| TOOL-009 | `image` | MIME/read/resize, capability gating, terminal rendering, and error. |
| TOOL-010 | mutation queue | Concurrent writes, ordering, conflict, cancellation, deferred writes, and cleanup. |
| TOOL-011 | tool policy | Allow/deny built-ins and extensions, strict schema, malformed calls, and blocked terminate behavior. |
| TRUST-001 | project trust | First-run prompt, approve/no-approve, persisted decision, path canonicalization, symlink, and failure. |
| TRUST-002 | trust UI | Selector navigation, cancel, save, reload, and subsequent startup behavior. |

## F. Session, JSONL, compaction, branching, and export

| ID | Capability that must work | Required acceptance |
|---|---|---|
| SES-001 | JSONL v4 header | Required fields, version, ID, cwd, parent, metadata, and invalid header. |
| SES-002 | message entries | IDs, sequence, parent links, timestamps, termination, custom messages, and unknown fields. |
| SES-003 | model/thinking/tool entries | All change records, ordering, active tools, and replay. |
| SES-004 | operation lane records | start/abort/finish/attempt/tool/queue/deferred/usage records and sequence allocation. |
| SES-005 | append/flush/reopen | Atomic append, partial final line, concurrent readers/writers, and restart. |
| SES-006 | v1/v2/v3 migration | Every supported legacy shape, hook/custom role mapping, backup, and malformed refusal. |
| SES-007 | session discovery | cwd root, IDs, labels, names, symlink, modified time, invalid file, and ordering. |
| SES-008 | resume/continue | Rehydrate context, model/provider, thinking, tools, queued state, and footer. |
| SES-009 | session tree | Branch navigation, parent/child, selection, search/filter, cancel, and active-stream guard. |
| SES-010 | fork | Fork from prior user message, retained tail, parent session, and independent persistence. |
| SES-011 | clone | Duplicate at current position, metadata, and independent future turns. |
| SES-012 | new/import | New session reset, JSONL import, invalid import, and context preservation. |
| SES-013 | manual compaction | Summary, retained tail, token accounting, extension hooks, abort, and no duplicate messages. |
| SES-014 | automatic compaction | Threshold, reserve tokens, retry, zero-usage, queued prompts, and cancellation. |
| SES-015 | branch summary | Summary generation, auth/config, failure fallback, and replay. |
| SES-016 | session stats/usage | Input/output/cache/reasoning/total tokens, costs, provider/model, and footer. |
| SES-017 | HTML export | Content, thinking, tools, images, skills, whitespace, XSS, Unicode, and missing assets. |
| SES-018 | JSONL export | Byte-valid JSONL, session metadata, and round-trip import. |

## G. Interactive TUI and terminal behavior

| ID | Capability that must work | Required acceptance |
|---|---|---|
| TUI-001 | regular mode | Main screen remains visible, output scrolls, prompt remains usable, and no alt-screen leak. |
| TUI-002 | fullscreen alt-screen | Enter/leave bytes, prior screen restoration, nested modal restoration, and crash/signal cleanup. |
| TUI-003 | resize | Shrink/grow, CJK width, wrapped lines, viewport, footer, editor, and redraw stability. |
| TUI-004 | differential renderer | No stale cells, no duplicate rows, clear-on-shrink, churn, and cursor placement. |
| TUI-005 | editor insertion | ASCII, Unicode, combining marks, emoji, CJK, tabs, newlines, and cursor movement. |
| TUI-006 | editor deletion | Backspace/delete, grapheme boundaries, line joins, and empty editor. |
| TUI-007 | editor history | Previous/next, multiline entries, deduplication, cursor state, and persistence scope. |
| TUI-008 | kill/yank/undo | Kill ring, yank, yank-pop, undo/redo, boundaries, and multiline behavior. |
| TUI-009 | word navigation | Ctrl/Alt word left/right/delete, punctuation, Unicode, and platform key encodings. |
| TUI-010 | bracketed paste | Start/end markers, multiline, large paste, embedded escape bytes, and no marker leakage. |
| TUI-011 | autocomplete | Slash, prompt, skill, template, extension command, filtering, cycling, accept/cancel. |
| TUI-012 | input buffer | Partial UTF-8, escape timeout, pasted bytes, overflow, EOF, and event ordering. |
| TUI-013 | key decoding | arrows, function keys, modifiers, kitty/native modifiers, alt/meta, ctrl, and unknown bytes. |
| TUI-014 | keybinding config | Defaults, migration, custom bindings, conflicts, reload, and extension precedence. |
| TUI-015 | slash command menu | Every command, fuzzy search, argument hints, provider/model argument completion. |
| TUI-016 | slash command execution | **VERIFIED — live:** every `BUILTIN_SLASH_COMMANDS` entry maps to an explicit `SlashKind` arm; the defensive `not wired`/`Unsupported` catch-all is removed. The complete real-PTY command target passes all four cases. See the command audit below. |
| TUI-017 | model selector | Search, grouping, auth state, provider/model display, cancel, selection, and scope cycling. |
| TUI-018 | thinking selector | All levels, provider support, pending tool/stream state, cancel, and persistence. |
| TUI-019 | settings selector | Every setting, nested controls, defaults, invalid values, save, cancel, reload. |
| TUI-020 | theme picker/controller | Built-in/user themes, invalid/missing theme, colors, reload, export, and fallback. |
| TUI-021 | login dialog | Persistent bordered dialog, browser URL/device code, links, progress, waiting, cancel, and secret masking. |
| TUI-022 | session picker | Search, named filter, path labels, modified time, delete/trash, rename, cancel, and open. |
| TUI-023 | tree selector | Branch display, navigation, search, current marker, streaming guard, and cancel. |
| TUI-024 | trust selector | Allow/deny, path display, save/cancel, and keyboard navigation. |
| TUI-025 | modal overlays | Capture/non-capture, stacking, short content, CJK boundary, style cleanup, and input routing. |
| TUI-026 | markdown | Headings, lists, code, tables, links, emphasis, ANSI/theme, wrapping, and malformed markdown. |
| TUI-027 | assistant rendering | Streaming text/thinking, tool calls, errors after partial content, stop reasons, and usage. |
| TUI-028 | user/tool rendering | User messages exactly once, tool progress/result/error/images, custom messages, and compaction banners. |
| TUI-029 | footer/status | cwd, model/provider, usage/cost, spinner, retry, compaction, session name, and width truncation. |
| TUI-030 | images/terminal graphics | Capability probe, kitty/iTerm/sixel fallback, block images, resize, and cleanup. |
| TUI-031 | external editor | Launch, temp file, edit result, cancel, failure, and terminal restoration. |
| TUI-032 | clipboard | Text copy/paste, native backend availability, Wayland/X11/Termux fallback, and image clipboard. |
| TUI-033 | interrupt/cancel | Ctrl-C during prompt, stream, tool, compaction, retry, modal, login, and external editor. |
| TUI-034 | quit/shutdown | Clean save, child termination, extension cleanup, terminal restoration, SIGTERM, and repeated quit. |
| TUI-035 | terminal portability | Linux terminals, tmux, TERM variants, no color, narrow width, no display, and non-interactive stdin. |
| TUI-036 | hidden `/debug` command | **VERIFIED — live/unit:** Rust writes the bounded `pi-debug.log` artifact with the upstream ISO timestamp, terminal size, rendered-line widths/JSON, and Agent-message JSONL, then reports success or an explicit write error. The real PTY asserts the exact agent-directory path and ISO header. |
| TUI-037 | hidden `/arminsayshi` command | **VERIFIED — live/unit:** Rust renders the pinned XBM-derived component with bounded time-based scanline animation and width-safe output; it owns no worker/task, so quit drops it cleanly. The real PTY covers repeat and terminal restoration, and unit coverage spans widths 1–64. |
| TUI-038 | hidden `/dementedelves` command | **VERIFIED — live/unit:** Rust renders the Earendil announcement with upstream text/link, optional-asset-safe fallback, bounded borders, and width-safe clipping. The real PTY exercises it after resize and quit restoration. |
| TUI-039 | OpenCode/Kimi easter egg (Daxnuts) | **VERIFIED — live/unit:** the exact 6,144-character upstream 32x32 RGB payload is embedded in Rust, rendered as truecolor half-blocks, guarded by non-empty-image and real-ESC tests, and triggered only for provider `opencode` plus a case-insensitive `kimi-k2.5` model id. The real PTY exercises the matching model path. |

Complete built-in interactive command set (each must be exercised). The first
23 entries are the pinned upstream command surface; `/theme`, `/clear`, and
`/help` are additional Rust-native commands exposed by the current product and
are therefore part of the acceptance surface too:

`/settings`, `/model`, `/thinking`, `/scoped-models`, `/theme`, `/export`,
`/import`, `/share`, `/copy`, `/name`, `/session`, `/changelog`, `/compact`,
`/clear`, `/hotkeys`, `/help`, `/quit`, `/fork`, `/clone`, `/tree`, `/trust`,
`/login`, `/logout`, `/new`, `/resume`, and `/reload`.

The pinned upstream interactive implementation also contains three hidden
diagnostic/easter-egg invocations—`/debug`, `/arminsayshi`, and
`/dementedelves`—plus a model-triggered OpenCode/Kimi Daxnuts component. They
are tracked as the hidden-command rows above; they are not silently excluded
merely because they are absent from `BUILTIN_SLASH_COMMANDS`.

### Interactive implementation checkpoint: hidden handlers and Daxnuts (2026-08-26)

The four hidden-command rows now have Rust-native implementations and live
PTY evidence. The pinned oracle's
`packages/coding-agent/src/modes/interactive/interactive-mode.ts` dispatches
`/debug`, `/arminsayshi`, and `/dementedelves` at lines 3068–3079 and defines
their user-visible handlers at lines 6450–6493. The same file defines the
Daxnuts trigger and handler at lines 6495–6504; the implementation is in
`packages/coding-agent/src/modes/interactive/components/daxnuts.ts`, with the
Armin and Earendil components in the neighboring `armin.ts` and
`earendil-announcement.ts` files.

The scoped Rust implementation is in
`crates/pi-coding-agent/src/interactive/easter_eggs.rs`,
`crates/pi-coding-agent/src/interactive/slash.rs`,
`crates/pi-coding-agent/src/interactive/mod.rs`, and
`crates/pi-coding-agent/src/modes/interactive.rs`, with permanent PTY coverage
in `crates/pi-coding-agent/tests/interactive_slash_complete_pty.rs`. The
components use render-time `Instant` state rather than spawned tasks; quitting
the loop therefore drops all component state without a task leak.

Audit evidence:

```text
rg -n 'handleDebugCommand|handleArminSaysHi|handleDementedDelves|handleDaxnuts|checkDaxnutsEasterEgg|text === "/(debug|arminsayshi|dementedelves)"' ../pi-rust-s1-audit.KMw0N2/upstream_pi/packages/coding-agent/src/modes/interactive/interactive-mode.ts
  -> pinned dispatch/handler/trigger matches above

rg -n -i 'daxnuts|arminsayshi|dementedelves|handleDebugCommand|handleArminSaysHi|handleDementedDelves' crates/pi-tui/src crates/pi-coding-agent/src
  -> Rust-native handlers/components and dispatch now match the four hidden surfaces

rg -n 'name: "' crates/pi-coding-agent/src/interactive/slash.rs
  -> 26 registered normal commands; every command kind has an explicit arm in
     interactive.rs, including export, new, resume, name, import, reload,
     fork, clone, trust, copy, tree, share, and the selector/terminal commands.

rg -n 'not wired|SlashKind::Unsupported' crates/pi-coding-agent/src/modes/interactive.rs crates/pi-coding-agent/src/interactive/slash.rs
  -> no matches

python3 DAX payload/escape audit (read-only source comparison)
  -> Rust DAX_HEX length 6,144; pinned upstream length 6,144; SHA-256
     4a1df9e4bdd8ecbf6beb4ddc6c7dfa6b80a16f0ff6e18fb9e0139d415ad59f1d on
     both payloads; no literal backslash escape in Rust scanline source.

Focused Cargo checks
  -> `cargo check -p pi-coding-agent --offline`: exit 0;
     `cargo test -p pi-coding-agent --offline --lib interactive::easter_eggs`
     6 passed; hidden parser test 1 passed; timestamp unit test 1 passed;
     `cargo test -p pi-coding-agent --offline --test
     interactive_slash_complete_pty -- --test-threads=1`: 4 passed;
     `cargo test -p pi-coding-agent --offline --test interactive_full_matrix
     -- --test-threads=1`: 7 passed.

Strict clippy boundary
  -> the unmodified strict package command reports four existing diagnostics
     in `core/changelog.rs`, `core/extensions/integration.rs`, and
     `modes/rpc.rs`; the same command with
     `-A clippy::invalid_regex -A clippy::needless_update
     -A clippy::drop_non_drop` exits 0. No interactive diagnostic remains.
```

The scoped release condition is met: the shared coding-agent package compiles,
the complete hidden-command PTY case passes, and the case covers
success/repeat/narrow/resize/cancellation/error/quit-restoration. Workspace
strict clippy still reports unrelated diagnostics outside this interactive
scope; the scoped clippy run passes when those existing diagnostics are
explicitly allowed.

## H. Text, JSON, RPC, server, client, and protocol surfaces

| ID | Capability that must work | Required acceptance |
|---|---|---|
| MODE-001 | text/print lifecycle | startup, prompt, streaming, tools, usage, errors, retry, compaction, exit. |
| MODE-002 | JSON event taxonomy | Every start/delta/end/done/error/tool/usage/compaction/session event and field. |
| MODE-003 | JSON ordering | No interleaving violations, linear stream, flush behavior, and valid JSON per line. |
| RPC-001 | prompt | Success, multi-turn, empty, malformed, concurrent, and response correlation. |
| RPC-002 | steer/follow_up | Queue modes, pending stream, cancellation, and ordering. |
| RPC-003 | abort/abort_retry/abort_bash | Each target state, idempotence, race, and response. |
| RPC-004 | new_session/switch_session/fork/clone | Persistence, context, IDs, parent links, and errors. |
| RPC-005 | get_state/get_messages/get_entries/get_tree | Exact schemas, pagination/selection, and state transitions. |
| RPC-006 | set_model/cycle_model/get_available_models | Resolver, auth, scope, and response schema. |
| RPC-007 | set_thinking_level/cycle/get_available | All levels, provider limits, and schema. |
| RPC-008 | set_steering_mode/set_follow_up_mode | Valid/invalid values and queue behavior. |
| RPC-009 | compact/set_auto_compaction/set_auto_retry | Runtime changes, in-flight operation, and persistence. |
| RPC-010 | bash | Cwd/env/timeout/output/abort and event correlation. |
| RPC-011 | get_session_stats/export_html/set_session_name | Exact values, files, errors, and response correlation. |
| RPC-012 | unknown commands/IDs | Deterministic error without poisoning subsequent commands. |
| RPC-013 | RPC process exit | EOF, SIGTERM, broken pipe, child process exit, and no leaked tasks. |
| PROTO-001 | CBOR value model | Definite lengths, i53, floats, maps, arrays, null/bool, undefined rules, limits. |
| PROTO-002 | frame codec | 4-byte big-endian length, partial reads, max 16 MiB, malformed length, EOF. |
| PROTO-003 | protocol schema | Hello/version, every client/server message, unknown fields, and invalid values. |
| SERVER-001 | Unix listener | Path, stale socket, permissions, bind failure, shutdown, and reconnect. |
| SERVER-002 | connection lifecycle | Handshake, request/response, event subscription, disconnect, timeout, and disposal. |
| SERVER-003 | session manager | Exclusivity, snapshots, updates, queues, close, and concurrent clients. |
| CLIENT-001 | connect/reconnect | Timeout, late response suppression, listeners, state sync, and disposal. |
| CLIENT-002 | requests/sessions | All typed operations, correlation, malformed response, and server error. |
| BACKEND-001 | SQLite/session backend | Schema, migrations, CRUD, index/search, locking, concurrent access, and corruption. |

## I. Extensions, package/update, evaluation, and distribution

| ID | Capability that must work | Required acceptance |
|---|---|---|
| EXT-001 | native command | Registration, invocation, args, output, error, and reload. |
| EXT-002 | native hook | Before/after lifecycle, mutation, cancel, error, and cleanup. |
| EXT-003 | native renderer | Message/tool/custom rendering, fallback, and reload. |
| EXT-004 | native tool | Schema, execution, abort, result/details, policy, and next-turn visibility. |
| EXT-005 | native flags | Parsing, help, value types, conflicts, and mode propagation. |
| EXT-006 | native provider/model | Registration, auth, stream, model selection, and reload. |
| EXT-007 | extension UI/editor | Custom component, keybindings, modal, progress, and cleanup. |
| EXT-008 | extension discovery/reload | Precedence, collisions, invalid source, stale-resource removal, and no duplicate subscriptions. |
| EXT-009 | extension context actions | Native handlers can read model/session/trust/queue/signal state and invoke abort, shutdown, messaging, labels, tools, model/thinking, compaction, and session actions with correct lifecycle/stale-context behavior. |
| EXT-010 | extension UI context completeness | Terminal input listeners, custom overlays, header/footer, hidden-thinking label, autocomplete/editor factories, theme access/switching, and tool-expansion state. |
| EXT-011 | extension tool contract | Label/prompt metadata, prepare-arguments, constrained sampling, sequential/parallel execution, update callbacks, render-call/result state, abort, and details. |
| PKG-001 | install/remove/list | Native package source resolution, paths, settings update, rollback, and errors. |
| PKG-002 | git/SSH package source | URL parsing, auth boundary, revision, network failure, and cleanup. |
| PKG-003 | update/version | Changelog, version check, offline, current/new version, and safe failure. |
| PKG-004 | unsupported JS/npm/Bun boundary | Deterministic Rust-only guidance and no execution. |
| EVAL-001 | harness execution | Input/session capture, deterministic scores, diagnostics, and failures. |
| EVAL-002 | usage summary | Session JSONL accounting, cost/tokens, table, and malformed session. |
| DIST-001 | release build | Reproducible optimized binary, no JS/TS source/runtime, and clean package boundary. |
| DIST-002 | installed command | PATH launcher/install points to Rust binary, `pi --version`, help, and interactive launch. |
| DIST-003 | clean environment | Fresh HOME/config/session roots, no existing auth/settings, and first-run behavior. |
| DIST-004 | upgrade/rollback | Existing sessions/settings/auth survive version replacement and failed update. |

## J. Cross-cutting quality and adversarial cases

| ID | Capability that must work | Required acceptance |
|---|---|---|
| X-001 | Unicode and terminal width | CJK, combining marks, emoji, regional indicators, RTL-ish text, and invalid UTF-8 boundaries. |
| X-002 | empty/null/omitted values | Every parser and wire schema distinguishes empty, null, missing, and default correctly. |
| X-003 | malformed files | Settings, models, auth, sessions, resources, manifests, and exports fail safely. |
| X-004 | filesystem safety | Symlinks, traversal, permissions, races, atomic writes, and unrelated-file preservation. |
| X-005 | secret safety | API keys, OAuth codes/tokens, cookies, headers, and command arguments never enter logs, sessions, errors, or test artifacts. |
| X-006 | concurrency | Simultaneous turns, refreshes, session reads/writes, tool writes, RPC commands, client reconnects, and shutdown. |
| X-007 | cancellation | Every async operation has a cancellation path that settles once and releases resources. |
| X-008 | retry/idempotence | Repeated commands, duplicate events, retry, reconnect, and re-run do not duplicate durable state. |
| X-009 | resource limits | Large prompt/output/session/file/image/frame, deep JSON, narrow terminal, and slow consumer. |
| X-010 | platform boundaries | Linux desktop, tmux, no display, no browser opener, proxy, offline, and non-TTY stdin. |
| X-011 | diagnostics | Error messages identify action/provider/path and provide recovery without internal secrets. |
| X-012 | regression discipline | Every discovered failure gets a permanent reproducer and a real runtime re-test. |

## Oracle inventory

The pinned upstream oracle is `../pi-rust-s1-audit.KMw0N2/upstream_pi` at the
revision recorded in `PLAN.md`. The source and test inventory must be refreshed
with these commands and their output retained in the scope audit:

```bash
rg --files ../pi-rust-s1-audit.KMw0N2/upstream_pi/packages/*/src \
  ../pi-rust-s1-audit.KMw0N2/upstream_pi/packages/*/test | sort
rg --files crates/*/src crates/*/tests | sort
```

Upstream currently contains 1,300+ package source/test artifacts across
protocol, telemetry, ai, agent, client, server, session-backends, tui,
coding-agent, and evals. The test filenames are not themselves proof of Rust
parity: each relevant test must map to one of the capability IDs above and to
an executable Rust test or an explicitly reviewed live/manual gate.

## Definition of complete

The campaign is complete only when:

1. every ID has an implementation review and current evidence;
2. every upstream test family has a mapped Rust oracle or a documented,
   user-approved Rust-only boundary with equivalent behavior;
3. every user-facing command and TUI transition has a real process/PTY test;
4. every provider that can be credentialed in this environment has a live
   request/stream/error test, while unavailable credentials remain explicitly
   blocked rather than silently marked green;
5. debug and release binaries pass the same black-box matrix;
6. clean-HOME, restart, cancellation, malformed-input, concurrency, and
   terminal-restoration passes are green; and
7. `PLAN.md`, `HANDOFF.md`, `CONVERSION-LEDGER.md`, and the scoped gates agree
   with the measured result.

Until those conditions hold, pi-rust is not reported as 1:1 or flawless.
