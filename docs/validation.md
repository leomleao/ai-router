# Local validation

The subsequent [n8n compatibility regression review](n8n-compatibility.md) records
the schema fix, diagnostic privacy checks, restored CLI coverage, and separate
Vercel/LangChain client matrices. Historical counts below describe the earlier
implementation baseline.

## Accept-and-disclose compatibility bundle (2026-10-21)

An external audit replayed request bodies that the official SDKs, Vercel AI
SDK, LangChain, n8n, Supabase Studio, Twenty and Cognee send by default. This
change closes audit items A1, A2, B1, B3, part of B5, B7 to B11 and the
Responses `stream_options` gap:

- Effort `none`/`minimal` is accepted. OpenAI receives `none` unchanged. It
  receives `minimal` unless the catalogue's `supported_reasoning_levels` omit
  it; then the lowest listed level above `minimal` is sent, disclosed as
  `X-AI-Router-Reasoning-Effort: mapped=<level>`. On Antigravity both mean "no
  explicit effort", disclosed as `ignored`, with no model-suffix conflict.
- Sampling hints are range-checked and ignored (`X-AI-Router-Sampling-Controls:
  ignored`). `n>1`, `logit_bias`, `logprobs`, `top_logprobs>0` and a non-empty
  `stop` stay rejected.
- `metadata` (echoed on Responses), `user`, `safety_identifier`, `service_tier`
  `auto`/`default` and Responses `truncation:"auto"` are accepted and named in
  `X-AI-Router-Ignored-Fields`. Responses `stream_options` accepts
  `include_obfuscation:false`.
- Unencrypted Responses reasoning replay is dropped (`X-AI-Router-Reasoning-Replay:
  dropped`). A Responses assistant `refusal` part becomes text. Chat
  `annotations`, `reasoning_content` and part-level `cache_control` are
  ignored and disclosed.
- Errors carry retry guidance: `x-should-retry: false` for validation errors,
  invalid or incomplete output shapes and durable restrictions, and
  `retry-after` for other 503s. Local saturation is 503 `busy`; key rate limits
  stay 429. Upstream 401/403 become 503 with the provider's code.

Overload latching was checked against the new headers. A 503 overload, transport
failure or upstream 401/403/5xx latches OpenAI until the next catalogue probe
(at most 60 s while healthy). Retries inside that window get the same 503 and
`retry-after: 30` locally, with no upstream call. A durable plan or
authorization restriction stays latched and returns `x-should-retry: false`.

Validation used the Docker test stage (`ai-router-test:t1-compat-accept`):
213 Rust tests passed (203 before), with 134 official SDK checks per model
(125 before), 44 Studio checks per provider (40 before; the new checks drive
the SDK's effort-`none` plus `temperature:0` request), 170 n8n Vercel checks,
38 LangChain checks, monitor PTY checks and 5 login checks. Formatting passed;
clippy reported only the existing style-warning categories. The audit's synthetic probe
harness replayed all 164 case bodies before and after the change. 43 cases
moved from a 400 rejection to a provider call, including Studio `title-v2`,
`code-complete`, `onboarding-design` and `policy`. Studio `title-v2`, `policy`
and `feedback-classify` then return the fixture-only `invalid_structured_output`
502, the same as the audit's control without reasoning. The remaining 400s are
deliberately rejected or owned by other changes: active sampling controls,
forced `tool_choice`, hosted tools and other `include` values, media,
`previous_response_id`/`conversation`/`background`/`prompt`, non-neutral
verbosity, unknown model IDs, and an explicit effort tier that conflicts with
an Antigravity model ID. No live provider, server or deployment was used.

## Streaming error semantics (2026-10-21)

External audit item B6: streamed requests sent HTTP 200 and
`response.created`/`in_progress` before the provider had accepted the request,
then reported upstream overload, quota and authentication failures only as
`response.failed`. LangChain JS (and so n8n streaming agents) and the official
Python SDK ignore that event, so the failure produced an empty answer.

A stream now commits its 200 only after the provider's first event (`Init`,
after accepted upstream headers). Earlier failures return the nonstreaming
status, body and headers. The commit wait is bounded at 15 seconds, the existing
keepalive interval; a provider still silent then gets a committed stream, as
before. After commit, a Responses failure emits the spec `error` event
(with the top-level `error` object the SDKs raise on) before `response.failed`,
and a Chat failure emits a chunk with a top-level `error` object, typed like the
HTTP body, before `[DONE]`.

New synthetic fixtures cover pre-output overload, quota and authentication
failures and a mid-stream failure after partial text, for both providers. The
tests compare streamed against nonstreamed status, body, `retry-after` and
telemetry classification, and check the event order and fields, the 15-second
bound (shortened in the test harness), and client disconnects before and after
commit. `tests/sdk_contract.py`, `tests/n8n_contract.mjs` and
`tests/n8n_langchain_contract.mjs` gained a failure mode. Each client runs once
per fresh gateway (overload and authentication latch provider state) across 2
providers, 3 failures and both APIs: 36 runs, every one of which must raise. On
the old code 30 of those 36 runs failed. Python and LangChain streamed Responses
ended silently on all three failures, and pre-output Chat failures surfaced
without an HTTP status. The existing overload and queue-deadline tests now
expect 503 and 504 rather than 200.

Audit probe replay (`gap-audit.md` harness, 164 cases): `error.responses.stream.overload`
changed from HTTP 200 with `response.created, response.in_progress,
response.failed` to HTTP 503 `server_is_overloaded`. The other 163 cases were
unchanged.

The Docker test stage passed: 208 Rust tests (203 before; 5 new), including
the existing official SDK, n8n Vercel, Studio and LangChain matrices and the 36
failure runs, plus monitor PTY checks and 5 login checks. Formatting passed;
clippy reported only the existing style-warning categories. Only synthetic
loopback fixtures were used.

## Supabase Studio Responses compatibility (2026-10-21)

Supabase Studio's assistant (`supabase/studio:2026.10.12`, `ai@7.0.59`,
`@ai-sdk/openai@4.0.41`) sets `store:false` and a reasoning effort for
`gpt-6-luna`. For that reasoning model the SDK adds
`include:["reasoning.encrypted_content"]` and `reasoning.summary:"detailed"`, which
were rejected before the provider was called. Responses now accept that
include value and the `auto`/`concise`/`detailed` summary hints; the normalized
request is unchanged. OpenAI already requests and returns encrypted reasoning;
Antigravity returns no reasoning items. Summaries are never produced, so
`X-AI-Router-Reasoning-Summary: unavailable` and capabilities disclose that.
Other include values, malformed summaries, and Chat `include` stay rejected.

`tests/studio_contract.mjs` drives the pinned Studio SDKs through streamed and
nonstreamed multi-step tool turns. It checks the exact wire fields, the
store-false replay of encrypted reasoning, and the rejection of other includes.
It runs against the OpenAI and Antigravity fixtures, with 40 checks each. 203 Rust
tests passed, together with 125 official SDK checks per model, 170 n8n Vercel
checks, 38 LangChain checks, monitor PTY checks, and 5 login checks. The test
stage now builds the debug binary that the PTY and login checks run; without it,
`scripts/test-local.sh` failed on `main`. Formatting, `git diff --check` and
clippy passed; clippy reported only the existing style-warning categories. No live
Studio, server, or deployment was used.

## Direct model-only implementation (2026-10-20)

The local implementation and independent review rounds completed. The previous
CLI implementation remains on `archive/agy-cli-2026-10-20` at `4d23e015`.
No live server, other user terminal, home-server Compose, push or remote deployment
was used. The existing local gateway on 8091 and legacy AGY auth volume were
left untouched. The direct test gateway uses loopback port 8092 and its own
Antigravity state/telemetry volumes; OpenAI keeps its separate existing state.

Final deterministic checks used explicit safe source/test mounts, synthetic
credentials, and external networking disabled:

- 181 Rust tests passed; the updated binary built successfully.
- Real TCP contracts passed 38 official OpenAI SDK checks and 8 pinned Vercel
  SDK checks, across reviewed Gemini and Claude profiles.
- Monitor PTY checks passed: pages/scrolling, pause/range, disconnect/recovery,
  themes, resizing, exit, and text/JSON output.
- Clippy completed with two retained style warnings: constructor argument count
  and a pre-existing test clone. No compilation errors.
- Docker policy checks passed: nonroot/read-only, loopback, dedicated volumes,
  and protected context exclusions tested with synthetic dummy files only.
- Formatting, shell syntax, documentation links, `git diff --check`, and DAG
  acyclicity passed (16 nodes, 20 dependencies).

The CLI-free production image `ai-router:direct-local` built successfully. Runtime
smoke checks verified UID 10001, installed attribution, no AGY binary, readiness,
schema-v3 model-only capabilities, and removed native routes returning 404.
All temporary diagnostic code was removed before this final build.

### Public OAuth registration packaging and operations follow-up

The reviewed public installed-app registration is now Base64-packaged like OMP;
encoding is not encryption and no personal access/refresh token is bundled.
The compiled protocol baseline includes decoded-registration SHA-256 fingerprints.
Tests verify decoding, malformed data and fingerprint drift. Status exposes only
registration provenance and integrity/matching booleans, never decoded values.

A changed configured client ID now blocks admission, dispatch and cached access-token reuse even before expiration; synthetic coverage verifies those paths.
Custom operator registrations remain supported. A matching saved client ID does
not validate a changed client secret or prove live renewal; real refresh remains
an explicit acceptance limit. Public update checks still run at startup and
hourly and remain advisory, with all credential/protocol upgrades reviewed and
applied through an explicit image rebuild. After replacing only the local 8092
gateway, status reported `bundled_registration_valid:true`,
`registration_source:reviewed_omp` and `session_registration_matches:true`.
Saved sign-in remained available, discovery returned 13 Antigravity and 7 OpenAI
models, and both upstream revision/version advisories were visible without
affecting readiness. See the [operations runbook](operations.md).

### Authorized local provider checks

Separate Google consent using the approved pinned OMP registration succeeded;
project resolution and exact discovery returned 13 reviewed Antigravity IDs and
7 independent OpenAI IDs. Recreating the direct gateway preserved sign-in and
restored discovery. No raw saved session or protected secret file was inspected.

After the empty-text replay fix, each of `gemini-3.8-flash-high`,
`claude-opus-5-5-medium`, and `claude-sonnet-5-5-high` passed 15 live official SDK
checks: authenticated discovery, Chat/Responses text and streaming, both JSON
schema modes, both tool handoffs and complete client-owned tool-result round
trips, call IDs/arguments, and disconnect. Flash Low passed the same 15 checks
earlier in development; it was not rerun after the final replay fix. The final
image also passed an Opus
tool round trip after temporary diagnostics were removed. OpenAI `gpt-6-luna`
continued answering independently. `gemini-4-argon-medium` returned upstream
429 quota errors on both attempts; no success or fallback is claimed for Argon.

Review and live validation resolved dotted-ID rejection, exposure of unreviewed
image/internal SKUs, Claude header/private-state differences, default fields on
function-call parts, and empty visible replay parts. The last caused a nested
Anthropic `text.text: Field required` error; omitting empty visible text parts
at wire emission fixed the live Claude tool-result continuation. Private signed
state remains memory-only and never enters public outputs or persisted logs.
Auth reviews also fixed cross-process dispatch closure, cancellation epochs,
refresh-lock contention, and prompt handling of validated OAuth denial.

### Remaining release gates

Correctness checks above do not establish a matched speedup. No authenticated
CLI/direct A/B comparison was run: the existing CLI gateway was left untouched,
and its raw client key was not read from files. The loopback-only
[comparison helper](../tests/latency_compare.py) is ready for an operator to
supply the existing client key through stdin. Its synthetic tests cover redirect
refusal, proxy bypass, bounded SSE and deadlines; a same-direct-gateway control
pair passed, which is explicitly **not** a CLI comparison. Early short-prompt
samples during development are not acceptance evidence for the final protocol.

Real token expiration/rotating refresh and comprehensive account recovery remain
untested against Google; races and durable restriction recovery are covered by
synthetic tests. Wider model/media acceptance, hard output caps and forced tool
selection are not claimed. The temporary local smoke client is removed from the
final gateway configuration; existing model client digest records are preserved.
See the [plan](direct-antigravity-plan.md), [DAG](direct-antigravity-dag.md), and
[auth contract](direct-auth-contract.md) for these release gates.

The sections below are historical validation records for earlier CLI-based
revisions. They do not establish acceptance of this direct revision.

## Provider monitor correction and replacement planning (2026-10-20)

The monitor now uses neutral startup labels, identifies combined usage totals,
counts Other/unknown provider records, honours stale/disabled provider state,
and publishes effective OpenAI restrictions/account-generation state consistently
with model admission and capability errors. See the
[review](monitor-provider-review.md) for findings and retained compatibility.

Final checks ran in an isolated Docker test container, with synthetic state,
explicit source/manifest/test mounts and external networking disabled:

- 130 library + 25 HTTP + 15 runner tests passed (170 total).
- The updated binary built successfully; monitor PTY checks passed for pages,
  scrolling, pause/range, disconnect/recovery, themes, resize, exit and text/JSON.
- Rust formatting and `git diff --check` passed.
- Local documentation links resolve; the implementation DAG is acyclic
  (16 nodes, 20 dependencies).

The initial full-test container mounted its synthetic temporary directory with
Docker's default `noexec`, preventing the HTTP spy fixture from executing. The
same suite passed after correcting that test-only mount to permit its generated
fixture. Production mount settings were not changed.

The current CLI implementation was preserved locally on
`archive/agy-cli-2026-10-20` at `4d23e015`. The
[replacement plan](direct-antigravity-plan.md) and
[DAG](direct-antigravity-dag.md) were independently reviewed. They are planning
artifacts: no direct adapter, live credential migration, production restart,
server Compose deployment or push occurred in this round. Existing server-login
reuse is a feasibility gate, not a verified capability.

## Dual-provider checkpoint (2026-10-20)

The dual-provider implementation has passed deterministic local integration
validation and cross-review. The production gateway image and isolated
production smoke checks passed. Clippy completed with three existing style
warnings (argument count, nested conditional, and a test-only clone). Earlier sections below
preserve historical AGY-only checkpoints. Authorized local OAuth, discovery,
and basic live inference passed; the complete O1/O2 acceptance remains partial.

Automated tests run in local Docker with synthetic keys/state. OpenAI uses
explicit mock services; the pinned official AGY CLI also runs against a loopback
mock model with external networking disabled. No signed-in provider state was
used.

### Current integrated evidence

- Rust: 127 library + 25 HTTP + 15 runner tests passed (167 total).
- Client regressions: 235 OpenAI 3.24.0 TCP/SDK/policy checks passed; 122 n8n
  Vercel AI 7.0.66 / OpenAI 4.0.20 checks passed.
- AGY boundary: 109 pinned official AGY 1.4.3 / gateway / MCP checks passed
  against a loopback mock with synthetic state and networking disabled.
- Monitor PTY: page navigation, scrolling, pause, range selection, disconnect
  and recovery, themes, resize, `q`, Ctrl-C, text, and JSON checks passed.
- Docker policy: loopback binding, nonroot runtime, three separate volumes,
  OpenAI disabled by default, and synthetic protected-context exclusions passed.
- `cargo fmt --all -- --check` and `git diff --check` passed.
- `cargo clippy --locked --all-targets -j 2` completed; three existing style
  warnings remain, with no build errors.
- Production image `ai-router:subscription-local` built successfully. With only
  synthetic tmpfs state and external networking disabled, smoke checks passed
  for UID 10001, liveness, unavailable-provider readiness, empty model list,
  schema-v2 capabilities, and operator account status. No existing service or
  authentication volume was changed.

Final review added regression coverage for coded authentication/server failures
hiding OpenAI immediately and reasoning-only incomplete stream indexes. It also
fixed a test-fixture race: a synthetic descendant PID file can exist briefly
before its writer publishes the number; cancellation checks now wait for a
complete PID rather than interpreting that empty file.

Local browser setup exposed an idle-connection callback timeout. The listener
now accepts bounded concurrent connections, ignores expired or unrelated
requests, and preserves state validation and the overall authorization deadline.
Three additional synthetic regressions cover preconnects, malformed requests,
timeout recovery, and reader cleanup. All 157 Rust tests passed after this fix.

Live OpenAI responses exposed two stream differences: missing Content-Type and
completed tool items omitted from the terminal output array. The adapter now
validates missing-header streams and reconstructs bounded finished items by
index and stable ID, rejecting conflicts, gaps, partial arguments, and invalid
terminals. Review also fixed restriction writes being canceled by request
deadlines and transient errors replacing durable quota latches. The expanded
167-test Rust suite passed with these changes; quota tests use synthetic state.

The automated results above establish deterministic local behavior. Live
evidence is recorded separately below. The full n8n and Hermes applications
were not run; the client checks above are SDK-shaped compatibility tests.

### Local signed-in Docker smoke (2026-10-20)

The rebuilt production image was started on host-loopback port 8091, preserving
the existing client digest and AGY/telemetry volumes. AGY 1.4.3 discovered 13
models. Liveness, readiness, bearer authentication, schema-v2 capabilities,
and unknown-model rejection passed. Live `gemini-3.8-flash-low` returned
`ROUTER_OK` through nonstreaming Chat Completions and streaming Responses.
A live client-tool round trip selected `router_probe`, returned validated
arguments, accepted the synthetic client result, and produced `ROUTER_TOOL_OK`.
The operator completed ChatGPT authorization. Discovery added 7 OpenAI model
IDs alongside the 13 AGY models, with both providers ready and no conflicts.
Live `gpt-6-luna` passed Chat Completions and Responses in streaming and
nonstreaming modes, plus locally validated JSON-schema output. Client-tool
continuation passed through both APIs: one validated `router_probe` call,
a synthetic client result, and the requested final marker. The rebuilt image
includes the finished-item reconstruction fix; both provider routes were also
exercised concurrently.
The selected session survived container recreation. Actual rotating refresh,
full installed-client acceptance, and independent integration-category review
remain outside these local checks. The ephemeral test client digest was removed
after testing, its key was verified rejected, and the original client digest
record was verified unchanged. After the final recreation both providers
recovered their 20-model union and `/health` and `/ready` returned success.

## AGY 1.4.3 and direct client-tool functions

The installer pins official release `1.4.3-4577820882632704`, with separate
SHA-512 checks for Linux ARM64 and AMD64, and verifies `agy --version`.
The official ARM64 executable's 60-name `init.tools` registry was independently
recaptured against a synthetic Gemini endpoint and matches the compiled
1.4.3 snapshot. Inventory drift still fails closed.

The latest production failure had `unapproved_mcp_server`, with no selected
tool preserved by the old diagnostics. Direct functions now remove that
model-selected routing envelope: the inert MCP catalogue advertises
`_meta.antigravity/alwaysLoad:true`, a capability documented in the official
[1.3.3 changelog](https://github.com/google-antigravity/antigravity-cli/blob/main/CHANGELOG.md).
The real 1.4.3 model declarations contain qualified functions bound to the
private server and omit `call_mcp_tool`. Native/provider tool steps and unknown
aliases remain terminal errors. Stream claims without an actual validated
relay handoff cannot produce a client call.

Real CLI testing also established that its required `toolSummary` metadata
must be separate from closed client argument schemas. A fixed private `input`
envelope preserves client fields, including a field named `toolSummary`.
The relay unwraps and independently validates the original schema. Client
schemas receive their own resource root when needed to preserve local refs.
Logs classify malformed dispatch shapes and server-name status without raw
server names, encoded envelopes or argument values.

`tests/agy_cli_contract.py` exercises the official CLI, real gateway and real
inert relay in fresh synthetic state. It covers both API styles and streaming
modes, colliding native/client names, second-tool selection, nested objects,
closed union schemas, local references, metadata separation, invalid-argument
correction and native-tool rejection. Quota/model/agent-list management probe
metadata is synthetic: fresh API-key mode lists no main agents on both 1.4.2
and 1.4.3, although actual custom-agent execution works. Production's strict
account-backed preflight is unchanged. These checks do not claim subscription
authentication or live inference acceptance; that remains an operator gate.

`AI_ROUTER_TEST_IMAGE=ai-router-test:agy143-direct-final ./scripts/test-local.sh`
passed 106 Rust tests (66 unit, 25 HTTP, 15 runner), monitor PTY checks,
109 official CLI/gateway/MCP checks, 122 pinned n8n SDK checks and 235 OpenAI
3.24.0 TCP/SDK/policy checks. `python3 scripts/test-docker-policy.py` passed
with synthetic protected-path fixtures. The production gateway image built
successfully with the pinned ARM64 executable. No signed-in provider state
was used for these checks.

## Distinct client-tool dispatch names

Live failures included direct native `write_todos` selections and a nested
`call_mcp_tool` selection. The private MCP catalogue now gives tools canonical
request-local aliases and the owned prompt specifies one dispatcher layer,
the exact server and alias mapping. The relay resolves the alias before schema
validation and handoff; the decoder requires a corresponding approved MCP
dispatch. API clients continue to see their original tool names. Invalid
argument feedback repeats the private alias and retains the existing bounded
schema correction guidance and budget.

Synthetic runner coverage selects the second tool after reordering colliding
client names, replays original-name tool history through Chat and Responses,
and requires one validated original-name call. Native calls, nested dispatcher
selections and another MCP server must fail without a tool handoff. Unit checks
cover canonical aliases, out-of-range indexes, empty catalogues and client
names that themselves resemble aliases. TCP SDK checks exercise both APIs and
both streaming modes. Live AGY selection remains an operator validation step.

`./scripts/test-local.sh` passed 103 Rust tests (63 unit, 25 HTTP, 15 runner),
monitor PTY checks, 122 pinned n8n SDK checks and 229 OpenAI 3.24.0
TCP/SDK/policy checks. The production gateway image built successfully.

## Precise extra-field correction feedback

A live `Hub_Spot_manage_crm_objects` request exhausted all three attempts with
root `additionalProperties` failures after 84 seconds. Generic schema-rule
feedback did not identify which fields AGY needed to remove. Provider-only
`field_feedback` now identifies unexpected names and declared fields at the
failing object location, without argument values. Container logs retain their
previous metadata-only contract; JSON serialization and Debug output exclude
the provider-only names and paths.

Unit coverage includes root and nested objects, local `$ref`, pattern fields,
separate `anyOf`/`oneOf` branch hints, property-free schemas, bounds and privacy.
The real synthetic MCP fixture requires the exact unexpected/allowed names
before correcting its call. SDK coverage reproduces the HubSpot-shaped failure
with a declared `createRequest` and an extra field: the correction must remove
only the undeclared field, preserve the declared argument and produce one valid
call in both streaming modes. Logs/client responses must exclude the private
unexpected-field name. Live model behavior remains an operator check.

`./scripts/test-local.sh` passed 101 Rust tests (62 unit, 25 HTTP, 14 runner),
monitor PTY checks, 122 pinned n8n SDK checks and 193 OpenAI 3.24.0
TCP/SDK/policy checks. The production gateway image built successfully.

## Bounded client-tool argument correction

New diagnostics identified the live failure precisely: AGY selected
`Twenty_CRM_learn_tools` without required `toolNames` and with undeclared fields.
Known tools now receive bounded MCP `isError:true` feedback for non-object
arguments/schema mismatches, with up to two corrections in the same CLI run.
The third rejection remains a terminal `invalid_provider_tool_call`; unknown
tools and native-profile boundary violations still fail immediately. No rejected
call is handed to the client or executed by the router.

The budget uses atomic, non-overwriting numbered workspace publications and
survives MCP relay restarts/connections. The supervisor revalidates and logs
each rejected attempt, and emits `client_tool_call_corrected` after successful
recovery. Original deadlines, cancellation, IO bounds and partial usage labeling
remain in effect. Corrections do not start another CLI run or reset SSE IDs.

Synthetic relay/runner tests reproduce the `toolNames` failure, provide real
MCP error feedback, and require a valid correction before one client call is
published. Coverage includes budget exhaustion across relay restarts and
deadline/cancellation after feedback. TCP SDK checks exercise Chat and
Responses, both streaming modes, valid final arguments, observed partial usage,
rejection/recovery logs after cleanup and exclusion of private argument values.
Model self-correction on authenticated AGY remains a separate operator gate.

`./scripts/test-local.sh` passed 98 Rust tests (59 unit, 25 HTTP, 14 runner),
monitor PTY checks, 122 pinned n8n SDK checks and 187 OpenAI 3.24.0
TCP/SDK/policy checks. The production gateway image built successfully.
Streaming exhaustion produces one terminal failure and no client tool call.

## Rejected client-tool diagnostics

A live AGY 1.4.2 run selected `load_skill` successfully, then failed its next
tool attempt with `invalid_provider_tool_call`. Existing logs retained the
request/tool inventory and generic error, but discarded the attempted tool
and validation reason. The new `client_tool_rejected` gateway stderr event
preserves bounded validation metadata with the request ID, model and stage.
It does not recover previously discarded attempts or change HTTP errors.

The relay and supervisor share schema validation. A rejected call is carried
through the owned temporary handoff only until the supervisor classifies it;
cleanup still removes that workspace. Metadata distinguishes unknown tools,
non-object arguments, schema mismatches, invalid handoffs and disallowed
dispatches. Schema diagnostics report paths, JSON types and missing required
fields without formatting validator errors or recording argument values,
dynamic instance keys, enum values or schema descriptions.

Formatter unit tests cover classification, missing required fields, enum/type
failures, dynamic-key privacy and bounded error lists/paths. TCP tests send real
synthetic MCP calls, assert unchanged failures, wait for workspace cleanup, and
then inspect gateway stderr for exactly one correlated diagnostic per request.
They verify the field/type mismatch and confirm synthetic private argument
values, descriptions and bearer keys are absent from logs.

`./scripts/test-local.sh` passed 95 Rust tests (58 unit, 25 HTTP, 12 runner),
monitor PTY checks, 122 pinned n8n SDK checks and 170 OpenAI 3.24.0
TCP/SDK/policy checks. The production gateway image built successfully.
These checks use synthetic AGY; a fresh live failure after deployment is still
needed to diagnose the original model's rejected call.

## n8n Responses item references

The private monitor confirmed the rejected n8n request contained an
`item_reference` to a prior router-generated assistant message. Upgrading AGY
alone cannot resolve an HTTP adapter reference. The router now saves completed
Responses output items as exact, client-scoped JSON in bounded process memory
and expands references before normal message/tool-history validation. It does
not reconstruct content from redacted monitor captures or resume AGY sessions.

Default/null/true `store` enables storage, and `store:false` opts out of saving
new items. A 24-hour TTL and oldest-first per-client/global count and byte
limits bound retention; restarts clear the store. Headers, capabilities and
operator docs disclose these boundaries. Clients must start a new chat or
resend complete history when a referenced item is unavailable, including IDs
issued before deploying this feature.

Validation passed 93 Rust tests (56 unit, 25 HTTP, 12 runner), monitor PTY
checks, 122 pinned n8n Vercel SDK checks and 156 OpenAI 3.24.0 TCP/SDK/policy
checks through `./scripts/test-local.sh`. Actual SDK response messages are
replayed across streaming/nonstreaming turns with omitted/true/false `store`;
the tests assert the SDK sends item references for stored turns and complete
content for opt-out turns. The provider fixture asserts resolved assistant text
reaches its prompt. HTTP tests cover referenced function calls/results with
unchanged call IDs, owner isolation, malformed storage flags and failed-stream
items; unit tests cover exact unredacted synthetic output, expiry, a fresh
store, count/byte eviction, immutable IDs, atomic oversized writes, reference
shape and expanded-input limits. These tests use only synthetic AGY and do not
claim a live n8n application or authenticated AGY 1.4.2 acceptance run.
The production image also passed health, item-reference capability limits,
missing-reference errors, expected CLI pin and unauthenticated-readiness smoke
checks with fresh disposable state; its installed AGY reports 1.4.2.

## AGY 1.4.2 upgrade

The current adapter and Compose pin target **1.4.2**. Official Linux ARM64 and
AMD64 manifests agree on release `1.4.2-4835981233356800`; both archives'
SHA-512 digests were checked. The Linux ARM64 runtime reports `1.4.2`.

The verified binary's `agy changelog` includes 1.4.1 and 1.4.2 patch notes
that the public web changelog had not yet listed when checked. Changes since
1.3.5 relevant to this adapter include network-disconnect retries, temporary
startup eligibility retries and MCP process-exit handling. Large structured
tool-result truncation and Auto-mode sandbox reviews improve native execution;
they do not add OpenAI generation parameters. The 1.4.2 patch itself fixes
Remote Control daemon conversation history.

The runner now explicitly sets `AGY_CLI_OSC7501=off` for every subprocess,
including management probes, to keep terminal-status sequences out of its
headless pipes. The image sets the same default for manual CLI commands.
Router deadlines still apply: the default 120-second deadline expires before
AGY's five-minute stalled-connection retry threshold. Native execution stays
behind the existing sandbox-verification gate.

The 60-name registry snapshot is carried forward from the previously observed
1.3.5 contract; it has not been recaptured from authenticated Linux 1.4.2.
The adapter still fails closed on any additional, missing or duplicate tool.
Fresh unauthenticated 1.4.2 cannot emit the authenticated init payload, so a
live request after the operator deploys remains a separate acceptance gate.
No new answer-verbosity, token-limit or prompt-cache flags appear in `--help`.

Validation passed 87 Rust tests, monitor PTY checks, 86 pinned n8n SDK checks
and 156 OpenAI 3.24.0 TCP/SDK/policy checks. The fixture verifies both disabled
auto-update and `AGY_CLI_OSC7501=off` for every subprocess, including probes.
Production-image smoke checks passed health, matching expected/observed 1.4.2
versions, unauthenticated readiness, disabled native execution and n8n cache
disclosure. The actual CLI's unauthenticated stream output parses as JSON and
contains no terminal escape sequences.

Sources: [official ARM64 manifest](https://antigravity-cli-auto-updater-974169037036.us-central1.run.app/manifests/linux_arm64.json),
[official AMD64 manifest](https://antigravity-cli-auto-updater-974169037036.us-central1.run.app/manifests/linux_amd64.json),
[published CLI changelog](https://www.antigravity.google/docs/changelog), and
`agy changelog` from the checksum-verified Linux 1.4.2 executable.

## Earlier AGY 1.3.5 upgrade

The earlier adapter and Compose pin targeted **1.3.5**, as reported by the official
[Linux ARM64 manifest](https://antigravity-cli-auto-updater-974169037036.us-central1.run.app/manifests/linux_arm64.json)
and [Linux AMD64 manifest](https://antigravity-cli-auto-updater-974169037036.us-central1.run.app/manifests/linux_amd64.json).
Both archives' SHA-512 digests were checked. The new ARM64 runtime image reports
`1.3.5`, and its `--help` lists `plan`, `accept-edits` and `auto` execution modes.

The supplied terminal audit of host AGY 1.3.5 reported the same 60 `init.tools`
names as 1.2.15. The versioned public snapshot carries that inventory forward;
the adapter still rejects duplicates, omissions and additions before sending
user input. A fresh unauthenticated Linux CLI exits with an authentication
error before emitting `init`, so this upgrade does not claim a new authenticated
Linux inventory capture or live model acceptance run.

Native `auto` is accepted only behind the existing native enablement and scope
gates. HTTP and runner fixtures check forwarding with `--sandbox`, invalid-mode
rejection and operation without an explicit effort. Answer verbosity keeps its
existing neutral-default/rejection behavior. `/compact` is not exposed because
the router replays submitted history in fresh conversations.

The new production ARM64 image passed health, CLI version, capabilities,
readiness/authentication failure handling and disabled-native smoke checks with
fresh disposable unauthenticated state. Docker policy checks passed using only
synthetic banned-path fixtures. The Rust suite passed 85 tests (51 unit,
22 HTTP and 12 runner), including the new mode regressions. Monitor PTY checks,
156 OpenAI 3.24.0 TCP/SDK/policy checks and 56 n8n Vercel SDK checks also passed
via `./scripts/test-local.sh`. Rust formatting and `git diff --check` passed.

The evidence below records the earlier 1.2.15 acceptance runs; it is not a claim
that those live-provider checks were repeated against 1.3.5.

## n8n prompt cache hint compatibility

Inspection of the running server's private monitor confirmed n8n sends
Responses requests with a string `prompt_cache_key`,
`prompt_cache_retention:"24h"`, `stream:true` and `tool_choice:"auto"`.
The captured error rejected the cache key before starting an AGY run.

Both model APIs now validate and ignore string cache keys and the known
`in_memory`/`24h` retention hints. They do not reach AGY, alter the normalized
prompt/tool history, isolate provider caches or promise a retention period.
Successful hinted responses disclose `X-AI-Router-Prompt-Cache-Controls: ignored`;
capabilities report the same limitation. Malformed hints and unrelated controls
remain rejected.

Validation passed 87 Rust tests (52 unit, 23 HTTP, 12 runner), monitor PTY
checks, 86 pinned n8n Vercel SDK checks and 156 OpenAI 3.24.0 TCP/SDK/policy
checks. Coverage includes both API styles, both streaming modes, tool
handoff/result replay with cache hints, unchanged normalized requests and
usage, and omission/null behavior. A fresh unauthenticated production image
reports the limitation and admits hinted requests to the provider readiness
gate. This does not claim a post-fix live n8n run or deployment.

## Gates

| Gate | Evidence |
| --- | --- |
| P0 plan before implementation | Initial plan/interface commit |
| Official Linux CLI pin | Google download SHA512 checked; Linux ARM64 binary reports 1.2.15 |
| Rust/runner/monitor/HTTP fixtures | Docker suite: 83 passed (51 unit, 21 HTTP, 11 runner), 0 failed |
| Interactive monitor | All six pages at four terminal sizes; synthetic PTY checks for navigation, pause/range, disconnect/recovery, themes, resize, q/Ctrl+C cleanup, text/JSON |
| Local Docker restrictions | Nonroot UID 10001, read-only root, loopback origin, separate named volumes/tmpfs verified |
| Secret-path build policy | Synthetic banned paths excluded before context transfer |
| Network/OpenAI SDK contracts | 76 TCP/SDK 3.24.0/policy checks passed on fresh Docker build |
| Actual production image | 13 unauthenticated smoke checks: health, readiness/auth failures, disabled native, private monitor, CLI pin and graceful exit |
| Shutdown/cleanup | Active model/native SIGTERM persists cancellation, kills descendants and removes owned workspaces; stalled request bodies exit within the test bound |
| Independent review | Findings fixed and focused follow-up review approved; see coverage below |
| Authenticated Linux subscription generation | 15 live OpenAI SDK checks passed on AGY 1.2.15 / gemini-3.8-flash-low: text, both SSE formats, both schema formats, both client tool loops and startup disconnect |
| Login persistence/recreate | Real Linux login survives force-recreation and restart; fresh claude-sonnet-4-6 text request passed after restart |
| Real installed n8n/Hermes flows | Requires installed client configuration and separate full-app testing |
| Native OS sandbox | Requires authenticated synthetic canary checks; disabled by default |

Run `./scripts/test-local.sh` to reproduce local automated checks. Its fake
provider never discovers or invokes the signed-in host `agy`. Test named
volumes are separate from `ai-router-auth` and cleaned by the script.

Production smoke checks used `docker build --target gateway -t ai-router:local .`
and a separate disposable container with synthetic key digests, fresh named
volumes, a loopback port, UID 10001, read-only root, dropped capabilities and
the shipped tmpfs restrictions. The actual official Linux ARM64 CLI reported
1.2.15. Authentication and request readiness remained unavailable as expected;
this is a production packaging check, not a live inference test. The temporary
container and its volumes were removed afterwards.

## Review coverage

Independent reviews covered module integration, API/tool contracts, public
admission limits, ownership and lifecycle. Focused regressions cover:

- Startup cancellation and filesystem failures drain preparation before
  cleanup; pre-existing directories remain unowned and are never deleted.
- Tool schemas with omitted types work through the actual inert MCP relay;
  captured calls and provider output agree before handoff is accepted.
- Native failures publish one terminal event atomically with failed status;
  replay-limit failures produce matching failed telemetry.
- A synthetic 50,000-record monitor history plus 10,000 rejection updates
  retains bounded work. Network floods preserve unsampled fixed counters and
  cap detailed preflight rejection history at 30 records/minute.
- Shutdown waits for final telemetry and ownership-aware cleanup before flush.
  Queue-to-active transitions remain counted, and native cleanup failures
  prevent a successful exit report.

Client bodies that remain undelivered beyond the bounded shutdown drain may
leave an incomplete terminal record; the gateway reports a cleanup failure.
Compose gives shutdown 30 seconds. Sudden process loss cannot guarantee the
last buffered metadata is persisted.

## Operator authentication gate

The development login container has the same UID and auth home as the final
gateway. From this repository, outside Codex:

```sh
./scripts/login.sh --remote
```

Complete the browser/code flow. Do not paste credentials, auth codes or raw
API keys into agent logs. Notify the coordinator after login so real CLI tests
can run without reading saved credential files. The official CLI alone consumes
its cached authentication state.

Do not call fixture success a live-provider result. Record real capability
checks separately, pinned to the CLI/model/client versions. Native must remain
off when its isolation test cannot pass under the shipped Docker restrictions.

## CLI observations

An actual unauthenticated Linux print-mode `/permissions` probe displayed an
authentication prompt and waited, even with a print timeout. The router does
not assume this CLI mode fails immediately: probes close stdin, impose a
separate 20-second process deadline and kill the process group. Readiness stays false,
authentication diagnostics are sanitised, and no generation is admitted.

A nonroot `unshare --user --map-root-user` probe returned `Operation not
permitted` under the shipped restrictions. Native OS isolation is not verified
and remains disabled; this is not treated as permission to weaken the container.

No auth-home file contents were examined. The idle development helper was
stopped before starting the full gateway; ephemeral fixture containers/volumes
are removed by the test script.

## Local service evaluation

The Compose gateway was evaluated through its loopback origin using the
shared named auth volume. Health, bearer enforcement, the disabled native
profile and the private monitor passed before authentication.

The real unauthenticated `agy models` command reported "Please sign in" on
stdout with exit code 1. Probe diagnostics now classify bounded stdout and
stderr, returning only the safe `provider_auth_required` code. The final ten
runner contract tests passed in offline local Docker, including this regression.

Authenticated testing exposed three additional pinned-CLI contract details:

- `init.tools` is the global 60-name registry, including tools disabled in the
  selected agent. The adapter verifies its exact public 1.2.15 inventory, with
  no duplicate, missing or additional entries. Actual native/subagent steps
  still fail model runs. Live synthetic file canaries could not retrieve an
  unknown token, both without tools and with the fixed inert MCP relay.
- `init.agent` echoes the requested name even when that agent is absent. The
  fixed print-mode `/agents` management command discovers parsed manifests:
  valid, missing, malformed and differently named files were distinguished
  with zero turns and zero tokens. This bounded, cancellable preflight now
  precedes the conversation process and user input. The positional `agents`
  subcommand returned an empty list before discovery and is not used.
- `--json-schema` in the isolated model profile echoes the requested schema
  and repeats/concatenates answers, without authoritative structured output.
  Model requests omit that flag, request raw JSON in the owned agent, and
  validate the entire final value in Rust. A live canary returned one valid
  `{"answer":7}` value. Fences, prose, invalid JSON, wrong types and conflicting
  explicit structured output are rejected; nothing is repaired or selected
  from multiple answers. Native schema support remains an independent gate.

The fixed agent declares `tools:[]`, `excludeDefaultComponents:true` and
`inheritMcp:false`. MCP dispatch is supplied dynamically by its sole configured
relay, with a fixed executable, catalogue and workspace cwd. Explicitly listing
`call_mcp_tool` as a built-in component fails CLI component resolution.

The final production image passed all 15 checks in
`tests/live_client_contract.py` using OpenAI Python SDK 3.24.0 over Docker TCP.
Chat and Responses each returned real text, correct SSE termination, valid
schema output and a client-owned weather-tool handoff/result/final-answer
round trip with stable call IDs. The test weather result was synthetic and
executed by the client; the router ran no weather command or network tool.

After the live startup-disconnect check, the private monitor recorded
`client_disconnected`, zero active/queued work and no persistence errors.
There were no retained request workspaces; `docker top` showed only the init
process and gateway. Successful runs exposed actual usage, first-output and
total timings. Earlier bring-up failures remain in the retained history.

After `docker restart ai-router`, readiness returned 200 and a fresh
`claude-sonnet-4-6` Chat request returned the exact restart canary. Earlier
force-recreation preserved the same login and the complete Gemini live suite
passed on the new production image. Monitor history also survived restart.

Live tests used a fresh temporary 64-byte gateway key, held in memory and
passed to the isolated SDK consumer on stdin. It was never printed or saved;
Compose received only its digest. The consumer mounted no AGY auth volume.
Generate and configure operator-owned keys before connecting regular clients;
see [operations](operations.md).

To repeat authenticated checks with an operator-owned model key, build the
test image with `./scripts/test-local.sh`, then run this from the repository
in an interactive terminal. The key is entered without echo and passed on
stdin, with no credential file or auth-volume mount:

```sh
python3 - <<'PY'
import getpass, json, pathlib, subprocess

source = pathlib.Path("tests/live_client_contract.py").read_text()
configuration = {
    "api_key": getpass.getpass("Gateway model API key: "),
    "base_url": "http://ai-router:8080/v1",
}
result = subprocess.run([
    "docker", "run", "--rm", "-i", "--network", "ai-router_default",
    "--user", "10001:10001", "--read-only", "--cap-drop", "ALL",
    "--security-opt", "no-new-privileges", "--entrypoint",
    "/opt/test-venv/bin/python", "ai-router-test:local", "-u", "-c", source,
], input=json.dumps(configuration) + "\n", text=True)
raise SystemExit(result.returncode)
PY
```

After the disconnect check, run `./scripts/monitor.sh --text --range 1h` to
confirm cancellation and zero active/queued requests. This suite uses real
subscription generation; it is deliberately separate from fixture tests.
