# Upstream review — 2026-10-24

Reviewed OMP [`0b5c27ab`](https://github.com/can1357/oh-my-pi/tree/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0)
against [`d5cd750e`](https://github.com/can1357/oh-my-pi/tree/d5cd750e41fede277f9eb933455da92a0738915e),
and the official Hub manifest reporting **2.26.0**. The router now records
`omp-0b5c27ab-antigravity-v3` as its reviewed contract.

The review used public source and release metadata only. It did not inspect
saved OAuth state, protected environment files, private keys, or server secrets.
The running server and the existing authenticated local containers were not
modified. See [the alignment policy](omp-alignment.md) for the maintenance goal.

## Changes incorporated

- Advance the reviewed OMP source and Hub version together, preserving the exact
  public source revision in the compiled baseline.
- Send `antigravity/hub/2.26.0 (aidev_client; os_type=darwin; arch=arm64; cl=963137146)`.
  OMP's unchanged [header constructor](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/catalog/src/wire/gemini-headers.ts)
  resolves the version from the same public manifest. The router adopts the
  observed value in a reviewed build. Platform and changelist are still OMP's
  constructor constants, not a new native-client traffic capture.
- Renew Google access tokens inside the same **five-minute window** as OMP's
  [Antigravity auth rule](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/catalog/src/compat/rules/auth/google-antigravity.kdl),
  which specifies `expires` with `skew-ms=300000`. OMP subtracts that skew when
  mapping credential expiry; this router retains actual expiry and applies the
  equivalent margin before token dispatch. Our previous margin was 90 seconds.
  This fixes an existing difference discovered by the expanded review, rather
  than a newly changed upstream auth rule.
- Record and validate that refresh margin in the compiled baseline. Keep the
  existing serialized refresh, rotation persistence, account-generation checks
  and original absolute deadline.
- Retain the unchanged public OAuth registration, five scopes, Google endpoints,
  project requirement, CCA inference origin, request envelope and reviewed model
  inventory. Existing matching sign-in can be retained through explicit adoption.

## Review scope and evidence

The 12 files in the preceding protocol review are byte-identical at this OMP
revision. The expanded review covered **59 public files**: those core files,
OAuth exchange/refresh engines, callback and PKCE helpers, Google provisioning,
message transforms, schema support, retry/transport helpers, relevant gateway
changes, compatibility code and package manifests. SHA-256 hashes for both
revisions are recorded per path under `source.file_sha256` in
[the baseline](antigravity-baseline.json).

| Area | Result |
| --- | --- |
| Google auth declaration, registration, scopes and token endpoint | Unchanged; decoded public registration matches router fingerprints |
| OAuth code/refresh engines and credential mapping | Unchanged; form grants and five-minute expiry skew reviewed |
| Google project resolution and OAuth request helper | Unchanged |
| CCA provider, shared conversion, headers and discovery | Unchanged |
| Message transforms, schema modules and HTTP retry helpers | Unchanged |
| General auth-retry helper | Initial credential-resolution failures now preserve a configuration error instead of becoming a missing-key error; router already distinguishes auth absence from transport failure |
| OMP auth gateway | Clamps unsupported reasoning tiers to model capabilities; router retains its separately tested explicit model/effort contract rather than silently changing requested effort |
| Compatibility axes and Anthropic helper | Persistent-cache metadata and first-party Anthropic API-version selection; no change to this CCA route |
| AI, catalog and utils package manifests | Version-only changes; no dependency changes in these manifests |

Changed-path inspection included AI, catalog and utils source between the two
commits. Missing candidate paths were resolved to their real source locations
before inclusion in the recorded matrix. This is a scoped protocol review, not
an audit of every OMP feature or third-party dependency implementation.

## Intentional router differences

Alignment concerns the OAuth and authenticated request method. These existing
router boundaries remain explicit:

- The Hub identity is adopted through reviewed builds. Runtime release checks
  remain advisory and do not change identity, registration or credentials.
- The OAuth callback uses the router's configurable, loopback-published port;
  it need not use OMP's default port. PKCE and state verification remain required.
- OAuth I/O uses the router's existing bounded timeouts and cancellation rules;
  OMP's provisioning helper has its own 30-second request timeout.
- The router requires an already enrolled project; it does not automatically
  perform OMP's free-tier onboarding or change account provisioning.
- The router keeps its own auth store, validates account identity and granted
  scopes, refuses redirects and maintains persistent provider restrictions.
- Model routing, client-tool validation, private replay, visible output and paid
  generation retry behavior retain the router's reviewed contracts. No tool-call
  validation behavior is changed by this baseline update.

## Validation

Local deterministic validation passed with external networking disabled in the
isolated test container:

- 243 Rust tests, including the real-TCP official Python, Vercel, LangChain and
  Studio client suites and their streaming/error checks.
- A still-valid token with four minutes remaining renews before dispatch; a
  token outside OMP's refresh window remains cached. Project and unrotated
  refresh credentials are preserved.
- Both v1 and v2 synthetic saved baselines block before adoption; explicit
  resume preserves the session, rotates the generation and restores dispatch.
- The updated debug binary built. Five login-helper tests, Docker policy and
  protected-context checks passed; formatting and whitespace checks passed.
- Independent review checks matched the decoded public registration, all five
  scopes and all 59 recorded upstream SHA-256 hashes.

The monitor PTY checks also passed: pages/scroll, pause/range,
disconnect/recovery, themes, resize, exit, and text/JSON output. No authenticated
Google login, real token-expiry cycle or live generation is claimed for 2.26.0. Earlier live acceptance evidence belongs to the earlier
baseline and is not carried forward as proof of this update.

## Operator adoption

A changed baseline intentionally requires explicit adoption before using a saved
AGY session. Rebuilding and restarting alone can produce
`agy_baseline_update_required`; it is not a reason to delete the auth volume or
repeat consent when the registration still matches.

After this revision is published, run these commands from the home-server
repository's `ai-router` directory. They are operator steps that load private
runtime configuration; Codex did not execute them on the server.

```sh
# Build first while the existing router can continue serving.
docker compose build --pull ai-router

# Finish active conversations before stopping the gateway.
docker compose stop ai-router
./login.sh agy status
```

Inspect the reported baseline and any account/protocol restriction. If the
existing restriction has been reviewed and adoption is appropriate, retain the
same sign-in and adopt the reviewed baseline:

```sh
./login.sh agy resume
docker compose up -d --no-build --no-deps --pull never --force-recreate --wait ai-router
curl -fsS --retry 10 --retry-delay 2 --retry-all-errors http://127.0.0.1:31337/ready
```

The helper uses the newly built `ai-router:local` image and requires the gateway
stopped for mutation. Resume rotates the account generation and preserves the
matching session; it also clears existing restrictions, so inspect status first.
The explicit `--pull never` uses the image just built. No deployment Compose or
authentication files need editing for this update.

Verify AGY status and representative client calls after startup. Readiness can
also be satisfied by OpenAI alone; it is not sufficient proof of AGY adoption.
The advisory should match Hub 2.26.0 / OMP 0b5c27ab unless upstream has moved again.
