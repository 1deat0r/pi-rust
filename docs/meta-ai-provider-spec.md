# meta-ai provider spec (intentional-divergence oracle)

There is no upstream pi oracle for Meta Model API: `upstream_pi`
(pinned d7296c0) has no Meta provider. This file is the oracle for
the `meta-ai` provider row. Facts below were verified live on
2026-09-21 unless marked otherwise.

## Endpoint and auth

- Base URL: `https://api.meta.ai/v1` (dev.meta.ai docs).
- Auth: Bearer token. Docs name `MODEL_API_KEY`; locally
  `META_API_KEY` is the working key (`MODEL_API_KEY` 401s on this
  machine). The provider accepts `MODEL_API_KEY` first, then
  `META_API_KEY`.
- Protocols: Responses API, Chat Completions, Messages
  (Anthropic-compatible). The Rust lane uses `openai-completions`
  (top-level `reasoning_effort` matches the docs exactly).

## Live `/v1/models` roster (HTTP 200, 8 entries, 2026-09-21)

Chat/agentic (ported): `muse-spark-1.3`,
`muse-spark-1.3-contributor`, `muse-spark-1.2`,
`muse-spark-1.2-contributor`, `muse-spark-1.1`.

Non-chat (deliberately excluded, same as the Hermes `meta-ai`
profile's `muse-image-`/`muse-voice-` prefix filter):
`muse-image-1.0` (image gen/edit), `muse-voice-transcribe-1.0`
(speech-to-text), `sam-3.1` (segmentation).

Docs tiers: Standard = spark-1.3/1.2/1.1; Contributor =
spark-1.3-contributor/1.2-contributor. Context window 1,048,576
tokens (docs).

## Reasoning effort (dev.meta.ai/docs/reasoning)

Levels: `minimal`, `low`, `medium`, `high`, `xhigh`, `max`.
`none` returns HTTP 400 on Muse Spark — the catalog maps `off` to
`minimal` (closest to off). `max` is Standard-tier `muse-spark-1.3`
only; Contributor models omit it (`max: null` → param omitted →
model default). All other levels pass through verbatim on both
tiers. Reasoning tokens bill as output tokens; `maxTokens` 16384
follows the Hermes provider default (reasoning consumes the budget
first). Model `cost` is vendored as zero: Meta per-token pricing was
not pinned at port time — zeros mean unknown, not free.

## Evidence tier

Unit + mock. Live vendor traffic is offline-unverifiable; the roster
above is a dated live observation, not a replayable fixture.
