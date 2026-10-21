//! ChatGPT plan inference through the public Responses API. No CLI credential reuse.
use crate::{config::Config, openai_auth::AuthStore, protocol::*, provider_stream::RunStream};
use chrono::Utc;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

const BASE: &str = "https://api.openai.com/v1";
const EVENT_LIMIT: usize = 1024 * 1024;
const REPLAY_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct OpenAi {
    config: Arc<Config>,
    auth: AuthStore,
    client: reqwest::Client,
    discovered_generation: Arc<Mutex<Option<String>>>,
    reasoning_levels: Arc<Mutex<HashMap<String, Vec<String>>>>,
    replay: Arc<Mutex<VecDeque<Replay>>>,
    local_block: Arc<Mutex<Option<(String, ApiError)>>>,
    #[cfg(test)]
    test_base: Option<String>,
    #[cfg(test)]
    test_token: Option<String>,
}

struct Replay {
    owner: String,
    call_id: String,
    generation: String,
    items: Vec<Value>,
    created: Instant,
    bytes: usize,
}

impl OpenAi {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            auth: AuthStore::new(config.openai_state_dir.clone()),
            config,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .pool_max_idle_per_host(4)
                .build()
                .expect("TLS HTTP client"),
            discovered_generation: Arc::new(Mutex::new(None)),
            reasoning_levels: Arc::new(Mutex::new(HashMap::new())),
            replay: Arc::new(Mutex::new(VecDeque::new())),
            local_block: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            test_base: None,
            #[cfg(test)]
            test_token: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn synthetic(config: Arc<Config>, base: String, token: String) -> Self {
        let parsed = url::Url::parse(&base).expect("synthetic URL");
        assert_eq!(parsed.scheme(), "http");
        assert_eq!(parsed.host_str(), Some("127.0.0.1"));
        let mut provider = Self::new(config);
        provider.test_base = Some(base);
        provider.test_token = Some(token);
        provider
    }

    fn url(&self, path: &str) -> String {
        #[cfg(test)]
        if let Some(base) = &self.test_base {
            return format!("{base}/{path}");
        }
        format!("{BASE}/{path}")
    }

    pub async fn generation(&self) -> Result<String, ApiError> {
        if !self.config.openai_enabled {
            return Err(failure(503, "provider_disabled"));
        }
        #[cfg(test)]
        if self.test_token.is_some() {
            return Ok("synthetic-generation".into());
        }
        self.auth.generation().await
    }

    async fn token(&self) -> Result<(String, String), ApiError> {
        #[cfg(test)]
        if let Some(token) = &self.test_token {
            return Ok((token.clone(), "synthetic-generation".into()));
        }
        let token = self.auth.token().await?;
        Ok((token.access_token, token.generation))
    }

    pub async fn availability_error(&self) -> Option<ApiError> {
        if !self.config.openai_enabled {
            return Some(failure(503, "provider_disabled"));
        }
        if let Some((blocked_generation, error)) = self.local_block.lock().await.as_ref() {
            if self.generation().await.as_ref().ok() == Some(blocked_generation) {
                return Some(error.clone());
            }
        }
        #[cfg(test)]
        if self.test_token.is_some() {
            return None;
        }
        if let Some(error) = self.auth.availability_error().await {
            return Some(error);
        }
        match self.generation().await {
            Ok(generation)
                if self.discovered_generation.lock().await.as_ref() == Some(&generation) =>
            {
                None
            }
            Ok(_) => Some(failure(503, "provider_discovery_required")),
            Err(error) => Some(error),
        }
    }

    pub async fn probe(&self) -> ProviderStatus {
        let mut status = ProviderStatus {
            version: "chatgpt-plan-v1".into(),
            checked_at: Utc::now().to_rfc3339(),
            ..Default::default()
        };
        if !self.config.openai_enabled {
            status.error = Some("provider_disabled".into());
            return status;
        }
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            let (token, generation) = self.token().await?;
            let response = self
                .client
                .get(self.url("models"))
                .bearer_auth(token)
                .send()
                .await
                .map_err(|_| failure(503, "provider_transport_error"))?;
            let code = response.status().as_u16();
            let body = bounded_body(response, EVENT_LIMIT).await?;
            if !(200..300).contains(&code) {
                return Err(self.http_error(code, &body, Some(&generation)).await);
            }
            let body: Value = serde_json::from_slice(&body)
                .map_err(|_| failure(502, "invalid_provider_catalogue"))?;
            let models = parse_models(&body)?;
            *self.reasoning_levels.lock().await = parse_reasoning_levels(&body);
            *self.discovered_generation.lock().await = Some(generation.clone());
            Ok::<_, ApiError>((models, generation))
        })
        .await
        .unwrap_or_else(|_| Err(failure(503, "provider_probe_timeout")));
        match result {
            Ok((models, generation)) => {
                {
                    let mut block = self.local_block.lock().await;
                    if block.as_ref().is_some_and(|(blocked_generation, e)| {
                        blocked_generation != &generation || !durable_restriction(&e.code)
                    }) {
                        *block = None;
                    }
                }
                status.registration_generation = Some(generation);
                status.authenticated = true;
                status.models = models;
                status.error = None;
            }
            Err(error) => {
                status.error = Some(error.code);
            }
        }
        status
    }

    /// The effort to send instead of `minimal`, when the discovered catalogue
    /// lists this model's levels without it: the lowest listed level above
    /// `minimal`. Unlisted levels are forwarded for the upstream to decide.
    pub async fn minimal_effort_substitute(&self, model: &str) -> Option<&'static str> {
        let levels = self.reasoning_levels.lock().await;
        let levels = levels.get(model)?;
        if levels.iter().any(|level| level == "minimal") {
            return None;
        }
        ["low", "medium", "high", "xhigh", "max"]
            .into_iter()
            .find(|effort| levels.iter().any(|level| level == effort))
    }

    async fn latch_transient(&self, generation: &str, error: &ApiError) {
        let mut block = self.local_block.lock().await;
        // A request timeout or concurrent network failure cannot turn a
        // durable plan restriction into a probe-recoverable transport failure.
        if block
            .as_ref()
            .is_some_and(|(_, blocked_error)| durable_restriction(&blocked_error.code))
        {
            return;
        }
        *block = Some((generation.to_owned(), error.clone()));
    }

    async fn http_error(&self, status: u16, body: &[u8], generation: Option<&str>) -> ApiError {
        let value = serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
        let error = upstream_error(status, &value);
        if transient_availability(&error) || matches!(error.status, 401 | 403 | 500..=599) {
            if let Some(generation) = generation {
                self.latch_transient(generation, &error).await;
            }
        }
        if matches!(
            error.code.as_str(),
            "subscription_sharing_usage_limit_exceeded"
                | "subscription_sharing_user_not_eligible"
                | "chatpass_v2_scope_not_authorized"
                | "chatpass_v2_invalid_authorization_context"
                | "subscription_sharing_invalid_user"
        ) {
            if let Some(generation) = generation {
                *self.local_block.lock().await = Some((generation.to_owned(), error.clone()));
            }
            #[cfg(test)]
            if self.test_token.is_some() {
                return error;
            }
            // Keep an in-memory latch even if durable state fails; do not let a
            // successful model-list response authorize another charged request.
            if let Some(generation) = generation {
                let _ = self
                    .auth
                    .block_for_generation(&error.code, generation)
                    .await;
            }
        }
        error
    }

    pub async fn start(
        &self,
        request: RunRequest,
        cancel: CancellationToken,
    ) -> Result<RunStream, ApiError> {
        if !self.config.openai_enabled {
            return Err(failure(503, "provider_disabled"));
        }
        // Validate before token renewal or any billable request.
        let _ = serialize(&request)?;
        let (sender, events) = mpsc::channel(32);
        let finished = CancellationToken::new();
        let completion = finished.clone();
        let this = self.clone();
        tokio::spawn(async move {
            // Retain response metadata outside the execution future so timeout,
            // cancellation, and every parser failure preserve a known request ID.
            let upstream_request_id = Mutex::new(None);
            let work = this.execute(&request, &sender, &upstream_request_id);
            let outcome = tokio::select! {
                _ = cancel.cancelled() => Err(failure(499, "request_cancelled")),
                _ = sender.closed() => Err(failure(499, "request_cancelled")),
                error = this.watch_generation(request.provider_generation.as_deref()) => Err(error),
                result = tokio::time::timeout(Duration::from_millis(request.remaining_ms.unwrap_or(this.config.request_timeout_secs * 1000)), work) =>
                    result.unwrap_or_else(|_| Err(failure(504, "provider_timeout"))),
            };
            if let Err(mut error) = outcome {
                if error.upstream_request_id.is_none() {
                    error.upstream_request_id = upstream_request_id.lock().await.clone();
                }
                if transient_availability(&error) {
                    if let Some(generation) = &request.provider_generation {
                        this.latch_transient(generation, &error).await;
                    }
                }
                let _ = tokio::time::timeout(
                    Duration::from_secs(1),
                    sender.send(RunEvent::Error(error)),
                )
                .await;
            }
            completion.cancel();
        });
        Ok(RunStream { events, finished })
    }

    async fn watch_generation(&self, expected: Option<&str>) -> ApiError {
        let Some(expected) = expected else {
            return std::future::pending().await;
        };
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Ok(Ok(current)) =
                tokio::time::timeout(Duration::from_millis(100), self.generation()).await
            {
                if current != expected {
                    return failure(503, "provider_account_changed");
                }
            }
        }
    }

    async fn execute(
        &self,
        request: &RunRequest,
        sender: &mpsc::Sender<RunEvent>,
        upstream_request_id: &Mutex<Option<String>>,
    ) -> Result<(), ApiError> {
        if let Some(error) = self.availability_error().await {
            return Err(error);
        }
        let (token, generation) = self.token().await?;
        if request
            .provider_generation
            .as_ref()
            .is_some_and(|expected| expected != &generation)
        {
            return Err(failure(503, "provider_account_changed"));
        }
        let mut body = serialize(request)?;
        if request.endpoint == "chat" {
            let mut replay = self.replay.lock().await;
            replay.retain(|r| r.created.elapsed() < Duration::from_secs(86400));
            let mut input = Vec::new();
            let mut seen = HashSet::new();
            for item in body["input"].as_array().unwrap() {
                if item["type"] == "function_call" {
                    if let Some(entry) = replay.iter().find(|r| {
                        r.owner == request.owner
                            && r.generation == generation
                            && item["call_id"] == r.call_id
                    }) {
                        for reasoning in &entry.items {
                            if seen.insert(reasoning["id"].to_string()) {
                                input.push(reasoning.clone());
                            }
                        }
                    }
                }
                input.push(item.clone());
            }
            body["input"] = json!(order_replay(input));
        }
        if body.to_string().len() > self.config.max_body_bytes {
            return Err(failure(413, "input_too_large"));
        }
        let response = self
            .client
            .post(self.url("responses"))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .map_err(|_| failure(503, "provider_transport_error"))?;
        let status = response.status().as_u16();
        let request_id = response
            .headers()
            .get("x-request-id")
            .or_else(|| response.headers().get("openai-request-id"))
            .and_then(|v| v.to_str().ok())
            .filter(|v| v.len() <= 256 && v.bytes().all(|b| b.is_ascii_graphic()))
            .map(str::to_owned);
        *upstream_request_id.lock().await = request_id.clone();
        if !(200..300).contains(&status) {
            let body = bounded_body(response, EVENT_LIMIT).await?;
            let mut error = self.http_error(status, &body, Some(&generation)).await;
            error.upstream_request_id = request_id;
            return Err(error);
        }
        // The public subscription route can omit Content-Type while returning
        // SSE. Missing metadata is accepted only by the strict event decoder;
        // an explicitly incompatible media type still fails closed.
        if response.headers().get("content-type").is_some_and(|value| {
            !value.to_str().is_ok_and(|value| {
                value
                    .split(';')
                    .next()
                    .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
            })
        }) {
            // Some upstream gateways return a structured failure with a 2xx
            // status. Classify it without exposing the body, while retaining
            // the documented requirement that successful inference uses SSE.
            let body = bounded_body(response, EVENT_LIMIT).await?;
            if serde_json::from_slice::<Value>(&body)
                .is_ok_and(|value| value.get("error").is_some_and(|error| !error.is_null()))
            {
                return Err(self.http_error(502, &body, Some(&generation)).await);
            }
            return Err(failure(502, "invalid_provider_content_type"));
        }
        send(
            sender,
            RunEvent::Init {
                conversation_id: request.request_id.clone(),
                tools: request.tools.iter().map(|t| t.name.clone()).collect(),
            },
        )
        .await?;
        let mut stream = response.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut finished_items = FinishedItems::new(self.config.max_output_bytes);
        let mut bytes = 0usize;
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| failure(502, "provider_stream_interrupted"))?;
            bytes = bytes.saturating_add(chunk.len());
            if bytes > self.config.max_output_bytes {
                return Err(failure(502, "output_limit_exceeded"));
            }
            for data in decoder.push(&chunk)? {
                let event: Value = serde_json::from_str(&data)
                    .map_err(|_| failure(502, "invalid_provider_event"))?;
                match event["type"].as_str() {
                    Some("response.output_item.done") => finished_items.observe(&event)?,
                    Some("response.output_text.delta") => {
                        let delta = event["delta"]
                            .as_str()
                            .ok_or_else(|| failure(502, "invalid_provider_event"))?;
                        text.push_str(delta);
                        if !request.withholds_text() {
                            send(
                                sender,
                                RunEvent::TextDelta {
                                    delta: delta.into(),
                                },
                            )
                            .await?;
                        }
                    }
                    Some("response.completed" | "response.incomplete") => {
                        let terminal = event
                            .get("response")
                            .ok_or_else(|| failure(502, "invalid_provider_event"))?;
                        if (event["type"] == "response.completed"
                            && terminal["status"] != "completed")
                            || (event["type"] == "response.incomplete"
                                && terminal["status"] != "incomplete")
                        {
                            return Err(failure(502, "invalid_provider_terminal"));
                        }
                        let terminal = finished_items.reconstruct(terminal)?;
                        let mut result = parse_result(request, &terminal, &text)?;
                        result.upstream_request_id = request_id;
                        if request.endpoint == "chat"
                            && !result.output_items.is_empty()
                            && !result.tool_calls.is_empty()
                        {
                            let mut replay = self.replay.lock().await;
                            let size = result
                                .output_items
                                .iter()
                                .map(|v| v.to_string().len())
                                .sum::<usize>();
                            for call in &result.tool_calls {
                                replay.push_back(Replay {
                                    owner: request.owner.clone(),
                                    call_id: call.id.clone(),
                                    generation: generation.clone(),
                                    items: result.output_items.clone(),
                                    created: Instant::now(),
                                    bytes: size,
                                });
                            }
                            while replay.len() > 4096
                                || replay.iter().map(|r| r.bytes).sum::<usize>() > 64 * 1024 * 1024
                            {
                                replay.pop_front();
                            }
                            while replay
                                .iter()
                                .filter(|r| r.owner == request.owner)
                                .map(|r| r.bytes)
                                .sum::<usize>()
                                > REPLAY_LIMIT
                            {
                                if let Some(index) =
                                    replay.iter().position(|r| r.owner == request.owner)
                                {
                                    replay.remove(index);
                                } else {
                                    break;
                                }
                            }
                        }
                        if !result.tool_calls.is_empty()
                            && !result.text.is_empty()
                            && (request.withholds_text() || text.is_empty())
                        {
                            send(
                                sender,
                                RunEvent::TextDelta {
                                    delta: result.text.clone(),
                                },
                            )
                            .await?;
                        }
                        for call in &result.tool_calls {
                            send(sender, RunEvent::ToolCall(call.clone())).await?;
                        }
                        send(sender, RunEvent::Completed(result)).await?;
                        return Ok(());
                    }
                    Some("response.failed" | "error") => {
                        let error = event.get("response").unwrap_or(&event);
                        let mut error = self
                            .http_error(502, &serde_json::to_vec(error).unwrap(), Some(&generation))
                            .await;
                        error.upstream_request_id = request_id;
                        return Err(error);
                    }
                    // Partial function-argument events are deliberately withheld until
                    // terminal parsing and schema validation prevent unsafe handoffs.
                    Some(_) => {}
                    None => return Err(failure(502, "invalid_provider_event")),
                }
            }
        }
        Err(failure(502, "incomplete_provider_output"))
    }
}

// The subscription stream can omit already-finished items from its terminal
// response. Only complete output_item.done objects may fill those omissions;
// argument deltas never become executable tool calls by themselves.
struct FinishedItems {
    items: BTreeMap<usize, Value>,
    ids: HashMap<String, usize>,
    bytes: usize,
    max_bytes: usize,
}
impl FinishedItems {
    fn new(max_bytes: usize) -> Self {
        Self {
            items: BTreeMap::new(),
            ids: HashMap::new(),
            bytes: 0,
            max_bytes,
        }
    }
    fn observe(&mut self, event: &Value) -> Result<(), ApiError> {
        let index = event["output_index"]
            .as_u64()
            .filter(|index| *index < 1000)
            .ok_or_else(|| failure(502, "invalid_provider_output_index"))?
            as usize;
        let item = event
            .get("item")
            .filter(|item| item.is_object())
            .ok_or_else(|| failure(502, "invalid_provider_output"))?;
        let id = output_item_id(item)?.ok_or_else(|| failure(502, "invalid_provider_output"))?;
        if let Some(existing) = self.items.get(&index) {
            return if existing == item {
                Ok(())
            } else {
                Err(failure(502, "inconsistent_provider_output"))
            };
        }
        if self.ids.contains_key(id) {
            return Err(failure(502, "inconsistent_provider_output"));
        }
        let bytes = self.bytes.saturating_add(item.to_string().len());
        if bytes > self.max_bytes {
            return Err(failure(502, "output_limit_exceeded"));
        }
        self.bytes = bytes;
        self.ids.insert(id.to_owned(), index);
        self.items.insert(index, item.clone());
        Ok(())
    }
    fn reconstruct(&self, terminal: &Value) -> Result<Value, ApiError> {
        let output = terminal["output"]
            .as_array()
            .filter(|output| output.len() <= 1000)
            .ok_or_else(|| failure(502, "invalid_provider_output"))?;
        let mut merged = self.items.clone();
        let mut terminal_ids = HashSet::new();
        let mut last_terminal_index = None;
        for (position, item) in output.iter().enumerate() {
            let id = output_item_id(item)?;
            if id.is_some_and(|id| !terminal_ids.insert(id)) {
                return Err(failure(502, "inconsistent_provider_output"));
            }
            // A partial terminal array can omit a reasoning prefix, shifting
            // its array positions. Its stable item ID retains the stream index.
            let index = id
                .and_then(|id| self.ids.get(id))
                .copied()
                .unwrap_or(position);
            if last_terminal_index.is_some_and(|previous| index <= previous) {
                return Err(failure(502, "inconsistent_provider_output"));
            }
            last_terminal_index = Some(index);
            if let Some(existing) = merged.get(&index) {
                if existing != item {
                    return Err(failure(502, "inconsistent_provider_output"));
                }
            } else {
                merged.insert(index, item.clone());
            }
        }
        let mut ids = HashSet::new();
        let mut bytes = 0usize;
        for (expected_index, (index, item)) in merged.iter().enumerate() {
            if expected_index != *index {
                return Err(failure(502, "invalid_provider_output_index"));
            }
            if let Some(id) = output_item_id(item)? {
                if !ids.insert(id) {
                    return Err(failure(502, "inconsistent_provider_output"));
                }
            }
            bytes = bytes.saturating_add(item.to_string().len());
            if bytes > self.max_bytes {
                return Err(failure(502, "output_limit_exceeded"));
            }
        }
        let mut terminal = terminal.clone();
        terminal["output"] = json!(merged.into_values().collect::<Vec<_>>());
        Ok(terminal)
    }
}
fn output_item_id(item: &Value) -> Result<Option<&str>, ApiError> {
    match item.get("id") {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .filter(|id| {
                !id.is_empty() && id.len() <= 256 && id.bytes().all(|b| b.is_ascii_graphic())
            })
            .map(Some)
            .ok_or_else(|| failure(502, "invalid_provider_output")),
    }
}

async fn send(sender: &mpsc::Sender<RunEvent>, event: RunEvent) -> Result<(), ApiError> {
    sender
        .send(event)
        .await
        .map_err(|_| failure(499, "request_cancelled"))
}

async fn bounded_body(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, ApiError> {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| failure(502, "provider_transport_error"))?;
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(failure(502, "output_limit_exceeded"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn parse_models(value: &Value) -> Result<Vec<ModelInfo>, ApiError> {
    let list = value["models"]
        .as_array()
        .filter(|v| v.len() <= 500)
        .ok_or_else(|| failure(502, "invalid_provider_catalogue"))?;
    let mut models = Vec::new();
    let mut seen = HashSet::new();
    for entry in list.iter().filter(|m| m["visibility"] == "list") {
        let id = entry["slug"]
            .as_str()
            .filter(|s| {
                !s.is_empty()
                    && s.len() <= 128
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b))
            })
            .ok_or_else(|| failure(502, "invalid_provider_catalogue"))?;
        if !seen.insert(id) {
            return Err(failure(502, "invalid_provider_catalogue"));
        }
        let name = entry["display_name"].as_str().unwrap_or(id);
        models.push(ModelInfo {
            id: id.into(),
            name: name.chars().filter(|c| !c.is_control()).take(128).collect(),
        });
    }
    Ok(models)
}

/// Catalogue entries may list `supported_reasoning_levels` as effort strings
/// or `{effort, description}` presets. Unknown or absent lists are omitted.
fn parse_reasoning_levels(value: &Value) -> HashMap<String, Vec<String>> {
    const EFFORTS: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];
    let mut levels = HashMap::new();
    for entry in value["models"].as_array().into_iter().flatten().take(500) {
        let Some(id) = entry["slug"].as_str() else {
            continue;
        };
        let listed: Vec<String> = entry["supported_reasoning_levels"]
            .as_array()
            .into_iter()
            .flatten()
            .take(16)
            .filter_map(|level| level.as_str().or_else(|| level["effort"].as_str()))
            .filter(|level| EFFORTS.contains(level))
            .map(str::to_owned)
            .collect();
        if !listed.is_empty() {
            levels.insert(id.to_owned(), listed);
        }
    }
    levels
}

fn serialize(request: &RunRequest) -> Result<Value, ApiError> {
    let mut input = Vec::new();
    for instruction in &request.instructions {
        input.push(json!({"role":"developer", "content": content_text(&instruction["content"])?}));
    }
    for item in &request.input {
        match item["type"].as_str().unwrap_or("message") {
            "reasoning" => {
                // Only encrypted state is replayed; summaries are optional and never instructions.
                let mut item = item.clone();
                item.as_object_mut().unwrap().remove("status");
                input.push(item);
            }
            "function_call" => input.push(json!({"type":"function_call", "call_id":item["call_id"], "name":item["name"], "arguments":item["arguments"]})),
            "function_call_output" => input.push(json!({"type":"function_call_output", "call_id":item["call_id"], "output":content_text(&item["output"])?})),
            "message" => {
                let role = item["role"].as_str().ok_or_else(|| ApiError::bad_request("Missing message role"))?;
                if role == "tool" {
                    input.push(json!({"type":"function_call_output", "call_id":item["tool_call_id"], "output":content_text(&item["content"])?}));
                    continue;
                }
                let mapped_role = if role == "system" { "developer" } else { role };
                let content = content_text(&item["content"])?;
                if !content.is_empty() || item.get("tool_calls").is_none() {
                    input.push(json!({"role":mapped_role, "content":content}));
                }
                if let Some(calls) = item["tool_calls"].as_array() {
                    for call in calls { input.push(json!({"type":"function_call", "call_id":call["id"], "name":call["function"]["name"], "arguments":call["function"]["arguments"]})); }
                }
            }
            _ => return Err(ApiError::unsupported("input item")),
        }
    }
    // Gateway streaming may append opaque reasoning after visible outputs.
    // Restore provider state before the assistant output group on replay,
    // without moving it across a user message or tool-result boundary.
    let mut input = order_replay(input);
    if input.is_empty() {
        return Err(ApiError::bad_request("Conversation input is required"));
    }
    if request.schema.is_some() {
        input.insert(0, json!({"role":"developer", "content":format!("Return only valid JSON satisfying this JSON Schema. Do not use Markdown fences: {}", request.schema.as_ref().unwrap())}));
    }
    let mut body = json!({"model":request.model,"input":input,"store":false,"stream":true,"parallel_tool_calls":request.parallel_tool_calls,"include":["reasoning.encrypted_content"]});
    if !request.tools.is_empty() {
        // additional_tools is the documented subscription route's dynamic tool catalogue.
        let definitions: Vec<_> = request.tools.iter().map(|t| json!({"type":"function", "name":t.name,"description":t.description,"parameters":t.parameters,"strict":false})).collect();
        body["input"].as_array_mut().unwrap().insert(
            0,
            json!({"type":"additional_tools", "role":"developer", "tools":definitions}),
        );
        // Named and allowed-tools choices are already narrowed to this
        // catalogue, so "required" expresses each of them without naming a
        // tool upstream. Local validation still fails closed without a call.
        if request.tool_required {
            body["tool_choice"] = json!("required");
        }
    }
    if let Some(effort) = &request.effort {
        body["reasoning"] = json!({"effort":effort});
    }
    Ok(body)
}

fn order_replay(input: Vec<Value>) -> Vec<Value> {
    let mut ordered = Vec::new();
    let mut turn_start = 0;
    for item in input {
        if item["type"] == "reasoning" {
            ordered.insert(turn_start, item);
            turn_start += 1;
        } else {
            let boundary = item["role"] == "user" || item["type"] == "function_call_output";
            ordered.push(item);
            if boundary {
                turn_start = ordered.len();
            }
        }
    }
    ordered
}

fn content_text(value: &Value) -> Result<String, ApiError> {
    if value.is_null() {
        return Ok(String::new());
    }
    if let Some(text) = value.as_str() {
        return Ok(text.into());
    }
    let parts = value
        .as_array()
        .ok_or_else(|| ApiError::bad_request("Expected text content"))?;
    let mut out = String::new();
    for part in parts {
        let text = part["text"]
            .as_str()
            .ok_or_else(|| ApiError::unsupported("non-text input"))?;
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(text);
    }
    Ok(out)
}

fn parse_result(
    request: &RunRequest,
    response: &Value,
    streamed_text: &str,
) -> Result<RunResult, ApiError> {
    let status = response["status"]
        .as_str()
        .filter(|s| matches!(*s, "completed" | "incomplete"))
        .ok_or_else(|| failure(502, "invalid_provider_terminal"))?;
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut ids = HashSet::new();
    let mut opaque = Vec::new();
    let output = response["output"]
        .as_array()
        .ok_or_else(|| failure(502, "invalid_provider_output"))?;
    if output.len() > 1000 {
        return Err(failure(502, "output_limit_exceeded"));
    }
    for item in output {
        match item["type"].as_str() {
            Some("message") => {
                for part in item["content"]
                    .as_array()
                    .ok_or_else(|| failure(502, "invalid_provider_output"))?
                {
                    if part["type"] == "output_text" {
                        text.push_str(
                            part["text"]
                                .as_str()
                                .ok_or_else(|| failure(502, "invalid_provider_output"))?,
                        );
                    } else if part["type"] == "refusal" {
                        text.push_str(part["refusal"].as_str().unwrap_or("Request declined"));
                    } else {
                        return Err(failure(502, "unsupported_provider_output"));
                    }
                }
            }
            Some("function_call") if status == "completed" => {
                let id = item["call_id"]
                    .as_str()
                    .filter(|s| {
                        !s.is_empty() && s.len() <= 256 && s.bytes().all(|b| b.is_ascii_graphic())
                    })
                    .ok_or_else(|| failure(502, "invalid_provider_tool_call"))?;
                if !ids.insert(id) {
                    return Err(failure(502, "invalid_provider_tool_call"));
                }
                let name = item["name"].as_str().unwrap_or("");
                if item.get("status").is_some_and(|s| s != "completed") {
                    return Err(failure(502, "invalid_provider_tool_call"));
                }
                if !request.tools.iter().any(|t| t.name == name) {
                    return Err(failure(502, "invalid_provider_tool_call"));
                }
                let args: Value = serde_json::from_str(item["arguments"].as_str().unwrap_or(""))
                    .map_err(|_| failure(502, "invalid_provider_tool_call"))?;
                crate::tool_diagnostics::validate_call(&request.tools, item.get("name"), &args)
                    .map_err(|_| failure(502, "invalid_provider_tool_call"))?;
                calls.push(ToolCall {
                    id: id.into(),
                    name: name.into(),
                    arguments: args,
                });
            }
            Some("function_call") => {} // Never hand off incomplete arguments.
            Some("reasoning") => {
                if item.get("encrypted_content").is_some_and(Value::is_string) {
                    let id = item["id"]
                        .as_str()
                        .filter(|id| !id.is_empty() && id.len() <= 256)
                        .ok_or_else(|| failure(502, "invalid_provider_output"))?;
                    opaque.push(json!({"type":"reasoning","id":id,"summary":[],"encrypted_content":item["encrypted_content"]}));
                }
            }
            _ => return Err(failure(502, "unsupported_provider_output")),
        }
    }
    if !request.parallel_tool_calls && calls.len() > 1 {
        return Err(failure(502, "invalid_provider_tool_call"));
    }
    if request.tool_required && calls.is_empty() {
        // The unmet field, as for tool_choice validation errors.
        return Err(failure(502, "missing_required_tool_call").with_param("tool_choice"));
    }
    if text.is_empty() && !streamed_text.is_empty() {
        text = streamed_text.into();
    }
    if !streamed_text.is_empty() && text != streamed_text {
        return Err(failure(502, "inconsistent_provider_output"));
    }
    let structured_output = if let Some(schema) = &request.schema {
        if status != "completed" {
            return Err(failure(502, "incomplete_structured_output"));
        }
        let value: Value =
            serde_json::from_str(&text).map_err(|_| failure(502, "invalid_structured_output"))?;
        let validator =
            jsonschema::validator_for(schema).map_err(|_| failure(500, "invalid_schema"))?;
        if !validator.is_valid(&value) {
            return Err(failure(502, "invalid_structured_output"));
        }
        Some(value)
    } else {
        None
    };
    let usage = response
        .get("usage")
        .filter(|u| !u.is_null())
        .map(|u| {
            Ok::<_, ApiError>(Usage {
                input_tokens: u["input_tokens"]
                    .as_u64()
                    .ok_or_else(|| failure(502, "invalid_provider_usage"))?,
                output_tokens: u["output_tokens"]
                    .as_u64()
                    .ok_or_else(|| failure(502, "invalid_provider_usage"))?,
                total_tokens: u["total_tokens"]
                    .as_u64()
                    .ok_or_else(|| failure(502, "invalid_provider_usage"))?,
                thinking_tokens: u
                    .pointer("/output_tokens_details/reasoning_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                cache_read_tokens: u
                    .pointer("/input_tokens_details/cached_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            })
        })
        .transpose()?;
    Ok(RunResult {
        text,
        structured_output,
        conversation_id: response["id"]
            .as_str()
            .unwrap_or(&request.request_id)
            .into(),
        usage_partial: usage.is_none() || status != "completed",
        usage,
        tool_calls: calls,
        status: status.into(),
        output_items: opaque,
        upstream_request_id: None,
    })
}

fn upstream_error(http_status: u16, value: &Value) -> ApiError {
    let raw = value
        .pointer("/error/code")
        .or_else(|| value.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let code = if !raw.is_empty()
        && raw.len() <= 96
        && raw.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        raw
    } else {
        "provider_request_failed"
    };
    let status = match code {
        "server_is_overloaded" => 503,
        "subscription_sharing_usage_limit_exceeded" => 429,
        "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" => 503,
        // Upstream authorization or plan problems are not the client's router
        // key. SDKs read 401/403 as "your API key is wrong", so the provider is
        // reported unavailable while the code keeps the explanation.
        "subscription_sharing_user_not_eligible"
        | "subscription_sharing_route_not_supported"
        | "chatpass_v2_scope_not_authorized"
        | "chatpass_v2_invalid_authorization_context"
        | "subscription_sharing_invalid_user" => 503,
        "subscription_sharing_unsupported_capability" => 400,
        _ if matches!(http_status, 401 | 403) => 503,
        _ => http_status,
    };
    failure(status, code)
}

fn durable_restriction(code: &str) -> bool {
    matches!(
        code,
        "subscription_sharing_usage_limit_exceeded"
            | "subscription_sharing_user_not_eligible"
            | "chatpass_v2_scope_not_authorized"
            | "chatpass_v2_invalid_authorization_context"
            | "subscription_sharing_invalid_user"
    )
}
fn transient_availability(error: &ApiError) -> bool {
    matches!(
        error.code.as_str(),
        "provider_transport_error"
            | "provider_stream_interrupted"
            | "provider_timeout"
            | "subscription_sharing_usage_unavailable"
            | "subscription_sharing_user_unavailable"
    ) || (error.code == "provider_request_failed" && matches!(error.status, 401 | 403 | 503))
}

fn failure(status: u16, code: &str) -> ApiError {
    ApiError::new(
        status,
        code,
        crate::protocol::error_explanation("openai", code).unwrap_or(
            "OpenAI subscription provider could not complete this request; consult private provider status",
        ),
    )
}

#[derive(Default)]
struct SseDecoder {
    line: Vec<u8>,
    data: Vec<String>,
    size: usize,
}
impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, ApiError> {
        let mut events = Vec::new();
        for byte in bytes {
            if *byte != b'\n' {
                self.line.push(*byte);
                if self.line.len() + self.size > EVENT_LIMIT {
                    return Err(failure(502, "provider_event_too_large"));
                }
                continue;
            }
            if self.line.last() == Some(&b'\r') {
                self.line.pop();
            }
            let line = std::str::from_utf8(&self.line)
                .map_err(|_| failure(502, "invalid_provider_utf8"))?;
            if line.is_empty() {
                if !self.data.is_empty() {
                    events.push(self.data.join("\n"));
                }
                self.data.clear();
                self.size = 0;
            } else if let Some(data) = line.strip_prefix("data:") {
                let data = data.strip_prefix(' ').unwrap_or(data);
                self.size += data.len();
                self.data.push(data.into());
            }
            self.line.clear();
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> RunRequest {
        normalize_chat(json!({"model":"synthetic-openai", "messages":[{"role":"system","content":"Be useful"},{"role":"user","content":"Hi"}]}),"synthetic".into()).unwrap()
    }
    #[test]
    fn native_history_and_contract_fields() {
        let mut req = request();
        req.tools = vec![ToolDefinition {
            name: "lookup".into(),
            description: "Find".into(),
            parameters: json!({"type":"object","properties":{"q":{"type":"string"}},"required":["q"],"additionalProperties":false}),
        }];
        let body = serialize(&req).unwrap();
        assert_eq!(
            body["input"][1],
            json!({"role":"developer","content":"Be useful"})
        );
        assert_eq!(body["input"][2], json!({"role":"user","content":"Hi"}));
        assert_eq!(body["input"][0]["type"], "additional_tools");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert!(body.get("temperature").is_none());
        assert!(body.get("max_output_tokens").is_none());
    }
    #[test]
    fn catalogue_reasoning_levels_accept_presets_and_strings_only_when_known() {
        let levels = parse_reasoning_levels(&json!({"models":[
            {"slug":"presets","supported_reasoning_levels":[{"effort":"low","description":"Fast"},{"effort":"high"}]},
            {"slug":"strings","supported_reasoning_levels":["minimal","medium","unknown"]},
            {"slug":"unlisted"},
            {"slug":"empty","supported_reasoning_levels":[{"effort":"turbo"}]}]}));
        assert_eq!(levels["presets"], ["low", "high"]);
        assert_eq!(levels["strings"], ["minimal", "medium"]);
        assert!(!levels.contains_key("unlisted") && !levels.contains_key("empty"));
    }
    #[test]
    fn account_model_shape_and_visibility() {
        let result = parse_models(&json!({"models":[{"slug":"synthetic","display_name":"Test","visibility":"list"},{"slug":"hidden","visibility":"hide"}]})).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "synthetic");
        assert!(parse_models(&json!({"data":[{"id":"wrong-api-key-shape"}]})).is_err());
    }
    #[test]
    fn decoder_handles_split_utf8_crlf_and_multiline() {
        let bytes =
            "event: test\r\ndata: {\"type\":\"event\",\r\ndata: \"text\":\"é\"}\r\n\r\n".as_bytes();
        let mut decoder = SseDecoder::default();
        let mut events = vec![];
        for byte in bytes {
            events.extend(decoder.push(&[*byte]).unwrap());
        }
        assert_eq!(events.len(), 1);
        assert_eq!(
            serde_json::from_str::<Value>(&events[0]).unwrap()["text"],
            "é"
        );
    }
    #[test]
    fn decoder_bounds_unterminated_line() {
        assert!(
            SseDecoder::default()
                .push(&vec![b'x'; EVENT_LIMIT + 1])
                .is_err()
        );
    }
    #[test]
    fn validates_tools_and_keeps_actual_usage() {
        let mut req = request();
        req.tools = vec![ToolDefinition {
            name: "lookup".into(),
            description: String::new(),
            parameters: json!({"type":"object","required":["q"],"properties":{"q":{"type":"string"}}}),
        }];
        let mut terminal = json!({"id":"synthetic","status":"completed","output":[{"type":"function_call","call_id":"c1","name":"lookup","arguments":"{\"q\":\"yes\"}"},{"type":"function_call","call_id":"c2","name":"lookup","arguments":"{\"q\":\"two\"}"}],"usage":{"input_tokens":10,"output_tokens":20,"total_tokens":30,"output_tokens_details":{"reasoning_tokens":12}}});
        let result = parse_result(&req, &terminal, "").unwrap();
        assert_eq!(result.tool_calls.len(), 2);
        assert_eq!(result.usage.unwrap().total_tokens, 30);
        terminal["output"][0]["arguments"] = json!("{\"q\":12}");
        assert!(parse_result(&req, &terminal, "").is_err());
        terminal["output"][0]["name"] = json!("undeclared");
        assert!(parse_result(&req, &terminal, "").is_err());
    }
    #[test]
    fn required_tool_choice_is_forwarded_and_text_alone_fails_closed() {
        let mut req = request();
        req.tools = vec![ToolDefinition {
            name: "lookup".into(),
            description: String::new(),
            parameters: json!({"type":"object","required":["q"],"properties":{"q":{"type":"string"}}}),
        }];
        assert!(serialize(&req).unwrap().get("tool_choice").is_none());
        req.tool_required = true;
        assert!(req.withholds_text());
        assert_eq!(serialize(&req).unwrap()["tool_choice"], "required");
        let call = json!({"type":"function_call","call_id":"c1","name":"lookup","arguments":"{\"q\":\"yes\"}"});
        let terminal = json!({"status":"completed","output":[call]});
        assert_eq!(
            parse_result(&req, &terminal, "").unwrap().tool_calls.len(),
            1
        );
        let text = json!({"type":"message","content":[{"type":"output_text","text":"No call"}]});
        for terminal in [
            json!({"status":"completed","output":[text]}),
            json!({"status":"incomplete","output":[call]}),
        ] {
            let error = parse_result(&req, &terminal, "").unwrap_err();
            assert_eq!(
                (error.status, error.code.as_str()),
                (502, "missing_required_tool_call")
            );
        }
    }
    #[test]
    fn incomplete_never_hands_off_tools_and_missing_usage_is_partial() {
        let result=parse_result(&request(),&json!({"status":"incomplete","output":[{"type":"function_call","call_id":"x","arguments":"{"}]}),"").unwrap();
        assert_eq!(result.status, "incomplete");
        assert!(result.tool_calls.is_empty());
        assert!(result.usage.is_none());
        assert!(result.usage_partial);
    }
    #[test]
    fn schema_and_stream_consistency() {
        let mut req = request();
        req.schema =
            Some(json!({"type":"object","required":["ok"],"properties":{"ok":{"type":"boolean"}}}));
        let terminal = json!({"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"{\"ok\":true}"}]}]});
        assert!(
            parse_result(&req, &terminal, "{\"ok\":true}")
                .unwrap()
                .structured_output
                .is_some()
        );
        assert!(parse_result(&req, &terminal, "mismatch").is_err());
    }
    #[test]
    fn errors_dont_expose_provider_body() {
        let e = upstream_error(
            502,
            &json!({"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"synthetic-token-must-not-escape"}}),
        );
        assert_eq!(e.status, 429);
        assert!(!e.message.contains("synthetic-token"));
        assert!(e.message.contains("for this app or plan"));
        assert!(e.message.contains("https://chatgpt.com/settings/usage"));
    }

    #[tokio::test]
    async fn overload_errors_explain_the_upstream_failure_without_retry_or_private_messages() {
        let private = "synthetic-private-provider-message";
        let error = json!({"code":"server_is_overloaded","message":private});
        for (body, status, media) in [
            (
                json!({"error":error.clone()}).to_string(),
                503,
                "application/json",
            ),
            (
                json!({"error":error.clone()}).to_string(),
                200,
                "application/json",
            ),
            (
                wire(json!({"type":"response.failed","response":{"error":error}})),
                200,
                "text/event-stream",
            ),
        ] {
            let (_root, provider, captured, task) =
                mock_provider_content_type(body, status, 1024 * 1024, media).await;
            let events = collect(&provider, request()).await;
            let error = events
                .iter()
                .find_map(|event| match event {
                    RunEvent::Error(error) => Some(error),
                    _ => None,
                })
                .unwrap();
            assert_eq!(error.code, "server_is_overloaded");
            assert_eq!(error.status, 503);
            assert!(error.message.contains("OpenAI is temporarily overloaded"));
            assert!(error.message.contains("did not retry automatically"));
            assert!(!error.response_body().to_string().contains(private));
            assert_eq!(
                error.upstream_request_id.as_deref(),
                Some("synthetic-upstream-id")
            );
            assert!(!durable_restriction(&error.code));
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, RunEvent::Completed(_)))
            );
            assert_eq!(captured.lock().await.len(), 1);
            task.abort();
        }
    }

    #[tokio::test]
    async fn coded_authentication_and_server_failures_hide_provider_immediately() {
        let (_root, provider, _captured, task) =
            mock_provider(String::new(), 200, 1024 * 1024).await;
        for (status, code) in [
            (401, "invalid_token"),
            (403, "permission_denied"),
            (500, "server_error"),
            (502, "gateway_error"),
            (503, "server_error"),
            (504, "gateway_timeout"),
        ] {
            let error = provider
                .http_error(
                    status,
                    &serde_json::to_vec(&json!({"error":{"code":code}})).unwrap(),
                    Some("synthetic-generation"),
                )
                .await;
            // Upstream auth failures are provider unavailability, not a bad router key.
            assert_eq!(
                error.status,
                if matches!(status, 401 | 403) {
                    503
                } else {
                    status
                }
            );
            assert_eq!(error.code, code);
            assert_eq!(provider.availability_error().await.unwrap().code, code);
        }
        task.abort();
    }

    async fn mock_provider(
        wire: String,
        status: u16,
        limit: usize,
    ) -> (
        tempfile::TempDir,
        OpenAi,
        Arc<Mutex<Vec<Value>>>,
        tokio::task::JoinHandle<()>,
    ) {
        mock_provider_content_type(wire, status, limit, "text/event-stream").await
    }

    async fn mock_provider_content_type(
        wire: String,
        status: u16,
        limit: usize,
        content_type: &'static str,
    ) -> (
        tempfile::TempDir,
        OpenAi,
        Arc<Mutex<Vec<Value>>>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::{
            Json, Router,
            http::{StatusCode, header},
            response::IntoResponse,
            routing::{get, post},
        };
        let captured = Arc::new(Mutex::new(Vec::new()));
        let capture = captured.clone();
        let app = Router::new()
            .route(
                "/responses",
                post(move |Json(body): Json<Value>| {
                    let captured = capture.clone();
                    let wire = wire.clone();
                    async move {
                        captured.lock().await.push(body);
                        if wire == "synthetic-stall" {
                            tokio::time::sleep(Duration::from_secs(60)).await;
                        }
                        let mut headers = axum::http::HeaderMap::new();
                        if !content_type.is_empty() {
                            headers.insert(
                                header::CONTENT_TYPE,
                                axum::http::HeaderValue::from_static(content_type),
                            );
                        }
                        headers.insert(
                            header::HeaderName::from_static("x-request-id"),
                            axum::http::HeaderValue::from_static("synthetic-upstream-id"),
                        );
                        if wire == "synthetic-stall-body" {
                            let stream = async_stream::stream! {
                                yield Ok::<_, std::convert::Infallible>(": synthetic-wait\n\n");
                                tokio::time::sleep(Duration::from_secs(60)).await;
                            };
                            return (
                                StatusCode::from_u16(status).unwrap(),
                                headers,
                                axum::body::Body::from_stream(stream),
                            )
                                .into_response();
                        }
                        (
                            StatusCode::from_u16(status).unwrap(),
                            headers,
                            axum::body::Body::from(wire),
                        )
                            .into_response()
                    }
                }),
            )
            .route(
                "/models",
                get(|| async {
                    Json(json!({"models":[{"slug":"synthetic-openai","visibility":"list"}]}))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let root = tempfile::tempdir().unwrap();
        let mut config = Config::for_test(root.path());
        config.openai_enabled = true;
        config.max_output_bytes = limit;
        config.request_timeout_secs = 2;
        let provider = OpenAi::synthetic(Arc::new(config), base, "synthetic-only-token".into());
        (root, provider, captured, task)
    }

    fn wire(value: Value) -> String {
        format!("data: {value}\r\n\r\n")
    }
    fn terminal(text: &str) -> Value {
        json!({"type":"response.completed","response":{"id":"synthetic-response","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":text}]}]}})
    }
    async fn collect(provider: &OpenAi, request: RunRequest) -> Vec<RunEvent> {
        let mut run = provider
            .start(request, CancellationToken::new())
            .await
            .unwrap();
        let mut events = vec![];
        while let Some(event) = run.events.recv().await {
            events.push(event);
        }
        assert!(run.finished.is_cancelled());
        events
    }
    #[tokio::test]
    async fn mock_http_text_stream_and_terminal_request_id_without_workspace() {
        let data = wire(json!({"type":"response.output_text.delta","delta":"hello"}))
            + &wire(terminal("hello"));
        let (_root, provider, captured, task) = mock_provider(data, 200, 1024 * 1024).await;
        let events = collect(&provider, request()).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e,RunEvent::TextDelta{delta} if delta=="hello"))
        );
        let result = events
            .iter()
            .find_map(|e| {
                if let RunEvent::Completed(r) = e {
                    Some(r)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(result.text, "hello");
        assert_eq!(
            result.upstream_request_id.as_deref(),
            Some("synthetic-upstream-id")
        );
        let body = &captured.lock().await[0];
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        task.abort();
    }
    #[tokio::test]
    async fn mock_http_missing_terminal_and_mismatched_terminal_fail() {
        for data in [
            wire(json!({"type":"response.output_text.delta","delta":"partial"})),
            wire(
                json!({"type":"response.incomplete","response":{"status":"completed","output":[]}}),
            ),
        ] {
            let (_root, provider, captured, task) = mock_provider(data, 200, 1024 * 1024).await;
            let events = collect(&provider, request()).await;
            assert!(events.iter().any(|e| matches!(e, RunEvent::Error(error) if error.upstream_request_id.as_deref() == Some("synthetic-upstream-id"))));
            assert!(!events.iter().any(|e| matches!(e, RunEvent::Completed(_))));
            assert_eq!(captured.lock().await.len(), 1);
            task.abort();
        }
    }
    #[tokio::test]
    async fn mock_http_quota_latches_even_after_stream_started() {
        let data = wire(json!({"type":"response.output_text.delta","delta":"partial"}))
            + &wire(
                json!({"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"synthetic-sensitive-body"}}}),
            );
        let (_root, provider, captured, task) = mock_provider(data, 200, 1024 * 1024).await;
        let events = collect(&provider, request()).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e,RunEvent::Error(e) if e.status==429))
        );
        assert_eq!(
            provider.availability_error().await.unwrap().code,
            "subscription_sharing_usage_limit_exceeded"
        );
        let _ = collect(&provider, request()).await;
        assert_eq!(captured.lock().await.len(), 1);
        task.abort();
    }
    #[tokio::test]
    async fn mock_http_output_bound_and_changed_account_never_retry() {
        let (_root, provider, captured, task) =
            mock_provider(wire(terminal("a long answer")), 200, 10).await;
        let events = collect(&provider, request()).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e,RunEvent::Error(e) if e.code=="output_limit_exceeded"))
        );
        let mut req = request();
        req.provider_generation = Some("old-generation".into());
        let events = collect(&provider, req).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e,RunEvent::Error(e) if e.code=="provider_account_changed"))
        );
        assert_eq!(captured.lock().await.len(), 1);
        task.abort();
    }
    #[tokio::test]
    async fn mock_http_schema_withholds_provisional_text_and_preserves_reasoning() {
        let data = wire(json!({"type":"response.output_text.delta","delta":"{\"ok\":true}"}))
            + &wire(
                json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"reasoning","id":"rs_synthetic","summary":[],"encrypted_content":"synthetic-opaque"},{"type":"message","content":[{"type":"output_text","text":"{\"ok\":true}"}]}]}}),
            );
        let (_root, provider, _captured, task) = mock_provider(data, 200, 1024 * 1024).await;
        let mut req = request();
        req.schema =
            Some(json!({"type":"object","required":["ok"],"properties":{"ok":{"type":"boolean"}}}));
        let events = collect(&provider, req).await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, RunEvent::TextDelta { .. }))
        );
        assert!(events.iter().any(|e| matches!(e,RunEvent::Completed(r) if r.structured_output==Some(json!({"ok":true})) && r.output_items.len()==1)));
        task.abort();
    }

    #[tokio::test]
    async fn mock_http_original_budget_and_disconnect_cancel_without_retry() {
        let (_root, provider, captured, task) =
            mock_provider("synthetic-stall".into(), 200, 1024 * 1024).await;
        let mut req = request();
        req.remaining_ms = Some(50);
        let started = Instant::now();
        let events = collect(&provider, req).await;
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(
            events
                .iter()
                .any(|e| matches!(e,RunEvent::Error(e) if e.code=="provider_timeout"))
        );
        assert_eq!(captured.lock().await.len(), 1);
        let cancel = CancellationToken::new();
        let mut run = provider.start(request(), cancel.clone()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while captured.lock().await.len() < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), run.finished.cancelled())
            .await
            .unwrap();
        let mut cancelled = false;
        while let Some(event) = run.events.recv().await {
            if let RunEvent::Error(e) = event {
                cancelled = e.code == "request_cancelled";
            }
        }
        assert!(cancelled);
        assert_eq!(captured.lock().await.len(), 2);
        task.abort();
    }

    #[test]
    fn reasoning_replay_precedes_assistant_calls_without_crossing_tool_results() {
        let req=normalize_response(json!({"model":"synthetic","input":[
            {"role":"user","content":"question"},
            {"type":"function_call","call_id":"c1","name":"lookup","arguments":"{}"},
            {"type":"reasoning","id":"rs_synthetic","encrypted_content":"synthetic-opaque","summary":[]},
            {"type":"function_call_output","call_id":"c1","output":"answer"}
        ]}),"synthetic".into()).unwrap();
        let body = serialize(&req).unwrap();
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][1]["type"], "reasoning");
        assert_eq!(body["input"][2]["type"], "function_call");
        assert_eq!(body["input"][3]["type"], "function_call_output");
    }
    #[tokio::test]
    async fn mock_http_success_status_json_errors_are_classified_and_redacted() {
        for (code, expected_status) in [
            ("subscription_sharing_usage_limit_exceeded", 429),
            ("subscription_sharing_invalid_user", 503),
            ("subscription_sharing_unsupported_capability", 400),
        ] {
            let body =
                json!({"error":{"code":code,"message":"synthetic-private-provider-message"}})
                    .to_string();
            let (_root, provider, captured, task) =
                mock_provider_content_type(body, 200, 1024 * 1024, "application/json").await;
            let events = collect(&provider, request()).await;
            let error = events
                .iter()
                .find_map(|event| {
                    if let RunEvent::Error(error) = event {
                        Some(error)
                    } else {
                        None
                    }
                })
                .unwrap();
            assert_eq!(error.code, code);
            assert_eq!(error.status, expected_status);
            assert_eq!(
                error.upstream_request_id.as_deref(),
                Some("synthetic-upstream-id")
            );
            assert!(!error.message.contains("synthetic-private-provider-message"));
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, RunEvent::Completed(_)))
            );
            assert_eq!(captured.lock().await.len(), 1);
            if durable_restriction(code) {
                assert_eq!(provider.availability_error().await.unwrap().code, code);
            }
            task.abort();
        }
    }

    #[tokio::test]
    async fn mock_http_non_sse_success_and_oversize_fail_with_request_id() {
        for (body, code) in [
            (
                terminal("must not be accepted as JSON success")["response"].to_string(),
                "invalid_provider_content_type",
            ),
            (
                "not-json synthetic-private-body".into(),
                "invalid_provider_content_type",
            ),
            ("x".repeat(EVENT_LIMIT + 1), "output_limit_exceeded"),
        ] {
            let (_root, provider, _captured, task) =
                mock_provider_content_type(body, 200, 1024 * 1024, "application/json").await;
            let events = collect(&provider, request()).await;
            assert!(events.iter().any(|event|matches!(event,RunEvent::Error(error) if error.code==code && error.upstream_request_id.as_deref()==Some("synthetic-upstream-id"))));
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, RunEvent::Completed(_)))
            );
            task.abort();
        }
    }

    #[tokio::test]
    async fn mock_http_timeout_after_headers_retains_upstream_request_id() {
        let (_root, provider, _captured, task) =
            mock_provider("synthetic-stall-body".into(), 200, 1024 * 1024).await;
        let mut request = request();
        request.remaining_ms = Some(100);
        let events = collect(&provider, request).await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, RunEvent::Init { .. }))
        );
        assert!(events.iter().any(|event|matches!(event,RunEvent::Error(error) if error.code=="provider_timeout" && error.upstream_request_id.as_deref()==Some("synthetic-upstream-id"))));
        task.abort();
    }

    #[tokio::test]
    async fn mock_http_missing_content_type_still_requires_a_valid_sse_terminal() {
        for (body, success) in [
            (wire(terminal("valid SSE without content type")), true),
            (
                terminal("JSON must not complete")["response"].to_string(),
                false,
            ),
            ("plain synthetic body".into(), false),
            (
                wire(
                    json!({"type":"response.completed","response":{"status":"in_progress","output":[]}}),
                ),
                false,
            ),
        ] {
            let (_root, provider, _captured, task) =
                mock_provider_content_type(body, 200, 1024 * 1024, "").await;
            let events = collect(&provider, request()).await;
            assert_eq!(
                events
                    .iter()
                    .any(|event| matches!(event, RunEvent::Completed(_))),
                success
            );
            assert_eq!(
                events
                    .iter()
                    .any(|event| matches!(event, RunEvent::Error(_))),
                !success
            );
            task.abort();
        }
        let (_root, provider, _captured, task) = mock_provider_content_type(
            wire(terminal("explicit media type must be honored")),
            200,
            1024 * 1024,
            "application/json",
        )
        .await;
        let events = collect(&provider, request()).await;
        assert!(events.iter().any(|event|matches!(event,RunEvent::Error(error) if error.code=="invalid_provider_content_type")));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RunEvent::Completed(_)))
        );
        task.abort();
    }
    #[tokio::test]
    async fn timeout_and_concurrent_transport_failure_cannot_clear_a_plan_restriction() {
        let (_root, provider, captured, task) =
            mock_provider("synthetic-stall-body".into(), 200, 1024 * 1024).await;
        let mut request = request();
        request.provider_generation = Some("synthetic-generation".into());
        request.remaining_ms = Some(200);
        let mut run = provider
            .start(request.clone(), CancellationToken::new())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while captured.lock().await.is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let quota =
            json!({"error":{"code":"subscription_sharing_usage_limit_exceeded"}}).to_string();
        provider
            .http_error(429, quota.as_bytes(), Some("synthetic-generation"))
            .await;
        let mut timed_out = false;
        while let Some(event) = run.events.recv().await {
            if let RunEvent::Error(error) = event {
                timed_out = error.code == "provider_timeout";
            }
        }
        assert!(timed_out);
        assert_eq!(
            provider.availability_error().await.unwrap().code,
            "subscription_sharing_usage_limit_exceeded"
        );
        provider
            .http_error(
                503,
                br#"{"error":{"code":"server_error"}}"#,
                Some("synthetic-generation"),
            )
            .await;
        assert_eq!(
            provider.availability_error().await.unwrap().code,
            "subscription_sharing_usage_limit_exceeded"
        );
        // A late failure belonging to a retired registration must not remove
        // the current registration's durable restriction either.
        provider
            .http_error(
                503,
                br#"{"error":{"code":"server_error"}}"#,
                Some("retired-generation"),
            )
            .await;
        assert_eq!(
            provider.availability_error().await.unwrap().code,
            "subscription_sharing_usage_limit_exceeded"
        );
        assert!(provider.probe().await.authenticated);
        assert_eq!(
            provider.availability_error().await.unwrap().code,
            "subscription_sharing_usage_limit_exceeded"
        );
        let events = collect(&provider, request).await;
        assert!(events.iter().any(|event|matches!(event,RunEvent::Error(error) if error.code=="subscription_sharing_usage_limit_exceeded")));
        assert_eq!(
            captured.lock().await.len(),
            1,
            "plan restriction must prevent another inference request"
        );
        // Simulate a latch left by the account generation that preceded the
        // selected synthetic registration. Only successful fresh discovery may
        // retire this stale durable state.
        *provider.local_block.lock().await = Some((
            "retired-generation".into(),
            failure(429, "subscription_sharing_usage_limit_exceeded"),
        ));
        assert!(provider.probe().await.authenticated);
        assert!(provider.local_block.lock().await.is_none());
        assert!(provider.availability_error().await.is_none());
        task.abort();
    }
    fn done_item(index: usize, item: Value) -> Value {
        json!({"type":"response.output_item.done","output_index":index,"item":item})
    }
    fn done_tool(arguments: &str) -> Value {
        json!({"id":"fc_finished_synthetic","type":"function_call","call_id":"call_finished_synthetic","name":"lookup","arguments":arguments,"status":"completed"})
    }
    fn tool_request() -> RunRequest {
        let mut request = request();
        request.tools = vec![ToolDefinition {
            name: "lookup".into(),
            description: String::new(),
            parameters: json!({"type":"object","properties":{"q":{"type":"string"}},"required":["q"],"additionalProperties":false}),
        }];
        request
    }
    #[tokio::test]
    async fn mock_http_finished_tools_and_reasoning_survive_empty_or_partial_terminal_output() {
        let reasoning = json!({"id":"rs_finished_synthetic","type":"reasoning","summary":[],"encrypted_content":"synthetic-encrypted-finished"});
        let call = done_tool("{\"q\":\"done\"}");
        for output in [vec![], vec![call.clone()]] {
            let data = wire(done_item(0, reasoning.clone()))
                + &wire(
                    json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"partial-is-not-executable"}),
                )
                + &wire(done_item(1, call.clone()))
                + &wire(
                    json!({"type":"response.completed","response":{"status":"completed","output":output}}),
                );
            let (_root, provider, _captured, task) = mock_provider(data, 200, 1024 * 1024).await;
            let events = collect(&provider, tool_request()).await;
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, RunEvent::ToolCall(_)))
                    .count(),
                1
            );
            let result = events
                .iter()
                .find_map(|event| {
                    if let RunEvent::Completed(result) = event {
                        Some(result)
                    } else {
                        None
                    }
                })
                .unwrap();
            assert_eq!(result.tool_calls[0].arguments, json!({"q":"done"}));
            assert_eq!(result.output_items, vec![reasoning.clone()]);
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, RunEvent::Error(_)))
            );
            task.abort();
        }
    }
    #[tokio::test]
    async fn mock_http_finished_item_conflicts_malformed_calls_and_bounds_fail_closed() {
        let call = done_tool("{\"q\":\"done\"}");
        let mut changed = call.clone();
        changed["arguments"] = json!("{\"q\":\"changed\"}");
        let completed = wire(
            json!({"type":"response.completed","response":{"status":"completed","output":[]}}),
        );
        let mut duplicate_id = call.clone();
        duplicate_id["call_id"] = json!("different-call");
        let cases = [
            wire(done_item(0, done_tool("{\"q\":12}"))) + &completed,
            wire(done_item(0, call.clone()))
                + &wire(
                    json!({"type":"response.completed","response":{"status":"completed","output":[changed.clone()]}}),
                ),
            wire(done_item(0, call.clone())) + &wire(done_item(0, changed)) + &completed,
            wire(done_item(0, call.clone())) + &wire(done_item(1, duplicate_id)) + &completed,
            wire(done_item(1000, call.clone())) + &completed,
            wire(done_item(1, call.clone())) + &completed,
            wire(
                json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"missing-id"}}),
            ) + &completed,
        ];
        for data in cases {
            let (_root, provider, _captured, task) = mock_provider(data, 200, 1024 * 1024).await;
            let events = collect(&provider, tool_request()).await;
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, RunEvent::Error(_)))
            );
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, RunEvent::ToolCall(_) | RunEvent::Completed(_)))
            );
            task.abort();
        }
    }
    #[tokio::test]
    async fn mock_http_partial_or_incomplete_tool_streams_never_handoff() {
        let call = done_tool("{\"q\":\"done\"}");
        for data in [
            wire(
                json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"q\":\"partial"}),
            ),
            wire(done_item(0, call.clone())),
            wire(done_item(0, call))
                + &wire(
                    json!({"type":"response.incomplete","response":{"status":"incomplete","output":[]}}),
                ),
        ] {
            let (_root, provider, _captured, task) = mock_provider(data, 200, 1024 * 1024).await;
            let events = collect(&provider, tool_request()).await;
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, RunEvent::ToolCall(_)))
            );
            assert!(!events.iter().any(
                |event| matches!(event,RunEvent::Completed(result) if !result.tool_calls.is_empty())
            ));
            task.abort();
        }
    }
}
