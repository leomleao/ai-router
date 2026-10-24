# Local operations

The Compose service listens on `127.0.0.1:8090` by default. For remote use,
put an HTTPS reverse proxy in front of that loopback-only origin. Set
`AI_ROUTER_PORT` in the calling process to change the host port.

Use `--env-file /dev/null` on every Compose command. Compose otherwise reads a
local `.env` file for interpolation. This project does not load dotenv files or
use Compose `env_file` entries; configuration comes from process environment.

## Client keys and startup

Build the direct model-only image and create a client key:

```sh
docker build --target gateway -t ai-router:local .
./scripts/keygen.sh n8n model
```

The key generator displays the raw API key once and prints a digest record.
Keep the raw key in the client credential manager. Export only digest records
in the shell used to start Compose:

```sh
export AI_ROUTER_KEYS='[{"id":"n8n","sha256":"REPLACE_WITH_GENERATED_DIGEST","scopes":["model"]}]'
docker compose --env-file /dev/null up -d
curl --fail http://127.0.0.1:8090/health
curl --fail http://127.0.0.1:8090/ready
```

All keys must have only the `model` scope. Remove legacy `native` scope entries
before starting this version; they are rejected rather than granting reduced
execution permissions. Configuration stores SHA-256 digests,
not raw client keys. To rotate a key, add its digest, restart the service, move
clients to the new raw key, then remove the old digest.

`/health` reports process liveness. `/ready` succeeds when at least one enabled
provider has a ready model. A provider's catalogue is usable only while its
discovery result is fresh and the selected account generation remains current.

## Matched local latency comparison

`tests/latency_compare.py` compares Chat Completions streaming against two
loopback gateways: the archived CLI service at `http://127.0.0.1:8091/v1` and
the direct service at `http://127.0.0.1:8092/v1`. Use it only when both services
are independently running with the same exact model ID available. It sends the
same prompt and request settings sequentially, alternates CLI/direct and
direct/CLI order across samples, and does not retry failures. Direct HTTP
connections ignore proxy environment variables and do not follow redirects. SSE
line reads are capped at 1 MiB, with a 180-second monotonic deadline enforced
through the stream, including heartbeat lines.

Run it from a terminal and provide one JSON line on stdin with `api_key` for a
shared client key, or `cli_key` and `direct_key` for separate client keys. The
keys are used only as local bearer headers and are never printed or included in
the report. Do not put keys in command arguments. For example, the input shape
is `{"cli_key":"…","direct_key":"…"}`. The prompt defaults to
`Reply exactly ROUTER_OK`; `--prompt` can set a different matched prompt.

```sh
python3 tests/latency_compare.py --model EXACT_MODEL_ID --samples 5
```

The helper accepts 1–20 samples (default 5), rejects non-loopback URLs, and
reports raw successful TTFT/total timings, nearest-rank p50/p95, failure counts,
HTTP status plus a bounded static error code, and SHA-256 hashes for the prompt
and each visible output. It never prints prompts, output text, or raw server
errors. A hash mismatch means the two outputs differed; matching hashes do not
establish equivalent quality. This small local sample is an operator
comparison aid, not a performance claim or a substitute for a larger matched
acceptance run. It exits nonzero if any request in the comparison fails.

## Unified provider sign-in

Antigravity uses a new router-owned Google sign-in. The operator approved the
reviewed pinned OMP OAuth registration for this flow. Existing AGY CLI state is
not imported: no documented supported handoff was established. The new state
uses the dedicated `ai-router-antigravity-auth` volume at
`/var/lib/ai-router/antigravity-auth`; the old `ai-router-auth` volume is kept
unmounted for rollback.

Run login before starting the gateway, or stop it first:

```sh
./scripts/login.sh agy
```

The one-off helper uses the same image and only the Antigravity auth volume.
It publishes the OAuth callback on host loopback at port 1456 by default. Use
`--port N` if that port is occupied. For a remote Docker host, open a local SSH
forward, then run the helper on the host:

```sh
ssh -L 1456:127.0.0.1:1456 USER@SERVER
./scripts/login.sh agy login --port 1456
```

Keep the SSH session open until the browser callback completes. Export optional
`AI_ROUTER_GOOGLE_CLIENT_ID` and `AI_ROUTER_GOOGLE_CLIENT_SECRET` overrides in
the process environment when using an operator-owned registration; the default
is the reviewed pinned registration. The helper does not read or copy saved
credential files. Login verifies an existing account/project and does not
automatically enroll a tier or change account provisioning. If provisioning is
needed, complete it through the official application and sign in again.

Inspect status with `./scripts/login.sh agy status`. `logout` removes the
router's local session without remote revocation. `resume` clears the saved
restriction state for the current account generation. Login, logout, and resume
are blocked while the gateway is running so only one process changes the
credential store at a time. Do not delete either Antigravity volume during
rollback or migration.

The unified helper also accepts `agy status|logout|resume`; status uses `docker
exec` while the gateway is running and a network-disabled one-off container
when stopped. OpenAI status and account listing follow the same rule. Both
providers share the helper's strict action/option validation, loopback-only
callback publication, and stopped-gateway requirement for state mutations.
OpenAI logout and resume use outbound networking for remote revocation or grant
renewal; they do not publish callback ports. Other non-login one-offs are offline.

### Adopting a newly reviewed baseline

The 2026-10-24 release adopts `omp-0b5c27ab-antigravity-v3` / Hub 2.26.0,
including OMP's five-minute token-renewal window.
The OAuth registration is unchanged, so an existing matching Google sign-in can
be retained. Build the new image, stop the gateway, inspect
`./scripts/login.sh agy status`, then run `./scripts/login.sh agy resume` before
starting it again. Use the actual image/container/volume overrides for a custom
local deployment. An old saved baseline returns `agy_baseline_update_required`
until this explicit step; the status includes `saved_baseline` and
`baseline_update_required`. Review any existing account/protocol restrictions
before resuming. Do not log in again or delete state just to update the baseline.

See [the upstream review](upstream-review-2026-10-24.md) for evidence, exact
operator commands and the live-validation boundary. None of these changes
requires an additional environment variable.

### Upstream release advisory and manual updates

Follow [the OMP alignment policy](omp-alignment.md) when this advisory changes.
Keeping OAuth and authenticated AGY request behavior aligned with reviewed OMP
changes is an ongoing maintenance requirement.

At startup the router checks the public Antigravity Hub version manifest and
the OMP `main` revision; it repeats both checks hourly. The bounded check uses
fixed public URLs without account credentials, proxy settings, or redirects.
Its result appears in `GET /v1/capabilities` under Antigravity's
`release_advisory` and in the monitor's Provider view. Each source becomes stale
three hours after its last successful check. A failed or stale check is shown as
an advisory condition.

A changed Hub version or OMP revision does not change the compiled identity or
baseline, block otherwise-working Antigravity requests, or affect OpenAI. The
advisory never downloads or applies source or credentials. Compiled baseline
integrity and registration checks still apply before dispatch. Classified
protocol, authentication or account failures from actual provider operations
persist restrictions and require explicit recovery. These checks and restrictions
do not disable OpenAI.

The catalogue contains only the 13 reviewed exact model IDs, and the saved
account roster determines which of those IDs are available. New upstream model
SKUs require a reviewed baseline/code change, tests, and a rebuilt image before
they can appear. Normal OAuth `refresh_token` calls renew an existing session;
they do not update model support, the protocol baseline, or the application's
OAuth client registration.

For an alert, review the public upstream diff from the pinned revision, including
`packages/catalog/src/compat/rules/auth/google-antigravity.kdl`. Confirm whether
the bundled installed-app OAuth registration changed. Its Base64 representation
is public registration data, not a secret; never Base64-encode or publish
personal access tokens, OAuth access tokens, or refresh tokens. If the protocol
or registration needs an update, review and change the checked-in source and
baseline in a code change, then run offline and local Docker validation and
build an explicitly versioned image. Deploy it only through the operator's
separate release process. A revision or manifest alert alone does not require
sign-in. If the reviewed OAuth registration itself changes, perform explicit
fresh consent for the new registration before resuming requests; no automatic
credential migration, registration replacement, version upgrade, or provider
fallback occurs.

`AI_ROUTER_ANTIGRAVITY_ENABLED=false` disables Antigravity routing without
disabling OpenAI. The direct adapter never falls back to a CLI, another account,
or another provider after a failure. The only cross-provider path is model
fallback for chat model IDs the router has never served (see
[capabilities](capabilities.md)); a known ID, including each fallback target,
always reports its own provider's outage.

## OpenAI opt-in

OpenAI has its own OAuth lifecycle and `ai-router-openai-auth` volume. It is
disabled unless `AI_ROUTER_OPENAI_ENABLED=true` is exported when starting the
gateway. Use the same entry point with the provider selector:

```sh
./scripts/login.sh openai
AI_ROUTER_OPENAI_ENABLED=true docker compose --env-file /dev/null up -d
./scripts/login.sh openai status
```

The helper's default callback port is 1455. When using Docker on a remote host,
forward that port over SSH and keep the tunnel open. Use `--port N` or
`AI_ROUTER_OPENAI_CALLBACK_PORT` to select another free port. Use
`./scripts/login.sh openai accounts` to list saved registrations. Select an
account with `./scripts/login.sh openai select ID`. The compatibility wrappers
`scripts/antigravity.sh` and `scripts/openai.sh` call the same implementation.
Account selection,
login, logout, and resume require the gateway to be stopped; the helper checks
this before changing the shared volume. OpenAI account or plan restrictions
leave Antigravity available, and Antigravity restrictions leave OpenAI
available.

`AI_ROUTER_OPENAI_MAX_CONCURRENT` and `AI_ROUTER_OPENAI_MAX_QUEUE` default to
2 and 8. Antigravity has its own `AI_ROUTER_MAX_CONCURRENT` and
`AI_ROUTER_MAX_QUEUE`, also defaulting to 2 and 8. Do not mount the OpenAI auth
volume into the Antigravity helper or the reverse.

### OpenAI usage visibility

The public Sign in with ChatGPT route does not document a numeric remaining-plan
quota endpoint. The monitor reports that remaining allowance is unavailable and
links to ChatGPT Usage; it does not call Codex's separate backend usage APIs.
`subscription_sharing_usage_limit_exceeded` pauses requests until explicit
recovery, but may describe a plan-wide or app-specific limit. It does not prove
that the whole plan is exhausted or provide a reset time. This follows the
[official OpenAI documentation](https://developers.openai.com/siwc/token-sharing-open-source/errors-and-recovery).

### Interpreting OpenAI failures

`server_is_overloaded` means OpenAI reported temporary overload. Chat
Completions and Responses requests return HTTP 503 with the original code and a
fixed explanation to try again shortly, with `retry-after: 30`. Streaming
requests do too, because a stream's HTTP 200 waits for its first output, never
before the provider accepts the request (at most 15 seconds). Only a failure
after that is delivered in-stream: a Chat chunk with a
top-level `error` object, or a Responses `error` event followed by
`response.failed`; its HTTP status cannot change once headers are sent. The router does not automatically
retry, switch accounts or fall back to another provider. Normal successful
provider discovery can clear this transient condition; it does not require
signing in again. While it lasts, known OpenAI IDs, including the fallback
target, return the same 503; only never-served IDs may be served by the
Gemini fallback target, with `X-AI-Router-Model-Fallback`.

An upstream 401 or 403 (for example `invalid_token`) is a 503 with the
provider's code and `x-should-retry: false`: it repeats until the operator signs
in again, so clients are told not to retry. `provider_timeout` (504) also says
not to retry, because each retry is up to another full request deadline of
paid generation. Antigravity overload is a 503 `agy_upstream_error` with
`retry-after: 30`, like OpenAI's.

`invalid_structured_output` means the returned text was invalid JSON or failed
the requested schema; `incomplete_structured_output` means generation ended
before completing that JSON output. Both withhold unvalidated output and have
distinct explanations. `client_disconnected` identifies a client connection
ending before completion.

The selected-request monitor and text snapshots derive these explanations from
the saved provider/error code, so they also explain older records after private
payloads expire or the server restarts. The selected-request panel shows the
upstream request ID when one was supplied. Failure logs add the same fixed
`explanation`; raw provider messages remain private and are not copied into
public errors or persisted logs. Monitor JSON record fields are unchanged.

## Persistent and temporary state

| Location | Contents and lifetime |
| --- | --- |
| `ai-router-antigravity-auth` at `/var/lib/ai-router/antigravity-auth` | Router-owned Antigravity OAuth session and persistent restriction state. |
| `ai-router-openai-auth` at `/var/lib/ai-router/openai-auth` | Separate OpenAI registrations and account state. |
| `ai-router-telemetry` at `/var/lib/ai-router/telemetry` | Bounded request metadata, retained up to the configured days/event count. Also `known-models.json`: the provider of every model ID served, at most 4,096 (model IDs only), so fallback never cross-serves a known model after a restart. It is the only writable volume under the read-only root filesystem that is not credential state. Deleting it is safe but, until each provider's catalogue is probed again, lets a known ID whose provider is down be treated as never seen. A corrupt file is ignored and rewritten. |
| `/run/ai-router` and `/tmp` | Container tmpfs for the private monitor socket and bounded temporary data; cleared at stop. |
| Process memory | Responses output items and provider-private replay state; cleared at restart. |

Keep the old `ai-router-auth` volume for rollback. It is not mounted by the
direct image. Do not run both the old CLI image and the direct router as writers
to one rotating token store. Avoid `docker compose --env-file /dev/null down --volumes` unless intentionally erasing the active provider state and telemetry.
The service uses UID/GID 10001, a read-only root filesystem, dropped
capabilities, no-new-privileges, bounded memory/PIDs, no host home mount, and no
Docker socket.

Responses item storage is client-scoped and separate from disk telemetry. It
holds exact completed output items for 24 hours, up to 1,024 items / 16 MiB per
client and 4,096 items / 64 MiB globally. Oldest items are evicted first when
limits are reached. Reasoning references also require the same provider and
account generation. On expiry, eviction, or restart, resend full history or
start a new conversation. `store:false` disables new item storage but can still
resolve earlier saved items.

## Monitor

```sh
./scripts/monitor.sh
./scripts/monitor.sh --text --range 24h
./scripts/monitor.sh --json --range 1h
```

The private Unix socket is mode 0600 and has no HTTP administration endpoint.
The terminal dashboard has Overview, Usage, Provider, Errors, Events, and Help
pages. Tab, arrows, or `1`–`6` change pages; `r` cycles the time range; arrows
scroll; Space pauses refresh; `?` opens help; `q` exits. Enter opens a selected
request detail. The dashboard refreshes every two seconds and retains the last
snapshot while reconnecting after a socket interruption.

Provider discovery and account readiness are shown independently. The
dashboard includes per-provider quota/model status, active and queued work,
latency percentiles, observed usage, errors, and recent requests. `startup_ms`
measures elapsed time to accepted upstream streaming response headers for both
providers; it includes queueing and request/auth setup and is not process
startup alone. `first_output_ms` measures the first text delta or validated
tool call, excluding private reasoning. Schema mode can withhold output after
that underlying event. Aggregate timing and usage combine both providers, and
each request retains its provider ID. Unknown usage stays unknown; partial
usage remains labelled.

The `counts_since_start` counters include every rejected request since their
`started_at` time, independent of the selected history range, and reset on
restart. Rejection history is sampled to at most 30 records per minute, so
stored history aggregates do not include every rejected attempt.

Persisted telemetry contains bounded metadata. The monitor can also show the
latest 100 request and response bodies in memory, up to 64 KiB each; known
credential fields and encrypted reasoning are redacted. Those details are
cleared on restart and are not written to telemetry files. Keep the socket
private because prompts, schemas, and returned text can appear in request
details.

Graceful shutdown flushes telemetry after provider work drains. Persistence
errors are reported in the monitor; forced termination can lose the most recent
unflushed batch.
