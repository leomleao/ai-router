//! One fixed-origin authenticated dispatch path. No redirects, retries or endpoint fallback.
use crate::{
    antigravity_auth::{
        AccessToken, AntigravityAuth, CCA_ORIGIN, PINNED_USER_AGENT, Restriction, timeout,
    },
    protocol::ApiError,
};
use serde_json::Value;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

pub const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Generate,
    Models,
    Quota,
    LoadCodeAssist,
}
impl Operation {
    pub fn path(self) -> &'static str {
        match self {
            Self::Generate => "/v1internal:streamGenerateContent?alt=sse",
            Self::Models => "/v1internal:fetchAvailableModels",
            Self::Quota => "/v1internal:retrieveUserQuotaSummary",
            Self::LoadCodeAssist => "/v1internal:loadCodeAssist",
        }
    }
}
#[derive(Clone)]
pub struct AntigravityTransport {
    auth: AntigravityAuth,
}
impl AntigravityTransport {
    pub fn new(auth: AntigravityAuth) -> Self {
        Self { auth }
    }
    pub async fn post_json(
        &self,
        op: Operation,
        body: &Value,
        token: &AccessToken,
        deadline: Instant,
    ) -> Result<GuardedResponse, ApiError> {
        let bytes = serde_json::to_vec(body)
            .map_err(|_| ApiError::bad_request("Antigravity request cannot be encoded"))?;
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err(ApiError::new(
                413,
                "agy_request_too_large",
                "Antigravity request exceeds its wire bound",
            ));
        }
        if Instant::now() >= deadline {
            return Err(timeout());
        }
        let guard = self
            .auth
            .dispatch_guard(&token.generation, deadline)
            .await?;
        let cancellation = self.auth.cancellation();
        let mut outbound = self
            .auth
            .client()
            .post(self.auth.endpoint(&format!("{CCA_ORIGIN}{}", op.path())))
            .bearer_auth(&token.access_token)
            .header("User-Agent", PINNED_USER_AGENT)
            .header("Content-Type", "application/json")
            .header(
                "Accept",
                if op == Operation::Generate {
                    "text/event-stream"
                } else {
                    "application/json"
                },
            )
            .body(bytes);
        if op == Operation::Generate
            && matches!(
                body["model"].as_str(),
                Some(
                    "claude-opus-5-5-low"
                        | "claude-opus-5-5-medium"
                        | "claude-opus-5-5-high"
                        | "claude-sonnet-5-5-low"
                        | "claude-sonnet-5-5-medium"
                        | "claude-sonnet-5-5-high"
                )
            )
        {
            outbound = outbound.header("anthropic-beta", "interleaved-thinking-2025-05-14");
        }
        let send = outbound.send();
        tokio::pin!(send);
        let mut checks = tokio::time::interval(Duration::from_millis(100));
        checks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let response = loop {
            tokio::select! {biased;
                _=cancellation.cancelled()=>return Err(cancelled()),
                _=tokio::time::sleep_until(deadline.into())=>return Err(timeout()),
                _=checks.tick()=>{let _validation=self.auth.try_validate_generation(&token.generation,deadline)?;},
                result=&mut send=>break result.map_err(|_|ApiError::unavailable("Antigravity HTTP dispatch failed; request was not retried"))?,
            }
        };
        drop(guard);
        let status = response.status();
        let request_id = response
            .headers()
            .get("x-request-id")
            .or_else(|| response.headers().get("x-goog-request-id"))
            .and_then(|s| s.to_str().ok())
            .filter(|s| s.len() <= 256 && !s.chars().any(char::is_control))
            .map(str::to_owned);
        if status.is_redirection() {
            self.auth
                .block_for_generation(Restriction::Protocol, &token.generation)
                .await?;
            return Err(ApiError::new(
                503,
                "agy_redirect_refused",
                "Antigravity returned an unexpected redirect; protocol review required",
            ));
        }
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let guarded = GuardedResponse {
            response,
            auth: self.auth.clone(),
            generation: token.generation.clone(),
            cancellation,
            deadline,
            seen: 0,
            next_state_check: Instant::now(),
            request_id,
            content_type,
        };
        if !status.is_success() {
            let kind = if status.as_u16() == 401 {
                Some(Restriction::Authentication)
            } else if status.as_u16() == 403 {
                Some(Restriction::Account)
            } else if matches!(status.as_u16(), 405 | 415) {
                Some(Restriction::Protocol)
            } else {
                None
            };
            if let Some(kind) = kind {
                self.auth
                    .block_for_generation(kind, &token.generation)
                    .await?;
            }
            let mut error = match status.as_u16() {
                401 => ApiError::new(
                    503,
                    "agy_auth_blocked",
                    "Antigravity rejected authorization; explicit operator recovery required",
                ),
                403 => ApiError::new(
                    503,
                    "agy_account_blocked",
                    "Antigravity rejected the account; explicit operator recovery required",
                ),
                429 => ApiError::new(
                    429,
                    "agy_rate_limited",
                    "Antigravity model family is rate limited; no retry was attempted",
                ),
                400 | 422 => ApiError::new(
                    400,
                    "provider_invalid_request",
                    "Antigravity rejected this request; check context size and supported tool schemas",
                ),
                404 => ApiError::new(
                    503,
                    "provider_model_unavailable",
                    "Antigravity model is currently unavailable; refresh the model catalogue",
                ),
                405 | 415 => ApiError::new(
                    503,
                    "agy_protocol_blocked",
                    "Antigravity rejected the reviewed protocol; operator review required",
                ),
                _ => ApiError::new(
                    502,
                    "agy_upstream_error",
                    "Antigravity returned a transient upstream failure; no retry was attempted",
                ),
            };
            error.upstream_request_id = guarded.request_id.clone();
            return Err(error);
        }
        let expected = if op == Operation::Generate {
            "text/event-stream"
        } else {
            "application/json"
        };
        if guarded.content_type.as_deref().is_none_or(|s| {
            s.split(';')
                .next()
                .is_none_or(|s| !s.trim().eq_ignore_ascii_case(expected))
        }) {
            self.auth
                .block_for_generation(Restriction::Protocol, &token.generation)
                .await?;
            return Err(ApiError::new(
                503,
                "agy_protocol_blocked",
                "Antigravity response media type changed; protocol review required",
            ));
        }
        Ok(guarded)
    }
}
pub struct GuardedResponse {
    response: reqwest::Response,
    auth: AntigravityAuth,
    generation: String,
    cancellation: CancellationToken,
    deadline: Instant,
    seen: usize,
    next_state_check: Instant,
    pub request_id: Option<String>,
    pub content_type: Option<String>,
}
impl GuardedResponse {
    pub async fn chunk(&mut self) -> Result<Option<Vec<u8>>, ApiError> {
        loop {
            // The persistent timestamp survives both fast chunks and repeated chunk()
            // calls, preventing data from starving cross-process closure checks.
            if Instant::now() >= self.next_state_check {
                // Busy is a distinct outcome: refresh owns the durable state
                // lock while safely rotating. It does not validate the epoch,
                // and it must not manufacture an original-deadline failure.
                let _validation = self
                    .auth
                    .try_validate_generation(&self.generation, self.deadline)?;
                self.next_state_check = Instant::now() + Duration::from_millis(100);
            }
            tokio::select! {biased;
                _=self.cancellation.cancelled()=>return Err(cancelled()),
                _=tokio::time::sleep_until(self.deadline.into())=>return Err(timeout()),
                _=tokio::time::sleep_until(self.next_state_check.into())=>{},
                result=self.response.chunk()=>{
                    let chunk=result.map_err(|_|ApiError::new(502,"agy_stream_interrupted","Antigravity response disconnected before completion"))?;
                    if let Some(chunk)=chunk {
                        self.seen=self.seen.saturating_add(chunk.len());
                        if self.seen>MAX_RESPONSE_BYTES {self.auth.block_for_generation(Restriction::Protocol,&self.generation).await?;return Err(ApiError::new(503,"agy_response_too_large","Antigravity response exceeded its wire bound"));}
                        return Ok(Some(chunk.to_vec()));
                    }return Ok(None);
                }
            }
        }
    }
    pub async fn json(mut self) -> Result<Value, ApiError> {
        let mut b = Vec::new();
        while let Some(c) = self.chunk().await? {
            if b.len() + c.len() > 2 * 1024 * 1024 {
                self.auth
                    .block_for_generation(Restriction::Protocol, &self.generation)
                    .await?;
                return Err(ApiError::unavailable(
                    "Antigravity control response exceeded its bound",
                ));
            }
            b.extend_from_slice(&c);
        }
        match serde_json::from_slice(&b) {
            Ok(v) => Ok(v),
            Err(_) => {
                self.auth
                    .block_for_generation(Restriction::Protocol, &self.generation)
                    .await?;
                Err(ApiError::new(
                    503,
                    "agy_protocol_invalid",
                    "Antigravity control response was not valid JSON",
                ))
            }
        }
    }
}
fn cancelled() -> ApiError {
    ApiError::new(
        503,
        "agy_dispatch_cancelled",
        "Antigravity traffic stopped because authorization or compatibility state changed",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::antigravity_auth::OAuthConfig;
    use axum::{Router, routing::post};
    async fn mock(handler: Router) -> (String, tokio::task::JoinHandle<()>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(l, handler).await.unwrap() });
        (format!("http://{addr}"), task)
    }
    #[test]
    fn operation_paths_do_not_accept_origins() {
        for op in [
            Operation::Generate,
            Operation::Models,
            Operation::Quota,
            Operation::LoadCodeAssist,
        ] {
            assert!(op.path().starts_with("/v1internal:"));
            assert!(!op.path().contains("://"));
        }
        assert_eq!(CCA_ORIGIN, "https://daily-cloudcode-pa.googleapis.com");
    }
    #[tokio::test]
    async fn synthetic_redirect_is_never_followed() {
        let (origin, server) = mock(Router::new().route(
            "/v1internal:fetchAvailableModels",
            post(|| async {
                (
                    axum::http::StatusCode::FOUND,
                    [("location", "http://127.0.0.1:1/never")],
                )
            }),
        ))
        .await;
        let d = tempfile::tempdir().unwrap();
        let auth = AntigravityAuth::synthetic(d.path().join("state"), origin).await;
        let token = auth
            .token(Instant::now() + Duration::from_secs(2))
            .await
            .unwrap();
        let transport = AntigravityTransport::new(auth.clone());
        let error = transport
            .post_json(
                Operation::Models,
                &serde_json::json!({}),
                &token,
                Instant::now() + Duration::from_secs(2),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, "agy_redirect_refused");
        assert_eq!(
            auth.availability_error().await.unwrap().code,
            "agy_protocol_blocked"
        );
        server.abort();
    }
    #[tokio::test]
    async fn quota_does_not_permanently_restrict_account() {
        let (origin, server) = mock(Router::new().route(
            "/v1internal:streamGenerateContent",
            post(|| async { axum::http::StatusCode::TOO_MANY_REQUESTS }),
        ))
        .await;
        let d = tempfile::tempdir().unwrap();
        let auth = AntigravityAuth::synthetic(d.path().join("state"), origin).await;
        let token = auth
            .token(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        let error = AntigravityTransport::new(auth.clone())
            .post_json(
                Operation::Generate,
                &serde_json::json!({}),
                &token,
                Instant::now() + Duration::from_secs(2),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error.status, 429);
        assert!(auth.availability_error().await.is_none());
        server.abort();
    }
    #[tokio::test]
    async fn restriction_cancels_pending_headers_and_prevents_next_dispatch() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let (origin, server) = mock(Router::new().route(
            "/v1internal:fetchAvailableModels",
            post(move || {
                let count = count.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    axum::Json(serde_json::json!({"models":{}}))
                }
            }),
        ))
        .await;
        let d = tempfile::tempdir().unwrap();
        let auth = AntigravityAuth::synthetic(d.path().join("state"), origin).await;
        let token = auth
            .token(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        let generation = token.generation.clone();
        let a = auth.clone();
        let pending = tokio::spawn(async move {
            AntigravityTransport::new(a)
                .post_json(
                    Operation::Models,
                    &serde_json::json!({}),
                    &token,
                    Instant::now() + Duration::from_secs(5),
                )
                .await
        });
        for _ in 0..50 {
            if calls.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let operator =
            AntigravityAuth::new(d.path().join("state"), OAuthConfig::default()).unwrap();
        operator
            .block_for_generation(Restriction::Account, &generation)
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            result.err().unwrap().code.as_str(),
            "agy_dispatch_cancelled" | "agy_account_blocked"
        ));
        let token = AccessToken {
            access_token: "synthetic-access".into(),
            generation,
            project: "synthetic-project".into(),
        };
        assert!(
            AntigravityTransport::new(auth)
                .post_json(
                    Operation::Models,
                    &serde_json::json!({}),
                    &token,
                    Instant::now() + Duration::from_secs(1)
                )
                .await
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        server.abort();
    }
    #[tokio::test]
    async fn rapid_chunks_cannot_starve_external_restriction_and_recovery() {
        let(origin,server)=mock(Router::new().route("/v1internal:streamGenerateContent",post(||async{
            let stream=async_stream::stream!{loop{tokio::time::sleep(Duration::from_millis(2)).await;yield Ok::<_,std::convert::Infallible>("data: synthetic\n\n");}};
            ([("content-type","text/event-stream")],axum::body::Body::from_stream(stream))
        })).route("/v1internal:fetchAvailableModels",post(||async{axum::Json(serde_json::json!({"models":{}}))}))).await;
        let d = tempfile::tempdir().unwrap();
        let auth = AntigravityAuth::synthetic(d.path().join("state"), origin).await;
        let token = auth
            .token(Instant::now() + Duration::from_secs(2))
            .await
            .unwrap();
        let generation = token.generation.clone();
        let transport = AntigravityTransport::new(auth.clone());
        let mut response = transport
            .post_json(
                Operation::Generate,
                &serde_json::json!({}),
                &token,
                Instant::now() + Duration::from_secs(3),
            )
            .await
            .unwrap();
        assert!(response.chunk().await.unwrap().is_some());
        let operator =
            AntigravityAuth::new(d.path().join("state"), OAuthConfig::default()).unwrap();
        operator
            .block_for_generation(Restriction::Account, &generation)
            .await
            .unwrap();
        let stopped = tokio::time::timeout(Duration::from_millis(600), async {
            loop {
                if response.chunk().await.is_err() {
                    break;
                }
            }
        })
        .await;
        assert!(stopped.is_ok());
        operator.resume(&generation).await.unwrap();
        let recovered = auth
            .token(Instant::now() + Duration::from_secs(2))
            .await
            .unwrap();
        assert_ne!(recovered.generation, generation);
        assert!(!auth.cancellation().is_cancelled());
        let roster = transport
            .post_json(
                Operation::Models,
                &serde_json::json!({}),
                &recovered,
                Instant::now() + Duration::from_secs(2),
            )
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(roster, serde_json::json!({"models":{}}));
        server.abort();
    }
    #[tokio::test]
    async fn ordinary_request_rejection_does_not_close_account() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let (origin, server) = mock(Router::new().route(
            "/v1internal:streamGenerateContent",
            post(move || {
                let calls = calls.clone();
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        (
                            axum::http::StatusCode::BAD_REQUEST,
                            [("content-type", "application/json")],
                            "{\"error\":\"synthetic schema rejection\"}",
                        )
                    } else {
                        (
                            axum::http::StatusCode::OK,
                            [("content-type", "text/event-stream")],
                            "data: synthetic\n\n",
                        )
                    }
                }
            }),
        ))
        .await;
        let d = tempfile::tempdir().unwrap();
        let auth = AntigravityAuth::synthetic(d.path().join("state"), origin).await;
        let token = auth
            .token(Instant::now() + Duration::from_secs(2))
            .await
            .unwrap();
        let transport = AntigravityTransport::new(auth.clone());
        let body = serde_json::json!({"model":"gemini-4-argon-medium"});
        let rejected = transport
            .post_json(
                Operation::Generate,
                &body,
                &token,
                Instant::now() + Duration::from_secs(2),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(rejected.status, 400);
        assert_eq!(rejected.code, "provider_invalid_request");
        assert!(auth.availability_error().await.is_none());
        assert!(
            transport
                .post_json(
                    Operation::Generate,
                    &body,
                    &token,
                    Instant::now() + Duration::from_secs(2)
                )
                .await
                .is_ok()
        );
        assert_eq!(count.load(Ordering::SeqCst), 2);
        server.abort();
    }
    #[tokio::test]
    async fn claude_beta_header_is_fixed_and_family_scoped() {
        use std::sync::{Arc, Mutex};
        let seen = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let captured = seen.clone();
        let (origin, server) = mock(Router::new().route(
            "/v1internal:streamGenerateContent",
            post(move |headers: axum::http::HeaderMap| {
                let captured = captured.clone();
                async move {
                    captured.lock().unwrap().push(
                        headers
                            .get("anthropic-beta")
                            .and_then(|h| h.to_str().ok())
                            .map(str::to_owned),
                    );
                    (
                        [("content-type", "text/event-stream")],
                        "data: synthetic\n\n",
                    )
                }
            }),
        ))
        .await;
        let d = tempfile::tempdir().unwrap();
        let auth = AntigravityAuth::synthetic(d.path().join("state"), origin).await;
        let token = auth
            .token(Instant::now() + Duration::from_secs(2))
            .await
            .unwrap();
        let transport = AntigravityTransport::new(auth);
        for model in [
            "claude-opus-5-5-medium",
            "claude-sonnet-5-5-high",
            "gemini-4-argon-medium",
            "gpt-oss-120b-medium",
        ] {
            transport
                .post_json(
                    Operation::Generate,
                    &serde_json::json!({"model":model}),
                    &token,
                    Instant::now() + Duration::from_secs(2),
                )
                .await
                .unwrap();
        }
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                Some("interleaved-thinking-2025-05-14".into()),
                Some("interleaved-thinking-2025-05-14".into()),
                None,
                None
            ]
        );
        server.abort();
    }
    #[tokio::test]
    async fn expired_deadline_prevents_dispatch() {
        let d = tempfile::tempdir().unwrap();
        let auth = AntigravityAuth::new(d.path().join("state"), OAuthConfig::default()).unwrap();
        let token = AccessToken {
            access_token: "synthetic".into(),
            generation: "synthetic".into(),
            project: "synthetic".into(),
        };
        let error = AntigravityTransport::new(auth)
            .post_json(
                Operation::Generate,
                &serde_json::json!({}),
                &token,
                Instant::now() - Duration::from_secs(1),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, "agy_deadline_exceeded");
    }
}
