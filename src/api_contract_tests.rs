// Shared HTTP regressions retained from archive/agy-cli-2026-10-20.
// Synthetic direct transport replaces process fixtures; public API invariants remain.
use super::*;
use axum::http::Method;
use http_body_util::BodyExt;
const MODEL: &str = "gemini-4-argon-medium";
type Harness = DualHarness;
async fn completed_response(harness: &Harness, body: Value) -> Value {
    let streaming = body["stream"] == true;
    let response = harness
        .send(Method::POST, "/v1/responses", Some(body), Some("test-key"))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    if !streaming {
        return read_json(response).await;
    }
    let frames = frames(&read_bytes(response).await);
    event_json(
        frames
            .iter()
            .find(|f| f.event == "response.completed")
            .unwrap(),
    )["response"]
        .clone()
}

#[tokio::test]
async fn response_item_references_replay_exact_history_and_enforce_storage_and_ownership() {
    let harness = DualHarness::new().await;
    for streaming in [false, true] {
        for store in [
            None,
            Some(json!(null)),
            Some(json!(true)),
            Some(json!(false)),
        ] {
            let mut body = json!({"model":MODEL,"input":"hello","stream":streaming});
            if let Some(store) = &store {
                body["store"] = store.clone();
            }
            let first = completed_response(&harness, body).await;
            let stored = store != Some(json!(false));
            assert_eq!(first["store"], stored);
            let item = &first["output"][0];
            let history = json!([{"role":"user","content":"hello"},
                {"type":"item_reference","id":item["id"]},
                {"role":"user","content":"__fake_referenced_history__"}]);
            let next = json!({"model":MODEL,"input":history,"stream":streaming});
            if stored {
                let second = completed_response(&harness, next.clone()).await;
                assert_eq!(
                    second["output"][0]["content"][0]["text"],
                    "Hello from fixture"
                );
            } else {
                let (status, error) = harness.post("/v1/responses", next.clone()).await;
                assert_eq!(status, StatusCode::BAD_REQUEST);
                assert_eq!(error["error"]["code"], "item_reference_not_found");
            }
            // A different valid key cannot discover whether this ID exists.
            let (status, foreign) = harness
                .json(
                    Method::POST,
                    "/v1/responses",
                    Some(next),
                    Some("other-test-key"),
                )
                .await;
            let (_, unknown) = harness
                .post(
                    "/v1/responses",
                    json!({"model":MODEL,"input":[{"type":"item_reference","id":"unknown"}]}),
                )
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(foreign["error"], unknown["error"]);
        }
    }
    for store in [json!(1), json!("false"), json!({})] {
        let (status, error) = harness
            .post(
                "/v1/responses",
                json!({"model":MODEL,"input":"hello","store":store}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error["error"]["code"], "invalid_request");
    }
}

#[tokio::test]
async fn cache_hints_work_for_both_apis_and_streams_with_explicit_disclosure() {
    let harness = DualHarness::new().await;
    for path in ["/v1/chat/completions", "/v1/responses"] {
        for streaming in [false, true] {
            for hints in [
                json!({}),
                json!({"prompt_cache_key":null,"prompt_cache_retention":null}),
                json!({"prompt_cache_key":"synthetic-cache-key"}),
                json!({"prompt_cache_retention":"in_memory"}),
                json!({"prompt_cache_key":"synthetic-cache-key","prompt_cache_retention":"24h"}),
            ] {
                let mut body = if path.ends_with("completions") {
                    json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}]})
                } else {
                    json!({"model":MODEL,"input":"hello"})
                };
                let has_hints = hints
                    .as_object()
                    .unwrap()
                    .values()
                    .any(|value| !value.is_null());
                body.as_object_mut()
                    .unwrap()
                    .extend(hints.as_object().unwrap().clone());
                body["stream"] = json!(streaming);
                let response = harness
                    .send(Method::POST, path, Some(body), Some("test-key"))
                    .await;
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(
                    response
                        .headers()
                        .get("x-ai-router-prompt-cache-controls")
                        .map(|v| v.to_str().unwrap()),
                    has_hints.then_some("ignored")
                );
                let bytes = read_bytes(response).await;
                assert!(
                    String::from_utf8_lossy(&bytes).contains("Hello from fixture") || streaming
                );
                if streaming {
                    let frames = frames(&bytes);
                    assert!(
                        frames
                            .iter()
                            .any(|frame| frame.event == "response.completed"
                                || frame.data == "[DONE]")
                    );
                }
            }
        }
        for (field, value) in [
            ("prompt_cache_key", json!(true)),
            ("prompt_cache_retention", json!("forever")),
        ] {
            let mut body = if path.ends_with("completions") {
                json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}]})
            } else {
                json!({"model":MODEL,"input":"hello"})
            };
            body[field] = value;
            let (status, error) = harness.post(path, body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(error["error"]["code"], "unsupported_parameter");
        }
    }
    let (status, capabilities) = harness
        .json(Method::GET, "/v1/capabilities", None, Some("test-key"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        capabilities["profiles"]["model"]["prompt_cache_controls"]["mode"],
        "ignored"
    );
    assert_eq!(
        capabilities["profiles"]["model"]["prompt_cache_controls"]["retention_guarantee"],
        false
    );
}

#[tokio::test]
async fn monitor_captures_actual_call_json_and_streamed_final_output_without_persisting_bodies() {
    let harness = DualHarness::new().await;
    for streaming in [false, true] {
        for path in ["/v1/chat/completions", "/v1/responses"] {
            let body = if path.ends_with("completions") {
                json!({"model":MODEL,"messages":[{"role":"user","content":"Synthetic monitor input"}],"stream":streaming})
            } else {
                json!({"model":MODEL,"input":"Synthetic monitor input","stream":streaming})
            };
            let response = harness
                .send(Method::POST, path, Some(body.clone()), Some("test-key"))
                .await;
            assert_eq!(response.status(), StatusCode::OK);
            let id = response.headers()["x-request-id"]
                .to_str()
                .unwrap()
                .to_owned();
            let wire = response.into_body().collect().await.unwrap().to_bytes();
            assert!(harness.state.drain(Duration::from_secs(5)).await);
            let detail = harness.state.telemetry.request_detail(&id).await.unwrap();
            assert_eq!(detail.endpoint, path);
            assert_eq!(
                serde_json::from_str::<Value>(&detail.request.json).unwrap(),
                body
            );
            let output = detail.response.unwrap();
            assert!(!output.truncated && !output.json.contains("test-key"));
            let captured: Value = serde_json::from_str(&output.json).unwrap();
            assert_eq!(captured["model"], MODEL);
            if !streaming {
                assert_eq!(captured, serde_json::from_slice::<Value>(&wire).unwrap());
            } else {
                assert!(captured.get("choices").is_some() || captured.get("output").is_some());
            }
        }
    }
    harness.state.telemetry.flush().await;
    let metadata =
        std::fs::read_to_string(harness.state.config.telemetry_dir.join("telemetry.json")).unwrap();
    assert!(!metadata.contains("Synthetic monitor input"));
    let response = harness
        .send(
            Method::POST,
            "/v1/responses",
            Some(json!({"model":MODEL,"input":"bad","unknown":true})),
            Some("test-key"),
        )
        .await;
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let detail = harness.state.telemetry.request_detail(&id).await.unwrap();
    let wire = response.into_body().collect().await.unwrap().to_bytes();
    let captured: Value = serde_json::from_str(&detail.response.unwrap().json).unwrap();
    assert_eq!(captured, serde_json::from_slice::<Value>(&wire).unwrap());
    assert_eq!(captured["error"]["code"], "unsupported_parameter");
}

async fn read_bytes(response: Response) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(10), response.into_body().collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes()
        .to_vec()
}
async fn read_json(response: Response) -> Value {
    serde_json::from_slice(&read_bytes(response).await).unwrap()
}

#[derive(Debug)]
struct SseFrame {
    event: String,
    id: Option<String>,
    data: String,
}
fn frames(bytes: &[u8]) -> Vec<SseFrame> {
    std::str::from_utf8(bytes)
        .unwrap()
        .replace("\r\n", "\n")
        .split("\n\n")
        .filter_map(|block| {
            let mut frame = SseFrame {
                event: String::new(),
                id: None,
                data: String::new(),
            };
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("event:") {
                    frame.event = value.trim_start().to_owned();
                } else if let Some(value) = line.strip_prefix("id:") {
                    frame.id = Some(value.trim_start().to_owned());
                } else if let Some(value) = line.strip_prefix("data:") {
                    if !frame.data.is_empty() {
                        frame.data.push('\n');
                    }
                    frame.data.push_str(value.trim_start());
                }
            }
            (!frame.data.is_empty()).then_some(frame)
        })
        .collect()
}
fn event_json(frame: &SseFrame) -> Value {
    serde_json::from_str(&frame.data).unwrap()
}

// Supabase Studio's assistant uses @ai-sdk/openai 4.x Responses with
// store=false and a reasoning effort, so the SDK adds encrypted reasoning
// include and a detailed summary request to every streamed tool turn.
#[tokio::test]
async fn studio_store_false_reasoning_tool_turns_round_trip_on_both_providers() {
    let harness = DualHarness::new().await;
    for model in ["synthetic-openai", MODEL] {
        for streaming in [false, true] {
            let mut tool = lookup_tool(true);
            tool["strict"] = json!(false);
            let mut input = vec![
                json!({"role":"developer","content":"Synthetic Studio system prompt"}),
                json!({"role":"user","content":[{"type":"input_text","text":"tools-fixture"}]}),
            ];
            let body = |input: &Vec<Value>| {
                json!({"model":model,"input":input,"store":false,
                    "include":["reasoning.encrypted_content"],
                    "reasoning":{"effort":"medium","summary":"detailed"},
                    "tools":[tool.clone()],"tool_choice":"auto","stream":streaming})
            };
            let response = harness
                .send(
                    Method::POST,
                    "/v1/responses",
                    Some(body(&input)),
                    Some("test-key"),
                )
                .await;
            assert_eq!(response.status(), StatusCode::OK);
            let header = |name: &str| response.headers()[name].to_str().unwrap().to_owned();
            assert_eq!(header("x-ai-router-reasoning-summary"), "unavailable");
            assert_eq!(header("x-ai-router-item-storage"), "disabled");
            let bytes = read_bytes(response).await;
            let first = if streaming {
                event_json(
                    frames(&bytes)
                        .iter()
                        .find(|f| f.event == "response.completed")
                        .unwrap(),
                )["response"]
                    .clone()
            } else {
                serde_json::from_slice(&bytes).unwrap()
            };
            let output = first["output"].as_array().unwrap();
            let calls: Vec<_> = output
                .iter()
                .filter(|item| item["type"] == "function_call")
                .collect();
            assert_eq!(calls.len(), 2);
            // Encrypted reasoning is returned only where the provider has it;
            // summaries are never fabricated.
            let reasoning: Vec<_> = output
                .iter()
                .filter(|item| item["type"] == "reasoning")
                .collect();
            if model == MODEL {
                assert!(reasoning.is_empty());
            } else {
                assert_eq!(reasoning.len(), 1);
                assert_eq!(
                    reasoning[0]["encrypted_content"],
                    "synthetic-encrypted-reasoning"
                );
                assert_eq!(reasoning[0]["summary"], json!([]));
                let upstream = harness.captured.lock().await.last().unwrap().clone();
                assert_eq!(upstream["include"], json!(["reasoning.encrypted_content"]));
                assert_eq!(upstream["reasoning"], json!({"effort":"medium"}));
            }
            // store=false replays full items exactly as the SDK serializes them.
            for item in &reasoning {
                input.push(json!({"type":"reasoning","id":item["id"],
                    "encrypted_content":item["encrypted_content"],"summary":[]}));
            }
            for call in &calls {
                input.push(
                    json!({"type":"function_call","id":call["id"],"call_id":call["call_id"],
                    "name":call["name"],"arguments":call["arguments"]}),
                );
            }
            for call in &calls {
                input.push(
                    json!({"type":"function_call_output","call_id":call["call_id"],
                    "output":"{\"rows\":[]}"}),
                );
            }
            let response = harness
                .send(
                    Method::POST,
                    "/v1/responses",
                    Some(body(&input)),
                    Some("test-key"),
                )
                .await;
            assert_eq!(response.status(), StatusCode::OK);
            let wire = String::from_utf8(read_bytes(response).await).unwrap();
            assert!(wire.contains("tool results accepted"), "{wire}");
            if model != MODEL {
                let upstream = harness.captured.lock().await.last().unwrap().clone();
                assert!(
                    upstream["input"].as_array().unwrap().iter().any(|item| {
                        item["encrypted_content"] == "synthetic-encrypted-reasoning"
                    })
                );
            }
        }
    }
    for (body, field) in [
        (
            json!({"model":"synthetic-openai","input":"hello","include":["file_search_call.results"]}),
            "include",
        ),
        (
            json!({"model":"synthetic-openai","input":"hello","reasoning":{"effort":"medium","summary":"verbose"}}),
            "reasoning.summary",
        ),
        (
            json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],
                "include":["reasoning.encrypted_content"]}),
            "include",
        ),
    ] {
        let path = if body.get("messages").is_some() {
            "/v1/chat/completions"
        } else {
            "/v1/responses"
        };
        let (status, error) = harness.post(path, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error["error"]["code"], "unsupported_parameter");
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with(field)
        );
    }
    // Without a summary request, no disclosure header is added.
    let response = harness
        .send(
            Method::POST,
            "/v1/responses",
            Some(json!({"model":MODEL,"input":"hello","reasoning":{"effort":"medium"}})),
            Some("test-key"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get("x-ai-router-reasoning-summary")
            .is_none()
    );
    let (_, capabilities) = harness
        .json(Method::GET, "/v1/capabilities", None, Some("test-key"))
        .await;
    let profile = &capabilities["profiles"]["model"];
    assert_eq!(
        profile["responses_include"],
        json!(["reasoning.encrypted_content"])
    );
    assert_eq!(profile["reasoning_summary"]["mode"], "unavailable");
}
