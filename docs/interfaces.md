# Provider interfaces

`ai-router` is one Rust crate with the model API, provider registry, shared
protocol, independent Antigravity and OpenAI adapters, and monitor. Current
provider integration is under review; this document describes the code's
interfaces. The approved pinned OMP sign-in succeeded; catalogue retry and
live model validation remain pending.

## Configuration and client authentication

`Config::from_env() -> Result<Config, String>` reads process environment only.
It does not load dotenv or configuration files. `Config::for_test(root)` uses
synthetic key digests and separates Antigravity, OpenAI, and telemetry paths.

Main configuration fields and defaults:

| Field | Default |
| --- | --- |
| `bind` | `0.0.0.0:8080` |
| `state_dir` | `/var/lib/ai-router/antigravity-auth` |
| `antigravity_enabled` | `true` (`AI_ROUTER_ANTIGRAVITY_ENABLED`) |
| `openai_enabled` | `false` (`AI_ROUTER_OPENAI_ENABLED`) |
| `openai_state_dir` | `/var/lib/ai-router/openai-auth` |
| `telemetry_dir` | `/var/lib/ai-router/telemetry` |
| `monitor_socket` | `/run/ai-router/monitor.sock` |
| body / output bounds | 1 MiB / 8 MiB |
| concurrent / queue per provider | 2 / 8 |
| request deadline | 120 seconds |
| telemetry retention | 30 days / 50,000 events |
| `model_fallback` | `true` (`AI_ROUTER_MODEL_FALLBACK`) |
| `fallback_openai_model` | `gpt-6-luna` (`AI_ROUTER_FALLBACK_OPENAI_MODEL`) |
| `fallback_gemini_model` | `gemini-3.8-flash-medium` (`AI_ROUTER_FALLBACK_GEMINI_MODEL`) |

Antigravity, OpenAI, and telemetry paths must be separate absolute directories;
configured paths reject protected filename and directory patterns. The monitor
socket must remain outside those storage paths. Fallback targets must be 1–128
ASCII letters, digits, dots, colons, underscores, or hyphens.

`KeyConfig { id, sha256, scopes }` stores digest records in `AI_ROUTER_KEYS`.
Only the `model` scope is valid. `auth::generate_key(id, scopes)` creates a
random API key and its digest record; `authenticate` uses a constant-time digest
comparison. The independent Antigravity OAuth configuration reads
`AI_ROUTER_GOOGLE_CLIENT_ID` and optional `AI_ROUTER_GOOGLE_CLIENT_SECRET` from
process environment, using the reviewed pinned registration when unset. Neither
auth lifecycle reads an auth file from the other provider.

## Shared model protocol

`protocol::RunRequest` is the normalized input shared by both providers:

```text
owner: String
parallel_tool_calls: bool
remaining_ms: Option<u64>
input: Vec<Value>                 chronological typed conversation history
instructions: Vec<Value>          leading system/developer instructions
provider_generation: Option<String>
endpoint: String                  "chat" or "responses"
request_id, model: String
tools: Vec<ToolDefinition>
schema: Option<Value>
effort: Option<String>
ignored: Vec<String>              accepted no-op fields, for disclosure only (not serialized)
```

Both provider serializers consume `instructions` and typed `input`; no
rendered transcript or CLI-era profile is carried in `RunRequest`. Normalization
bounds the UTF-8 JSON encoding of the combined typed history and instructions to
1 MiB. Chat history retains assistant tool calls as messages with original IDs and JSON-string arguments, followed by
matching `tool` results. Responses history retains typed `message`,
`function_call`, `function_call_output`, and encrypted `reasoning` items. System
and developer messages that precede the conversation remain instructions;
later turns retain their original chronology.

`normalize_chat` and `normalize_response` validate supported fields, text-only
content, call/result pairing, identifiers, and bounded item counts. They
support tool choice `auto` and `none`; `none` removes the tool inventory.
Forced/named choices and active sampling controls (`n` other than 1,
`logit_bias`, `logprobs`, `top_logprobs`, non-empty `stop`) fail explicitly.
Sampling hints (`temperature`, `top_p`, `seed`, `frequency_penalty`,
`presence_penalty`) are range-checked and recorded in `ignored`, as are
`metadata`, `user`, `safety_identifier`, `service_tier` `auto`/`default` and
Responses `truncation: "auto"`; none of them changes the rest of `RunRequest`.
Replay tolerance also records into `ignored`: Responses `reasoning` items
without `encrypted_content` are dropped, Chat assistant `annotations` and
`reasoning_content` and content-part `cache_control` are removed, and Responses
assistant `refusal` parts become `output_text`. Effort accepts `none` and
`minimal`; the API applies the provider policy after model resolution
(Antigravity clears both, OpenAI may replace `minimal` with the lowest
catalogue-listed level) and discloses changes in response headers.
Responses accept `include: ["reasoning.encrypted_content"]` and a
`reasoning.summary` hint of `auto`, `concise` or `detailed`; neither changes
`RunRequest`, and summaries are disclosed as unavailable. Other include values
and any non-empty Chat `include` remain `unsupported_parameter`.
Positive output budgets add visible-answer guidance only. They do not cap
provider tokens or truncate text, schema output, or tool arguments.

`ApiError::retry_header` gives retry guidance on JSON error responses:
`x-should-retry: false` for validation errors, deterministic output-shape
failures and durable provider restrictions, and `retry-after` for other 503s.
The API layer adds `retry-after` to 429 rate limits that are not durable.

Shared types include `ToolDefinition`, `ToolCall`, `Usage`, `RunResult`, and
`RunEvent`. `RunResult::usage_partial` carries incomplete or inferred usage.
Chat and Responses output continue to expose both `agy_usage_partial` and
`ai_router_usage_partial`. `RunEvent` contains `Init`, `TextDelta`, `ToolCall`,
`Completed`, and `Error`; there is no native event.

`protocol::validate_schema` enforces schema byte/structure bounds, meta-schema
validity, compilation, and local-only references. `tool_diagnostics::validate_call`
is shared by the direct provider, OpenAI adapter, and client-facing API. It
validates the declared tool, object arguments, and caller schema while producing
bounded diagnostics without logging raw argument values. Keep these helpers
even though the MCP relay was removed.

## Provider ownership and dispatch

`providers::ProviderId` retains persisted IDs `Agy` (`"agy"`) and `OpenAi`
(`"openai"`). Ownership is exact and never inferred from model prefixes. The
registry withholds conflicting IDs and remembers up to 4,096 last-known owners
so a hidden provider model reports unavailable instead of being routed to the
other provider. Readiness requires authentication and a catalogue no older than
180 seconds.

`Snapshot::fallback(requested, openai_target, gemini_target)` is the only
prefix rule. It returns a target only for an ID that is not live, conflicting,
or remembered: the Gemini target for a case-insensitive `gemini` prefix,
otherwise the OpenAI target, and the other target when the first is not live.
The API rewrites the request's `model` to the served ID before normalization,
so ownership, item scope, response bodies and telemetry use the served ID.
`RequestRecord.requested_model` keeps the sanitized requested ID (omitted when
no fallback applied), and the protect middleware adds
`x-ai-router-model-fallback` to every response for that request. For an
Antigravity target, a differing client effort is cleared before serialization
and `x-ai-router-reasoning-effort` reports the served tier.

Antigravity's `probe`, `generation`, `availability_error`, `model_availability`,
and `start` methods feed the same API registry and provider-specific admission
used by OpenAI. The API pins the account generation before expanding Responses
item references, admits against the selected provider's own queue/semaphore,
then re-resolves exact ownership and generation after queueing. Provider
dispatch rechecks persistent restriction and account-generation state. There
is no CLI or alternate-provider fallback.

`AntigravityTransport` centralizes authenticated HTTP dispatch to fixed reviewed
origins and operation paths. Redirects are disabled; request, response, event,
and elapsed-time bounds apply across the original request deadline. The direct
adapter uses a bounded SSE decoder and emits one terminal event. It validates
complete tool calls before publication and validates complete structured JSON
against the shared schema. Its private reasoning/signature replay cache is
memory-only and keyed by client, exact model, and account generation, with a
24-hour TTL and per-client/global byte bounds.

`openai::OpenAi` and `openai_auth` keep their own state directory, auth
lifecycle, generation, provider probe, and Responses transport. The probe also
records each model's catalogue `supported_reasoning_levels`, which
`OpenAi::minimal_effort_substitute` uses for the `minimal` effort policy. Disabling or
restricting either provider does not disable the other. `provider_stream::RunStream`
holds an event receiver and a `finished` cancellation token used by API
completion/drain handling.

## Responses items and API

`items::ItemStore` keeps exact completed Responses output items in bounded
process memory. Ownership is the configured client ID. Reasoning references
also require the same `provider:generation` scope; ordinary visible items remain
safe to replay across providers for that client. Unknown, foreign, expired,
evicted, and wrong-generation reasoning references return the same
`item_reference_not_found` response. Storage is cleared at restart.

The router exposes `/health`, `/ready`, `/v1/models`, `/v1/capabilities`,
`/v1/chat/completions`, and `/v1/responses`. The removed `/v1/agy/runs` route
and its children fall through to 404. All protected routes require a model-scoped
client key. Capabilities currently report schema version 3 and explicit
unsupported features.

Streaming requests commit their SSE response only after the provider's first
`RunEvent` (normally `Init`, sent after accepted upstream response headers) or
after `STREAM_COMMIT_WAIT` (15 seconds, the keepalive interval). A provider
`Error` before that returns the nonstreaming `ApiError` response, so status,
body, `retry-after` and telemetry classification match the nonstreaming path.
After commit, `StreamRenderer` turns an `Error` into a Responses `error` event
followed by `response.failed`, or a Chat chunk with a top-level `error` object
followed by `[DONE]`. The `error` event carries the OpenAPI `ResponseErrorEvent`
fields and the same nested `error` object as the HTTP body (with `param: null`),
because openai-node and openai-python raise only on that object. A client that
disconnects before or after commit drops the same `CompletionGuard`, which
cancels the provider and records `client_disconnected`.

## Monitor and Docker boundary

`monitor::Telemetry` stores retained request metadata separately from a
memory-only bounded request/response detail cache. The Unix socket is private
and read-only. Persisted records include provider, endpoint, timings, sanitized
error code, and observed usage. `startup_ms` measures elapsed time through
accepted upstream streaming response headers for either provider; `first_output_ms`
measures the first text delta or validated tool call. Private reasoning is not
counted as first output. The six rejection counters count all attempts since
process start; retained rejection records are sampled to 30 per minute.

The Compose gateway runs nonroot with a read-only root filesystem, dropped
capabilities, and tmpfs `/run/ai-router` and `/tmp`. It mounts
`ai-router-antigravity-auth`, `ai-router-openai-auth`, and
`ai-router-telemetry` independently. The prior `ai-router-auth` CLI volume is
preserved unmounted. No CLI binary, native sandbox, workspace runner, or MCP
relay is installed in the gateway image. Compose commands use
`--env-file /dev/null` to prevent implicit dotenv loading.
