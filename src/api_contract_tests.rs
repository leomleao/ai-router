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

async fn exchange(
    harness: &Harness,
    path: &str,
    body: Value,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let response = harness
        .send(Method::POST, path, Some(body), Some("test-key"))
        .await;
    let status = response.status();
    let headers = response.headers().clone();
    let wire = String::from_utf8(read_bytes(response).await).unwrap();
    (status, headers, wire)
}
fn header<'a>(headers: &'a axum::http::HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).map(|value| value.to_str().unwrap())
}

// Supabase Studio's simple endpoints (title, SQL edit, onboarding, policy)
// send effort "none" with encrypted-reasoning include and store=false.
#[tokio::test]
async fn studio_effort_none_reaches_openai_and_is_disclosed_as_ignored_on_antigravity() {
    let harness = DualHarness::new().await;
    let tool = json!({"type":"function","name":"getSchemaDefinitions","description":"Get schema",
        "parameters":{"$schema":"http://json-schema.org/draft-07/schema#","type":"object",
            "properties":{"schemas":{"type":"array","items":{"type":"string"}}},
            "required":["schemas"],"additionalProperties":false}});
    for model in ["synthetic-openai", MODEL, "claude-sonnet-5-5-high"] {
        for streaming in [false, true] {
            let body = json!({"model":model,"input":[{"role":"developer","content":"You are a SQL editor assistant."},
                {"role":"user","content":[{"type":"input_text","text":"rewrite select 1"}]}],
                "store":false,"include":["reasoning.encrypted_content"],"reasoning":{"effort":"none"},
                "tools":[tool.clone()],"tool_choice":"auto","stream":streaming});
            let (status, headers, wire) = exchange(&harness, "/v1/responses", body).await;
            assert_eq!(status, StatusCode::OK, "{model}: {wire}");
            if model == "synthetic-openai" {
                // OpenAI decides whether the model supports "none".
                assert!(header(&headers, "x-ai-router-reasoning-effort").is_none());
                let upstream = harness.captured.lock().await.last().unwrap().clone();
                assert_eq!(upstream["reasoning"], json!({"effort":"none"}));
            } else {
                // The exact Antigravity model ID decides; no conflict error.
                assert_eq!(
                    header(&headers, "x-ai-router-reasoning-effort"),
                    Some("ignored")
                );
            }
        }
    }
    // Chat sends reasoning_effort; minimal follows the same Antigravity rule.
    for effort in ["none", "minimal"] {
        let (status, headers, wire) = exchange(
            &harness,
            "/v1/chat/completions",
            json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"reasoning_effort":effort}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{wire}");
        assert_eq!(
            header(&headers, "x-ai-router-reasoning-effort"),
            Some("ignored")
        );
    }
    // The fixture catalogue lists low/medium/high, so minimal becomes low.
    let (status, headers, wire) = exchange(
        &harness,
        "/v1/chat/completions",
        json!({"model":"synthetic-openai","messages":[{"role":"user","content":"hello"}],"reasoning_effort":"minimal"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{wire}");
    assert_eq!(
        header(&headers, "x-ai-router-reasoning-effort"),
        Some("mapped=low")
    );
    let upstream = harness.captured.lock().await.last().unwrap().clone();
    assert_eq!(upstream["reasoning"], json!({"effort":"low"}));
    // A real tier that differs from the model ID still fails explicitly.
    let (status, error) = harness
        .post(
            "/v1/responses",
            json!({"model":MODEL,"input":"hello","reasoning":{"effort":"low"}}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        error["error"]["message"],
        "reasoning_effort conflicts with the exact model ID"
    );
    // Studio title-v2 passes validation and reaches the provider. The fixture's
    // fixed text does not match the title schema, which is deterministic.
    let before = harness.captured.lock().await.len();
    let (status, headers, wire) = exchange(&harness, "/v1/responses", json!({"model":"synthetic-openai",
        "input":[{"role":"user","content":[{"type":"input_text","text":"schema-fixture Generate a title for: select 1"}]}],
        "text":{"format":{"type":"json_schema","strict":true,"name":"response","schema":{
            "$schema":"http://json-schema.org/draft-07/schema#","type":"object",
            "properties":{"title":{"type":"string"},"description":{"type":"string"}},
            "required":["title","description"],"additionalProperties":false}}},
        "store":false,"include":["reasoning.encrypted_content"],"reasoning":{"effort":"none"},"temperature":0})).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{wire}");
    assert!(wire.contains("invalid_structured_output"));
    assert_eq!(header(&headers, "x-should-retry"), Some("false"));
    assert_eq!(harness.captured.lock().await.len(), before + 1);
    let (_, capabilities) = harness
        .json(Method::GET, "/v1/capabilities", None, Some("test-key"))
        .await;
    let effort = &capabilities["profiles"]["model"]["reasoning_effort"];
    assert_eq!(effort["agy"]["none"], "ignored");
    assert_eq!(effort["openai"]["none"], "forwarded");
}

#[tokio::test]
async fn sampling_hints_and_no_op_fields_are_accepted_ignored_and_disclosed() {
    let harness = DualHarness::new().await;
    for model in ["synthetic-openai", MODEL] {
        for path in ["/v1/chat/completions", "/v1/responses"] {
            let responses = path == "/v1/responses";
            for streaming in [false, true] {
                let mut body = if responses {
                    json!({"model":model,"input":"hello","truncation":"auto"})
                } else {
                    json!({"model":model,"messages":[{"role":"user","content":"hello"}],
                        "seed":42,"frequency_penalty":0.5,"presence_penalty":-0.5})
                };
                for (field, value) in [
                    ("temperature", json!(0.7)),
                    ("top_p", json!(1)),
                    ("metadata", json!({"workflow":"synthetic"})),
                    ("user", json!("user-123")),
                    ("safety_identifier", json!("sid")),
                    ("service_tier", json!("auto")),
                    ("stream", json!(streaming)),
                ] {
                    body[field] = value;
                }
                if streaming && responses {
                    body["stream_options"] = json!({"include_obfuscation":false});
                }
                let (status, headers, wire) = exchange(&harness, path, body).await;
                assert_eq!(status, StatusCode::OK, "{wire}");
                assert_eq!(
                    header(&headers, "x-ai-router-sampling-controls"),
                    Some("ignored")
                );
                assert_eq!(
                    header(&headers, "x-ai-router-ignored-fields"),
                    Some(if responses {
                        "metadata, safety_identifier, service_tier, truncation, user"
                    } else {
                        "metadata, safety_identifier, service_tier, user"
                    })
                );
                if model == "synthetic-openai" {
                    let upstream = harness.captured.lock().await.last().unwrap().clone();
                    for field in [
                        "temperature",
                        "top_p",
                        "seed",
                        "metadata",
                        "user",
                        "safety_identifier",
                        "service_tier",
                        "truncation",
                    ] {
                        assert!(upstream.get(field).is_none(), "{field} reached OpenAI");
                    }
                }
                if responses {
                    let object = if streaming {
                        let frames = frames(wire.as_bytes());
                        let created = event_json(&frames[0]);
                        assert_eq!(
                            created["response"]["metadata"],
                            json!({"workflow":"synthetic"})
                        );
                        event_json(
                            frames
                                .iter()
                                .find(|f| f.event == "response.completed")
                                .unwrap(),
                        )["response"]
                            .clone()
                    } else {
                        serde_json::from_str(&wire).unwrap()
                    };
                    assert_eq!(object["metadata"], json!({"workflow":"synthetic"}));
                }
            }
        }
    }
    // Without hints nothing is disclosed, and Responses echo empty metadata.
    let (status, headers, wire) = exchange(
        &harness,
        "/v1/responses",
        json!({"model":MODEL,"input":"hello"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(header(&headers, "x-ai-router-sampling-controls").is_none());
    assert!(header(&headers, "x-ai-router-ignored-fields").is_none());
    assert_eq!(
        serde_json::from_str::<Value>(&wire).unwrap()["metadata"],
        json!({})
    );
    // Out-of-range hints and active controls are explicit, non-retryable 400s.
    for (field, value, code) in [
        ("temperature", json!(2.5), "invalid_request"),
        ("top_p", json!(-1), "invalid_request"),
        ("seed", json!("42"), "invalid_request"),
        ("service_tier", json!("priority"), "unsupported_parameter"),
        ("n", json!(2), "unsupported_parameter"),
        ("logit_bias", json!({"1":1}), "unsupported_parameter"),
        ("stop", json!(["END"]), "unsupported_parameter"),
    ] {
        let mut body = json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}]});
        body[field] = value;
        let (status, headers, wire) = exchange(&harness, "/v1/chat/completions", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{field}");
        let error: Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(error["error"]["code"], code);
        assert_eq!(error["error"]["type"], "invalid_request_error");
        assert_eq!(header(&headers, "x-should-retry"), Some("false"));
        assert!(header(&headers, "retry-after").is_none());
    }
    let (_, capabilities) = harness
        .json(Method::GET, "/v1/capabilities", None, Some("test-key"))
        .await;
    let profile = &capabilities["profiles"]["model"];
    assert_eq!(profile["sampling_controls"]["mode"], "ignored");
    assert_eq!(
        profile["sampling_controls"]["accepted"],
        json!([
            "temperature",
            "top_p",
            "seed",
            "frequency_penalty",
            "presence_penalty"
        ])
    );
    assert_eq!(profile["ignored_fields"]["truncation"], "never");
}

#[tokio::test]
async fn replayed_history_extras_are_tolerated_and_disclosed() {
    let harness = DualHarness::new().await;
    for model in ["synthetic-openai", MODEL] {
        // LangChain JS replays reasoning without encrypted state, and a refusal part.
        let (status, headers, wire) = exchange(&harness, "/v1/responses", json!({"model":model,"input":[
            {"type":"message","role":"user","content":"hello"},
            {"type":"reasoning","id":"rs_abc","summary":[]},
            {"type":"message","role":"assistant","id":"msg_abc","content":[{"type":"refusal","refusal":"I cannot help"}]},
            {"type":"message","role":"user","content":"again"}]})).await;
        assert_eq!(status, StatusCode::OK, "{wire}");
        assert_eq!(
            header(&headers, "x-ai-router-reasoning-replay"),
            Some("dropped")
        );
        if model == "synthetic-openai" {
            let upstream = harness.captured.lock().await.last().unwrap().clone();
            let input = upstream["input"].as_array().unwrap();
            assert!(!input.iter().any(|item| item["type"] == "reasoning"));
            assert!(
                input
                    .iter()
                    .any(|item| item["role"] == "assistant" && item["content"] == "I cannot help")
            );
        }
        // Python clients replay model_dump() of real completions.
        let (status, headers, wire) = exchange(&harness, "/v1/chat/completions", json!({"model":model,"messages":[
            {"role":"user","content":[{"type":"text","text":"hello","cache_control":{"type":"ephemeral"}}]},
            {"role":"assistant","content":"hi","refusal":null,"annotations":[],"reasoning_content":"elsewhere"},
            {"role":"user","content":"again"}]})).await;
        assert_eq!(status, StatusCode::OK, "{wire}");
        assert_eq!(
            header(&headers, "x-ai-router-ignored-fields"),
            Some("cache_control, reasoning_content")
        );
        assert!(header(&headers, "x-ai-router-reasoning-replay").is_none());
    }
}

#[tokio::test]
async fn retry_hints_separate_deterministic_transient_and_provider_auth_failures() {
    // Overload is transient: retry-after, also while the provider is latched.
    let harness = DualHarness::new().await;
    let overload = json!({"model":"synthetic-openai","input":"overload-fixture"});
    for _ in 0..2 {
        let (status, headers, wire) = exchange(&harness, "/v1/responses", overload.clone()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(wire.contains("server_is_overloaded"));
        assert_eq!(header(&headers, "retry-after"), Some("30"));
        assert!(header(&headers, "x-should-retry").is_none());
    }
    // The latched retry is answered locally, without another upstream call.
    assert_eq!(harness.captured.lock().await.len(), 1);

    // A plan usage limit is durable: never retried, even though it is a 429.
    let harness = DualHarness::new().await;
    let (status, headers, _) = exchange(
        &harness,
        "/v1/responses",
        json!({"model":"synthetic-openai","input":"quota-fixture"}),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(header(&headers, "x-should-retry"), Some("false"));
    assert!(header(&headers, "retry-after").is_none());

    // Upstream authorization failure is the provider, not the router key.
    let harness = DualHarness::new().await;
    let (status, _, wire) = exchange(
        &harness,
        "/v1/responses",
        json!({"model":"synthetic-openai","input":"upstream-auth-fixture"}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{wire}");
    let error: Value = serde_json::from_str(&wire).unwrap();
    assert_eq!(error["error"]["code"], "invalid_token");
    assert_ne!(error["error"]["type"], "authentication_error");
    assert!(!wire.contains("synthetic-provider-private-detail"));

    // Output that fails the requested schema fails the same way again.
    let mut body = json!({"model":MODEL,"input":"direct-invalid-schema-fixture"});
    body["text"] = json!({"format":{"type":"json_schema","name":"x","schema":{"type":"object"}}});
    let (status, headers, _) = exchange(&harness, "/v1/responses", body).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(header(&headers, "x-should-retry"), Some("false"));
    // Validation errors are deterministic too.
    let (status, headers, _) = exchange(
        &harness,
        "/v1/responses",
        json!({"model":MODEL,"input":"hello","unknown":true}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(header(&headers, "x-should-retry"), Some("false"));
    // Successful responses carry no retry guidance.
    let (status, headers, _) = exchange(
        &harness,
        "/v1/responses",
        json!({"model":MODEL,"input":"hello"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        header(&headers, "x-should-retry").is_none() && header(&headers, "retry-after").is_none()
    );
}
