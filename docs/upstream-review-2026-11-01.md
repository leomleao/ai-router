# Upstream review — 2026-11-01

Reviewed OMP [`686b5e2f`](https://github.com/can1357/oh-my-pi/tree/686b5e2fb542848dc3ff1a9e36a42a8fefdd2278)
against [`0b5c27ab`](https://github.com/can1357/oh-my-pi/tree/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0).
The official [Hub manifest](https://antigravity-hub-auto-updater-974169037036.us-central1.run.app/manifest/latest-arm64-mac.yml)
reported **2.28.2**. The reviewed adapter is `omp-686b5e2f-antigravity-v4`.

This review used public source and release metadata. It did not inspect saved
OAuth state or protected files, modify the server, or use the authenticated
local gateways. See [the alignment policy](omp-alignment.md).

## Changes incorporated

- The [OAuth rule](https://github.com/can1357/oh-my-pi/blob/686b5e2fb542848dc3ff1a9e36a42a8fefdd2278/packages/catalog/src/compat/rules/auth/google-antigravity.kdl)
  adds `https://www.googleapis.com/auth/aicode` and explicitly enables PKCE.
  The other five scopes, public registration and Google endpoints are unchanged.
  The router already uses PKCE. It now requests all six scopes and saves grant
  evidence. Old or incomplete grants cannot reach token refresh or generation;
  status reports `scope_update_required`, with a fixed recovery message.
  Resume never invents a permission. Fresh authorization can omit `scope` when
  the grant equals the requested scopes; refresh omission preserves existing
  evidence. Incomplete explicit grants are rejected.
- Follow OMP's [native header constructor](https://github.com/can1357/oh-my-pi/blob/686b5e2fb542848dc3ff1a9e36a42a8fefdd2278/packages/catalog/src/wire/gemini-headers.ts):
  Hub 2.28.2, changelist `1005016104`, and the native build for the host OS
  (Linux/amd64, Darwin/arm64, Windows/amd64; Linux fallback). As in OMP, this
  identifies the shipped build rather than the host CPU architecture.
  CCA requests offer gzip, transparently decode it within existing decoded-byte
  limits/deadlines, and send no explicit SSE/JSON `Accept` header. Claude 5.5
  routes omit the older interleaved-thinking beta header.
- Match the new stateless request labels: cascade/root cascade, trajectory,
  request counter, last step and non-Gemini usage. Context turns and individual
  tool results determine the step. IDs remain ephemeral per call, matching OMP
  direct callers without `providerSessionState`; the router does not add a
  cross-request trajectory counter cache.
- Apply optional [discovered model profiles](https://github.com/can1357/oh-my-pi/blob/686b5e2fb542848dc3ff1a9e36a42a8fefdd2278/packages/catalog/src/discovery/antigravity.ts)
  from the serving account: model enum, fixed output ceiling and thinking
  settings. Numeric levels 1/2/3 map to LOW/MEDIUM/HIGH and take precedence over
  numeric budgets; `-1` retains dynamic budgeting. Profiles are bounded and
  keyed to the exact reviewed model and account generation. Missing constants
  are not guessed or borrowed. Discovery already sent the project, as OMP now
  explicitly does. Caller output budgets remain visible-answer prompt guidance;
  discovered provider ceilings are separate wire constants.
- In-band stream errors retain safe diagnostic counts and up to eight
  allowlisted Google RPC detail type names. Arbitrary messages, violations and
  detail payloads remain excluded from persisted diagnostics. Existing private
  bounded captures and retention rules remain intact.

## Review scope and intentional differences

The source/hash matrix in [the baseline](antigravity-baseline.json) covers **65
public source and test files**, extending the previous 59-file review with auth
and catalog types, stream helpers and three upstream regression files. Both
revision hashes are recorded per path. Decoded public registration fingerprints,
six scopes and explicit PKCE were independently checked against that source.
This is a scoped protocol review, not an audit of every OMP dependency.

Changed shared schema code includes immutable transforms/memoization and
Codex-specific array repair; it does not require replacing the router's
separately tested CCA schema projection. General stream timeout/cancellation
helpers and multi-account refresh provenance belong to OMP's own runtime; this
router keeps bounded I/O, serialized refresh, dispatch barriers and generation
isolation. Other model/catalog families are not automatically adopted.

The existing 13 reviewed IDs, routing/effort contract, private replay, strict
tool enforcement and non-strict warning/relay behavior are retained. The new
profile path does not change tool-call validation. OMP's offline profile table
contains older IDs outside this inventory; the router uses serving-account
discovery only. Runtime version checks remain advisory; identity changes require
reviewed builds. Endpoint failover, automatic project enrollment and agent-side
tool execution remain outside this adapter's contract.

## Validation and limits

With external networking disabled, the isolated Docker environment passed:

- **258 Rust tests**, including actual TCP official Python, Vercel, LangChain
  and Studio client contracts, streaming failures and tool continuations.
- New request-label, gzip/header, profile precedence/bounds and end-to-end
  discovery-to-dispatch fixtures. A generation change makes old profiles
  unavailable until rediscovery.
- Legacy sessions remain readable, but missing scope evidence blocks dispatch.
  Baseline resume preserves the session without adding grants; explicit fresh
  consent restores availability. Refresh cannot manufacture the added scope.
- Safe SSE detail metadata excludes arbitrary provider payload text.
- Updated binary build, monitor PTY checks, five login-helper tests, Docker
  policy/protected-context checks, formatting and whitespace checks.

Only the required gzip dependency chain was added; existing locked dependency
versions were retained. No live Google consent, real token-expiry cycle, live
generation or deployed-server acceptance is claimed for this baseline.

## Operator adoption

The added OAuth scope needs fresh Google consent. Rebuilding/restarting or
`resume` alone cannot provide it. Existing sessions remain saved but unavailable.
Newly saved sessions have a scope field that older binaries reject; retain a
private backup of the stopped auth volume before adopting/writing state if
rollback may be needed. Restore that backup only with the gateway stopped and
the matching older image. Do not manually edit auth state or delete volumes.

After publication, run from `~/home-server-docker/ai-router` on the server.
These commands use private runtime configuration and are operator steps; Codex
did not execute them. Build first while the current image can keep serving:

```sh
docker compose build --pull ai-router
# Finish active flows, stop, and take your private stopped-volume backup.
docker compose stop ai-router
./login.sh agy status
```

Inspect account/protocol restrictions before resume, since it clears them.
Adopt an older saved baseline, then start the fresh consent flow:

```sh
./login.sh agy resume
./login.sh agy login
```

For a remote server, forward `127.0.0.1:1456` from your browser's machine to
the server's `127.0.0.1:1456` over SSH before login, and complete the printed
Google authorization URL. The helper requires the configured gateway stopped
and publishes only the loopback callback. No deployment configuration or extra
environment variable is required.

After successful consent:

```sh
docker compose up -d --no-build --pull never --no-deps --force-recreate --wait ai-router
./login.sh agy status
curl -fsS --retry 10 --retry-delay 2 --retry-all-errors http://127.0.0.1:31337/ready
```

Verify `baseline_update_required: false`, `scope_update_required: false` and
AGY `available: true`, then make representative client/tool round trips.
Readiness alone can be satisfied by OpenAI. The advisory should match this
review unless upstream has moved again. Build/push alone does not deploy.
