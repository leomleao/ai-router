# Capability matrix

The installed/tested CLI pin is **1.2.15**. Capabilities are not inferred from
the OpenAI endpoint catalogue. The official CLI is the provider boundary.
`GET /v1/capabilities` reports the active version, catalogue, readiness and
verification status. Fixture/SDK tests prove the adapter, not Google inference.

| Feature | HTTP surface | Current evidence / boundary |
| --- | --- | --- |
| Model catalogue | `/v1/models` | Authenticated Linux AGY 1.2.15 exposes 14 account model IDs; discovered catalogue is not proof every model supports every feature |
| Quota/reset windows | Capabilities/private monitor | Authenticated Linux `/quota` metadata verified; cached freshness and reset windows reported |
| Text generation | Chat and Responses | Real SDK checks passed on gemini-3.8-flash-low; claude-sonnet-4-6 Chat text passed after restart |
| Incremental output | Chat/Responses SSE | Real SDK content and terminal events passed on Gemini; actual CLI may emit short answers only on completion |
| JSON Schema | Chat `response_format`, Responses `text.format`, native `json_schema` | Model profile requests raw JSON and validates the complete final value independently; invalid JSON/schema fails the request and provisional text is withheld. No provider constrained decoding is promised; native schema support remains unverified |
| Client tools | Chat `tool_calls`, Responses `function_call` | Both real Gemini handoff/result/final-answer loops passed through the inert MCP relay and original schema validation; one call per turn |
| Multiple tool rounds | Stateless replay of complete history | Live result continuation and automated multi-round contracts; preserve call/result IDs and roles |
| Parallel client tools | Request permission accepted | Produces one call per turn; never claims parallel execution |
| Forced/required tool choice | None | Rejected; no proven CLI enforcement |
| Effort/model selection | OpenAI reasoning effort/native effort | Official flags passed through; acceptance depends on the selected model |
| Native execution modes | Native `mode` | `plan` / `accept-edits` select the official CLI flag; profile gated by sandbox verification |
| Built-in tools/subagents | Native run/event API | Tool/agent steps retained; only built-ins allowed by fixed policy and sandbox, profile off until verified |
| Input files | Native uploads | Explicit base64 uploads into `input/`; modality interpretation depends on CLI/model/tool support |
| Artifacts | Native owner-scoped list/download | Safe bounded byte transport; no cross-owner access, protected paths, symlinks or hardlinks |
| Images | Potential native artifacts | Generic files supported; OpenAI images API unavailable until a real generation/edit path is verified |
| Audio | None advertised | Voice UI/microphone facilities do not establish a headless audio API; no fabricated speech/transcription endpoint |
| Persistent conversation resume | None | Raw AGY IDs and global `--continue` would violate ownership isolation; request history replay supported |
| Custom agents/skills/MCP installation | Operator configuration only | Caller definitions/servers/slash expansion rejected; no remote configuration/auth endpoint |
| Sampling/token limits | None | Non-null unsupported controls rejected instead of silently ignored |
| Embeddings/Realtime/batches/fine-tuning | None | No demonstrated CLI backend; unsupported endpoint errors |

Native limitations: its API is implemented and fixture-tested, but public
native execution remains disabled. The current container restrictions may
prevent OS sandbox creation. Enabling native requires real canary tests,
including auth-home denial, other-request workspace/temp denial, command
containment, and cancellation of language servers/subagents that may create
their own process groups. Failure keeps native disabled.

The full n8n Assistant needs its own sandbox; this gateway replaces its model
endpoint only. SDK-shaped compatibility probes are separate from exercising
the complete n8n/Hermes apps with their installed versions.

Sources: [AGY headless](https://www.antigravity.google/docs/cli/headless/),
[AGY permissions](https://www.antigravity.google/docs/permissions?tab=cli),
[CLI custom agents](https://www.antigravity.google/docs/subagents/),
[n8n Assistant](https://docs.n8n.io/deploy/host-n8n/configure-n8n/set-up-n8n-assistant),
[Hermes AGY reference adapter](https://hermes-agent.nousresearch.com/docs/plugins/antigravity-agy).
