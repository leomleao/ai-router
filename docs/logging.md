# Logging

The gateway writes one JSON object per line to stderr (`docker logs ai-router`).
Every event has `ts` (UTC, milliseconds) and `event`. Logs carry identifiers,
counts, sizes, codes and value-free validation summaries. They never contain
prompts, instructions, tool arguments, model output, credentials or provider
bodies. When a failure needs that content to diagnose, it is kept only in the
memory-only monitor capture (latest 100 requests, cleared on restart):

```sh
docker logs ai-router 2>&1 | grep '"event":"provider_request_failed"'
docker exec --user 10001:10001 ai-router ai-router monitor --detail <request_id>
```

`request_id` is also returned to the client as `x-request-id`.

## Request lifecycle

A dispatched request logs `model_request`, then exactly one terminal event:
`model_request_completed`, `provider_request_failed` or
`model_request_ended_early`. A request refused before dispatch logs
`request_rejected` instead.

Shared fields on lifecycle events: `request_id`, `client_id` (key ID),
`origin` (`method`, `path`, `client_ip`, `user_agent`), `endpoint`, `model`
(served), `requested_model` (set when a model fallback served it), `provider`,
`streaming`, `elapsed_ms`, `startup_ms` (provider accepted the request),
`first_output_ms` (first text or tool call) and `stream_committed` (the SSE 200
was already sent, so a failure reached the client as a stream event).

| Event | When | Main fields |
| --- | --- | --- |
| `request_rejected` | `stage:"admission"`: rate limit, header or body bounds, authentication, scope, body timeout (sampled to 30 per minute). `stage:"request"`: any later error returned before dispatch, such as validation, unknown model or queue saturation | `status`, `code`, `param`, `message` (the router's own public message), `origin`; for `request` also `request` (shape, below), `provider`, `served_model`, `duration_ms` |
| `model_request` | After validation, admission and queueing, immediately before the provider call | `request` (shape), `dispatch_ms` (time to dispatch, including the queue), `client_tool_names`, `strict_tool_names`, `tool_required`, `tool_relay`, `parallel_tool_calls`, `output_schema`, `effort` (after the effort policy), `ignored_fields`, `disclosures` (every `X-AI-Router-*` header and value), `store_items`, input/instruction/tool-schema byte counts |
| `tool_call_relayed` | A noncompliant call to a non-strict tool was handed to the client (see "Noncompliant tool calls" in [capabilities](capabilities.md)) | `tool_name`, `call_id`, `declared` (false for an undeclared tool), `diagnostics` (`reason` `unknown_tool` or `schema_mismatch`, `validation_errors` with schema paths, expected and actual JSON types and missing required field names) |
| `model_request_completed` | The provider returned a result | `status` (`completed`/`incomplete`), `finish_reason`, `text_bytes`, `structured_output`, `tool_call_count`, `tool_call_names`, `relayed_tool_calls`, `opaque_output_items` (e.g. reasoning item counts), `usage`, `usage_partial`, `upstream_request_id` |
| `provider_request_failed` | The provider call failed or its output was rejected | `status`, `code`, `param`, `retry` (`x-should-retry`), `explanation`, `upstream_request_id`, `diagnostics` (below), `private_capture` (the monitor capture holds the offending content) |
| `model_request_ended_early` | No terminal event: the client disconnected, or the request deadline passed | `status` (`cancelled`/`failed`), `code` (`client_disconnected`/`provider_timeout`) |

The request shape (`request`) summarizes what the client sent without content:
`requested_model`, `stream`, `fields` (top-level field names), `input_items`
(Responses item types, or Chat roles, with counts), `tool_count`,
`strict_tools`, `tool_types`, `tool_choice` (value, or type and mode),
`parallel_tool_calls`, `store`, `include`, `reasoning_effort`,
`reasoning_summary` and `output_format`. Field names and `user_agent` usually
identify the client (n8n, Twenty, Supabase Studio, Cognee).

## Failure diagnostics

`provider_request_failed.diagnostics.phase` says where the failure happened:

| Phase | Meaning | Details |
| --- | --- | --- |
| `tool_validation` | A returned tool call was rejected | `reason`: `invalid_call_id`, `duplicate_call_id`, `call_not_completed`, `arguments_not_json`, `invalid_tool_name`, `unknown_tool`, `arguments_not_object`, `schema_mismatch`, `parallel_calls_not_permitted` or `no_call_returned`; `provider_item` (`name`, `status`, `call_id_present`, `arguments_bytes`); `validation` (schema error summary). The rejected call itself is in the monitor capture as `upstream_response` |
| `structured_output` | JSON output was withheld | `reason` `not_json` (with `json_error_line`/`column` and `markdown_fenced`), `schema_mismatch` (with `validation_errors`) or `provider_incomplete`; `text_bytes`. The withheld text is in the monitor capture |
| `upstream_http`, `upstream_error_body`, `upstream_stream` | The provider returned an HTTP error, an error body with a 2xx status, or a `response.failed`/`error` event | `http_status`, `upstream_code`, `upstream_type`, `upstream_param`, `upstream_status`, `body_bytes`; stream failures add `event_type` and `text_bytes`. The upstream body is in the monitor capture |
| `upstream_transport` | Connection or stream failure | `stage` (`send`/`stream`), `kind` (`timeout`, `connect`, `body`, `decode`, `request`, `other`); stream failures add `bytes_received`, `text_bytes` |
| `wire_validation` | An Antigravity frame was malformed or unsupported | `reason` |

Antigravity tool failures use the same `tool_validation` phase and keep the
rejected `functionCall` part in the capture. Errors without diagnostics are the
router's own conditions (deadline, cancellation, account change), identified by
`code`.

## Provider state

| Event | When | Fields |
| --- | --- | --- |
| `provider_status_changed` | A catalogue probe changes readiness, error or listed models (probes repeat while the gateway runs; unchanged results are not logged) | `provider`, `authenticated`, `error`, `previous_error`, `version`, `model_count`, `models_added`, `models_removed` |
| `provider_latched` | An OpenAI failure stops further upstream calls: `kind:"transient"` until the next successful probe, `kind:"durable"` until an operator resumes the account | `provider`, `kind`, `status`, `code`, `explanation`, `note` |
| `known_models_store_ignored`, `known_models_store_failed` | The persisted model-ownership file was unreadable or could not be written | `reason` |
