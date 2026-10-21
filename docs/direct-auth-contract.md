# Direct Antigravity authentication contract

The runtime owns a new private Antigravity session. It never reads the official
CLI's or OMP's saved authentication. No supported, documented renewable-token
handoff suitable for the independent Cloud Code Assist client was identified in
the reviewed public interfaces. The legacy volume remains a rollback asset;
reuse is not implemented by extracting or copying its contents.

The operator explicitly selected fresh consent using the reviewed OMP installed
application registration. The baseline is upstream revision
`d5cd750e41fede277f9eb933455da92a0738915e`,
`packages/catalog/src/compat/rules/auth/google-antigravity.kdl`. The registration
is bundled in that public source; it is not evidence of provider authorization
for a separate application. An operator-owned reviewed installed-application
registration can be provided through process variables
`AI_ROUTER_GOOGLE_CLIENT_ID` and `AI_ROUTER_GOOGLE_CLIENT_SECRET`. These variables
are never populated by reading a configuration file inside the authentication
implementation. Registration changes require explicit sign-in.

The status response exposes registration metadata, never registration values:
`bundled_registration_valid` says whether the decoded bundled public pair
matches its compiled fingerprint; `registration_source` is `reviewed_omp`,
`operator_configured`, or `unconfigured`; and `session_registration_matches`
compares the saved session's client ID with the configured client ID (`null`
when no session is saved). These are local integrity/configuration checks, not
proof of live Google approval. Session matching cannot detect a client-secret
change until Google validates it during refresh. Configuration validity and
session/client-ID compatibility are checked before token use, including use of
an unexpired token, and again during generation validation and dispatch
admission. `available` includes this local check; it does not guarantee that
Google will accept a later request.

Consent uses Google's fixed authorization/token/userinfo origins, a random
256-bit state, S256 PKCE and the reviewed scopes. The bounded local callback
checks its exact path, one state, one code and exact loopback Host authority.
Inside Docker the listener binds all interfaces so the published callback port
can reach it; Docker must publish that port on host loopback only. Remote use
requires an explicit loopback SSH forwarding workflow. The listener and consent
expire after ten minutes; individual control-plane requests are bounded to
fifteen seconds inside the original deadline. Idle browser preconnections are
bounded separately and cannot end the entire flow. A matching `access_denied`
callback ends consent promptly with a sanitized cancellation. Wrong-state,
duplicate and mixed code/error callbacks cannot cancel the flow.

The router resolves an existing Cloud Code Assist project with `loadCodeAssist`.
It never calls onboarding or enrolls a free tier. Missing enrollment/project
returns an actionable error and leaves the saved session unchanged. Userinfo is
queried over Google's fixed TLS origin; no unsigned ID token is trusted.

Dedicated state uses a 0700 directory, 0600 single-link files, no-follow ancestor
walking, cross-process flock, exclusive temporary files, atomic replacement and
file/directory synchronization. Runtime state paths reject the protected file
patterns. Logs/status never contain tokens or subject identifiers. A refresh
runs in an owned bounded task, so caller cancellation cannot interrupt durable
replacement after rotation. The refresh registration must match the session's
registration. Refresh serialization holds the state lock across its bounded
network operation and durable write; it does not change the selected account
generation. Token values and raw state are never part of validation evidence.

Protocol restrictions are durable across restart and fresh sign-in. Account and
auth restrictions apply to the selected generation. Explicit fresh login may
replace an account restriction; protocol review/recovery is separate. A delayed
old-generation error cannot restrict a newly selected session. Local logout
removes only the router session; it does not remotely revoke Google grants or
the official CLI session, or change a Google account. Recovery rotates the generation and never replays the
failed inference request automatically.

Every inference, catalogue and quota dispatch uses the same fixed daily Cloud
Code Assist origin and reviewed user agent. Reviewed Claude variants also send
the fixed interleaved-thinking beta header. OAuth destinations are fixed
separately. The checked-in baseline is compiled into the binary and checked
against the adapter revision, source commit, identity, destinations and paths.
No runtime baseline file, endpoint/account/CLI fallback or network-driven
identity update changes authenticated dispatch.

## Adopting the 2026-10-21 baseline

The reviewed baseline is `omp-d5cd750e-antigravity-v2`, using the explicit Hub
2.24.0 version pin. Its registration, scopes and project flow match the prior
baseline. Existing sessions are preserved, but prior-baseline state returns
`agy_baseline_update_required` before token use or dispatch. Status exposes
`saved_baseline` and `baseline_update_required` alongside the compiled `adapter`.
Stop the gateway and explicitly run `login.sh agy status`, then
`login.sh agy resume` with the new image to adopt this reviewed baseline. Resume
validates compiled metadata and configuration before changing state, retains
the matching session and project, and rotates the generation under the exclusive
dispatch barrier. It does not renew consent or retry a generation. Inspect any
saved account/protocol restriction before recovery; a new image never silently
clears it. See the [review and upgrade steps](upstream-review-2026-10-21.md).

## Upstream advisory and review lifecycle

At startup and hourly, a separate unauthenticated monitor checks the public Hub
version manifest and OMP `main` revision endpoint. It uses fixed URLs, no proxy,
no redirects, no provider credentials, bounded five-second requests, and a
three-hour freshness threshold. The advisory is exposed through capabilities
and the private monitor. Changed or stale upstream metadata does not change the
compiled baseline, registration, account state, dispatch readiness, or OpenAI
availability.

A human reviews an observed change against the pinned upstream diff, especially
`packages/catalog/src/compat/rules/auth/google-antigravity.kdl`. The bundled
Base64 OAuth registration is public installed-app registration data, not
credential secrecy. Personal access tokens and OAuth access or refresh tokens
must never be encoded or published. A needed change is made explicitly in
reviewed source/baseline code, checked offline and in local Docker, and shipped
in an explicitly built release. Only version/revision metadata is fetched; no upstream source, identity,
credential, or image is installed automatically. A new upstream version does
not stop working requests; an advisory alone cannot close the Antigravity gate.
Classified errors from actual provider operations can set durable restrictions.
A changed OAuth registration requires a deliberate fresh
sign-in because a saved session is bound to its registration. OpenAI remains
independent throughout.

The catalogue contains only 13 reviewed exact model IDs, and the saved account
roster determines which of those IDs are available. A newly advertised upstream
SKU remains unavailable until a reviewed baseline/code change is tested and
shipped in a rebuilt image. Normal OAuth `refresh_token` calls renew an existing
session; they do not change the model catalogue, registration baseline, or
credentials.

Cross-process admission uses a private `.dispatch-lock`. A request obtains a
shared file lock and local read guard, then checks the selected generation and
durable restriction state under the state lock. It retains its admission permit
through the HTTP response-header phase. Refresh and explicit login control-plane
requests also hold a shared admission while their bounded HTTP operation runs.

Closing a generation first persists its restriction, or removes the session for
logout. That durable write is the closing intent and rejects new admissions.
The closer then releases the state lock, obtains the local write guard and
acquires the dispatch file lock exclusively. Successful acknowledgement means
prior header-phase admissions have drained. Requests admitted before the intent
may already have reached the network; their dispatch cannot be recalled. If the
20-second closure wait times out, the durable restriction remains and the
operation reports failure instead of claiming that the drain completed. Resume
and login commits acquire the exclusive barrier before changing the generation.
Operator authentication helpers additionally require the gateway to be stopped
for credential mutations.

Local cancellation belongs to an observed account generation. Observing a new
durable generation cancels the previous epoch and creates a fresh cancellation
token. State readers retain the state lock through epoch observation, preventing
a delayed old snapshot from cancelling a newly selected session.

Header and body watchers attempt a durable-state check every 100 milliseconds;
continuous stream chunks cannot starve those checks. A rotating refresh may
hold the state lock longer than that interval. Watchers distinguish this `Busy`
condition from a validated generation and defer the check without manufacturing
a deadline failure. Local cancellation and the original absolute request
deadline remain active. Initial admission always obtains the state lock and
validates; it never skips validation because the lock is busy. Cross-process
cancellation therefore follows the next successful check, rather than carrying
a strict 100-millisecond bound during state-lock contention.

HTTP 401 and 403 restrict authentication/account traffic respectively; 405,
415 and unexpected redirects restrict the protocol. HTTP 400/422 reject the
individual request, 404 reports model unavailability, and ordinary 429 quota
responses do not persist an account/protocol restriction. Error bodies are not
exposed. Explicit authentication status distinguishes saved-session presence
(`authenticated`) from the local gate predicate (`available`); model readiness
also depends on provider discovery, admission and any family cooldown.

Synthetic tests cover callback validation, private storage, generation changes,
refresh single-flight and rotation, cancellation, durable restrictions,
cross-process admission/draining, and slow-refresh contention during streaming.
The operator also completed separate local OAuth sign-in, project/model
discovery and live Flash/Claude inference. Those observed operations establish
that specific login and request path. Real token expiration, rotating refresh and comprehensive account-lifecycle
recovery have not been verified against Google. Remote grant revocation is not
implemented by local logout. Refresh races remain synthetic evidence; performance
and wider model acceptance are reported separately from this auth contract.
