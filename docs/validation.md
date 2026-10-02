# Local validation

All automated tests run in local Docker with explicit fake AGY paths and
synthetic keys/state.

## Gates

| Gate | Evidence |
| --- | --- |
| P0 plan before implementation | Initial plan/interface commit |
| Official Linux CLI pin | Google download SHA512 checked; Linux ARM64 binary reports 1.2.15 |
| Rust/runner/monitor/HTTP fixtures | Docker suite: 73 passed (43 unit, 20 HTTP, 10 runner), 0 failed |
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
