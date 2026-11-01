# Antigravity image implementation review

Review date: 2026-11-01. Scope: `src/images.rs`, `src/antigravity_images.rs`, and `src/image_api.rs`, plus the shared transport, route middleware, `src/media.rs` validator, and focused tests. This is a read-only source review; I did not run image tests, live provider calls, or inspect credentials or saved OAuth state. The combined tests remain with the coordinator.

## Findings

### Medium — generated output accepts truncated or malformed image containers

`src/antigravity_images.rs:361-368,399-405` decodes base64 and checks only the MIME string, byte limits, and shallow magic bytes. For example, any in-bound payload beginning with the PNG signature passes as `image/png`, even if it has no IHDR, image data, IEND, sane dimensions, or complete container. The same gap applies to JPEG/WebP/GIF beyond their small prefixes/suffixes. This is weaker than the shared `media::validate_image_bytes` used on edit inputs and the approved requirement to validate complete bounded image output.

**Repro:** pass a STOP candidate containing `inlineData: {"mimeType":"image/png","data":"iVBORw0KGgo="}`; `decode_sse_images` accepts and returns the truncated bytes. Reuse the shared bounded image validator for generated output and add malformed/truncated output fixtures. No image decoding is needed.

### Medium — provider tests still use PNG-signature-only fixtures

`src/images.rs:260-266` calls `ReferenceImage::from_validated_bytes` with `b"\x89PNG\r\n\x1a\nsynthetic-png"` and unwraps it. That constructor routes through `media::normalize_content` at lines 38-43, which now requires a structurally bounded PNG with IHDR, IDAT, IEND, and dimensions, so this test fixture is rejected and the unwrap fails. Separately, `src/antigravity_images.rs:482-491` uses the same signature-plus-text style as a generated PNG fixture; the shallow output validator makes that SSE test pass, masking the preceding finding.

**Repro:** run the focused `images::tests::edits_require_validated_bounded_reference_images`; it should fail at the unwrap. Replace both fake fixtures with a structurally valid synthetic 1x1 PNG and make the output tests reject signature-only/truncated bytes. Per task direction, I did not run the stale image tests.

### Medium — image generation does not explicitly observe downstream disconnects

`src/image_api.rs:79-91` supplies `state.shutdown.child_token()` to generation. The `CompletionGuard` can cancel that token when dropped (`src/api.rs:1723-1726`), but this handler does not watch a response sender/client-closed signal while it awaits the unary upstream generation, and it returns no response until generation completes. The provider-level cancellation and deadline logic work for explicit token cancellation, shutdown, auth changes, and elapsed deadline, but there is no explicit route-to-token client-disconnect path here. A disconnected client may therefore leave a paid upstream generation running until completion/deadline if Axum keeps the handler future alive.

**Repro:** use a delayed mock `/v1internal:streamGenerateContent`, start an authenticated image request, close the client connection while the provider is waiting, and assert the upstream request is canceled promptly. Add an explicit disconnect signal or document/verify the server's handler-drop behavior before claiming this acceptance.

## Reviewed controls

- `src/antigravity_images.rs:96-143,146-185` gets token and generation from the serving `AntigravityAuth`, parses only `imageGenerationModelIds`, checks the exact requested ID before generation dispatch, and rechecks the pinned generation around discovery and dispatch. `src/image_api.rs:70-82` pins the current account generation before admission and generation. No image-model guessing or chat fallback was found.
- `src/antigravity_transport.rs:1,24-28,63-83` uses the fixed CCA origin and operation path, disallows redirects, and does not retry. It also enforces an absolute deadline, auth cancellation, and response byte ceiling. `GuardedResponse::chunk` maps interrupted response reads to an upstream-disconnected error.
- `src/antigravity_images.rs:231-339,384-395,409-464` bounds SSE bytes, lines, frames, candidates, generated image count/bytes, MIME allowlist and base64 decode; it requires each requested candidate to terminate with `STOP`, treats EOF after a complete event as success, and accepts an optional `[DONE]`. Partial or malformed streams fail before output is returned.
- Transport errors map 429 to `agy_rate_limited`, 503 overload to `agy_upstream_error`, and auth/account/protocol failures to durable 503s (`src/antigravity_transport.rs:124-187`). The API adds retry hints: 429 gets `retry-after: 10`; 503 gets `retry-after: 30`; deterministic 4xx and provider timeout 504 get `x-should-retry: false` (`src/protocol.rs:172-197`, `src/api.rs:762-769`).
- `/v1/images/generations` accepts a bounded JSON object and only `model`, `prompt`, `n`, `size`, and `response_format=b64_json`. `/v1/images/edits` accepts standard multipart `image`/`image[]`, bounds total request body and field count, rejects duplicate or unknown options, validates MIME and input image structure, and ignores the upload filename rather than storing or forwarding it (`src/image_api.rs:6-59`, `src/images.rs:61-179`). Masks and other unsupported edit controls are rejected. Captures omit multipart bytes and filenames; generated base64 is passed to the normal response capture redactor.
- The generation body requests `responseModalities:["IMAGE"]` and does not set safety settings. Tests assert that unset-safety shape; this review does not claim any upstream account acceptance.

## Test and acceptance limits

No stale image tests were started and no live provider acceptance was attempted. The integration test uses a local synthetic HTTP server and a known 1x1 PNG constant (`src/image_api.rs:113-167`); its SSE response has a STOP event followed by HTTP EOF without `[DONE]`, exercising the optional-DONE path if the image fixture passes the shared validator. Add explicit STOP+EOF and STOP+`[DONE]` transport-level cases, malformed generated image cases, requested-count >1/candidate-index cases, and delayed-disconnect cancellation coverage. The existing unit test fake fixtures need correction before a combined test run can serve as evidence.

## Coordinator closure

These findings describe the pre-correction snapshot above. Generated images now
use the same bounded structural container checker as inputs, with a separate
larger output byte bound. PNG-signature-only and truncated fixtures are rejected;
positive fixtures use a complete synthetic 1×1 PNG. Tests cover STOP plus EOF,
optional DONE and malformed/error streams. A real TCP disconnect test proves
that closing the client connection drops the unary handler and releases AGY
admission before its deadline. The generation body retains provider default
safety settings. Request IDs and the image system instruction match the pinned
OMP image implementation; model/origin fallback and retries remain disabled.
Final combined test evidence is in [validation](validation.md).
