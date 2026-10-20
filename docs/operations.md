# Local operations

Run the gateway in Docker with a loopback-only origin at `127.0.0.1:8090`.
For remote access, configure an HTTPS reverse proxy on your own domain.
Change the loopback port with `AI_ROUTER_PORT`.

The image contains the official Linux AGY **1.4.3** executable. Installation
uses immutable release URLs and SHA-512 values from the official installer’s
Linux ARM64/AMD64 manifests, then checks `agy --version`. The documented
`AGY_CLI_DISABLE_AUTO_UPDATE=true` setting prevents background updates from
changing the tested pin or trying to write the read-only executable. It does not extract
Google credentials, copy host authentication, or install a paid API provider.

The runner and image set `AGY_CLI_OSC7501=off` so headless JSON pipes do not
carry terminal-status escape sequences. Release upgrades update the immutable
URL, both checksums, the compiled adapter version and its inventory contract
together. If your deployment overrides `AI_ROUTER_EXPECTED_AGY_VERSION`, change
it to `1.4.3` when deploying this release, or remove that override to use the
compiled default. This repository's Compose file pins the matching version.

## Build and keys

```sh
docker build -t ai-router:local .
./scripts/keygen.sh n8n model
./scripts/keygen.sh hermes model
```

The key generator prints a new raw key and its digest record. Save raw keys in
the client’s credential manager. Export only the JSON array of digest records
as `AI_ROUTER_KEYS` in your calling shell. The router never needs the raw keys.
Do not paste real keys into terminal recordings or application logs. Rotation
means generating another key, updating the digest array, restarting the service,
then removing the old record after clients switch.

Use an explicit empty Compose environment file so Compose does not implicitly
load a local `.env` file. Values below come from exported process environment.

```sh
docker compose --env-file /dev/null up -d
curl --fail http://127.0.0.1:8090/health
curl --fail http://127.0.0.1:8090/ready
```

Health indicates the gateway process is alive. Readiness additionally reflects
the bounded, cached AGY version/authentication probe. Missing or expired AGY
login does not require restarting the gateway repeatedly. Generation remains
unavailable until the operator signs in successfully.

## Operator authentication

The operator can log in after the full gateway starts:

```sh
./scripts/login.sh --remote
```

This runs `docker exec -it --user 10001:10001` with the same
`HOME=/var/lib/ai-router/auth` as generation. It forwards `SSH_CONNECTION` when
present, or supplies a remote-session marker for local Docker. AGY’s documented
SSH flow prints a URL for the operator’s local browser and asks for the returned
code. Container detection of that flow is a real acceptance gate; if AGY behaves
differently, keep authentication operator-driven and investigate the CLI rather
than reading its saved tokens. No SSH daemon runs inside the image.

Authentication can happen before the Rust gateway build finishes:

```sh
docker build --target agy-runtime -t ai-router-agy:local .
docker run -d --name ai-router-login --init \
  --user 10001:10001 --read-only --cap-drop ALL \
  --security-opt no-new-privileges \
  --mount type=volume,source=ai-router-auth,target=/var/lib/ai-router/auth \
  --tmpfs /run/ai-router:rw,nosuid,nodev,noexec,size=256m,uid=10001,gid=10001,mode=0700 \
  ai-router-agy:local
AI_ROUTER_CONTAINER=ai-router-login ./scripts/login.sh --remote
docker rm -f ai-router-login
```

Removing this helper container preserves `ai-router-auth`, which the full
Compose service uses. Never run both containers’ AGY processes against that
volume concurrently. Before claiming persistent login support, verify fresh
processes, container restart, and container recreation on the actual Linux
image. A successful host macOS login does not prove these cases.

## Storage and permissions

| Location | Lifecycle | Contents |
| --- | --- | --- |
| Named volume `ai-router-auth` | Persists through container replacement | AGY-owned login and CLI state |
| Named volume `ai-router-telemetry` | Persists through container replacement | Router request metadata, bounded to 30 days and 50,000 events |
| `/run/ai-router/workspaces` on tmpfs | Disappears when the container stops | Request workspaces, uploads, temporary artifacts |
| `/run/ai-router/monitor.sock` on tmpfs | Disappears when the container stops | Private read-only monitor socket, mode 0600 |
| Router process memory | Cleared by restart/redeployment | Exact completed Responses output items, scoped to configured client ID; enabled unless `store:false` |

The Responses item store holds raw output content so `item_reference` can replay
messages and function calls exactly. It is separate from redacted telemetry and
is never written to the router's volumes. Items expire after 24 hours, with
earlier oldest-first eviction at 1,024 items / 16 MiB per client or 4,096 items /
64 MiB globally (serialized JSON bytes; allocation overhead is additional).
Expiration runs on access and the periodic provider refresh. Successful
Responses advertise `X-AI-Router-Item-Storage: memory` or `disabled`; capabilities
publish the same limits. `store:false` disables storage of new outputs but does
not remove prior items or affect AGY's own state. Keep configured client IDs
unique to their owners. On restart, eviction or expiry, clients must resend full
history or start a new chat. Start a new n8n chat when first deploying this
feature because older response IDs have no saved content.

These are Docker-managed named volumes, with no manually managed host folder.
AGY may persist its own conversations/cache inside its state volume. The
router’s metadata-only logging rule does not promise that AGY stores no content.
Provider state retention must be evaluated separately after actual CLI tests.
On gateway startup, fixed service permissions and an empty global MCP catalogue
are written without reading saved credential files. Use this dedicated volume;
do not install personal hooks, agents, plugins, or MCP servers in the service
home. Native verification must also test a synthetic poisoned configuration to
prove inheritance cannot start unexpected processes.
Do not run `docker compose down --volumes` unless you intend to erase login and
monitor history. Any later server backup integration must list these volumes.

The runtime has a read-only root filesystem, fixed UID/GID 10001, no added Linux
capabilities, no privilege escalation, bounded memory/PIDs, and no Docker socket
or host home mount. The HTTP port binds exclusively to host loopback. Configure
exact trusted reverse-proxy peer IPs in `AI_ROUTER_TRUSTED_PROXIES` if forwarded
client IPs are needed; untrusted forwarded headers must not bypass rate limits.

Native tool execution defaults off. AGY’s Linux terminal sandbox uses kernel
namespaces and may be unavailable under Docker’s default seccomp/user-namespace
policy, dropped capabilities, or a host that disallows unprivileged namespaces.
Installing `bubblewrap` is not proof of containment. Verify synthetic workspace
canaries, denied outside reads/writes, denied unsandboxed execution, and cleanup
before enabling native capabilities. Do not fix a sandbox failure by enabling
`privileged`, broad permission skipping, or unrestricted host mounts. The stock
Compose file keeps native execution disabled.

## Monitor

```sh
./scripts/monitor.sh
./scripts/monitor.sh --text --range 24h
./scripts/monitor.sh --json --range 1h
```

The default terminal view is a full-screen dashboard with coloured panels,
a request-volume chart, latency percentiles, and scrollable tables. Its pages
are Overview, Usage, Provider (including quota bars), Errors, Events, and Help.
It refreshes every two seconds and adapts to the terminal size.

Use **Tab**, **←/→**, or **1–6** to change pages, **r** to cycle through
1h/24h/7d/30d, **↑/↓** to scroll, and **Space** to pause or resume refresh.
**t** cycles warm/cool/mono colours; **Ctrl+T** switches light/dark colours.

The selected-request panel shows full identifiers, endpoint, UTC start time,
status/error code, exact latency values, and observed token counts, with complete,
partial, or unknown usage clearly labelled. **Enter** expands the panel;
**PgUp/PgDn** scroll long details, **↑/↓** selects another request, and
**Enter/Esc** returns to the request list. Short terminals prioritise the request
list and details over the overview charts.

The expanded view includes the original request JSON (messages/input, tools,
schemas, and parameters) and response JSON, including returned text and tool-call
arguments. Streaming calls show the assembled final response; native calls show
the final provider result. **a** shows all data, **i** the request, and **o** the
response. **PgUp/PgDn** scrolls the selected view. The selected request stays
selected while new calls arrive.

Call data is retained for the latest 100 authenticated JSON calls in memory,
up to 64 KiB per request and response; larger bodies are marked truncated.
Restarting clears it. Older requests show their saved metadata with an explicit
data-unavailable message. HTTP authentication headers are excluded; known
credential fields (including JSON-encoded tool arguments) are redacted, and
uploaded file bytes are omitted. These call bodies are retrieved on demand over
the same private socket and are absent from telemetry files and `--json` snapshots.

**q** or **Ctrl+C** exits and restores the terminal; **Esc** also exits from
the normal dashboard. Over SSH,
allocate a terminal with `ssh -t leo@server '~/home-server-docker/ai-router/monitor.sh'`.
If the private socket disconnects, the dashboard keeps the last snapshot visible
and retries with a clear disconnected status.

Text and JSON modes
return one snapshot, suitable for pipes or local scheduled checks. The monitor
uses a read-only Unix socket; it has no HTTP admin endpoint or public port.
It reports active/queued work, cached AGY authentication/model/quota status,
request/failure counts, observed token usage, AGY readiness, first-output and total latency,
and recent request IDs. AGY readiness latency includes workspace preparation
and process startup through the verified CLI init event. Partial usage is labelled separately; unknown usage
is not replaced with invented token counts. Quota timestamps indicate when
the provider was actually checked.

`counts_since_start` counts every rejected request in six fixed categories:
authentication, rate, scope, input, capacity, and other. These in-memory counters
reset when the gateway process restarts; their `started_at` timestamp identifies
that window, and monitor `--range` filters do not change them. Rejection history
is sampled to at most 30 records per minute. Retained-history request/failure
aggregates therefore describe stored events, while these rejection counters
include floods that are omitted from the history.

Persisted router telemetry does not include prompts, generated text, tool arguments,
headers, credentials, or arbitrary provider error payloads. Records and
aggregates use bounded identifiers/error codes. Persistence uses atomic mode
0600 snapshots and batches updates every second so HTTP admission does not
wait for disk. Graceful shutdown flushes; forced termination can lose the latest
unflushed monitoring batch. Persistence failures appear in snapshots
without blocking generation.
Compose allows 30 seconds before forced termination so the gateway's bounded
HTTP shutdown and cleanup drain can finish and flush their metadata.
The history holds at most 50,000 records. Expired records are pruned at startup
and at most once per minute during operation, including when traffic is idle.

### Rejected client-tool diagnostics

AGY sees private tool aliases (`router_client_000`, etc.) mapped to the current
client names in its owned agent catalogue. This avoids exposing a client tool
such as `write_todos` under the same MCP name as an AGY native tool. The CLI
loads each tool eagerly as a direct function, for example
`mcp_ai-router_router_client_000`. The server binding is fixed; the model no
longer supplies `ServerName`, `ToolName` or `Arguments`. Its client arguments go
inside `input`, separate from the CLI-required `toolSummary` metadata. n8n still
receives the original function name and validated client arguments.
Invalid-argument feedback repeats the direct `provider_name` for correction.
Alias indexes belong only to the current request; historical client names do
not establish a provider route. No changes to n8n's tool definitions are needed.

This reduces model selection mistakes; it does not guarantee compliance by a
live model. Real native calls, unknown aliases, another MCP server or a nested
dispatcher remain terminal boundary errors. Do not bypass these checks to
accept a name that merely resembles a supplied client tool.

Container stderr emits a JSON `client_tool_rejected` event for each call failing
relay/handoff validation, crosses the model profile's tool boundary, or completes
without its required handoff. Each event includes `request_id`, `model`, `stage`
(`relay`, `handoff`, `dispatch` or `completion`) and a `diagnostic` object.
Correlate its request ID with the existing `model_request` tool inventory and
the private monitor's failure record:

```sh
docker logs --since 15m ai-router 2>&1 | jq -R 'fromjson? | select(.event == "client_tool_rejected")'
```

Reasons distinguish `unknown_tool`, `invalid_tool_name`, `arguments_not_object`,
`schema_mismatch`, `invalid_tool_schema`, `invalid_call_id`, `malformed_handoff`,
`relay_rejected`, `unapproved_provider_tool`, `unapproved_mcp_server`,
`malformed_dispatch`, `invalid_tool_envelope` and `missing_handoff`. Dispatch
events also report parameter/field JSON types, whether a subagent was reported,
and `server_name_status` (`missing`, `null`, `expected`, `other`, `non_string`).
They never include the raw server name or encoded argument envelopes.
Valid identifier-shaped selected
tool names are recorded up to 64 ASCII characters; other names are omitted.
Schema failures include up to eight errors, each with the schema path (up to
256 characters), instance nesting depth and actual JSON type. Type constraints
also report expected types; required constraints report up to eight missing
schema-declared field names, each limited to 64 characters. Truncation flags
identify limited error lists and schema paths.

Known tools with invalid arguments get up to two correction opportunities
inside the original AGY run. Each `client_tool_rejected` relay event records
`rejection_attempt` (1–3), `correction_offered` and `remaining_corrections`.
The first two failures return bounded MCP validation feedback; the third
ends the request. A valid corrected selection emits `client_tool_call_corrected`
with the request ID, model, selected tool and number of preceding rejections.
Unknown tools, disallowed dispatches, invalid schemas and malformed handoffs
remain immediate failures. Corrections keep the original deadline, cancellation
and IO limits; they may consume more tokens/time. They never execute client
tools or start another CLI run, and only the validated final call reaches n8n.
The request-wide counter survives MCP relay restarts via fixed numbered files.
Capabilities publish the two-correction budget; no new configuration is needed.

For additional-property failures, provider-only feedback includes `field_feedback`:
the unexpected field names, declared `allowed_fields`, schema path and instance
JSON pointer for each failing object. Root instance paths are the empty string.
Local JSON-pointer `$ref` traversals are resolved for declared field lists;
unresolved locations use `allowed_fields_available:false` rather than guessing.
`anyOf`/`oneOf` branch hints retain separate schema paths and must not be merged
into a single allowed-field list. Pattern-based fields are indicated separately.
Schemas that permit no properties report an empty allowed-field list.

Provider hints are bounded to eight objects, 16 names per list, 64 characters per
name and 256 characters per path. The serialized hint array is capped at 8 KiB;
branch traversal stops after 64 errors or eight nested branch levels. Flags
identify truncated names, lists, paths and hints; AGY is instructed to consult
the complete `inputSchema` when a limit is reached. No argument values, enum
values, schema descriptions or raw validator error messages are included.
Dynamic names and instance paths are excluded from both JSON and Debug log
formatting. They are sent only back to the provider that generated the call,
not attached to HTTP errors, monitor records or persisted router telemetry.

These are metadata-only container logs: no argument values, dynamic argument
keys/instance paths, enum values, schema descriptions, prompts or raw validator
messages are emitted. Rejected raw arguments travel only through the existing
owned temporary handoff/rejection files (at most three rejection files, each
bounded to 1 MiB), removed with the request workspace.
HTTP errors and the private monitor retain their existing generic error codes;
diagnostic details are not added to API responses or persisted router telemetry.
Diagnostics apply to new failures after deployment; previous failed calls cannot
be reconstructed from their generic errors. Recreating the router also clears
its Responses item store, so start a new n8n chat afterward.

The feedback uses [MCP tool errors](https://modelcontextprotocol.io/specification/2025-11-25/server/tools#error-handling)
with `isError:true`, so AGY receives actionable validation feedback without a
fabricated successful tool result. Automated correction tests prove the adapter;
whether a particular live model corrects its call remains an operator check.

## Verification boundaries

```sh
./scripts/test-local.sh
python3 scripts/test-docker-policy.py
```

The first command builds the dedicated test image, runs Rust checks, and runs
the actual OpenAI Python SDK 3.24.0 over loopback TCP inside an isolated Docker
container. It exercises Chat/Responses text and streams, the SDK stream
accumulator, client tool handoffs and results, JSON schema, admission/failure
limits, native artifact transport fixtures, and monitoring persistence. The
test image's provider path points explicitly to the synthetic executable; its
temporary named volumes have unique `ai-router-contract-*` names and are
removed after the run. It uses neither the operator's auth volume nor host AGY.
Python and the OpenAI SDK are installed in the test image only. A separate
build step runs the official pinned CLI with a loopback mock Gemini endpoint
and external networking disabled. It verifies real custom-agent loading,
direct MCP dispatch, nested schemas and CLI metadata handling in fresh
synthetic state. Only version discovery and conversation execution use the
official binary; quota/model/agent-list probe metadata is synthetic. No saved
login or real inference is used by this test.

The second command uses host Python to drive Docker/Compose configuration and
build-context checks with freshly created synthetic dummy files. It passes
`--env-file /dev/null` to Compose and verifies excluded paths are absent from
the Docker build. It never opens real credential files.

Deterministic tests use an explicit synthetic fake AGY executable. They cover
contracts and failure handling without invoking the signed-in host CLI. Real
AGY authentication, subscriptions, CLI streaming/schema/tool handoffs, n8n and
Hermes application behaviour, and native containment are distinct acceptance
checks. Until each real check passes, capabilities must identify it as pending.

The authenticated local evaluation now passes both API styles, both streams,
both schema formats and client-tool loops on Gemini, plus Claude text after
restart. Login survives restart and container recreation. Full n8n/Hermes app
configuration and native
containment remain separate gates. See [validation](validation.md) for results
and the opt-in live SDK command. The evaluation key is temporary; configure
operator-owned client keys using the procedure above.

Sources: [official installation and authentication](https://www.antigravity.google/docs/cli/install/),
[official terminal sandbox](https://www.antigravity.google/docs/sandbox?tab=cli),
[official self-updater troubleshooting](https://www.antigravity.google/docs/cli/troubleshooting/),
[official Linux ARM64 manifest](https://antigravity-cli-auto-updater-974169037036.us-central1.run.app/manifests/linux_arm64.json),
[official Linux AMD64 manifest](https://antigravity-cli-auto-updater-974169037036.us-central1.run.app/manifests/linux_amd64.json).
