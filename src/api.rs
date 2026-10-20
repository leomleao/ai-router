use crate::{
    auth::{self, RateLimiter},
    config::{Config, KeyConfig},
    items::{self, ItemStore},
    monitor::{CapturedPayload, RequestRecord, Telemetry},
    native::{self, RunStore},
    openai::OpenAi,
    protocol::*,
    providers::{self, ProviderId, Registry, Snapshot},
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
    pub openai: OpenAi,
    pub openai_provider: RwLock<ProviderStatus>,
    registry: std::sync::Mutex<Registry>,
    pub runs: RunStore,
    items: ItemStore,
    pub shutdown: CancellationToken,
    slots: Arc<Semaphore>,
    openai_slots: Arc<Semaphore>,
    openai_queued: AtomicUsize,
    agy_queued: AtomicUsize,
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
            openai_slots: Arc::new(Semaphore::new(config.openai_max_concurrent)),
            openai_queued: AtomicUsize::new(0),
            agy_queued: AtomicUsize::new(0),
            openai: OpenAi::new(config.clone()),
            openai_provider: RwLock::new(ProviderStatus::default()),
            registry: std::sync::Mutex::new(Registry::default()),
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
            items: ItemStore::default(),
            shutdown: CancellationToken::new(),
            active: AtomicUsize::new(0),
            queued: AtomicUsize::new(0),
            drained: Notify::new(),
        })
    }

    pub async fn refresh_agy(&self) {
        self.items.prune();
        let status = tokio::time::timeout(Duration::from_secs(10), self.runner.probe())
            .await
            .unwrap_or_else(|_| ProviderStatus {
                checked_at: Utc::now().to_rfc3339(),
                error: Some("probe_timeout".into()),
                ..Default::default()
            });
        self.registry
            .lock()
            .unwrap()
            .remember(ProviderId::Agy, &status);
        self.telemetry.set_provider(status.clone()).await;
        *self.provider.write().await = status;
    }

    pub async fn refresh_openai(&self) {
        if self.config.openai_enabled {
            let status = tokio::time::timeout(Duration::from_secs(10), self.openai.probe())
                .await
                .unwrap_or_else(|_| ProviderStatus {
                    checked_at: Utc::now().to_rfc3339(),
                    error: Some("probe_timeout".into()),
                    ..Default::default()
                });
            self.registry
                .lock()
                .unwrap()
                .remember(ProviderId::OpenAi, &status);
            self.telemetry.set_openai_provider(status.clone()).await;
            *self.openai_provider.write().await = status;
        }
    }

    pub async fn refresh_provider(&self) {
        // Each future publishes its result as soon as its own probe finishes.
        tokio::join!(self.refresh_agy(), self.refresh_openai());
    }

    async fn snapshot(&self) -> Snapshot {
        // The registration generation travels with its catalogue. Read that
        // pair before inspecting current authorization so a newly discovered
        // account cannot make the prior account's catalogue appear ready.
        let mut openai = self.openai_provider.read().await.clone();
        if self.config.openai_enabled {
            let available = tokio::time::timeout(Duration::from_millis(100), async {
                if self.openai.availability_error().await.is_some() {
                    return false;
                }
                self.openai.generation().await.is_ok_and(|generation| {
                    openai.registration_generation.as_ref() == Some(&generation)
                })
            })
            .await
            .unwrap_or(false);
            if !available {
                openai.authenticated = false;
            }
        }
        let agy = self.provider.read().await;
        self.registry.lock().unwrap().snapshot(
            &agy,
            &openai,
            self.config.openai_enabled,
            Utc::now(),
        )
    }

    async fn resolve_model(&self, model: &str) -> Result<ProviderId, ApiError> {
        let snapshot = self.snapshot().await;
        let resolved = snapshot.resolve(model);
        if resolved
            .as_ref()
            .is_err_and(|e| e.code == "provider_unavailable")
            && self.config.openai_enabled
            && snapshot.owner(model) == Some(ProviderId::OpenAi)
        {
            // Preserve the provider's actionable quota/auth reason for cached IDs.
            if let Some(error) = self.openai.availability_error().await {
                return Err(error);
            }
        }
        resolved
    }

    pub async fn require_model(&self, model: &str) -> Result<(), ApiError> {
        let status = self.provider.read().await;
        if !providers::ready(&status, Utc::now()) {
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
        self.admit_provider(ProviderId::Agy).await
    }

    async fn admit_provider(self: &Arc<Self>, provider: ProviderId) -> Result<Admission, ApiError> {
        let (slots, max_queue, provider_queued) = match provider {
            ProviderId::Agy => (&self.slots, self.config.max_queue, None),
            ProviderId::OpenAi => (
                &self.openai_slots,
                self.config.openai_max_queue,
                Some(&self.openai_queued),
            ),
        };
        if self.shutdown.is_cancelled() {
            return Err(error(503, "shutting_down", "Service is shutting down"));
        }
        let mut queued_guard = None;
        let permit = match slots.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                // Provider queues consume no execution slots belonging to the other provider.
                let counter = provider_queued.unwrap_or(&self.agy_queued);
                counter
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                        (n < max_queue).then_some(n + 1)
                    })
                    .map_err(|_| error(429, "busy", "The provider request queue is full"))?;
                self.queued.fetch_add(1, Ordering::SeqCst);
                queued_guard = Some(QueueGuard(self.clone(), provider));
                self.runtime().await;
                let wait = Duration::from_secs(self.config.request_timeout_secs.min(30));
                let result = tokio::time::timeout(wait, slots.clone().acquire_owned()).await;
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
        self.openai_slots.close();
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

struct QueueGuard(Arc<AppState>, ProviderId);
impl Drop for QueueGuard {
    fn drop(&mut self) {
        self.0.queued.fetch_sub(1, Ordering::SeqCst);
        match self.1 {
            ProviderId::Agy => &self.0.agy_queued,
            ProviderId::OpenAi => &self.0.openai_queued,
        }
        .fetch_sub(1, Ordering::SeqCst);
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
        upstream_request_id: None,
        status,
        code: code.into(),
        message: message.into(),
    }
}

pub struct RequestContext {
    pub id: String,
    selection: std::sync::Mutex<Option<(String, String)>>,
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
    let available = !state.snapshot().await.entries.is_empty();
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
        selection: std::sync::Mutex::new(None),
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
        let selection = context.selection.lock().unwrap().clone();
        state
            .telemetry
            .note_rejection(&format!("http_{}", response.status().as_u16()));
        state
            .telemetry
            .record(RequestRecord {
                upstream_request_id: None,
                request_id: context.id.clone(),
                client_id,
                model: selection
                    .as_ref()
                    .map(|(_, model)| model.clone())
                    .unwrap_or_default(),
                provider: selection
                    .as_ref()
                    .map(|(provider, _)| provider.clone())
                    .unwrap_or_else(|| {
                        if endpoint == "native" {
                            "agy"
                        } else {
                            "unknown"
                        }
                        .into()
                    }),
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
                upstream_request_id: None,
                request_id: Uuid::new_v4().to_string(),
                client_id: "rejected".into(),
                model: String::new(),
                provider: "unknown".into(),
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
    let snapshot = state.snapshot().await;
    Json(
        json!({"object":"list", "data":snapshot.entries.iter().map(|entry| json!({"id":entry.model.id,"object":"model","created":0,"owned_by":entry.provider.owned_by()})).collect::<Vec<_>>()}),
    )
}

async fn capabilities(State(state): State<Arc<AppState>>) -> Json<Value> {
    let snapshot = state.snapshot().await;
    let provider = state.provider.read().await;
    let openai = state.openai_provider.read().await;
    let mut value = json!({"provider":"agy", "cli_version":provider.version, "expected_cli_version":state.config.expected_agy_version, "provider_ready":providers::ready(&provider,Utc::now()),
        "profiles":{"model":{"chat_completions":true,"responses":true,"streaming":true,"text":true,"json_schema":true,"client_tools":true,"max_tool_calls_per_turn":1,"parallel_tool_execution":false,"media_input":false,
                    "client_tool_argument_correction":{"max_corrections":crate::relay::MAX_TOOL_CORRECTIONS,"scope":"request","same_run":true,"unknown_tools":false},
                    "item_references":{"enabled":true,"storage":"memory","default_store":true,"ttl_seconds":items::TTL_SECONDS,"max_items":items::MAX_ITEMS,"max_bytes":items::MAX_BYTES,"per_client_items":items::CLIENT_ITEMS,"per_client_bytes":items::CLIENT_BYTES,"cleared_on_restart":true,"eviction":"oldest_first"},
                    "prompt_cache_controls":{"mode":"ignored","retention_guarantee":false,"parameters":["prompt_cache_key","prompt_cache_retention"]},
                    "output_token_budget":{"mode":"prompt_guidance","hard_limit":false,"limits_reasoning_tokens":false,"parameters":["max_output_tokens","max_tokens","max_completion_tokens"]}},
                    "native":{"enabled":state.config.native_enabled && !state.config.openai_enabled,"requires_operator_sandbox_verification":true,"runs":true,"uploaded_files":true,"artifacts":true,"continuation":false}},
        "unsupported":["sampling_controls","forced_tool_choice","embeddings","audio","image_generation_api","realtime","batches","fine_tuning","provider_management"],
        "verification":{"adapter_contract":"automated_fixtures","real_model_and_client_loops":"operator_gate","native_sandbox":"operator_gate"},
        "models":provider.models,"per_model":provider.models.iter().map(|m|json!({"id":m.id,"cli_version":provider.version,"model_profile_verification":"operator_gate","native_profile_verification":"operator_gate"})).collect::<Vec<_>>(),"quota":provider.quota,"checked_at":provider.checked_at});
    value["schema_version"] = json!(2);
    value["admission"] = json!({"total_max_concurrent":state.config.max_concurrent + if state.config.openai_enabled {state.config.openai_max_concurrent} else {0}, "providers":{"agy":{"max_concurrent":state.config.max_concurrent,"max_queue":state.config.max_queue},"openai":{"enabled":state.config.openai_enabled,"max_concurrent":state.config.openai_max_concurrent,"max_queue":state.config.openai_max_queue}}});
    value["providers"] = json!({
        "agy": {"enabled":true,"ready":snapshot.entries.iter().any(|entry|entry.provider==ProviderId::Agy),"version":provider.version,"checked_at":provider.checked_at,"error":provider.error,"quota":provider.quota},
        "openai": {"enabled":state.config.openai_enabled,"ready":snapshot.entries.iter().any(|entry|entry.provider==ProviderId::OpenAi),"version":openai.version,"checked_at":openai.checked_at,"error":openai.error,"quota":openai.quota,"usage_url":"https://chatgpt.com/settings/usage","upstream":"responses","upstream_store":false,"upstream_stream":true,"client_tools":true,"media_input":false,"output_token_budget":{"mode":"prompt_guidance","hard_limit":false},"unsupported":["sampling_controls","hosted_tools","native"],"reasoning_effort":"model_dependent"}
    });
    value["model_conflicts"] = json!(snapshot.conflicts);
    value["per_model"] = json!(snapshot.entries.iter().map(|entry| match entry.provider {
        ProviderId::Agy => json!({"id":entry.model.id,"provider":"agy","cli_version":provider.version,"model_profile_verification":"operator_gate","native_profile_verification":"operator_gate","chat_completions":true,"responses":true,"client_tools":true,"max_tool_calls_per_turn":1,"parallel_tool_execution":false,"output_token_budget":{"mode":"prompt_guidance"}}),
        ProviderId::OpenAi => json!({"id":entry.model.id,"provider":"openai","chat_completions":true,"responses":true,"streaming":true,"client_tools":true,"native":false,"media_input":false,"output_token_budget":{"mode":"prompt_guidance","hard_limit":false},"model_profile_verification":"operator_gate"}),
    }).collect::<Vec<_>>());
    Json(value)
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
    state
        .telemetry
        .capture_request(
            &context.id,
            if responses {
                "/v1/responses"
            } else {
                "/v1/chat/completions"
            },
            &body,
        )
        .await;
    let has_cache_hints = ["prompt_cache_key", "prompt_cache_retention"]
        .iter()
        .any(|field| body.get(*field).is_some_and(|value| !value.is_null()));
    let storage_mode = if body.get("store").and_then(Value::as_bool) == Some(false) {
        "disabled"
    } else {
        "memory"
    };
    let remaining = Duration::from_secs(state.config.request_timeout_secs)
        .checked_sub(context.started.elapsed())
        .unwrap_or_default();
    let result = tokio::time::timeout(
        remaining,
        generate_body(state.clone(), key, context.clone(), body, responses),
    )
    .await
    .unwrap_or_else(|_| Err(error(504, "provider_timeout", "Request deadline exceeded")))
    .map(|mut response| {
        if responses {
            response
                .headers_mut()
                .insert("x-ai-router-item-storage", storage_mode.parse().unwrap());
        }
        if has_cache_hints {
            response.headers_mut().insert(
                "x-ai-router-prompt-cache-controls",
                "ignored".parse().unwrap(),
            );
        }
        response
    });
    if let Err(error) = &result {
        state
            .telemetry
            .finish_detail(
                &context.id,
                Some(CapturedPayload::capture(&error.response_body())),
            )
            .await;
    }
    result
}

async fn generate_body(
    state: Arc<AppState>,
    key: KeyConfig,
    context: Arc<RequestContext>,
    mut body: Value,
    responses: bool,
) -> Result<Response, ApiError> {
    let model_id = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("model must be a string"))?;
    let selected = state.resolve_model(model_id).await?;
    *context.selection.lock().unwrap() = Some((selected.as_str().into(), model_id.to_owned()));
    let pinned_generation = if selected == ProviderId::OpenAi {
        Some(state.openai.generation().await?)
    } else {
        None
    };
    let item_scope = format!(
        "{}:{}",
        selected.as_str(),
        pinned_generation.as_deref().unwrap_or("agy")
    );
    let store_items = responses && body.get("store").and_then(Value::as_bool).unwrap_or(true);
    if responses {
        state
            .items
            .resolve_scoped(&key.id, &mut body, state.config.max_body_bytes, &item_scope)?;
    }
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let has_token_budget = ["max_output_tokens", "max_tokens", "max_completion_tokens"]
        .iter()
        .any(|field| body.get(*field).is_some_and(|value| !value.is_null()));
    let include_usage = body
        .pointer("/stream_options/include_usage")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let id = context.id.clone();
    let mut request = if responses {
        normalize_response(body, id.clone())?
    } else {
        normalize_chat(body, id.clone())?
    };
    request.owner = key.id.clone();
    request.endpoint = if responses { "responses" } else { "chat" }.into();
    request.provider_generation = pinned_generation.clone();
    let started = context.started;
    let admission = state.admit_provider(selected).await?;
    if state.resolve_model(&request.model).await? != selected {
        return Err(error(
            503,
            "provider_unavailable",
            "The selected model ownership changed while queued",
        ));
    }
    if selected == ProviderId::OpenAi && Some(state.openai.generation().await?) != pinned_generation
    {
        return Err(error(
            503,
            "provider_unavailable",
            "The selected ChatGPT registration changed while queued",
        ));
    }
    let cancel = state.shutdown.child_token();
    let model = request.model.clone();
    eprintln!(
        "{}",
        json!({
            "event":"model_request", "request_id":id,
            "endpoint":if responses { "responses" } else { "chat" }, "model":model,
            "client_tool_count":request.tools.len(),
            "client_tool_names":request.tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>()
        })
    );
    let remaining = Duration::from_secs(state.config.request_timeout_secs)
        .checked_sub(context.started.elapsed())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| error(504, "provider_timeout", "Request deadline exceeded"))?;
    request.remaining_ms = Some(remaining.as_millis().min(u128::from(u64::MAX)) as u64);
    let mut run = match selected {
        ProviderId::Agy => state.runner.start(request, cancel.clone()).await?,
        ProviderId::OpenAi => state.openai.start(request, cancel.clone()).await?,
    };
    let owner = key.id.clone();
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
    guard.record.provider = selected.as_str().into();
    let completion_id = format!("{}{}", if responses { "resp_" } else { "chatcmpl_" }, id);
    let created = Utc::now().timestamp();
    if !streaming {
        while let Some(event) = run.events.recv().await {
            guard.observe(&event);
            match event {
                RunEvent::Completed(result) => {
                    if let Err(error) = validate_result(&result) {
                        guard.observe(&RunEvent::Error(error.clone()));
                        return Err(error);
                    }
                    let output = if responses {
                        response_object(&completion_id, created, &model, &result, store_items)
                    } else {
                        chat_object(&completion_id, created, &model, &result)
                    };
                    if store_items {
                        if let Err(error) = state.items.save_scoped(&owner, &output, &item_scope) {
                            guard.observe(&RunEvent::Error(error.clone()));
                            return Err(error);
                        }
                    }
                    guard.capture_response(&output);
                    return Ok(token_budget_response(
                        Json(output).into_response(),
                        has_token_budget,
                    ));
                }
                RunEvent::Error(e) => return Err(e),
                _ => {}
            }
        }
        let failure = error(
            502,
            "incomplete_provider_output",
            "Provider ended without a result",
        );
        guard.observe(&RunEvent::Error(failure.clone()));
        return Err(failure);
    }
    let stream = async_stream::stream! {
        let mut guard = guard;
        let mut render = StreamRenderer::new(responses, completion_id.clone(), model.clone(), created, include_usage, store_items);
        for item in render.begin() { yield Ok::<Event,Infallible>(item); }
        let mut terminal = false;
        while let Some(mut event) = run.events.recv().await {
            if let RunEvent::Completed(result) = &event {
                if let Err(error) = validate_result(result) { event = RunEvent::Error(error); }
            }
            if store_items {
                if let RunEvent::Completed(result) = &event {
                    let output = response_object(&completion_id, created, &model, result, true);
                    if let Err(error) = state.items.save_scoped(&owner, &output, &item_scope) {
                        event = RunEvent::Error(error);
                    }
                }
            }
            guard.observe(&event);
            if let RunEvent::Completed(result) = &event {
                let output = if responses { response_object(&completion_id, created, &model, result, store_items) }
                    else { chat_object(&completion_id, created, &model, result) };
                guard.capture_response(&output);
            }
            terminal = matches!(&event, RunEvent::Completed(_) | RunEvent::Error(_));
            for item in render.event(event) { yield Ok(item); }
            if terminal { break; }
        }
        if !terminal {let failure=RunEvent::Error(error(502,"incomplete_provider_output","Provider ended without a result"));guard.observe(&failure);for item in render.event(failure) {yield Ok(item);}}
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
    Ok(token_budget_response(response, has_token_budget))
}

fn token_budget_response(mut response: Response, has_budget: bool) -> Response {
    if has_budget {
        response.headers_mut().insert(
            "x-ai-router-token-budget-mode",
            "prompt-guidance".parse().unwrap(),
        );
    }
    response
}

pub fn chat_usage(usage: &Option<Usage>) -> Value {
    usage.as_ref().map(|u| json!({"prompt_tokens":u.input_tokens,"completion_tokens":u.output_tokens,"total_tokens":u.total_tokens,"prompt_tokens_details":{"cached_tokens":u.cache_read_tokens},"completion_tokens_details":{"reasoning_tokens":u.thinking_tokens}})).unwrap_or(Value::Null)
}
pub fn response_usage(usage: &Option<Usage>) -> Value {
    usage.as_ref().map(|u| json!({"input_tokens":u.input_tokens,"output_tokens":u.output_tokens,"total_tokens":u.total_tokens,"input_tokens_details":{"cached_tokens":u.cache_read_tokens},"output_tokens_details":{"reasoning_tokens":u.thinking_tokens}})).unwrap_or(Value::Null)
}
fn validate_result(result: &RunResult) -> Result<(), ApiError> {
    if matches!(
        result.status.as_str(),
        "completed" | "tool_calls" | "incomplete"
    ) {
        Ok(())
    } else {
        Err(error(
            502,
            "incomplete_provider_output",
            "Provider returned an unsupported terminal status",
        ))
    }
}
fn finish_reason(result: &RunResult) -> &'static str {
    if result.status == "incomplete" {
        "length"
    } else if result.tool_calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    }
}
fn tool_json(tool: &ToolCall) -> Value {
    json!({"id":tool.id,"type":"function","function":{"name":tool.name,"arguments":tool.arguments.to_string()}})
}
pub fn chat_object(id: &str, created: i64, model: &str, result: &RunResult) -> Value {
    let mut message = json!({"role":"assistant","content":if result.text.is_empty() && !result.tool_calls.is_empty() {Value::Null} else {Value::String(result.text.clone())}});
    if !result.tool_calls.is_empty() {
        message["tool_calls"] = json!(result.tool_calls.iter().map(tool_json).collect::<Vec<_>>());
    }
    json!({"id":id,"object":"chat.completion","created":created,"model":model,"choices":[{"index":0,"message":message,"finish_reason":finish_reason(result)}],"usage":chat_usage(&result.usage),"agy_usage_partial":result.usage_partial,"ai_router_usage_partial":result.usage_partial})
}
fn response_tool(tool: &ToolCall) -> Value {
    json!({"id":format!("fc_{}",tool.id),"type":"function_call","status":"completed","call_id":tool.id,"name":tool.name,"arguments":tool.arguments.to_string()})
}
pub fn response_object(
    id: &str,
    created: i64,
    model: &str,
    result: &RunResult,
    store: bool,
) -> Value {
    let mut output = Vec::new();
    if !result.text.is_empty() || (result.tool_calls.is_empty() && result.output_items.is_empty()) {
        output.push(json!({"id":format!("msg_{id}"),"type":"message","status":if result.status=="incomplete" {"incomplete"} else {"completed"},"role":"assistant","content":[{"type":"output_text","text":result.text,"annotations":[]}]}));
    }
    output.extend(result.tool_calls.iter().map(response_tool));
    output.extend(result.output_items.clone());
    json!({"id":id,"object":"response","created_at":created,"status":if result.status=="incomplete" {"incomplete"} else {"completed"},"error":null,"incomplete_details":if result.status=="incomplete" {json!({"reason":"upstream_incomplete"})} else {Value::Null},"model":model,"output":output,"usage":response_usage(&result.usage),"parallel_tool_calls":result.tool_calls.len()>1,"store":store,"agy_usage_partial":result.usage_partial,"ai_router_usage_partial":result.usage_partial})
}

pub struct CompletionGuard {
    response: Option<CapturedPayload>,
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
            response: None,
            state,
            record: RequestRecord {
                upstream_request_id: None,
                request_id,
                client_id,
                model,
                provider: "agy".into(),
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
    pub fn capture_response(&mut self, value: &Value) {
        self.response = Some(CapturedPayload::capture(value));
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
                self.capture_response(&json!(result));
                self.record.status = if result.status == "incomplete" {
                    "incomplete"
                } else {
                    "success"
                }
                .into();
                self.record.error_code = None;
                self.record.upstream_request_id = result.upstream_request_id.clone();
                self.record.usage = result.usage.clone();
                self.record.usage_partial = result.usage_partial;
            }
            RunEvent::Error(e) => {
                self.capture_response(&e.response_body());
                self.record.status = if e.code == "request_cancelled" {
                    "cancelled"
                } else {
                    "failed"
                }
                .into();
                self.record.upstream_request_id = e.upstream_request_id.clone();
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
        if self.record.error_code.as_deref() == Some("client_disconnected")
            && self.started.elapsed() >= Duration::from_secs(self.state.config.request_timeout_secs)
        {
            self.record.status = "failed".into();
            self.record.error_code = Some("provider_timeout".into());
        }
        self.record.duration_ms = self.started.elapsed().as_millis() as u64;
        let record = self.record.clone();
        let response = self.response.take();
        let state = self.state.clone();
        let workspace = self.workspace.take();
        let workspace_owned = self.workspace_owned.clone();
        let finished = self.finished.clone();
        let admission = self.admission.take();
        tokio::spawn(async move {
            let stopped = tokio::time::timeout(Duration::from_secs(30), finished.cancelled())
                .await
                .is_ok();
            state
                .telemetry
                .finish_detail(&record.request_id, response)
                .await;
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
    store_items: bool,
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
    fn new(
        responses: bool,
        id: String,
        model: String,
        created: i64,
        include_usage: bool,
        store_items: bool,
    ) -> Self {
        Self {
            responses,
            store_items,
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
        let response = json!({"id":self.id,"object":"response","created_at":self.created,"model":self.model,"status":"in_progress","output":[],"error":null,"incomplete_details":null,"usage":null,"store":self.store_items});
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
                        let item = json!({"id":format!("msg_{}",self.id),"type":"message","status":if result.status=="incomplete" {"incomplete"} else {"completed"},"role":"assistant","content":[{"type":"output_text","text":self.text,"annotations":[]}]});
                        events.push(self.emit(
                            "response.output_item.done",
                            json!({"output_index":0,"item":item}),
                        ));
                    }
                    let base = usize::from(self.text_started) + self.tool_order.len();
                    for (offset, item) in result.output_items.iter().enumerate() {
                        events.push(self.emit(
                            "response.output_item.added",
                            json!({"output_index":base+offset,"item":item}),
                        ));
                        events.push(self.emit(
                            "response.output_item.done",
                            json!({"output_index":base+offset,"item":item}),
                        ));
                    }
                    let kind = if result.status == "incomplete" {
                        "response.incomplete"
                    } else {
                        "response.completed"
                    };
                    events.push(self.emit(kind,json!({"response":response_object(&self.id,self.created,&self.model,&result,self.store_items)})));
                } else {
                    events.push(self.chunk(json!({}), json!(finish_reason(&result))));
                    if self.include_usage {
                        events.push(self.emit("",json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,"choices":[],"usage":chat_usage(&result.usage),"agy_usage_partial":result.usage_partial,"ai_router_usage_partial":result.usage_partial})));
                    }
                    events.push(Event::default().data("[DONE]"));
                }
                events
            }
            RunEvent::Error(e) => {
                if self.responses {
                    vec![self.emit("response.failed",json!({"response":{"id":self.id,"object":"response","created_at":self.created,"model":self.model,"status":"failed","output":[],"error":{"code":e.code,"message":e.message,"upstream_request_id":e.upstream_request_id},"usage":null}}))]
                } else {
                    vec![self.emit("error",json!({"error":{"code":e.code,"message":e.message,"type":"provider_error","upstream_request_id":e.upstream_request_id}})),Event::default().data("[DONE]")]
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
    fn test_state() -> (tempfile::TempDir, Arc<AppState>) {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::for_test(temp.path());
        config.max_concurrent = 1;
        config.max_queue = 0;
        config.openai_max_concurrent = 1;
        config.openai_max_queue = 0;
        let telemetry = Arc::new(
            Telemetry::open(
                &config.telemetry_dir,
                config.retention_days,
                config.max_events,
            )
            .unwrap(),
        );
        let config = Arc::new(config);
        let state = AppState::new(config.clone(), Runner::new(config), telemetry);
        (temp, state)
    }
    #[tokio::test]
    async fn saturated_provider_does_not_reserve_other_provider_capacity() {
        let (_temp, state) = test_state();
        let agy = state.admit_provider(ProviderId::Agy).await.unwrap();
        assert!(state.admit_provider(ProviderId::Agy).await.is_err());
        let openai = state.admit_provider(ProviderId::OpenAi).await.unwrap();
        assert!(state.admit_provider(ProviderId::OpenAi).await.is_err());
        assert_eq!(state.active.load(Ordering::SeqCst), 2);
        drop(agy);
        let agy = state.admit_provider(ProviderId::Agy).await.unwrap();
        drop(openai);
        drop(agy);
        state.stop();
        assert!(state.drain(Duration::from_secs(1)).await);
    }
    #[tokio::test]
    async fn disabled_openai_and_stale_agy_are_not_ready() {
        let (_temp, state) = test_state();
        *state.provider.write().await = ProviderStatus {
            authenticated: true,
            checked_at: (Utc::now() - chrono::Duration::seconds(181)).to_rfc3339(),
            models: vec![ModelInfo {
                id: "synthetic".into(),
                name: "synthetic".into(),
            }],
            ..Default::default()
        };
        assert!(state.snapshot().await.entries.is_empty());
        assert_eq!(
            state.resolve_model("synthetic").await.unwrap_err().code,
            "provider_unavailable"
        );
    }

    struct DualHarness {
        _root: tempfile::TempDir,
        state: Arc<AppState>,
        app: Router,
        captured: Arc<tokio::sync::Mutex<Vec<Value>>>,
        server: tokio::task::JoinHandle<()>,
        model_mode: Arc<AtomicUsize>,
    }
    impl Drop for DualHarness {
        fn drop(&mut self) {
            self.server.abort();
            self.state.stop();
        }
    }
    async fn mock_openai_response(
        State(captured): State<Arc<tokio::sync::Mutex<Vec<Value>>>>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Response {
        assert_eq!(headers["authorization"], "Bearer synthetic-openai-token");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert!(body["input"].is_array());
        assert!(body.get("max_output_tokens").is_none());
        captured.lock().await.push(body.clone());
        let input = body["input"].to_string();
        if input.contains("deadline-fixture") {
            tokio::time::sleep(Duration::from_millis(700)).await;
        }
        if input.contains("quota-fixture") {
            return (StatusCode::TOO_MANY_REQUESTS, Json(json!({"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"synthetic-provider-private-detail"}}))).into_response();
        }
        let continuing = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call_output");
        let (text, output) = if input.contains("tools-fixture") && !continuing {
            (
                String::new(),
                vec![
                    json!({"type":"reasoning","id":"rs_synthetic","summary":[],"encrypted_content":"synthetic-encrypted-reasoning"}),
                    json!({"type":"function_call","call_id":"call_one","name":"lookup","arguments":"{\"q\":\"one\"}","status":"completed"}),
                    json!({"type":"function_call","call_id":"call_two","name":"lookup","arguments":"{\"q\":\"two\"}","status":"completed"}),
                ],
            )
        } else if input.contains("reasoning-only-fixture") {
            (
                String::new(),
                vec![
                    json!({"type":"reasoning","id":"rs_reasoning_only_synthetic","summary":[],"encrypted_content":"synthetic-encrypted-reasoning-only"}),
                ],
            )
        } else if input.contains("malformed-fixture") {
            (
                String::new(),
                vec![
                    json!({"type":"function_call","call_id":"call_bad","name":"lookup","arguments":"{\"q\":12}","status":"completed"}),
                ],
            )
        } else {
            let text = if input.contains("schema-fixture") {
                "{\"answer\":\"valid\"}"
            } else if continuing {
                "tool results accepted"
            } else {
                "OpenAI synthetic answer"
            }
            .to_owned();
            (
                text.clone(),
                vec![
                    json!({"id":"msg_upstream","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text}]}),
                ],
            )
        };
        let status =
            if input.contains("incomplete-fixture") || input.contains("reasoning-only-fixture") {
                "incomplete"
            } else {
                "completed"
            };
        let terminal = json!({"id":"resp_upstream","status":status,"output":output,"usage":{"input_tokens":4,"output_tokens":6,"total_tokens":10}});
        let mut wire = String::new();
        if !text.is_empty() {
            wire.push_str(&format!(
                "data: {}\n\n",
                json!({"type":"response.output_text.delta","delta":text})
            ));
        }
        wire.push_str(&format!(
            "data: {}\n\n",
            json!({"type":format!("response.{status}"),"response":terminal})
        ));
        ([("content-type", "text/event-stream")], wire).into_response()
    }
    impl DualHarness {
        async fn new() -> Self {
            Self::with_timeout(5).await
        }
        async fn with_timeout(timeout_seconds: u64) -> Self {
            let root = tempfile::tempdir().unwrap();
            let mut config = Config::for_test(root.path());
            config.openai_enabled = true;
            config.agy_bin =
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-agy");
            config.request_timeout_secs = timeout_seconds;
            config.preauth_per_minute = 10000;
            config.global_preauth_per_minute = 10000;
            config.per_key_per_minute = 10000;
            let config = Arc::new(config);
            let captured = Arc::new(tokio::sync::Mutex::new(Vec::new()));
            let model_mode = Arc::new(AtomicUsize::new(0));
            let mode = model_mode.clone();
            let mock = Router::new().route("/models", get(move || {
                let mode=mode.clone();
                async move {
                    if mode.load(Ordering::SeqCst)==2 { tokio::time::sleep(Duration::from_secs(1)).await; }
                    if mode.load(Ordering::SeqCst)==1 { return StatusCode::SERVICE_UNAVAILABLE.into_response(); }
                    Json(json!({"models":[{"slug":"synthetic-openai","display_name":"Synthetic OpenAI","visibility":"list"}]})).into_response()
                }
            }))
                .route("/responses", post(mock_openai_response)).with_state(captured.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, mock).await.unwrap();
            });
            let telemetry = Arc::new(
                Telemetry::open(
                    &config.telemetry_dir,
                    config.retention_days,
                    config.max_events,
                )
                .unwrap(),
            );
            let mut state = AppState::new(config.clone(), Runner::new(config.clone()), telemetry);
            Arc::get_mut(&mut state).unwrap().openai = OpenAi::synthetic(
                config,
                format!("http://{address}"),
                "synthetic-openai-token".into(),
            );
            state.refresh_provider().await;
            assert!(state.provider.read().await.authenticated);
            assert!(state.openai_provider.read().await.authenticated);
            let app = router(state.clone());
            Self {
                _root: root,
                state,
                app,
                captured,
                server,
                model_mode,
            }
        }
        async fn call(&self, path: &str, body: Option<Value>) -> (StatusCode, String) {
            use http_body_util::BodyExt;
            use tower::ServiceExt;
            let request = Request::builder()
                .method(if body.is_some() { "POST" } else { "GET" })
                .uri(path)
                .header("authorization", "Bearer test-key")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    body.map(|v| v.to_string()).unwrap_or_default(),
                ))
                .unwrap();
            let response = self.app.clone().oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            (status, String::from_utf8(bytes.to_vec()).unwrap())
        }
    }
    fn lookup_tool(responses: bool) -> Value {
        let tool = json!({"name":"lookup","description":"Lookup","parameters":{"type":"object","properties":{"q":{"type":"string"}},"required":["q"],"additionalProperties":false}});
        if responses {
            let mut value = tool;
            value["type"] = json!("function");
            value
        } else {
            json!({"type":"function","function":tool})
        }
    }
    #[tokio::test]
    async fn dual_provider_apis_stream_nonstream_and_incomplete() {
        let h = DualHarness::new().await;
        let (_, models) = h.call("/v1/models", None).await;
        let models: Value = serde_json::from_str(&models).unwrap();
        assert_eq!(models["data"].as_array().unwrap().len(), 3);
        for responses in [false, true] {
            let path = if responses {
                "/v1/responses"
            } else {
                "/v1/chat/completions"
            };
            for stream in [false, true] {
                let mut body = if responses {
                    json!({"model":"synthetic-openai","input":"hello"})
                } else {
                    json!({"model":"synthetic-openai","messages":[{"role":"user","content":"hello"}]})
                };
                body["stream"] = json!(stream);
                let (status, wire) = h.call(path, Some(body)).await;
                assert_eq!(status, StatusCode::OK, "{wire}");
                assert!(wire.contains("OpenAI synthetic answer"));
                if stream {
                    assert!(wire.contains(if responses {
                        "response.completed"
                    } else {
                        "[DONE]"
                    }));
                }
            }
            let body = if responses {
                json!({"model":"synthetic-openai","input":"incomplete-fixture"})
            } else {
                json!({"model":"synthetic-openai","messages":[{"role":"user","content":"incomplete-fixture"}]})
            };
            let (status, wire) = h.call(path, Some(body)).await;
            assert_eq!(status, StatusCode::OK, "{wire}");
            let value: Value = serde_json::from_str(&wire).unwrap();
            assert_eq!(
                if responses {
                    &value["status"]
                } else {
                    &value["choices"][0]["finish_reason"]
                },
                if responses { "incomplete" } else { "length" }
            );
        }
    }
    #[tokio::test]
    async fn openai_tools_replay_opaque_items_and_local_references() {
        let h = DualHarness::new().await;
        let (status,wire)=h.call("/v1/responses",Some(json!({"model":"synthetic-openai","input":"tools-fixture","tools":[lookup_tool(true)]}))).await;
        assert_eq!(status, StatusCode::OK, "{wire}");
        let value: Value = serde_json::from_str(&wire).unwrap();
        let output = value["output"].as_array().unwrap();
        assert_eq!(
            output
                .iter()
                .filter(|item| item["type"] == "function_call")
                .count(),
            2
        );
        assert!(
            output
                .iter()
                .any(|item| item["encrypted_content"] == "synthetic-encrypted-reasoning")
        );
        let mut input = vec![json!({"role":"user","content":"tools-fixture"})];
        input.extend(
            output
                .iter()
                .map(|item| json!({"type":"item_reference","id":item["id"]})),
        );
        input.extend([
            json!({"type":"function_call_output","call_id":"call_one","output":"one"}),
            json!({"type":"function_call_output","call_id":"call_two","output":"two"}),
        ]);
        let (status, wire) = h
            .call(
                "/v1/responses",
                Some(json!({"model":"synthetic-openai","input":input,"tools":[lookup_tool(true)]})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{wire}");
        assert!(wire.contains("tool results accepted"));
        let captured = h.captured.lock().await;
        assert!(
            captured.last().unwrap()["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "reasoning")
        );
        drop(captured);
        let (status,wire)=h.call("/v1/chat/completions",Some(json!({"model":"synthetic-openai","messages":[{"role":"user","content":"tools-fixture"}],"tools":[lookup_tool(false)]}))).await;
        assert_eq!(status, StatusCode::OK, "{wire}");
        let first: Value = serde_json::from_str(&wire).unwrap();
        let assistant = first["choices"][0]["message"].clone();
        let (status,wire)=h.call("/v1/chat/completions",Some(json!({"model":"synthetic-openai","messages":[{"role":"user","content":"tools-fixture"},assistant,{"role":"tool","tool_call_id":"call_one","content":"one"},{"role":"tool","tool_call_id":"call_two","content":"two"}],"tools":[lookup_tool(false)]}))).await;
        assert_eq!(status, StatusCode::OK, "{wire}");
        assert!(wire.contains("tool results accepted"));
        let captured = h.captured.lock().await;
        assert!(
            captured.last().unwrap()["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "reasoning")
        );
    }
    #[tokio::test]
    async fn openai_schema_tool_failures_and_quota_leave_agy_available() {
        let h = DualHarness::new().await;
        let schema = json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false});
        let (status,wire)=h.call("/v1/chat/completions",Some(json!({"model":"synthetic-openai","messages":[{"role":"user","content":"schema-fixture"}],"response_format":{"type":"json_schema","json_schema":{"name":"answer","schema":schema}}}))).await;
        assert_eq!(status, StatusCode::OK, "{wire}");
        let (status,wire)=h.call("/v1/responses",Some(json!({"model":"synthetic-openai","input":"malformed-fixture","tools":[lookup_tool(true)]}))).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{wire}");
        assert!(wire.contains("invalid_provider_tool_call"));
        let (status, wire) = h
            .call(
                "/v1/responses",
                Some(json!({"model":"synthetic-openai","input":"quota-fixture"})),
            )
            .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{wire}");
        assert!(!wire.contains("synthetic-provider-private-detail"));
        // A successful model discovery cannot clear a plan-usage latch.
        h.state.refresh_openai().await;
        let (_, wire) = h.call("/v1/models", None).await;
        assert!(!wire.contains("synthetic-openai"));
        assert!(wire.contains("gemini-3-pro"));
        let (status, wire) = h
            .call(
                "/v1/chat/completions",
                Some(
                    json!({"model":"gemini-3-pro","messages":[{"role":"user","content":"hello"}]}),
                ),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{wire}");
        let (_, wire) = h
            .call(
                "/v1/responses",
                Some(json!({"model":"synthetic-openai","input":"hello"})),
            )
            .await;
        assert!(wire.contains("subscription_sharing_usage_limit_exceeded"));
    }

    #[tokio::test]
    async fn probes_publish_independently_and_outages_isolate_in_both_directions() {
        let h = DualHarness::new().await;
        *h.state.provider.write().await = ProviderStatus::default();
        let (_, wire) = h.call("/v1/models", None).await;
        assert!(wire.contains("synthetic-openai"));
        assert!(!wire.contains("gemini-3-pro"));
        h.model_mode.store(2, Ordering::SeqCst);
        let state = h.state.clone();
        let refresh = tokio::spawn(async move {
            state.refresh_provider().await;
        });
        tokio::time::timeout(Duration::from_millis(800), async {
            loop {
                if h.state.provider.read().await.authenticated {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("AGY publication must not wait for OpenAI discovery");
        assert!(!refresh.is_finished());
        refresh.await.unwrap();
        h.model_mode.store(1, Ordering::SeqCst);
        h.state.refresh_openai().await;
        let (_, wire) = h.call("/v1/models", None).await;
        assert!(!wire.contains("synthetic-openai"));
        assert!(wire.contains("gemini-3-pro"));
        let (status, wire) = h
            .call(
                "/v1/responses",
                Some(json!({"model":"synthetic-openai","input":"hello"})),
            )
            .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{wire}");
    }
    #[tokio::test]
    async fn streaming_queue_wait_consumes_the_original_generation_deadline() {
        let h = DualHarness::with_timeout(1).await;
        let first = h.state.admit_provider(ProviderId::OpenAi).await.unwrap();
        let second = h.state.admit_provider(ProviderId::OpenAi).await.unwrap();
        let started = Instant::now();
        let request = h.call("/v1/chat/completions", Some(json!({"model":"synthetic-openai", "stream":true,"messages":[{"role":"user","content":"deadline-fixture"}]})));
        let release = async move {
            tokio::time::sleep(Duration::from_millis(550)).await;
            drop(first);
            drop(second);
        };
        let ((status, wire), ()) = tokio::join!(request, release);
        assert_eq!(status, StatusCode::OK, "{wire}");
        assert!(wire.contains("provider_timeout"), "{wire}");
        assert!(!wire.contains("OpenAI synthetic answer"), "{wire}");
        assert!(started.elapsed() < Duration::from_millis(1800));
        assert!(h.state.drain(Duration::from_secs(2)).await);
    }
    #[tokio::test]
    async fn reasoning_only_incomplete_stream_uses_consistent_output_indexes() {
        let h = DualHarness::new().await;
        let (status, wire) = h.call("/v1/responses", Some(json!({"model":"synthetic-openai","stream":true,"input":"reasoning-only-fixture"}))).await;
        assert_eq!(status, StatusCode::OK, "{wire}");
        let events: Vec<Value> = wire
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|data| serde_json::from_str(data).unwrap())
            .collect();
        let added = events
            .iter()
            .find(|event| event["type"] == "response.output_item.added")
            .unwrap();
        let done = events
            .iter()
            .find(|event| event["type"] == "response.output_item.done")
            .unwrap();
        let terminal = events
            .iter()
            .find(|event| event["type"] == "response.incomplete")
            .unwrap();
        assert_eq!(added["output_index"], 0);
        assert_eq!(done["output_index"], 0);
        assert_eq!(terminal["response"]["status"], "incomplete");
        let output = terminal["response"]["output"].as_array().unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(added["item"], output[0]);
        assert_eq!(done["item"], output[0]);
        assert!(
            !events
                .iter()
                .any(|event| event["type"] == "response.output_text.delta")
        );
        // Preserve the existing AGY empty-output response shape.
        let empty = RunResult {
            text: String::new(),
            structured_output: None,
            conversation_id: String::new(),
            usage: None,
            usage_partial: true,
            tool_calls: vec![],
            status: "completed".into(),
            output_items: vec![],
            upstream_request_id: None,
        };
        assert_eq!(
            response_object("synthetic", 0, "agy-synthetic", &empty, false)["output"][0]["type"],
            "message"
        );
    }
}
