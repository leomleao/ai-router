# Capabilities

This is a model-only router. `/v1/capabilities` reports schema version 3 and
the current direct Antigravity and OpenAI provider readiness. Contract fixtures
exercise local serialization and decoding. Local Google sign-in and exact-ID
discovery succeeded; Flash High, Opus Medium and Sonnet High passed live text,
streaming, JSON and client-tool round trips through both APIs. Argon Medium
returned upstream quota errors. Matched CLI latency and real token refresh
remain acceptance gates; see [validation](validation.md).

| Feature | Current behavior |
| --- | --- |
| Provider routing | Exact discovered model IDs map to one provider and are never remapped. Conflicting IDs are withheld. There is no account fallback, and a known model whose provider is unavailable is never rerouted. |
| Model fallback | A requested ID the router has never served (not live, conflicting, or remembered) is served by a configured exact target instead of returning `model_not_found`. IDs starting with `gemini` (any case) use `AI_ROUTER_FALLBACK_GEMINI_MODEL` (default `gemini-3.8-flash-medium`); all others, including `gpt-*`, `o*`, codex and `claude-*`, use `AI_ROUTER_FALLBACK_OPENAI_MODEL` (default `gpt-6-luna`). If that target is not in the live catalogue, the other target is used; if neither is, `model_not_found` is returned as before. Every affected response, including errors, carries `X-AI-Router-Model-Fallback: <requested> -> <served>`, with the requested ID reduced to identifier characters and at most 128 characters. Response bodies report the served ID. `/v1/models` lists only real catalogue IDs. `AI_ROUTER_MODEL_FALLBACK=false` restores strict exact-ID behaviour. |
| Readiness | Catalogues are usable only while their provider is enabled, authenticated, and checked within 180 seconds. Account generation and persistent restrictions are checked at admission and before dispatch. |
| Antigravity | Direct Rust HTTP/SSE adapter using a fixed reviewed origin, pinned identity, and reviewed protocol baseline. Text generation, history, client tools, and validated JSON are in scope. |
| OpenAI | Independent opt-in Responses adapter. `AI_ROUTER_OPENAI_ENABLED` defaults to false; OpenAI state and admission are separate from Antigravity. |
| Client tools | The gateway validates tool names, object arguments, and the full JSON Schema before returning calls to the client. The client runs tools and sends results in later history. The gateway never executes client tools. |
| Tool choice | `auto` and `none` are supported. `none` omits the tool inventory; any unexpected returned call fails. Forced or named selection is unsupported. |
| Parallel calls | The gateway may return multiple validated calls when the request permits them. It does not execute tools or promise parallel execution. |
| JSON Schema output | Chat `response_format` and Responses `text.format` schemas are validated locally. Provisional text is withheld until a complete value passes validation. Provider constrained decoding is not promised. |
| History | Typed chronological user, assistant, and tool turns retain original call IDs and results. Private Antigravity reasoning/signature replay is opaque, memory-only, and scoped by client, model, and account generation. |
| Responses item references | Completed items are client-scoped in process memory, with a 24-hour TTL, 1,024 items / 16 MiB per client and 4,096 items / 64 MiB globally. Reasoning references also require the same provider and account generation. Restart clears items. |
| Usage | Prompt, candidate, reasoning, cached, and total counts are mapped where supplied. Missing or inferred usage is marked partial. Responses retain both `agy_usage_partial` and `ai_router_usage_partial` for compatibility. |
| Responses `include` | Only `["reasoning.encrypted_content"]` (or `[]`) is accepted. OpenAI always requests and returns encrypted reasoning items; Antigravity returns no reasoning items, so the hint adds nothing there. Any other value returns `unsupported_parameter`. Chat Completions accepts only an empty `include`. |
| Reasoning controls | Responses `reasoning.effort` is supported (`low` to `max`; Antigravity requires the exact model ID's effort). When a fallback serves an Antigravity model, its ID fixes the tier: a different client effort is replaced by that tier and disclosed as `X-AI-Router-Reasoning-Effort: <tier>`, instead of the conflict error that an exact ID receives. A fallback to OpenAI forwards the client's effort unchanged. `reasoning.summary` `auto`, `concise` or `detailed` is accepted because the Vercel AI SDK adds `detailed` whenever an effort is set, but summaries are not produced: reasoning items carry `summary: []`. Requests with a summary receive `X-AI-Router-Reasoning-Summary: unavailable`, and capabilities report the same. Other reasoning fields are rejected. |
| Output budgets | Positive Chat and Responses budgets become best-effort visible-answer guidance. They are not hard output, reasoning, or usage limits. The response header and capabilities disclose this mode. |
| Media | Image, audio, video, and other non-text input are unsupported. |
| Native execution | Removed. `/v1/agy/runs` and all child routes return 404. Only `model` is a valid client-key scope. |
| Conversations | The router does not expose provider conversation IDs or resume a provider-side session. Submit complete history or use supported Responses item references. |

The OpenAI provider remains available when Antigravity is disabled or
restricted, and the reverse is also true. `GET /v1/ready` succeeds when at least
one enabled provider has a ready model. `/v1/models` contains only exact IDs
from currently usable catalogues. Antigravity additionally requires membership
in the 13-ID reviewed baseline; unreviewed image, internal and future SKUs are
withheld until their contracts are reviewed. Previously observed but unavailable IDs
return an actionable provider-unavailable error instead of routing elsewhere.
Model fallback applies only to IDs never observed, so it cannot hide such an
outage. `/v1/capabilities` reports the policy under `profiles.model.model_fallback`,
and monitor request records keep the requested ID beside the served model.

The provider admission limits are independent. `AI_ROUTER_MAX_CONCURRENT` and
`AI_ROUTER_MAX_QUEUE` default to 2 and 8 for Antigravity;
`AI_ROUTER_OPENAI_MAX_CONCURRENT` and `AI_ROUTER_OPENAI_MAX_QUEUE` default to 2
and 8 for OpenAI. Their active and queued ceilings add together. Global body,
pre-authentication, and client-key limits remain shared.

Monitor `startup_ms` measures elapsed time to the provider adapter's `Init`
event, emitted after accepted upstream streaming response headers for both
providers. It includes request setup, queueing, and authentication through that
point; it does not measure local process startup alone. `first_output_ms`
records the first text delta or validated tool call, excluding private
reasoning. Schema buffering can delay client delivery beyond the underlying
provider event. Aggregate timing and token totals combine both providers, while
each request record retains its provider ID.

The monitor retains rejection counters for the current process separately from
stored request history. Counters include every rejection since `started_at` and
reset on restart; the recent rejection history is sampled to at most 30 records
per minute. Range filters change retained-history aggregates but not those
counters. Persisted telemetry contains bounded metadata only. The private
monitor socket can also retrieve up to 100 recent request/response bodies in
memory, each capped at 64 KiB and redacted for known credential fields; those
details are cleared on restart and are not persisted.
