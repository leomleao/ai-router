# AGY and ChatGPT subscription routing implementation plan

Status: deterministic implementation and acceptance passed locally: 167 Rust
tests (127 library, 25 HTTP, 15 runner), client SDK checks, AGY loopback, monitor
PTY, and Docker policy. Formatting passed; Clippy passed with the same three
existing warnings. The final production image was rebuilt and started locally with OpenAI
enabled on port 8091. OAuth/discovery found 7 OpenAI and 13 AGY model IDs (20
total), and state persisted across container recreations. The strict SSE decoder
handles observed events without a Content-Type header, and bounded item
reconstruction fixed empty final tool output. Live `gpt-6-luna` Chat and
Responses streaming and nonstreaming, Chat and Responses client-tool
continuations, and validated JSON schema passed. Real rotating refresh and full
installed-client checks have not been performed. Integration-category review
is not independently attested. Research baseline:
2026-10-20. Execution dependencies and acceptance evidence are defined in [the
implementation DAG](openai-subscription-dag.md).

## Intended behavior

Run AGY and OpenAI together behind the existing gateway. Clients retain their
gateway API keys, base URL, Chat Completions endpoint, and Responses endpoint.
The requested model ID selects the provider. There is no provider selector,
model prefix requirement, model substitution, or automatic billing fallback.

`GET /v1/models` returns the union of models from providers currently available
for model requests. OpenAI models appear after the operator enables the provider,
completes ChatGPT authorization, and account-specific discovery succeeds. They
disappear when that provider becomes unavailable. AGY continues independently.
OpenAI access uses the official ChatGPT plan OAuth flow and public Responses API.

This document extends the AGY architecture in [PLAN.md](../PLAN.md). It records
the implementation and remaining validation work. Deployment, independent
integration-category attestation, rotating-refresh verification, and full-client
acceptance remain operator-controlled gates.

## Implementation checkpoint and remaining verification

| Component | Current implementation |
| --- | --- |
| `src/api.rs`, `src/providers.rs` | Exact-ID provider registry/dispatch, separate availability, bounded admission, provider-aware readiness and capabilities. Deterministic integration checks and all four live Chat/Responses streaming modes passed |
| `src/protocol.rs`, `src/openai.rs` | Typed conversation input, OpenAI Responses serialization/SSE, tool and error mapping. The strict SSE decoder handles observed missing Content-Type; bounded item reconstruction preserves completed tool output. Live Chat/Responses tool continuations and validated JSON schema passed |
| `src/runner.rs` | AGY adapter keeps its existing process, cancellation, workspace, and inert tool contracts |
| `src/items.rs`, `src/api.rs` | Provider-provenanced Responses item storage preserves encrypted reasoning for OpenAI replay; OpenAI Chat uses a bounded owner-scoped in-memory replay cache |
| `src/monitor/` | Separate AGY/OpenAI snapshots, provider-tagged request totals, legacy AGY defaults, and synthetic OAuth redaction coverage |
| `src/main.rs`, `src/config.rs`, Docker and operator scripts | OpenAI opt-in, separate state volume, operator OAuth controls, and independent probes; Docker policy and deterministic acceptance passed. Live OAuth/discovery succeeded with 7 OpenAI and 13 AGY IDs and survived container recreation; integration-category attestation remains pending |
| `src/native.rs` | Native routes remain AGY-only and must stay disabled whenever OpenAI is enabled until containment covers its state volume |

## Provider and routing contract

Introduce a provider ID enum (`agy`, `openai`) and a shared model-provider
interface with discovery/status, capability lookup, and cancellable execution.
Use a provider-neutral event/result contract derived from `RunEvent` and
`RunResult`. OpenAI transport must not need an AGY process, relay, or workspace.
Represent cleanup explicitly so admission is released only after the selected
provider finishes teardown. Preserve AGY process-group cancellation.

Each model registry entry holds the exact public ID, provider ID, provider model
ID, display name, capabilities, account-registration generation, and catalogue
generation. Public IDs are unchanged provider IDs. OpenAI discovery reads the
account-specific `models` array, keeps `visibility: "list"`, and maps `slug` to
the public model ID. Do not assume the API-key model-list response shape.

Build a validated snapshot and swap it atomically. Sort providers consistently
and preserve each provider's discovery order. Set `owned_by` appropriately.
There is no hardcoded assumption that every `gpt-*` ID belongs to OpenAI.

Request processing is:

1. Authenticate and apply existing body, header, and request-rate bounds.
2. Resolve the exact model once against the current registry.
3. Resolve client-owned stored items and normalize typed input under that
   model's capabilities. Preserve instruction order and tool-call/result IDs.
4. Enter bounded global and selected-provider admission, recheck availability
   and account generation, and dispatch to the pinned provider/model.
5. Map provider output, usage, cancellation, and errors to the existing client
   protocol. Record the provider in telemetry and release resources on teardown.

A catalogue refresh never reroutes an admitted request. Account switching or
logout invalidates queued requests bound to the old registration. Already-started
requests are cancelled on explicit logout. Ordinary discovery updates do not
interrupt otherwise valid in-flight requests.

Current catalogues are expected to be disjoint. If a future snapshot contains a
duplicate public ID, withhold that ID from both providers and report a collision
in private status. Requests for it return `model_conflict`; unrelated models
remain usable. Never pick a provider by registration order or silently add aliases.
Unknown IDs return `model_not_found`; retain a bounded last-known ownership map
for temporarily hidden IDs so cached clients receive `provider_unavailable` or
the applicable quota/authentication error without dispatching elsewhere.

## Availability and admission

Track configuration, credential readiness, catalogue freshness, transport health,
and inference restrictions separately. Suggested states are `disabled`,
`auth_required`, `discovering`, `ready`, `unavailable`, `usage_blocked`, and
`ineligible`, with sanitized reasons and timestamps.

| Situation | Catalogue and recovery behavior |
| --- | --- |
| OpenAI disabled or not signed in | No OpenAI models; AGY starts and serves normally |
| Valid grant and successful discovery | Publish discovered OpenAI models; eligibility/inference remains unverified until a request succeeds |
| Transient discovery/network failure | Mark provider unavailable and hide its models immediately; retry control-plane probes with bounded exponential backoff and jitter |
| No successful refresh within freshness limit | Hide expired entries even if a probe hangs; retain last-known IDs privately |
| Confirmed revoked/terminally invalid grant | Hide provider, stop refresh loops, require operator login |
| Explicit subscription usage limit | Persist the provider-generation block, hide OpenAI models, and pause new inference; discovery success alone does not clear it |
| Eligibility/permission denial | Hide OpenAI models and retain diagnostic state until configuration/authorization is corrected |
| Bad request or unsupported capability | Fail that request; do not mark all models unhealthy |

The implemented discovery interval is 60 seconds, with a 180-second maximum
snapshot age. Probes have a 10-second deadline; transient retry delay starts at
5 seconds, doubles to a 60-second ceiling, and adds jitter. The API metadata lock
gate returns within 100 ms when provider state cannot be read promptly. Status
publication and catalogue pruning do not wait on the other provider's probe.
Discovery is a control-plane check, never a periodic generation prompt. Listing
a model means known availability, not a guarantee of quota for the next request.

After a quota block, honor a reliable provider retry/reset signal when supplied.
Without one, persist the block until explicit operator `resume` after reviewing
ChatGPT Usage; do not invent a reset time or repeatedly spend inference to probe
recovery. Resume revalidates the grant and catalogue; the next requested
inference can block it again. Separate quota errors from ordinary short-lived
rate limits.

`/health` remains minimal process liveness. `/ready` returns success when at least
one enabled provider has at least one routable model; otherwise return 503. It
does not expose account details. `/v1/models` remains a valid list, possibly empty.
Authenticated capabilities and the private monitor explain partial availability.

The existing `AI_ROUTER_MAX_CONCURRENT` and `AI_ROUTER_MAX_QUEUE` are the AGY
active and waiting budgets. OpenAI has separate `AI_ROUTER_OPENAI_MAX_CONCURRENT`
and `AI_ROUTER_OPENAI_MAX_QUEUE` budgets, defaulting to 2 and 8. The combined
active/queued ceiling is the sum of provider budgets; body, output, deadline,
and request-rate bounds remain gateway-wide. A stalled provider must not reserve
the other provider's admission slots. All queue waits consume the original
request deadline. Deterministic verification must test stalls in both directions.
Provider health failures must not terminate the gateway or consume the other
provider's retry budget.

## Typed input and client compatibility

Separate provider-neutral parsing from provider serialization. Preserve messages,
instructions, function declarations, call/result IDs, requested formatting,
reasoning effort, output-budget hints, and cache hints as structured values.
Only the AGY serializer renders its current transcript prompt and MCP catalogue.
Retain AGY contract tests through this refactor before adding OpenAI execution.

OpenAI receives native Responses input, with system instructions mapped to
`instructions` or developer messages. Convert Chat Completions history and tools
to that format. OpenAI function calls return directly to clients; the server does
not execute them or use AGY's inert relay. Validate returned names/arguments
against the request catalogue. Support multiple returned calls with complete
call/result replay, including supported Responses `additional_tools` supplied as
developer items; do not carry AGY's one-call restriction into OpenAI metadata.

Initially support text, both client streaming modes, client function tools, and
the existing JSON Schema interface. For schemas, request raw JSON and validate
the complete final value locally, withholding provisional JSON as AGY does;
do not claim constrained decoding. Keep forced/required tool selection rejected
until separately verified. Accept reasoning controls only when model support is
known. Images/files, hosted web search, other hosted tools, audio, and additional
API routes are outside this initial delivery even where upstream supports some.

The current ChatGPT plan preview requires upstream `store:false`, `stream:true`,
array input with full context, and no HTTP `previous_response_id`. Nonstreaming
gateway requests collect the bounded upstream stream. Gateway `store:true`
continues to mean the existing local, client-owned item store; it never enables
OpenAI storage. Preserve the opt-out header and retention/eviction behavior.

Unsupported upstream fields include output-token caps, sampling controls, and
cache-retention controls. Maintain existing gateway compatibility explicitly:
positive token budgets remain disclosed best-effort answer-length instructions,
cache hints remain disclosed as ignored, and sampling controls are rejected.
Do not forward ignored fields or advertise them as enforced OpenAI capabilities.
Other unsupported fields fail validation rather than being silently dropped.
See the current [preview contract](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations).

The OpenAI Responses item store preserves encrypted reasoning items and their
provider state for replay, with provider provenance and client ownership; it
never places that state in the AGY text transcript. OpenAI Chat replay uses a
separate owner-scoped in-memory cache with a 24-hour TTL, 16 MiB per client,
64 MiB globally, and 4,096 entries; restart clears it. Deterministic tests must
cover bounds, ownership, replay, and restart behavior. Ordinary text history may
move between models; provider-specific opaque state may not.

## OpenAI authentication and transport

Operator-only `login`, `status`, `accounts`, `select`, `logout`, and `resume`
commands are separate from public model routes. OpenAI is disabled by default.
`AI_ROUTER_OPENAI_ENABLED` controls enablement;
`AI_ROUTER_OPENAI_STATE_DIR` defaults to `/var/lib/ai-router/openai-auth`;
`AI_ROUTER_OPENAI_MAX_CONCURRENT` and `AI_ROUTER_OPENAI_MAX_QUEUE` default to
2 and 8. Provider-specific auth/runtime failures leave AGY available; structurally
unsafe configuration may still fail startup. There is no static subscription
token in gateway client-key configuration and no dotenv loading.

Implement the documented public OAuth client flow:

- Persist one opaque host ID before first login. Use `dynamic_agent_client` only
  for initial registration; save the issued client ID per verified account and
  workspace registration. Reuse stable registration and host identity.
- Use the system browser, loopback `127.0.0.1` callback, PKCE S256, random state
  and nonce, exact redirect matching, and the API resource. Request identity
  scopes plus `offline_access resource.invoke chatgpt.tokens.use.direct`.
- Verify ID-token signature/JWKS, issuer, audience, expiry, and nonce. Check the
  granted plan-use scope before discovery/inference. Validate returning account
  identity before replacing its token set. Never use identity-only login as an
  inference grant.
- Store registrations separately, with atomic owner-only writes and no symlink
  or hardlink acceptance. Allow one selected account per running OpenAI provider;
  support saved registrations and explicit operator switching without merging
  identities or transparently rotating accounts for quota.
- Serialize rotating refreshes across processes with a state lock. The server
  owns refresh while active; helpers cannot concurrently refresh the same session.
  Refresh near expiry using the issued client ID and atomically replace the
  token set. Retain credentials on transient failures; terminal grant failures
  stop use and require login. Implement revocation and honest logout status.

For local Docker login, the operator starts the helper with a published
loopback-only callback port and the same dedicated OpenAI volume. The callback
listener binds the helper container interface but is published only on host
`127.0.0.1`; the browser redirect still uses `127.0.0.1`. Stop the gateway while
the helper mutates registrations. For a remote VM, create an SSH local forward
from the browser computer to the VM's host-loopback callback port, then run the
login helper on the VM through that SSH session. Keep the tunnel open through
callback completion and use the same port on both sides. Preserve the VM's host
ID and assign refresh ownership to the VM; do not transfer token files. Never
print token records or ask the operator to paste tokens into chat.

Use fixed official OAuth/discovery/inference destinations and TLS verification;
production configuration must not redirect bearer credentials to arbitrary
upstreams. Mock endpoint injection is test-only. Do not forward authorization
across redirects. Use bounded pooled HTTP connections, response/event sizes,
deadlines, cancellation, and sanitized error conversion.

Treat inference as complete only on `response.completed`. Map `response.failed`,
`response.incomplete`, malformed/truncated streams, and transport failures
explicitly, preserving request IDs, error codes, partial usage, and honest terminal
status. Fix existing renderers where they currently assume every result is a
normal stop. Errors can occur after the stream opens. Never replay a generation
request automatically after sending its body or receiving output, switch billing,
or clear a quota block merely because model listing succeeds. Refresh before
dispatch; credential renewal does not authorize retrying a possibly consumed run.

## Operations, monitoring, and rollout

Provider-specific snapshots and capability records are present, while existing
AGY fields continue to describe AGY. Telemetry defaults old records to AGY and
identifies the provider on new records. Deterministic migration and monitor
validation passed locally; the monitor PTY suite also passed its navigation,
scroll, pause, range, disconnect/recovery, theme, resize, quit, text, and JSON
checks. Preserve the legacy AGY field while exposing generic partial usage and
provider request IDs/errors.

Record model/provider, timings, queue/admission, actual token usage, sanitized
error codes, upstream request IDs, and freshness. Never infer remaining quota
from token counts or fabricate usage. Show subscription use and a Manage usage
link in operator status/help and monitor views. Document shared plan limits,
possible account-configured credit use, and that the router adds no allowance.
Prompts, tool arguments, OAuth tokens/codes/URLs, and opaque replay state stay
out of persistent telemetry. In-memory OAuth and encrypted-state redaction have synthetic unit coverage;
integrated monitor migration and PTY checks passed locally.

Separate OpenAI state from AGY HOME, telemetry, and workspaces in a named volume.
Prevent AGY subprocesses/native tools from reading OpenAI state; do not inject
OpenAI tokens into their environment. Native is unavailable whenever OpenAI is
enabled; re-enable the existing AGY native path only with OpenAI disabled. Future
native use with OpenAI enabled requires new containment evidence for the OpenAI
state volume. Preserve nonroot
containers, loopback service exposure, and existing build-context exclusions.

All work follows the secret-file policy: never read `.env*`, `*.pem`, `*.key`,
or files under `secrets` or `credentials`, including through builds, archives,
logs, tools, or delegated work. Use synthetic auth records and mock services for
deterministic development. An explicitly user-authorized local Docker runtime
test may exercise the normal gateway/helper flow with signed-in runtime state;
never inspect, copy, or expose raw credential/token files. Record only sanitized
outcomes and IDs. Routine implementation does not depend on an interactive
login.

Confirm the applicable integration category before live rollout: current docs
cover open-source/local personal tools and a self-hosted OSS VM flow; private,
paid, or remotely hosted offerings may need separate access. Do not equate this
personal gateway with permission to offer subscription-backed inference to other
users. Recheck official docs before implementing the preview contract.

The AGY/provider refactor, mock transport, opt-in configuration, and operator
helpers passed deterministic local acceptance and cross-review. Formatting
passed; Clippy passed with three existing warnings. All 167 Rust tests passed,
and the final production image was rebuilt and started locally with OpenAI
enabled on port 8091. OAuth/discovery found 7 OpenAI and 13 AGY model IDs;
selected state persisted across container recreation. All four live `gpt-6-luna`
Chat/Responses streaming modes, Chat and Responses tool continuations, and
validated JSON schema passed. Real rotating refresh, full installed-client
checks, and independent integration-category attestation remain open. The
current local evidence is recorded in [validation](validation.md). Roll out only
after those gates. Roll
back by disabling OpenAI; AGY and its volume remain usable. Do not automatically downgrade code
that cannot read a migrated state format; version state and document restoration.

## Acceptance requirements and pending evidence

These are release requirements. Deterministic local acceptance has passed as
recorded in [validation](validation.md); formatting, Clippy, and the final
production image build also passed. O1 login/discovery and O2 expanded local
checks are partial: state persisted across container recreation, AGY round trips
passed, and all four OpenAI Chat/Responses streaming modes, client-tool
continuations, and validated JSON schema passed. Rotating refresh, full
installed-client checks, and independent integration-category attestation
remain open.

1. Existing AGY HTTP, SDK, inert-tool, isolation, native-gate, cancellation, and
   monitor contracts pass with OpenAI disabled; client configuration is unchanged.
2. The catalogue is the exact union of ready providers. Login loss, stale probes,
   collisions, quota blocks, restart, recovery, and account changes produce the
   documented lists/errors without routing to the wrong provider.
3. Both client APIs and streaming modes pass text, instruction/history, complete
   tool-result/final-answer cycles, multiple OpenAI calls, schema validation,
   item-storage opt-out, opaque-state handling, and malformed-request contracts.
4. Synthetic OAuth tests cover PKCE/state/nonce/identity/scope failure, registration
   reuse, refresh rotation races, logout/revocation, restart persistence, and
   credential redaction. No signed-in host state is a test dependency.
5. Mock HTTP/SSE tests cover errors before/after headers, nonstandard admission
   bodies, all terminal states, missing usage, disconnects, output bounds, and
   deadlines. No duplicate generation, hidden paid fallback, or server tool
   execution occurs.
6. Stalling or exhausting either provider leaves the other usable within its
   admission budget. Probes, queueing, cleanup, and shutdown stay bounded.
7. Operator live evidence separately verifies integration-category eligibility,
   catalogue, successful OpenAI API/tool flows, refresh/restart, simultaneous AGY
   use, and selected installed clients. OAuth/discovery, persistence, AGY round
   trips, and successful API/tool/schema checks do not complete O1/O2 while
   rotating refresh, full installed-client coverage, or category attestation
   remain open; fixture success is never reported as subscription access.

## Official references

- [Integration options and availability](https://developers.openai.com/siwc/quickstart)
- [Open-source plan usage and host identity](https://developers.openai.com/siwc/token-sharing-open-source)
- [Registration and sign-in](https://developers.openai.com/siwc/token-sharing-open-source/sign-in)
- [Accounts, refresh, and revocation](https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions)
- [Account models and inference](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference)
- [Preview limitations](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations)
- [Errors and recovery](https://developers.openai.com/siwc/token-sharing-open-source/errors-and-recovery)
- [Self-hosted VMs](https://developers.openai.com/siwc/token-sharing-open-source/self-hosted-vms)
- [Usage presentation](https://developers.openai.com/siwc/ui-ux-guidelines)
- [User plan allowance and app limits](https://learn.chatgpt.com/docs/sign-in-with-chatgpt)
