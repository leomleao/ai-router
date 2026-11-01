//! Antigravity image generation over the shared authenticated transport.
use crate::{
    antigravity_auth::{AccessToken, AntigravityAuth},
    antigravity_transport::{AntigravityTransport, MAX_RESPONSE_BYTES, Operation},
    images::{ImageRequest, MAX_GENERATED_IMAGE_BYTES, MAX_GENERATED_TOTAL_BYTES, MAX_IMAGE_COUNT},
    protocol::ApiError,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{collections::HashMap, time::Instant};
use tokio_util::sync::CancellationToken;

const MAX_SSE_FRAME_BYTES: usize = MAX_RESPONSE_BYTES;
const MAX_MODELS: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedImage {
    pub b64_json: String,
    pub mime_type: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageUsage {
    pub prompt_tokens: Option<u64>,
    pub candidate_tokens: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageGenerationResult {
    pub images: Vec<GeneratedImage>,
    pub usage: Option<ImageUsage>,
    pub upstream_request_id: Option<String>,
}

impl ImageGenerationResult {
    pub fn image_count(&self) -> usize {
        self.images.len()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageModelCatalogue {
    pub generation: String,
    pub model_ids: Vec<String>,
}

#[derive(Clone)]
pub struct AntigravityImages {
    auth: AntigravityAuth,
    transport: AntigravityTransport,
}

impl AntigravityImages {
    pub fn new(auth: AntigravityAuth) -> Self {
        Self {
            transport: AntigravityTransport::new(auth.clone()),
            auth,
        }
    }

    /// Standalone discovery exercised by synthetic provider contract tests.
    #[cfg(test)]
    pub async fn discover_models(
        &self,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<ImageModelCatalogue, ApiError> {
        let work = self.discover_inner(deadline);
        tokio::pin!(work);
        tokio::select! { biased;
            _ = cancel.cancelled() => Err(ApiError::new(499, "request_cancelled", "Request was cancelled")),
            _ = tokio::time::sleep_until(deadline.into()) => Err(ApiError::new(504, "provider_timeout", "Request deadline exceeded")),
            result = &mut work => result.map(|(catalogue, _)| catalogue),
        }
    }

    /// Rediscover the exact ID before a single generation and enforce the caller's pinned account epoch.
    pub async fn generate(
        &self,
        request: ImageRequest,
        pinned_generation: &str,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<ImageGenerationResult, ApiError> {
        let work = self.generate_inner(&request, pinned_generation, deadline);
        tokio::pin!(work);
        tokio::select! { biased;
            _ = cancel.cancelled() => Err(ApiError::new(499, "request_cancelled", "Request was cancelled")),
            _ = tokio::time::sleep_until(deadline.into()) => Err(ApiError::new(504, "provider_timeout", "Request deadline exceeded")),
            result = &mut work => result,
        }
    }

    async fn discover_inner(
        &self,
        deadline: Instant,
    ) -> Result<(ImageModelCatalogue, AccessToken), ApiError> {
        if let Some(error) = self.auth.availability_error().await {
            return Err(error);
        }
        let expected_generation = self.auth.generation().await?;
        let token = self.auth.token(deadline).await?;
        ensure_generation(&expected_generation, &token.generation)?;

        let catalogue = self
            .transport
            .post_json(
                Operation::Models,
                &json!({"project":token.project}),
                &token,
                deadline,
            )
            .await?
            .json()
            .await?;
        ensure_generation(&expected_generation, &self.auth.generation().await?)?;
        let advertised = catalogue
            .get("imageGenerationModelIds")
            .and_then(Value::as_array)
            .filter(|ids| ids.len() <= MAX_MODELS)
            .ok_or_else(|| invalid("invalid_image_catalogue"))?;
        let mut seen = std::collections::HashSet::new();
        let mut ids = Vec::with_capacity(advertised.len());
        for value in advertised {
            let id = value
                .as_str()
                .filter(|s| crate::config::valid_model_id(s))
                .ok_or_else(|| invalid("invalid_image_catalogue"))?;
            if !seen.insert(id) {
                return Err(invalid("invalid_image_catalogue"));
            }
            ids.push(id);
        }
        ensure_generation(&expected_generation, &self.auth.generation().await?)?;
        Ok((
            ImageModelCatalogue {
                generation: expected_generation,
                model_ids: ids.into_iter().map(str::to_owned).collect(),
            },
            token,
        ))
    }

    async fn generate_inner(
        &self,
        request: &ImageRequest,
        pinned_generation: &str,
        deadline: Instant,
    ) -> Result<ImageGenerationResult, ApiError> {
        ensure_generation(pinned_generation, &self.auth.generation().await?)?;
        let (catalogue, token) = self.discover_inner(deadline).await?;
        ensure_generation(pinned_generation, &catalogue.generation)?;
        if !catalogue.model_ids.iter().any(|id| id == &request.model) {
            return Err(ApiError::new(
                400,
                "provider_model_unavailable",
                "Requested image model is not advertised for the serving account",
            )
            .with_param("model"));
        }

        ensure_generation(pinned_generation, &self.auth.generation().await?)?;
        let body = request.wire_body(&token.project, &request.model);
        let mut response = self
            .transport
            .post_json(Operation::Generate, &body, &token, deadline)
            .await?;
        let upstream_request_id = response.request_id.clone();
        ensure_generation(pinned_generation, &self.auth.generation().await?)?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|mut error| {
            error.upstream_request_id = upstream_request_id.clone();
            if error.diagnostics.is_none() {
                error.diagnostics = Some(crate::protocol::FailureDiagnostics::metadata(json!({
                    "phase":"image_stream","bytes_received":bytes.len()
                })));
            }
            error
        })? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(invalid("image_response_too_large"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let (images, usage) = decode_sse_images(&bytes, request.count).map_err(|mut error| {
            error.upstream_request_id = upstream_request_id.clone();
            if let Some(details) = error.diagnostics.as_mut() {
                details.metadata["bytes_received"] = json!(bytes.len());
            }
            error
        })?;
        Ok(ImageGenerationResult {
            images,
            usage,
            upstream_request_id,
        })
    }
}

fn ensure_generation(expected: &str, actual: &str) -> Result<(), ApiError> {
    if expected == actual {
        Ok(())
    } else {
        Err(ApiError::new(
            503,
            "provider_account_changed",
            "Antigravity account changed during image generation",
        ))
    }
}

fn invalid(code: &'static str) -> ApiError {
    let mut error = ApiError::new(
        502,
        code,
        "Antigravity returned an invalid or unsupported image response",
    );
    error.durable = true;
    error.diagnostics = Some(crate::protocol::FailureDiagnostics::metadata(
        json!({"phase":"image_wire_validation","reason":code}),
    ));
    error
}

// Persist only numeric codes and reviewed status values, never provider messages.
fn provider_frame_error(value: &Value) -> ApiError {
    let mut error = invalid("image_provider_error");
    let code = value["code"].as_u64().filter(|code| *code <= 65535);
    let status = value["status"].as_str().filter(|status| {
        matches!(
            *status,
            "INVALID_ARGUMENT"
                | "UNAUTHENTICATED"
                | "PERMISSION_DENIED"
                | "RESOURCE_EXHAUSTED"
                | "FAILED_PRECONDITION"
                | "INTERNAL"
                | "UNAVAILABLE"
                | "DEADLINE_EXCEEDED"
                | "CANCELLED"
        )
    });
    let metadata = &mut error.diagnostics.as_mut().unwrap().metadata;
    metadata["provider_code"] = json!(code);
    metadata["provider_status"] = json!(status);
    error
}

#[derive(Default)]
struct ImageDecoder {
    frames: usize,
    done: bool,
    terminal: HashMap<usize, String>,
    images: Vec<GeneratedImage>,
    total_bytes: usize,
    usage: Option<ImageUsage>,
}

impl ImageDecoder {
    fn frame(&mut self, data: &[u8], requested: usize) -> Result<(), ApiError> {
        self.frames += 1;
        if self.frames > 16384 {
            return Err(invalid("image_event_limit"));
        }
        if data == b"[DONE]" {
            if self.done
                || self.terminal.is_empty()
                || self.terminal.values().any(|reason| reason != "STOP")
            {
                return Err(invalid("invalid_image_terminal"));
            }
            self.done = true;
            return Ok(());
        }
        if self.done {
            return Err(invalid("content_after_image_terminal"));
        }
        let value: Value =
            serde_json::from_slice(data).map_err(|_| invalid("invalid_image_json"))?;
        if let Some(error) = value.get("error") {
            return Err(provider_frame_error(error));
        }
        let response = value
            .get("response")
            .filter(|v| v.is_object())
            .ok_or_else(|| invalid("invalid_image_envelope"))?;
        if let Some(error) = response.get("error") {
            return Err(provider_frame_error(error));
        }
        if response.pointer("/promptFeedback/blockReason").is_some() {
            return Err(invalid("image_provider_error"));
        }
        if let Some(candidates) = response.get("candidates") {
            let candidates = candidates
                .as_array()
                .ok_or_else(|| invalid("invalid_image_candidates"))?;
            if candidates.len() > requested {
                return Err(invalid("too_many_image_candidates"));
            }
            let mut event_indexes = std::collections::HashSet::new();
            for (ordinal, candidate) in candidates.iter().enumerate() {
                if !candidate.is_object() {
                    return Err(invalid("invalid_image_candidate"));
                }
                let index = match candidate.get("index") {
                    Some(value) => value
                        .as_u64()
                        .and_then(|n| usize::try_from(n).ok())
                        .ok_or_else(|| invalid("invalid_image_candidate"))?,
                    None if requested == 1 => ordinal,
                    None => return Err(invalid("invalid_image_candidate")),
                };
                if index >= requested
                    || self.terminal.contains_key(&index)
                    || !event_indexes.insert(index)
                {
                    return Err(invalid("invalid_image_candidate"));
                }
                if let Some(content) = candidate.get("content") {
                    if content.get("role").is_some_and(|role| role != "model") {
                        return Err(invalid("invalid_image_candidate"));
                    }
                    let parts = content
                        .get("parts")
                        .and_then(Value::as_array)
                        .ok_or_else(|| invalid("invalid_image_parts"))?;
                    for part in parts {
                        if let Some(inline) = part.get("inlineData") {
                            self.image(inline)?;
                            if self.images.len() > requested {
                                return Err(invalid("too_many_generated_images"));
                            }
                        }
                        if part.get("inline_data").is_some() {
                            return Err(invalid("invalid_image_part"));
                        }
                    }
                }
                if let Some(reason) = candidate.get("finishReason") {
                    let reason = reason
                        .as_str()
                        .ok_or_else(|| invalid("invalid_image_terminal"))?;
                    if reason != "STOP" {
                        return Err(invalid("image_generation_incomplete"));
                    }
                    self.terminal.insert(index, reason.to_owned());
                }
            }
        }
        if let Some(metadata) = response.get("usageMetadata") {
            if !metadata.is_object() {
                return Err(invalid("invalid_image_usage"));
            }
            let prompt_tokens = metadata
                .get("promptTokenCount")
                .map(bounded_tokens)
                .transpose()?
                .flatten();
            let candidate_tokens = metadata
                .get("candidatesTokenCount")
                .map(bounded_tokens)
                .transpose()?
                .flatten();
            self.usage = Some(ImageUsage {
                prompt_tokens,
                candidate_tokens,
            });
        }
        Ok(())
    }

    fn image(&mut self, inline: &Value) -> Result<(), ApiError> {
        let mime = inline
            .get("mimeType")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("invalid_image_mime"))?;
        let data = inline
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("invalid_image_data"))?;
        if !matches!(
            mime,
            "image/png" | "image/jpeg" | "image/webp" | "image/gif"
        ) {
            return Err(invalid("invalid_image_mime"));
        }
        let max_encoded = MAX_GENERATED_IMAGE_BYTES.saturating_mul(4) / 3 + 8;
        if data.is_empty() || data.len() > max_encoded {
            return Err(invalid("generated_image_too_large"));
        }
        let decoded = STANDARD
            .decode(data)
            .map_err(|_| invalid("invalid_image_base64"))?;
        if decoded.is_empty() || decoded.len() > MAX_GENERATED_IMAGE_BYTES {
            return Err(invalid("invalid_image_bytes"));
        }
        crate::media::validate_image_container(mime, &decoded)
            .map_err(|_| invalid("invalid_image_bytes"))?;
        self.total_bytes = self
            .total_bytes
            .checked_add(decoded.len())
            .ok_or_else(|| invalid("generated_images_too_large"))?;
        if self.images.len() >= MAX_IMAGE_COUNT || self.total_bytes > MAX_GENERATED_TOTAL_BYTES {
            return Err(invalid("generated_images_too_large"));
        }
        self.images.push(GeneratedImage {
            b64_json: STANDARD.encode(decoded),
            mime_type: mime.to_owned(),
        });
        Ok(())
    }

    fn finish(
        self,
        requested: usize,
    ) -> Result<(Vec<GeneratedImage>, Option<ImageUsage>), ApiError> {
        if self.terminal.len() != requested || self.terminal.values().any(|reason| reason != "STOP")
        {
            return Err(invalid("missing_image_terminal"));
        }
        if self.images.len() != requested {
            return Err(invalid("incomplete_image_output"));
        }
        Ok((self.images, self.usage))
    }
}

fn decode_sse_images(
    bytes: &[u8],
    requested: usize,
) -> Result<(Vec<GeneratedImage>, Option<ImageUsage>), ApiError> {
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(invalid("image_response_too_large"));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("invalid_image_utf8"))?;
    let mut decoder = ImageDecoder::default();
    let mut data = Vec::new();
    let mut event_error = false;
    for raw_line in text.split_inclusive('\n') {
        let line = raw_line
            .strip_suffix('\n')
            .unwrap_or(raw_line)
            .strip_suffix('\r')
            .unwrap_or(raw_line.strip_suffix('\n').unwrap_or(raw_line));
        if line.len() > MAX_SSE_FRAME_BYTES {
            return Err(invalid("image_frame_too_large"));
        }
        if line.is_empty() {
            if event_error {
                return Err(invalid("image_provider_error"));
            }
            if !data.is_empty() {
                decoder.frame(&data, requested)?;
                data.clear();
            }
            continue;
        }
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => {
                if !data.is_empty() {
                    data.push(b'\n');
                }
                data.extend_from_slice(value.as_bytes());
                if data.len() > MAX_SSE_FRAME_BYTES {
                    return Err(invalid("image_frame_too_large"));
                }
            }
            "event" if value == "error" => event_error = true,
            "event" if matches!(value, "" | "message") => {}
            "id" | "retry" => {}
            _ => return Err(invalid("unsupported_image_event")),
        }
    }
    if event_error || !data.is_empty() {
        return Err(invalid("truncated_image_stream"));
    }
    decoder.finish(requested)
}

fn bounded_tokens(value: &Value) -> Result<Option<u64>, ApiError> {
    value
        .as_u64()
        .filter(|n| *n <= 10_000_000)
        .map(Some)
        .ok_or_else(|| invalid("invalid_image_usage"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn frame(value: Value) -> String {
        format!("data: {value}\n\n")
    }
    fn png() -> String {
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/T9sAAAAASUVORK5CYII=".to_owned()
    }

    #[test]
    fn sse_requires_successful_terminal_valid_image_and_withholds_partial_output() {
        let good = frame(
            json!({"response":{"candidates":[{"index":0,"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":png()}}]},"finishReason":"STOP"}]}}),
        );
        let (output, _) = decode_sse_images(good.as_bytes(), 1).unwrap();
        assert_eq!(output.len(), 1);
        let done = format!("{}data: [DONE]\n\n", good);
        assert_eq!(decode_sse_images(done.as_bytes(), 1).unwrap().0.len(), 1);
        for bad in [
            frame(
                json!({"response":{"candidates":[{"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":png()}}]},"finishReason":"MAX_TOKENS"}]}}),
            ),
            frame(
                json!({"response":{"candidates":[{"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":"%%%"}}]},"finishReason":"STOP"}]}}),
            ),
            frame(
                json!({"response":{"candidates":[{"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":png()}}]}}]}}),
            ),
            frame(
                json!({"response":{"candidates":[{"content":{"parts":[{"inlineData":{"mimeType":"image/jpeg","data":png()}}]},"finishReason":"STOP"}]}}),
            ),
            frame(
                json!({"response":{"candidates":[{"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":"iVBORw0KGgo="}}]},"finishReason":"STOP"}]}}),
            ),
            frame(
                json!({"response":{"candidates":[{"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":STANDARD.encode(&STANDARD.decode(png()).unwrap()[..20])}}]},"finishReason":"STOP"}]}}),
            ),
            frame(json!({"error":{"message":"synthetic private upstream detail"}})),
            "event: error\ndata: {}\n\n".to_owned(),
            format!(
                "{}{}",
                frame(json!({"response":{"candidates":[{"finishReason":"STOP"}]}})),
                "data: [DONE]\n\ndata: {}\n\n"
            ),
        ] {
            assert!(decode_sse_images(bad.as_bytes(), 1).is_err());
        }
        let pair = frame(json!({"response":{"candidates":[
            {"index":1,"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":png()}}]},"finishReason":"STOP"},
            {"index":0,"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":png()}}]},"finishReason":"STOP"}
        ]}}));
        assert_eq!(decode_sse_images(pair.as_bytes(), 2).unwrap().0.len(), 2);
        assert!(decode_sse_images(pair.as_bytes(), 1).is_err());
        let duplicate = pair.replace("\"index\":1", "\"index\":0");
        assert!(decode_sse_images(duplicate.as_bytes(), 2).is_err());
        let too_large = STANDARD.encode(vec![0u8; MAX_GENERATED_IMAGE_BYTES + 1]);
        let over = format!(
            "{}{}",
            frame(
                json!({"response":{"candidates":[{"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":too_large}}]},"finishReason":"STOP"}]}})
            ),
            "data: [DONE]\n\n"
        );
        assert!(decode_sse_images(over.as_bytes(), 1).is_err());
    }

    #[test]
    fn invalid_provider_response_is_explicitly_non_retryable() {
        let error = invalid("invalid_image_bytes");
        assert!(error.durable);
        assert_eq!(error.retry_header(), Some(("x-should-retry", "false")));
        let error = provider_frame_error(
            &json!({"code":429,"status":"RESOURCE_EXHAUSTED","message":"private prompt"}),
        );
        let metadata = error.diagnostics.unwrap().metadata;
        assert_eq!(metadata["provider_code"], 429);
        assert_eq!(metadata["provider_status"], "RESOURCE_EXHAUSTED");
        assert!(!metadata.to_string().contains("private prompt"));
    }

    #[tokio::test]
    async fn advertised_id_is_required_before_generation_dispatch() {
        use crate::antigravity_auth::AntigravityAuth;
        use axum::{Json, Router, routing::post};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let generated = Arc::new(AtomicUsize::new(0));
        let generated_route = generated.clone();
        let app = Router::new()
            .route(
                "/v1internal:fetchAvailableModels",
                post(|| async { Json(json!({"imageGenerationModelIds":["fixture-image"]})) }),
            )
            .route(
                "/v1internal:streamGenerateContent",
                post(move || {
                    let generated = generated_route.clone();
                    async move {
                        generated.fetch_add(1, Ordering::SeqCst);
                        (
                            [("content-type", "text/event-stream")],
                            "data: [DONE]\\n\\n",
                        )
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let auth = AntigravityAuth::synthetic(dir.path().join("state"), origin).await;
        let generation = auth.generation().await.unwrap();
        let provider = AntigravityImages::new(auth);
        let request =
            ImageRequest::from_generation_json(&json!({"model":"unadvertised","prompt":"fixture"}))
                .unwrap();
        let error = provider
            .generate(
                request,
                &generation,
                Instant::now() + Duration::from_secs(2),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "provider_model_unavailable");
        assert_eq!(generated.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[tokio::test]
    async fn mock_http_generation_uses_advertised_id_and_reviewed_wire_shape() {
        use crate::antigravity_auth::AntigravityAuth;
        use axum::{Json, Router, routing::post};
        use std::sync::{Arc, Mutex};
        let captured = Arc::new(Mutex::new(None));
        let capture_route = captured.clone();
        let image_data = png();
        let event = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"response":{"candidates":[{"index":0,"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":image_data}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3}}})
        );
        let app = Router::new()
            .route(
                "/v1internal:fetchAvailableModels",
                post(|| async { Json(json!({"imageGenerationModelIds":["fixture-image"]})) }),
            )
            .route(
                "/v1internal:streamGenerateContent",
                post(move |Json(body): Json<Value>| {
                    let captured = capture_route.clone();
                    let event = event.clone();
                    async move {
                        *captured.lock().unwrap() = Some(body);
                        ([("content-type", "text/event-stream")], event)
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let auth = AntigravityAuth::synthetic(dir.path().join("state"), origin).await;
        let provider = AntigravityImages::new(auth);
        let catalogue = provider
            .discover_models(
                Instant::now() + Duration::from_secs(2),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(catalogue.model_ids, vec!["fixture-image"]);
        let request = ImageRequest::from_generation_json(
            &json!({"model":"fixture-image","prompt":"synthetic lighthouse","size":"16:9"}),
        )
        .unwrap();
        let result = provider
            .generate(
                request,
                &catalogue.generation,
                Instant::now() + Duration::from_secs(2),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.image_count(), 1);
        assert_eq!(
            result.usage,
            Some(ImageUsage {
                prompt_tokens: Some(7),
                candidate_tokens: Some(3)
            })
        );
        let body = captured.lock().unwrap().clone().unwrap();
        assert_eq!(body["model"], "fixture-image");
        assert_eq!(body["project"], "synthetic-project");
        assert_eq!(
            body["request"]["generationConfig"]["responseModalities"],
            json!(["IMAGE"])
        );
        assert_eq!(
            body["request"]["generationConfig"]["imageConfig"]["aspectRatio"],
            "16:9"
        );
        let request_id = body["requestId"].as_str().unwrap();
        assert!(request_id.starts_with("agent-"));
        assert_eq!(request_id.rsplit('-').next().unwrap().len(), 9);
        assert!(body["request"].get("safetySettings").is_none());
        server.abort();
    }

    #[tokio::test]
    async fn caller_cancellation_and_expired_deadline_do_not_dispatch() {
        use crate::antigravity_auth::AntigravityAuth;
        use axum::{Json, Router, routing::post};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_route = calls.clone();
        let app = Router::new().route(
            "/v1internal:fetchAvailableModels",
            post(move || {
                let calls = calls_route.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"imageGenerationModelIds":["fixture-image"]}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let auth = AntigravityAuth::synthetic(dir.path().join("state"), origin).await;
        let generation = auth.generation().await.unwrap();
        let provider = AntigravityImages::new(auth);
        let request = ImageRequest::from_generation_json(
            &json!({"model":"fixture-image","prompt":"fixture"}),
        )
        .unwrap();
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert_eq!(
            provider
                .generate(
                    request.clone(),
                    &generation,
                    Instant::now() + Duration::from_secs(2),
                    cancelled
                )
                .await
                .unwrap_err()
                .code,
            "request_cancelled"
        );
        assert_eq!(
            provider
                .generate(
                    request,
                    &generation,
                    Instant::now() - Duration::from_secs(1),
                    CancellationToken::new()
                )
                .await
                .unwrap_err()
                .code,
            "provider_timeout"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        server.abort();
    }
}
