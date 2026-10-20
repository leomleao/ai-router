# Provider monitoring review — 2026-10-20

Scope: review misleading AGY-specific labels and adjacent provider attribution,
readiness, timing and usage behaviour in the dual-provider router. This round
changes monitoring/status reporting; the direct Antigravity replacement remains
planned work in the [plan](direct-antigravity-plan.md) and
[DAG](direct-antigravity-dag.md).

## Findings and changes

| Finding | Impact | Resolution |
| --- | --- | --- |
| Shared startup percentiles, request details and text output said “AGY ready,” including OpenAI calls. | Users attribute HTTP setup/response latency to AGY process startup. | Use neutral “Startup”/“startup” labels throughout; preserve existing JSON field names. Document the adapter-specific initialization milestones. |
| TUI readiness relied on `authenticated` alone, ignoring expired/invalid catalogue timestamps and explicit errors. | Cached authenticated status can appear ready when the registry refuses it. | Honour explicit errors and the registry's freshness predicate; display stale state and adjust status colour. |
| OpenAI cached probe status could remain healthy after a latched quota/account restriction or account-generation change; capabilities could show no explanatory error. | Monitor/capabilities contradict request admission. | Share effective availability/generation checks between registry/status reporting. Publish local effective status roughly once per second and immediately after probes, without adding network probes. Successful catalogue discovery cannot transiently publish healthy status over a durable block. |
| Disabled OpenAI stayed “not checked.” | Operators cannot distinguish disabled configuration from incomplete discovery. | Publish and render explicit disabled state. |
| TUI provider counts omitted non-AGY/OpenAI records; text counted only literal `other`, omitting `unknown`. | Provider subtotals do not reconcile with retained request counts. | Sum all remaining provider IDs into Other, retaining raw IDs in JSON records. |
| Token totals had no explicit combined-provider scope. | Combined observed totals can be mistaken for one account/provider's usage. | Label the usage panel and text totals as all providers; document that summary latency percentiles also combine providers in the selected range. |

## Reviewed behaviours intentionally preserved

- `agy_usage_partial` is an intentional legacy API alias beside
  `ai_router_usage_partial`. It does not identify the selected provider. Removing
  it would be a separate compatibility change.
- Summary timing/usage aggregates combine retained records. Individual records
  still carry provider and model; this round does not add per-provider percentile
  panels or reinterpret old telemetry.
- Rejection counters cover the current process lifetime; the selected range
  filters retained history separately. Sampling and lifetime labels are correct.
- `startup_ms` measures elapsed time to `RunEvent::Init`: verified CLI init for
  AGY and accepted streaming HTTP headers for OpenAI. Neither measurement is
  pure local startup overhead. `first_output_ms` excludes private reasoning and
  can precede client delivery when JSON schema validation buffers output.
- Status publication and dashboard refresh are sampled; no atomic identity
  between a displayed snapshot and a later admitted request is promised.

## Review and validation

A Luna agent reviewed the existing telemetry and implemented the bounded local
OpenAI status publication; the primary agent reviewed that change and corrected
the probe/publication ordering. The primary agent implemented display/freshness
changes; the Luna agent independently reviewed those changes. A separate agent
reviewed the direct-provider research, plan and DAG; its source/PKCE/tool-choice
corrections were incorporated.

Synthetic regressions cover mixed-provider timing labels, Other counts, stale
and malformed provider timestamps, explicit errors/disabled state, durable
restriction after a successful probe, account-generation mismatch, and matching
capability error reporting. Existing timing, unknown usage, terminal sizing and
legacy telemetry checks remain in scope.

Final test results are recorded in [validation](validation.md). No production
container was rebuilt/restarted in this round, no credentials were inspected,
and no direct-backend inference was performed. The running monitor displays the
new behaviour after the updated binary is built and deployed.
