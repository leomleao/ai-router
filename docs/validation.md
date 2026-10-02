# Local validation

All automated tests run in local Docker with explicit fake AGY paths and
synthetic keys/state.

## Gates

| Gate | Evidence |
| --- | --- |
| P0 plan before implementation | Initial plan/interface commit |
| Official Linux CLI pin | Google download SHA512 checked; Linux ARM64 binary reports 1.2.15 |
| Rust/runner/monitor/HTTP fixtures | Fresh integrated Docker suite: 67 passed (40 unit, 20 HTTP, 7 runner), 0 failed |
| Local Docker restrictions | Nonroot UID 10001, read-only root, loopback origin, separate named volumes/tmpfs verified |
| Secret-path build policy | Synthetic banned paths excluded before context transfer |
| Network/OpenAI SDK contracts | 76 TCP/SDK 3.24.0/policy checks passed on fresh Docker build |
| Actual production image | 13 unauthenticated smoke checks: health, readiness/auth failures, disabled native, private monitor, CLI pin and graceful exit |
| Shutdown/cleanup | Active model/native SIGTERM persists cancellation, kills descendants and removes owned workspaces; stalled request bodies exit within the test bound |
| Independent review | Findings fixed and focused follow-up review approved; see coverage below |
| Authenticated Linux subscription generation | Requires operator login |
| Login persistence/recreate | Requires operator login |
| Real installed n8n/Hermes flows | Requires model login and client configuration |
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
AI_ROUTER_CONTAINER=ai-router-login ./scripts/login.sh --remote
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
separate 15-second deadline and kill the process group. Readiness stays false,
authentication diagnostics are sanitised, and no generation is admitted.

A nonroot `unshare --user --map-root-user` probe returned `Operation not
permitted` under the shipped restrictions. Native OS isolation is not verified
and remains disabled; this is not treated as permission to weaken the container.

No auth-home file contents were examined. The development helper is left
available for the operator's login; ephemeral fixture containers/volumes are
removed by the test script.
