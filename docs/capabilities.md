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
| Responses `include` | Only `["reasoning.encrypted_content"]` (or `[]`) is accepted. OpenAI always requests and returns encrypted reasoning items; Antigravity returns no reasoning items, so the hint adds nothing there. Any other value returns `unsupported_parameter`. Chat Completions accepts only an empty `include`. |
| Reasoning controls | Responses `reasoning.effort` and Chat `reasoning_effort` accept `none`, `minimal`, `low`, `medium`, `high`, `xhigh` and `max`. OpenAI receives `none` unchanged and decides per model. OpenAI also receives `minimal` unless the discovered catalogue lists the model's `supported_reasoning_levels` without it; then the lowest listed level above `minimal` is sent and `X-AI-Router-Reasoning-Effort: mapped=<level>` discloses it. An Antigravity model ID fixes its effort: `none` and `minimal` mean "no explicit effort" there (`X-AI-Router-Reasoning-Effort: ignored`), while any other effort must match the ID's suffix or the request fails. `reasoning.summary` `auto`, `concise` or `detailed` is accepted because the Vercel AI SDK adds `detailed` whenever an effort is set, but summaries are not produced: reasoning items carry `summary: []`. Requests with a summary receive `X-AI-Router-Reasoning-Summary: unavailable`, and capabilities report the same. Other reasoning fields are rejected. |
| Sampling hints | `temperature` (0 to 2), `top_p` (0 to 1), integer `seed`, and `frequency_penalty`/`presence_penalty` (-2 to 2) are accepted on both APIs. They are range-checked, never forwarded, and have no effect; out-of-range or mistyped values return 400 `invalid_request`. Responses with any of them carry `X-AI-Router-Sampling-Controls: ignored`. Active controls remain `unsupported_parameter`: `n` other than 1, `logit_bias`, `logprobs: true`, `top_logprobs` above 0, and a non-empty `stop`. |
| No-op request fields | Both APIs accept `metadata` (at most 16 string values, 64-byte keys, 512-byte values), `user`, `safety_identifier` and `service_tier` `auto`/`default`; `flex`, `priority` and `scale` return `unsupported_parameter`. Responses also accept `truncation: "auto"` and `stream_options: {"include_obfuscation": false}`. The router never truncates history, so an oversized conversation fails at the provider's context limit instead. None of these reach a provider. Responses echo `metadata` (or `{}`) on the response object, including stream events. `X-AI-Router-Ignored-Fields` lists the accepted fields that had no effect. |
| History replay tolerance | A Responses `reasoning` item without `encrypted_content` cannot be used by either provider; it is dropped and `X-AI-Router-Reasoning-Replay: dropped` is returned. A replayed Responses assistant `refusal` part becomes assistant text, as a Chat `refusal` already does. Chat assistant `annotations` (empty or `url_citation`), assistant `reasoning_content` and content-part `cache_control` are accepted, removed, and named in `X-AI-Router-Ignored-Fields` (empty annotations are not named). |
| Retry guidance | Validation errors (4xx other than 408, 409 and 429), invalid or incomplete output shapes (`invalid_structured_output`, `incomplete_structured_output`, `invalid_provider_tool_call`, `incomplete_provider_output`, `output_limit_exceeded`) and durable provider restrictions return `x-should-retry: false`, so official SDKs do not repeat a paid request. Other 503s return `retry-after`: 10 seconds for local saturation (`busy`, `shutting_down`), 30 seconds for provider conditions such as overload. An OpenAI overload, transport failure or upstream 401/403/5xx latches the provider until the next catalogue probe (at most 60 seconds while healthy); requests in that window get the same 503 locally, with `retry-after`, and make no upstream call. Local queue and body-slot saturation is 503 `busy`, not 429, because n8n never retries 429. Rate limits stay 429 with `retry-after: 10`. Upstream 401/403 responses become 503 with the provider's explanatory code, so SDKs do not report the router key as wrong. |
| Streaming errors | A stream's HTTP 200, first Chat chunk and `response.created` wait for the provider's acceptance (its `Init` event, after accepted upstream response headers). Failures before then (overload, quota, authentication, timeout) return the same HTTP status, error body and headers as a nonstreaming request, as on OpenAI. The wait is bounded at 15 seconds, the keepalive interval; a provider still silent then gets a committed stream. Later Responses failures emit the spec `error` event (`code`, `message`, `param: null`, `sequence_number`, plus a top-level `error` object, which official SDKs and LangChain raise on), then `response.failed`. Later Chat failures emit a chunk with a top-level `error` object, then `[DONE]`. |
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
