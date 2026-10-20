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
| Provider routing | Exact discovered model IDs map to one provider. No prefix inference, provider fallback, or account fallback. Conflicting IDs are withheld. |
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
