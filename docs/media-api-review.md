# Media API cross-review

Read-only review of the current media changes against `docs/media-search-plan.md`.
No tests or live provider calls were run for this review. The image/media container
parsers were inspected for bounded indexing; no short-buffer panic was found.

## Findings

### P2 — Chat Completions does not enforce the advertised one-file request limit

`src/media.rs:138-158` accepts Chat Completions `file` content blocks and converts
them to `input_file`. `src/protocol.rs:453-529` accumulates the image count only;
unlike Responses normalization at `src/protocol.rs:677-721`, it never accumulates
or checks file count. The capabilities response advertises `max_files: 1` at
`src/api.rs:843`.

Reproduction: send one Chat Completions request with two user messages, each
containing a `file` block whose `filename` is `a.pdf` / `b.pdf` and whose
`file_data` is `data:application/pdf;base64,` plus the base64 of the synthetic
bytes `%PDF-1.7\nsynthetic\n%%EOF`. Both files normalize successfully and the
request proceeds with two files despite `max_files: 1`. Count files across the
whole Chat request and reject above the advertised limit, or publish endpoint
specific limits.

### P2 — The OOXML and PDF checks accept malformed containers

`src/media.rs:340-348` treats a PDF as valid when it has a `%PDF-1.<digit>`
prefix and contains `%%EOF`; it does not verify PDF structure or that EOF is a
terminal marker. `valid_zip` only checks that bytes start with a local-file
signature and contain an end-of-central-directory signature. It does not parse
the ZIP directory or require the OOXML package parts. The test at
`src/media.rs:862-877` explicitly accepts a synthetic byte string made from
`PK\x03\x04`, 20 zero bytes, `PK\x05\x06`, and 18 zero bytes as a DOCX.

Reproduction: submit that exact byte string as `data:application/vnd.openxmlformats-officedocument.wordprocessingml.document;base64,...`
with `filename: "doc.docx"`; normalization succeeds although the bytes are not
a DOCX or a valid ZIP archive. Similarly, `%PDF-1.7junk%%EOF` passes the current
PDF predicate. These inputs are forwarded to OpenAI and fail only upstream.
Either validate the relevant container structure or describe these checks as
signature heuristics and avoid claiming MIME/container validation.

### P2 — Common OpenAI image `size` values are rejected by the generation route

`src/images.rs:104-120` accepts only the listed aspect ratios or `1K`, `2K`, and
`4K`. OpenAI's Images API documents values such as `auto`, `1024x1024`,
`1536x1024`, and `1024x1536`; a standard client sending one of these receives a
local 400 before discovery or dispatch. This is an OpenAI/n8n compatibility gap
for `/v1/images/generations` and `/v1/images/edits`, even though requests that
omit `size` work. Map only reviewed OpenAI dimensions to reviewed CCA settings,
or document the accepted `size` subset in the capability response. [OpenAI
Images API reference](https://developers.openai.com/api/reference/resources/images/methods/generate)

### P2 — Responses file MIME support omits documented types without an explicit subset contract

`src/media.rs:292-300` accepts only PDF, plain text, CSV, JSON, and OOXML
DOCX/PPTX/XLSX. Current Responses file-input documentation also lists common
types such as Markdown/HTML/XML, RTF/ODT, legacy DOC/PPT/XLS, and additional
text/code MIME types. These standard `file_data` requests are rejected locally
as unsupported MIME despite the generic `file_input: "model_dependent"`
capability at `src/api.rs:882`. Either extend the allowlist with matching
filename/content checks or publish the restricted format list. [OpenAI file
inputs guide](https://developers.openai.com/api/docs/guides/file-inputs)

## Checks that looked sound

- `src/media.rs` validates image container slices only after length checks; the
  PNG, JPEG, WebP, and GIF parser indexing inspected here has no short-buffer
  panic path. `src/image_api.rs:71-86` also guards WebP sniffing with a 12-byte
  check before slicing.
- HTTPS media URLs are validated and forwarded; no router-side fetch or local
  path handling was found. AGY serialization rejects remote image URLs before
  dispatch, and the parent request path rejects AGY files and unsupported image
  models before provider admission (`src/api.rs:1116-1125`).
- Image edits are body-bounded by middleware and the route checks cumulative
  multipart field bytes (`src/api.rs:649-679`, `src/image_api.rs:42-68`). MIME
  sniffing is used only when the multipart MIME is absent or octet-stream; the
  shared image validator checks the actual bytes afterward.
- The image-only list is derived from account discovery (`src/antigravity.rs:168-181`)
  and exposed separately; generation re-fetches `imageGenerationModelIds`, checks
  the requested exact ID and pinned generation, and performs one Generate call
  (`src/antigravity_images.rs:96-179`). No guessed model, provider fallback, or
  automatic generation retry was found.
- Request/response captures are memory-only and bounded. Inline data URLs and
  `b64_json` values are redacted by `src/monitor/payload.rs`; image route event
  metadata records counts and byte sizes rather than prompt, filename, or image
  bytes (`src/image_api.rs:66-76, 93-110`).

## Test status and limits

The findings above come from source inspection and the synthetic fixtures in the
current diff. No test suite was run for this read-only review, per task
instruction; no live provider acceptance was performed.

## Coordinator closure

These findings describe the pre-correction snapshot above; line references are
historical. The combined implementation now counts Chat files across all turns
and tests rejection of a second PDF. Standard OpenAI size aliases map to the
reviewed AGY ratio/resolution controls, with exact pixels explicitly unpromised.
Capabilities and the media guide enumerate the supported inline document subset.
PDF/OOXML validation is intentionally described as signature hints; documents
are never extracted locally and upstream processing remains authoritative.
Remote AGY image URLs are rejected before admission as well as by serialization.
Final regression evidence is in [validation](validation.md).
