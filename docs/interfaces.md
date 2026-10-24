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
| `tool_call_relay` | `true` (`AI_ROUTER_TOOL_CALL_RELAY`) |
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
tool_required: bool               tool_choice demands a call
tool_relay: bool                  tool_choice absent or auto; noncompliant non-strict calls may be relayed
```

`ToolDefinition.strict` records a client `strict: true`. `ToolCall.relayed`
holds the sanitized validation rejection when a call was handed to the client
without passing validation; it is never part of a public response.

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
support tool choice `auto`, `none`, `required`, a named function and
`allowed_tools`. `none` removes the tool inventory; named and allowed-tools
choices narrow it to the selection; `required`, a named function and
`allowed_tools` with `mode:"required"` set `RunRequest.tool_required`. With that
flag, both adapters withhold visible text until the result is known
(`RunRequest::withholds_text`, shared with schema output), and fail with
`502 missing_required_tool_call` when no validated call is returned. OpenAI
also receives `tool_choice:"required"`; Antigravity keeps `VALIDATED` mode and
receives a system instruction naming the remaining tools. Tool-choice
validation errors carry `param:"tool_choice"` (`ApiError::with_param`), and a
forcing choice cannot be combined with a structured output format.
Active sampling controls (`n` other than 1, `logit_bias`, `logprobs`,
`top_logprobs`, non-empty `stop`) fail explicitly.
Sampling hints (`temperature`, `top_p`, `seed`, `frequency_penalty`,
`presence_penalty`) are range-checked and recorded in `ignored`, as are
`metadata`, `user`, `safety_identifier`, `service_tier` `auto`/`default` and
Responses `truncation: "auto"`; none of them changes the rest of `RunRequest`.
Replay tolerance also records into `ignored`: Responses `reasoning` items
without `encrypted_content` are dropped, Chat assistant `annotations` and
`reasoning_content` and content-part `cache_control` are removed, and Responses
assistant `refusal` parts become `output_text`. Effort accepts `none` and
`minimal`; the API applies the provider policy after model resolution
(Antigravity clears both, OpenAI may replace `none` or `minimal` with the
lowest catalogue-listed level above it, and a fallback-served Antigravity ID
keeps its own tier) and discloses changes in response headers.
Responses accept `include: ["reasoning.encrypted_content"]` and a
`reasoning.summary` hint of `auto`, `concise` or `detailed`; neither changes
`RunRequest`, and summaries are disclosed as unavailable. Other include values
and any non-empty Chat `include` remain `unsupported_parameter`.
Positive output budgets add visible-answer guidance only. They do not cap
provider tokens or truncate text, schema output, or tool arguments.

`ApiError::retry_header` gives retry guidance on JSON error responses:
`x-should-retry: false` for validation errors, deterministic output-shape
failures, durable provider restrictions, errors marked `durable` (upstream
401/403 credential rejections) and `504 provider_timeout`, and `retry-after`
for other 503s. The API layer adds `retry-after` to 429 rate limits that are
not durable. `IntoResponse` also stores the code as a `protocol::ErrorCode`
response extension, which the protect middleware uses for the monitor bucket
(`Telemetry::note_error`, falling back to the HTTP status) and the request
record's `error_code`.
`ApiError::new` is the only constructor (`api::error` delegates to it);
`with_param` sets the optional `param` that `response_body` serializes, and
`missing_required_tool_call` carries `param:"tool_choice"` like tool-choice
validation errors.

Shared types include `ToolDefinition`, `ToolCall`, `Usage`, `RunResult`, and
`RunEvent`. `RunResult::usage_partial` carries incomplete or inferred usage.
Chat and Responses output continue to expose both `agy_usage_partial` and
`ai_router_usage_partial`. `RunEvent` contains `Init`, `TextDelta`, `ToolCall`,
`Completed`, and `Error`; there is no native event.

`protocol::validate_schema` enforces schema byte/structure bounds, meta-schema
validity, compilation, and local-only references. `tool_diagnostics::validate_call`
is shared by the direct provider, OpenAI adapter, and client-facing API. It
validates the declared tool, object arguments, and caller schema while producing
bounded diagnostics without logging raw argument values. Both adapters call it
through `tool_diagnostics::admit_call`, which applies the relay policy: with
`RunRequest.tool_relay` (cleared by `AI_ROUTER_TOOL_CALL_RELAY=false`), an
`unknown_tool` rejection while no declared tool is strict, or a
`schema_mismatch` against a non-strict tool, is admitted and returned as
`ToolCall.relayed`; every other rejection fails with `invalid_provider_tool_call`.
`tool_diagnostics::schema_errors` produces the same value-free error summaries
for structured-output failures. Keep these helpers even though the MCP relay was
removed.

Adapter failures carry `FailureDiagnostics`: `metadata` (phase, reason and
value-free details) is logged, and `private_body` (the offending provider item,
withheld output or upstream error body) goes only to the memory-only monitor
capture. `crate::log` writes the structured log events described in
[logging](logging.md).

## Provider ownership and dispatch

`providers::ProviderId` retains persisted IDs `Agy` (`"agy"`) and `OpenAi`
(`"openai"`). Ownership is exact and never inferred from model prefixes. The
registry withholds conflicting IDs and remembers up to 4,096 last-known owners
so a hidden provider model reports unavailable instead of being routed to the
other provider. `Registry::open` restores that map from
`known-models.json` in the telemetry directory and rewrites it (0600, atomic
rename) whenever ownership changes; an unreadable or corrupt file starts empty. Readiness requires authentication and a catalogue no older than
180 seconds.

`Snapshot::fallback(requested, openai_target, gemini_target)` is the only
prefix rule. It returns a `FallbackTarget` only for a valid chat model ID that
is not live, conflicting, remembered or itself a target: the Gemini target for
a case-insensitive `gemini` prefix, otherwise the OpenAI target, and the other
target when the first is not live (`crossed`). With neither live it still
returns the first remembered target, so `resolve` reports that target's
availability error; with neither ever seen it returns `None`. A non-chat model
family is an `Err(400 model_not_found)`, and a target requested directly that
no catalogue has published is an `Err(503 provider_unavailable)`. A crossed
fallback to Antigravity refuses Responses input that replays reasoning
(`ItemStore::carries_reasoning`) with 503 `provider_unavailable`.
The API rewrites the request's `model` to the served ID before normalization,
so ownership, item scope, response bodies and telemetry use the served ID.
`RequestRecord.requested_model` keeps the sanitized requested ID (omitted when
no fallback applied), and `x-ai-router-model-fallback` is recorded on the
request context as soon as the fallback is chosen.

`api::apply_effort_policy` makes the single effort decision after
normalization, against the served model, and returns at most one
`x-ai-router-reasoning-effort` value: Antigravity `none`/`minimal` are cleared
(`ignored`); a fallback-served Antigravity ID clears a differing effort
(`mapped=<tier>`); OpenAI `none` or `minimal` that the catalogue lists levels
without becomes the lowest listed level above it (`mapped=<level>`). Other efforts are forwarded, and the Antigravity adapter
still rejects an exact ID's conflicting effort.

Disclosures are recorded with `RequestContext::disclose` once the request has
validated: the effort value, `disclosure_headers(&request.ignored)` (sampling,
replay and ignored fields), reasoning summary, prompt-cache hints, token-budget
mode and Responses item storage. The protect middleware inserts every recorded
header into the final response, so success, streamed, pre-commit failure and
nonstreaming failure responses carry the same set, each header at most once.

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

Streaming requests commit their SSE response only once `StreamRenderer` has
client-visible output beyond `response.created`/`in_progress` (or the Chat role
chunk): a text delta, a tool call or the terminal result. `Init` renders
nothing, so commit is never earlier than provider acceptance. Commit also
happens after `STREAM_COMMIT_WAIT` (15 seconds, the keepalive interval). Every
event before commit goes through the same `settle_stream_event` as after it
(result validation, item storage, telemetry), and an `Error` before commit,
including withheld schema output that fails validation and a missing required
tool call, returns the nonstreaming `ApiError` response, so status, body,
retry headers, disclosures and telemetry classification match the nonstreaming
path. After commit, `StreamRenderer` turns an `Error` into a Responses `error`
event followed by `response.failed`, or a Chat chunk with a top-level `error`
object followed by `[DONE]`. The `error` event carries the OpenAPI
`ResponseErrorEvent` fields and the same nested `error` object as the HTTP body,
with `param` set from the error (or `null`), because openai-node and
openai-python raise only on that object. A client that disconnects before or
after commit drops the same `CompletionGuard`, which cancels the provider and
records `client_disconnected`.

## Monitor and Docker boundary

`monitor::Telemetry` stores retained request metadata separately from a
memory-only bounded request/response detail cache. The Unix socket is private
and read-only. Persisted records include provider, endpoint, timings, sanitized
error code, and observed usage. `startup_ms` measures elapsed time through
accepted upstream streaming response headers for either provider; `first_output_ms`
measures the first text delta or validated tool call. Private reasoning is not
counted as first output. The six rejection counters count all attempts since
process start; retained rejection records are sampled to 30 per minute.
Each captured request or response is bounded to 128 KiB: long strings are
shortened first (8 KiB, then 2 KiB, 512 and 128 bytes, each marked with the
omitted size) so the capture stays valid JSON with its tool list and item order,
and only then is the text cut. `ai-router monitor --detail <request-id>` prints
one retained capture, including a failure's `diagnostics` and private
`upstream_response`.

The Compose gateway runs nonroot with a read-only root filesystem, dropped
capabilities, and tmpfs `/run/ai-router` and `/tmp`. It mounts
`ai-router-antigravity-auth`, `ai-router-openai-auth`, and
`ai-router-telemetry` independently. The prior `ai-router-auth` CLI volume is
preserved unmounted. No CLI binary, native sandbox, workspace runner, or MCP
relay is installed in the gateway image. Compose commands use
`--env-file /dev/null` to prevent implicit dotenv loading.
