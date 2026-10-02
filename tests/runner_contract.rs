use ai_router::{
    config::Config,
    protocol::{RunEvent, RunProfile, RunRequest, RunResult, ToolDefinition},
    runner::Runner,
};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-agy")
}
fn config(root: &Path) -> Config {
    let mut config = Config::for_test(root);
    config.agy_bin = fixture();
    config.request_timeout_secs = 3;
    config
}
fn runner(config: Config) -> Runner {
    Runner::with_relay_executable(
        Arc::new(config),
        PathBuf::from(env!("CARGO_BIN_EXE_ai-router")),
    )
}
fn request(prompt: &str) -> RunRequest {
    RunRequest {
        request_id: uuid::Uuid::new_v4().simple().to_string(),
        model: "gemini-3-pro".into(),
        prompt: prompt.into(),
        system: "Synthetic system\n---\nnot YAML".into(),
        tools: vec![],
        schema: None,
        profile: RunProfile::Model,
        effort: None,
        mode: None,
        files: vec![],
    }
}
async fn collect(runner: &Runner, request: RunRequest) -> Vec<RunEvent> {
    let mut stream = runner
        .start(request, CancellationToken::new())
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(event) = tokio::time::timeout(Duration::from_secs(10), stream.events.recv())
        .await
        .unwrap()
    {
        events.push(event);
    }
    events
}
fn completed(events: &[RunEvent]) -> &RunResult {
    events
        .iter()
        .find_map(|event| {
            if let RunEvent::Completed(result) = event {
                Some(result)
            } else {
                None
            }
        })
        .expect("completion")
}
fn error(events: &[RunEvent]) -> &str {
    events
        .iter()
        .find_map(|event| {
            if let RunEvent::Error(error) = event {
                Some(error.code.as_str())
            } else {
                None
            }
        })
        .expect("provider error")
}

#[tokio::test]
async fn provider_discovery_checks_pin_and_sanitizes_quota() {
    let root = tempfile::tempdir().unwrap();
    let mut config = config(root.path());
    let status = runner(config.clone()).probe().await;
    assert!(status.authenticated, "{:?}", status.error);
    assert_eq!(status.version, "1.2.15");
    assert_eq!(status.models.len(), 2);
    assert_eq!(
        status
            .quota
            .unwrap()
            .pointer("/groups/0/buckets/0/remaining_fraction"),
        Some(&json!(0.75))
    );
    config.expected_agy_version = "9.9.9".into();
    let status = runner(config).probe().await;
    assert!(!status.authenticated);
    assert_eq!(status.error.as_deref(), Some("provider_version_mismatch"));
}

#[tokio::test]
async fn text_deltas_result_only_and_schema_use_official_envelopes() {
    let root = tempfile::tempdir().unwrap();
    let runner = runner(config(root.path()));
    for prompt in ["plain text", "__fake_result_only__"] {
        let events = collect(&runner, request(prompt)).await;
        let text = events
            .iter()
            .filter_map(|event| {
                if let RunEvent::TextDelta { delta } = event {
                    Some(delta.as_str())
                } else {
                    None
                }
            })
            .collect::<String>();
        assert_eq!(text, completed(&events).text);
        assert_eq!(completed(&events).text, "Hello from fixture");
        assert_eq!(completed(&events).usage.as_ref().unwrap().total_tokens, 23);
        assert!(!completed(&events).usage_partial);
    }
    let mut input = request("schema");
    input.schema = Some(
        json!({"type":"object","properties":{"count":{"type":"integer"}},"required":["count"]}),
    );
    let events = collect(&runner, input).await;
    assert_eq!(
        completed(&events).structured_output,
        Some(json!({"count":1}))
    );
    let mut input = request("__fake_bad_schema__");
    input.schema = Some(
        json!({"type":"object","properties":{"count":{"type":"integer"}},"required":["count"]}),
    );
    let events = collect(&runner, input).await;
    assert_eq!(error(&events), "provider_schema_violation");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RunEvent::TextDelta { .. }))
    );
}

#[tokio::test]
async fn inert_mcp_handoff_and_followup_preserve_client_execution_boundary() {
    let root = tempfile::tempdir().unwrap();
    let runner = runner(config(root.path()));
    let tool = ToolDefinition {
        name: "weather".into(),
        description: "Weather for a city".into(),
        parameters: json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}),
    };
    let mut input = request("__fake_tool__");
    input.tools.push(tool.clone());
    let events = collect(&runner, input).await;
    let result = completed(&events);
    assert_eq!(result.tool_calls.len(), 1);
    assert_eq!(result.tool_calls[0].name, "weather");
    assert_eq!(result.tool_calls[0].arguments, json!({"city":"London"}));
    assert!(result.tool_calls[0].id.starts_with("call_"));
    assert!(result.usage_partial);
    assert_eq!(result.usage.as_ref().unwrap().total_tokens, 23);
    let mut followup = request(&format!(
        "[{{\"role\":\"assistant\",\"tool_calls\":[{{\"id\":\"{}\"}}]}},{{\"role\":\"tool\",\"content\":\"15 C\"}}]",
        result.tool_calls[0].id
    ));
    followup.tools.push(tool);
    let events = collect(&runner, followup).await;
    assert_eq!(completed(&events).text, "Fixture final answer");
    assert!(completed(&events).tool_calls.is_empty());
    let mut invalid = request("__fake_tool__");
    invalid.tools.push(ToolDefinition { name: "weather".into(), description: "A restricted test".into(), parameters: json!({"type":"object","properties":{"city":{"type":"string","enum":["Paris"]}},"required":["city"]}) });
    let events = collect(&runner, invalid).await;
    assert_eq!(error(&events), "invalid_provider_tool_call");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RunEvent::ToolCall(_)))
    );
}

#[tokio::test]
async fn provider_failures_have_safe_codes_and_one_terminal_event() {
    let root = tempfile::tempdir().unwrap();
    let runner = runner(config(root.path()));
    for (prompt, expected) in [
        ("__fake_auth__", "provider_auth_required"),
        ("__fake_quota__", "provider_quota_exhausted"),
        ("__fake_malformed__", "provider_protocol_error"),
        ("__fake_huge__", "provider_output_limit"),
        (
            "__fake_native_violation__",
            "provider_native_tool_violation",
        ),
        ("__fake_permission__", "provider_permission_denied"),
        ("__fake_stderr_permission__", "provider_permission_denied"),
        ("__fake_no_result__", "provider_protocol_error"),
    ] {
        let events = collect(&runner, request(prompt)).await;
        assert_eq!(error(&events), expected, "{prompt}");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RunEvent::Completed(_) | RunEvent::Error(_)))
                .count(),
            1
        );
        assert!(
            !serde_json::to_string(&events)
                .unwrap()
                .contains("sensitive diagnostic")
        );
    }
}

#[tokio::test]
async fn fallback_inventory_is_rejected_before_any_prompt_is_supplied() {
    let root = tempfile::tempdir().unwrap();
    let runner = runner(config(root.path()));
    let mut input = request("this input must never reach a fallback agent");
    input.model = "fake-fallback".into();
    let workspace = root.path().join("workspaces").join(&input.request_id);
    let events = collect(&runner, input).await;
    assert_eq!(error(&events), "provider_agent_not_loaded");
    assert!(!workspace.join("prompt-received.txt").exists());
    let mut input = request("expired authentication never receives a prompt");
    input.model = "fake-auth-before-init".into();
    let events = collect(&runner, input).await;
    assert_eq!(error(&events), "provider_auth_required");
}

#[cfg(unix)]
async fn assert_descendant_dead(workspace: &Path) {
    let pid: libc::pid_t = tokio::fs::read_to_string(workspace.join("descendant.pid"))
        .await
        .unwrap()
        .parse()
        .unwrap();
    for _ in 0..100 {
        let live = unsafe { libc::kill(pid, 0) == 0 };
        // Linux PID1 can leave a dead grandchild as a zombie. It cannot execute.
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
    panic!("descendant survived process-group cancellation");
}

#[tokio::test]
async fn deadline_cancellation_disconnect_and_completion_kill_descendants() {
    let root = tempfile::tempdir().unwrap();
    let mut config = config(root.path());
    config.request_timeout_secs = 1;
    let runner = runner(config);
    let input = request("__fake_timeout__");
    let workspace = root.path().join("workspaces").join(&input.request_id);
    let events = collect(&runner, input).await;
    assert_eq!(error(&events), "provider_timeout");
    #[cfg(unix)]
    assert_descendant_dead(&workspace).await;
    for disconnect in [false, true] {
        let cancel = CancellationToken::new();
        let mut stream = runner
            .start(request("__fake_timeout__"), cancel.clone())
            .await
            .unwrap();
        for _ in 0..100 {
            if tokio::fs::metadata(stream.workspace.join("descendant.pid"))
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let workspace = stream.workspace.clone();
        if disconnect {
            drop(stream.events);
            tokio::time::sleep(Duration::from_millis(100)).await;
        } else {
            cancel.cancel();
            let mut events = Vec::new();
            while let Some(event) = stream.events.recv().await {
                events.push(event);
            }
            assert_eq!(error(&events), "request_cancelled");
        }
        #[cfg(unix)]
        assert_descendant_dead(&workspace).await;
    }
    let input = request("__fake_descendant__");
    let workspace = root.path().join("workspaces").join(&input.request_id);
    let events = collect(&runner, input).await;
    assert_eq!(completed(&events).text, "Hello from fixture");
    #[cfg(unix)]
    assert_descendant_dead(&workspace).await;
}

#[tokio::test]
async fn native_profile_requires_gate_and_preserves_native_events() {
    let root = tempfile::tempdir().unwrap();
    let mut config = config(root.path());
    let mut input = request("native run");
    input.profile = RunProfile::Native;
    let error = match runner(config.clone())
        .start(input.clone(), CancellationToken::new())
        .await
    {
        Ok(_) => panic!("native unexpectedly enabled"),
        Err(error) => error,
    };
    assert_eq!(error.code, "native_unverified");
    config.native_enabled = true;
    let events = collect(&runner(config), input).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RunEvent::Native(value) if value["event"] == "result"))
    );
    assert_eq!(completed(&events).text, "Hello from fixture");
}
