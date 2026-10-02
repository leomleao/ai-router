# ai-router
A rust based OpenAI compatible router
## Overview

The goal of this project is to expose a **local OpenAI-compatible API endpoint** that internally delegates work to Google's **Antigravity tooling** authenticated with an existing **Google AI Pro subscription**.

The motivation is simple: an AI Pro subscription may include substantial Antigravity usage that otherwise goes unused when day-to-day work is performed through tools such as ChatGPT, Codex, Claude Code, OpenWebUI, Continue, or custom applications.

Instead of requiring every client to understand Antigravity directly, a local gateway translates standard OpenAI-style requests into Antigravity CLI jobs and translates the output back into an OpenAI-compatible response.

The intended architecture is:

```text
OpenAI-compatible client
        |
        | HTTPS / OpenAI API
        v
+-----------------------+
| Local AI Gateway      |
| FastAPI / Node        |
|                       |
| - API authentication  |
| - request validation  |
| - OpenAI translation  |
| - streaming adapter   |
| - logging             |
| - rate limiting       |
+-----------+-----------+
            |
            | local subprocess
            v
+-----------------------+
| Antigravity CLI       |
| authenticated using   |
| the user's Google     |
| account / AI Pro      |
+-----------+-----------+
            |
            v
       Google services
```

## Core Idea

The gateway exposes endpoints that resemble the OpenAI API:

```text
GET  /v1/models
POST /v1/chat/completions
POST /v1/responses
```

A client can therefore be configured with something like:

```text
OPENAI_BASE_URL=https://ai.example.com/v1
OPENAI_API_KEY=sk-local-xxxxxxxx
```

From the client's perspective, it is talking to an OpenAI-compatible model provider.

Internally, however, the gateway starts or communicates with the official Antigravity CLI and forwards the request to it.

## Why Use a Gateway?

Many AI tools support custom OpenAI-compatible endpoints even when they do not directly support every AI provider.

A compatibility layer therefore makes it possible to use Antigravity-backed inference from applications such as:

- OpenWebUI
- Continue
- custom Python applications
- internal automation
- coding agents
- workflow engines
- applications built around the OpenAI SDK

The gateway also provides one central location for authentication, logging, routing, and access control.

## Important Design Principle

The gateway should treat the official Antigravity tooling as the provider boundary.

The preferred design is:

```text
Client
  |
  v
OpenAI-compatible gateway
  |
  v
Official Antigravity CLI
  |
  v
Existing Google authentication
```

The gateway should **not** attempt to extract browser cookies, OAuth tokens, internal credentials, or reverse-engineer private Google endpoints.

Avoid architectures such as:

```text
Gateway
  |
  v
Extract Google session token
  |
  v
Call undocumented backend directly
```

Using the official client as the boundary keeps the system substantially easier to maintain and limits the amount of authentication logic handled by the gateway.

## Request Flow

A typical request would look like this:

### 1. Client Request

```json
{
  "model": "antigravity",
  "messages": [
    {
      "role": "user",
      "content": "Refactor this Python function."
    }
  ]
}
```

### 2. Gateway Translation

The gateway converts the OpenAI message structure into the input format expected by the Antigravity CLI.

Conceptually:

```python
process = await asyncio.create_subprocess_exec(
    "agy",
    "...",
    stdin=asyncio.subprocess.PIPE,
    stdout=asyncio.subprocess.PIPE,
)
```

The exact arguments depend on the supported Antigravity CLI interface.

### 3. Antigravity Execution

The Antigravity CLI executes the task using the Google account already authenticated on the host.

### 4. Output Translation

The gateway converts Antigravity output into an OpenAI-compatible response.

For non-streaming requests:

```json
{
  "id": "chatcmpl-local-123",
  "object": "chat.completion",
  "choices": [
    {
      "index": 0,
      "message": {
        "role": "assistant",
        "content": "Here is the refactored function..."
      },
      "finish_reason": "stop"
    }
  ]
}
```

For streaming requests, the gateway can emit Server-Sent Events:

```text
data: {"choices":[{"delta":{"content":"Here"}}]}

data: {"choices":[{"delta":{"content":" is"}}]}

data: {"choices":[{"delta":{"content":" the"}}]}

data: [DONE]
```

## Suggested Project Structure

```text
antigravity-openai/
|
|-- docker-compose.yml
|-- .env
|
|-- gateway/
|   |-- Dockerfile
|   |-- requirements.txt
|   |
|   |-- app/
|       |-- main.py
|       |-- auth.py
|       |-- config.py
|       |-- models.py
|       |-- openai_api.py
|       |-- antigravity.py
|       |-- streaming.py
|       `-- logging.py
|
`-- state/
    `-- antigravity/
```

## Example `/v1/models`

The gateway could expose one or more logical models:

```json
{
  "object": "list",
  "data": [
    {
      "id": "antigravity",
      "object": "model",
      "owned_by": "local"
    },
    {
      "id": "antigravity-auto",
      "object": "model",
      "owned_by": "local"
    }
  ]
}
```

The model names do not necessarily need to map directly to Google's internal model names.

They can instead represent gateway routing policies.

For example:

```text
antigravity
    -> normal Antigravity session

antigravity-fast
    -> lightweight task profile

antigravity-code
    -> coding-oriented prompt / tool profile

antigravity-auto
    -> gateway decides which profile to use
```

## Authentication

The public-facing endpoint should have its own authentication layer.

For example:

```text
Authorization: Bearer sk-local-xxxxxxxxxxxxxxxx
```

API keys should be independent from Google credentials.

The gateway should never expose the user's Google authentication details to API clients.

A simple first implementation can store hashed local API keys in configuration.

A more mature version could support:

- multiple API keys
- per-key quotas
- expiration
- revocation
- usage tracking
- IP restrictions

## Docker Considerations

The primary complication is authentication state.

If Antigravity requires an authenticated local user session, the container needs access to the relevant CLI configuration or authentication state.

Possible approaches include:

### Option A — Run Antigravity on the Host

```text
Docker gateway
      |
      v
host-side Antigravity service
```

The gateway communicates with a small host-side daemon.

This isolates Google authentication from the container.

### Option B — Mount Authentication State

```yaml
volumes:
  - ./state/antigravity:/home/app/.config/antigravity
```

This is simpler, but credentials or session state may become accessible inside the container.

### Option C — Run the Entire Gateway Outside Docker

For an initial prototype, running FastAPI directly on the server may be easier.

Docker can be added after the behaviour of the CLI and authentication mechanism is well understood.

## Security

Because this gateway may expose an AI agent capable of executing commands or interacting with files, it should be treated as a privileged service.

Recommended safeguards include:

- HTTPS only
- strong gateway API keys
- firewall restrictions
- rate limiting
- request-size limits
- command timeouts
- maximum concurrent jobs
- process isolation
- structured logging
- no arbitrary shell interpolation
- restricted filesystem permissions
- separate service account on the host
- optional VPN-only exposure

If only personal remote access is required, exposing the service through a VPN such as Tailscale or WireGuard is preferable to exposing it directly to the public internet.

## Command Execution Safety

Never construct shell commands using unescaped user input.

Avoid:

```python
os.system(f"agy --prompt '{prompt}'")
```

Prefer direct process arguments:

```python
await asyncio.create_subprocess_exec(
    "agy",
    "--prompt",
    prompt,
)
```

Even better, use standard input if the CLI supports it.

## Concurrency

A coding agent may maintain state between messages.

The gateway therefore needs to decide whether each request should:

1. create a completely new Antigravity process, or
2. attach to an existing session.

A simple first version should use one process per request.

Later versions can implement session mapping:

```text
OpenAI conversation
        |
        v
gateway session ID
        |
        v
Antigravity session
```

This can substantially improve multi-turn coding workflows.

## Streaming

Streaming support is important because many OpenAI-compatible applications expect token-like incremental responses.

The gateway can:

1. read stdout from the Antigravity process,
2. parse structured output,
3. convert content events into OpenAI delta events,
4. send them using Server-Sent Events.

Conceptually:

```python
async for event in antigravity_stream:
    chunk = convert_to_openai_chunk(event)
    yield f"data: {json.dumps(chunk)}\n\n"

yield "data: [DONE]\n\n"
```

## Tool Calling

OpenAI tool calling and an agent CLI do not necessarily share the same abstraction.

For the first version, it may be better to support:

```text
messages -> text response
```

before attempting full compatibility with:

```text
tools
tool_choice
function calls
structured output
parallel tool calls
```

Once basic chat compatibility works, these capabilities can be added selectively.

## Responses API

Supporting `/v1/responses` may eventually be more useful than implementing every historical Chat Completions feature.

A minimal first implementation could support:

```text
POST /v1/chat/completions
GET  /v1/models
```

Then add:

```text
POST /v1/responses
```

after the translation layer is stable.

## Routing Multiple Providers

The gateway can eventually become more than an Antigravity adapter.

For example:

```text
                     +-> Antigravity
                     |
OpenAI client -> Gateway -> Ollama
                     |
                     +-> Gemini API
                     |
                     +-> OpenAI
                     |
                     +-> Anthropic
```

Logical models could then represent providers:

```text
google-subscription
local-llama
openai
anthropic
auto
```

An `auto` model could choose a provider based on task characteristics, cost, quotas, privacy, or availability.

## Example Use Case

A coding tool could be configured as:

```text
Provider: OpenAI Compatible

Base URL:
https://ai.example.com/v1

API key:
sk-local-xxxxxxxx

Model:
antigravity
```

The coding tool would believe it is using a standard OpenAI-compatible service.

The gateway would transparently route the work through Antigravity.

## Limitations

This approach is an adapter, not a true implementation of the OpenAI API.

Differences may include:

- different context handling
- different system prompt behaviour
- different model capabilities
- incomplete tool-calling compatibility
- usage accounting differences
- different error semantics
- different streaming behaviour
- different cancellation behaviour
- possible Antigravity CLI interface changes

Applications that only depend on basic chat completions are therefore likely to be easier to support than applications that rely on every OpenAI API feature.

## Terms and Service Considerations

The intent of this design is to invoke official Google tooling through the user's own authenticated environment.

It should not depend on:

- reverse-engineering private endpoints
- extracting authentication tokens
- impersonating Google services
- reselling subscription capacity
- sharing personal subscription access with unrelated users

Because subscription products and their terms can change, the current Google documentation and applicable terms should be reviewed before relying on the gateway for production or multi-user use.

## Recommended MVP

A useful first milestone would include only:

```text
GET  /health
GET  /v1/models
POST /v1/chat/completions
```

with support for:

- local API-key authentication
- non-streaming responses
- streaming responses
- one Antigravity process per request
- configurable command timeout
- basic logging
- Docker deployment

Avoid initially implementing:

- embeddings
- image generation
- fine-tuning
- assistants
- batch API
- complex tool calling
- persistent conversation state

The goal of the MVP is simply to prove:

```text
OpenAI-compatible client
        ->
local gateway
        ->
Antigravity CLI
        ->
Google AI Pro usage
        ->
OpenAI-compatible response
```

Once that path is reliable, the gateway can be expanded incrementally.

## Summary

The project is effectively an **OpenAI-compatible facade over the official Antigravity client**.

Its value is not in reproducing the OpenAI platform. Instead, it provides a compatibility layer that lets existing OpenAI-oriented applications make use of AI capacity already available through the user's Google AI Pro subscription.

The cleanest implementation keeps three responsibilities separate:

```text
Client compatibility
        |
        v
OpenAI translation gateway
        |
        v
Official Antigravity client
        |
        v
Google authentication and subscription
```

This separation keeps the gateway relatively simple, avoids embedding Google credentials into client applications, and makes it possible to add additional AI providers later.
