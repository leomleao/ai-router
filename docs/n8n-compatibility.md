# n8n compatibility regression review — 2026-10-20

The direct adapter is tested with both n8n client families, not just requests
that resemble an agent prompt. The archived comparison is
`archive/agy-cli-2026-10-20` (`4d23e015`). Tests use synthetic credentials, an
actual TCP router, loopback provider fixtures, and no external networking during
execution. They do not launch an installed n8n instance or call signed-in models.

## Client matrix

| n8n use | Exercised contract | Tests |
| --- | --- | --- |
| Credential verification and model selection | Discovery, unaugmented short-budget probes, supported budget aliases and disclosure | `sdk_contract.py`, `n8n_contract.mjs` |
| Workflow AI Agent / workflow tools / subagent results | Actual LangChain ChatOpenAI, bound HTTP Request/Wikipedia/Code-shaped functions, ToolMessage results, structured JSON | `n8n_langchain_contract.mjs` |
| Assistant / agent builder | Actual Vercel OpenAI provider, larger discovery/workspace/CRM-shaped tool inventory, changed inventory after discovery | `n8n_contract.mjs` |
| Multi-turn agent conversations | Text and function replay, stored item references, store omitted/true/false, owner isolation, out-of-order parallel results | Both SDK suites and `src/api_contract_tests.rs` |
| Both public APIs | Chat Completions and Responses, streaming and nonstreaming, terminal events, IDs, usage and JSON output | All three SDK suites |
| Unsupported features | Active sampling/verbosity, forced tool choice, hosted tools, unknown controls, legacy Assistants endpoints | Official SDK negative checks and protocol tests |

Pins are `ai@7.0.66`, `@ai-sdk/openai@4.0.20`,
`@langchain/openai@1.4.4`, `@langchain/core@1.2.8`, and the official Python
`openai@3.24.0` SDK. The n8n versions were checked against its
[public dependency catalog](https://github.com/n8n-io/n8n/blob/630b973bbad5bfc00cdf60b367c0971cba49a2af/pnpm-workspace.yaml)
and [OpenAI Chat Model implementation](https://github.com/n8n-io/n8n/blob/630b973bbad5bfc00cdf60b367c0971cba49a2af/packages/%40n8n/nodes-langchain/nodes/llms/LMChatOpenAi/LmChatOpenAi.node.ts).
The LangChain package is unpatched: n8n's catalog patch adds empty-choice error
handling and DeepSeek reasoning replay; neither changes the successful
Gemini/Claude paths tested here. This is not a claim about the operator's exact
installed n8n version.

## Comparison with the CLI branch

| Archived coverage | Direct adapter coverage / disposition |
| --- | --- |
| 122 Vercel assertions, one fixture profile | All 122 assertions restored, plus 48 tool/inventory assertions; 170 per model, two profiles |
| SDK text, streaming IDs/lifecycle, usage and stream accumulator | Official SDK tests on both model profiles, including stream accumulators and sequence numbers |
| Budget aliases, malformed values, cache hints, neutral defaults | Restored SDK budget matrix and HTTP cache-hint matrix; capability parameter listing restored |
| Stored item references, owner isolation, store disabled, monitor capture vs persistence | Shared HTTP tests ported to direct fixtures; retained item-store unit tests |
| Type-omitted/rich schemas and tool round trips | Original schema locally enforced; CCA projection, strict mock rejection, refs/nullable/nested schema tests and both SDK families |
| Tool aliasing and MCP process/profile escape checks | No MCP or AGY subprocess remains; current inventory validation rejects undeclared tools before public handoff |
| CLI bounded argument correction | Not retained: direct generation fails closed on invalid calls and does not automatically issue additional paid requests |
| CLI subprocess groups, native jobs, files and artifacts | Intentionally removed; direct transport cancellation/deadlines/restrictions replace process tests; removed routes return 404 |
| Admission, shutdown and rejection counters | Existing direct admission, original-deadline, provider isolation, cross-process restriction and telemetry tests retained |
| Monitor PTY and Docker policy | Run separately with synthetic fixtures |

The archived “235 client checks” combined SDK, admission, telemetry, native,
process and CLI-specific checks; it is not an SDK-only count. Passing a larger
assertion count does not establish semantic equivalence for removed features.

## Tool-schema fix

CCA's legacy `parameters` field accepts a limited Schema dialect. Sending the
original `$schema`, `$defs`, `$ref` or `additionalProperties` there can produce an
immediate provider `INVALID_ARGUMENT`. The former mock accepted arbitrary JSON
and failed to catch that mistake.

The serializer now resolves local references and emits bounded CCA-compatible
hints in schema positions only. Client property names such as `$schema` remain
property names. Unsupported constraints, nullable/union semantics and recursive
schemas remain in the full original contract included in the tool description;
the original schema validates every returned argument object locally. Provider
hints are a projection, not full constrained decoding: complex unions can have
narrower hints, recursion has a bounded fallback, and invalid output still fails
closed. No automatic retry or tool execution is added.

This follows the pinned OMP adapter's choice of legacy CCA parameters, without
assuming the separate Gemini `parametersJsonSchema` API is accepted by CCA.
The strict independent mock rejects the original failing dialect and accepts
only the supported schema fields.

## Mixed-type union follow-up — 2026-10-21

A live Flash High request was rejected with HTTP 400 / `INVALID_ARGUMENT` at
`parameters.properties[nodeTypes].items.properties`: properties were present
on a non-object type. Projection merged fields from string and object union
branches but retained object properties after selecting a string hint.

The projection now removes fields incompatible with the final type at every
recursive schema node: `properties`/`required` belong to objects, `items` to
arrays, and the supported string `enum` hint to strings. The original client
schema remains in the tool description and still validates returned arguments;
both original union branches remain valid locally. Provider hints for mixed
types are still narrower than the full client contract.

Two new projection regressions reproduced the bad shape before the fix. Coverage
now also checks the serialized tool request and original argument validation,
rejects type-incompatible fields in the independent CCA mock, and includes a
mixed `nodeTypes` discovery tool in the real Vercel SDK matrix. This local fix
does not update the deployed server; acceptance of the exact original client
request must be rechecked after deploying the rebuilt image.

Local validation passed 194 Rust tests, including the official Python, Vercel
and LangChain SDK matrices, plus monitor PTY and five login-helper tests. Test
execution used synthetic credentials and disabled external networking.

## Diagnostics

`model_request` records provider/model, request ID, endpoint, tool inventory,
input item count and byte counts for input, instructions and schemas.
`provider_request_failed` records correlation IDs, elapsed time, error code,
HTTP status and safe diagnostic metadata. HTTP failures include body-read
outcome, JSON validity, bounded body size, media class, numeric Retry-After,
allowlisted provider status and known schema-keyword mentions. Tool-validation
failures include bounded reasons and schema-validation metadata. Stream errors
record their phase and numeric provider code.

Upstream error JSON or text is limited to 16 KiB and at most 500 ms within the
original deadline. Truncation, invalid JSON, disconnect and timeout are explicit.
Auth/account/protocol restrictions publish before optional body inspection.
The actual dispatch bearer is scrubbed, then existing monitor redaction applies.
Raw provider descriptions, field violations and echoed data stay exclusively in
the existing bounded private in-memory response capture. They are absent from
public errors, ApiError serialization/Debug, and persisted telemetry. Inspect
`detail <request-id>` through the private monitor socket while the detail is
still retained. Restart/eviction clears that capture.

## Results and limits

- 190 Rust tests passed.
- 250 official Python SDK assertions passed (125 per Gemini/Claude profile).
- 340 Vercel assertions passed (170 per profile).
- 76 LangChain assertions passed (38 per profile).
- Monitor PTY checks, five unified-login mock methods and Docker policy checks passed.
- The updated binary built; formatting and diff whitespace checks passed. Clippy
  was unavailable in the local test toolchain and is not claimed as run.

The model fixtures are `gemini-4-argon-medium` and `claude-sonnet-5-5-high`.
These checks establish the shared text/JSON/client-function contracts. They do
not establish live Gemini acceptance, full installed-n8n execution, token-refresh
acceptance, or CLI/direct latency equivalence. Legacy OpenAI Assistants
`/assistants`/`/threads`/runs, hosted tools, media and embeddings remain outside
the router's Chat/Responses contract.
