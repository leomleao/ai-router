# Direct Antigravity replacement plan

Status: model-only implementation present; local Docker correctness validation
completed on 2026-10-20. Matched CLI latency, real rotating refresh and wider
model acceptance remain release gates; see [validation](validation.md). The operator authorized implementation, local Docker tests and a
separate OMP-registration sign-in. Live server work and other terminals are out of
scope. Secret-file inspection and credential extraction remain prohibited.

Research: [latest OMP analysis and original proposal](alternative-direct-provider.md).
Execution order and review rounds: [implementation DAG](direct-antigravity-dag.md).
Related completed work: [monitor review](monitor-provider-review.md).

## Intended result

Replace AGY subprocess model inference on `main` with a Rust direct
Antigravity adapter. Remove native-agent execution, uploaded-file execution,
artifacts and the official CLI dependency from the resulting runtime. Keep
Chat Completions, Responses, model discovery, client-owned tools, validated
JSON output, existing client authentication, and the independent OpenAI route.

The previous implementation is preserved locally on
`archive/agy-cli-2026-10-20`, at
`4d23e015ba273ed1600ec43c7815f791b4c04286`. It includes the existing dual-provider
CLI implementation; subsequent monitor fixes are separate working-tree changes.
No branch was pushed. Rollback means choosing that implementation and its image,
not silently retrying a direct request through the CLI.

Success means correct client behaviour plus measured lower overhead on matched
workloads. OMP's observed speed motivates the change; it is not a substitute for
measuring this implementation.

## Decisions and accepted assumption ledger

| Item | Decision and reason |
| --- | --- |
| Mainline architecture | Full replacement on `main`; preserve the previous implementation on a separate local branch. Operator decision. |
| Native execution | Remove `/v1/agy/runs` and every child route, native scopes/configuration, artifact storage and sandbox execution. Operator selected a model-only router. |
| Protocol drift | Pin a reviewed wire protocol and client identity. Alert on upstream releases; do not automatically block solely because a manifest changed or cannot be fetched. Operator decision. |
| Failure handling | Persistent protocol/authentication/account-restriction state, explicit recovery; no automatic CLI, account or endpoint fallback. Makes dispatch and billing outcomes predictable. |
| Authentication | Investigate reuse of the existing server sign-in first; preserve the auth volume. If no supported compatible handoff exists, use a reviewed fresh OAuth login. Operator accepted this amendment. |
| Existing clients | Preserve exact discovered model IDs and independent OpenAI routing. No client model-name migration merely to match OMP's collapsed aliases. |
| Initial scope | Text, chronological history, client tools, validated JSON, accurate usage and streaming. Media is a later change with its own contracts. |
| Performance acceptance | Compare matching model ID, effort, prompt/context, tools, schema and output budget. Report medians, tails and failures, not a promised speed ratio. |
| Provider naming | Retain internal/persisted provider ID `agy` for compatibility; display “Antigravity” and adapter revision in new operational views. Reversible naming default. |
| Porting approach | Translate the needed protocol behaviour into Rust; do not run OMP/Node as the provider. Preserve MIT attribution for substantial translations. Reuses existing Rust deployment. |
| Output policy | Visible text is preserved verbatim; private reasoning state stays opaque. No OMP Flash content-removal heuristics. Required for a transparent gateway. |
| Client feature claims | Keep forced/named tool choices and unverified hard output limits unsupported. Do not expand advertised guarantees based only on upstream request fields. |
| Remote provisioning | Do not silently enroll a tier or change account/project configuration. Report required provisioning and use an explicit operator workflow if necessary. |

The operator accepted the ledger, including reuse-first authentication, in this
session. Reopen a decision only if implementation evidence invalidates it.

## Existing foundations

The dual-provider seam already exists. Extend `src/providers.rs` exact ownership
and readiness, `src/protocol.rs` typed requests, shared `RunEvent`/`RunResult`,
`src/api.rs` admission/renderers, and `src/items.rs` ownership/provenance.
Do not restart the completed OpenAI refactor or infer provider ownership from
model prefixes. The existing OpenAI OAuth lifecycle remains independent.

AGY currently adds CLI transcript/policy rendering, a workspace, configuration
verification, a model subprocess and, for tools, an MCP relay. Remove those
requirements from direct inference. Do not remove shared schema/tool validation
just because the relay disappears. Separate truly CLI-only dependencies from
utilities also used by the direct adapter or OpenAI.

## Authentication feasibility before committing the migration

Investigate a documented official authentication interface that can supply
renewable credentials suitable for the reviewed direct endpoint. Official
shared sign-in is evidence for official local surfaces, not proof that a Rust
Cloud Code Assist client can use a CLI token profile.

Record only nonsecret outcomes: supported interface found, refresh supported,
scope/audience compatibility, issuing OAuth registration, project resolution,
storage isolation and refresh ownership. No token values, account dumps or raw
profiles belong in evidence. Codex must not read/load `.env*`, PEM/key files,
or anything below `secrets`/`credentials`; secret-dependent checks are performed
by the operator outside Codex. Do not circumvent this through a copying helper.

Choose one credential authority per account. A one-time supported migration
must not leave CLI and router independently rotating the same refresh token.
If reuse requires keeping a CLI token broker in production, that conflicts with
the selected CLI-free target: report the tradeoff rather than hiding it.

If no supported compatible handoff exists, implement fresh Google consent with
a reviewed OAuth registration, strict state validation, PKCE where supported,
bounded loopback callbacks and explicit remote-server operation. Do not assume
OMP's bundled client registration can be reused. Project onboarding, userinfo,
token refresh and revocation must use reviewed destinations and avoid logging
credentials. Store router-owned state in a new dedicated nonroot named volume.
Keep old AGY state intact for rollback and preserve OpenAI state separately.

This feasibility checkpoint can block live evaluation while offline serializer,
decoder and integration work proceeds against synthetic credentials/services.

## Transport, compatibility and shutdown state

Use a pooled Rust HTTP client with redirects disabled. Define fixed reviewed
HTTPS origins and allowed paths separately for OAuth, project discovery,
inference, model discovery, quota and optional public release metadata. No
caller-controlled base URLs, headers, endpoint fallback or identity overrides.
Check DNS/TLS/redirect handling and enforce outbound origin restrictions in the
deployment boundary as well as application dispatch where practical.

Every authenticated operation goes through one transport/gate, including refresh,
provisioning, background probes and any future retry. Check the selected account
generation, approved baseline, endpoint and persistent restriction state
immediately before dispatch. Bound request/response bytes, queued work, SSE
frames, item counts and elapsed time. Carry one original absolute deadline
through queueing, authentication, headers, first event, body and cleanup.

The checked-in baseline records adapter revision, source provenance, supported
request/response schemas, model/effort mappings, identity fields and allowed
origins. It must distinguish Hub observations from independently documented CLI
behaviour. Offline fixtures prove agreement with that baseline, not permission
to access the service or protection from account suspension.

State categories must remain distinct:

- Protocol incompatibility disables the affected direct adapter until review.
- Authentication/account restrictions stop the affected Antigravity account's
  authenticated traffic across inference and control-plane paths. Persist state
  across restart; recovery is explicit and tied to account generation.
- Ordinary rate/quota exhaustion uses bounded model-family cooldowns and any
  validated reset metadata. It must not become a permanent protocol failure.
- Transient transport failures return a bounded, sanitized error. Default
  generation calls to one attempt; never retry after visible output or reroute
  to another provider/account/endpoint.

Stop queued dispatch when a gate closes and cancel affected in-flight work.
Distinguish already-sent requests from prevented dispatch. An Antigravity
restriction must not disable OpenAI. Public release checks are advisory: cache
bounded metadata, show freshness/alerts, and never update identity automatically.

## Conversation, tools, reasoning and usage

Serialize system/instructions and typed user/assistant/tool turns directly.
Preserve chronology, original tool IDs, tool names and schema meaning. Handle
multiple calls only after fixtures establish the backend's ordering semantics.
Reorder tool results into the original call group where necessary; never match
unrelated results by position or reinterpret tool output as instructions.

Decode bounded UTF-8/SSE across arbitrary chunk boundaries. Detect in-band
errors, invalid JSON, truncation, missing finish markers, duplicate/conflicting
items, incomplete results and disconnects. Emit exactly one terminal state.
Publish client tool calls only after complete validated arguments and a valid
handoff terminal. Schema mode continues to withhold provisional text until the
complete output validates. Reject unsupported forced/named choice initially.
For `tool_choice:none`, omit tools from the wire inventory and reject any
unexpected returned tool call; never execute or forward it to the client.

Preserve provider-required thought/signature state in owner-, model-, account-
and generation-scoped bounded memory, including signature-only empty thoughts.
Do not expose private chain-of-thought in Chat/Responses output, monitor details,
logs or durable telemetry. Test Chat-to-Responses and Responses item replay,
cross-provider rejection, TTL/eviction, restart and account switching.

Map prompt, candidate, reasoning, cached and total token counts with explicit
semantics. Reasoning can be included in output counts; do not add it again.
Mark missing/inferred usage partial. Preserve the generic partial-usage field
and the intentional legacy alias until a separately versioned API change.
Request budgets remain guidance unless a tested enforceable limit exists.

## Exact catalogue and capabilities

Preserve the current IDs when present and usable in the actual raw roster:

| Family | IDs |
| --- | --- |
| Argon | `gemini-4-argon-low`, `gemini-4-argon-medium`, `gemini-4-argon-high` |
| Flash | `gemini-3.8-flash-low`, `gemini-3.8-flash-medium`, `gemini-3.8-flash-high` |
| Opus | `claude-opus-5-5-low`, `claude-opus-5-5-medium`, `claude-opus-5-5-high` |
| Sonnet | `claude-sonnet-5-5-low`, `claude-sonnet-5-5-medium`, `claude-sonnet-5-5-high` |
| GPT-OSS | `gpt-oss-120b-medium` |

Treat a recognized empty roster differently from malformed data. Filter disabled,
internal and denied rows; preserve last-known ownership to return an actionable
unavailable error for cached IDs. Account rejection stops discovery without
trying another origin. Conflicting providers/IDs fail closed. Model-specific
effort and output behaviour require fixtures and later live validation; suffixes
alone do not establish numeric budgets or capability ceilings.

Replace CLI-version/sandbox-verification capability fields with adapter/baseline
metadata through a documented schema migration. Explicitly remove native support.
Preserve API paths for retained model endpoints, model identity, client key scope
semantics and OpenAI capability/readiness reporting. Remove native routes rather
than returning success for a reduced implementation; document their 404 outcome.

## Docker and server migration

Remove official CLI installation/downloads, sandbox execution dependencies and
request-workspace/MCP startup configuration after model-only integration passes.
Use a slim nonroot read-only runtime, bounded temporary storage, private monitor
socket and independent provider state volumes. Do not erase the legacy AGY auth
volume or telemetry to simplify migration.

The live server and `/Users/leo/dev/home-server-docker` are out of scope under
the latest operator instruction. Prepare migration guidance in this repository
only; do not edit server Compose, run SSH or use other terminals. A later server
migration can cover Compose, login/status helpers and backup/restore coverage.
Do not load that project's `.env` or service `env_file`. Validate a sanitized
configuration with synthetic values and the protected paths removed, or ask the
operator to run secret-dependent Compose actions outside Codex. Document which
volume the old and new images each own; never mount a live rotating credential
store into both writers. Build and tag a rollback image before cutover.

Preparing this plan did not authorize push, server deployment or sign-in.
The operator subsequently authorized implementation and local OAuth; those
completed local checks are recorded in [validation](validation.md). Push and
live-server deployment remain outside this implementation run.

## Validation and evidence

Run deterministic Rust, HTTP/SDK and monitor checks with synthetic services and
external networking disabled. Cover the OMP edge cases in the research, both
client APIs in stream/nonstream modes, text/history/tools/schema/incomplete
output, malformed rosters, restriction persistence, refresh races, concurrent
gate closure, stale discovery, disconnect/cancellation and independent OpenAI
availability. Removal checks must prove no `agy` process, native route, MCP relay
or sandbox execution is required by model requests.

After the auth checkpoint, use an operator-authorized matched comparison of the
archived CLI, direct adapter and, where reproducibly configurable, local OMP.
Record the OMP version/settings at test time; the installed binary's version is
currently unverified. Interleave runs, keep concurrency equal, record warm/cold
conditions and cached context, and report sample counts, p50/p95, errors and
usage. Measure queue/auth/header/first-event/first-public-output/total separately;
retain provider-specific milestone definitions. Include short text, long context,
tool continuation and schema output at low and high effort where available.

Require repeatable lower startup/first-output overhead for matched successful
workloads, no correctness regressions and acceptable failure tails. Establish
the baseline before setting a numeric improvement target. Report any workload
where reasoning/backend time dominates or direct inference does not improve.

Finish each implementation round with an independent review and only rerun
affected checks after fixes. Record evidence and remaining limitations before
releasing; do not mark live gates complete from mocked services or model listing.
