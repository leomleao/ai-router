//! Direct Antigravity model provider. No subprocesses, local tools or fallback.
use crate::{
    antigravity_auth::{AntigravityAuth, BASELINE_REVISION, OAuthConfig, Restriction},
    antigravity_release::ReleaseMonitor,
    antigravity_transport::{AntigravityTransport, Operation},
    antigravity_wire::{self, Decoder},
    config::Config,
    protocol::*,
    provider_stream::RunStream,
};
use chrono::Utc;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

const MAX_REPLAY_BYTES: usize = 64 * 1024 * 1024;
const CLIENT_REPLAY_BYTES: usize = 16 * 1024 * 1024;
const REPLAY_TTL: Duration = Duration::from_secs(24 * 3600);

#[derive(Clone)]
pub struct Antigravity {
    config: Arc<Config>,
    pub auth: AntigravityAuth,
    pub release: ReleaseMonitor,
    transport: AntigravityTransport,
    replay: Arc<Mutex<VecDeque<Replay>>>,
    cooldowns: Arc<Mutex<HashMap<String, Instant>>>,
}
struct Replay {
    owner: String,
    model: String,
    generation: String,
    value: Value,
    bytes: usize,
    at: Instant,
}

impl Antigravity {
    pub fn new(config: Arc<Config>) -> Self {
        let auth = AntigravityAuth::new(config.state_dir.clone(), OAuthConfig::from_env())
            .expect("fixed TLS client configuration");
        Self::with_auth(config, auth)
    }
    fn with_auth(config: Arc<Config>, auth: AntigravityAuth) -> Self {
        Self {
            config,
            transport: AntigravityTransport::new(auth.clone()),
            auth,
            release: ReleaseMonitor::new(),
            replay: Default::default(),
            cooldowns: Default::default(),
        }
    }
    #[cfg(test)]
    pub(crate) async fn synthetic(config: Arc<Config>, origin: String) -> Self {
        let auth = AntigravityAuth::synthetic(config.state_dir.clone(), origin).await;
        Self::with_auth(config, auth)
    }
    pub async fn generation(&self) -> Result<String, ApiError> {
        self.auth.generation().await
    }
    pub async fn availability_error(&self) -> Option<ApiError> {
        if !self.config.antigravity_enabled {
            return Some(failure(503, "provider_disabled"));
        }
        self.auth.availability_error().await
    }
    pub async fn model_availability(&self, model: &str) -> Result<(), ApiError> {
        if let Some(error) = self.availability_error().await {
            return Err(error);
        }
        let mut cooldowns = self.cooldowns.lock().await;
        cooldowns.retain(|_, until| *until > Instant::now());
        if cooldowns.contains_key(family(model)) {
            return Err(failure(429, "agy_family_quota_exhausted"));
        }
        Ok(())
    }
    pub async fn probe(&self) -> ProviderStatus {
        let mut status = ProviderStatus {
            version: BASELINE_REVISION.into(),
            checked_at: Utc::now().to_rfc3339(),
            ..Default::default()
        };
        let result = async {
            if let Some(error) = self.availability_error().await {
                return Err(error);
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            let token = self.auth.token(deadline).await?;
            let value = self
                .transport
                .post_json(
                    Operation::Models,
                    &json!({"project":token.project}),
                    &token,
                    deadline,
                )
                .await?
                .json()
                .await?;
            let models = match parse_models(&value) {
                Ok(models) => models,
                Err(error) => {
                    self.auth
                        .block_for_generation(Restriction::Protocol, &token.generation)
                        .await?;
                    return Err(error);
                }
            };
            status.models = models;
            status.registration_generation = Some(token.generation);
            status.quota = roster_quota(&value);
            status.authenticated = true;
            status.error = None;
            Ok::<(), ApiError>(())
        }
        .await;
        if let Err(error) = result {
            status.error = Some(error.code);
        }
        status
    }
    pub async fn start(
        &self,
        request: RunRequest,
        cancel: CancellationToken,
    ) -> Result<RunStream, ApiError> {
        // Pure validation precedes authentication, refresh or paid dispatch.
        let _ = antigravity_wire::serialize(&request, "synthetic-validation", &[])?;
        if !self.config.antigravity_enabled {
            return Err(failure(503, "provider_disabled"));
        }
        let (sender, events) = mpsc::channel(32);
        let finished = CancellationToken::new();
        let completion = finished.clone();
        let this = self.clone();
        tokio::spawn(async move {
            let deadline = Instant::now()
                + Duration::from_millis(
                    request
                        .remaining_ms
                        .unwrap_or(this.config.request_timeout_secs * 1000),
                );
            let work = this.execute(&request, &sender, deadline);
            let outcome = tokio::select! {
                _ = cancel.cancelled() => Err(failure(499,"request_cancelled")),
                _ = sender.closed() => Err(failure(499,"request_cancelled")),
                result = tokio::time::timeout_at(deadline.into(),work) => result.unwrap_or_else(|_|Err(failure(504,"provider_timeout"))),
            };
            if let Err(error) = outcome {
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
    async fn execute(
        &self,
        request: &RunRequest,
        sender: &mpsc::Sender<RunEvent>,
        deadline: Instant,
    ) -> Result<(), ApiError> {
        self.model_availability(&request.model).await?;
        let token = self.auth.token(deadline).await?;
        if request
            .provider_generation
            .as_ref()
            .is_some_and(|g| g != &token.generation)
        {
            return Err(failure(503, "provider_account_changed"));
        }
        let replay = {
            let mut cache = self.replay.lock().await;
            cache.retain(|entry| entry.at.elapsed() < REPLAY_TTL);
            cache
                .iter()
                .filter(|e| {
                    e.owner == request.owner
                        && e.model == request.model
                        && e.generation == token.generation
                })
                .map(|e| e.value.clone())
                .collect::<Vec<_>>()
        };
        let body = antigravity_wire::serialize(request, &token.project, &replay)?;
        let anchor = antigravity_wire::request_anchor(&body);
        let mut response = match self
            .transport
            .post_json(Operation::Generate, &body, &token, deadline)
            .await
        {
            Ok(response) => response,
            Err(error) => {
                if error.status == 429 {
                    self.cooldowns.lock().await.insert(
                        family(&request.model).into(),
                        Instant::now() + Duration::from_secs(60),
                    );
                }
                return Err(error);
            }
        };
        let upstream_id = response.request_id.clone();
        let result = async {
            send(
                sender,
                RunEvent::Init {
                    conversation_id: request.request_id.clone(),
                    tools: request.tools.iter().map(|t| t.name.clone()).collect(),
                },
            )
            .await?;
            let mut decoder = Decoder::new(request, self.config.max_output_bytes);
            while let Some(chunk) = response.chunk().await? {
                for delta in decoder.push(&chunk)? {
                    send(sender, RunEvent::TextDelta { delta }).await?;
                }
            }
            let mut decoded = decoder.finish()?;
            if decoded.status != "incomplete" {
                let mut cache = self.replay.lock().await;
                for mut value in decoded.replay.drain(..) {
                    value["anchor"] = anchor.clone();
                    let bytes = value.to_string().len();
                    if bytes > CLIENT_REPLAY_BYTES {
                        return Err(failure(502, "agy_replay_limit"));
                    }
                    cache.retain(|e| {
                        !(e.owner == request.owner
                            && e.model == request.model
                            && e.generation == token.generation
                            && e.value == value)
                    });
                    while cache
                        .iter()
                        .filter(|e| e.owner == request.owner)
                        .map(|e| e.bytes)
                        .sum::<usize>()
                        + bytes
                        > CLIENT_REPLAY_BYTES
                    {
                        let index = cache
                            .iter()
                            .position(|e| e.owner == request.owner)
                            .expect("owner exceeds limit");
                        cache.remove(index);
                    }
                    while cache.len() >= 4096
                        || cache.iter().map(|e| e.bytes).sum::<usize>() + bytes > MAX_REPLAY_BYTES
                    {
                        cache.pop_front();
                    }
                    cache.push_back(Replay {
                        owner: request.owner.clone(),
                        model: request.model.clone(),
                        generation: token.generation.clone(),
                        value,
                        bytes,
                        at: Instant::now(),
                    });
                }
            }
            if request.schema.is_some() && !decoded.text.is_empty() {
                send(
                    sender,
                    RunEvent::TextDelta {
                        delta: decoded.text.clone(),
                    },
                )
                .await?;
            }
            for call in &decoded.tool_calls {
                send(sender, RunEvent::ToolCall(call.clone())).await?;
            }
            let structured_output = if request.schema.is_some()
                && decoded.status != "incomplete"
                && decoded.tool_calls.is_empty()
            {
                Some(
                    serde_json::from_str(&decoded.text)
                        .map_err(|_| failure(502, "invalid_json_output"))?,
                )
            } else {
                None
            };
            send(
                sender,
                RunEvent::Completed(RunResult {
                    output_items: vec![],
                    upstream_request_id: decoded.upstream_request_id.or(upstream_id.clone()),
                    text: decoded.text,
                    structured_output,
                    conversation_id: request.request_id.clone(),
                    usage: decoded.usage,
                    usage_partial: decoded.usage_partial,
                    tool_calls: decoded.tool_calls,
                    status: decoded.status,
                }),
            )
            .await
        }
        .await;
        match result {
            Ok(()) => Ok(()),
            Err(mut error) => {
                // Local input errors and transient I/O do not poison the protocol baseline.
                if matches!(
                    error.code.as_str(),
                    "provider_authentication_failed" | "provider_account_restricted"
                ) {
                    let kind = if error.status == 401 {
                        Restriction::Authentication
                    } else {
                        Restriction::Account
                    };
                    self.auth
                        .block_for_generation(kind, &token.generation)
                        .await?;
                } else if error.status == 429 {
                    self.cooldowns.lock().await.insert(
                        family(&request.model).into(),
                        Instant::now() + Duration::from_secs(60),
                    );
                } else if protocol_failure(&error.code) {
                    self.auth
                        .block_for_generation(Restriction::Protocol, &token.generation)
                        .await?;
                }

                error.upstream_request_id = error.upstream_request_id.or(upstream_id);
                Err(error)
            }
        }
    }
}
async fn send(sender: &mpsc::Sender<RunEvent>, event: RunEvent) -> Result<(), ApiError> {
    sender
        .send(event)
        .await
        .map_err(|_| failure(499, "request_cancelled"))
}
fn failure(status: u16, code: &str) -> ApiError {
    ApiError::new(
        status,
        code,
        "Antigravity model request could not complete; inspect private provider status",
    )
}
fn family(model: &str) -> &str {
    match model {
        "gemini-4-argon-low" | "gemini-4-argon-medium" | "gemini-4-argon-high" => "gemini-4-argon",
        "gemini-3.8-flash-low" | "gemini-3.8-flash-medium" | "gemini-3.8-flash-high" => {
            "gemini-3.8-flash"
        }
        "claude-opus-5-5-low" | "claude-opus-5-5-medium" | "claude-opus-5-5-high" => {
            "claude-opus-5-5"
        }
        "claude-sonnet-5-5-low" | "claude-sonnet-5-5-medium" | "claude-sonnet-5-5-high" => {
            "claude-sonnet-5-5"
        }
        _ => model,
    }
}
fn reviewed_model(id: &str) -> bool {
    // Raw account rosters also contain image, tab-completion and newly added
    // SKUs. Publishing those as generic text/tool models would promise an
    // unreviewed contract and could let an expected media response trip the
    // protocol gate for every model. The compiled baseline is the authority.
    static REVIEWED: OnceLock<HashSet<String>> = OnceLock::new();
    REVIEWED
        .get_or_init(|| {
            serde_json::from_str::<Value>(include_str!("../docs/antigravity-baseline.json"))
                .ok()
                .and_then(|value| {
                    value["model_ids_when_present_in_usable_raw_roster"]
                        .as_array()
                        .cloned()
                })
                .unwrap_or_default()
                .into_iter()
                .filter_map(|id| id.as_str().map(str::to_owned))
                .collect()
        })
        .contains(id)
}
pub(crate) fn parse_models(value: &Value) -> Result<Vec<ModelInfo>, ApiError> {
    let models = value
        .get("models")
        .and_then(Value::as_object)
        .filter(|m| m.len() <= 4096)
        .ok_or_else(|| failure(502, "invalid_provider_catalogue"))?;
    let mut output = Vec::new();
    for (id, row) in models {
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b))
            || !row.is_object()
        {
            return Err(failure(502, "invalid_provider_catalogue"));
        }
        for flag in ["disabled", "isInternal"] {
            if row.get(flag).is_some_and(|v| !v.is_boolean()) {
                return Err(failure(502, "invalid_provider_catalogue"));
            }
        }
        if row["disabled"] == true
            || row["isInternal"] == true
            || !reviewed_model(id)
            || matches!(id.as_str(), "chat_20706" | "chat_23310" | "gemini-2.5-pro")
        {
            continue;
        }
        let name = row.get("displayName").and_then(Value::as_str).unwrap_or(id);
        if name.len() > 256 || name.chars().any(char::is_control) {
            return Err(failure(502, "invalid_provider_catalogue"));
        }
        output.push(ModelInfo {
            id: id.clone(),
            name: name.into(),
        });
    }
    output.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(output)
}
fn roster_quota(value: &Value) -> Option<Value> {
    let mut buckets = Vec::new();
    for (id, row) in value.get("models")?.as_object()? {
        if row["disabled"] == true
            || row["isInternal"] == true
            || !reviewed_model(id)
            || matches!(id.as_str(), "chat_20706" | "chat_23310" | "gemini-2.5-pro")
        {
            continue;
        }
        let Some(fraction) = row
            .pointer("/quotaInfo/remainingFraction")
            .and_then(Value::as_f64)
            .filter(|v| (0.0..=1.0).contains(v))
        else {
            continue;
        };
        let reset = row
            .pointer("/quotaInfo/resetTime")
            .and_then(Value::as_str)
            .filter(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok());
        buckets.push(json!({"id":id,"name":id,"remaining_fraction":fraction,"reset_time":reset}));
    }
    (!buckets.is_empty())
        .then(|| json!({"groups":[{"name":"Antigravity model quota","buckets":buckets}]}))
}

fn protocol_failure(code: &str) -> bool {
    matches!(
        code,
        "invalid_provider_candidate"
            | "invalid_provider_envelope"
            | "invalid_provider_json"
            | "invalid_provider_part"
            | "invalid_provider_parts"
            | "invalid_provider_role"
            | "invalid_provider_stream"
            | "invalid_provider_terminal"
            | "invalid_provider_text"
            | "invalid_provider_usage"
            | "invalid_provider_utf8"
            | "invalid_provider_request_id"
            | "conflicting_provider_request_id"
            | "content_after_provider_terminal"
            | "duplicate_provider_terminal"
            | "missing_provider_terminal"
            | "truncated_provider_stream"
            | "unsupported_provider_candidates"
            | "unsupported_provider_event"
            | "unsupported_provider_part"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalogue_requires_recognized_shape_and_preserves_exact_variants() {
        assert!(parse_models(&json!({})).is_err());
        assert!(parse_models(&json!({"models":[]})).is_err());
        assert!(parse_models(&json!({"models":{}})).unwrap().is_empty());
        let models=parse_models(&json!({"models":{"gemini-4-argon-medium":{"displayName":"Argon Medium"},"gemini-4-argon-disabled":{"disabled":true},"private":{"isInternal":true}}})).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gemini-4-argon-medium");
    }
    #[test]
    fn real_catalogue_variant_names_allow_dots_and_keep_all_thirteen_ids() {
        let baseline: Value =
            serde_json::from_str(include_str!("../docs/antigravity-baseline.json")).unwrap();
        let ids = baseline["model_ids_when_present_in_usable_raw_roster"]
            .as_array()
            .unwrap();
        let roster = ids
            .iter()
            .map(|id| (id.as_str().unwrap().to_owned(), json!({"displayName":id})))
            .collect::<serde_json::Map<_, _>>();
        let models = parse_models(&json!({"models":roster})).unwrap();
        assert_eq!(models.len(), 13);
        assert!(models.iter().any(|m| m.id == "gemini-3.8-flash-high"));
        assert!(models.iter().any(|m| m.id == "claude-opus-5-5-medium"));
    }
    #[test]
    fn quota_cooldowns_are_explicit_family_scoped() {
        assert_eq!(family("gemini-4-argon-low"), family("gemini-4-argon-high"));
        assert_ne!(
            family("claude-opus-5-5-high"),
            family("claude-sonnet-5-5-high")
        );
        assert_eq!(family("custom-id"), "custom-id");
    }
    #[test]
    fn catalogue_and_quota_publish_only_reviewed_text_models() {
        let roster = json!({"models":{
            "gemini-4-argon-medium":{"displayName":"Argon Medium","quotaInfo":{"remainingFraction":0.5}},
            "gemini-3-pro-image":{"displayName":"Image","quotaInfo":{"remainingFraction":1.0}},
            "tab_flash":{"displayName":"Tab","quotaInfo":{"remainingFraction":1.0}},
            "gemini-new-unreviewed-high":{"displayName":"Future Text","quotaInfo":{"remainingFraction":1.0}},
            "claude-new-high":{"displayName":"Future Claude","quotaInfo":{"remainingFraction":1.0}},
            "claude-sonnet-5-5-high":{"disabled":true,"quotaInfo":{"remainingFraction":1.0}}
        }});
        let models = parse_models(&roster).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gemini-4-argon-medium");
        let quota = roster_quota(&roster).unwrap();
        let buckets = quota["groups"][0]["buckets"].as_array().unwrap();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0]["id"], "gemini-4-argon-medium");
        let unsupported = json!({"models":{"tab_flash":{"quotaInfo":{"remainingFraction":1.0}}}});
        assert!(parse_models(&unsupported).unwrap().is_empty());
        assert!(roster_quota(&unsupported).is_none());
        // Unreviewed rows do not bypass validation of the roster's known shape.
        assert!(parse_models(&json!({"models":{"unreviewed":{"disabled":"invalid"}}})).is_err());
    }
}
