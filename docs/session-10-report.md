# pi → pi-rust 1:1 port — Session 10 report (2026-08-22)

> Historical snapshot. Its landed/remaining lists describe the 2026-08-22
> checkpoint and are not the current conversion status. Use
> CONVERSION-LEDGER.md, PLAN.md, and HANDOFF.md for current evidence.

**HEAD:** 37ca48c → cca3a30 — 8 commits, all pushed. **Tests:** 411 → 529 passing, 0 warnings, deterministic.

## Landed this session

1. **pi-ai adaptors**: google-generative-ai (REST SSE, thought signatures, per-family thinking configs + budget tables), openai-responses (+shared full event loop: partial-JSON tool args, reasoning replay/backfill, service-tier pricing), azure-openai-responses (deployment/resource resolution, azure host normalization), transform-messages (cross-model safety). Registry dispatch fixes: google→google adaptor, openai→responses (fidelity bug), opencode/go→ByApi, vercel-ai-gateway→anthropic (confirmed by live 401).
2. **Model runtime (P4)**: upstream defaultModelPerProvider (39 providers) + `provider/model:thinking` hint parsing; `pi -p` routes real providers through the Models facade; terminal errors exit nonzero. Live E2E: vercel wire path + auth-error parsing. faux path unchanged.
3. **RPC mode (P5)**: full `--mode rpc` JSONL-over-stdio — prompt/steer/follow_up stream `message_update` events + `agent_settled`; state/model/thinking/queue-mode/bash/session/messages commands; harness compaction wired into `compact` (errors are failure responses, never kill the loop; empty sessions return a zero result).
4. **Server + client (P6)**: pi-server (UnixSocketListener: stale-socket probe, private bind + symlink, mode; PiServer handshake/version check/hello_error, Command dispatch, snapshot publisher with revision + broadcast; InMemoryService) + pi-client (hello handshake awaited, request correlation, ServerEvent fanout, snapshot state). E2E over a real unix socket incl. bad-version hello_error; codec framing probe.
5. **TUI core + interactive mode (P7)**: pi-tui (crossterm backend + pi key surface, Component/Scene + differential line renderer, flex solver, keys model, Text/Spacer/VStack/HStack/Box/Loader/SelectList/ScrollView/TruncatedText/Input with unicode cursor editing); interactive TTY loop (You:/π: transcript, Boxed input bar, inline editing, live text_delta streaming into the transcript, Ctrl-C exit, JSONL session persistence) — verified end-to-end in tmux (100×30): typed `hello tui`, faux reply rendered + persisted, clean exit.

## Documented divergences
- mistral routed via openai-completions until the mistral-conversations adaptor port;  RPC compact needs a facade-registered provider (faux intentionally not in the builtin registry).
- RPC export_html returns the error surface until export-html is ported.
- pi-ai Usage tokens remain u64 (negative adjustments unrepresentable) — decision deferred.

## Remaining (multi-session)
- Full TUI surface: Editor, Markdown, Image, SettingsList, overlays, terminal-image, fuzzy, kill-ring, undo stack.
- coding-agent parity: extensions (types/loader/runner/wrapper), package manager, export-html, themes, provider attribution/composer, usage totals/event bus, config/auth CLI commands, interactive feature set (slash commands, selectors, footer).
- P9: session-backends sqlite, evals, packaging + parity suite.
- Adaptors: openai-codex-responses, mistral-conversations, bedrock-converse, cloudflare (+stream/auth), github-copilot headers, google-vertex, pi-messages.

## Files
- PLAN.md — Session 10 ledger added. Crates TODO.md updated. Harness global memory scheduled for continuation handoff.
