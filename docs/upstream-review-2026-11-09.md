# Upstream assessment and live observations — 2026-11-09

Status: implemented and locally validated, starting from
`41f0762`. The updated compiled baseline is Hub 2.29.0 / OMP 21f1188c / adapter
`omp-21f1188c-antigravity-v5`. No server deployment, saved-auth mutation or live
inference was performed. The live observations below describe the existing v4
deployment, not acceptance of this candidate.

## Upstream comparison

The starting compiled baseline was Hub **2.28.2**, OMP
[`686b5e2f`](https://github.com/can1357/oh-my-pi/tree/686b5e2fb542848dc3ff1a9e36a42a8fefdd2278),
adapter `omp-686b5e2f-antigravity-v4`. Public checks on November 9 observed
Hub **2.29.0** and OMP
[`21f1188c`](https://github.com/can1357/oh-my-pi/tree/21f1188cf88b2f534c092181968cd5a4d1a9c296),
package version **18.18.3**.

All 68 source paths recorded in the current baseline were fetched at the new
revision and compared with their recorded reviewed hashes: **57 unchanged,
11 changed, no missing paths**. This is a scoped source comparison, not a
complete audit of every upstream dependency or proof of account acceptance.

The unchanged paths include Google OAuth registration/scopes, OAuth provider,
native header constructor, account discovery, main Gemini CLI/AGY provider,
and the image generation adapter. No additional consent requirement was
identified in these paths. The header constructor still discovers the Hub
version from its manifest; its unchanged source does not imply an unchanged
runtime identity. The router deliberately adopts reviewed identities through
tested builds rather than changing them automatically.

Changed paths:

- `packages/ai/src/providers/google-shared.ts`
- `packages/ai/src/providers/google-types.ts`
- `packages/ai/src/providers/transform-messages.ts`
- `packages/ai/src/auth-retry.ts`
- `packages/ai/src/auth/types.ts`
- `packages/catalog/src/compat/rules/classes/anthropic.kdl`
- `packages/catalog/src/compat/axes.ts`
- `packages/catalog/src/types.ts`
- `packages/ai/package.json`
- `packages/catalog/package.json`
- `packages/utils/package.json`

### Implemented compatibility fix

OMP now explicitly maps Gemini `MALFORMED_RESPONSE` to a generation error in
[google-shared.ts](https://github.com/can1357/oh-my-pi/blob/21f1188cf88b2f534c092181968cd5a4d1a9c296/packages/ai/src/providers/google-shared.ts).
Previously the router mapped this finish reason to `invalid_provider_terminal`,
which could persist a provider protocol restriction. Recognized unsuccessful
finishes now return non-retryable `provider_generation_failed`, with allowlisted
finish reason and value-free counts. Classification precedes interpretation of
the failed candidate's unusable parts. No executable calls or private replay
are published; the failed decoder stays poisoned for that request. Unknown
terminals and malformed envelopes retain their protocol protection. A synthetic
HTTP regression follows each recognized failure with a successful request and
checks that an unknown terminal still latches the protection. It also checks
exact dispatch counts to exclude automatic retries.

Other reviewed changes concern foreign-reasoning classification, retry error
precedence across rotated accounts, direct Anthropic message threads,
additional compatibility axes/types and package/tooling versions. They do not
justify adding account rotation, retries or server-held conversations to this
router. Preserve and document the router's intentional differences.

The update advances baseline constants, Hub identity, the paired source hashes
and attribution together. Shared retry/account-rotation and direct Anthropic
features remain outside our contract. No runtime dependency versions changed.
The existing isolated regression matrices validate the candidate; see the
validation section below and [the alignment policy](omp-alignment.md).

### OpenAI boundary

Current official [subscription limitations](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations)
retain the Responses HTTP `store:false`/`stream:true`/full-input-history
contract. Images/files are model dependent and web search is also subject to
account policy. Hosted image generation, file search, Code Interpreter and
other listed hosted tools remain unsupported on this route. Recent additions
in the [API changelog](https://developers.openai.com/api/docs/changelog) do not
by themselves establish subscription eligibility or require enabling a new
router feature.

## Supplied live read-only evidence

The user supplied the following results from a separate read-only inspection
of terminal `term_9016e071-f236-4c6c-8a45-805d69b625c1`. This review incorporates
that evidence without re-reading the terminal, saved OAuth state or private
configuration. That inspection reported no source/deployment/auth mutations
or inference calls.

For **2026-11-08 00:00:00Z through 2026-11-09 00:00:00Z**, the monitor's
48-hour JSON output, filtered to the day, and bounded Docker logs agree:

- 16 requests: 14 completed successfully, two cancelled.
- Zero `provider_request_failed` events and no AGY requests that day.
- Both cancellations were streaming Chat Completions requests from
  `Buddy/1 CFNetwork/3896.100.1.1.1 Darwin/27.0.0`.
- Both requested `gpt-6.1-luna` and were served as `gpt-6-luna` through the
  disclosed configured fallback.

| Request | Started UTC | Ended UTC | Reported elapsed | Upstream startup |
| --- | --- | --- | --- | --- |
| `4dd9fdd8-aa49-4d8a-888e-bc034438581a` | 2026-11-08T12:45:48.693618790Z | 12:45:51.263Z | 2576 ms | 1534 ms |
| `0cda469b-6f8c-4d53-acfb-4fcb44040b77` | 2026-11-08T16:18:33.923420648Z | 16:18:36.375Z | 2455 ms | null |

Both records have `code=client_disconnected`, `status=cancelled`,
`first_output_ms=null`, `stream_committed=false` and
`upstream_request_id=null`. Successful requests that day took 1800–4006 ms.
Europe/London was UTC on this date.

### Interpretation and diagnostic follow-up

These records suggest investigating Buddy cancellation/deadline behavior and
proxy disconnects while requests await initial SSE output. They do **not**
prove a client timeout, identify which connection ended, or rule out every
router issue. `CompletionGuard` defaults unfinished requests to
`client_disconnected` and logs an early ending when dropped; the label alone
is not a transport-level diagnosis. Null timing/ID fields describe missing
recorded observations, not proof that no upstream work occurred.

Correlate any recurrence with client and proxy timing, and distinguish observed
peer closure, explicit cancellation, shutdown and router deadline paths where
the runtime exposes that evidence. Keep unknown causes explicit. Additional
diagnostics must remain metadata only in persisted logs; private captures keep
their existing bounded, memory-only policy.

Do not classify these cancellations as OpenAI quota, authentication or provider
failures, and do not add automatic paid generation retries. No AGY requests
occurred on the day, so the proposed AGY terminal-handling fix is a separate
upstream compatibility concern, not an explanation of these two records.

## Supplied current live status

At **2026-11-09T10:12Z**, both providers reported `authenticated=true` and
`error=null`; telemetry reported `persistence_errors=0`. Two auth rejection
counters exist since process startup at **2026-11-01T22:42:31Z**, without
timestamps. They cannot be attributed to November 8.

The live baseline remains v4 / Hub 2.28.2. Its advisory observes Hub 2.29.0 and
the same OMP `21f1188c` revision checked above, with
`affects_readiness=false`. This supports the maintenance review and does not
establish an outage or require an immediate restart/auth reset.

## Cancellation diagnostics implemented

The legacy `client_disconnected` telemetry code remains readable, while new
`model_request_ended_early` events record `termination_evidence`:
`handler_future_dropped`, `response_body_dropped`, `shutdown_token_cancelled`,
`request_token_cancelled` or `request_deadline_elapsed`. Tokens are inspected
before cleanup cancels the request token. The event explicitly records
`disconnect_confirmed:false`, whether the stream committed, startup/first-output
timings and whether the provider task finished. These observations distinguish
request phases without claiming which remote peer closed a connection or why.
The same metadata is captured in bounded private response details. Historical
monitor explanations now acknowledge uncertainty instead of asserting that the
client disconnected. No deadline, stream-commit policy or retry behavior changed.

## Operator adoption

After publication, build the exact new commit while the existing gateway can
continue serving. Finish active flows, stop the gateway, inspect `./login.sh agy status`, then run `./login.sh agy resume` with the new image to adopt v5. Resume
also clears saved restrictions, so inspect their cause first. Start using the
built image and verify AGY status separately from overall readiness. Existing
fully consented v4 sessions keep the same registration, six scopes and state
format: this update does not require fresh login. Older sessions missing the
November 1 scope still need that consent step. No volume deletion or server
Compose edits are needed. All deployment/auth operations remain operator steps.

## Validation

The isolated, externally network-disabled environment passed **289 Rust tests**,
including the existing official Python, n8n Vercel/LangChain, Studio, media and
hosted-search contracts over real loopback TCP. New regressions cover failed
then successful generation, malformed failed-candidate parts, withheld calls,
no automatic retries, unknown-terminal protection, v4 baseline adoption, and
early-ending metadata before/after SSE commit and during shutdown.

The binary built without compiler warnings. Monitor PTY, five login-helper
tests, Docker policy/protected-context checks, formatting and whitespace checks
passed. The 68 pairs of public source hashes were independently verified (136
individual hashes). No runtime dependencies changed. No server files, running
provider services or saved OAuth state were modified. Live inference, real
token-expiry acceptance and matched CLI/direct latency are not claimed. The
supplied live observations are explicitly separate from these synthetic checks.
