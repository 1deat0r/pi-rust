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


## Done (Session 9 — model catalog + Models facade + provider registry)
- model_catalog.rs: the full 39-provider / 1267-model catalog vendored from
  the published @earendil-works/pi-ai@0.84.2 tarball (`crates/pi-ai/data/*.json`
  + `.manifest.json`; upstream gitignores these models.dev outputs). Port of
  model-catalog.ts flatten + models.generated.ts MODELS table +
  providers/all.ts catalog read side (getBuiltinModel / getBuiltinModels /
  getBuiltinProviders / getBuiltinModelDataGeneratedAt). 8 tests.
- auth.rs: port of auth/types.ts + auth/helpers.ts — Credential union,
  ApiKeyCredential/OAuthCredential, AuthContext (env/fileExists), ModelAuth,
  AuthResult/AuthCheck, ApiKeyAuth/OAuthAuth/ProviderAuth traits,
  CredentialStore trait + InMemoryCredentialStore (interior mutability),
  envApiKeyAuth helper.
- models.rs: port of models.ts + models-store.ts — Provider struct with
  single/by-api stream dispatch (missing api -> upstream "no API
  implementation" stream error), createProvider, mergeHeaders (case-
  insensitive override), ModelsStore + InMemoryModelsStore, createModels
  (providers RwLock map + CredentialStore + ModelsStore + AuthContext),
  setProvider/delete/clear/getProviders/getProvider/getModels/getModel,
  checkAuth/getAvailable/getAuth/applyAuth (auth application: apiKey/headers/
  env/baseUrl override + model-static header merge), stream/complete/
  streamSimple/completeSimple with lazy auth + error-stream termination.
  9 tests.
- providers/all.rs: all 39 builtin provider factories with vendored catalogs
  + env-key auth (anthropic wired to the real anthropic_messages adaptor;
  other API adaptors stream the upstream no-API-implementation error until
  ported). builtinModels() registry collection. 7 tests.
- pi-coding-agent: --list-models [search] flag + list_models.rs port of
  cli/list-models.ts (table format: provider/model/context/max-out/thinking/
  images; formatTokenCount; fuzzy-filter placeholder = substring).
  3 tests; binary verified against real env keys.
- Workspace: 411 tests passing (was 384); pi-ai 80; 0 lib warnings in touched
  crates; new modules clippy-clean.

## Done (Session 11 — full adaptor completion)
- api/: mistral-conversations (native), openai-codex-responses (SSE), bedrock-converse (SigV4 +
  aws-eventstream), google-vertex (API-key + ADC JWT), cloudflare (workers-ai/ai-gateway auth),
  github-copilot-headers (dynamic request headers), pi-messages (broker), openrouter-images (+ images
  facade with 45-model vendored catalog). All 39 catalog providers now dispatch real streams.
- providers/all.rs wiring: amazon-bedrock, google-vertex, cloudflare-ai-gateway/workers-ai,
  github-copilot, mistral, openai-codex all route to their adaptors; no_stream() helper removed.
- Tests: pi-ai 265 (was 142 at Session 10 baseline release).
- Remaining (documented divergences, TODO-marked): OAuth device-code flows,
  DeferredHandles fetch machinery, provider-models.json runtime merge (seam in coding-agent), images
  retry loop, deferred tools, and the codex WebSocket/session-cache parity audit.

## Done (Session 10 — adaptor completion)
- api/: google-generative-ai (REST SSE + thought signatures + family
  thinking configs + budgets), openai-responses (+shared: full event loop,
  partial-JSON tool args, reasoning signature replay + backfill, service
  tier pricing, developer-role system prompts, fc_ id normalization),
  azure-openai-responses (deployment/resource config, azure host
  normalization), transform-messages (cross-model safety rules). Provider
  registry now dispatches google→google, openai→responses, opencode(s)→ByApi,
  vercel-ai-gateway→anthropic.
- Remaining api: deferred tools and the residual provider-specific audits;
  constrained JSON-schema and OpenAI grammar tool sampling is implemented in
  the shared resolver and all adaptors that advertise those capabilities.

- providers/: all 39 factories registered with catalogs + auth (Session 9);
  non-anthropic providers stream the upstream no-API-implementation error
  until their api adaptor is ported. Special auth remains: cloudflare-auth
  (account/gateway), github-copilot OAuth filter, google-vertex ambient,
  amazon-bedrock ambient, openai-codex OAuth, radius dynamic refresh.
- model catalog runtime load: on-disk `~/.pi/agent/models.json` merge over
  the bundled catalog (pi-coding-agent model-config + models-store; the
  bundled read side is complete).
- auth: OAuth flows (oauth.ts flows, device code, PKCE), session-resources,
  lazyOAuth helpers, login/logout orchestration on the facade.
- images API (openrouter-images)
- transform-messages.ts (message transformers: deferred tools, session
  affinity); residual websocket transports; deferred constrained-tool loading
- Anthropic gaps: deferred tools (tool_reference), server-side fallback
  (fallbacks), OAuth Claude-Code name mapping, adaptive-thinking replay
  (forceAdaptiveThinking), eager input streaming beta header, client
  injection (options.client)
