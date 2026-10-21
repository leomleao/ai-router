use crate::{
    antigravity::Antigravity,
    auth::{self, RateLimiter},
    config::{Config, KeyConfig},
    items::{self, ItemStore},
    monitor::{CapturedPayload, RequestRecord, Telemetry},
    openai::OpenAi,
    protocol::*,
    providers::{ProviderId, Registry, Snapshot},
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
    pub antigravity: Antigravity,
    pub telemetry: Arc<Telemetry>,
    pub provider: RwLock<ProviderStatus>,
    pub openai: OpenAi,
    pub openai_provider: RwLock<ProviderStatus>,
    registry: std::sync::Mutex<Registry>,
    items: ItemStore,
    pub shutdown: CancellationToken,
    slots: Arc<Semaphore>,
    openai_slots: Arc<Semaphore>,
    openai_queued: AtomicUsize,
    agy_queued: AtomicUsize,
    body_slots: Arc<Semaphore>,
    active: AtomicUsize,
    queued: AtomicUsize,
    drained: Notify,
    preauth: RateLimiter,
    global: RateLimiter,
    clients: RateLimiter,
    rejection_logs: RateLimiter,
    stream_commit_wait: Duration,
}

/// Longest a stream waits for its first client-visible output before
/// committing its 200: the keepalive interval, so the wait adds no longer
/// silence than an open stream.
const STREAM_COMMIT_WAIT: Duration = Duration::from_secs(15);

impl AppState {
    pub fn new(
        config: Arc<Config>,
        antigravity: Antigravity,
        telemetry: Arc<Telemetry>,
    ) -> Arc<Self> {
        Arc::new(Self {
            slots: Arc::new(Semaphore::new(config.max_concurrent)),
            openai_slots: Arc::new(Semaphore::new(config.openai_max_concurrent)),
            openai_queued: AtomicUsize::new(0),
            agy_queued: AtomicUsize::new(0),
            openai: OpenAi::new(config.clone()),
            openai_provider: RwLock::new(ProviderStatus::default()),
            registry: std::sync::Mutex::new(Registry::default()),
            body_slots: Arc::new(Semaphore::new(32)),
            preauth: RateLimiter::new(config.preauth_per_minute, 4096),
            global: RateLimiter::new(config.global_preauth_per_minute, 1),
            clients: RateLimiter::new(config.per_key_per_minute, config.keys.len().max(1)),
            rejection_logs: RateLimiter::new(30, 1),
            config,
            antigravity,
            telemetry,
            provider: RwLock::new(ProviderStatus::default()),
            items: ItemStore::default(),
            shutdown: CancellationToken::new(),
            active: AtomicUsize::new(0),
            queued: AtomicUsize::new(0),
            drained: Notify::new(),
            stream_commit_wait: STREAM_COMMIT_WAIT,
        })
    }

    pub async fn refresh_agy(&self) {
        self.items.prune();
        let status = tokio::time::timeout(Duration::from_secs(10), self.antigravity.probe())
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
        *self.provider.write().await = status;
        self.publish_agy_monitor_status().await;
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
            *self.openai_provider.write().await = status;
            self.publish_openai_monitor_status().await;
        } else {
            let status = ProviderStatus {
                checked_at: Utc::now().to_rfc3339(),
                error: Some("provider_disabled".into()),
                ..Default::default()
            };
            self.telemetry.set_openai_provider(status).await;
        }
    }

    /// Publish effective OpenAI availability to operational telemetry without
    /// probing the provider. The catalogue can remain authenticated after a
    /// request latches an account or plan restriction, so telemetry must apply
    /// the same local availability and account-generation checks as admission.
    pub async fn publish_openai_monitor_status(&self) {
        let status = self.effective_openai_status().await;
        self.telemetry.set_openai_provider(status).await;
    }

    pub async fn refresh_provider(&self) {
        // Each future publishes its result as soon as its own probe finishes.
        tokio::join!(self.refresh_agy(), self.refresh_openai());
    }

    async fn snapshot(&self) -> Snapshot {
        // The registration generation travels with its catalogue. Read that
        // pair before inspecting current authorization so a newly discovered
        // account cannot make the prior account's catalogue appear ready.
        let openai = self.effective_openai_status().await;
        let agy = self.effective_agy_status().await;
        self.registry.lock().unwrap().snapshot(
            &agy,
            &openai,
            self.config.openai_enabled,
            Utc::now(),
        )
    }

    pub async fn publish_agy_monitor_status(&self) {
        self.telemetry
            .set_provider(self.effective_agy_status().await)
            .await;
    }
    async fn effective_agy_status(&self) -> ProviderStatus {
        let mut status = self.provider.read().await.clone();
        let check = tokio::time::timeout(Duration::from_millis(100), async {
            if let Some(error) = self.antigravity.availability_error().await {
                return Err(error);
            }
            let generation = self.antigravity.generation().await?;
            if status.registration_generation.as_ref() != Some(&generation) {
                return Err(error(
                    503,
                    "provider_account_changed",
                    "Antigravity account changed; rediscovery is required",
                ));
            }
            Ok(())
        })
        .await;
        match check {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                status.authenticated = false;
                status.error = Some(error.code);
            }
            Err(_) => {
                status.authenticated = false;
                status.error = Some("provider_status_timeout".into());
            }
        }
        status
    }

    async fn effective_openai_status(&self) -> ProviderStatus {
        let mut status = self.openai_provider.read().await.clone();
        if !self.config.openai_enabled {
            status.authenticated = false;
            status.error = Some("provider_disabled".into());
            return status;
        }
        let available = tokio::time::timeout(Duration::from_millis(100), async {
            if let Some(error) = self.openai.availability_error().await {
                return Err(error);
            }
            match self.openai.generation().await {
                Ok(generation) if status.registration_generation.as_ref() == Some(&generation) => {
                    Ok(())
                }
                Ok(_) => Err(error(
                    503,
                    "provider_account_changed",
                    "OpenAI account changed; provider discovery is required",
                )),
                Err(error) => Err(error),
            }
        })
        .await;
        match available {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                status.authenticated = false;
                status.error = Some(error.code);
            }
            Err(_) => {
                status.authenticated = false;
                status.error = Some("provider_status_timeout".into());
            }
        }
        status
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
        if let Ok(ProviderId::Agy) = resolved.as_ref() {
            self.antigravity.model_availability(model).await?;
        }
        if resolved
            .as_ref()
            .is_err_and(|e| e.code == "provider_unavailable")
            && snapshot.owner(model) == Some(ProviderId::Agy)
        {
            if let Some(error) = self.antigravity.availability_error().await {
                return Err(error);
            }
        }
        resolved
    }

    /// Map an ID the registry has never served onto a configured target.
    async fn fallback_model(&self, requested: &str) -> Option<String> {
        if !self.config.model_fallback {
            return None;
        }
        self.snapshot()
            .await
            .fallback(
                requested,
                &self.config.fallback_openai_model,
                &self.config.fallback_gemini_model,
            )
            .map(str::to_owned)
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
                    // Saturation is local and brief, not a client rate limit:
                    // 503 with retry-after is retried by clients that never
                    // retry 429 (n8n).
                    .map_err(|_| error(503, "busy", "The provider request queue is full"))?;
                self.queued.fetch_add(1, Ordering::SeqCst);
                queued_guard = Some(QueueGuard(self.clone(), provider));
                self.runtime().await;
                let wait = Duration::from_secs(self.config.request_timeout_secs.min(30));
                let result = tokio::time::timeout(wait, slots.clone().acquire_owned()).await;
                result
                    .map_err(|_| error(503, "busy", "The request queue wait expired"))?
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

    /// Admission is released after provider completion and final telemetry.
    /// Drain these tasks before flushing during shutdown.
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
    ApiError::new(status, code, message)
}

pub struct RequestContext {
    pub id: String,
    selection: std::sync::Mutex<Option<(String, String)>>,
    fallback: std::sync::Mutex<Option<ModelFallback>>,
    disclosures: std::sync::Mutex<Vec<(&'static str, String)>>,
    pub started: Instant,
    pub recorded: AtomicBool,
}
impl RequestContext {
    /// Records how the router interpreted the request. `protect` adds every
    /// recorded header to the final response, whether it succeeds, streams,
    /// or fails before or after a stream commits.
    fn disclose(&self, name: &'static str, value: impl Into<String>) {
        let mut disclosures = self.disclosures.lock().unwrap();
        disclosures.retain(|(existing, _)| *existing != name);
        disclosures.push((name, value.into()));
    }
}

/// A served substitute for an unknown requested model, disclosed on every
/// response. `requested` is already bounded to a header- and log-safe ID.
#[derive(Clone)]
struct ModelFallback {
    requested: String,
    served: String,
}
impl ModelFallback {
    fn new(requested: &str, served: &str) -> Self {
        let requested: String = requested
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || "._:/-".contains(*c))
            .take(128)
            .collect();
        Self {
            requested: if requested.is_empty() {
                "invalid".into()
            } else {
                requested
            },
            served: served.into(),
        }
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let protected = Router::new()
        .route("/v1/models", get(models))
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/chat/completions", post(chat))
        .route("/v1/responses", post(responses))
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
    let scope = "model";
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
                error(503, "busy", "Too many request bodies in progress"),
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
    let context = Arc::new(RequestContext {
        selection: std::sync::Mutex::new(None),
        fallback: std::sync::Mutex::new(None),
        disclosures: std::sync::Mutex::new(Vec::new()),
        id: Uuid::new_v4().to_string(),
        started: Instant::now(),
        recorded: AtomicBool::new(false),
    });
    let client_id = key.id.clone();
    let endpoint = if request.uri().path().starts_with("/v1/chat/") {
        "chat"
    } else if request.uri().path() == "/v1/responses" {
        "responses"
    } else {
        "api"
    };
    request.extensions_mut().insert(key);
    request.extensions_mut().insert(context.clone());
    let mut response = next.run(request).await;
    for (name, value) in context.disclosures.lock().unwrap().iter() {
        if let Ok(value) = value.parse() {
            response.headers_mut().insert(*name, value);
        }
    }
    let fallback = context.fallback.lock().unwrap().clone();
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
                requested_model: fallback.map(|fallback| fallback.requested),
                provider: selection
                    .as_ref()
                    .map(|(provider, _)| provider.clone())
                    .unwrap_or_else(|| "unknown".into()),
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
    // Rate limits are retryable; a durable restriction says it is not.
    if response.status() == StatusCode::TOO_MANY_REQUESTS
        && !response.headers().contains_key("x-should-retry")
    {
        response
            .headers_mut()
            .insert("retry-after", "10".parse().unwrap());
    }
    response
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
                requested_model: None,
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
        "This endpoint is not supported by the model-only router",
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
    let agy = state.effective_agy_status().await;
    let openai = state.effective_openai_status().await;
    let release = state.antigravity.release.snapshot().await;
    Json(json!({
        "schema_version":3,"provider":"model-router","native":false,
        "profiles":{"model":{"chat_completions":true,"responses":true,"streaming":true,"text":true,"json_schema":true,"client_tools":true,"parallel_tool_execution":false,"media_input":false,
            "output_token_budget":{"mode":"prompt_guidance","hard_limit":false,"limits_reasoning_tokens":false,"parameters":["max_output_tokens","max_tokens","max_completion_tokens"]},
            "prompt_cache_controls":{"mode":"ignored","retention_guarantee":false},
            "sampling_controls":{"mode":"ignored","accepted":SAMPLING_HINTS,"rejected":["n>1","logit_bias","logprobs","top_logprobs>0","stop"]},
            "ignored_fields":{"accepted":["metadata","user","safety_identifier","service_tier:auto","service_tier:default","truncation:auto","stream_options.include_obfuscation:false","annotations","cache_control","reasoning_content"],"metadata_echo":"responses","truncation":"never"},
            "reasoning_effort":{"accepted":["none","minimal","low","medium","high","xhigh","max"],"openai":{"none":"forwarded","minimal":"lowest_listed_level_when_unlisted"},"agy":{"none":"ignored","minimal":"ignored","other":"must_match_model_id","fallback_served":"served_model_tier"},
                "disclosure_header":"x-ai-router-reasoning-effort","header_values":["ignored","mapped=<level>"],"values_per_request":1},
            "reasoning_replay":{"unencrypted":"dropped"},
            "retry_hints":{"deterministic":"x-should-retry:false","transient_503":"retry-after","local_saturation":{"status":503,"code":"busy"},"rate_limit":{"status":429}},
            "responses_include":["reasoning.encrypted_content"],
            "reasoning_summary":{"mode":"unavailable","accepted":["auto","concise","detailed"]},
            "model_fallback":{"enabled":state.config.model_fallback,"applies_to":"model_ids_never_served","exact_ids_win":true,"listed_in_models":false,
                "gemini_prefix_target":state.config.fallback_gemini_model,"default_target":state.config.fallback_openai_model,"unavailable_target":"other_target","neither_available":"model_not_found",
                "disclosure_header":"x-ai-router-model-fallback","antigravity_effort":"served_model_tier","effort_disclosure_header":"x-ai-router-reasoning-effort"},
            "tool_choice":{"accepted":["auto","none","required","function","allowed_tools"],"inventory":"narrowed_to_selection","missing_required_call":"error","missing_required_call_error":{"status":502,"code":"missing_required_tool_call","retry":false},"error_param":"tool_choice"},
            "stream_commit":{"after":"first_client_output","max_wait_seconds":STREAM_COMMIT_WAIT.as_secs(),"failure_before_commit":"http_error","failure_after_commit":"error_event","responses_error_event":"error_then_response.failed"},
            "disclosure_headers":{"scope":"every_response_after_validation","model_fallback_scope":"every_response","streaming_and_errors":"same_as_nonstreaming",
                "headers":["x-ai-router-model-fallback","x-ai-router-reasoning-effort","x-ai-router-sampling-controls","x-ai-router-ignored-fields","x-ai-router-reasoning-replay","x-ai-router-reasoning-summary","x-ai-router-prompt-cache-controls","x-ai-router-token-budget-mode","x-ai-router-item-storage"]},
            "item_references":{"enabled":true,"storage":"memory","default_store":true,"ttl_seconds":items::TTL_SECONDS,"max_items":items::MAX_ITEMS,"max_bytes":items::MAX_BYTES,"per_client_items":items::CLIENT_ITEMS,"per_client_bytes":items::CLIENT_BYTES,"cleared_on_restart":true}}},
        "providers":{
            "agy":{"enabled":state.config.antigravity_enabled,"ready":snapshot.entries.iter().any(|e|e.provider==ProviderId::Agy),"adapter":agy.version,"baseline":crate::antigravity_auth::BASELINE_REVISION,"checked_at":agy.checked_at,"error":agy.error,"quota":agy.quota,"transport":"direct","fallback":false,"release_advisory":release,"tool_choice_forcing":"prompt_instruction"},
            "openai":{"enabled":state.config.openai_enabled,"ready":snapshot.entries.iter().any(|e|e.provider==ProviderId::OpenAi),"version":openai.version,"checked_at":openai.checked_at,"error":openai.error,"quota":openai.quota,"usage_url":"https://chatgpt.com/settings/usage","upstream":"responses","upstream_store":false,"upstream_stream":true,"client_tools":true,"media_input":false,"output_token_budget":{"mode":"prompt_guidance","hard_limit":false},"unsupported":["active_sampling_controls","hosted_tools","native"],"reasoning_effort":"model_dependent","tool_choice_forcing":"upstream_required"}},
        "admission":{"total_max_concurrent":state.config.max_concurrent+if state.config.openai_enabled {state.config.openai_max_concurrent}else{0},"providers":{"agy":{"max_concurrent":state.config.max_concurrent,"max_queue":state.config.max_queue},"openai":{"enabled":state.config.openai_enabled,"max_concurrent":state.config.openai_max_concurrent,"max_queue":state.config.openai_max_queue}}},
        "unsupported":["native","media_input","active_sampling_controls","embeddings","audio","image_generation_api","realtime","batches","fine_tuning","provider_management"],
        "verification":{"adapter_contract":"automated_fixtures","live_direct_provider":"operator_validation_required"},
        "model_conflicts":snapshot.conflicts,
        "per_model":snapshot.entries.iter().map(|e|json!({"id":e.model.id,"provider":e.provider.as_str(),"chat_completions":true,"responses":true,"streaming":true,"client_tools":true,"native":false,"media_input":false,"output_token_budget":{"mode":"prompt_guidance","hard_limit":false}})).collect::<Vec<_>>()
    }))
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
    let remaining = Duration::from_secs(state.config.request_timeout_secs)
        .checked_sub(context.started.elapsed())
        .unwrap_or_default();
    let result = tokio::time::timeout(
        remaining,
        generate_body(state.clone(), key, context.clone(), body, responses),
    )
    .await
    .unwrap_or_else(|_| Err(error(504, "provider_timeout", "Request deadline exceeded")));
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
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("model must be a string"))?
        .to_owned();
    let fallback = state
        .fallback_model(&requested)
        .await
        .map(|served| ModelFallback::new(&requested, &served));
    if let Some(fallback) = &fallback {
        // The response body reports the served ID, as OpenAI does for snapshots.
        body["model"] = json!(fallback.served);
        *context.fallback.lock().unwrap() = Some(fallback.clone());
        context.disclose(
            "x-ai-router-model-fallback",
            format!("{} -> {}", fallback.requested, fallback.served),
        );
    }
    let model_id = fallback.as_ref().map_or(requested.as_str(), |f| &f.served);
    let selected = state.resolve_model(model_id).await?;
    *context.selection.lock().unwrap() = Some((selected.as_str().into(), model_id.to_owned()));
    let pinned_generation = Some(match selected {
        ProviderId::OpenAi => state.openai.generation().await?,
        ProviderId::Agy => state.antigravity.generation().await?,
    });
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
    let has_cache_hints = ["prompt_cache_key", "prompt_cache_retention"]
        .iter()
        .any(|field| body.get(*field).is_some_and(|value| !value.is_null()));
    let wants_reasoning_summary = responses
        && body
            .pointer("/reasoning/summary")
            .is_some_and(|v| !v.is_null());
    // Responses echo request metadata, as OpenAI does. It never reaches a provider.
    let metadata = body
        .get("metadata")
        .filter(|value| !value.is_null())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let id = context.id.clone();
    let mut request = if responses {
        normalize_response(body, id.clone())?
    } else {
        normalize_chat(body, id.clone())?
    };
    // From here the request is valid, so its disclosures describe every
    // response, including provider failures before or after a stream commits.
    let mut disclosures = disclosure_headers(&request.ignored);
    if let Some(mode) = apply_effort_policy(&state, selected, fallback.is_some(), &mut request).await
    {
        disclosures.push(("x-ai-router-reasoning-effort", mode));
    }
    if responses {
        let storage = if store_items { "memory" } else { "disabled" };
        disclosures.push(("x-ai-router-item-storage", storage.into()));
    }
    if has_cache_hints {
        disclosures.push(("x-ai-router-prompt-cache-controls", "ignored".into()));
    }
    if wants_reasoning_summary {
        disclosures.push(("x-ai-router-reasoning-summary", "unavailable".into()));
    }
    if has_token_budget {
        disclosures.push(("x-ai-router-token-budget-mode", "prompt-guidance".into()));
    }
    for (name, value) in disclosures {
        context.disclose(name, value);
    }
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
    let current_generation = match selected {
        ProviderId::OpenAi => state.openai.generation().await?,
        ProviderId::Agy => state.antigravity.generation().await?,
    };
    if Some(current_generation) != pinned_generation {
        return Err(error(
            503,
            "provider_unavailable",
            "The selected provider account changed while queued",
        ));
    }
    let cancel = state.shutdown.child_token();
    let model = request.model.clone();
    eprintln!(
        "{}",
        json!({
            "event":"model_request", "request_id":id,
            "endpoint":if responses { "responses" } else { "chat" }, "model":model,
            "requested_model":fallback.as_ref().map(|f| &f.requested),
            "provider":selected.as_str(),
            "input_item_count":request.input.len(),
            "input_bytes":serde_json::to_vec(&request.input).map_or(0,|v|v.len()),
            "instruction_bytes":serde_json::to_vec(&request.instructions).map_or(0,|v|v.len()),
            "tool_schema_bytes":request.tools.iter().map(|t|t.parameters.to_string().len()).sum::<usize>(),
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
        ProviderId::Agy => state.antigravity.start(request, cancel.clone()).await?,
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
        cancel,
        run.finished.clone(),
        admission,
    );
    guard.record.provider = selected.as_str().into();
    guard.record.requested_model = fallback.map(|f| f.requested);
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
                        echo_metadata(
                            response_object(&completion_id, created, &model, &result, store_items),
                            &metadata,
                        )
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
                    return Ok(Json(output).into_response());
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
    let stream_output = StreamOutput {
        responses,
        store_items,
        owner,
        item_scope,
        completion_id: completion_id.clone(),
        created,
        model: model.clone(),
        metadata: metadata.clone(),
    };
    // Commit the SSE 200 only once the stream has client-visible output (a
    // delta, call or terminal result), never before the provider's Init.
    // Earlier failures, including withheld output that fails validation or a
    // required tool call, keep the non-streaming status, body and headers, as
    // on OpenAI. A stream still silent after the commit wait is committed with
    // keepalives; later failures are events.
    let mut render = StreamRenderer::new(
        responses,
        completion_id.clone(),
        model.clone(),
        created,
        include_usage,
        store_items,
        metadata.clone(),
    );
    let mut pending = render.begin();
    let begun = pending.len();
    let mut terminal = false;
    let deadline = tokio::time::Instant::now() + state.stream_commit_wait;
    while pending.len() == begun {
        let event = match tokio::time::timeout_at(deadline, run.events.recv()).await {
            Err(_) => break,
            Ok(Some(event)) => event,
            Ok(None) => {
                let failure = error(
                    502,
                    "incomplete_provider_output",
                    "Provider ended without a result",
                );
                guard.observe(&RunEvent::Error(failure.clone()));
                return Err(failure);
            }
        };
        let event = settle_stream_event(&state, &mut guard, event, &stream_output);
        if let RunEvent::Error(failure) = event {
            return Err(failure);
        }
        terminal = matches!(&event, RunEvent::Completed(_));
        pending.extend(render.event(event));
    }
    let stream = async_stream::stream! {
        let mut guard = guard;
        for item in pending { yield Ok::<Event,Infallible>(item); }
        while !terminal {
            let Some(event) = run.events.recv().await else { break };
            let event = settle_stream_event(&state, &mut guard, event, &stream_output);
            terminal = matches!(&event, RunEvent::Completed(_) | RunEvent::Error(_));
            for item in render.event(event) { yield Ok(item); }
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
    Ok(response)
}

/// What a streamed result stores and reports, so validation, item storage and
/// telemetry are identical before and after the stream commits.
struct StreamOutput {
    responses: bool,
    store_items: bool,
    owner: String,
    item_scope: String,
    completion_id: String,
    created: i64,
    model: String,
    metadata: Value,
}

fn settle_stream_event(
    state: &AppState,
    guard: &mut CompletionGuard,
    mut event: RunEvent,
    out: &StreamOutput,
) -> RunEvent {
    if let RunEvent::Completed(result) = &event {
        if let Err(error) = validate_result(result) {
            event = RunEvent::Error(error);
        }
    }
    if out.store_items {
        if let RunEvent::Completed(result) = &event {
            let output = echo_metadata(
                response_object(&out.completion_id, out.created, &out.model, result, true),
                &out.metadata,
            );
            if let Err(error) = state.items.save_scoped(&out.owner, &output, &out.item_scope) {
                event = RunEvent::Error(error);
            }
        }
    }
    guard.observe(&event);
    if let RunEvent::Completed(result) = &event {
        let output = if out.responses {
            echo_metadata(
                response_object(
                    &out.completion_id,
                    out.created,
                    &out.model,
                    result,
                    out.store_items,
                ),
                &out.metadata,
            )
        } else {
            chat_object(&out.completion_id, out.created, &out.model, result)
        };
        guard.capture_response(&output);
    }
    event
}

/// Accepted fields with no effect are named in response headers, so a
/// client can tell an ignored hint from an enforced control.
fn disclosure_headers(ignored: &[String]) -> Vec<(&'static str, String)> {
    let mut headers = Vec::new();
    if ignored
        .iter()
        .any(|field| SAMPLING_HINTS.contains(&field.as_str()))
    {
        headers.push(("x-ai-router-sampling-controls", "ignored".to_owned()));
    }
    if ignored.iter().any(|field| field == DROPPED_REASONING) {
        headers.push(("x-ai-router-reasoning-replay", "dropped".to_owned()));
    }
    let mut fields: Vec<&str> = ignored
        .iter()
        .map(String::as_str)
        .filter(|field| !SAMPLING_HINTS.contains(field) && *field != DROPPED_REASONING)
        .collect();
    fields.sort_unstable();
    fields.dedup();
    if !fields.is_empty() {
        headers.push(("x-ai-router-ignored-fields", fields.join(", ")));
    }
    headers
}

/// The one effort decision for a request, made against the served model.
/// At most one `x-ai-router-reasoning-effort` value results, by precedence:
/// 1. Antigravity `none`/`minimal` -> `ignored`: its model ID fixes the effort,
///    so they mean "no explicit effort" (exact or fallback-served ID).
/// 2. A fallback-served Antigravity ID with a different effort ->
///    `mapped=<tier>`: the client chose that effort for the model it asked for.
///    An exact ID's conflict is still rejected by the adapter.
/// 3. OpenAI `minimal` that the catalogue does not list -> `mapped=<level>`,
///    the lowest listed level above it.
/// Anything else reaches the provider unchanged, without the header.
async fn apply_effort_policy(
    state: &AppState,
    provider: ProviderId,
    fallback: bool,
    request: &mut RunRequest,
) -> Option<String> {
    let effort = request.effort.as_deref()?;
    match provider {
        ProviderId::Agy if matches!(effort, "none" | "minimal") => {
            request.effort = None;
            Some("ignored".into())
        }
        ProviderId::Agy if fallback => {
            let tier = request.model.rsplit('-').next()?.to_owned();
            if effort == tier {
                return None;
            }
            request.effort = None;
            Some(format!("mapped={tier}"))
        }
        ProviderId::OpenAi if effort == "minimal" => {
            let applied = state
                .openai
                .minimal_effort_substitute(&request.model)
                .await?;
            request.effort = Some(applied.into());
            Some(format!("mapped={applied}"))
        }
        _ => None,
    }
}

fn echo_metadata(mut output: Value, metadata: &Value) -> Value {
    output["metadata"] = metadata.clone();
    output
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
                requested_model: None,
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
            cancel,
            finished,
            admission: Some(admission),
        }
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
                let mut captured = e.response_body();
                if let Some(diagnostics) = &e.diagnostics {
                    captured["diagnostics"] = diagnostics.metadata.clone();
                    if let Some(body) = &diagnostics.private_body {
                        captured["upstream_response"] = body.clone();
                    }
                }
                self.capture_response(&captured);
                eprintln!(
                    "{}",
                    json!({"event":"provider_request_failed",
                    "request_id":self.record.request_id,"model":self.record.model,"provider":self.record.provider,
                    "endpoint":self.record.endpoint,"elapsed_ms":self.started.elapsed().as_millis() as u64,
                    "status":e.status,"code":e.code,"upstream_request_id":e.upstream_request_id,
                    "explanation":crate::protocol::error_explanation(&self.record.provider, &e.code),
                    "diagnostics":e.diagnostics.as_ref().map(|d| &d.metadata)})
                );
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
        let finished = self.finished.clone();
        let admission = self.admission.take();
        tokio::spawn(async move {
            let _stopped = tokio::time::timeout(Duration::from_secs(30), finished.cancelled())
                .await
                .is_ok();
            state
                .telemetry
                .finish_detail(&record.request_id, response)
                .await;
            state.telemetry.record(record).await;
            drop(admission);
        });
    }
}

struct StreamRenderer {
    responses: bool,
    store_items: bool,
    metadata: Value,
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
        metadata: Value,
    ) -> Self {
        Self {
            responses,
            store_items,
            metadata,
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
        let response = json!({"id":self.id,"object":"response","created_at":self.created,"model":self.model,"status":"in_progress","output":[],"error":null,"incomplete_details":null,"usage":null,"store":self.store_items,"metadata":self.metadata});
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
                    let response = echo_metadata(
                        response_object(
                            &self.id,
                            self.created,
                            &self.model,
                            &result,
                            self.store_items,
                        ),
                        &self.metadata,
                    );
                    events.push(self.emit(kind, json!({"response":response})));
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
                // Official SDKs (and so LangChain) raise only on a top-level
                // `error` object, so it accompanies the spec error fields.
                let mut detail = e.response_body()["error"].take();
                detail["param"] = json!(e.param);
                if self.responses {
                    vec![self.emit("error",json!({"code":e.code,"message":e.message,"param":e.param,"error":detail})),
                        self.emit("response.failed",json!({"response":{"id":self.id,"object":"response","created_at":self.created,"model":self.model,"status":"failed","output":[],"error":{"code":e.code,"message":e.message,"upstream_request_id":e.upstream_request_id},"usage":null}}))]
                } else {
                    vec![
                        self.emit("error", json!({"error":detail})),
                        Event::default().data("[DONE]"),
                    ]
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
        let state = AppState::new(config.clone(), Antigravity::new(config), telemetry);
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
    async fn local_saturation_is_a_retryable_503_while_key_limits_stay_429() {
        let (_temp, state) = test_state();
        let _held = state.admit_provider(ProviderId::OpenAi).await.unwrap();
        let Err(busy) = state.admit_provider(ProviderId::OpenAi).await else {
            panic!("a saturated queue admitted a request");
        };
        assert_eq!((busy.status, busy.code.as_str()), (503, "busy"));
        let response = busy.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["retry-after"], "10");
        assert!(response.headers().get("x-should-retry").is_none());
        state.stop();

        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::for_test(temp.path());
        config.per_key_per_minute = 1;
        let telemetry = Arc::new(
            Telemetry::open(
                &config.telemetry_dir,
                config.retention_days,
                config.max_events,
            )
            .unwrap(),
        );
        let config = Arc::new(config);
        let state = AppState::new(config.clone(), Antigravity::new(config), telemetry);
        let app = router(state.clone());
        let mut statuses = Vec::new();
        for _ in 0..2 {
            use tower::ServiceExt;
            let request = Request::builder()
                .uri("/v1/models")
                .header("authorization", "Bearer test-key")
                .body(axum::body::Body::empty())
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            statuses.push((
                response.status(),
                response.headers().get("retry-after").cloned(),
            ));
        }
        assert_eq!(statuses[0], (StatusCode::OK, None));
        assert_eq!(
            statuses[1],
            (StatusCode::TOO_MANY_REQUESTS, Some("10".parse().unwrap()))
        );
        state.stop();
    }
    #[tokio::test]
    async fn disabled_openai_and_unauthenticated_antigravity_are_not_ready() {
        let (_temp, state) = test_state();
        state.publish_openai_monitor_status().await;
        let monitor_status = state.telemetry.snapshot().await.openai_provider;
        assert!(!monitor_status.authenticated);
        assert_eq!(monitor_status.error.as_deref(), Some("provider_disabled"));
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
            "agy_auth_required"
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
        if input.contains("upstream-auth-fixture") {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error":{"code":"invalid_token","message":"synthetic-provider-private-detail"}})),
            )
                .into_response();
        }
        if input.contains("overload-fixture") {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [("x-request-id", "synthetic-overload-id")],
                Json(json!({"error":{
                "code":"server_is_overloaded","message":"synthetic-provider-private-detail"}})),
            )
                .into_response();
        }
        if input.contains("auth-fixture") {
            return (StatusCode::UNAUTHORIZED, Json(json!({"error":{"code":"invalid_api_key","message":"synthetic-provider-private-detail"}}))).into_response();
        }
        if input.contains("midstream-fixture") {
            let wire = [
                json!({"type":"response.output_text.delta","delta":"partial "}),
                json!({"type":"response.failed","response":{"error":{"code":"server_error","message":"synthetic-provider-private-detail"}}}),
            ]
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>();
            return ([("content-type", "text/event-stream")], wire).into_response();
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
        } else if body["tool_choice"] == "required"
            && !continuing
            && !input.contains("no-call-fixture")
        {
            // An upstream that honours forcing calls the first offered tool.
            let tool = &body["input"][0]["tools"][0];
            let arguments = synthetic_arguments(&tool["parameters"]).to_string();
            let call_id = format!("call_forced_{}", captured.lock().await.len());
            (
                String::new(),
                vec![
                    json!({"type":"function_call","call_id":call_id,"name":tool["name"],"arguments":arguments,"status":"completed"}),
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
    // Fills only required fields; enough for a fixture model to obey forcing.
    fn synthetic_arguments(schema: &Value) -> Value {
        match schema["type"].as_str() {
            Some("object") => Value::Object(
                schema["required"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(|name| {
                        (
                            name.to_owned(),
                            synthetic_arguments(&schema["properties"][name]),
                        )
                    })
                    .collect(),
            ),
            Some("array") => json!([]),
            Some("integer" | "number") => json!(0),
            Some("boolean") => json!(false),
            _ => json!("synthetic"),
        }
    }
    // Independent legacy CCA validator: a permissive mock previously hid
    // production INVALID_ARGUMENT errors for ordinary n8n JSON Schemas.
    fn valid_cca_schema(value: &Value) -> bool {
        let Some(fields) = value.as_object() else {
            return false;
        };
        let Some(kind) = fields.get("type").and_then(Value::as_str) else {
            return false;
        };
        fields.iter().all(|(key, value)| match key.as_str() {
            "type" => value.as_str().is_some_and(|s| {
                matches!(
                    s,
                    "object" | "array" | "string" | "boolean" | "integer" | "number"
                )
            }),
            "title" | "description" => value.is_string(),
            "enum" | "required" => {
                let expected = if key == "enum" { "string" } else { "object" };
                kind == expected
                    && value
                        .as_array()
                        .is_some_and(|items| items.iter().all(Value::is_string))
            }
            "properties" => {
                kind == "object"
                    && value
                        .as_object()
                        .is_some_and(|map| map.values().all(valid_cca_schema))
            }
            "items" => kind == "array" && valid_cca_schema(value),
            _ => false,
        })
    }
    async fn mock_direct_response(Json(body): Json<Value>) -> Response {
        assert!(
            body.get("requestType").is_none(),
            "reviewed Antigravity envelope must omit requestType"
        );
        if let Some(tools) = body.pointer("/request/tools").and_then(Value::as_array) {
            for tool in tools {
                for declaration in tool["functionDeclarations"].as_array().unwrap() {
                    if !valid_cca_schema(&declaration["parameters"]) {
                        return (StatusCode::BAD_REQUEST, Json(json!({"error":{"code":400,
                            "status":"INVALID_ARGUMENT","message":"Unknown name in function_declarations.parameters: $schema/additionalProperties"}}))).into_response();
                    }
                }
            }
        }
        // Gemini's reviewed CCA profile uses ordered groups without wire IDs.
        if !body["model"].as_str().unwrap().starts_with("claude-") {
            for turn in body["request"]["contents"].as_array().unwrap() {
                for part in turn["parts"].as_array().unwrap() {
                    assert!(part.pointer("/functionCall/id").is_none());
                    assert!(part.pointer("/functionResponse/id").is_none());
                }
            }
        }
        let text = body.to_string();
        if text.contains("direct-rejection-fixture") {
            return (StatusCode::BAD_REQUEST, [("x-request-id","synthetic-upstream-id")], Json(json!({"error":{
                "code":400,"status":"INVALID_ARGUMENT","message":"Unknown name $schema; private echoed prompt",
                "details":[{"fieldViolations":[{"field":"request.tools[0].parameters","description":"additionalProperties unsupported"}]}]
            }}))).into_response();
        }
        if text.contains("direct-quota-fixture") {
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
        if text.contains("overload-fixture") {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        if text.contains("auth-fixture") {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        if text.contains("midstream-fixture") {
            // Separate chunks, so the partial text is delivered before the
            // malformed frame fails the decoder.
            let wire = async_stream::stream! {
                yield Ok::<_, Infallible>(format!("data: {}\n\n", json!({"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"partial "}]}}]}})));
                tokio::time::sleep(Duration::from_millis(50)).await;
                yield Ok("data: not-json\n\n".to_owned());
            };
            return (
                [("content-type", "text/event-stream")],
                axum::body::Body::from_stream(wire),
            )
                .into_response();
        }
        if text.contains("direct-protocol-fixture") {
            return ([("content-type", "text/event-stream")], "data: {}\n\n").into_response();
        }
        if text.contains("direct-invalid-schema-fixture") {
            return ([("content-type","text/event-stream")],"data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"not JSON\"}]},\"finishReason\":\"STOP\"}]}}\n\n").into_response();
        }

        let continuing = body
            .pointer("/request/contents")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .any(|turn| {
                turn["parts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|part| part.get("functionResponse").is_some())
            });
        let parts = if text.contains("tools-fixture")
            && !continuing
            && body.pointer("/request/tools").is_some()
        {
            json!([{"text":"private reasoning","thought":true,"thoughtSignature":"c2lnbmF0dXJl"},
                {"functionCall":{"name":"lookup","args":{"q":"one"}}},
                {"functionCall":{"name":"lookup","args":{"q":"two"}}}])
        } else if text.contains("This turn must call a tool.")
            && !continuing
            && !text.contains("no-call-fixture")
        {
            // A model that follows the forcing instruction calls the first tool.
            let declaration = &body["request"]["tools"][0]["functionDeclarations"][0];
            json!([{"functionCall":{"name":declaration["name"],
                "args":synthetic_arguments(&declaration["parameters"])}}])
        } else {
            json!([{"text":if text.contains("schema-fixture") {"{\"answer\":\"valid\"}"} else if continuing {"tool results accepted"} else {"Hello from fixture"}}])
        };
        let wire = format!(
            "data: {}\n\n",
            json!({"response":{"candidates":[{"content":{"role":"model","parts":parts},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":6,"totalTokenCount":10}}})
        );
        ([("content-type", "text/event-stream")], wire).into_response()
    }
    impl DualHarness {
        async fn new() -> Self {
            Self::with_timeout(5).await
        }
        async fn with_timeout(timeout_seconds: u64) -> Self {
            Self::configured(|config| config.request_timeout_secs = timeout_seconds).await
        }
        async fn with_commit_wait(timeout_seconds: u64, commit_wait: Duration) -> Self {
            Self::configured_with_commit_wait(
                |config| config.request_timeout_secs = timeout_seconds,
                commit_wait,
            )
            .await
        }
        async fn configured(adjust: impl FnOnce(&mut Config)) -> Self {
            Self::configured_with_commit_wait(adjust, STREAM_COMMIT_WAIT).await
        }
        async fn configured_with_commit_wait(
            adjust: impl FnOnce(&mut Config),
            commit_wait: Duration,
        ) -> Self {
            let root = tempfile::tempdir().unwrap();
            let mut config = Config::for_test(root.path());
            config.openai_enabled = true;
            use sha2::{Digest, Sha256};
            config.keys.push(crate::config::KeyConfig {
                id: "other".into(),
                sha256: format!("{:x}", Sha256::digest(b"other-test-key")),
                scopes: vec!["model".into()],
            });
            config.request_timeout_secs = 5;
            config.preauth_per_minute = 10000;
            config.global_preauth_per_minute = 10000;
            config.per_key_per_minute = 10000;
            adjust(&mut config);
            let config = Arc::new(config);
            let captured = Arc::new(tokio::sync::Mutex::new(Vec::new()));
            let model_mode = Arc::new(AtomicUsize::new(0));
            let mode = model_mode.clone();
            let mock = Router::new().route("/models", get(move || {
                let mode=mode.clone();
                async move {
                    if mode.load(Ordering::SeqCst)==2 { tokio::time::sleep(Duration::from_secs(1)).await; }
                    if mode.load(Ordering::SeqCst)==1 { return StatusCode::SERVICE_UNAVAILABLE.into_response(); }
                    Json(json!({"models":[{"slug":"synthetic-openai","display_name":"Synthetic OpenAI","visibility":"list",
                        "supported_reasoning_levels":[{"effort":"low"},{"effort":"medium"},{"effort":"high"}]}]})).into_response()
                }
            }))
                .route("/responses", post(mock_openai_response))
                .route("/v1internal:fetchAvailableModels",post(||async {Json(json!({"models":{"gemini-4-argon-medium":{"displayName":"Gemini fixture"},"claude-sonnet-5-5-high":{"displayName":"Claude fixture"}}}))}))
                .route("/v1internal:streamGenerateContent",post(mock_direct_response))
                .with_state(captured.clone());
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
            let mut state =
                AppState::new(config.clone(), Antigravity::new(config.clone()), telemetry);
            Arc::get_mut(&mut state).unwrap().antigravity =
                Antigravity::synthetic(config.clone(), format!("http://{address}")).await;
            Arc::get_mut(&mut state).unwrap().openai = OpenAi::synthetic(
                config,
                format!("http://{address}"),
                "synthetic-openai-token".into(),
            );
            Arc::get_mut(&mut state).unwrap().stream_commit_wait = commit_wait;
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
        async fn send(
            &self,
            method: axum::http::Method,
            path: &str,
            body: Option<Value>,
            key: Option<&str>,
        ) -> Response {
            use tower::ServiceExt;
            let mut request = Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json");
            if let Some(key) = key {
                request = request.header("authorization", format!("Bearer {key}"));
            }
            self.app
                .clone()
                .oneshot(
                    request
                        .body(axum::body::Body::from(
                            body.map(|v| v.to_string()).unwrap_or_default(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap()
        }
        async fn json(
            &self,
            method: axum::http::Method,
            path: &str,
            body: Option<Value>,
            key: Option<&str>,
        ) -> (StatusCode, Value) {
            use http_body_util::BodyExt;
            let response = self.send(method, path, body, key).await;
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice(&bytes).unwrap())
        }
        async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
            self.json(axum::http::Method::POST, path, Some(body), Some("test-key"))
                .await
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
    async fn openai_overload_has_actionable_errors_on_both_public_apis_without_retry() {
        for responses in [false, true] {
            for streaming in [false, true] {
                let h = DualHarness::new().await;
                let (path, body) = if responses {
                    (
                        "/v1/responses",
                        json!({"model":"synthetic-openai","input":"overload-fixture","stream":streaming}),
                    )
                } else {
                    (
                        "/v1/chat/completions",
                        json!({"model":"synthetic-openai","messages":[{"role":"user","content":"overload-fixture"}],"stream":streaming}),
                    )
                };
                let (status, wire) = h.call(path, Some(body)).await;
                // Streams commit only after provider acceptance, so both modes
                // return the same HTTP failure.
                assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
                assert!(wire.contains("server_is_overloaded"));
                assert!(wire.contains("OpenAI is temporarily overloaded"));
                assert!(wire.contains("did not retry automatically"));
                assert!(wire.contains("synthetic-overload-id"));
                assert!(!wire.contains("synthetic-provider-private-detail"));
                assert_eq!(h.captured.lock().await.len(), 1);
                assert_eq!(
                    h.state.openai.availability_error().await.unwrap().code,
                    "server_is_overloaded"
                );
                let error: Value = serde_json::from_str(&wire).unwrap();
                assert_eq!(error["error"]["code"], "server_is_overloaded");
                // Overload clears through normal discovery, without re-login.
                h.state.refresh_openai().await;
                let (status, wire) = h
                    .call(
                        "/v1/responses",
                        Some(json!({"model":"synthetic-openai","input":"hello"})),
                    )
                    .await;
                assert_eq!(status, StatusCode::OK, "{wire}");
            }
        }
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
        // The catalogue probe remains healthy, but the latched usage limit
        // must be reflected in the monitor's effective status.
        assert!(h.state.openai_provider.read().await.authenticated);
        h.state.publish_openai_monitor_status().await;
        let monitor_status = h.state.telemetry.snapshot().await.openai_provider;
        assert!(!monitor_status.authenticated);
        assert_eq!(
            monitor_status.error.as_deref(),
            Some("subscription_sharing_usage_limit_exceeded")
        );
        // A successful model discovery cannot clear a plan-usage latch.
        h.state.refresh_openai().await;
        let monitor_status = h.state.telemetry.snapshot().await.openai_provider;
        assert!(!monitor_status.authenticated);
        assert_eq!(
            monitor_status.error.as_deref(),
            Some("subscription_sharing_usage_limit_exceeded")
        );
        let (status, wire) = h.call("/v1/capabilities", None).await;
        assert_eq!(status, StatusCode::OK, "{wire}");
        let capabilities: Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(capabilities["providers"]["openai"]["ready"], false);
        assert_eq!(
            capabilities["providers"]["openai"]["error"],
            "subscription_sharing_usage_limit_exceeded"
        );
        let (_, wire) = h.call("/v1/models", None).await;
        assert!(!wire.contains("synthetic-openai"));
        assert!(wire.contains("gemini-4-argon-medium"));
        let (status, wire) = h
            .call(
                "/v1/chat/completions",
                Some(
                    json!({"model":"gemini-4-argon-medium","messages":[{"role":"user","content":"hello"}]}),
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
    async fn monitor_openai_status_rejects_a_catalogue_from_another_generation() {
        let h = DualHarness::new().await;
        {
            let mut status = h.state.openai_provider.write().await;
            status.registration_generation = Some("synthetic-old-generation".into());
        }
        h.state.publish_openai_monitor_status().await;
        let monitor_status = h.state.telemetry.snapshot().await.openai_provider;
        assert!(!monitor_status.authenticated);
        assert_eq!(
            monitor_status.error.as_deref(),
            Some("provider_account_changed")
        );
    }

    #[tokio::test]
    async fn probes_publish_independently_and_outages_isolate_in_both_directions() {
        let h = DualHarness::new().await;
        *h.state.provider.write().await = ProviderStatus::default();
        let (_, wire) = h.call("/v1/models", None).await;
        assert!(wire.contains("synthetic-openai"));
        assert!(!wire.contains("gemini-4-argon-medium"));
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
        assert!(wire.contains("gemini-4-argon-medium"));
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
        // The provider never accepted the request, so the timeout is HTTP.
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{wire}");
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
    #[tokio::test]
    async fn official_sdks_cover_direct_gateway_over_real_tcp() {
        use tokio::io::AsyncWriteExt;
        let h = DualHarness::new().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = h.app.clone();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for (program, path, model) in [
            ("python3", "tests/sdk_contract.py", "gemini-4-argon-medium"),
            ("python3", "tests/sdk_contract.py", "claude-sonnet-5-5-high"),
            ("node", "tests/n8n_contract.mjs", "gemini-4-argon-medium"),
            ("node", "tests/n8n_contract.mjs", "claude-sonnet-5-5-high"),
            ("node", "tests/studio_contract.mjs", "synthetic-openai"),
            ("node", "tests/studio_contract.mjs", "gemini-4-argon-medium"),
            (
                "node",
                "tests/n8n_langchain_contract.mjs",
                "gemini-4-argon-medium",
            ),
            (
                "node",
                "tests/n8n_langchain_contract.mjs",
                "claude-sonnet-5-5-high",
            ),
        ] {
            let mut child = tokio::process::Command::new(program)
                .arg(path)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(
                    json!({"base_url":format!("http://{address}/v1"),"model":model})
                        .to_string()
                        .as_bytes(),
                )
                .await
                .unwrap();
            let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
                .await
                .unwrap()
                .unwrap();
            assert!(
                output.status.success(),
                "{program}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            println!(
                "{}: {}",
                model,
                String::from_utf8_lossy(&output.stdout).trim()
            );
        }
        server.abort();
        h.state.stop();
        assert!(h.state.drain(Duration::from_secs(2)).await);
    }

    #[test]
    fn cca_mock_rejects_original_n8n_schema_and_accepts_projection() {
        let schema = json!({"$schema":"http://json-schema.org/draft-07/schema#","type":"object",
            "properties":{"$schema":{"type":"string"}},"additionalProperties":false});
        assert!(!valid_cca_schema(&schema));
        assert!(valid_cca_schema(&crate::antigravity_schema::parameters(
            &schema
        )));
    }

    #[test]
    fn cca_mock_rejects_type_incompatible_fields_in_nested_tool_schemas() {
        for items in [
            json!({"type":"string","properties":{"name":{"type":"string"}}}),
            json!({"type":"string","required":["name"]}),
            json!({"type":"object","items":{"type":"string"}}),
            json!({"type":"object","enum":["name"]}),
        ] {
            let schema =
                json!({"type":"object","properties":{"nodeTypes":{"type":"array","items":items}}});
            assert!(!valid_cca_schema(&schema));
            assert!(valid_cca_schema(&crate::antigravity_schema::parameters(
                &schema
            )));
        }
    }

    #[tokio::test]
    async fn provider_rejection_has_private_details_but_generic_public_error() {
        use http_body_util::BodyExt;
        let h = DualHarness::new().await;
        for streaming in [false, true] {
            let response = h
                .send(
                    axum::http::Method::POST,
                    "/v1/responses",
                    Some(json!({"model":"gemini-4-argon-medium",
                "input":"direct-rejection-fixture","stream":streaming})),
                    Some("test-key"),
                )
                .await;
            let id = response.headers()["x-request-id"]
                .to_str()
                .unwrap()
                .to_owned();
            // A rejection before provider acceptance is HTTP in both modes.
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let wire = String::from_utf8(
                response
                    .into_body()
                    .collect()
                    .await
                    .unwrap()
                    .to_bytes()
                    .to_vec(),
            )
            .unwrap();
            assert!(wire.contains("synthetic-upstream-id"));
            assert!(!wire.contains("private echoed prompt"));
            assert!(!wire.contains("upstream_response"));
            assert!(h.state.drain(Duration::from_secs(2)).await);
            let detail = h
                .state
                .telemetry
                .request_detail(&id)
                .await
                .unwrap()
                .response
                .unwrap();
            let captured: Value = serde_json::from_str(&detail.json).unwrap();
            assert!(
                captured["upstream_response"]["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("private echoed prompt")
            );
            assert_eq!(
                captured["diagnostics"]["provider_status"],
                "INVALID_ARGUMENT"
            );
        }
        h.state.telemetry.flush().await;
        let persisted =
            std::fs::read_to_string(h.state.config.telemetry_dir.join("telemetry.json")).unwrap();
        assert!(!persisted.contains("private echoed prompt"));
        assert!(!persisted.contains("direct-rejection-fixture"));
        assert!(!persisted.contains("fieldViolations"));
        let (status, _) = h
            .call(
                "/v1/responses",
                Some(json!({"model":"gemini-4-argon-medium","input":"hello"})),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn stale_antigravity_snapshot_hides_only_its_models() {
        let h = DualHarness::new().await;
        h.state.provider.write().await.checked_at =
            (Utc::now() - chrono::Duration::seconds(181)).to_rfc3339();
        let (_, body) = h.call("/v1/models", None).await;
        assert!(!body.contains("gemini-4-argon-medium"));
        assert!(body.contains("synthetic-openai"));
        assert_eq!(
            h.state
                .resolve_model("gemini-4-argon-medium")
                .await
                .unwrap_err()
                .code,
            "provider_unavailable"
        );
    }
    #[tokio::test]
    async fn direct_quota_protocol_and_schema_failures_have_distinct_scope() {
        let h = DualHarness::new().await;
        let request = |input: &str| json!({"model":"gemini-4-argon-medium","input":input});
        let (status, _) = h
            .call("/v1/responses", Some(request("direct-quota-fixture")))
            .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(h.state.antigravity.availability_error().await.is_none());
        assert_eq!(
            h.state
                .antigravity
                .model_availability("gemini-4-argon-medium")
                .await
                .unwrap_err()
                .status,
            429
        );
        let (status, _) = h
            .call(
                "/v1/responses",
                Some(json!({"model":"claude-sonnet-5-5-high","input":"hello"})),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        let h = DualHarness::new().await;
        let mut body = request("direct-invalid-schema-fixture");
        body["text"] =
            json!({"format":{"type":"json_schema","name":"x","schema":{"type":"object"}}});
        assert_eq!(
            h.call("/v1/responses", Some(body)).await.0,
            StatusCode::BAD_GATEWAY
        );
        assert!(h.state.antigravity.availability_error().await.is_none());
        assert_eq!(
            h.call("/v1/responses", Some(request("hello"))).await.0,
            StatusCode::OK
        );
        assert_eq!(
            h.call("/v1/responses", Some(request("direct-protocol-fixture")))
                .await
                .0,
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            h.state.antigravity.availability_error().await.unwrap().code,
            "agy_protocol_blocked"
        );
        assert_eq!(
            h.call(
                "/v1/responses",
                Some(json!({"model":"synthetic-openai","input":"hello"}))
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    fn failure_request(
        model: &str,
        api: &str,
        fixture: &str,
        stream: bool,
    ) -> (&'static str, Value) {
        if api == "responses" {
            (
                "/v1/responses",
                json!({"model":model,"input":fixture,"stream":stream}),
            )
        } else {
            (
                "/v1/chat/completions",
                json!({"model":model,"messages":[{"role":"user","content":fixture}],"stream":stream}),
            )
        }
    }
    fn sse_events(wire: &str) -> Vec<(Option<String>, Value)> {
        wire.split("\n\n")
            .filter_map(|frame| {
                let event = frame
                    .lines()
                    .find_map(|l| l.strip_prefix("event:"))
                    .map(|e| e.trim().to_owned());
                let data = frame.lines().find_map(|l| l.strip_prefix("data:"))?.trim();
                Some((
                    event,
                    serde_json::from_str(data).unwrap_or_else(|_| json!(data)),
                ))
            })
            .collect()
    }
    async fn last_record(h: &DualHarness) -> RequestRecord {
        for _ in 0..150 {
            if let Some(record) = h.state.telemetry.snapshot().await.records.last() {
                return record.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("request was not recorded");
    }
    #[tokio::test]
    async fn streamed_failures_before_provider_acceptance_are_real_http_errors() {
        for (model, fixture) in [
            ("synthetic-openai", "overload-fixture"),
            ("synthetic-openai", "auth-fixture"),
            ("synthetic-openai", "quota-fixture"),
            ("gemini-4-argon-medium", "overload-fixture"),
            ("gemini-4-argon-medium", "auth-fixture"),
            ("gemini-4-argon-medium", "direct-quota-fixture"),
        ] {
            for api in ["chat", "responses"] {
                let h = DualHarness::new().await;
                let (path, body) = failure_request(model, api, fixture, false);
                let plain = h
                    .send(axum::http::Method::POST, path, Some(body), Some("test-key"))
                    .await;
                let h = DualHarness::new().await;
                let (path, body) = failure_request(model, api, fixture, true);
                let streamed = h
                    .send(axum::http::Method::POST, path, Some(body), Some("test-key"))
                    .await;
                // Before deferral this was 200 + response.created/in_progress
                // + response.failed, which LangChain and n8n ignore.
                assert_ne!(streamed.status(), StatusCode::OK, "{model} {fixture} {api}");
                assert_eq!(streamed.status(), plain.status(), "{model} {fixture} {api}");
                for header in ["content-type", "retry-after", "x-ai-router-item-storage"] {
                    assert_eq!(
                        streamed.headers().get(header),
                        plain.headers().get(header),
                        "{header}"
                    );
                }
                use http_body_util::BodyExt;
                let plain: Value =
                    serde_json::from_slice(&plain.into_body().collect().await.unwrap().to_bytes())
                        .unwrap();
                let wire = streamed.into_body().collect().await.unwrap().to_bytes();
                let streamed: Value = serde_json::from_slice(&wire).unwrap();
                assert_eq!(streamed, plain);
                assert!(
                    !String::from_utf8_lossy(&wire).contains("synthetic-provider-private-detail")
                );
                // Monitor and telemetry classify it exactly like the nonstreaming failure.
                let record = last_record(&h).await;
                assert_eq!(record.status, "failed");
                assert_eq!(
                    record.error_code.as_deref(),
                    streamed["error"]["code"].as_str()
                );
                assert_eq!(
                    record.provider,
                    if model == "synthetic-openai" {
                        "openai"
                    } else {
                        "agy"
                    }
                );
            }
        }
    }
    #[tokio::test]
    async fn midstream_failures_emit_spec_error_event_then_terminal_failure() {
        for model in ["synthetic-openai", "gemini-4-argon-medium"] {
            for api in ["chat", "responses"] {
                let h = DualHarness::new().await;
                let (path, body) = failure_request(model, api, "midstream-fixture", true);
                let (status, wire) = h.call(path, Some(body)).await;
                assert_eq!(status, StatusCode::OK, "{wire}");
                assert!(!wire.contains("synthetic-provider-private-detail"));
                let events = sse_events(&wire);
                let record = last_record(&h).await;
                assert_eq!(record.status, "failed");
                let code = record.error_code.unwrap();
                if api == "responses" {
                    let kinds = events
                        .iter()
                        .map(|(kind, _)| kind.clone().unwrap())
                        .collect::<Vec<_>>();
                    assert_eq!(&kinds[..2], ["response.created", "response.in_progress"]);
                    assert_eq!(
                        &kinds[kinds.len() - 2..],
                        ["error", "response.failed"],
                        "{wire}"
                    );
                    assert!(kinds.contains(&"response.output_text.delta".to_owned()));
                    assert!(
                        events
                            .iter()
                            .enumerate()
                            .all(|(i, (_, event))| event["sequence_number"] == i)
                    );
                    let error = &events[kinds.len() - 2].1;
                    // ResponseErrorEvent fields, plus the object the SDKs raise on.
                    assert_eq!(error["type"], "error");
                    assert_eq!(error["code"], code.as_str());
                    assert_eq!(error["param"], Value::Null);
                    assert!(error["message"].is_string());
                    assert_eq!(error["error"]["code"], code.as_str());
                    assert_eq!(error["error"]["message"], error["message"]);
                    assert_eq!(error["error"]["type"], "server_error");
                    let failed = &events.last().unwrap().1["response"];
                    assert_eq!(failed["status"], "failed");
                    assert_eq!(failed["error"]["code"], code.as_str());
                } else {
                    assert_eq!(events[0].1["choices"][0]["delta"]["role"], "assistant");
                    assert_eq!(events[1].1["choices"][0]["delta"]["content"], "partial ");
                    let error = &events[events.len() - 2].1["error"];
                    assert_eq!(error["code"], code.as_str());
                    assert_eq!(error["type"], "server_error");
                    assert_eq!(error["param"], Value::Null);
                    assert_eq!(events.last().unwrap().1, "[DONE]");
                }
            }
        }
    }
    #[tokio::test]
    async fn slow_provider_acceptance_commits_the_stream_after_the_bound() {
        let h = DualHarness::with_commit_wait(5, Duration::from_millis(100)).await;
        let started = Instant::now();
        let response = h
            .send(axum::http::Method::POST, "/v1/responses",
                Some(json!({"model":"synthetic-openai","input":"deadline-fixture overload-fixture","stream":true})), Some("test-key"))
            .await;
        // Headers arrive at the bound, before the provider's 700 ms answer.
        assert!(started.elapsed() < Duration::from_millis(600));
        assert_eq!(response.status(), StatusCode::OK);
        use http_body_util::BodyExt;
        let wire = String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        let kinds = sse_events(&wire)
            .into_iter()
            .map(|(kind, _)| kind.unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                "response.created",
                "response.in_progress",
                "error",
                "response.failed"
            ]
        );
        assert!(wire.contains("server_is_overloaded"));
    }
    #[tokio::test]
    async fn client_disconnect_is_recorded_before_and_after_stream_commit() {
        let h = DualHarness::new().await;
        let pending = h.send(
            axum::http::Method::POST,
            "/v1/responses",
            Some(json!({"model":"synthetic-openai","input":"deadline-fixture","stream":true})),
            Some("test-key"),
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(200), pending)
                .await
                .is_err()
        );
        let record = last_record(&h).await;
        assert_eq!(
            (record.status.as_str(), record.error_code.as_deref()),
            ("cancelled", Some("client_disconnected"))
        );
        assert!(h.state.drain(Duration::from_secs(2)).await);
        let h = DualHarness::with_commit_wait(5, Duration::from_millis(100)).await;
        let response = h.send(axum::http::Method::POST, "/v1/chat/completions",
            Some(json!({"model":"synthetic-openai","messages":[{"role":"user","content":"deadline-fixture"}],"stream":true})), Some("test-key")).await;
        assert_eq!(response.status(), StatusCode::OK);
        drop(response);
        let record = last_record(&h).await;
        assert_eq!(
            (record.status.as_str(), record.error_code.as_deref()),
            ("cancelled", Some("client_disconnected"))
        );
        assert!(h.state.drain(Duration::from_secs(2)).await);
    }
    // Each client gets a fresh gateway per failure: overload and auth failures
    // latch provider availability, which would mask the streamed path.
    #[tokio::test]
    async fn official_n8n_and_langchain_clients_raise_streamed_failures() {
        use tokio::io::AsyncWriteExt;
        let mut runs = 0;
        for model in ["synthetic-openai", "gemini-4-argon-medium"] {
            for fixture in ["overload-fixture", "auth-fixture", "midstream-fixture"] {
                for api in ["chat", "responses"] {
                    let h = DualHarness::new().await;
                    let (path, body) = failure_request(model, api, fixture, true);
                    let (status, wire) = h.call(path, Some(body)).await;
                    let expected = if status == StatusCode::OK {
                        json!({"status":null,"code":last_record(&h).await.error_code})
                    } else {
                        json!({"status":status.as_u16(),"code":serde_json::from_str::<Value>(&wire).unwrap()["error"]["code"]})
                    };
                    let clients = [
                        ("python3", "tests/sdk_contract.py"),
                        ("node", "tests/n8n_contract.mjs"),
                        ("node", "tests/n8n_langchain_contract.mjs"),
                    ]
                    .map(|(program, script)| {
                        let failure = json!({"fixture":fixture,"api":api,"status":expected["status"],"code":expected["code"]});
                        async move {
                            let h = DualHarness::new().await;
                            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                            let address = listener.local_addr().unwrap();
                            let app = h.app.clone();
                            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
                            let mut child = tokio::process::Command::new(program)
                                .arg(script)
                                .stdin(std::process::Stdio::piped())
                                .stdout(std::process::Stdio::piped())
                                .stderr(std::process::Stdio::piped())
                                .kill_on_drop(true)
                                .spawn()
                                .unwrap();
                            let input = json!({"base_url":format!("http://{address}/v1"),"model":model,"failure":failure});
                            child.stdin.take().unwrap().write_all(input.to_string().as_bytes()).await.unwrap();
                            let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output()).await.unwrap().unwrap();
                            server.abort();
                            let stdout = String::from_utf8_lossy(&output.stdout);
                            assert!(output.status.success() && stdout.contains("streamed failure"), "{script} {model} {failure}: {stdout} {}",
                                String::from_utf8_lossy(&output.stderr));
                        }
                    });
                    futures_util::future::join_all(clients).await;
                    runs += 3;
                }
            }
        }
        assert_eq!(runs, 36);
    }
    mod compatibility_contract {
        include!("api_contract_tests.rs");
    }
}
