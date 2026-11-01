# Media, web search and image endpoints

The router accepts images in chat history, OpenAI document inputs and OpenAI
hosted web search. AGY also has separate image generation/editing routes.
Availability depends on the serving account and selected model. Automated
fixtures establish serialization and client compatibility; live provider
acceptance must be checked separately after deployment.

## Image understanding

Chat Completions uses the standard user content shape:

```json
{
  "model": "<discovered-model-id>",
  "messages": [{"role": "user", "content": [
    {"type": "text", "text": "Describe this screenshot."},
    {"type": "image_url", "image_url": {
      "url": "data:image/png;base64,<base64>", "detail": "auto"
    }}
  ]}]
}
```

Responses uses `input_image` with a string `image_url` and optional `detail`.
PNG, JPEG, WebP and non-animated GIF are accepted in user messages with bounded container checks. Multimedia tool-result parts and assistant image outputs remain unsupported on the chat routes.
The router does not fully decode images. AGY requires inline data and the
serving roster must advertise `supportsImages` for that exact model. OpenAI
also accepts HTTPS image URLs and `auto`/`low`/`high` detail hints; its model
decides whether vision is available. The router never downloads URLs or reads
client filesystem paths. Uploaded file IDs are unsupported.

The existing **1 MiB request/history limit** includes base64 expansion, JSON
and conversation history. Each inline media item is at most **768 KiB decoded**;
eight images are allowed across the complete request. The effective usable
payload is smaller when the full request would exceed 1 MiB. Cross-provider
fallback is refused for media; choose an available model explicitly.

## OpenAI documents

Responses accepts `input_file` with either `filename` plus a base64 data URL in
`file_data`, or an HTTPS `file_url`. Inline formats are PDF, UTF-8 TXT/CSV,
JSON, DOCX, PPTX and XLSX, with their matching MIME and filename extension.
Chat Completions accepts PDF through `{ "type": "file", "file": {
"filename": "document.pdf", "file_data": "data:application/pdf;base64,..." } }`.
Only one file is allowed across a request, within the same media/body bounds.

PDF/OOXML checks are bounded MIME, extension and signature hints, not complete
document validation. Files are forwarded without local extraction, execution,
conversion or disk storage; the upstream model handles document processing and
can reject a malformed or unsupported document. Other inline MIME types,
Files API uploads/IDs, local paths and AGY document inputs remain unsupported.

## OpenAI web search

Responses accepts `tools: [{"type":"web_search"}]` or the supported preview
variant. `include: ["web_search_call.action.sources"]` requests available
sources. The router's Chat endpoint also accepts this hosted-tool extension
and translates it to its upstream Responses transport. It does not call an
upstream Chat Completions search endpoint. `web_search_options` stays unsupported.

Search output items and URL citations are returned on Responses; Chat returns
assistant annotations. Streaming responses include citation annotations at
completion. Hosted search is separate from client function calls: the client
does not execute a `web_search_call`. Function tools can still be supplied for
client execution under the supported tool-choice contract. AGY rejects hosted
search, and account/workspace policy may also reject it on OpenAI.

This does not enable OpenAI subscription image generation, file search, Code
Interpreter, hosted MCP, tool search or audio/video. The
[subscription limitations](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations)
remain the provider boundary.

## AGY image generation and editing

Use an exact image model advertised by the account in `/v1/models` or
`/v1/capabilities`. Image-only entries are separate from chat routing; there is
no guessed model, model fallback, origin fallback or automatic generation retry.

`POST /v1/images/generations` accepts JSON:

```json
{
  "model": "<advertised-image-model-id>",
  "prompt": "A lighthouse at sunrise",
  "n": 1,
  "size": "1024x1024",
  "response_format": "b64_json"
}
```

`POST /v1/images/edits` accepts multipart `model`, `prompt`, `image` or
`image[]`, and the same optional `n`, `size` and `response_format` controls.
It supports reference-image editing; mask-based editing is unsupported.
Both routes return `created` and `data[].b64_json`.
Hosted image URLs, quality/style controls and streaming image output are not
implemented. Use a client option that accepts base64 output.

Up to four output images are allowed. Sizes `auto`, `1024x1024`, `1536x1024`
and `1024x1536` map to reviewed AGY aspect-ratio/resolution settings; exact
pixel dimensions are not guaranteed. Native ratio strings and `1K`/`2K`/`4K`
are also accepted. Output is bounded to 8 MiB decoded per image, 12 MiB decoded
in total and the transport's 16 MiB wire limit. Complete successful candidates
are required; partial image data is never returned.

## Clients and diagnostics

n8n Agent, Assistant and workflow integrations keep their existing Chat or
Responses credentials and can send the supported content parts. Image app
nodes must use an advertised AGY image ID and base64 response mode. Features
are explicit per provider in capabilities; these do not imply that every
installed n8n version exposes every option in its UI.

The image routes share model-key authentication, rate limits, the AGY queue,
account-generation guards, deadline and telemetry. Private captures omit
inline binary data before truncation. Persisted diagnostics contain counts,
types, timing and safe failure reasons; media bytes, document contents,
filenames, URLs, prompts and generated text are not persisted.

See [the plan and DAG](media-search-plan.md) and the review reports
([contracts](media-search-review.md), [media/API](media-api-review.md),
[AGY images](agy-images-review.md)) for design and validation work.

## Protocol provenance

The reviewed OMP identity remains Hub 2.28.2 / OMP 686b5e2f / adapter v4.
This feature review adds the pinned image adapter/shared/types source hashes
to the existing baseline, for 68 pairs of public source hashes. It introduces no
additional OAuth scope, registration change or automatic state migration. The
added `aicode` consent requirement from the preceding baseline update still
applies when adopting that update for the first time.
