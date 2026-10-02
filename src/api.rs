use crate::{
    auth::{self, RateLimiter},
    config::{Config, KeyConfig},
    monitor::{RequestRecord, Telemetry},
    native::{self, RunStore},
    protocol::*,
    runner::Runner,
};
use axum::{
    Extension, Json, Router,
    extract::{ConnectInfo, DefaultBodyLimit, State},
    http::{HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use chrono::Utc;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    convert::Infallible,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Notify, OwnedSemaphorePermit, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub struct AppState {
    pub config: Arc<Config>,
    pub runner: Runner,
    pub telemetry: Arc<Telemetry>,
    pub provider: RwLock<ProviderStatus>,
    pub runs: RunStore,
    pub shutdown: CancellationToken,
    slots: Arc<Semaphore>,
    body_slots: Arc<Semaphore>,
    heavy_slots: Arc<Semaphore>,
    active: AtomicUsize,
    queued: AtomicUsize,
    drained: Notify,
    preauth: RateLimiter,
    global: RateLimiter,
    clients: RateLimiter,
    rejection_logs: RateLimiter,
}

impl AppState {
    pub fn new(config: Arc<Config>, runner: Runner, telemetry: Arc<Telemetry>) -> Arc<Self> {
        Arc::new(Self {
            slots: Arc::new(Semaphore::new(config.max_concurrent)),
            body_slots: Arc::new(Semaphore::new(32)),
            heavy_slots: Arc::new(Semaphore::new(8)),
            preauth: RateLimiter::new(config.preauth_per_minute, 4096),
            global: RateLimiter::new(config.global_preauth_per_minute, 1),
            clients: RateLimiter::new(config.per_key_per_minute, config.keys.len().max(1)),
            rejection_logs: RateLimiter::new(30, 1),
            config,
            runner,
            telemetry,
            provider: RwLock::new(ProviderStatus::default()),
            runs: RunStore::default(),
            shutdown: CancellationToken::new(),
            active: AtomicUsize::new(0),
            queued: AtomicUsize::new(0),
            drained: Notify::new(),
        })
    }

    pub async fn refresh_provider(&self) {
        let status = self.runner.probe().await;
        self.telemetry.set_provider(status.clone()).await;
        *self.provider.write().await = status;
    }

    pub async fn require_model(&self, model: &str) -> Result<(), ApiError> {
        let status = self.provider.read().await;
        if !status.authenticated {
            return Err(error(
                503,
                "provider_unavailable",
                "AGY is not ready; operator authentication may be required",
            ));
        }
        if !status.models.iter().any(|m| m.id == model) {
            return Err(error(
                400,
                "model_not_found",
                "Choose a model returned by /v1/models",
            ));
        }
        Ok(())
    }

    async fn runtime(&self) {
        self.telemetry
            .set_runtime(
                self.active.load(Ordering::Relaxed),
                self.queued.load(Ordering::Relaxed),
            )
            .await;
    }

    pub async fn admit(self: &Arc<Self>) -> Result<Admission, ApiError> {
        if self.shutdown.is_cancelled() {
            return Err(error(503, "shutting_down", "Service is shutting down"));
        }
        let mut queued_guard = None;
        let permit = match self.slots.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                self.queued
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                        (n < self.config.max_queue).then_some(n + 1)
                    })
                    .map_err(|_| error(429, "busy", "The request queue is full"))?;
                queued_guard = Some(QueueGuard(self.clone()));
                self.runtime().await;
                let wait = Duration::from_secs(self.config.request_timeout_secs.min(30));
                let result = tokio::time::timeout(wait, self.slots.clone().acquire_owned()).await;
                result
                    .map_err(|_| error(429, "busy", "The request queue wait expired"))?
                    .map_err(|_| error(503, "shutting_down", "Service is shutting down"))?
            }
        };
        if self.shutdown.is_cancelled() {
            return Err(error(503, "shutting_down", "Service is shutting down"));
        }
        self.active.fetch_add(1, Ordering::SeqCst);
        let admission = Admission {
            state: self.clone(),
            _permit: permit,
        };
        drop(queued_guard);
        self.runtime().await;
        Ok(admission)
    }

    pub fn stop(&self) {
        self.shutdown.cancel();
        self.slots.close();
    }

    /// Admission is released only after the final record and owned workspace
    /// cleanup. Wait for those tasks before flushing during shutdown.
    pub async fn drain(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            // During shutdown a queued admission moves to active before it
            // leaves queued. Read queued first so this one-way transition
            // cannot appear as two separate zero values.
            if self.queued.load(Ordering::SeqCst) == 0 && self.active.load(Ordering::SeqCst) == 0 {
                self.runtime().await;
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return false;
            }
        }
    }
}

struct QueueGuard(Arc<AppState>);
impl Drop for QueueGuard {
    fn drop(&mut self) {
        self.0.queued.fetch_sub(1, Ordering::SeqCst);
        self.0.drained.notify_waiters();
        let state = self.0.clone();
        tokio::spawn(async move {
            state.runtime().await;
        });
    }
}
pub struct Admission {
    state: Arc<AppState>,
    _permit: OwnedSemaphorePermit,
}
impl Drop for Admission {
    fn drop(&mut self) {
        self.state.active.fetch_sub(1, Ordering::SeqCst);
        self.state.drained.notify_waiters();
        let state = self.state.clone();
        tokio::spawn(async move {
            state.runtime().await;
        });
    }
}

pub fn error(status: u16, code: &str, message: &str) -> ApiError {
    ApiError {
        status,
        code: code.into(),
        message: message.into(),
    }
}

pub struct RequestContext {
    pub id: String,
    pub started: Instant,
    pub recorded: AtomicBool,
}

fn hold_response(response: Response, permit: OwnedSemaphorePermit) -> Response {
    let (parts, body) = response.into_parts();
    let stream = async_stream::stream! {
        let _permit=permit;let mut input=body.into_data_stream();
        use futures_util::StreamExt;
        while let Some(chunk)=input.next().await {yield chunk;}
    };
    Response::from_parts(parts, axum::body::Body::from_stream(stream))
}

pub fn router(state: Arc<AppState>) -> Router {
    let protected = Router::new()
        .route("/v1/models", get(models))
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/chat/completions", post(chat))
        .route("/v1/responses", post(responses))
        .route("/v1/agy/runs", post(native::create))
        .route(
            "/v1/agy/runs/{id}",
            get(native::status).delete(native::cancel),
        )
        .route("/v1/agy/runs/{id}/events", get(native::events))
        .route("/v1/agy/runs/{id}/artifacts", get(native::artifacts))
        .route("/v1/agy/runs/{id}/artifacts/{*path}", get(native::download))
        .fallback(unsupported)
        .layer(middleware::from_fn_with_state(state.clone(), protect));
    Router::new()
        .route("/health", get(|| async { Json(json!({"status":"ok"})) }))
        .route("/ready", get(ready))
        .merge(protected)
        .layer(DefaultBodyLimit::max(state.config.max_body_bytes))
        .with_state(state)
}

async fn ready(State(state): State<Arc<AppState>>) -> Response {
    let available = state.provider.read().await.authenticated;
    (
        if available {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(json!({"ready":available})),
    )
        .into_response()
}

fn client_ip(headers: &HeaderMap, peer: IpAddr, config: &Config) -> IpAddr {
    if !config.trusted_proxies.contains(&peer) {
        return peer;
    }
    // The configured proxy must overwrite this single header. Never trust XFF chains.
    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(peer)
}

async fn protect(
    State(state): State<Arc<AppState>>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|p| p.0.ip())
        .unwrap_or(IpAddr::from([127, 0, 0, 1]));
    let ip = client_ip(request.headers(), peer, &state.config);
    let rejection = if !state.global.check("all") || !state.preauth.check(&ip.to_string()) {
        Some(error(429, "rate_limit_exceeded", "Request limit exceeded"))
    } else if request
        .headers()
        .iter()
        .map(|(k, v)| k.as_str().len() + v.as_bytes().len())
        .sum::<usize>()
        > 16_384
    {
        Some(error(
            431,
            "headers_too_large",
            "Request headers are too large",
        ))
    } else if request
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok())
        .is_some_and(|n| n > state.config.max_body_bytes)
    {
        Some(error(413, "request_too_large", "Request body is too large"))
    } else {
        None
    };
    if let Some(e) = rejection {
        return rejection_response(&state, e).await;
    }
    let key = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .filter(|s| s.len() <= 512)
        .and_then(|s| s.strip_prefix("Bearer "))
        .and_then(|bearer| auth::authenticate(&state.config.keys, bearer));
    let Some(key) = key else {
        return rejection_response(&state, error(401, "invalid_api_key", "Invalid API key")).await;
    };
    if !state.clients.check(&key.id) {
        return rejection_response(
            &state,
            error(429, "rate_limit_exceeded", "Request limit exceeded"),
        )
        .await;
    }
    let scope = if request.uri().path().starts_with("/v1/agy/") {
        "native"
    } else {
        "model"
    };
    if !key.scopes.iter().any(|s| s == scope) {
        return rejection_response(
            &state,
            error(
                403,
                "insufficient_scope",
                "API key does not permit this operation",
            ),
        )
        .await;
    }
    if request.method() == axum::http::Method::POST {
        let Ok(_permit) = state.body_slots.clone().try_acquire_owned() else {
            return rejection_response(
                &state,
                error(429, "busy", "Too many request bodies in progress"),
            )
            .await;
        };
        let (parts, body) = request.into_parts();
        let bytes = match tokio::time::timeout(
            Duration::from_secs(10),
            axum::body::to_bytes(body, state.config.max_body_bytes),
        )
        .await
        {
            Err(_) => {
                return rejection_response(
                    &state,
                    error(408, "body_timeout", "Request body deadline exceeded"),
                )
                .await;
            }
            Ok(Err(_)) => {
                return rejection_response(
                    &state,
                    error(
                        413,
                        "request_too_large",
                        "Request body exceeds the configured bound",
                    ),
                )
                .await;
            }
            Ok(Ok(bytes)) => bytes,
        };
        request = Request::from_parts(parts, axum::body::Body::from(bytes));
    }
    let heavy = if request.method() == axum::http::Method::GET
        && request.uri().path().starts_with("/v1/agy/")
    {
        match state.heavy_slots.clone().try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                return rejection_response(
                    &state,
                    error(429, "busy", "Too many native response streams"),
                )
                .await;
            }
        }
    } else {
        None
    };
    let context = Arc::new(RequestContext {
        id: Uuid::new_v4().to_string(),
        started: Instant::now(),
        recorded: AtomicBool::new(false),
    });
    let client_id = key.id.clone();
    let endpoint = if request.uri().path().starts_with("/v1/chat/") {
        "chat"
    } else if request.uri().path() == "/v1/responses" {
        "responses"
    } else if request.uri().path().starts_with("/v1/agy/") {
        "native"
    } else {
        "api"
    };
    request.extensions_mut().insert(key);
    request.extensions_mut().insert(context.clone());
    let mut response = next.run(request).await;
    if response.status().as_u16() >= 400 && !context.recorded.load(Ordering::Relaxed) {
        state
            .telemetry
            .note_rejection(&format!("http_{}", response.status().as_u16()));
        state
            .telemetry
            .record(RequestRecord {
                request_id: context.id.clone(),
                client_id,
                model: String::new(),
                endpoint: endpoint.into(),
                started_at: Utc::now().to_rfc3339(),
                duration_ms: context.started.elapsed().as_millis() as u64,
                startup_ms: None,
                first_output_ms: None,
                status: "rejected".into(),
                error_code: Some(format!("http_{}", response.status().as_u16())),
                usage: None,
                usage_partial: false,
            })
            .await;
    }
    response
        .headers_mut()
        .insert("x-request-id", context.id.parse().unwrap());
    if response.status() == StatusCode::TOO_MANY_REQUESTS {
        response
            .headers_mut()
            .insert("retry-after", "10".parse().unwrap());
    }
    if let Some(permit) = heavy {
        hold_response(response, permit)
    } else {
        response
    }
}

async fn rejection_response(state: &AppState, e: ApiError) -> Response {
    state.telemetry.note_rejection(&e.code);
    if state.rejection_logs.check("all") {
        state
            .telemetry
            .record(RequestRecord {
                request_id: Uuid::new_v4().to_string(),
                client_id: "rejected".into(),
                model: String::new(),
                endpoint: "authentication".into(),
                started_at: Utc::now().to_rfc3339(),
                duration_ms: 0,
                startup_ms: None,
                first_output_ms: None,
                status: "rejected".into(),
                error_code: Some(e.code.clone()),
                usage: None,
                usage_partial: false,
            })
            .await;
    }
    let limited = e.status == 429;
    let mut response = e.into_response();
    if limited {
        response
            .headers_mut()
            .insert("retry-after", "10".parse().unwrap());
    }
    response
}

async fn unsupported() -> ApiError {
    error(
        404,
        "unsupported_endpoint",
        "This endpoint is not supported by the configured CLI adapter",
    )
}

async fn models(State(state): State<Arc<AppState>>) -> Json<Value> {
    let provider = state.provider.read().await;
    Json(
        json!({"object":"list", "data":provider.models.iter().map(|m| json!({"id":m.id,"object":"model","created":0,"owned_by":"antigravity"})).collect::<Vec<_>>()}),
    )
}

async fn capabilities(State(state): State<Arc<AppState>>) -> Json<Value> {
    let provider = state.provider.read().await;
    Json(
        json!({"provider":"agy", "cli_version":provider.version, "expected_cli_version":state.config.expected_agy_version, "provider_ready":provider.authenticated,
        "profiles":{"model":{"chat_completions":true,"responses":true,"streaming":true,"text":true,"json_schema":true,"client_tools":true,"max_tool_calls_per_turn":1,"parallel_tool_execution":false,"media_input":false},
                    "native":{"enabled":state.config.native_enabled,"requires_operator_sandbox_verification":true,"runs":true,"uploaded_files":true,"artifacts":true,"continuation":false}},
        "unsupported":["sampling_controls","forced_tool_choice","embeddings","audio","image_generation_api","realtime","batches","fine_tuning","provider_management"],
        "verification":{"adapter_contract":"automated_fixtures","real_model_and_client_loops":"operator_gate","native_sandbox":"operator_gate"},
        "models":provider.models,"per_model":provider.models.iter().map(|m|json!({"id":m.id,"cli_version":provider.version,"model_profile_verification":"operator_gate","native_profile_verification":"operator_gate"})).collect::<Vec<_>>(),"quota":provider.quota,"checked_at":provider.checked_at}),
    )
}

async fn chat(
    State(state): State<Arc<AppState>>,
    Extension(key): Extension<KeyConfig>,
    Extension(context): Extension<Arc<RequestContext>>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    generate(state, key, context, body, false).await
}
async fn responses(
    State(state): State<Arc<AppState>>,
    Extension(key): Extension<KeyConfig>,
    Extension(context): Extension<Arc<RequestContext>>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    generate(state, key, context, body, true).await
}

async fn generate(
    state: Arc<AppState>,
    key: KeyConfig,
    context: Arc<RequestContext>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
    responses: bool,
) -> Result<Response, ApiError> {
    let Json(body) = body.map_err(|e| {
        error(
            e.status().as_u16(),
            "invalid_request",
            "Expected a bounded JSON request body",
        )
    })?;
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let include_usage = body
        .pointer("/stream_options/include_usage")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let id = context.id.clone();
    let request = if responses {
        normalize_response(body, id.clone())?
    } else {
        normalize_chat(body, id.clone())?
    };
    state.require_model(&request.model).await?;
    let started = Instant::now();
    let admission = state.admit().await?;
    let cancel = state.shutdown.child_token();
    let model = request.model.clone();
    let mut run = state.runner.start(request, cancel.clone()).await?;
    context.recorded.store(true, Ordering::Relaxed);
    let mut guard = CompletionGuard::new(
        state.clone(),
        key.id,
        model.clone(),
        if responses { "responses" } else { "chat" },
        id.clone(),
        started,
        run.workspace.clone(),
        run.workspace_owned.clone(),
        cancel,
        run.finished.clone(),
        admission,
    );
    let completion_id = format!("{}{}", if responses { "resp_" } else { "chatcmpl_" }, id);
    let created = Utc::now().timestamp();
    if !streaming {
        while let Some(event) = run.events.recv().await {
            guard.observe(&event);
            match event {
                RunEvent::Completed(result) => {
                    let output = if responses {
                        response_object(&completion_id, created, &model, &result)
                    } else {
                        chat_object(&completion_id, created, &model, &result)
                    };
                    return Ok(Json(output).into_response());
                }
                RunEvent::Error(e) => return Err(e),
                _ => {}
            }
        }
        let failure = error(
            502,
            "incomplete_provider_output",
            "AGY ended without a result",
        );
        guard.observe(&RunEvent::Error(failure.clone()));
        return Err(failure);
    }
    let stream = async_stream::stream! {
        let mut guard = guard;
        let mut render = StreamRenderer::new(responses, completion_id, model, created, include_usage);
        for item in render.begin() { yield Ok::<Event,Infallible>(item); }
        let mut terminal = false;
        while let Some(event) = run.events.recv().await {
            guard.observe(&event);
            terminal = matches!(&event, RunEvent::Completed(_) | RunEvent::Error(_));
            for item in render.event(event) { yield Ok(item); }
            if terminal { break; }
        }
        if !terminal {let failure=RunEvent::Error(error(502,"incomplete_provider_output","AGY ended without a result"));guard.observe(&failure);for item in render.event(failure) {yield Ok(item);}}
    };
    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response();
    response
        .headers_mut()
        .insert("x-accel-buffering", "no".parse().unwrap());
    response
        .headers_mut()
        .insert("cache-control", "no-cache, no-transform".parse().unwrap());
    Ok(response)
}

pub fn chat_usage(usage: &Option<Usage>) -> Value {
    usage.as_ref().map(|u| json!({"prompt_tokens":u.input_tokens,"completion_tokens":u.output_tokens,"total_tokens":u.total_tokens,"prompt_tokens_details":{"cached_tokens":u.cache_read_tokens},"completion_tokens_details":{"reasoning_tokens":u.thinking_tokens}})).unwrap_or(Value::Null)
}
pub fn response_usage(usage: &Option<Usage>) -> Value {
    usage.as_ref().map(|u| json!({"input_tokens":u.input_tokens,"output_tokens":u.output_tokens,"total_tokens":u.total_tokens,"input_tokens_details":{"cached_tokens":u.cache_read_tokens},"output_tokens_details":{"reasoning_tokens":u.thinking_tokens}})).unwrap_or(Value::Null)
}
fn tool_json(tool: &ToolCall) -> Value {
    json!({"id":tool.id,"type":"function","function":{"name":tool.name,"arguments":tool.arguments.to_string()}})
}
pub fn chat_object(id: &str, created: i64, model: &str, result: &RunResult) -> Value {
    let mut message = json!({"role":"assistant","content":if result.text.is_empty() && !result.tool_calls.is_empty() {Value::Null} else {Value::String(result.text.clone())}});
    if !result.tool_calls.is_empty() {
        message["tool_calls"] = json!(result.tool_calls.iter().map(tool_json).collect::<Vec<_>>());
    }
    json!({"id":id,"object":"chat.completion","created":created,"model":model,"choices":[{"index":0,"message":message,"finish_reason":if result.tool_calls.is_empty(){"stop"}else{"tool_calls"}}],"usage":chat_usage(&result.usage),"agy_usage_partial":result.usage_partial})
}
fn response_tool(tool: &ToolCall) -> Value {
    json!({"id":format!("fc_{}",tool.id),"type":"function_call","status":"completed","call_id":tool.id,"name":tool.name,"arguments":tool.arguments.to_string()})
}
pub fn response_object(id: &str, created: i64, model: &str, result: &RunResult) -> Value {
    let mut output = Vec::new();
    if !result.text.is_empty() || result.tool_calls.is_empty() {
        output.push(json!({"id":format!("msg_{id}"),"type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":result.text,"annotations":[]}]}));
    }
    output.extend(result.tool_calls.iter().map(response_tool));
    json!({"id":id,"object":"response","created_at":created,"status":"completed","error":null,"incomplete_details":null,"model":model,"output":output,"usage":response_usage(&result.usage),"parallel_tool_calls":false,"store":false,"agy_usage_partial":result.usage_partial})
}

pub struct CompletionGuard {
    state: Arc<AppState>,
    record: RequestRecord,
    started: Instant,
    workspace: Option<PathBuf>,
    workspace_owned: Arc<AtomicBool>,
    cancel: CancellationToken,
    finished: CancellationToken,
    admission: Option<Admission>,
}
impl CompletionGuard {
    pub fn new(
        state: Arc<AppState>,
        client_id: String,
        model: String,
        endpoint: &str,
        request_id: String,
        started: Instant,
        workspace: PathBuf,
        workspace_owned: Arc<AtomicBool>,
        cancel: CancellationToken,
        finished: CancellationToken,
        admission: Admission,
    ) -> Self {
        Self {
            state,
            record: RequestRecord {
                request_id,
                client_id,
                model,
                endpoint: endpoint.into(),
                started_at: Utc::now().to_rfc3339(),
                duration_ms: 0,
                startup_ms: None,
                first_output_ms: None,
                status: "cancelled".into(),
                error_code: Some("client_disconnected".into()),
                usage: None,
                usage_partial: true,
            },
            started,
            workspace: Some(workspace),
            workspace_owned,
            cancel,
            finished,
            admission: Some(admission),
        }
    }
    pub fn preserve_workspace(&mut self) {
        self.workspace = None;
    }
    pub fn observe(&mut self, event: &RunEvent) {
        if matches!(event, RunEvent::Init { .. }) && self.record.startup_ms.is_none() {
            self.record.startup_ms = Some(self.started.elapsed().as_millis() as u64);
        }
        if matches!(event, RunEvent::TextDelta { .. } | RunEvent::ToolCall(_))
            && self.record.first_output_ms.is_none()
        {
            self.record.first_output_ms = Some(self.started.elapsed().as_millis() as u64);
        }
        match event {
            RunEvent::Completed(result) => {
                self.record.status = "success".into();
                self.record.error_code = None;
                self.record.usage = result.usage.clone();
                self.record.usage_partial = result.usage_partial;
            }
            RunEvent::Error(e) => {
                self.record.status = if e.code == "request_cancelled" {
                    "cancelled"
                } else {
                    "failed"
                }
                .into();
                self.record.error_code = Some(e.code.clone());
                self.record.usage_partial = true;
            }
            _ => {}
        }
    }
}
impl Drop for CompletionGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.record.duration_ms = self.started.elapsed().as_millis() as u64;
        let record = self.record.clone();
        let state = self.state.clone();
        let workspace = self.workspace.take();
        let workspace_owned = self.workspace_owned.clone();
        let finished = self.finished.clone();
        let admission = self.admission.take();
        tokio::spawn(async move {
            let stopped = tokio::time::timeout(Duration::from_secs(30), finished.cancelled())
                .await
                .is_ok();
            state.telemetry.record(record).await;
            if stopped && workspace_owned.load(Ordering::Acquire) {
                if let Some(workspace) = workspace {
                    if tokio::fs::remove_dir_all(workspace).await.is_ok() {
                        workspace_owned.store(false, Ordering::Release);
                    }
                }
            }
            drop(admission);
        });
    }
}

struct StreamRenderer {
    responses: bool,
    id: String,
    model: String,
    created: i64,
    include_usage: bool,
    sequence: u64,
    text: String,
    text_started: bool,
    tools: HashSet<String>,
    tool_order: Vec<ToolCall>,
}
impl StreamRenderer {
    fn new(responses: bool, id: String, model: String, created: i64, include_usage: bool) -> Self {
        Self {
            responses,
            id,
            model,
            created,
            include_usage,
            sequence: 0,
            text: String::new(),
            text_started: false,
            tools: HashSet::new(),
            tool_order: Vec::new(),
        }
    }
    fn emit(&mut self, kind: &str, mut value: Value) -> Event {
        if self.responses {
            value["type"] = json!(kind);
            value["sequence_number"] = json!(self.sequence);
            self.sequence += 1;
            Event::default().event(kind).data(value.to_string())
        } else {
            Event::default().data(value.to_string())
        }
    }
    fn chunk(&mut self, delta: Value, reason: Value) -> Event {
        let value = json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,"choices":[{"index":0,"delta":delta,"finish_reason":reason}]});
        self.emit("", value)
    }
    fn begin(&mut self) -> Vec<Event> {
        if !self.responses {
            return vec![self.chunk(json!({"role":"assistant","content":""}), Value::Null)];
        }
        let response = json!({"id":self.id,"object":"response","created_at":self.created,"model":self.model,"status":"in_progress","output":[],"error":null,"incomplete_details":null,"usage":null});
        vec![
            self.emit("response.created", json!({"response":response})),
            self.emit("response.in_progress", json!({"response":response})),
        ]
    }
    fn text_delta(&mut self, delta: String) -> Vec<Event> {
        if delta.is_empty() {
            return vec![];
        }
        let mut events = Vec::new();
        if self.responses && !self.text_started {
            let item = json!({"id":format!("msg_{}",self.id),"type":"message","status":"in_progress","role":"assistant","content":[]});
            events.push(self.emit(
                "response.output_item.added",
                json!({"output_index":0,"item":item}),
            ));
            events.push(self.emit("response.content_part.added",json!({"output_index":0,"item_id":format!("msg_{}",self.id),"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}})));
        }
        self.text_started = true;
        self.text.push_str(&delta);
        events.push(if self.responses {self.emit("response.output_text.delta",json!({"output_index":0,"item_id":format!("msg_{}",self.id),"content_index":0,"delta":delta}))} else {self.chunk(json!({"content":delta}),Value::Null)});
        events
    }
    fn tool(&mut self, tool: ToolCall) -> Vec<Event> {
        if !self.tools.insert(tool.id.clone()) {
            return vec![];
        }
        let index = self.tool_order.len();
        self.tool_order.push(tool.clone());
        if !self.responses {
            return vec![self.chunk(json!({"tool_calls":[{"index":index,"id":tool.id,"type":"function","function":{"name":tool.name,"arguments":tool.arguments.to_string()}}]}),Value::Null)];
        }
        let output_index = index + usize::from(self.text_started);
        let mut item = response_tool(&tool);
        item["status"] = json!("in_progress");
        item["arguments"] = json!("");
        vec![self.emit("response.output_item.added",json!({"output_index":output_index,"item":item})),
            self.emit("response.function_call_arguments.delta",json!({"output_index":output_index,"item_id":format!("fc_{}",tool.id),"delta":tool.arguments.to_string()})),
            self.emit("response.function_call_arguments.done",json!({"output_index":output_index,"item_id":format!("fc_{}",tool.id),"arguments":tool.arguments.to_string()})),
            self.emit("response.output_item.done",json!({"output_index":output_index,"item":response_tool(&tool)}))]
    }
    fn event(&mut self, event: RunEvent) -> Vec<Event> {
        match event {
            RunEvent::TextDelta { delta } => self.text_delta(delta),
            RunEvent::ToolCall(tool) => self.tool(tool),
            RunEvent::Completed(result) => {
                let mut events = Vec::new();
                if self.text.is_empty() && !result.text.is_empty() {
                    events.extend(self.text_delta(result.text.clone()));
                }
                for tool in &result.tool_calls {
                    events.extend(self.tool(tool.clone()));
                }
                if self.responses {
                    if self.text_started {
                        events.push(self.emit("response.output_text.done",json!({"output_index":0,"item_id":format!("msg_{}",self.id),"content_index":0,"text":self.text})));
                        events.push(self.emit("response.content_part.done",json!({"output_index":0,"item_id":format!("msg_{}",self.id),"content_index":0,"part":{"type":"output_text","text":self.text,"annotations":[]}})));
                        let item = json!({"id":format!("msg_{}",self.id),"type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":self.text,"annotations":[]}]});
                        events.push(self.emit(
                            "response.output_item.done",
                            json!({"output_index":0,"item":item}),
                        ));
                    }
                    events.push(self.emit("response.completed",json!({"response":response_object(&self.id,self.created,&self.model,&result)})));
                } else {
                    events.push(self.chunk(
                        json!({}),
                        json!(if result.tool_calls.is_empty() {
                            "stop"
                        } else {
                            "tool_calls"
                        }),
                    ));
                    if self.include_usage {
                        events.push(self.emit("",json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,"choices":[],"usage":chat_usage(&result.usage),"agy_usage_partial":result.usage_partial})));
                    }
                    events.push(Event::default().data("[DONE]"));
                }
                events
            }
            RunEvent::Error(e) => {
                if self.responses {
                    vec![self.emit("response.failed",json!({"response":{"id":self.id,"object":"response","created_at":self.created,"model":self.model,"status":"failed","output":[],"error":{"code":e.code,"message":e.message},"usage":null}}))]
                } else {
                    vec![self.emit("error",json!({"error":{"code":e.code,"message":e.message,"type":"provider_error"}})),Event::default().data("[DONE]")]
                }
            }
            _ => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn forwarded_header_needs_explicit_peer_trust() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::for_test(temp.path());
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", "203.0.113.9".parse().unwrap());
        let peer = "127.0.0.1".parse().unwrap();
        assert_eq!(client_ip(&headers, peer, &config), peer);
        config.trusted_proxies.push(peer);
        assert_eq!(
            client_ip(&headers, peer, &config),
            "203.0.113.9".parse::<IpAddr>().unwrap()
        );
        headers.insert("x-real-ip", "203.0.113.9, 198.51.100.1".parse().unwrap());
        assert_eq!(client_ip(&headers, peer, &config), peer);
    }
    #[test]
    fn missing_usage_is_null_not_fabricated_zero() {
        assert!(chat_usage(&None).is_null());
        assert!(response_usage(&None).is_null());
    }
}
