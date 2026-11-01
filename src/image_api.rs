//! Image routes share the model gateway's protected admission and lifecycle.
use super::*;
use crate::{
    antigravity_images::AntigravityImages,
    images::{ImageRequest, ReferenceImage},
};
use axum::extract::Multipart;

pub(super) async fn generations(
    State(state): State<Arc<AppState>>,
    Extension(key): Extension<KeyConfig>,
    Extension(context): Extension<Arc<RequestContext>>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = body.map_err(|e| {
        error(
            e.status().as_u16(),
            "invalid_request",
            "Expected a bounded image JSON request",
        )
    })?;
    state
        .telemetry
        .capture_request(&context.id, "/v1/images/generations", &body)
        .await;
    let request = ImageRequest::from_generation_json(&body)?;
    execute(state, key, context, request, "images_generations").await
}

pub(super) async fn edits(
    State(state): State<Arc<AppState>>,
    Extension(key): Extension<KeyConfig>,
    Extension(context): Extension<Arc<RequestContext>>,
    body: Result<Multipart, axum::extract::multipart::MultipartRejection>,
) -> Result<Response, ApiError> {
    let mut multipart =
        body.map_err(|_| ApiError::bad_request("Expected bounded multipart image fields"))?;
    let mut fields = serde_json::Map::new();
    let mut references = Vec::new();
    let mut total = 0usize;
    let mut count = 0;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| ApiError::bad_request("Invalid multipart image request"))?
    {
        count += 1;
        if count > 16 {
            return Err(ApiError::bad_request("Too many multipart fields"));
        }
        let name = field.name().unwrap_or("").to_owned();
        let mime = field.content_type().unwrap_or("").to_owned();
        let bytes = field
            .bytes()
            .await
            .map_err(|_| ApiError::bad_request("Invalid multipart field"))?;
        total = total.saturating_add(bytes.len());
        if total > state.config.max_body_bytes {
            return Err(error(
                413,
                "request_too_large",
                "Image request exceeds the configured body bound",
            ));
        }
        if matches!(name.as_str(), "image" | "image[]") {
            if references.len() >= crate::media::MAX_IMAGES {
                return Err(ApiError::bad_request("Too many reference images"));
            }
            // SDK file uploads may omit a MIME or use octet-stream. Sniff only
            // a type hint here; the shared container validator checks bytes.
            let mime = if mime.is_empty() || mime == "application/octet-stream" {
                if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
                    "image/png"
                } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
                    "image/jpeg"
                } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
                    "image/webp"
                } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
                    "image/gif"
                } else {
                    return Err(ApiError::unsupported("image media type"));
                }
            } else {
                mime.as_str()
            };
            references.push(ReferenceImage::from_validated_bytes(mime, bytes.to_vec())?);
        } else {
            if !matches!(
                name.as_str(),
                "model" | "prompt" | "n" | "size" | "response_format"
            ) {
                return Err(ApiError::unsupported("image edit field"));
            }
            if fields.contains_key(&name) {
                return Err(ApiError::bad_request("Duplicate image edit field"));
            }
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| ApiError::bad_request("Image options must be UTF-8 text"))?;
            let value = if name == "n" {
                json!(
                    text.parse::<u64>()
                        .map_err(|_| ApiError::bad_request("n must be an integer"))?
                )
            } else {
                json!(text)
            };
            fields.insert(name, value);
        }
    }
    let base = ImageRequest::from_generation_json(&Value::Object(fields))?;
    let mut request = ImageRequest::for_edit(base.model, base.prompt, references)?;
    request.count = base.count;
    request.aspect_ratio = base.aspect_ratio;
    request.image_size = base.image_size;
    // Multipart bytes and filenames never enter captures; only bounded shape does.
    state.telemetry.capture_request(&context.id, "/v1/images/edits", &json!({"model":request.model,"image_count":request.reference_images.len(),"prompt":request.prompt,"n":request.count})).await;
    execute(state, key, context, request, "images_edits").await
}

async fn execute(
    state: Arc<AppState>,
    key: KeyConfig,
    context: Arc<RequestContext>,
    request: ImageRequest,
    endpoint: &str,
) -> Result<Response, ApiError> {
    let deadline = context.started + Duration::from_secs(state.config.request_timeout_secs);
    tokio::time::timeout_at(
        deadline.into(),
        execute_inner(state, key, context, request, endpoint, deadline),
    )
    .await
    .map_err(|_| error(504, "provider_timeout", "Request deadline exceeded"))?
}

async fn execute_inner(
    state: Arc<AppState>,
    key: KeyConfig,
    context: Arc<RequestContext>,
    request: ImageRequest,
    endpoint: &str,
    deadline: Instant,
) -> Result<Response, ApiError> {
    *context.selection.lock().unwrap() = Some(("agy".into(), request.model.clone()));
    *context.shape.lock().unwrap() = Some(
        json!({"model":request.model,"image_count":request.reference_images.len(),"n":request.count,"input_bytes":request.reference_images.iter().map(|i|i.bytes.len()).sum::<usize>()}),
    );
    if !crate::config::valid_model_id(&request.model) {
        return Err(ApiError::bad_request("Invalid image model").with_param("model"));
    }
    if let Some(error) = state.antigravity.availability_error().await {
        return Err(error);
    }
    let generation = state.antigravity.generation().await?;
    let admission = tokio::time::timeout_at(deadline.into(), state.admit_provider(ProviderId::Agy))
        .await
        .map_err(|_| error(504, "provider_timeout", "Request deadline exceeded"))??;
    let cancel = state.shutdown.child_token();
    let finished = CancellationToken::new();
    let mut guard = CompletionGuard::new(
        state.clone(),
        key.id,
        request.model.clone(),
        endpoint,
        context.id.clone(),
        context.started,
        cancel.clone(),
        finished.clone(),
        admission,
    );
    let _finished = finished.drop_guard();
    context.recorded.store(true, Ordering::Relaxed);
    guard.log_context = json!({"origin":context.origin,"image_count":request.reference_images.len(),"requested_images":request.count});
    crate::log::event(
        "image_request",
        guard.log_fields(
            context
                .shape
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| json!({})),
        ),
    );
    let result = AntigravityImages::new(state.antigravity.auth.clone())
        .generate(request, &generation, deadline, cancel)
        .await;
    match result {
        Ok(result) => {
            let mut body = crate::images::image_response(&result.images);
            let usage = result.usage.as_ref().map(|usage| Usage {
                input_tokens: usage.prompt_tokens.unwrap_or(0),
                output_tokens: usage.candidate_tokens.unwrap_or(0),
                total_tokens: usage
                    .prompt_tokens
                    .unwrap_or(0)
                    .saturating_add(usage.candidate_tokens.unwrap_or(0)),
                ..Usage::default()
            });
            if let Some(usage) = &usage {
                body["usage"] = json!(usage);
            }
            body["ai_router_usage_partial"] = json!(true);
            guard.record.first_output_ms = Some(context.started.elapsed().as_millis() as u64);
            guard.observe(&RunEvent::Completed(RunResult {
                annotations: vec![],
                output_items: vec![],
                upstream_request_id: result.upstream_request_id.clone(),
                text: String::new(),
                structured_output: None,
                conversation_id: context.id.clone(),
                usage,
                usage_partial: true,
                tool_calls: vec![],
                status: "completed".into(),
            }));
            crate::log::event(
                "image_request_completed",
                guard.log_fields(json!({"image_count":result.image_count()})),
            );
            guard.capture_response(&body);
            Ok(Json(body).into_response())
        }
        Err(error) => {
            guard.observe(&RunEvent::Error(error.clone()));
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/T9sAAAAASUVORK5CYII=";

    async fn harness() -> (
        tempfile::TempDir,
        Arc<AppState>,
        Arc<tokio::sync::Mutex<Vec<Value>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let captured = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let capture = captured.clone();
        let mock = Router::new()
            .route("/v1internal:fetchAvailableModels", post(|| async { Json(json!({
                "models":{"gemini-3.8-flash-medium":{"displayName":"Fixture","supportsImages":true}},
                "imageGenerationModelIds":["fixture-image"]
            })) }))
            .route("/v1internal:streamGenerateContent",post(move |Json(body):Json<Value>| {
                let capture = capture.clone(); async move {
                    capture.lock().await.push(body.clone());
                    if body["request"]["contents"][0]["parts"].as_array().is_some_and(|parts| parts.iter().any(|part| part["text"] == "disconnect-fixture")) { tokio::time::sleep(Duration::from_secs(30)).await; }
                    let part = if body["request"]["generationConfig"]["responseModalities"] == json!(["IMAGE"]) {
                        json!({"inlineData":{"mimeType":"image/png","data":PNG}})
                    } else { json!({"text":"Fixture describes one image."}) };
                    ([("content-type","text/event-stream")],format!("data: {}\n\n",json!({"response":{"candidates":[{"index":0,"content":{"role":"model","parts":[part]},"finishReason":"STOP"}]}})))
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let root = tempfile::tempdir().unwrap();
        let mut config = Config::for_test(root.path());
        config.model_fallback = false;
        config.request_timeout_secs = 3;
        let telemetry = Arc::new(Telemetry::open(&config.telemetry_dir, 30, 100).unwrap());
        let config = Arc::new(config);
        let provider = Antigravity::synthetic(config.clone(), origin).await;
        let state = AppState::new(config, provider, telemetry);
        state.refresh_agy().await;
        (root, state, captured, server)
    }
    async fn send(
        state: Arc<AppState>,
        path: &str,
        content_type: &str,
        body: Vec<u8>,
        authorized: bool,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", content_type);
        if authorized {
            request = request.header("authorization", "Bearer test-key");
        }
        let response = router(state)
            .oneshot(request.body(axum::body::Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
    #[tokio::test]
    async fn image_routes_use_exact_discovery_auth_and_real_multipart_with_redacted_telemetry() {
        let (_root, state, captured, server) = harness().await;
        let body = json!({"model":"fixture-image","prompt":"private image generation prompt","response_format":"b64_json"});
        let (status, _) = send(
            state.clone(),
            "/v1/images/generations",
            "application/json",
            body.to_string().into_bytes(),
            false,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(captured.lock().await.is_empty());
        let (status, output) = send(
            state.clone(),
            "/v1/images/generations",
            "application/json",
            body.to_string().into_bytes(),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{output}");
        assert_eq!(output["data"][0]["b64_json"], PNG);
        let mut multipart=b"--fixture\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nfixture-image\r\n--fixture\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nEdit this\r\n--fixture\r\nContent-Disposition: form-data; name=\"image\"; filename=\"private-image.png\"\r\nContent-Type: image/png\r\n\r\n".to_vec();
        multipart.extend(
            base64::engine::general_purpose::STANDARD
                .decode(PNG)
                .unwrap(),
        );
        multipart.extend(b"\r\n--fixture--\r\n");
        let (status, output) = send(
            state.clone(),
            "/v1/images/edits",
            "multipart/form-data; boundary=fixture",
            multipart,
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{output}");
        assert_eq!(
            captured.lock().await[1]["request"]["contents"][0]["parts"][0]["inlineData"]["data"],
            PNG
        );
        let (status, _) = send(
            state.clone(),
            "/v1/images/generations",
            "application/json",
            json!({"model":"unknown-image","prompt":"x"})
                .to_string()
                .into_bytes(),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(captured.lock().await.len(), 2);
        state.stop();
        assert!(state.drain(Duration::from_secs(3)).await);
        state.telemetry.flush().await;
        let saved =
            std::fs::read_to_string(state.config.telemetry_dir.join("telemetry.json")).unwrap();
        assert!(!saved.contains(PNG));
        assert!(!saved.contains("private image generation prompt"));
        assert!(!saved.contains("private-image.png"));
        server.abort();
    }
    #[tokio::test]
    async fn media_model_capabilities_gate_paid_dispatch_and_image_ids_never_fall_back_to_chat() {
        let (_root, state, captured, server) = harness().await;
        let body = json!({"model":"gemini-3.8-flash-medium","messages":[{"role":"user","content":[{"type":"text","text":"What is this?"},{"type":"image_url","image_url":{"url":format!("data:image/png;base64,{PNG}")}}]}]});
        let (status, output) = send(
            state.clone(),
            "/v1/chat/completions",
            "application/json",
            body.to_string().into_bytes(),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{output}");
        assert_eq!(captured.lock().await.len(), 1);
        let mut body = body;
        body["messages"][0]["content"][1]["image_url"]["url"] =
            json!("https://example.test/private-image.png");
        let (status, output) = send(
            state.clone(),
            "/v1/chat/completions",
            "application/json",
            body.to_string().into_bytes(),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{output}");
        assert_eq!(captured.lock().await.len(), 1);
        body["model"] = json!("fixture-image");
        let (status, output) = send(
            state.clone(),
            "/v1/chat/completions",
            "application/json",
            body.to_string().into_bytes(),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{output}");
        assert_eq!(captured.lock().await.len(), 1);
        state.stop();
        server.abort();
    }
    #[tokio::test]
    async fn official_python_sdk_generates_edits_and_sends_vision_over_real_tcp() {
        let (_root, state, captured, upstream) = harness().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let app = router(state.clone());
        let gateway = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let code = r#"import base64,sys
from openai import OpenAI
client=OpenAI(base_url=sys.argv[1],api_key="test-key",max_retries=0,timeout=10)
png=sys.argv[2]
generated=client.images.generate(model="fixture-image",prompt="Fixture",response_format="b64_json")
assert generated.data[0].b64_json==png
edited=client.images.edit(model="fixture-image",prompt="Edit fixture",image=("fixture.png",base64.b64decode(png),"image/png"),response_format="b64_json")
assert edited.data[0].b64_json==png
for stream in [False,True]:
 result=client.chat.completions.create(model="gemini-3.8-flash-medium",messages=[{"role":"user","content":[{"type":"text","text":"Describe"},{"type":"image_url","image_url":{"url":"data:image/png;base64,"+png}}]}],stream=stream)
 if stream:
  assert "Fixture" in "".join((chunk.choices[0].delta.content or "") for chunk in result if chunk.choices)
 else:
  assert "Fixture" in result.choices[0].message.content
print("Official Python media/image SDK: 4 checks passed")
"#;
        let output = tokio::process::Command::new("python3")
            .args(["-c", code, &base, PNG])
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(captured.lock().await.len(), 4);
        gateway.abort();
        upstream.abort();
        state.stop();
    }
    #[tokio::test]
    async fn downstream_disconnect_drops_unary_image_handler_and_releases_admission() {
        use tokio::io::AsyncWriteExt;
        let (_root, state, captured, upstream) = harness().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(state.clone());
        let gateway = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let body = json!({"model":"fixture-image","prompt":"disconnect-fixture"}).to_string();
        let request = format!(
            "POST /v1/images/generations HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer test-key\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client.write_all(request.as_bytes()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while captured.lock().await.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        drop(client);
        tokio::time::timeout(Duration::from_secs(2), async {
            while state.active.load(Ordering::SeqCst) != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("disconnect must cancel the handler before its request deadline");
        assert_eq!(captured.lock().await.len(), 1);
        gateway.abort();
        upstream.abort();
        state.stop();
    }
}
