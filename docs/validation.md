# Local validation

All automated tests run in local Docker with explicit fake AGY paths and
synthetic keys/state.

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
