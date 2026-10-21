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
async fn studio_effort_none_follows_the_openai_catalogue_and_is_ignored_on_antigravity() {
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
                // The fixture catalogue lists low/medium/high without none,
                // so none becomes the lowest listed level, disclosed.
                assert_eq!(
                    header(&headers, "x-ai-router-reasoning-effort"),
                    Some("mapped=low")
                );
                let upstream = harness.captured.lock().await.last().unwrap().clone();
                assert_eq!(upstream["reasoning"], json!({"effort":"low"}));
            } else {
                // The exact Antigravity model ID decides; no conflict error.
                assert_eq!(
                    header(&headers, "x-ai-router-reasoning-effort"),
                    Some("ignored")
                );
            }
        }
    }
    // none is forwarded unchanged when the catalogue lists it, or lists no
    // levels for the model, so OpenAI decides.
    for (levels, minimal) in [
        (&["none", "low", "high"][..], "low"),
        (&["minimal", "low"][..], "minimal"),
        (&[][..], "minimal"),
    ] {
        harness
            .state
            .openai
            .set_reasoning_levels("synthetic-openai", levels)
            .await;
        for path in ["/v1/chat/completions", "/v1/responses"] {
            for (effort, sent) in [("none", None), ("minimal", Some(minimal))] {
                let body = if path.ends_with("responses") {
                    json!({"model":"synthetic-openai","input":"hello","reasoning":{"effort":effort}})
                } else {
                    json!({"model":"synthetic-openai","messages":[{"role":"user","content":"hello"}],"reasoning_effort":effort})
                };
                let (status, headers, wire) = exchange(&harness, path, body).await;
                assert_eq!(status, StatusCode::OK, "{wire}");
                let upstream = harness.captured.lock().await.last().unwrap().clone();
                if effort == "none" && levels.first() == Some(&"minimal") {
                    // Listed without none: the lowest listed level, minimal.
                    assert_eq!(
                        header(&headers, "x-ai-router-reasoning-effort"),
                        Some("mapped=minimal")
                    );
                    assert_eq!(upstream["reasoning"]["effort"], "minimal");
                    continue;
                }
                let applied = sent.unwrap_or(effort);
                assert_eq!(upstream["reasoning"]["effort"], applied, "{levels:?} {effort}");
                let disclosed = (applied != effort).then(|| format!("mapped={applied}"));
                assert_eq!(
                    header(&headers, "x-ai-router-reasoning-effort"),
                    disclosed.as_deref(),
                    "{levels:?} {effort}"
                );
            }
        }
    }
    harness
        .state
        .openai
        .set_reasoning_levels("synthetic-openai", &["low", "medium", "high"])
        .await;
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
    assert_eq!(effort["openai"]["none"], "lowest_listed_level_when_unlisted");
    assert_eq!(effort["openai"]["unlisted_catalogue"], "forwarded");
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

// Supabase Studio hard-codes OpenAI IDs that the live catalogue may not
// advertise. Unseen IDs are served by a configured exact target and disclosed.
async fn fallback_harness(adjust: impl FnOnce(&mut Config)) -> Harness {
    DualHarness::configured(|config| {
        config.fallback_openai_model = "synthetic-openai".into();
        config.fallback_gemini_model = MODEL.into();
        adjust(config);
    })
    .await
}
struct Served {
    status: StatusCode,
    fallback: Option<String>,
    effort: Option<String>,
    models: Vec<Value>,
    body: Value,
    request_id: String,
}
async fn serve(harness: &Harness, path: &str, model: &str, extra: Value) -> Served {
    let streaming = extra["stream"] == true;
    let mut body = if path.ends_with("completions") {
        json!({"model":model,"messages":[{"role":"user","content":"hello"}]})
    } else {
        json!({"model":model,"input":"hello"})
    };
    for (field, value) in extra.as_object().unwrap() {
        body[field] = value.clone();
    }
    let response = harness
        .send(Method::POST, path, Some(body), Some("test-key"))
        .await;
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .map(|value| value.to_str().unwrap().to_owned())
    };
    let (fallback, effort) = (
        header("x-ai-router-model-fallback"),
        header("x-ai-router-reasoning-effort"),
    );
    let request_id = header("x-request-id").unwrap();
    let status = response.status();
    let (body, models) = if streaming && status == StatusCode::OK {
        let events: Vec<Value> = frames(&read_bytes(response).await)
            .iter()
            .filter(|frame| frame.data != "[DONE]")
            .map(event_json)
            .collect();
        let models = events
            .iter()
            .filter_map(|event| event.get("model").or(event.pointer("/response/model")))
            .cloned()
            .collect();
        (Value::Null, models)
    } else {
        let body = read_json(response).await;
        let models = body.get("model").cloned().into_iter().collect();
        (body, models)
    };
    Served {
        status,
        fallback,
        effort,
        models,
        body,
        request_id,
    }
}

#[tokio::test]
async fn unsupported_model_ids_are_served_by_disclosed_fallback_targets() {
    let harness = fallback_harness(|_| {}).await;
    for path in ["/v1/chat/completions", "/v1/responses"] {
        for streaming in [false, true] {
            for (requested, target) in [
                ("gpt-5.4-nano", "synthetic-openai"),
                ("gpt-5.3-codex", "synthetic-openai"),
                ("claude-unknown", "synthetic-openai"),
                ("gemini-9-ultra", MODEL),
                ("GEMINI-9-ULTRA", MODEL),
            ] {
                let served = serve(&harness, path, requested, json!({"stream":streaming})).await;
                assert_eq!(served.status, StatusCode::OK, "{path} {requested}");
                assert_eq!(
                    served.fallback.as_deref(),
                    Some(format!("{requested} -> {target}").as_str())
                );
                assert!(!served.models.is_empty());
                assert!(served.models.iter().all(|model| model == target));
                assert!(harness.state.drain(Duration::from_secs(5)).await);
                let snapshot = harness.state.telemetry.snapshot().await;
                let record = snapshot
                    .records
                    .iter()
                    .find(|record| record.request_id == served.request_id)
                    .unwrap();
                assert_eq!(record.model, target);
                assert_eq!(record.requested_model.as_deref(), Some(requested));
            }
            // Exact catalogue IDs are never remapped or annotated.
            for exact in ["synthetic-openai", MODEL, "claude-sonnet-5-5-high"] {
                let served = serve(&harness, path, exact, json!({"stream":streaming})).await;
                assert_eq!(served.status, StatusCode::OK);
                assert!(served.fallback.is_none());
                assert!(served.models.iter().all(|model| model == exact));
            }
        }
    }
    // OpenAI receives the served ID; the client's effort is model-dependent there.
    let served = serve(
        &harness,
        "/v1/responses",
        "gpt-5.4-nano",
        json!({"reasoning":{"effort":"low"}}),
    )
    .await;
    assert_eq!(served.status, StatusCode::OK);
    assert!(served.effort.is_none());
    let upstream = harness.captured.lock().await.last().unwrap().clone();
    assert_eq!(upstream["model"], "synthetic-openai");
    assert_eq!(upstream["reasoning"]["effort"], "low");
    // An Antigravity target's ID fixes its tier, so a mapped request's effort
    // yields to it with disclosure; an exact ID's conflict is still rejected.
    for (path, effort) in [
        ("/v1/chat/completions", json!({"reasoning_effort":"high"})),
        ("/v1/responses", json!({"reasoning":{"effort":"high"}})),
    ] {
        let served = serve(&harness, path, "gemini-9-ultra", effort.clone()).await;
        assert_eq!(served.status, StatusCode::OK, "{}", served.body);
        assert_eq!(served.effort.as_deref(), Some("mapped=medium"));
        let served = serve(&harness, path, "gemini-9-ultra", json!({})).await;
        assert!(served.effort.is_none());
        let exact = serve(&harness, path, MODEL, effort).await;
        assert_eq!(exact.status, StatusCode::BAD_REQUEST);
        assert!(exact.fallback.is_none());
        assert!(
            exact.body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("conflicts with the exact model ID")
        );
    }
    // Invalid IDs (control characters, spaces, markup, over 128 bytes) and
    // non-chat model families are never served as paid chat.
    for junk in [
        format!("gpt\u{1}\r\n{}", "x".repeat(300)),
        "x".repeat(129),
        "gpt 4o mini".into(),
        "<b>gpt</b>".into(),
    ] {
        let served = serve(&harness, "/v1/responses", &junk, json!({})).await;
        assert_eq!(served.status, StatusCode::BAD_REQUEST);
        assert_eq!(served.body["error"]["code"], "model_not_found");
        assert!(served.fallback.is_none());
    }
    let upstream_calls = harness.captured.lock().await.len();
    for path in ["/v1/chat/completions", "/v1/responses"] {
        for non_chat in [
            "text-embedding-3-large",
            "dall-e-3",
            "whisper-1",
            "omni-moderation-latest",
            "gpt-4o-mini-transcribe",
            "gpt-realtime",
        ] {
            let served = serve(&harness, path, non_chat, json!({})).await;
            assert_eq!(served.status, StatusCode::BAD_REQUEST, "{non_chat}");
            assert_eq!(served.body["error"]["code"], "model_not_found");
            assert!(
                served.body["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("chat models only")
            );
            assert!(served.fallback.is_none());
        }
    }
    assert_eq!(harness.captured.lock().await.len(), upstream_calls);
    let (_, models) = harness
        .json(Method::GET, "/v1/models", None, Some("test-key"))
        .await;
    let ids: Vec<&str> = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 3);
    assert!(!ids.iter().any(|id| id.starts_with("gpt-5") || *id == "gemini-9-ultra"));
    let (_, capabilities) = harness
        .json(Method::GET, "/v1/capabilities", None, Some("test-key"))
        .await;
    let policy = &capabilities["profiles"]["model"]["model_fallback"];
    assert_eq!(policy["enabled"], true);
    assert_eq!(policy["default_target"], "synthetic-openai");
    assert_eq!(policy["gemini_prefix_target"], MODEL);
    assert_eq!(policy["listed_in_models"], false);
}

#[tokio::test]
async fn unavailable_fallback_target_crosses_over_or_keeps_model_not_found() {
    let harness = fallback_harness(|_| {}).await;
    // OpenAI catalogue outage: unseen IDs cross to the Gemini target, while the
    // remembered exact OpenAI ID keeps its own outage error.
    harness.model_mode.store(1, Ordering::SeqCst);
    harness.state.refresh_openai().await;
    for path in ["/v1/chat/completions", "/v1/responses"] {
        let served = serve(&harness, path, "gpt-5.4-nano", json!({})).await;
        assert_eq!(served.status, StatusCode::OK);
        assert_eq!(served.fallback.as_deref(), Some(format!("gpt-5.4-nano -> {MODEL}").as_str()));
        assert_eq!(served.models, vec![json!(MODEL)]);
        let exact = serve(&harness, path, "synthetic-openai", json!({})).await;
        assert_eq!(exact.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(exact.fallback.is_none());
    }
    // A Gemini target missing from the catalogue crosses to the OpenAI target.
    let harness = fallback_harness(|config| config.fallback_gemini_model = "gemini-absent".into()).await;
    let served = serve(&harness, "/v1/responses", "gemini-9-ultra", json!({})).await;
    assert_eq!(served.status, StatusCode::OK);
    assert_eq!(served.fallback.as_deref(), Some("gemini-9-ultra -> synthetic-openai"));
    // With neither target available, today's error is unchanged.
    let harness = DualHarness::new().await;
    for model in ["gpt-5.4-nano", "gemini-9-ultra"] {
        let served = serve(&harness, "/v1/chat/completions", model, json!({})).await;
        assert_eq!(served.status, StatusCode::BAD_REQUEST);
        assert_eq!(served.body["error"]["code"], "model_not_found");
        assert!(served.fallback.is_none());
    }
}

#[tokio::test]
async fn disabled_model_fallback_restores_strict_model_not_found() {
    let harness = fallback_harness(|config| config.model_fallback = false).await;
    for path in ["/v1/chat/completions", "/v1/responses"] {
        for model in ["gpt-5.4-nano", "gpt-5.3-codex", "gemini-9-ultra"] {
            let served = serve(&harness, path, model, json!({})).await;
            assert_eq!(served.status, StatusCode::BAD_REQUEST);
            assert_eq!(served.body["error"]["code"], "model_not_found");
            assert_eq!(served.body["error"]["message"], "Choose a model returned by /v1/models");
            assert!(served.fallback.is_none());
        }
        let served = serve(&harness, path, MODEL, json!({})).await;
        assert_eq!(served.status, StatusCode::OK);
    }
    let (_, capabilities) = harness
        .json(Method::GET, "/v1/capabilities", None, Some("test-key"))
        .await;
    assert_eq!(capabilities["profiles"]["model"]["model_fallback"]["enabled"], false);
}

// Exact forced-choice client bodies: Twenty's workspace-setup kickoff and
// n8n's "Force Tool Call" (Responses, "required"), and LangChain Python
// with_structured_output(method="function_calling") (Chat, named, with
// parallel_tool_calls=false). Fixture models call the first offered tool only
// when forcing reaches them, so each call proves the narrowed inventory too.
#[tokio::test]
async fn forced_tool_choice_client_bodies_call_the_selected_tool_or_fail_closed() {
    let harness = DualHarness::new().await;
    let find = json!({"type":"function","name":"find_records","description":"Find","parameters":{"type":"object",
        "properties":{"q":{"type":"string"}},"required":["q"],"additionalProperties":false},"strict":false});
    let joke = json!({"type":"function","function":{"name":"Joke","description":"Joke to tell user.","parameters":{"type":"object",
        "properties":{"setup":{"type":"string"},"punchline":{"type":"string"}},"required":["setup","punchline"]}}});
    let mut n8n_tool = lookup_tool(true);
    n8n_tool["strict"] = json!(false);
    let chat_calls = |streaming: bool, wire: &[u8]| -> Vec<Value> {
        if !streaming {
            let value: Value = serde_json::from_slice(wire).unwrap();
            return value["choices"][0]["message"]["tool_calls"]
                .as_array()
                .cloned()
                .unwrap_or_default();
        }
        frames(wire)
            .iter()
            .filter(|frame| frame.data != "[DONE]")
            .flat_map(|frame| {
                event_json(frame)["choices"][0]["delta"]["tool_calls"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .collect()
    };
    for model in ["synthetic-openai", MODEL] {
        for streaming in [false, true] {
            let twenty = |text: &str| {
                json!({"model":model,"input":[{"role":"developer","content":"You are Twenty's assistant"},
                    {"role":"user","content":[{"type":"input_text","text":text}]}],
                    "store":false,"prompt_cache_key":"thread-123","include":["reasoning.encrypted_content"],
                    "reasoning":{"effort":"medium","summary":"auto"},"tools":[find.clone()],"tool_choice":"required","stream":streaming})
            };
            let n8n = json!({"model":model,"input":[{"type":"message","role":"system","content":"You are helpful"},
                {"type":"message","role":"user","content":"hello"}],"stream":streaming,"text":{},
                "tools":[n8n_tool.clone()],"tool_choice":"required"});
            let allowed = json!({"model":model,"input":"hello","stream":streaming,"tools":[lookup_tool(true),find.clone()],
                "tool_choice":{"type":"allowed_tools","mode":"required","tools":[{"type":"function","name":"find_records"}]}});
            for (body, name) in [
                (twenty("find acme"), "find_records"),
                (n8n, "lookup"),
                (allowed, "find_records"),
            ] {
                let output = completed_response(&harness, body).await;
                let calls: Vec<_> = output["output"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|item| item["type"] == "function_call")
                    .collect();
                assert_eq!(calls.len(), 1, "{model} {output}");
                assert_eq!(calls[0]["name"], name);
                let arguments: Value =
                    serde_json::from_str(calls[0]["arguments"].as_str().unwrap()).unwrap();
                assert_eq!(arguments, json!({"q":"synthetic"}));
            }
            let langchain = |text: &str| {
                json!({"model":model,"messages":[{"role":"user","content":text}],"stream":streaming,
                    "parallel_tool_calls":false,"tools":[lookup_tool(false),joke.clone()],
                    "tool_choice":{"type":"function","function":{"name":"Joke"}}})
            };
            let response = harness
                .send(
                    Method::POST,
                    "/v1/chat/completions",
                    Some(langchain("Tell me a joke about cats")),
                    Some("test-key"),
                )
                .await;
            assert_eq!(response.status(), StatusCode::OK);
            let calls = chat_calls(streaming, &read_bytes(response).await);
            assert_eq!(calls.len(), 1, "{model}");
            assert_eq!(calls[0]["function"]["name"], "Joke");
            let arguments: Value =
                serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
            assert_eq!(
                arguments,
                json!({"setup":"synthetic","punchline":"synthetic"})
            );
            if model == "synthetic-openai" {
                let upstream = harness.captured.lock().await.last().cloned().unwrap();
                assert_eq!(upstream["tool_choice"], "required");
                assert_eq!(upstream["parallel_tool_calls"], false);
                assert_eq!(upstream["input"][0]["type"], "additional_tools");
                assert_eq!(upstream["input"][0]["tools"].as_array().unwrap().len(), 1);
                assert_eq!(upstream["input"][0]["tools"][0]["name"], "Joke");
            }

            // A model that answers in text instead is withheld, never
            // completed with a fabricated call.
            for (path, body) in [
                ("/v1/responses", twenty("find acme no-call-fixture")),
                ("/v1/chat/completions", langchain("no-call-fixture joke")),
            ] {
                let response = harness
                    .send(Method::POST, path, Some(body), Some("test-key"))
                    .await;
                let status = response.status();
                let retry = response.headers().get("x-should-retry").cloned();
                let wire = read_bytes(response).await;
                // Withheld text means no output has started, so a stream fails
                // before it commits, exactly like the nonstreaming request.
                assert_eq!(status, StatusCode::BAD_GATEWAY, "{model} {path} {streaming}");
                assert_eq!(retry.as_ref().map(|v| v.to_str().unwrap()), Some("false"));
                let error: Value = serde_json::from_slice(&wire).unwrap();
                assert_eq!(error["error"]["code"], "missing_required_tool_call");
                let wire = String::from_utf8(wire).unwrap();
                assert!(!wire.contains("synthetic answer") && !wire.contains("Hello from fixture"));
            }

            // allowed_tools keeps auto selection but removes the other tools:
            // the fixture's habitual lookup call is now outside the inventory.
            let (status, error) = harness
                .post(
                    "/v1/responses",
                    json!({"model":model,"input":"tools-fixture","stream":false,"tools":[lookup_tool(true),find.clone()],
                        "tool_choice":{"type":"allowed_tools","mode":"auto","tools":[{"type":"function","name":"find_records"}]}}),
                )
                .await;
            assert_eq!(status, StatusCode::BAD_GATEWAY);
            assert_eq!(error["error"]["code"], "invalid_provider_tool_call");
        }
        for (path, body) in [
            (
                "/v1/chat/completions",
                json!({"model":model,"messages":[{"role":"user","content":"hello"}],"tools":[lookup_tool(false)],
                    "tool_choice":{"type":"function","function":{"name":"Missing"}}}),
            ),
            (
                "/v1/responses",
                json!({"model":model,"input":"hello","tools":[lookup_tool(true)],"tool_choice":{"type":"function","name":"Missing"}}),
            ),
            (
                "/v1/responses",
                json!({"model":model,"input":"hello","tools":[lookup_tool(true)],
                    "tool_choice":{"type":"allowed_tools","mode":"auto","tools":[{"type":"function","name":"Missing"}]}}),
            ),
            (
                "/v1/responses",
                json!({"model":model,"input":"hello","tool_choice":"required"}),
            ),
        ] {
            let (status, error) = harness.post(path, body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(error["error"]["type"], "invalid_request_error");
            assert_eq!(error["error"]["code"], "invalid_request");
            assert_eq!(error["error"]["param"], "tool_choice");
        }
    }
    let (_, capabilities) = harness
        .json(Method::GET, "/v1/capabilities", None, Some("test-key"))
        .await;
    assert_eq!(
        capabilities["profiles"]["model"]["tool_choice"]["missing_required_call"],
        "error"
    );
    assert_eq!(
        capabilities["providers"]["agy"]["tool_choice_forcing"],
        "prompt_instruction"
    );
    assert_eq!(
        capabilities["providers"]["openai"]["tool_choice_forcing"],
        "upstream_required"
    );
    assert!(
        !capabilities["unsupported"]
            .as_array()
            .unwrap()
            .contains(&json!("forced_tool_choice"))
    );
}

// Integration of the compatibility changes: each request gets one effort
// decision, made against the served model, and one disclosure-header set that
// is the same whether it streams, fails before the stream commits, or fails.
fn studio_body(path: &str, model: &str, effort: &str, input: &str, streaming: bool) -> Value {
    let mut body = if path.ends_with("completions") {
        json!({"model":model,"messages":[{"role":"user","content":input}],"reasoning_effort":effort})
    } else {
        json!({"model":model,"input":input,"store":false,"include":["reasoning.encrypted_content"],
            "reasoning":{"effort":effort,"summary":"detailed"}})
    };
    body["temperature"] = json!(0);
    body["stream"] = json!(streaming);
    body
}
fn values<'a>(headers: &'a axum::http::HeaderMap, name: &str) -> Vec<&'a str> {
    headers
        .get_all(name)
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect()
}
const DISCLOSURES: &[&str] = &[
    "x-ai-router-model-fallback",
    "x-ai-router-reasoning-effort",
    "x-ai-router-sampling-controls",
    "x-ai-router-ignored-fields",
    "x-ai-router-reasoning-replay",
    "x-ai-router-reasoning-summary",
    "x-ai-router-prompt-cache-controls",
    "x-ai-router-token-budget-mode",
    "x-ai-router-item-storage",
];
fn disclosures(headers: &axum::http::HeaderMap) -> Vec<(&'static str, Vec<String>)> {
    DISCLOSURES
        .iter()
        .map(|name| {
            let found = values(headers, name).into_iter().map(str::to_owned);
            (*name, found.collect())
        })
        .collect()
}

#[tokio::test]
async fn studio_fallback_effort_none_follows_the_openai_catalogue_and_is_ignored_on_antigravity() {
    let harness = fallback_harness(|_| {}).await;
    for path in ["/v1/chat/completions", "/v1/responses"] {
        for streaming in [false, true] {
            let body = studio_body(path, "gpt-5.4-nano", "none", "title this chat", streaming);
            let (status, headers, wire) = exchange(&harness, path, body).await;
            assert_eq!(status, StatusCode::OK, "{path} {wire}");
            assert_eq!(
                values(&headers, "x-ai-router-model-fallback"),
                ["gpt-5.4-nano -> synthetic-openai"]
            );
            // The served model's catalogue lists no none, so the upstream
            // receives its lowest listed level instead of a likely 400.
            assert_eq!(values(&headers, "x-ai-router-reasoning-effort"), ["mapped=low"]);
            assert_eq!(values(&headers, "x-ai-router-sampling-controls"), ["ignored"]);
            let upstream = harness.captured.lock().await.last().unwrap().clone();
            assert_eq!(upstream["model"], "synthetic-openai");
            assert_eq!(upstream["reasoning"]["effort"], "low");
            assert!(upstream.get("temperature").is_none());
            // minimal is outside the fixture catalogue, so it maps to low.
            let body = studio_body(path, "gpt-5.4-nano", "minimal", "title", streaming);
            let (status, headers, _) = exchange(&harness, path, body).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(values(&headers, "x-ai-router-reasoning-effort"), ["mapped=low"]);
            let upstream = harness.captured.lock().await.last().unwrap().clone();
            assert_eq!(upstream["reasoning"]["effort"], "low");
        }
    }
    // Only Antigravity live: the same requests cross to the Gemini target.
    harness.model_mode.store(1, Ordering::SeqCst);
    harness.state.refresh_openai().await;
    for path in ["/v1/chat/completions", "/v1/responses"] {
        for streaming in [false, true] {
            for (effort, disclosed) in [
                ("none", Some("ignored")),
                ("minimal", Some("ignored")),
                ("high", Some("mapped=medium")),
                ("medium", None),
            ] {
                let body = studio_body(path, "gpt-5.4-nano", effort, "title", streaming);
                let (status, headers, wire) = exchange(&harness, path, body).await;
                assert_eq!(status, StatusCode::OK, "{path} {effort} {wire}");
                assert_eq!(
                    values(&headers, "x-ai-router-model-fallback"),
                    [format!("gpt-5.4-nano -> {MODEL}")]
                );
                assert_eq!(
                    values(&headers, "x-ai-router-reasoning-effort"),
                    disclosed.into_iter().collect::<Vec<_>>(),
                    "{path} {effort} {streaming}"
                );
                assert!(wire.contains(MODEL) && !wire.contains("\"gpt-5.4-nano\""));
            }
        }
    }
    // An exact Antigravity ID keeps the strict effort check.
    let body = studio_body("/v1/responses", MODEL, "high", "title", false);
    let (status, headers, _) = exchange(&harness, "/v1/responses", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(values(&headers, "x-ai-router-reasoning-effort").is_empty());
    assert_eq!(values(&headers, "x-should-retry"), ["false"]);
    let (_, capabilities) = harness
        .json(Method::GET, "/v1/capabilities", None, Some("test-key"))
        .await;
    let effort = &capabilities["profiles"]["model"]["reasoning_effort"];
    assert_eq!(effort["agy"]["fallback_served"], "served_model_tier");
    assert_eq!(effort["values_per_request"], 1);
    assert_eq!(effort["header_values"], json!(["ignored", "mapped=<level>"]));
}

#[tokio::test]
async fn failures_before_stream_commit_carry_retry_fallback_and_disclosure_headers() {
    for (model, fixture, status, retry) in [
        (
            "gpt-5.4-nano",
            "overload-fixture",
            StatusCode::SERVICE_UNAVAILABLE,
            ("retry-after", "30"),
        ),
        (
            "gpt-5.4-nano",
            "quota-fixture",
            StatusCode::TOO_MANY_REQUESTS,
            ("x-should-retry", "false"),
        ),
        // Antigravity overload names a wait, exactly like OpenAI's.
        (
            "gemini-9-ultra",
            "overload-fixture",
            StatusCode::SERVICE_UNAVAILABLE,
            ("retry-after", "30"),
        ),
    ] {
        for path in ["/v1/chat/completions", "/v1/responses"] {
            let mut seen = Vec::new();
            for streaming in [false, true] {
                // Overload latches the provider, so each mode gets a fresh gateway.
                let harness = fallback_harness(|_| {}).await;
                let mut body = studio_body(path, model, "none", fixture, streaming);
                body["metadata"] = json!({"thread":"synthetic"});
                body["prompt_cache_key"] = json!("thread-1");
                body["max_tokens"] = json!(64);
                if path.ends_with("responses") {
                    body["max_output_tokens"] = body["max_tokens"].take();
                }
                let (got, headers, wire) = exchange(&harness, path, body).await;
                assert_eq!(got, status, "{model} {fixture} {path} {streaming}: {wire}");
                let expected: &[&str] = if retry.1.is_empty() { &[] } else { &[retry.1] };
                assert_eq!(values(&headers, retry.0), expected, "{model} {fixture}");
                let error: Value = serde_json::from_str(&wire).unwrap();
                assert!(error["error"]["code"].is_string());
                assert!(!wire.contains("synthetic-provider-private-detail"));
                let found = disclosures(&headers);
                for name in [
                    "x-ai-router-model-fallback",
                    "x-ai-router-sampling-controls",
                    "x-ai-router-ignored-fields",
                    "x-ai-router-prompt-cache-controls",
                    "x-ai-router-token-budget-mode",
                ] {
                    assert!(
                        found.iter().any(|(n, v)| *n == name && v.len() == 1),
                        "{name} missing on {model} {fixture} {path} {streaming}"
                    );
                }
                seen.push((found, wire));
            }
            assert_eq!(seen[0], seen[1], "{model} {fixture} {path}");
        }
    }
}

#[tokio::test]
async fn committed_streams_carry_the_same_disclosures_as_nonstreaming_requests() {
    let harness = fallback_harness(|_| {}).await;
    for model in ["gpt-5.4-nano", "gemini-9-ultra"] {
        for path in ["/v1/chat/completions", "/v1/responses"] {
            let mut seen = Vec::new();
            for streaming in [false, true] {
                let mut body = studio_body(path, model, "none", "hello", streaming);
                body["user"] = json!("user-1");
                let (status, headers, wire) = exchange(&harness, path, body).await;
                assert_eq!(status, StatusCode::OK, "{wire}");
                seen.push(disclosures(&headers));
            }
            assert_eq!(seen[0], seen[1], "{model} {path}");
            assert!(seen[0].iter().all(|(_, v)| v.len() <= 1));
        }
        // A failure after partial output is an event inside a committed 200
        // whose headers match the nonstreaming failure's.
        for path in ["/v1/chat/completions", "/v1/responses"] {
            let mut seen = Vec::new();
            for streaming in [false, true] {
                let harness = fallback_harness(|_| {}).await;
                let body = studio_body(path, model, "none", "midstream-fixture", streaming);
                let (status, headers, wire) = exchange(&harness, path, body).await;
                assert_eq!(status == StatusCode::OK, streaming, "{wire}");
                if streaming {
                    assert!(wire.contains("partial "));
                    assert!(wire.contains("\"error\""));
                }
                seen.push(disclosures(&headers));
            }
            assert_eq!(seen[0], seen[1], "{model} {path}");
        }
    }
}

#[tokio::test]
async fn missing_required_tool_call_is_not_retried_and_follows_stream_commit() {
    let body = |path: &str, input: &str, streaming: bool| {
        if path.ends_with("completions") {
            json!({"model":"synthetic-openai","messages":[{"role":"user","content":input}],"stream":streaming,
                "tools":[lookup_tool(false)],"tool_choice":"required"})
        } else {
            json!({"model":"synthetic-openai","input":input,"stream":streaming,
                "tools":[lookup_tool(true)],"tool_choice":"required"})
        }
    };
    // No output has started: an HTTP error in both modes, never retried, with
    // the field at fault.
    let harness = DualHarness::new().await;
    for path in ["/v1/chat/completions", "/v1/responses"] {
        for streaming in [false, true] {
            let (status, headers, wire) =
                exchange(&harness, path, body(path, "no-call-fixture", streaming)).await;
            assert_eq!(status, StatusCode::BAD_GATEWAY, "{path} {streaming} {wire}");
            assert_eq!(values(&headers, "x-should-retry"), ["false"]);
            let error: Value = serde_json::from_str(&wire).unwrap();
            assert_eq!(error["error"]["code"], "missing_required_tool_call");
            assert_eq!(error["error"]["param"], "tool_choice");
        }
        // Validation errors keep their param before commit too.
        let mut invalid = body(path, "hello", true);
        invalid["tool_choice"] = json!({"type":"custom","name":"lookup"});
        let (status, headers, wire) = exchange(&harness, path, invalid).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(values(&headers, "x-should-retry"), ["false"]);
        assert_eq!(serde_json::from_str::<Value>(&wire).unwrap()["error"]["param"], "tool_choice");
    }
    // A stream already committed by the wait bound reports it as an event.
    let harness = DualHarness::with_commit_wait(5, Duration::from_millis(100)).await;
    for path in ["/v1/chat/completions", "/v1/responses"] {
        let (status, _, wire) = exchange(
            &harness,
            path,
            body(path, "deadline-fixture no-call-fixture", true),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{wire}");
        let frames = frames(wire.as_bytes());
        assert!(!wire.contains("synthetic answer") && !wire.contains("Hello from fixture"));
        if path.ends_with("responses") {
            let kinds: Vec<&str> = frames.iter().map(|f| f.event.as_str()).collect();
            assert_eq!(
                kinds,
                ["response.created", "response.in_progress", "error", "response.failed"]
            );
            let error = event_json(&frames[2]);
            assert_eq!(error["code"], "missing_required_tool_call");
            assert_eq!(error["param"], "tool_choice");
            assert_eq!(error["error"]["param"], "tool_choice");
        } else {
            assert_eq!(frames.last().unwrap().data, "[DONE]");
            let error = event_json(&frames[frames.len() - 2]);
            assert_eq!(error["error"]["code"], "missing_required_tool_call");
            assert_eq!(error["error"]["param"], "tool_choice");
        }
    }
}

// T7 review findings. A known model's outage stays its own error after a
// restart; the remembered catalogue lives in the telemetry volume.
#[tokio::test]
async fn known_models_keep_their_outage_after_a_restart_instead_of_falling_back() {
    let gemini_target = |config: &mut Config| config.fallback_gemini_model = MODEL.into();
    let harness = DualHarness::configured(gemini_target).await;
    harness.model_mode.store(3, Ordering::SeqCst);
    harness.state.refresh_openai().await;
    let served = serve(&harness, "/v1/responses", "gpt-6-luna", json!({})).await;
    assert_eq!(served.status, StatusCode::OK);
    assert!(served.fallback.is_none());
    // Restart while the OpenAI catalogue probe fails (models: []).
    let harness = harness.restart(gemini_target, 1).await;
    assert!(harness.state.openai_provider.read().await.models.is_empty());
    for path in ["/v1/chat/completions", "/v1/responses"] {
        for streaming in [false, true] {
            for known in ["gpt-6-luna", "synthetic-openai"] {
                let body = studio_body(path, known, "low", "hello", streaming);
                let (status, headers, wire) = exchange(&harness, path, body).await;
                assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{known} {path}: {wire}");
                assert!(header(&headers, "x-ai-router-model-fallback").is_none());
                assert_eq!(header(&headers, "retry-after"), Some("30"));
                assert!(header(&headers, "x-should-retry").is_none());
                // provider_unavailable, refined to the provider's own latched
                // reason when it has one: here the failed catalogue probe.
                let error: Value = serde_json::from_str(&wire).unwrap();
                assert_eq!(error["error"]["code"], "provider_request_failed", "{wire}");
                assert!(!wire.contains(MODEL));
            }
        }
    }
    // IDs never seen still cross to the live Gemini target, disclosed.
    let served = serve(&harness, "/v1/responses", "gpt-5.4-nano", json!({})).await;
    assert_eq!(served.status, StatusCode::OK);
    assert_eq!(served.fallback.as_deref(), Some(format!("gpt-5.4-nano -> {MODEL}").as_str()));
}

#[tokio::test]
async fn remembered_fallback_target_reports_its_outage_when_no_target_is_live() {
    // The Gemini target was never seen and the OpenAI target is down.
    let harness =
        fallback_harness(|config| config.fallback_gemini_model = "gemini-absent".into()).await;
    harness.model_mode.store(1, Ordering::SeqCst);
    harness.state.refresh_openai().await;
    for path in ["/v1/chat/completions", "/v1/responses"] {
        for streaming in [false, true] {
            for requested in ["gpt-5.4-nano", "gemini-9-ultra"] {
                let body = studio_body(path, requested, "low", "hello", streaming);
                let (status, headers, wire) = exchange(&harness, path, body).await;
                assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{requested}: {wire}");
                assert_eq!(
                    values(&headers, "x-ai-router-model-fallback"),
                    [format!("{requested} -> synthetic-openai")]
                );
                assert_eq!(header(&headers, "retry-after"), Some("30"));
                assert!(header(&headers, "x-should-retry").is_none());
                assert_ne!(
                    serde_json::from_str::<Value>(&wire).unwrap()["error"]["code"],
                    "model_not_found"
                );
            }
        }
    }
    let (_, capabilities) = harness
        .json(Method::GET, "/v1/capabilities", None, Some("test-key"))
        .await;
    let policy = &capabilities["profiles"]["model"]["model_fallback"];
    assert_eq!(policy["neither_available"], "remembered_target_error");
    assert_eq!(policy["never_seen_targets"], "model_not_found");
    assert_eq!(policy["remembered_ids"], "persisted_across_restarts");
}

#[tokio::test]
async fn crossed_fallback_does_not_move_reasoning_replay_to_another_provider() {
    let harness = fallback_harness(|_| {}).await;
    // An OpenAI-served turn stores encrypted reasoning for the client.
    let (status, first) = harness
        .post(
            "/v1/responses",
            json!({"model":"gpt-5.4-nano","input":"tools-fixture","tools":[lookup_tool(true)]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let reasoning = first["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "reasoning")
        .unwrap()
        .clone();
    harness.model_mode.store(1, Ordering::SeqCst);
    harness.state.refresh_openai().await;
    let upstream_calls = harness.captured.lock().await.len();
    for input in [
        json!([{"type":"item_reference","id":reasoning["id"]},{"role":"user","content":"next"}]),
        json!([reasoning.clone(),{"role":"user","content":"next"}]),
    ] {
        for streaming in [false, true] {
            let (status, headers, wire) = exchange(
                &harness,
                "/v1/responses",
                json!({"model":"gpt-5.4-nano","input":input,"stream":streaming}),
            )
            .await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{wire}");
            assert_eq!(
                serde_json::from_str::<Value>(&wire).unwrap()["error"]["code"],
                "provider_unavailable"
            );
            assert_eq!(header(&headers, "retry-after"), Some("30"));
            assert_eq!(
                values(&headers, "x-ai-router-model-fallback"),
                [format!("gpt-5.4-nano -> {MODEL}")]
            );
        }
    }
    assert_eq!(harness.captured.lock().await.len(), upstream_calls);
    // The same crossed fallback without reasoning state is still served.
    let served = serve(&harness, "/v1/responses", "gpt-5.4-nano", json!({})).await;
    assert_eq!(served.status, StatusCode::OK);
}

#[tokio::test]
async fn retry_guidance_for_credentials_latches_and_timeouts() {
    // Upstream credential rejection is durable, also when replayed locally.
    let harness = DualHarness::new().await;
    for _ in 0..2 {
        let (status, headers, wire) = exchange(
            &harness,
            "/v1/responses",
            json!({"model":"synthetic-openai","input":"upstream-auth-fixture"}),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{wire}");
        assert!(wire.contains("invalid_token"));
        assert_eq!(header(&headers, "x-should-retry"), Some("false"));
        assert!(header(&headers, "retry-after").is_none());
    }
    assert_eq!(harness.captured.lock().await.len(), 1);
    // A latched mid-stream server failure replays as 503 with its own code.
    let harness = DualHarness::new().await;
    let (status, _, wire) = exchange(
        &harness,
        "/v1/responses",
        json!({"model":"synthetic-openai","input":"midstream-fixture","stream":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{wire}");
    assert!(wire.contains("response.failed"));
    let (status, headers, wire) = exchange(
        &harness,
        "/v1/responses",
        json!({"model":"synthetic-openai","input":"hello"}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{wire}");
    assert_eq!(
        serde_json::from_str::<Value>(&wire).unwrap()["error"]["code"],
        "server_error"
    );
    assert_eq!(header(&headers, "retry-after"), Some("30"));
    assert!(header(&headers, "x-should-retry").is_none());
    // A deadline is not retried: each retry is another paid generation.
    let harness = DualHarness::with_timeout(1).await;
    let first = harness.state.admit_provider(ProviderId::OpenAi).await.unwrap();
    let second = harness.state.admit_provider(ProviderId::OpenAi).await.unwrap();
    let request = exchange(
        &harness,
        "/v1/chat/completions",
        json!({"model":"synthetic-openai","messages":[{"role":"user","content":"deadline-fixture"}]}),
    );
    let release = async move {
        tokio::time::sleep(Duration::from_millis(550)).await;
        drop(first);
        drop(second);
    };
    let ((status, headers, wire), ()) = tokio::join!(request, release);
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{wire}");
    assert!(wire.contains("provider_timeout"));
    assert_eq!(header(&headers, "x-should-retry"), Some("false"));
    assert!(header(&headers, "retry-after").is_none());
}

#[tokio::test]
async fn saturation_is_recorded_and_counted_by_its_error_code() {
    let harness = DualHarness::configured(|config| {
        config.openai_max_concurrent = 1;
        config.openai_max_queue = 0;
    })
    .await;
    let held = harness.state.admit_provider(ProviderId::OpenAi).await.unwrap();
    let served = serve(&harness, "/v1/responses", "synthetic-openai", json!({})).await;
    assert_eq!(served.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(served.body["error"]["code"], "busy");
    drop(held);
    let invalid = serve(&harness, "/v1/responses", "never-seen-model", json!({})).await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
    let snapshot = harness.state.telemetry.snapshot().await;
    let code = |id: &str| {
        snapshot
            .records
            .iter()
            .find(|record| record.request_id == id)
            .and_then(|record| record.error_code.clone())
    };
    assert_eq!(code(&served.request_id).as_deref(), Some("busy"));
    assert_eq!(code(&invalid.request_id).as_deref(), Some("model_not_found"));
    let counts = &snapshot.counts_since_start;
    assert_eq!((counts.busy, counts.input, counts.other), (1, 1, 0));
}

#[tokio::test]
async fn failed_responses_stream_keeps_partial_output_and_metadata() {
    let harness = DualHarness::new().await;
    for model in ["synthetic-openai", MODEL] {
        let response = harness
            .send(
                Method::POST,
                "/v1/responses",
                Some(json!({"model":model,"input":"midstream-fixture","stream":true,"metadata":{"thread":"synthetic"}})),
                Some("test-key"),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let frames = frames(&read_bytes(response).await);
        let failed = event_json(
            frames
                .iter()
                .find(|frame| frame.event == "response.failed")
                .unwrap(),
        );
        let response = &failed["response"];
        assert_eq!(response["status"], "failed");
        assert_eq!(response["metadata"], json!({"thread":"synthetic"}), "{model}");
        let output = response["output"].as_array().unwrap();
        assert_eq!(output.len(), 1, "{model}: {response}");
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["status"], "incomplete");
        assert_eq!(output[0]["content"][0]["text"], "partial ");
    }
}

#[tokio::test]
async fn tool_narrowing_and_metadata_errors_name_their_param() {
    let harness = DualHarness::new().await;
    let find = json!({"type":"function","name":"find_records","description":"Find","parameters":{"type":"object","properties":{"q":{"type":"string"}},"required":["q"],"additionalProperties":false}});
    for streaming in [false, true] {
        let (status, _, wire) = exchange(
            &harness,
            "/v1/responses",
            json!({"model":"synthetic-openai","input":"tools-fixture","stream":streaming,"tools":[lookup_tool(true),find.clone()],
                "tool_choice":{"type":"allowed_tools","mode":"auto","tools":[{"type":"function","name":"find_records"}]}}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{wire}");
        let error: Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(error["error"]["code"], "invalid_provider_tool_call");
        assert_eq!(error["error"]["param"], "tool_choice");
    }
    // Without a narrowed choice the provider's call has no request field.
    let (status, error) = harness
        .post(
            "/v1/responses",
            json!({"model":"synthetic-openai","input":"malformed-fixture","tools":[lookup_tool(true)]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(error["error"].get("param").is_none());
    let (status, error) = harness
        .post(
            "/v1/responses",
            json!({"model":MODEL,"input":"hello","metadata":{"k":"v".repeat(513)}}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error["error"]["param"], "metadata");
}
