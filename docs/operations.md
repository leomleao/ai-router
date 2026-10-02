# Local operations

Run the gateway in Docker with a loopback-only origin at `127.0.0.1:8090`.
For remote access, configure an HTTPS reverse proxy on your own domain.
Change the loopback port with `AI_ROUTER_PORT`.

The image contains the official Linux AGY **1.2.15** executable. Installation
uses immutable release URLs and SHA-512 values from the official installer’s
Linux ARM64/AMD64 manifests, then checks `agy --version`. The documented
`AGY_CLI_DISABLE_AUTO_UPDATE=true` setting prevents background updates from
changing the tested pin or trying to write the read-only executable. It does not extract
Google credentials, copy host authentication, or install a paid API provider.

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
**q**, **Esc**, or **Ctrl+C** exits and restores the terminal. Over SSH,
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

Router telemetry does not include prompts, generated text, tool arguments,
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
Python and the OpenAI SDK are installed in the test image only.

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
