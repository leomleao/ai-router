# Capability matrix

Dual-provider deterministic checkpoint: OpenAI configuration, routing,
OAuth/session lifecycle, Responses transport, registry, telemetry, and operator
commands passed local Rust/client/monitor/Docker-policy validation and
cross-review. Clippy and the production image build are still running. No O1/O2
live subscription acceptance has been performed, so account eligibility and
real inference remain unverified.

The installed CLI pin is **1.4.3**. Authenticated Linux acceptance evidence
currently covers **1.2.15**; live 1.4.3 acceptance is an operator gate.
Capabilities are not inferred from the OpenAI endpoint catalogue. The official
CLI is the provider boundary.
`GET /v1/capabilities` reports the active version, catalogue, readiness and
verification status. Fixture/SDK tests prove the adapter, not Google inference.
The official 1.4.3 binary also passes direct MCP handoff tests against a
loopback mock model in isolated synthetic state; this does not verify account
authentication, quota or real model selection behavior.

| Feature | HTTP surface | Current evidence / boundary |
| --- | --- | --- |
| Provider selection | `/v1/models`, Chat, Responses | Exact discovered model IDs route to their registered provider; no model substitution or billing fallback. AGY and OpenAI readiness/catalogues are independent. Deterministic integration checks passed locally; live acceptance pending |
| OpenAI opt-in and OAuth | Operator `ai-router openai` commands | `AI_ROUTER_OPENAI_ENABLED` defaults false; dedicated `ai-router-openai-auth` state. Synthetic lifecycle checks and Docker policy passed; eligibility and live inference remain unverified (O1/O2 pending) |
| OpenAI Responses tools | Chat and Responses client-tool loops | OpenAI Responses can return multiple function calls and supported `additional_tools` developer items; gateway does not execute client tools. Deterministic integration validation passed locally; live acceptance pending |
| OpenAI reasoning replay | Responses local item store; Chat replay cache | Encrypted reasoning items retain provider state for replay. OpenAI Chat cache is owner-scoped, memory-only, 24-hour TTL, 16 MiB/client, 64 MiB/global, 4,096 entries; restart clears it. Deterministic bounds/ownership/replay checks passed locally |
| Provider admission | Gateway request routes | Existing `AI_ROUTER_MAX_CONCURRENT`/`AI_ROUTER_MAX_QUEUE` remain AGY budgets; OpenAI has separate defaults 2/8. Total active and queued ceiling is the sum. Body/rate limits stay global |
| Model catalogue | `/v1/models` | Authenticated Linux AGY 1.2.15 exposes 14 account model IDs; discovered catalogue is not proof every model supports every feature |
| Quota/reset windows | Capabilities/private monitor | Authenticated Linux `/quota` metadata verified; cached freshness and reset windows reported |
| Text generation | Chat and Responses | Real SDK checks passed on gemini-3.8-flash-low; claude-sonnet-4-6 Chat text passed after restart |
| Incremental output | Chat/Responses SSE | Real SDK content and terminal events passed on Gemini; actual CLI may emit short answers only on completion |
| JSON Schema | Chat `response_format`, Responses `text.format`, native `json_schema` | Model profile requests raw JSON and validates the complete final value independently; invalid JSON/schema fails the request and provisional text is withheld. No provider constrained decoding is promised; native schema support remains unverified |
| Client tools | Chat `tool_calls`, Responses `function_call` | Both real Gemini handoff/result/final-answer loops passed through the inert MCP relay and original schema validation; one call per turn |
| Client-tool argument correction | Same-run MCP validation feedback | Known tools with non-object arguments or schema mismatches receive up to two `isError` feedback results; the third rejection fails. Request-wide budget survives relay restarts. No extra CLI run, tool execution, unknown-tool correction or deadline extension. Fixture/SDK-tested; live model correction remains an operator gate |
| Multiple tool rounds | Stateless replay of complete history | Live result continuation and automated multi-round contracts; preserve call/result IDs and roles |
| Response item references | Responses `item_reference`, `store` | Completed message/function-call outputs saved as exact JSON in client-scoped process memory by default; `store:false` disables new storage. 24-hour TTL, earlier oldest-first eviction at 1,024 items / 16 MiB per client or 4,096 items / 64 MiB globally; restart clears items. Expanded history is validated and body-limited. Pinned n8n SDK multi-turn contracts cover default storage and opt-out; no response retrieval or `previous_response_id` |
| Parallel client tools | Request permission accepted | Produces one call per turn; never claims parallel execution |
| Forced/required tool choice | None | Rejected; no proven CLI enforcement |
| Effort/model selection | OpenAI reasoning effort/native effort | Official flags passed through; acceptance depends on the selected model |
| Answer verbosity | Responses `text.verbosity`, Chat `verbosity` | Absent, null and `medium` are equivalent: the neutral OpenAI/n8n default is accepted without changing the prompt, model or reasoning effort. `low`/`high` and malformed values are rejected; no AGY answer-detail control is promised |
| Prompt cache hints | Both APIs: `prompt_cache_key`, `prompt_cache_retention` | String keys and `in_memory`/`24h` are accepted and ignored for n8n compatibility. No provider cache control, key-based cache isolation or retention guarantee. Header `X-AI-Router-Prompt-Cache-Controls: ignored` and capabilities disclose this; observed cached-token usage remains unchanged |
| Native execution modes | Native `mode` | `plan` / `accept-edits` / `auto` select the official CLI flag; profile gated by sandbox verification |
| Built-in tools/subagents | Native run/event API | Tool/agent steps retained; only built-ins allowed by fixed policy and sandbox, profile off until verified |
| Input files | Native uploads | Explicit base64 uploads into `input/`; modality interpretation depends on CLI/model/tool support |
| Artifacts | Native owner-scoped list/download | Safe bounded byte transport; no cross-owner access, protected paths, symlinks or hardlinks |
| Images | Potential native artifacts | Generic files supported; OpenAI images API unavailable until a real generation/edit path is verified |
| Audio | None advertised | Voice UI/microphone facilities do not establish a headless audio API; no fabricated speech/transcription endpoint |
| Persistent conversation resume | None | Raw AGY IDs and global `--continue` would violate ownership isolation; request history replay supported |
| Custom agents/skills/MCP installation | Operator configuration only | Caller definitions/servers/slash expansion rejected; no remote configuration/auth endpoint |
| Output-token budgets | Responses `max_output_tokens`, Chat `max_tokens` / `max_completion_tokens` | Positive integers become best-effort visible-answer length instructions; no hard output, reasoning-token or usage cap. Complete JSON/tool arguments and actual usage are preserved. Capability metadata and a response header disclose prompt guidance |
| Sampling controls | None | Non-null unsupported controls rejected instead of silently ignored |
| Embeddings/Realtime/batches/fine-tuning | None | No demonstrated CLI backend; unsupported endpoint errors |

Native limitations: its API is implemented and fixture-tested, but public
native execution remains disabled. Native stays disabled whenever OpenAI is
enabled until containment verifies AGY and native processes cannot read the
OpenAI volume. With OpenAI disabled, the existing native gate applies. The
current container restrictions may prevent OS sandbox creation. Enabling native requires real canary tests,
including auth-home denial, other-request workspace/temp denial, command
containment, and cancellation of language servers/subagents that may create
their own process groups. Failure keeps native disabled.

The full n8n Assistant needs its own sandbox; this gateway replaces its model
endpoint only. SDK-shaped compatibility probes are separate from exercising
the complete n8n/Hermes apps with their installed versions.

Sources: [AGY headless](https://www.antigravity.google/docs/cli/headless/),
[OpenAI answer verbosity and reasoning effort](https://help.openai.com/en/articles/5072518-controlling-the-length-of-openai-model-responses),
[n8n OpenAI node defaults](https://github.com/n8n-io/n8n/blob/master/packages/%40n8n/nodes-langchain/nodes/llms/LMChatOpenAi/LmChatOpenAi.node.ts),
[n8n Assistant connection probe](https://github.com/n8n-io/n8n/blob/master/packages/cli/src/modules/instance-ai/instance-ai-verification.service.ts),
[AGY permissions](https://www.antigravity.google/docs/permissions?tab=cli),
[CLI custom agents](https://www.antigravity.google/docs/subagents/),
[n8n Assistant](https://docs.n8n.io/deploy/host-n8n/configure-n8n/set-up-n8n-assistant),
[Hermes AGY reference adapter](https://hermes-agent.nousresearch.com/docs/plugins/antigravity-agy).
