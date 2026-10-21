# Upstream review — 2026-10-21

Reviewed the public OMP revision [`d5cd750e`](https://github.com/can1357/oh-my-pi/commit/d5cd750e41fede277f9eb933455da92a0738915e)
(version 18.12.5, committed 2026-10-21) against the prior
[`2220e581`](https://github.com/can1357/oh-my-pi/commit/2220e58146ebe31508e7a216242840d373e082d0) baseline.
Only explicitly named public source files and public release metadata were read.
No OMP/CLI saved configuration, OAuth tokens, protected environment files,
private keys, or live-server state were read for this review.

## Changes incorporated

- Pin the reviewed source to `d5cd750e41fede277f9eb933455da92a0738915e` and adapter contract to
  `omp-d5cd750e-antigravity-v2`.
- Pin the Hub version to **2.24.0**, as reported by the
  [official Hub manifest](https://antigravity-hub-auto-updater-974169037036.us-central1.run.app/manifest/latest-arm64-mac.yml).
  The user agent follows the unchanged
  [reviewed OMP constructor](https://github.com/can1357/oh-my-pi/blob/d5cd750e41fede277f9eb933455da92a0738915e/packages/catalog/src/wire/gemini-headers.ts):
  `antigravity/hub/2.24.0 (aidev_client; os_type=darwin; arch=arm64; cl=963137146)`.
  OMP dynamically obtains the version; the router pins it in reviewed source.
  The platform and changelist remain constructor constants captured by OMP from
  an earlier native client. This is not a new native 2.24.0 traffic capture.
  The manifest artifact locator `5780333019398144` is not treated as a changelist.
- Omit `requestType: "agent"` from the wire envelope. OMP's
  [original fix](https://github.com/can1357/oh-my-pi/commit/f1fb41a9687191114be4a407b16c9fdcfb761027),
  integrated through the
  [2026-10-20 merge](https://github.com/can1357/oh-my-pi/commit/6d79b1b8ac2c91e96cfe80e7151b81a1c06b062a),
  reports false quota errors triggered by instruction wording. Matching the
  native client is OMP's reported evidence; it was not independently captured
  here. The comparison introduces no replacement envelope field.
- Keep registration, scopes, the existing project flow, the single inference
  endpoint and the 13 reviewed exact model IDs. Decoded public registration
  fingerprints are unchanged; no replacement values are published by this change.
- Retain the router's original-schema validation, mixed-union projection fix,
  private replay corrections, absolute deadlines and no-generation-retry policy.

## Reviewed source matrix

Raw-byte SHA-256 hashes for both revisions are recorded under
`source.file_sha256` in [the compiled baseline](antigravity-baseline.json).

| Public source path | Result |
|---|---|
| `packages/ai/src/providers/google-gemini-cli.ts` | Remove requestType from the Antigravity envelope |
| `packages/ai/src/providers/google-shared.ts` | Unchanged |
| `packages/ai/src/registry/oauth/google-antigravity.ts` | Unchanged |
| `packages/catalog/src/wire/gemini-headers.ts` | Unchanged |
| `packages/catalog/src/discovery/antigravity.ts` | Unchanged |
| `packages/catalog/src/compat/rules/auth/google-antigravity.kdl` | Unchanged |
| `packages/catalog/src/compat/rules/providers/google-antigravity.kdl` | Unchanged |
| `packages/catalog/src/compat/rules/classes/gemini.kdl` | Unchanged |
| `packages/catalog/src/compat/rules/taxonomy/_collapse.kdl` | Unchanged |
| `packages/catalog/src/compat/rules/classes/anthropic.kdl` | Compaction scoped to other Anthropic-message routes; no CCA change |
| `packages/ai/src/utils/schema/normalize.ts` | Unchanged |
| `packages/ai/src/utils/schema/fields.ts` | Unchanged |

The shared message-transform source was also compared and is unchanged.
The existing MIT notice remains in [THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md).
Public release advisories remain independent of readiness and never change a
compiled identity, session, registration or protocol automatically.

## Explicit adoption without fresh Google consent

A new binary does not silently rewrite saved state. Existing old-baseline state
returns `agy_baseline_update_required` before token use or dispatch. Inspect
`saved_baseline`, `baseline_update_required`, `protocol_blocked` and
`account_restriction` with the gateway stopped. Only resume after reviewing any
existing restriction; resume clears restriction state for the selected generation.
The matching Google session is retained and its generation rotates under the
exclusive dispatch barrier. Invalid compiled metadata or registration
configuration cannot be adopted. Session/client-ID mismatches still prevent
traffic and require the normal explicit fresh-login recovery.

For this repository's local Compose deployment, after building the new image:

```sh
docker stop ai-router
./scripts/login.sh agy status
./scripts/login.sh agy resume
docker compose --env-file /dev/null up -d
curl --fail http://127.0.0.1:8090/ready
```

Use the actual image/container/volume overrides for custom local deployments.
For the home-server repository, use `./login.sh` from its `ai-router` directory
and follow its [operator setup guide](https://github.com/leomleao/home-server-docker/blob/main/ai-router/setup.md).
That deployment loads private configuration and is operated by the user; no
server deployment or restart is performed as part of this source review.
There are no new required environment variables and no auth-volume migration.

## OpenAI quota visibility

OMP's Codex usage adapter and its base-URL helper are unchanged, and use a
separate ChatGPT backend route. They are not imported into this router's public
Sign in with ChatGPT integration. The
[official OpenAI inference contract](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference)
uses public model discovery and Responses. The reviewed public documentation
does not establish a numeric remaining-plan quota endpoint for this integration.
The monitor therefore says remaining allowance is unavailable through this route
and retains the Usage settings link. No private backend API or additional
credential/scopes are added.

The [official recovery guidance](https://developers.openai.com/siwc/token-sharing-open-source/errors-and-recovery)
says a usage-limit error may apply to the app or the plan. The public error and
monitor now describe that distinction, preserve the limit code and stop new
requests until explicit recovery. They do not infer whole-plan exhaustion or a
reset time.

## Validation

Two independent review rounds checked authentication/adoption and wire/schema
behavior, including the recorded upstream file hashes. Validation passed:

- 201 Rust tests, including independent provider mocks and real-TCP Python,
  Vercel and LangChain SDK checks; monitor PTY checks; five login-helper checks;
  formatting and whitespace checks.
- Local production image `ai-router:hub224-local` on loopback port 8092:
  old-baseline traffic was blocked before explicit adoption; resume retained
  the matching Google sign-in and rotated its generation.
- All 30 live SDK checks across `gemini-3.8-flash-high` and
  `claude-sonnet-5-5-high`: Chat Completions, Responses, both streams, independently
  validated JSON schemas, client tool handoffs/result continuations and startup
  disconnect handling.
- OpenAI discovered seven models after its startup discovery completed;
  one live `gpt-6-luna` Responses inference passed.
- The release advisory matched Hub 2.24.0 and OMP `d5cd750e` with no alerts.
  Temporary test client keys were revoked, original client digests restored and
  the upgraded local gateway left ready. Other containers and the live server
  were not changed.

No matched latency comparison was run for this update, so no speedup is claimed.
These live checks establish acceptance for the tested accounts/models and
requests, not every model or future provider behavior. Deployment still requires
publication and the separate operator upgrade process above.
