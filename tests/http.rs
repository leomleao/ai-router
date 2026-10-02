//! HTTP acceptance tests against the deterministic AGY subprocess fixture.
//! These verify n8n/Hermes-compatible wire contracts, not the complete apps or
//! any signed-in AGY model. Run the Python fixture only in local Docker.
use ai_router::{
    api::{self, AppState},
    config::{Config, KeyConfig},
    monitor::Telemetry,
    native,
    protocol::RunEvent,
    runner::Runner,
};
use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode},
    response::Response,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tower::ServiceExt;

const MODEL: &str = "gemini-3-pro";

struct Harness {
    root: tempfile::TempDir,
    state: Arc<AppState>,
    app: Router,
}

impl Harness {
    async fn new(adjust: impl FnOnce(&mut Config)) -> Self {
        let root = tempfile::tempdir().unwrap();
        let mut config = Config::for_test(root.path());
        config.agy_bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-agy");
        assert_eq!(config.agy_bin.file_name().unwrap(), "fake-agy");
        config.request_timeout_secs = 5;
        config.preauth_per_minute = 2000;
        config.global_preauth_per_minute = 10000;
        config.per_key_per_minute = 2000;
        let digest = Sha256::digest(b"other-test-key")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        config.keys.push(KeyConfig {
            id: "other".into(),
            sha256: digest,
            scopes: vec!["model".into(), "native".into()],
        });
        adjust(&mut config);
        config.validate().unwrap();
        let config = Arc::new(config);
        let telemetry = Arc::new(
            Telemetry::open(
                &config.telemetry_dir,
                config.retention_days,
                config.max_events,
            )
            .unwrap(),
        );
        let runner = Runner::with_relay_executable(
            config.clone(),
            PathBuf::from(env!("CARGO_BIN_EXE_ai-router")),
        );
        let state = AppState::new(config, runner, telemetry);
        state.refresh_provider().await;
        assert!(
            state.provider.read().await.authenticated,
            "fake provider probe failed"
        );
        let app = api::router(state.clone());
        Self { root, state, app }
    }

    fn request(
        method: Method,
        path: &str,
        body: Option<Value>,
        bearer: Option<&str>,
    ) -> Request<Body> {
        let mut request = Request::builder().method(method).uri(path);
        if let Some(key) = bearer {
            request = request.header("authorization", format!("Bearer {key}"));
        }
        if body.is_some() {
            request = request.header("content-type", "application/json");
        }
        request
            .body(
                body.map(|v| Body::from(v.to_string()))
                    .unwrap_or_else(Body::empty),
            )
            .unwrap()
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        bearer: Option<&str>,
    ) -> Response {
        tokio::time::timeout(
            Duration::from_secs(10),
            self.app
                .clone()
                .oneshot(Self::request(method, path, body, bearer)),
        )
        .await
        .unwrap()
        .unwrap()
    }

    async fn json(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        bearer: Option<&str>,
    ) -> (StatusCode, Value) {
        let response = self.send(method, path, body, bearer).await;
        let status = response.status();
        (status, read_json(response).await)
    }

    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.json(Method::POST, path, Some(body), Some("test-key"))
            .await
    }

    fn workspaces(&self) -> Vec<PathBuf> {
        std::fs::read_dir(&self.state.config.workspace_dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn wait_empty(&self) {
        for _ in 0..300 {
            if self.workspaces().is_empty() && self.state.telemetry.snapshot().await.active == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "generation workspace/slot survived: {:?}",
            self.workspaces()
        );
    }

    async fn wait_job(&self, id: &str, expected: &str) -> Value {
        for _ in 0..300 {
            let (status, value) = self
                .json(
                    Method::GET,
                    &format!("/v1/agy/runs/{id}"),
                    None,
                    Some("test-key"),
                )
                .await;
            assert_eq!(status, StatusCode::OK);
            if value["status"] == expected {
                return value;
            }
            if value["status"] == "failed" {
                panic!("native fixture unexpectedly failed: {value}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("native job did not become {expected}");
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.state.stop();
    }
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
fn assert_single_native_error(frames: &[SseFrame], code: &str) {
    let events: Vec<RunEvent> = frames
        .iter()
        .map(|frame| {
            serde_json::from_str(&frame.data).expect("native events must keep the RunEvent schema")
        })
        .collect();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RunEvent::Completed(_) | RunEvent::Error(_)))
            .count(),
        1,
        "native run emitted more than one terminal event"
    );
    assert!(
        matches!(events.last(), Some(RunEvent::Error(error)) if error.code == code),
        "expected terminal {code}, got {events:?}"
    );
}
fn chat(prompt: &str) -> Value {
    json!({"model":MODEL,"messages":[{"role":"user","content":prompt}]})
}
fn response(prompt: &str) -> Value {
    json!({"model":MODEL,"input":prompt,"store":false})
}
fn weather_chat() -> Value {
    json!({"type":"function","function":{"name":"weather","description":"Client-owned weather lookup","strict":true,"parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false}}})
}
fn weather_response() -> Value {
    let tool = weather_chat();
    let mut tool = tool["function"].clone();
    tool["type"] = json!("function");
    tool
}

#[tokio::test]
async fn model_discovery_readiness_and_explicit_capabilities_use_cached_probe() {
    let harness = Harness::new(|_| {}).await;
    let (status, ready) = harness.json(Method::GET, "/ready", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ready, json!({"ready":true}));
    let (status, models) = harness
        .json(Method::GET, "/v1/models", None, Some("test-key"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(models["data"][0]["id"], MODEL);
    let (status, caps) = harness
        .json(Method::GET, "/v1/capabilities", None, Some("test-key"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(caps["profiles"]["model"]["max_tool_calls_per_turn"], 1);
    assert_eq!(caps["profiles"]["native"]["enabled"], false);
    assert_eq!(
        caps["verification"]["real_model_and_client_loops"],
        "operator_gate"
    );
    let (status, _) = harness
        .post("/v1/embeddings", json!({"model":MODEL,"input":"hello"}))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(harness.workspaces().is_empty());
}

#[tokio::test]
async fn both_nonstreaming_api_styles_have_real_text_and_honest_usage() {
    let harness = Harness::new(|_| {}).await;
    let (status, value) = harness.post("/v1/chat/completions", chat("hello")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["object"], "chat.completion");
    assert_eq!(
        value["choices"][0]["message"]["content"],
        "Hello from fixture"
    );
    assert_eq!(value["choices"][0]["finish_reason"], "stop");
    assert_eq!(value["usage"]["total_tokens"], 23);
    assert_eq!(value["agy_usage_partial"], false);
    let (status, value) = harness.post("/v1/responses", response("hello")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["object"], "response");
    assert_eq!(value["status"], "completed");
    assert_eq!(
        value["output"][0]["content"][0]["text"],
        "Hello from fixture"
    );
    assert_eq!(value["usage"]["total_tokens"], 23);
    harness.wait_empty().await;
    let records = harness.state.telemetry.snapshot().await.records;
    assert_eq!(
        records
            .iter()
            .filter(|record| record.status == "success")
            .count(),
        2
    );
    assert!(
        records
            .iter()
            .all(|record| record.startup_ms.is_some() && record.first_output_ms.is_some())
    );
}

#[tokio::test]
async fn chat_stream_keeps_completion_id_and_finishes_with_usage_and_done() {
    let harness = Harness::new(|_| {}).await;
    let mut body = chat("hello");
    body["stream"] = json!(true);
    body["stream_options"] = json!({"include_usage":true});
    let stream = harness
        .send(
            Method::POST,
            "/v1/chat/completions",
            Some(body),
            Some("test-key"),
        )
        .await;
    assert_eq!(stream.status(), StatusCode::OK);
    assert_eq!(stream.headers()["content-type"], "text/event-stream");
    let request_id = stream.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let frames = frames(&read_bytes(stream).await);
    assert_eq!(frames.last().unwrap().data, "[DONE]");
    let values: Vec<_> = frames
        .iter()
        .filter(|frame| frame.data != "[DONE]")
        .map(event_json)
        .collect();
    let id = format!("chatcmpl_{request_id}");
    assert!(values.iter().all(|value| value["id"] == id));
    let text = values
        .iter()
        .filter_map(|value| {
            value
                .pointer("/choices/0/delta/content")
                .and_then(Value::as_str)
        })
        .collect::<String>();
    assert_eq!(text, "Hello from fixture");
    assert_eq!(
        values
            .iter()
            .filter(|value| value.pointer("/choices/0/finish_reason") == Some(&json!("stop")))
            .count(),
        1
    );
    let usage = values.last().unwrap();
    assert_eq!(usage["choices"], json!([]));
    assert_eq!(usage["usage"]["total_tokens"], 23);
    harness.wait_empty().await;
}

#[tokio::test]
async fn response_stream_has_contiguous_sequence_and_stable_item_ids() {
    let harness = Harness::new(|_| {}).await;
    let mut body = response("hello");
    body["stream"] = json!(true);
    let stream = harness
        .send(Method::POST, "/v1/responses", Some(body), Some("test-key"))
        .await;
    assert_eq!(stream.status(), StatusCode::OK);
    let frames = frames(&read_bytes(stream).await);
    let values: Vec<_> = frames.iter().map(event_json).collect();
    assert_eq!(frames[0].event, "response.created");
    assert_eq!(frames.last().unwrap().event, "response.completed");
    for (index, value) in values.iter().enumerate() {
        assert_eq!(value["sequence_number"], index as u64);
        assert_eq!(value["type"], frames[index].event);
    }
    let response_id = values[0]["response"]["id"].as_str().unwrap();
    let completed = values.last().unwrap();
    assert_eq!(completed["response"]["id"], response_id);
    let item_id = format!("msg_{response_id}");
    for value in &values {
        if let Some(value) = value.get("item_id") {
            assert_eq!(value, &json!(item_id));
        }
    }
    let text = values
        .iter()
        .filter(|value| value["type"] == "response.output_text.delta")
        .filter_map(|value| value["delta"].as_str())
        .collect::<String>();
    assert_eq!(
        text,
        completed["response"]["output"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
    );
    harness.wait_empty().await;
}

#[tokio::test]
async fn chat_completes_two_real_inert_mcp_tool_rounds_with_exact_history_ids() {
    let harness = Harness::new(|_| {}).await;
    let mut body = chat("__fake_second_tool__ weather lookup");
    body["tools"] = json!([weather_chat()]);
    body["tool_choice"] = json!("auto");
    body["parallel_tool_calls"] = json!(true);
    let mut ids = Vec::new();
    for round in 0..2 {
        let (status, value) = harness.post("/v1/chat/completions", body.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(value["agy_usage_partial"], true);
        let message = value["choices"][0]["message"].clone();
        let calls = message["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        let id = calls[0]["id"].as_str().unwrap().to_owned();
        assert!(!ids.contains(&id));
        ids.push(id.clone());
        assert_eq!(calls[0]["function"]["name"], "weather");
        assert_eq!(
            serde_json::from_str::<Value>(calls[0]["function"]["arguments"].as_str().unwrap())
                .unwrap(),
            json!({"city":"London"})
        );
        let history = body["messages"].as_array_mut().unwrap();
        history.push(message);
        // This is the client execution boundary; the relay never fabricates an output.
        history.push(json!({"role":"tool","tool_call_id":id,"content":format!("client-executed weather round {round}")}));
    }
    let (status, value) = harness.post("/v1/chat/completions", body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        value["choices"][0]["message"]["content"],
        "Fixture final answer"
    );
    assert_eq!(value["choices"][0]["finish_reason"], "stop");
    harness.wait_empty().await;
}

#[tokio::test]
async fn responses_complete_two_streamed_tool_rounds_with_sdk_replay_items() {
    let harness = Harness::new(|_| {}).await;
    let mut body = json!({"model":MODEL,"input":[{"role":"user","content":"__fake_second_tool__ weather lookup"}],"tools":[weather_response()],"tool_choice":"auto","parallel_tool_calls":true,"store":false,"stream":true});
    let mut ids = Vec::new();
    for round in 0..2 {
        let stream = harness
            .send(
                Method::POST,
                "/v1/responses",
                Some(body.clone()),
                Some("test-key"),
            )
            .await;
        assert_eq!(stream.status(), StatusCode::OK);
        let frames = frames(&read_bytes(stream).await);
        let values: Vec<_> = frames.iter().map(event_json).collect();
        for (index, value) in values.iter().enumerate() {
            assert_eq!(value["sequence_number"], index as u64);
        }
        let final_response = &values.last().unwrap()["response"];
        assert_eq!(final_response["status"], "completed");
        assert_eq!(final_response["agy_usage_partial"], true);
        let call = final_response["output"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call")
            .unwrap()
            .clone();
        let id = call["call_id"].as_str().unwrap().to_owned();
        assert!(!ids.contains(&id));
        ids.push(id.clone());
        let item_id = call["id"].as_str().unwrap();
        let argument_events: Vec<_> = values
            .iter()
            .filter(|value| value["type"] == "response.function_call_arguments.delta")
            .collect();
        assert_eq!(argument_events.len(), 1);
        assert_eq!(argument_events[0]["item_id"], item_id);
        assert_eq!(
            serde_json::from_str::<Value>(argument_events[0]["delta"].as_str().unwrap()).unwrap(),
            json!({"city":"London"})
        );
        body["input"].as_array_mut().unwrap().push(call);
        body["input"].as_array_mut().unwrap().push(json!({"type":"function_call_output","call_id":id,"output":format!("client-executed weather round {round}")}));
    }
    let stream = harness
        .send(Method::POST, "/v1/responses", Some(body), Some("test-key"))
        .await;
    let frames = frames(&read_bytes(stream).await);
    let completed = event_json(frames.last().unwrap());
    assert_eq!(
        completed["response"]["output"][0]["content"][0]["text"],
        "Fixture final answer"
    );
    harness.wait_empty().await;
}

#[tokio::test]
async fn type_omitted_function_schemas_reach_real_mcp_without_losing_constraints() {
    let harness = Harness::new(|_| {}).await;
    for schema in [
        json!({}),
        json!({"properties":{"city":{"type":"string"}},"oneOf":[{"required":["city"],"properties":{"city":{"enum":["London"]}}},{"required":["temperature"]}]}),
    ] {
        let expected = if schema.get("properties").is_some() {
            json!({"city":"London"})
        } else {
            json!({})
        };
        let mut chat_body = chat("Select the supplied client-owned tool");
        chat_body["tools"] = json!([{"type":"function","function":{"name":"lookup","strict":true,"parameters":schema}}]);
        let (status, value) = harness
            .post("/v1/chat/completions", chat_body.clone())
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["choices"][0]["finish_reason"], "tool_calls");
        let assistant = value["choices"][0]["message"].clone();
        let call = &assistant["tool_calls"][0];
        assert_eq!(
            serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap()).unwrap(),
            expected
        );
        let id = call["id"].clone();
        chat_body["messages"]
            .as_array_mut()
            .unwrap()
            .push(assistant);
        chat_body["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"tool","tool_call_id":id,"content":"Client completed lookup"}));
        let (status, value) = harness.post("/v1/chat/completions", chat_body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            value["choices"][0]["message"]["content"],
            "Fixture final answer"
        );
        let mut response_body = json!({"model":MODEL,"input":[{"role":"user","content":"Select the supplied client-owned tool"}],"tools":[{"type":"function","name":"lookup","strict":true,"parameters":schema}],"store":false});
        let (status, value) = harness.post("/v1/responses", response_body.clone()).await;
        assert_eq!(status, StatusCode::OK);
        let call = value["output"][0].clone();
        assert_eq!(call["type"], "function_call");
        assert_eq!(
            serde_json::from_str::<Value>(call["arguments"].as_str().unwrap()).unwrap(),
            expected
        );
        let id = call["call_id"].clone();
        response_body["input"].as_array_mut().unwrap().push(call);
        response_body["input"].as_array_mut().unwrap().push(
            json!({"type":"function_call_output","call_id":id,"output":"Client completed lookup"}),
        );
        let (status, value) = harness.post("/v1/responses", response_body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            value["output"][0]["content"][0]["text"],
            "Fixture final answer"
        );
    }
    harness.wait_empty().await;
}

#[tokio::test]
async fn schema_output_and_provider_errors_are_consistent_across_api_styles() {
    let harness = Harness::new(|_| {}).await;
    let schema = json!({"type":"object","properties":{"count":{"type":"integer"}},"required":["count"],"additionalProperties":false});
    let mut body = chat("schema");
    body["response_format"] =
        json!({"type":"json_schema","json_schema":{"name":"count","strict":true,"schema":schema}});
    let (status, value) = harness.post("/v1/chat/completions", body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(value["choices"][0]["message"]["content"].as_str().unwrap())
            .unwrap(),
        json!({"count":1})
    );
    let mut body = response("schema");
    body["text"] =
        json!({"format":{"type":"json_schema","name":"count","strict":true,"schema":schema}});
    let (status, value) = harness.post("/v1/responses", body.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(value["output"][0]["content"][0]["text"].as_str().unwrap())
            .unwrap(),
        json!({"count":1})
    );
    for prompt in [
        "__fake_bad_schema__",
        "__fake_schema_fenced__",
        "__fake_schema_prose__",
        "__fake_schema_invalid_json__",
    ] {
        let mut chat_body = chat(prompt);
        chat_body["response_format"] = json!({"type":"json_schema","json_schema":{"name":"count","strict":true,"schema":schema}});
        let (status, value) = harness.post("/v1/chat/completions", chat_body).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(value["error"]["code"], "provider_schema_violation");
        body["input"] = json!(prompt);
        body["stream"] = json!(true);
        let stream = harness
            .send(
                Method::POST,
                "/v1/responses",
                Some(body.clone()),
                Some("test-key"),
            )
            .await;
        let frames = frames(&read_bytes(stream).await);
        assert_eq!(frames.last().unwrap().event, "response.failed");
        assert!(
            !frames
                .iter()
                .any(|frame| frame.event == "response.output_text.delta")
        );
        assert_eq!(
            event_json(frames.last().unwrap())["response"]["error"]["code"],
            "provider_schema_violation"
        );
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame.event == "response.failed")
                .count(),
            1
        );
    }
    let (status, value) = harness
        .post("/v1/chat/completions", chat("__fake_auth__"))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(value["error"]["code"], "provider_auth_required");
    assert!(!value.to_string().contains("sensitive diagnostic"));
    harness.wait_empty().await;
}

#[tokio::test]
async fn invalid_keys_and_unsupported_inputs_never_start_generation() {
    let harness = Harness::new(|config| {
        use std::os::unix::fs::PermissionsExt;
        let root = config.state_dir.parent().unwrap();
        let spy = root.join("provider-spy.py");
        let log = root.join("provider-spawns.txt");
        // Count invocations of the explicitly configured fake, including the
        // three discovery probes. Never discover a provider from PATH.
        let script = format!("#!/usr/bin/env python3\nimport os,sys\nfrom pathlib import Path\nwith Path({}).open('a') as log: log.write('spawn\\n')\nprovider={}\nos.execv(provider,[provider,*sys.argv[1:]])\n", json!(log.to_str().unwrap()), json!(config.agy_bin.to_str().unwrap()));
        std::fs::write(&spy, script).unwrap();
        std::fs::set_permissions(&spy, std::fs::Permissions::from_mode(0o755)).unwrap();
        config.agy_bin = spy;
    }).await;
    let calls_before =
        std::fs::read_to_string(harness.root.path().join("provider-spawns.txt")).unwrap();
    for bearer in [None, Some("wrong-key")] {
        let (status, value) = harness
            .json(
                Method::POST,
                "/v1/chat/completions",
                Some(chat("__fake_timeout__")),
                bearer,
            )
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(value["error"]["code"], "invalid_api_key");
        assert!(harness.workspaces().is_empty());
    }
    for field in ["temperature", "max_tokens", "seed", "top_p"] {
        let mut body = chat("hello");
        body[field] = json!(1);
        let (status, value) = harness.post("/v1/chat/completions", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(value["error"]["code"], "unsupported_parameter");
        assert!(harness.workspaces().is_empty());
    }
    for verbosity in [json!("low"), json!("high"), json!("invalid"), json!(1)] {
        let mut body = response("hello");
        body["text"] = json!({"format":{"type":"text"}, "verbosity":verbosity});
        let (status, value) = harness.post("/v1/responses", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(value["error"]["code"], "unsupported_parameter");
        assert_eq!(
            value["error"]["message"],
            "text.verbosity is not supported by the AGY adapter"
        );
        assert!(harness.workspaces().is_empty());
    }
    let mut body = response("hello");
    body["previous_response_id"] = json!("resp_foreign");
    assert_eq!(
        harness.post("/v1/responses", body).await.0,
        StatusCode::BAD_REQUEST
    );
    let mut body = chat("hello");
    body["messages"][0]["content"] =
        json!([{"type":"image_url","image_url":{"url":"https://example.invalid/image"}}]);
    assert_eq!(
        harness.post("/v1/chat/completions", body).await.0,
        StatusCode::BAD_REQUEST
    );
    assert!(harness.workspaces().is_empty());
    assert_eq!(harness.state.telemetry.snapshot().await.active, 0);
    assert_eq!(
        std::fs::read_to_string(harness.root.path().join("provider-spawns.txt")).unwrap(),
        calls_before,
        "rejected requests launched a provider subprocess"
    );
}

#[tokio::test]
async fn header_and_streamed_body_bounds_reject_before_provider_start() {
    let harness = Harness::new(|config| config.max_body_bytes = 1024).await;
    let mut body = chat("hello");
    body["messages"][0]["content"] = json!("x".repeat(2048));
    assert_eq!(
        harness.post("/v1/chat/completions", body).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let chunks = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(
        axum::body::Bytes::from(vec![b'x'; 2048]),
    )]);
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer test-key")
        .header("content-type", "application/json")
        .body(Body::from_stream(chunks))
        .unwrap();
    assert_eq!(
        harness.app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer test-key")
        .header("x-large", "x".repeat(16385))
        .header("content-type", "application/json")
        .body(Body::from(chat("hello").to_string()))
        .unwrap();
    assert_eq!(
        harness.app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
    );
    assert!(harness.workspaces().is_empty());
}

#[tokio::test]
async fn key_and_global_rate_limits_are_cheap_and_include_retry_after() {
    let harness = Harness::new(|config| config.per_key_per_minute = 1).await;
    assert_eq!(
        harness
            .json(Method::GET, "/v1/models", None, Some("test-key"))
            .await
            .0,
        StatusCode::OK
    );
    let rejected = harness
        .send(
            Method::POST,
            "/v1/chat/completions",
            Some(chat("__fake_timeout__")),
            Some("test-key"),
        )
        .await;
    assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(rejected.headers().contains_key("retry-after"));
    assert!(harness.workspaces().is_empty());
    let harness = Harness::new(|config| config.global_preauth_per_minute = 1).await;
    assert_eq!(
        harness
            .json(Method::GET, "/v1/models", None, Some("wrong-key"))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let rejected = harness
        .send(Method::GET, "/v1/models", None, Some("test-key"))
        .await;
    assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(harness.workspaces().is_empty());
}

#[tokio::test]
async fn queue_saturation_is_bounded_and_shutdown_releases_waiters() {
    let harness = Harness::new(|config| {
        config.max_concurrent = 1;
        config.max_queue = 1;
    })
    .await;
    let active = harness.state.admit().await.unwrap();
    let state = harness.state.clone();
    let queued = tokio::spawn(async move { state.admit().await });
    for _ in 0..100 {
        if harness.state.telemetry.snapshot().await.queued == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let snapshot = harness.state.telemetry.snapshot().await;
    assert_eq!(snapshot.active, 1);
    assert_eq!(snapshot.queued, 1);
    let rejected = harness
        .send(
            Method::POST,
            "/v1/chat/completions",
            Some(chat("hello")),
            Some("test-key"),
        )
        .await;
    assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(rejected.headers().contains_key("retry-after"));
    assert!(harness.workspaces().is_empty());
    harness.state.stop();
    assert_eq!(queued.await.unwrap().err().unwrap().code, "shutting_down");
    drop(active);
    for _ in 0..100 {
        let state = harness.state.telemetry.snapshot().await;
        if state.active == 0 && state.queued == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("admission counters were not released");
}

#[tokio::test]
async fn native_gate_ownership_artifacts_and_synthetic_links_are_enforced() {
    let disabled = Harness::new(|_| {}).await;
    assert_eq!(
        disabled
            .post("/v1/agy/runs", json!({"model":MODEL,"prompt":"native"}))
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(disabled.workspaces().is_empty());
    let harness = Harness::new(|config| config.native_enabled = true).await;
    let (status, value) = harness.post("/v1/agy/runs", json!({"model":MODEL,"prompt":"native fixture","files":[{"path":"notes.txt","data_base64":"ZHVtbXk="}]})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = value["id"].as_str().unwrap();
    harness.wait_job(id, "completed").await;
    for suffix in ["", "/events", "/artifacts", "/artifacts/answer.txt"] {
        assert_eq!(
            harness
                .json(
                    Method::GET,
                    &format!("/v1/agy/runs/{id}{suffix}"),
                    None,
                    Some("other-test-key")
                )
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }
    let (status, listed) = harness
        .json(
            Method::GET,
            &format!("/v1/agy/runs/{id}/artifacts"),
            None,
            Some("test-key"),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        listed["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["path"] == "answer.txt")
    );
    let artifact = harness
        .send(
            Method::GET,
            &format!("/v1/agy/runs/{id}/artifacts/answer.txt"),
            None,
            Some("test-key"),
        )
        .await;
    assert_eq!(artifact.status(), StatusCode::OK);
    assert_eq!(artifact.headers()["x-content-type-options"], "nosniff");
    assert_eq!(read_bytes(artifact).await, b"Native fixture artifact\n");
    let workspace = harness.state.config.workspace_dir.join(id);
    let outside = harness.root.path().join("synthetic-canary.txt");
    std::fs::write(&outside, b"synthetic-only").unwrap();
    std::os::unix::fs::symlink(&outside, workspace.join("link.txt")).unwrap();
    std::fs::hard_link(&outside, workspace.join("alias.txt")).unwrap();
    for path in [
        "link.txt",
        "alias.txt",
        "../synthetic-canary.txt",
        ".env.example",
        "secrets/anything",
        "credentials/anything",
        "dummy.pem",
        "dummy.key",
    ] {
        let value = harness
            .send(
                Method::GET,
                &format!("/v1/agy/runs/{id}/artifacts/{path}"),
                None,
                Some("test-key"),
            )
            .await;
        assert!(
            matches!(
                value.status(),
                StatusCode::BAD_REQUEST | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
            ),
            "{path}"
        );
        assert!(!String::from_utf8_lossy(&read_bytes(value).await).contains("synthetic-only"));
    }
    for path in [
        "../escape",
        ".env",
        "a/x.key",
        "secrets/data",
        "credentials/data",
    ] {
        assert!(matches!(harness.post("/v1/agy/runs", json!({"model":MODEL,"prompt":"native","files":[{"path":path,"data_base64":"ZHVtbXk="}]})).await.0, StatusCode::BAD_REQUEST | StatusCode::FORBIDDEN));
    }
    let events = harness
        .send(
            Method::GET,
            &format!("/v1/agy/runs/{id}/events"),
            None,
            Some("test-key"),
        )
        .await;
    let frames = frames(&read_bytes(events).await);
    assert!(!frames.is_empty());
    for (index, frame) in frames.iter().enumerate() {
        assert_eq!(frame.id.as_deref(), Some(index.to_string().as_str()));
        assert_eq!(frame.event, "agy.event");
    }
    let deleted = harness
        .send(
            Method::DELETE,
            &format!("/v1/agy/runs/{id}"),
            None,
            Some("test-key"),
        )
        .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert!(read_bytes(deleted).await.is_empty());
    assert!(!workspace.exists());
    assert_eq!(
        harness
            .json(
                Method::GET,
                &format!("/v1/agy/runs/{id}"),
                None,
                Some("test-key")
            )
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

async fn wait_descendant(workspace: &Path) -> libc::pid_t {
    for _ in 0..300 {
        if let Ok(value) = tokio::fs::read_to_string(workspace.join("descendant.pid")).await {
            return value.parse().unwrap();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("fake subprocess did not create descendant");
}
async fn assert_dead(pid: libc::pid_t) {
    for _ in 0..300 {
        let live = unsafe { libc::kill(pid, 0) == 0 };
        let zombie = tokio::fs::read_to_string(format!("/proc/{pid}/stat"))
            .await
            .ok()
            .is_some_and(|stat| {
                stat.split_once(") ")
                    .is_some_and(|(_, fields)| fields.starts_with('Z'))
            });
        if !live || zombie {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("fake descendant {pid} survived cancellation");
}

#[tokio::test]
async fn streaming_disconnect_kills_descendants_and_cleans_workspace_before_release() {
    let harness = Harness::new(|config| config.max_concurrent = 1).await;
    let mut input = chat("__fake_timeout__");
    input["stream"] = json!(true);
    let stream = harness
        .send(
            Method::POST,
            "/v1/chat/completions",
            Some(input),
            Some("test-key"),
        )
        .await;
    assert_eq!(stream.status(), StatusCode::OK);
    let id = stream.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let workspace = harness.state.config.workspace_dir.join(&id);
    let pid = wait_descendant(&workspace).await;
    drop(stream);
    assert_dead(pid).await;
    harness.wait_empty().await;
    let records = harness.state.telemetry.snapshot().await.records;
    let record = records
        .iter()
        .find(|record| record.request_id == id)
        .unwrap();
    assert_eq!(record.status, "cancelled");
    assert_eq!(record.error_code.as_deref(), Some("client_disconnected"));
}

#[tokio::test]
async fn native_cancel_subscriber_limits_and_global_shutdown_kill_live_children() {
    let harness = Harness::new(|config| config.native_enabled = true).await;
    let (status, value) = harness
        .post(
            "/v1/agy/runs",
            json!({"model":MODEL,"prompt":"__fake_timeout__"}),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = value["id"].as_str().unwrap();
    let pid = wait_descendant(&harness.state.config.workspace_dir.join(id)).await;
    let first = harness
        .send(
            Method::GET,
            &format!("/v1/agy/runs/{id}/events"),
            None,
            Some("test-key"),
        )
        .await;
    let second = harness
        .send(
            Method::GET,
            &format!("/v1/agy/runs/{id}/events"),
            None,
            Some("test-key"),
        )
        .await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(
        harness
            .send(
                Method::GET,
                &format!("/v1/agy/runs/{id}/events"),
                None,
                Some("test-key")
            )
            .await
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        harness
            .json(
                Method::DELETE,
                &format!("/v1/agy/runs/{id}"),
                None,
                Some("other-test-key")
            )
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        harness
            .json(
                Method::DELETE,
                &format!("/v1/agy/runs/{id}"),
                None,
                Some("test-key")
            )
            .await
            .0,
        StatusCode::ACCEPTED
    );
    harness.wait_job(id, "cancelled").await;
    assert_dead(pid).await;
    let cancelled_events = frames(&read_bytes(first).await);
    assert_single_native_error(&cancelled_events, "request_cancelled");
    drop(second);
    let (status, value) = harness
        .post(
            "/v1/agy/runs",
            json!({"model":MODEL,"prompt":"__fake_timeout__"}),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = value["id"].as_str().unwrap();
    let pid = wait_descendant(&harness.state.config.workspace_dir.join(id)).await;
    harness.state.stop();
    native::shutdown(&harness.state).await;
    assert_dead(pid).await;
    assert_eq!(
        harness.post("/v1/chat/completions", chat("hello")).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn native_provider_failure_has_one_terminal_error_in_the_common_event_schema() {
    let harness = Harness::new(|config| config.native_enabled = true).await;
    let (status, value) = harness
        .post(
            "/v1/agy/runs",
            json!({"model":MODEL,"prompt":"__fake_permission__"}),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = value["id"].as_str().unwrap();
    let result = harness.wait_job(id, "failed").await;
    assert_eq!(result["error"]["code"], "provider_permission_denied");
    let events = harness
        .send(
            Method::GET,
            &format!("/v1/agy/runs/{id}/events"),
            None,
            Some("test-key"),
        )
        .await;
    assert_eq!(events.status(), StatusCode::OK);
    assert_single_native_error(
        &frames(&read_bytes(events).await),
        "provider_permission_denied",
    );
}

#[tokio::test]
async fn native_replay_limit_records_the_same_failed_outcome_as_the_job() {
    let harness = Harness::new(|config| config.native_enabled = true).await;
    let (status, accepted) = harness
        .post(
            "/v1/agy/runs",
            json!({"model":MODEL,"prompt":"__fake_large_native__"}),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = accepted["id"].as_str().unwrap();
    let failed = harness.wait_job(id, "failed").await;
    assert_eq!(failed["error"]["code"], "output_limit_exceeded");
    assert!(
        harness.state.drain(Duration::from_secs(3)).await,
        "failed native admission did not drain"
    );
    // Runtime snapshots are updated by the admission-drop task; allow that
    // notification to catch up with the already-drained semaphore counters.
    for _ in 0..100 {
        let snapshot = harness.state.telemetry.snapshot().await;
        if snapshot.active == 0 && snapshot.queued == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let snapshot = harness.state.telemetry.snapshot().await;
    let matching: Vec<_> = snapshot
        .records
        .iter()
        .filter(|record| record.request_id == id)
        .collect();
    assert_eq!(matching.len(), 1);
    assert_eq!(matching[0].status, "failed");
    assert_eq!(
        matching[0].error_code.as_deref(),
        Some("output_limit_exceeded")
    );
    assert!(matching[0].usage_partial);
    assert_eq!(snapshot.active, 0);
    assert_eq!(snapshot.queued, 0);
}

#[tokio::test]
async fn post_auth_normalizer_failures_increment_unsampled_input_counts() {
    let harness = Harness::new(|_| {}).await;
    assert_eq!(
        harness
            .state
            .telemetry
            .snapshot()
            .await
            .counts_since_start
            .input,
        0
    );
    let mut input = chat("hello");
    input["temperature"] = json!(0.2);
    let (status, error) = harness.post("/v1/chat/completions", input).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error["error"]["code"], "unsupported_parameter");
    let mut input = response("hello");
    input["previous_response_id"] = json!("resp_unavailable");
    assert_eq!(
        harness.post("/v1/responses", input).await.0,
        StatusCode::BAD_REQUEST
    );
    let snapshot = harness.state.telemetry.snapshot().await;
    assert_eq!(snapshot.counts_since_start.input, 2);
    assert_eq!(snapshot.counts_since_start.total, 2);
    assert_eq!(snapshot.counts_since_start.auth, 0);
    assert!(harness.workspaces().is_empty());
}

#[tokio::test]
async fn native_creation_obeys_shared_queue_bound_before_acceptance() {
    let harness = Harness::new(|config| {
        config.native_enabled = true;
        config.max_concurrent = 1;
        config.max_queue = 1;
    })
    .await;
    let (status, value) = harness
        .post(
            "/v1/agy/runs",
            json!({"model":MODEL,"prompt":"__fake_timeout__"}),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let first_id = value["id"].as_str().unwrap();
    let pid = wait_descendant(&harness.state.config.workspace_dir.join(first_id)).await;
    let app = harness.app.clone();
    let waiting = tokio::spawn(async move {
        app.oneshot(Harness::request(
            Method::POST,
            "/v1/agy/runs",
            Some(json!({"model":MODEL,"prompt":"native queued fixture"})),
            Some("test-key"),
        ))
        .await
        .unwrap()
    });
    for _ in 0..100 {
        if harness.state.telemetry.snapshot().await.queued == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let snapshot = harness.state.telemetry.snapshot().await;
    assert_eq!(snapshot.active, 1);
    assert_eq!(snapshot.queued, 1);
    let (status, error) = harness
        .post(
            "/v1/agy/runs",
            json!({"model":MODEL,"prompt":"must not create a job"}),
        )
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(error["error"]["code"], "busy");
    assert_eq!(harness.workspaces().len(), 1);
    assert_eq!(
        harness
            .json(
                Method::DELETE,
                &format!("/v1/agy/runs/{first_id}"),
                None,
                Some("test-key")
            )
            .await
            .0,
        StatusCode::ACCEPTED
    );
    let response = tokio::time::timeout(Duration::from_secs(3), waiting)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let accepted = read_json(response).await;
    harness
        .wait_job(accepted["id"].as_str().unwrap(), "completed")
        .await;
    assert_dead(pid).await;
    for _ in 0..100 {
        let snapshot = harness.state.telemetry.snapshot().await;
        if snapshot.active == 0 && snapshot.queued == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("native queue/active counters leaked");
}

#[tokio::test]
async fn global_shutdown_cancels_an_active_model_stream_and_reports_terminal_error() {
    let harness = Harness::new(|_| {}).await;
    let mut body = chat("__fake_timeout__");
    body["stream"] = json!(true);
    let stream = harness
        .send(
            Method::POST,
            "/v1/chat/completions",
            Some(body),
            Some("test-key"),
        )
        .await;
    assert_eq!(stream.status(), StatusCode::OK);
    let id = stream.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let pid = wait_descendant(&harness.state.config.workspace_dir.join(&id)).await;
    harness.state.stop();
    let frames = frames(&read_bytes(stream).await);
    assert_eq!(frames.last().unwrap().data, "[DONE]");
    assert!(
        frames
            .iter()
            .filter(|frame| frame.data != "[DONE]")
            .map(event_json)
            .any(|value| value.pointer("/error/code") == Some(&json!("request_cancelled")))
    );
    assert_dead(pid).await;
    harness.wait_empty().await;
    assert!(harness.state.drain(Duration::from_secs(3)).await);
    let snapshot = harness.state.telemetry.snapshot().await;
    let record = snapshot
        .records
        .iter()
        .find(|record| record.request_id == id)
        .expect("shutdown drain must preserve the final request record");
    assert_eq!(record.status, "cancelled");
    assert_eq!(record.error_code.as_deref(), Some("request_cancelled"));
}
