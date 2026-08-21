# pi-ai — port status

Core types/stream infra + faux + anthropic landed. 128 workspace tests, 0 warnings.

## Done
- types.rs (messages, content blocks, usage/cost, stop reasons, thinking
  levels, stream events, deferred handles, tools/context, images)
- event_stream.rs (AssistantMessageEventStream + StreamSink adapter)
- partial_json.rs (streaming tolerant parser, oracle-verified)
- sse.rs (SSE parser, UTF-8-safe)
- model.rs (Model/ModelCost, cost accounting incl. 1h cache write + tiers,
  thinking-level helpers)
- providers/faux.rs (scripted provider w/ usage estimation & deltas)
- api/anthropic_messages.rs (buildParams, convertMessages/convertTools,
  SSE event assembly, stop-reason mapping, cost on message_start/delta)
- providers/anthropic.rs (catalog subset: opus-4-8, sonnet-4-6, haiku-4-5,
  opus-5 w/ tier; env ANTHROPIC_API_KEY)
- 8 fixture-driven anthropic adaptor tests

## Remaining (upstream mapping)
- api/: openai-completions, openai-responses, azure, codex, google, bedrock,
  mistral, cloudflare, vertex, pi-messages (+ lazy variants)
- providers/: ~40 providers (openai, google, bedrock, xai, groq, deepseek,
  mistral, together, fireworks, openrouter, cerebras, nvidia, baseten,
  github-copilot, vercel, cloudflare, huggingface, minimax, moonshotai,
  qwen, zai, xiaomi, kimi, opencode, radius, ant-ling, amazon-bedrock, ...)
- model catalog data (models.generated.ts -> models.json; runtime load +
  merge over ~/.pi/agent/models.json)
- auth (oauth.ts, env-api-keys.ts, session-resources.ts, credential store)
- images API (openrouter-images)
- transform-messages.ts (message transformers: deferred tools, session
  affinity); websocket transports; constrained sampling
- Anthropic gaps: deferred tools (tool_reference), server-side fallback
  (fallbacks), OAuth Claude-Code name mapping, adaptive-thinking replay
  (forceAdaptiveThinking), eager input streaming beta header, client
  injection (options.client)
