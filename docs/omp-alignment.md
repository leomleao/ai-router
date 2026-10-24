# Keeping AGY aligned with OMP

The router should follow the reviewed Antigravity implementation in
[oh-my-pi (OMP)](https://github.com/can1357/oh-my-pi). This is an ongoing
maintenance requirement: when OMP changes its relevant OAuth or AGY request
behavior, review and update the router too.

The reason is practical compatibility. If sign-in, token renewal, or an
authenticated AGY request stops working, we want to compare our behavior with
OMP's current method. An independently drifting implementation makes it harder
to tell whether a failure comes from our adapter or an upstream change.

## What needs to stay aligned

“The same method” covers both Google OAuth and the AGY API that uses the issued
access token. These are separate stages, and either can change.

| Area | What to compare with OMP |
| --- | --- |
| OAuth sign-in | Authorization endpoint, public client registration, scopes, PKCE, callback handling and consent parameters |
| Token lifecycle | Token endpoint, exchange and refresh request format, grant handling, expiry and error classification |
| Account/project setup | Project resolution, enrollment requirements, discovery and account restrictions |
| Authenticated AGY requests | Destination, bearer authentication, headers, client identity, request envelope and model IDs |
| Tool and conversation handling | Schema conversion, message ordering, function results, private thought/signature continuation and response decoding |
| Supporting code | Relevant imported helpers, compatibility rules and dependencies, not just the main provider file |

The router owns its OAuth session. Alignment means reproducing reviewed public
protocol behavior; it does not require reading or importing OMP's saved tokens.
Router-specific client-tool validation, isolation and diagnostic privacy must
remain intact. Record any intentional difference from OMP and its reason.

## How to interpret the release advisory

The monitor distinguishes the **reviewed** baseline compiled into this image
from the **observed** upstream release or source revision.

- A different OMP revision triggers a source review, even if requests still work.
- A different Hub version triggers review of the release metadata and OMP's
  client-identity method. Changing a version string alone does not establish
  OAuth or protocol compatibility.
- Readiness reports current availability. It does not prove that the router is
  aligned with the latest upstream source or that the newest local changes have
  been published and deployed.

The advisory remains informational at runtime so an upstream release does not
interrupt working requests. Operationally, it is a maintenance task to resolve,
not a reason to leave the adapter on an old implementation indefinitely.
The running router does not download or adopt upstream code automatically.

## Update workflow

1. Resolve the observed OMP revision and Hub release. Compare them with
   [the compiled baseline](antigravity-baseline.json), using public source only.
2. Review the areas above, including changed imported helpers. Start with the
   source paths recorded in the baseline; extend the review when dependencies
   or upstream structure have changed.
3. Port relevant OAuth and protocol changes. If those paths are unchanged,
   record that evidence rather than inventing a behavioral change. Advance the
   reviewed source/version metadata as part of a tested baseline update, with
   an explicit record of what was and was not verified.
4. Update the baseline, matching compiled constants, source hashes, attribution
   and a dated review document together. Do not merely suppress the advisory or
   relabel an unreviewed implementation as current.
5. Run synthetic auth, refresh, request/response and schema tests, plus the
   Chat/Responses SDK matrices used by n8n. Verify streaming and client-owned
   tool-result continuations. Record live checks separately when authorized;
   synthetic tests alone do not prove acceptance by Google or AGY.
6. Commit and push the reviewed change. Verify the remote commit, rebuild the
   deployment image from it, and have the operator deploy it. A local commit or
   a successful push alone does not update the running server.
7. Verify readiness, provider status and representative calls on the deployed
   image. Check the advisory against the newly reviewed baseline.

If a baseline change requires explicit adoption, follow
[the operations procedure](operations.md#adopting-a-newly-reviewed-baseline).
Retain the matching sign-in when supported. A changed OAuth registration or
scope requirement needs its own reviewed consent/recovery procedure; do not
silently rewrite credentials or clear account restrictions to make an update
appear successful.

## When an OAuth or AGY request fails

Identify the failing stage first: authorization, token exchange, token refresh,
project/discovery, generation dispatch, or response/tool validation. Compare
that stage with current OMP before proposing a workaround. Record the deployed
image/baseline, HTTP status, safe error classification and upstream request ID
when available. Keep tokens and private request/response data out of persisted
logs; use the existing bounded private capture where applicable.

A newer upstream revision is useful diagnostic evidence, not proof that it
caused a failure. For example, an otherwise valid tool call with arguments that
violate the client's schema is a different failure from an OAuth rejection.

## Observation on 2026-10-24

The application baseline was Hub **2.24.0** and OMP **d5cd750e**. The advisory
reported Hub **2.26.0** and OMP **0b5c27ab**. A targeted byte comparison found
all 12 public source files listed in the recorded baseline unchanged at that
OMP revision, and the public Hub manifest confirmed 2.26.0.

That comparison did not adopt a new baseline, review every transitive dependency,
or validate a new Hub identity against a live account. It explains why the
advisory can remain after deploying the latest application commit. The next
baseline update should complete the review and validation above, keeping our
OAuth and authenticated AGY methods aligned with OMP.

The subsequent [completed review](upstream-review-2026-10-24.md) expanded the
comparison to 59 source files and adopted Hub 2.26.0 / OMP 0b5c27ab. It also
aligned the router's token-renewal margin with OMP's five-minute window. The
[preceding review](upstream-review-2026-10-21.md) remains historical evidence.
