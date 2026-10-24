//! Router-owned Google authorization. Never imports AGY/OMP token files.
//! All authenticated operations share this gate and fixed, reviewed origins.
//! OAuth registration provenance: OMP 0b5c27ab, catalog auth/google-antigravity.kdl.
//! This bundled installed-app registration was explicitly selected by the operator.
//! Login is an explicit account recovery exception; protocol restrictions still apply.
use crate::protocol::ApiError;
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{OwnedRwLockReadGuard, RwLock},
};
use tokio_util::sync::CancellationToken;

pub const CCA_ORIGIN: &str = "https://daily-cloudcode-pa.googleapis.com";
pub const PINNED_USER_AGENT: &str =
    "antigravity/hub/2.26.0 (aidev_client; os_type=darwin; arch=arm64; cl=963137146)";
pub const BASELINE_REVISION: &str = "omp-0b5c27ab-antigravity-v3";
// Match OMP's google-antigravity credential expires.skew-ms=300000.
// Store actual expiry; renew before dispatch inside the same five-minute window.
const TOKEN_REFRESH_SKEW_SECS: u64 = 300;
/// The reviewed baseline is compiled into the binary, never loaded from a
/// runtime path or changed by release metadata or environment variables.
pub fn baseline_valid() -> bool {
    static VALID: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *VALID
        .get_or_init(|| baseline_document_valid(include_str!("../docs/antigravity-baseline.json")))
}
fn baseline_document_valid(document: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(document) else {
        return false;
    };
    bundled_registration_valid(&v)
        && v["schema_version"] == 1
        && v["adapter_revision"] == BASELINE_REVISION
        && v["authentication"]["refresh_skew_seconds"] == TOKEN_REFRESH_SKEW_SECS
        && v.pointer("/identity/user_agent").and_then(Value::as_str) == Some(PINNED_USER_AGENT)
        && v.pointer("/identity/version").and_then(Value::as_str)
            == Some(crate::antigravity_release::PINNED_VERSION)
        && PINNED_USER_AGENT.starts_with(&format!(
            "antigravity/hub/{} ",
            crate::antigravity_release::PINNED_VERSION
        ))
        && v.pointer("/source/commit").and_then(Value::as_str)
            == Some(crate::antigravity_release::PINNED_REVISION)
        && [
            ("inference_origin", CCA_ORIGIN),
            (
                "inference_path",
                "/v1internal:streamGenerateContent?alt=sse",
            ),
            ("model_path", "/v1internal:fetchAvailableModels"),
            ("quota_path", "/v1internal:retrieveUserQuotaSummary"),
            ("project_path", "/v1internal:loadCodeAssist"),
            ("authorization_url", AUTHORIZE),
            ("token_url", TOKEN),
            ("userinfo_url", USERINFO),
        ]
        .iter()
        .all(|(field, expected)| v["destinations"][*field].as_str() == Some(*expected))
        && v.pointer("/identity/automatic_identity_updates") == Some(&Value::Bool(false))
        && v.pointer("/identity/caller_identity_overrides") == Some(&Value::Bool(false))
        && v.pointer("/destinations/redirects") == Some(&Value::Bool(false))
        && v.pointer("/destinations/environment_http_proxies") == Some(&Value::Bool(false))
        && v.pointer("/destinations/automatic_endpoint_or_account_fallback")
            == Some(&Value::Bool(false))
}
const AUTHORIZE: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN: &str = "https://oauth2.googleapis.com/token";
const USERINFO: &str = "https://www.googleapis.com/oauth2/v1/userinfo?alt=json";
const SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/cclog https://www.googleapis.com/auth/experimentsandconfigs";
const MAX_STATE: usize = 128 * 1024;
const MAX_HTTP: usize = 1024 * 1024;

// Public installed-app registration, packaged like the pinned OMP source.
// Base64 is not encryption. Personal access/refresh tokens are never bundled.
const REVIEWED_CLIENT_ID_BASE64: &str = "MTA3MTAwNjA2MDU5MS10bWhzc2luMmgyMWxjcmUyMzV2dG9sb2poNGc0MDNlcC5hcHBzLmdvb2dsZXVzZXJjb250ZW50LmNvbQ==";
const REVIEWED_CLIENT_SECRET_BASE64: &str = "R09DU1BYLUs1OEZXUjQ4NkxkTEoxbUxCOHNYQzR6NnFEQWY=";

fn decoded_registration(identifier: &str, secret: &str) -> Option<OAuthConfig> {
    let config = OAuthConfig {
        client_id: String::from_utf8(STANDARD.decode(identifier).ok()?).ok()?,
        client_secret: Some(String::from_utf8(STANDARD.decode(secret).ok()?).ok()?),
    };
    config.validate().ok()?;
    Some(config)
}
fn reviewed_registration() -> Option<OAuthConfig> {
    decoded_registration(REVIEWED_CLIENT_ID_BASE64, REVIEWED_CLIENT_SECRET_BASE64)
}
fn registration_matches_baseline(config: &OAuthConfig, baseline: &Value) -> bool {
    let Some(secret) = config.client_secret.as_ref() else {
        return false;
    };
    baseline
        .pointer("/authentication/registration/client_id_sha256")
        .and_then(Value::as_str)
        == Some(format!("{:x}", Sha256::digest(config.client_id.as_bytes())).as_str())
        && baseline
            .pointer("/authentication/registration/client_secret_sha256")
            .and_then(Value::as_str)
            == Some(format!("{:x}", Sha256::digest(secret.as_bytes())).as_str())
}
fn bundled_registration_valid(baseline: &Value) -> bool {
    reviewed_registration().is_some_and(|config| registration_matches_baseline(&config, baseline))
}
fn compiled_baseline() -> Value {
    serde_json::from_str(include_str!("../docs/antigravity-baseline.json")).unwrap_or(Value::Null)
}

/// No Debug implementation: the registration may contain a client secret.
#[derive(Clone, Default)]
pub struct OAuthConfig {
    pub client_id: String,
    pub client_secret: Option<String>,
}
impl OAuthConfig {
    /// Read only process environment. Never reads or sources configuration files.
    pub fn from_env() -> Self {
        let configured_id = std::env::var("AI_ROUTER_GOOGLE_CLIENT_ID")
            .ok()
            .filter(|s| !s.trim().is_empty());
        if let Some(client_id) = configured_id {
            return Self {
                client_id,
                client_secret: std::env::var("AI_ROUTER_GOOGLE_CLIENT_SECRET")
                    .ok()
                    .filter(|s| !s.is_empty()),
            };
        }
        // Invalid bundled data fails validation and the compiled baseline gate.
        reviewed_registration().unwrap_or_default()
    }
    fn validate(&self) -> Result<(), ApiError> {
        if self.client_id.is_empty()
            || self.client_id.len() > 1024
            || self.client_id.chars().any(char::is_control)
            || self
                .client_secret
                .as_ref()
                .is_some_and(|s| s.len() > 4096 || s.chars().any(char::is_control))
        {
            return Err(ApiError::new(
                503,
                "agy_oauth_configuration_required",
                "Configure an operator-owned reviewed Google installed-application OAuth registration using AI_ROUTER_GOOGLE_CLIENT_ID and optional AI_ROUTER_GOOGLE_CLIENT_SECRET in the process environment",
            ));
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Restriction {
    Authentication,
    Account,
    Protocol,
}
/// Deliberately no Debug: contains a bearer token.
pub struct AccessToken {
    pub access_token: String,
    pub generation: String,
    pub project: String,
}
#[derive(Clone)]
pub struct AntigravityAuth {
    directory: PathBuf,
    config: OAuthConfig,
    client: reqwest::Client,
    gate: Arc<RwLock<()>>,
    cancellation: Arc<Mutex<(Option<String>, CancellationToken)>>,
    #[cfg(test)]
    mock_origin: Option<String>,
}
/// An admission remains before the closure linearization point until the HTTP
/// headers arrive (or dispatch is cancelled). Closing first persists an intent,
/// then drains these shared permits before reporting closure complete.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum GenerationValidation {
    Valid,
    Busy,
}
pub(crate) struct DispatchPermit {
    _local: OwnedRwLockReadGuard<()>,
    _cross_process: File,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    generation: String,
    baseline: String,
    protocol_block: bool,
    account_block: Option<Restriction>,
    session: Option<Session>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Session {
    client_id: String,
    access_token: String,
    refresh_token: String,
    expires_at: u64,
    subject: String,
    project: String,
}
impl Default for State {
    fn default() -> Self {
        Self {
            version: 1,
            generation: uuid::Uuid::new_v4().to_string(),
            baseline: BASELINE_REVISION.into(),
            protocol_block: false,
            account_block: None,
            session: None,
        }
    }
}

impl AntigravityAuth {
    pub fn new(directory: PathBuf, config: OAuthConfig) -> Result<Self, ApiError> {
        Ok(Self {
            directory,
            config,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .connect_timeout(Duration::from_secs(5))
                .pool_max_idle_per_host(4)
                .build()
                .map_err(|_| transport_error())?,
            gate: Arc::new(RwLock::new(())),
            cancellation: Arc::new(Mutex::new((None, CancellationToken::new()))),
            #[cfg(test)]
            mock_origin: None,
        })
    }
    #[cfg(test)]
    pub(crate) async fn synthetic(directory: PathBuf, origin: String) -> Self {
        assert!(
            url::Url::parse(&origin)
                .is_ok_and(|u| u.scheme() == "http" && u.host_str() == Some("127.0.0.1"))
        );
        let mut this = Self::new(
            directory,
            OAuthConfig {
                client_id: "synthetic-client".into(),
                client_secret: None,
            },
        )
        .unwrap();
        this.mock_origin = Some(origin);
        let lock = this
            .lock(Instant::now() + Duration::from_secs(2))
            .await
            .unwrap();
        let mut s = lock.read().unwrap();
        s.session = Some(Session {
            client_id: "synthetic-client".into(),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            expires_at: now() + 3600,
            subject: "synthetic-user".into(),
            project: "synthetic-project".into(),
        });
        lock.write(&s).unwrap();
        drop(lock);
        this
    }
    pub(crate) fn client(&self) -> reqwest::Client {
        self.client.clone()
    }
    pub(crate) fn endpoint(&self, url: &str) -> String {
        #[cfg(test)]
        if let Some(origin) = &self.mock_origin {
            let parsed = url::Url::parse(url).expect("fixed URL");
            return format!(
                "{}{}{}",
                origin,
                parsed.path(),
                parsed.query().map(|q| format!("?{q}")).unwrap_or_default()
            );
        }
        url.to_owned()
    }
    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation
            .lock()
            .expect("cancellation mutex")
            .1
            .clone()
    }
    fn cancel(&self) {
        self.cancellation
            .lock()
            .expect("cancellation mutex")
            .1
            .cancel();
    }
    fn cancel_generation(&self, generation: &str) {
        let epoch = self.cancellation.lock().expect("cancellation mutex");
        if epoch.0.as_deref() == Some(generation) {
            epoch.1.cancel();
        }
    }
    fn observe_state(&self, state: &State, _locked: &LockedState) {
        let mut epoch = self.cancellation.lock().expect("cancellation mutex");
        if epoch.0.as_deref() != Some(&state.generation) {
            epoch.1.cancel();
            *epoch = (Some(state.generation.clone()), CancellationToken::new());
        }
        if allowed(state, None).is_err() {
            epoch.1.cancel();
        }
    }
    pub(crate) fn try_validate_generation(
        &self,
        generation: &str,
        deadline: Instant,
    ) -> Result<GenerationValidation, ApiError> {
        if Instant::now() >= deadline {
            return Err(timeout());
        }
        let directory = private_directory(&self.directory)?;
        let file = open_private(&directory, ".state-lock", true)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Ok(GenerationValidation::Busy);
            }
            return Err(storage_error());
        }
        let lock = LockedState {
            directory,
            _lock: file,
        };
        let state = lock.read()?;
        self.observe_state(&state, &lock);
        allowed(&state, Some(generation))?;
        self.ensure_registration(&state)?;
        Ok(GenerationValidation::Valid)
    }
    pub async fn generation(&self) -> Result<String, ApiError> {
        let lock = self.lock(Instant::now() + Duration::from_secs(5)).await?;
        let state = lock.read()?;
        self.observe_state(&state, &lock);
        Ok(state.generation)
    }
    pub async fn status(&self) -> Result<Value, ApiError> {
        let lock = self.lock(Instant::now() + Duration::from_secs(5)).await?;
        let s = lock.read()?;
        self.observe_state(&s, &lock);
        Ok(
            json!({"provider":"agy","adapter":BASELINE_REVISION,"authenticated":s.session.is_some(),"available":allowed(&s,None).is_ok() && self.ensure_registration(&s).is_ok(),"baseline_valid":baseline_valid(),"saved_baseline":s.baseline,"baseline_update_required":s.baseline != BASELINE_REVISION,"bundled_registration_valid":bundled_registration_valid(&compiled_baseline()),"registration_source":if self.config.validate().is_err(){"unconfigured"}else if registration_matches_baseline(&self.config,&compiled_baseline()){"reviewed_omp"}else{"operator_configured"},"session_registration_matches":s.session.as_ref().map(|session|session.client_id == self.config.client_id),"generation":s.generation,"protocol_blocked":s.protocol_block,"account_restriction":s.account_block,"project_resolved":s.session.as_ref().is_some_and(|s|!s.project.is_empty()),"registration_configured":self.config.validate().is_ok(),"legacy_auth_import":"unsupported_no_documented_handoff"}),
        )
    }
    pub async fn availability_error(&self) -> Option<ApiError> {
        let lock = match self.lock(Instant::now() + Duration::from_secs(5)).await {
            Ok(lock) => lock,
            Err(error) => return Some(error),
        };
        let state = match lock.read() {
            Ok(state) => state,
            Err(error) => return Some(error),
        };
        self.observe_state(&state, &lock);
        allowed(&state, None)
            .and_then(|_| self.ensure_registration(&state))
            .err()
    }
    fn ensure_registration(&self, state: &State) -> Result<(), ApiError> {
        self.config.validate()?;
        if state
            .session
            .as_ref()
            .is_some_and(|session| session.client_id != self.config.client_id)
        {
            return Err(ApiError::new(
                503,
                "agy_oauth_registration_changed",
                "Saved session belongs to a different Google OAuth registration; sign in explicitly",
            ));
        }
        Ok(())
    }
    /// The read guard linearizes dispatch against local restriction/recovery writes.
    pub(crate) async fn dispatch_guard(
        &self,
        generation: &str,
        deadline: Instant,
    ) -> Result<DispatchPermit, ApiError> {
        let guard = tokio::time::timeout_at(deadline.into(), self.gate.clone().read_owned())
            .await
            .map_err(|_| timeout())?;
        let cross_process = self.dispatch_lock(false, deadline).await?;
        let lock = self.lock(deadline).await?;
        let s = lock.read()?;
        self.observe_state(&s, &lock);
        allowed(&s, Some(generation))?;
        self.ensure_registration(&s)?;
        if self.cancellation().is_cancelled() {
            return Err(restricted(Restriction::Account));
        }
        Ok(DispatchPermit {
            _local: guard,
            _cross_process: cross_process,
        })
    }
    pub async fn token(&self, deadline: Instant) -> Result<AccessToken, ApiError> {
        // Once refresh starts its durable replacement is owned independently of
        // caller cancellation; the original absolute deadline still bounds I/O.
        let this = self.clone();
        tokio::spawn(async move { this.token_inner(deadline).await })
            .await
            .map_err(|_| transport_error())?
    }
    async fn token_inner(&self, deadline: Instant) -> Result<AccessToken, ApiError> {
        let _read = tokio::time::timeout_at(deadline.into(), self.gate.read())
            .await
            .map_err(|_| timeout())?;
        let _cross_process = self.dispatch_lock(false, deadline).await?;
        let lock = self.lock(deadline).await?;
        let mut state = lock.read()?;
        self.observe_state(&state, &lock);
        allowed(&state, None)?;
        if self.cancellation().is_cancelled() {
            return Err(restricted(Restriction::Account));
        }
        self.ensure_registration(&state)?;
        let mut session = state.session.clone().ok_or_else(auth_required)?;
        if session.expires_at <= now().saturating_add(TOKEN_REFRESH_SKEW_SECS) {
            let mut form = vec![
                ("grant_type", "refresh_token"),
                ("client_id", session.client_id.as_str()),
                ("refresh_token", session.refresh_token.as_str()),
            ];
            if let Some(secret) = &self.config.client_secret {
                form.push(("client_secret", secret));
            }
            let response = self
                .bounded(self.client.post(self.endpoint(TOKEN)).form(&form), deadline)
                .await;
            let (status, reply) = match response {
                Ok(reply) => reply,
                Err(error) if error.code == "agy_redirect_refused" => {
                    state.protocol_block = true;
                    lock.write(&state)?;
                    self.cancel();
                    let generation = state.generation.clone();
                    drop(lock);
                    drop(_cross_process);
                    drop(_read);
                    self.block_for_generation(Restriction::Protocol, &generation)
                        .await?;
                    return Err(restricted(Restriction::Protocol));
                }
                Err(error) if error.code == "agy_auth_response_invalid" => {
                    state.account_block = Some(Restriction::Authentication);
                    lock.write(&state)?;
                    self.cancel();
                    let generation = state.generation.clone();
                    drop(lock);
                    drop(_cross_process);
                    drop(_read);
                    self.block_for_generation(Restriction::Authentication, &generation)
                        .await?;
                    return Err(restricted(Restriction::Authentication));
                }
                Err(error) => return Err(error),
            };
            if !status.is_success() {
                if matches!(status.as_u16(), 401 | 403)
                    || matches!(
                        reply.get("error").and_then(Value::as_str),
                        Some("invalid_grant" | "invalid_client" | "unauthorized_client")
                    )
                {
                    state.account_block = Some(Restriction::Authentication);
                    lock.write(&state)?;
                    self.cancel();
                    let generation = state.generation.clone();
                    drop(lock);
                    drop(_cross_process);
                    drop(_read);
                    self.block_for_generation(Restriction::Authentication, &generation)
                        .await?;
                    return Err(restricted(Restriction::Authentication));
                }
                return Err(transport_error());
            }
            if update_tokens(&mut session, &reply, false).is_err() {
                state.account_block = Some(Restriction::Authentication);
                lock.write(&state)?;
                self.cancel();
                let generation = state.generation.clone();
                drop(lock);
                drop(_cross_process);
                drop(_read);
                self.block_for_generation(Restriction::Authentication, &generation)
                    .await?;
                return Err(restricted(Restriction::Authentication));
            }
            state.session = Some(session.clone());
            lock.write(&state)?;
        }
        Ok(AccessToken {
            access_token: session.access_token,
            generation: state.generation,
            project: session.project,
        })
    }
    pub async fn block_for_generation(
        &self,
        kind: Restriction,
        generation: &str,
    ) -> Result<(), ApiError> {
        self.cancel_generation(generation);
        let this = self.clone();
        let generation = generation.to_owned();
        tokio::spawn(async move {
            let deadline = Instant::now() + Duration::from_secs(20);
            let lock = this.lock(deadline).await?;
            let mut s = lock.read()?;
            if s.generation != generation {
                return Ok(());
            }
            this.cancel();
            if kind == Restriction::Protocol {
                s.protocol_block = true;
            } else {
                s.account_block = Some(kind);
            }
            lock.write(&s)?;
            // Durable restriction is the closing intent. Release state lock so
            // header watchers can observe it and relinquish shared admissions.
            drop(lock);
            let _write = tokio::time::timeout_at(deadline.into(), this.gate.write())
                .await
                .map_err(|_| timeout())?;
            let _closed = this.dispatch_lock(true, deadline).await?;
            Ok(())
        })
        .await
        .map_err(|_| storage_error())?
    }
    /// Explicit recovery is generation-bound and never retries the failed request.
    pub async fn resume(&self, expected_generation: &str) -> Result<(), ApiError> {
        if !baseline_valid() {
            return Err(restricted(Restriction::Protocol));
        }
        self.config.validate()?;
        let deadline = Instant::now() + Duration::from_secs(20);
        let _write = tokio::time::timeout_at(deadline.into(), self.gate.write())
            .await
            .map_err(|_| timeout())?;
        let _closed = self.dispatch_lock(true, deadline).await?;
        let lock = self.lock(deadline).await?;
        let mut s = lock.read()?;
        if s.generation != expected_generation {
            return Err(ApiError::bad_request(
                "Antigravity account generation changed; inspect status before recovery",
            ));
        }
        s.protocol_block = false;
        s.account_block = None;
        s.baseline = BASELINE_REVISION.into();
        s.generation = uuid::Uuid::new_v4().to_string();
        lock.write(&s)?;
        self.observe_state(&s, &lock);
        Ok(())
    }
    /// Local logout only: no unreviewed remote revocation or account mutation.
    pub async fn logout(&self) -> Result<(), ApiError> {
        let this = self.clone();
        tokio::spawn(async move {
            this.cancel();
            let deadline = Instant::now() + Duration::from_secs(20);
            let lock = this.lock(deadline).await?;
            let mut s = lock.read()?;
            s.session = None;
            s.account_block = None;
            s.generation = uuid::Uuid::new_v4().to_string();
            lock.write(&s)?;
            this.observe_state(&s, &lock);
            drop(lock);
            let _write = tokio::time::timeout_at(deadline.into(), this.gate.write())
                .await
                .map_err(|_| timeout())?;
            let _closed = this.dispatch_lock(true, deadline).await?;
            Ok(())
        })
        .await
        .map_err(|_| storage_error())?
    }
    pub async fn login(&self, port: u16) -> Result<Value, ApiError> {
        if !baseline_valid() {
            return Err(restricted(Restriction::Protocol));
        }
        self.config.validate()?;
        if port == 0 {
            return Err(ApiError::bad_request(
                "Choose a fixed nonzero loopback OAuth callback port",
            ));
        }
        let listener = TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port))
            .await
            .map_err(|_| {
                ApiError::unavailable("Cannot bind Antigravity loopback OAuth callback")
            })?;
        let initial = self
            .lock(Instant::now() + Duration::from_secs(5))
            .await?
            .read()?;
        validate_baseline(&initial)?;
        let initial_generation = initial.generation;
        let state = random();
        let verifier = random();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let redirect = format!("http://127.0.0.1:{port}/oauth-callback");
        let mut url = url::Url::parse(AUTHORIZE).expect("fixed URL");
        url.query_pairs_mut().extend_pairs([
            ("client_id", self.config.client_id.as_str()),
            ("redirect_uri", &redirect),
            ("response_type", "code"),
            ("scope", SCOPES),
            ("access_type", "offline"),
            ("prompt", "consent"),
            ("state", &state),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
        ]);
        println!(
            "Open this URL to authorize the router's independent Antigravity session:\n{url}\nFor remote Docker, forward the callback port to this machine. Waiting up to 10 minutes."
        );
        let deadline = Instant::now() + Duration::from_secs(600);
        let code = receive_code(listener, &state, deadline).await?;
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("client_id", self.config.client_id.as_str()),
            ("code", code.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("code_verifier", verifier.as_str()),
        ];
        if let Some(secret) = &self.config.client_secret {
            form.push(("client_secret", secret));
        }
        let (status, reply) = self
            .login_request(
                self.client.post(self.endpoint(TOKEN)).form(&form),
                deadline,
                &initial_generation,
            )
            .await?;
        if !status.is_success() {
            return Err(ApiError::new(
                503,
                "agy_oauth_exchange_failed",
                "Google rejected the authorization exchange; no saved session changed",
            ));
        }
        let mut session = Session {
            client_id: self.config.client_id.clone(),
            access_token: String::new(),
            refresh_token: String::new(),
            expires_at: 0,
            subject: String::new(),
            project: String::new(),
        };
        update_tokens(&mut session, &reply, true)?;
        let (status, identity) = self
            .login_request(
                self.client
                    .get(self.endpoint(USERINFO))
                    .bearer_auth(&session.access_token),
                deadline,
                &initial_generation,
            )
            .await?;
        if !status.is_success() {
            return Err(auth_required());
        }
        session.subject = identity
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= 1024)
            .ok_or_else(auth_required)?
            .into();
        let (status, project) = self
            .login_request(
                self.client
                    .post(self.endpoint(&format!("{CCA_ORIGIN}/v1internal:loadCodeAssist")))
                    .bearer_auth(&session.access_token)
                    .header("User-Agent", PINNED_USER_AGENT)
                    .json(&json!({"metadata":{"ideType":"ANTIGRAVITY"}})),
                deadline,
                &initial_generation,
            )
            .await?;
        if !status.is_success() {
            return Err(ApiError::new(
                503,
                "agy_project_resolution_failed",
                "Antigravity project lookup rejected; no saved session changed",
            ));
        }
        session.project = existing_project(&project)?;
        let _write = tokio::time::timeout_at(deadline.into(), self.gate.write())
            .await
            .map_err(|_| timeout())?;
        let _closed = self.dispatch_lock(true, deadline).await?;
        let lock = self.lock(deadline).await?;
        let mut s = lock.read()?;
        if s.generation != initial_generation {
            return Err(ApiError::new(
                409,
                "agy_session_changed",
                "Antigravity session changed during login; repeat the explicit login",
            ));
        }
        self.cancel_generation(&initial_generation);
        s.session = Some(session);
        s.account_block = None;
        s.generation = uuid::Uuid::new_v4().to_string();
        lock.write(&s)?;
        self.observe_state(&s, &lock);
        drop(lock);
        drop(_write);
        self.status().await
    }
    async fn login_request(
        &self,
        request: reqwest::RequestBuilder,
        deadline: Instant,
        generation: &str,
    ) -> Result<(reqwest::StatusCode, Value), ApiError> {
        let _guard = tokio::time::timeout_at(deadline.into(), self.gate.read())
            .await
            .map_err(|_| timeout())?;
        let _cross_process = self.dispatch_lock(false, deadline).await?;
        let state = self.lock(deadline).await?.read()?;
        if state.generation != generation {
            return Err(ApiError::new(
                409,
                "agy_session_changed",
                "Antigravity session changed during explicit sign-in",
            ));
        }
        validate_baseline(&state)?;
        let result = self.bounded(request, deadline).await;
        // Never acquire the exclusive closure barrier while retaining our own
        // shared admission or local read guard.
        drop(_cross_process);
        drop(_guard);
        if result
            .as_ref()
            .is_err_and(|error| error.code == "agy_redirect_refused")
        {
            self.block_for_generation(Restriction::Protocol, generation)
                .await?;
            return Err(restricted(Restriction::Protocol));
        }
        result
    }
    async fn bounded(
        &self,
        request: reqwest::RequestBuilder,
        deadline: Instant,
    ) -> Result<(reqwest::StatusCode, Value), ApiError> {
        if !baseline_valid() {
            return Err(restricted(Restriction::Protocol));
        }
        let deadline = deadline.min(Instant::now() + Duration::from_secs(15));
        let mut r = tokio::time::timeout_at(deadline.into(), request.send())
            .await
            .map_err(|_| timeout())?
            .map_err(|_| transport_error())?;
        let status = r.status();
        if status.is_redirection() {
            return Err(ApiError::new(
                503,
                "agy_redirect_refused",
                "Antigravity authentication redirect refused",
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = tokio::time::timeout_at(deadline.into(), r.chunk())
            .await
            .map_err(|_| timeout())?
            .map_err(|_| transport_error())?
        {
            if bytes.len() + chunk.len() > MAX_HTTP {
                return Err(ApiError::unavailable(
                    "Antigravity control response exceeded its bound",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        let value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(_) if !status.is_success() => Value::Null,
            Err(_) => {
                return Err(ApiError::new(
                    503,
                    "agy_auth_response_invalid",
                    "Antigravity authorization response was not valid JSON; explicit review required",
                ));
            }
        };
        Ok((status, value))
    }
    async fn dispatch_lock(&self, exclusive: bool, deadline: Instant) -> Result<File, ApiError> {
        let directory = private_directory(&self.directory)?;
        let lock = open_private(&directory, ".dispatch-lock", true)?;
        let mode = if exclusive {
            libc::LOCK_EX
        } else {
            libc::LOCK_SH
        };
        loop {
            if Instant::now() >= deadline {
                return Err(timeout());
            }
            if unsafe { libc::flock(lock.as_raw_fd(), mode | libc::LOCK_NB) } == 0 {
                return Ok(lock);
            }
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(storage_error());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    async fn lock(&self, deadline: Instant) -> Result<LockedState, ApiError> {
        let directory = private_directory(&self.directory)?;
        let lock = open_private(&directory, ".state-lock", true)?;
        loop {
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(LockedState {
                    directory,
                    _lock: lock,
                });
            }
            if Instant::now() >= deadline {
                return Err(timeout());
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(storage_error());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
fn validate_baseline(s: &State) -> Result<(), ApiError> {
    if !baseline_valid() || s.protocol_block {
        return Err(restricted(Restriction::Protocol));
    }
    if s.baseline != BASELINE_REVISION {
        return Err(ApiError::new(
            503,
            "agy_baseline_update_required",
            "Antigravity has a newly reviewed baseline. Stop the gateway, run login.sh agy status, then login.sh agy resume to adopt it while keeping the existing sign-in.",
        ));
    }
    Ok(())
}
fn allowed(s: &State, generation: Option<&str>) -> Result<(), ApiError> {
    if generation.is_some_and(|g| g != s.generation) {
        return Err(ApiError::new(
            503,
            "agy_session_changed",
            "Antigravity session changed before dispatch",
        ));
    }
    validate_baseline(s)?;
    if let Some(kind) = s.account_block {
        return Err(restricted(kind));
    }
    if s.session.is_none() {
        return Err(auth_required());
    }
    Ok(())
}
fn update_tokens(s: &mut Session, v: &Value, initial: bool) -> Result<(), ApiError> {
    let token = v
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty() && t.len() < 65536)
        .ok_or_else(auth_required)?;
    if v.get("token_type")
        .and_then(Value::as_str)
        .is_none_or(|t| !t.eq_ignore_ascii_case("bearer"))
    {
        return Err(auth_required());
    }
    let expires = v
        .get("expires_in")
        .and_then(Value::as_u64)
        .filter(|n| *n > 90 && *n <= 86400)
        .ok_or_else(auth_required)?;
    let refresh = v
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty() && t.len() < 65536);
    if initial && refresh.is_none() {
        return Err(ApiError::new(
            503,
            "agy_refresh_required",
            "Google did not grant renewable access; repeat explicit consent",
        ));
    }
    if let Some(scope) = v.get("scope").and_then(Value::as_str) {
        let scopes: Vec<_> = scope.split_whitespace().collect();
        if !SCOPES.split_whitespace().all(|s| scopes.contains(&s)) {
            return Err(ApiError::new(
                503,
                "agy_scope_required",
                "Google did not grant all required Antigravity scopes",
            ));
        }
    }
    s.access_token = token.into();
    if let Some(refresh) = refresh {
        s.refresh_token = refresh.into();
    }
    s.expires_at = now().saturating_add(expires);
    Ok(())
}
fn existing_project(v: &Value) -> Result<String, ApiError> {
    let enrolled = v.get("currentTier").is_some_and(Value::is_object)
        || v.get("paidTier").is_some_and(Value::is_object);
    let project = v
        .get("cloudaicompanionProject")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty() && p.len() <= 256 && !p.chars().any(char::is_control));
    if !enrolled || project.is_none() {
        return Err(ApiError::new(
            503,
            "agy_onboarding_required",
            "Antigravity did not return an existing enrolled project; complete account provisioning using the official application, then sign in again. The router never enrolls a tier automatically",
        ));
    }
    Ok(project.expect("checked").into())
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn random() -> String {
    let mut b = [0u8; 32];
    OsRng.fill_bytes(&mut b);
    URL_SAFE_NO_PAD.encode(b)
}
pub(crate) fn timeout() -> ApiError {
    ApiError::new(
        504,
        "agy_deadline_exceeded",
        "Antigravity original request deadline exceeded",
    )
}
fn storage_error() -> ApiError {
    ApiError::unavailable(
        "Antigravity private state unavailable or unsafe; inspect directory ownership and permissions",
    )
}
fn transport_error() -> ApiError {
    ApiError::unavailable("Antigravity authorization transport failed; saved state retained")
}
fn auth_required() -> ApiError {
    ApiError::new(
        503,
        "agy_auth_required",
        "Antigravity needs a separate router-owned Google sign-in; legacy AGY state is preserved and is not imported",
    )
}
fn restricted(kind: Restriction) -> ApiError {
    match kind {
        Restriction::Protocol => ApiError::new(
            503,
            "agy_protocol_blocked",
            "Antigravity protocol is restricted pending explicit operator review and recovery",
        ),
        Restriction::Authentication => ApiError::new(
            503,
            "agy_auth_blocked",
            "Antigravity authorization rejected; explicit sign-in or reviewed recovery required",
        ),
        Restriction::Account => ApiError::new(
            503,
            "agy_account_blocked",
            "Antigravity account restricted; explicit operator recovery required",
        ),
    }
}

async fn receive_code(
    listener: TcpListener,
    state: &str,
    deadline: Instant,
) -> Result<String, ApiError> {
    let authority = format!(
        "127.0.0.1:{}",
        listener.local_addr().map_err(|_| transport_error())?.port()
    );
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline.into()) => {
                tasks.abort_all();
                return Err(timeout());
            }
            accepted = listener.accept(), if tasks.len() < 16 => {
                let (mut stream, _peer) = accepted.map_err(|_| transport_error())?;
                let expected = state.to_owned();
                let authority = authority.clone();
                tasks.spawn(async move {
                    let mut bytes = Vec::new();
                    let per_connection = tokio::time::Instant::now() + Duration::from_secs(5);
                    loop {
                        let mut buffer = [0u8; 1024];
                        let n = tokio::time::timeout_at(per_connection, stream.read(&mut buffer))
                            .await.ok()?.ok()?;
                        if n == 0 {
                            return None;
                        }
                        bytes.extend_from_slice(&buffer[..n]);
                        if bytes.len() > 16384 {
                            return None;
                        }
                        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let text = std::str::from_utf8(&bytes).ok()?;
                    let outcome = callback_outcome(text, &expected, &authority)?;
                    let body = match &outcome {
                        CallbackOutcome::Code(_) => "Authorization received. You can close this window.",
                        CallbackOutcome::Denied => "Authorization cancelled. You can close this window.",
                    };
                    let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body);
                    let _ = tokio::time::timeout(Duration::from_secs(1), stream.write_all(reply.as_bytes())).await;
                    Some(outcome)
                });
            }
            result = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Ok(Some(outcome))) = result {
                    tasks.abort_all();
                    return match outcome {
                        CallbackOutcome::Code(code) => Ok(code),
                        CallbackOutcome::Denied => Err(ApiError::new(400,"agy_oauth_cancelled","Google authorization was cancelled; no saved session changed")),
                    };
                }
            }
        }
    }
}

// Deliberately no Debug implementation: a successful outcome contains a code.
enum CallbackOutcome {
    Code(String),
    Denied,
}
fn callback_outcome(request: &str, state: &str, authority: &str) -> Option<CallbackOutcome> {
    let mut lines = request.lines();
    let line = lines.next()?;
    let mut fields = line.split_whitespace();
    if fields.next() != Some("GET") {
        return None;
    }
    let target = fields.next()?;
    if !matches!(fields.next(), Some("HTTP/1.1" | "HTTP/1.0"))
        || fields.next().is_some()
        || !target.starts_with("/oauth-callback?")
        || target.contains('#')
    {
        return None;
    }
    let hosts: Vec<_> = lines
        .filter_map(|l| l.split_once(':'))
        .filter(|(k, _)| k.eq_ignore_ascii_case("host"))
        .collect();
    if hosts.len() != 1 || hosts[0].1.trim() != authority {
        return None;
    }
    let url = url::Url::parse(&format!("http://{authority}{target}")).ok()?;
    let pairs: Vec<_> = url.query_pairs().collect();
    let states: Vec<_> = pairs.iter().filter(|(k, _)| k == "state").collect();
    let codes: Vec<_> = pairs.iter().filter(|(k, _)| k == "code").collect();
    let errors: Vec<_> = pairs.iter().filter(|(k, _)| k == "error").collect();
    if states.len() != 1 || states[0].1 != state {
        return None;
    }
    if codes.is_empty() && errors.len() == 1 && errors[0].1 == "access_denied" {
        return Some(CallbackOutcome::Denied);
    }
    if codes.len() != 1 || codes[0].1.is_empty() || codes[0].1.len() > 8192 || !errors.is_empty() {
        return None;
    }
    Some(CallbackOutcome::Code(codes[0].1.to_string()))
}

fn forbidden(name: &str) -> bool {
    name.starts_with(".env")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || matches!(name, "secrets" | "credentials")
}
fn private_directory(path: &Path) -> Result<File, ApiError> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|_| storage_error())?
            .join(path)
    };
    let components: Vec<_> = absolute.components().collect();
    if components.len() < 2 {
        return Err(storage_error());
    }
    let fd = unsafe {
        libc::open(
            c"/".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(storage_error());
    }
    let mut dir = unsafe { File::from_raw_fd(fd) };
    for (index, component) in components.iter().enumerate() {
        let name = match component {
            Component::RootDir => continue,
            Component::Normal(n) => n,
            _ => return Err(storage_error()),
        };
        if forbidden(name.to_str().ok_or_else(storage_error)?) {
            return Err(storage_error());
        }
        let c = CString::new(name.as_bytes()).map_err(|_| storage_error())?;
        let mut fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            if unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), 0o700) } < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(storage_error());
            }
            fd = unsafe {
                libc::openat(
                    dir.as_raw_fd(),
                    c.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
        }
        if fd < 0 {
            return Err(storage_error());
        }
        dir = unsafe { File::from_raw_fd(fd) };
        let m = dir.metadata().map_err(|_| storage_error())?;
        if index == components.len() - 1
            && (m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0)
        {
            return Err(storage_error());
        }
    }
    Ok(dir)
}
fn open_private(dir: &File, name: &str, create: bool) -> Result<File, ApiError> {
    let name = CString::new(name).map_err(|_| storage_error())?;
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | if create { libc::O_CREAT } else { 0 },
            0o600,
        )
    };
    if fd < 0 {
        return Err(storage_error());
    }
    let f = unsafe { File::from_raw_fd(fd) };
    let m = f.metadata().map_err(|_| storage_error())?;
    if !m.is_file()
        || m.nlink() != 1
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o077 != 0
        || m.len() > MAX_STATE as u64
    {
        return Err(storage_error());
    }
    Ok(f)
}
struct LockedState {
    directory: File,
    _lock: File,
}
impl LockedState {
    fn read(&self) -> Result<State, ApiError> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                self.directory.as_raw_fd(),
                c"session.json".as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } < 0
        {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
                let s = State::default();
                self.write(&s)?;
                return Ok(s);
            }
            return Err(storage_error());
        }
        let mut b = Vec::new();
        open_private(&self.directory, "session.json", false)?
            .take((MAX_STATE + 1) as u64)
            .read_to_end(&mut b)
            .map_err(|_| storage_error())?;
        if b.len() > MAX_STATE {
            return Err(storage_error());
        }
        let s: State = serde_json::from_slice(&b).map_err(|_| storage_error())?;
        validate_state(&s)?;
        Ok(s)
    }
    fn write(&self, s: &State) -> Result<(), ApiError> {
        validate_state(s)?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                self.directory.as_raw_fd(),
                c"session.json".as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
        {
            open_private(&self.directory, "session.json", false)?;
        } else if std::io::Error::last_os_error().kind() != std::io::ErrorKind::NotFound {
            return Err(storage_error());
        }
        let b = serde_json::to_vec(s).map_err(|_| storage_error())?;
        if b.len() > MAX_STATE {
            return Err(storage_error());
        }
        let name = CString::new(format!(".state-{}.tmp", uuid::Uuid::new_v4())).expect("uuid");
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(storage_error());
        }
        let mut f = unsafe { File::from_raw_fd(fd) };
        let result = (|| {
            f.write_all(&b).map_err(|_| storage_error())?;
            f.sync_all().map_err(|_| storage_error())?;
            if unsafe {
                libc::renameat(
                    self.directory.as_raw_fd(),
                    name.as_ptr(),
                    self.directory.as_raw_fd(),
                    c"session.json".as_ptr(),
                )
            } != 0
            {
                return Err(storage_error());
            }
            self.directory.sync_all().map_err(|_| storage_error())
        })();
        if result.is_err() {
            unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) };
        }
        result
    }
}
fn validate_state(s: &State) -> Result<(), ApiError> {
    if s.version != 1
        || uuid::Uuid::parse_str(&s.generation).is_err()
        || s.baseline.is_empty()
        || s.baseline.len() > 256
        || s.session.as_ref().is_some_and(|s| {
            s.client_id.is_empty()
                || s.access_token.is_empty()
                || s.refresh_token.is_empty()
                || s.subject.is_empty()
                || s.project.is_empty()
                || s.access_token.len() > 65536
                || s.refresh_token.len() > 65536
                || s.client_id.len() > 1024
                || s.project.len() > 1024
                || s.subject.len() > 1024
        })
    {
        return Err(storage_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_registration() -> OAuthConfig {
        OAuthConfig {
            client_id: "synthetic-client".into(),
            client_secret: None,
        }
    }
    fn store() -> (tempfile::TempDir, AntigravityAuth) {
        let d = tempfile::tempdir().unwrap();
        let auth = AntigravityAuth::new(d.path().join("state"), test_registration()).unwrap();
        (d, auth)
    }
    async fn seed(auth: &AntigravityAuth) -> String {
        let l = auth
            .lock(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        let mut s = l.read().unwrap();
        s.session = Some(Session {
            client_id: "synthetic-client".into(),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            expires_at: now() + 3600,
            subject: "synthetic-user".into(),
            project: "synthetic-project".into(),
        });
        l.write(&s).unwrap();
        s.generation
    }
    #[tokio::test]
    async fn persistent_restriction_generation_and_explicit_resume() {
        let (_d, a) = store();
        let g = seed(&a).await;
        a.block_for_generation(Restriction::Protocol, &g)
            .await
            .unwrap();
        assert_eq!(
            a.token(Instant::now() + Duration::from_secs(1))
                .await
                .err()
                .unwrap()
                .code,
            "agy_protocol_blocked"
        );
        let b = AntigravityAuth::new(a.directory.clone(), test_registration()).unwrap();
        assert_eq!(
            b.availability_error().await.unwrap().code,
            "agy_protocol_blocked"
        );
        b.resume(&g).await.unwrap();
        assert_ne!(b.generation().await.unwrap(), g);
        a.block_for_generation(Restriction::Account, &g)
            .await
            .unwrap();
        assert!(b.availability_error().await.is_none());
    }

    #[tokio::test]
    async fn reviewed_upgrade_requires_explicit_resume_and_preserves_the_matching_session() {
        for previous in ["omp-2220e581-antigravity-v1", "omp-d5cd750e-antigravity-v2"] {
            let (_root, auth) = store();
            let generation = seed(&auth).await;
            let lock = auth
                .lock(Instant::now() + Duration::from_secs(1))
                .await
                .unwrap();
            let mut state = lock.read().unwrap();
            state.baseline = previous.into();
            let session_before = serde_json::to_value(&state.session).unwrap();
            lock.write(&state).unwrap();
            drop(lock);
            let status = auth.status().await.unwrap();
            assert_eq!(status["baseline_update_required"], true);
            assert_eq!(status["protocol_blocked"], false);
            assert_eq!(status["authenticated"], true);
            assert_eq!(status["available"], false);
            assert_eq!(
                auth.token(Instant::now() + Duration::from_secs(1))
                    .await
                    .err()
                    .unwrap()
                    .code,
                "agy_baseline_update_required"
            );
            assert_eq!(
                auth.dispatch_guard(&generation, Instant::now() + Duration::from_secs(1))
                    .await
                    .err()
                    .unwrap()
                    .code,
                "agy_baseline_update_required"
            );

            auth.resume(&generation).await.unwrap();
            let lock = auth
                .lock(Instant::now() + Duration::from_secs(1))
                .await
                .unwrap();
            let after = lock.read().unwrap();
            assert_eq!(
                serde_json::to_value(&after.session).unwrap(),
                session_before
            );
            assert_eq!(after.baseline, BASELINE_REVISION);
            assert_ne!(after.generation, generation);
            let current = after.generation;
            drop(lock);
            assert_eq!(
                auth.status().await.unwrap()["baseline_update_required"],
                false
            );
            assert!(auth.availability_error().await.is_none());
            assert_eq!(
                auth.try_validate_generation(&generation, Instant::now() + Duration::from_secs(1))
                    .err()
                    .unwrap()
                    .code,
                "agy_session_changed"
            );
            assert!(
                auth.dispatch_guard(&current, Instant::now() + Duration::from_secs(1))
                    .await
                    .is_ok()
            );
        }
    }

    #[tokio::test]
    async fn invalid_registration_cannot_adopt_a_new_baseline_on_resume() {
        let (_root, auth) = store();
        let generation = seed(&auth).await;
        let invalid = AntigravityAuth::new(auth.directory.clone(), OAuthConfig::default()).unwrap();
        assert_eq!(
            invalid.resume(&generation).await.unwrap_err().code,
            "agy_oauth_configuration_required"
        );
        assert_eq!(auth.generation().await.unwrap(), generation);
    }
    #[tokio::test]
    async fn no_auth_status_does_not_require_registration() {
        let d = tempfile::tempdir().unwrap();
        let a = AntigravityAuth::new(d.path().join("state"), OAuthConfig::default()).unwrap();
        assert!(
            !a.status().await.unwrap()["authenticated"]
                .as_bool()
                .unwrap()
        );
        assert_eq!(
            a.availability_error().await.unwrap().code,
            "agy_auth_required"
        );
    }
    #[tokio::test]
    async fn forbidden_components_and_symlinks_refused_without_read() {
        let (d, a) = store();
        let bad = AntigravityAuth::new(
            d.path().join("credentials").join("state"),
            test_registration(),
        )
        .unwrap();
        assert!(bad.status().await.is_err());
        let _ = seed(&a).await;
        std::fs::remove_file(a.directory.join("session.json")).unwrap();
        std::os::unix::fs::symlink("synthetic-never-read", a.directory.join("session.json"))
            .unwrap();
        assert!(a.status().await.is_err());
    }
    #[test]
    fn callback_requires_exact_host_state_path_and_no_duplicates() {
        let good = "GET /oauth-callback?state=random&code=synthetic HTTP/1.1\r\nHost: 127.0.0.1:1456\r\n\r\n";
        assert!(
            matches!(callback_outcome(good,"random","127.0.0.1:1456"),Some(CallbackOutcome::Code(code)) if code=="synthetic")
        );
        for bad in [
            good.replace("127.0.0.1:1456", "evil.invalid"),
            good.replace("state=random", "state=wrong"),
            good.replace("state=random", "state=random&state=random"),
            good.replace("code=synthetic", "code=synthetic&code=other"),
            good.replace("/oauth-callback?", "/other?"),
            good.replace("Host:", "Host: evil.invalid\r\nHost:"),
            good.replace("code=synthetic", "code=synthetic&error=denied"),
        ] {
            assert!(callback_outcome(&bad, "random", "127.0.0.1:1456").is_none());
        }
    }
    #[test]
    fn callback_denial_requires_exact_authority_state_and_exclusive_error() {
        let denied = "GET /oauth-callback?state=random&error=access_denied HTTP/1.1\r\nHost: 127.0.0.1:1456\r\n\r\n";
        assert!(matches!(
            callback_outcome(denied, "random", "127.0.0.1:1456"),
            Some(CallbackOutcome::Denied)
        ));
        for bad in [
            denied.replace("state=random", "state=wrong"),
            denied.replace("state=random", "state=random&state=random"),
            denied.replace(
                "error=access_denied",
                "error=access_denied&error=access_denied",
            ),
            denied.replace("error=access_denied", "error=access_denied&code=synthetic"),
            denied.replace("error=access_denied", "error=access_denied&code="),
            denied.replace("error=access_denied", "error=unknown"),
            denied.replace("127.0.0.1:1456", "evil.invalid"),
        ] {
            assert!(callback_outcome(&bad, "random", "127.0.0.1:1456").is_none());
        }
    }
    #[tokio::test]
    async fn callback_wrong_state_cannot_cancel_flow_but_valid_denial_is_prompt() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let waiting = tokio::spawn(receive_code(
            listener,
            "synthetic-state",
            Instant::now() + Duration::from_secs(5),
        ));
        let mut wrong = tokio::net::TcpStream::connect(addr).await.unwrap();
        wrong.write_all(format!("GET /oauth-callback?state=wrong&error=access_denied HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",addr.port()).as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(!waiting.is_finished());
        let mut valid = tokio::net::TcpStream::connect(addr).await.unwrap();
        valid.write_all(format!("GET /oauth-callback?state=synthetic-state&error=access_denied HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",addr.port()).as_bytes()).await.unwrap();
        let error = tokio::time::timeout(Duration::from_millis(500), waiting)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code, "agy_oauth_cancelled");
    }
    #[tokio::test]
    async fn protocol_restriction_survives_logout() {
        let (_d, a) = store();
        let g = seed(&a).await;
        a.block_for_generation(Restriction::Protocol, &g)
            .await
            .unwrap();
        a.logout().await.unwrap();
        let s = a.status().await.unwrap();
        assert_eq!(s["protocol_blocked"], true);
        assert_eq!(s["authenticated"], false);
    }
    #[tokio::test]
    async fn omp_refresh_window_renews_a_still_valid_token_before_dispatch() {
        use axum::{Json, Router, routing::post};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let requests = Arc::new(AtomicUsize::new(0));
        let count = requests.clone();
        let app=Router::new().route("/token",post(move || {
            let count=count.clone();
            async move {
                count.fetch_add(1,Ordering::SeqCst);
                Json(json!({"access_token":"synthetic-renewed-access","token_type":"Bearer","expires_in":3600}))
            }
        }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let root = tempfile::tempdir().unwrap();
        let auth = AntigravityAuth::synthetic(root.path().join("state"), origin).await;
        for (remaining, expected, calls) in [
            (360, "synthetic-access", 0),
            (240, "synthetic-renewed-access", 1),
        ] {
            let lock = auth
                .lock(Instant::now() + Duration::from_secs(1))
                .await
                .unwrap();
            let mut state = lock.read().unwrap();
            state.session.as_mut().unwrap().expires_at = now() + remaining;
            let before = state.session.as_ref().unwrap().clone();
            lock.write(&state).unwrap();
            drop(lock);
            let token = auth
                .token(Instant::now() + Duration::from_secs(2))
                .await
                .unwrap();
            assert_eq!(token.access_token, expected);
            assert_eq!(requests.load(Ordering::SeqCst), calls);
            let state = auth
                .lock(Instant::now() + Duration::from_secs(1))
                .await
                .unwrap()
                .read()
                .unwrap();
            let after = state.session.unwrap();
            assert_eq!(after.project, before.project);
            assert_eq!(after.refresh_token, before.refresh_token);
        }
        server.abort();
    }

    #[tokio::test]
    async fn refresh_is_serialized_and_survives_caller_cancellation() {
        use axum::{Json, Router, routing::post};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let requests = Arc::new(AtomicUsize::new(0));
        let count = requests.clone();
        let app=Router::new().route("/token",post(move||{let count=count.clone();async move{count.fetch_add(1,Ordering::SeqCst);tokio::time::sleep(Duration::from_millis(100)).await;Json(json!({"access_token":"synthetic-rotated-access","refresh_token":"synthetic-rotated-refresh","token_type":"Bearer","expires_in":3600}))}}));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let d = tempfile::tempdir().unwrap();
        let auth = AntigravityAuth::synthetic(d.path().join("state"), origin).await;
        let lock = auth
            .lock(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        let mut s = lock.read().unwrap();
        let generation = s.generation.clone();
        s.session.as_mut().unwrap().expires_at = 0;
        lock.write(&s).unwrap();
        drop(lock);
        let a = auth.clone();
        let outer =
            tokio::spawn(async move { a.token(Instant::now() + Duration::from_secs(2)).await });
        tokio::time::sleep(Duration::from_millis(25)).await;
        outer.abort();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..6 {
            let a = auth.clone();
            tasks.spawn(async move {
                a.token(Instant::now() + Duration::from_secs(2))
                    .await
                    .unwrap()
            });
        }
        while let Some(t) = tasks.join_next().await {
            let token = t.unwrap();
            assert_eq!(token.access_token, "synthetic-rotated-access");
            assert_eq!(token.generation, generation);
        }
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        let state = auth
            .lock(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap()
            .read()
            .unwrap();
        assert_eq!(
            state.session.unwrap().refresh_token,
            "synthetic-rotated-refresh"
        );
        server.abort();
    }
    #[tokio::test]
    async fn idle_callback_connection_does_not_prevent_valid_consent() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let waiting = tokio::spawn(receive_code(
            listener,
            "synthetic-state",
            Instant::now() + Duration::from_secs(2),
        ));
        let _idle = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut valid = tokio::net::TcpStream::connect(addr).await.unwrap();
        valid.write_all(format!("GET /oauth-callback?state=synthetic-state&code=synthetic-code HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",addr.port()).as_bytes()).await.unwrap();
        assert_eq!(waiting.await.unwrap().unwrap(), "synthetic-code");
    }
    #[tokio::test]
    async fn external_recovery_rotates_local_cancellation_epoch() {
        let (d, a) = store();
        let generation = seed(&a).await;
        let _ = a
            .token(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        let old = a.cancellation();
        let operator = AntigravityAuth::new(d.path().join("state"), test_registration()).unwrap();
        operator
            .block_for_generation(Restriction::Account, &generation)
            .await
            .unwrap();
        assert!(a.availability_error().await.is_some());
        assert!(old.is_cancelled());
        operator.resume(&generation).await.unwrap();
        let token = a
            .token(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        assert_ne!(token.generation, generation);
        assert!(!a.cancellation().is_cancelled());
        assert!(old.is_cancelled());
        operator.logout().await.unwrap();
        let _ = a.generation().await.unwrap();
        assert!(a.cancellation().is_cancelled());
        assert_eq!(
            a.availability_error().await.unwrap().code,
            "agy_auth_required"
        );
    }
    #[tokio::test]
    async fn closure_waits_for_admitted_dispatch_and_rejects_new_admission() {
        let (d, auth) = store();
        let generation = seed(&auth).await;
        let permit = auth
            .dispatch_guard(&generation, Instant::now() + Duration::from_secs(2))
            .await
            .unwrap();
        let operator = AntigravityAuth::new(d.path().join("state"), test_registration()).unwrap();
        let closing = operator.clone();
        let g = generation.clone();
        let barrier =
            tokio::spawn(
                async move { closing.block_for_generation(Restriction::Account, &g).await },
            );
        for _ in 0..100 {
            if operator.availability_error().await.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            operator.availability_error().await.unwrap().code,
            "agy_account_blocked"
        );
        assert!(
            !barrier.is_finished(),
            "closure cannot complete while an admitted dispatch permit is held"
        );
        let queued = AntigravityAuth::new(d.path().join("state"), test_registration()).unwrap();
        assert!(
            queued
                .dispatch_guard(&generation, Instant::now() + Duration::from_millis(200))
                .await
                .is_err()
        );
        drop(permit);
        tokio::time::timeout(Duration::from_secs(1), barrier)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            auth.dispatch_guard(&generation, Instant::now() + Duration::from_secs(1))
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn refresh_redirect_persists_protocol_restriction() {
        use axum::{Router, routing::post};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/token",
                    post(|| async {
                        (
                            axum::http::StatusCode::FOUND,
                            [("location", "http://127.0.0.1:1/never")],
                        )
                    }),
                ),
            )
            .await
            .unwrap()
        });
        let d = tempfile::tempdir().unwrap();
        let auth = AntigravityAuth::synthetic(d.path().join("state"), origin).await;
        let lock = auth
            .lock(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        let mut state = lock.read().unwrap();
        state.session.as_mut().unwrap().expires_at = 0;
        lock.write(&state).unwrap();
        drop(lock);
        assert_eq!(
            auth.token(Instant::now() + Duration::from_secs(2))
                .await
                .err()
                .unwrap()
                .code,
            "agy_protocol_blocked"
        );
        let reopened = AntigravityAuth::new(d.path().join("state"), test_registration()).unwrap();
        assert_eq!(
            reopened.availability_error().await.unwrap().code,
            "agy_protocol_blocked"
        );
        server.abort();
    }
    #[tokio::test]
    async fn fresh_login_control_redirects_persist_protocol_restriction_without_session() {
        use axum::{
            Router,
            routing::{get, post},
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let redirect = || async {
            (
                axum::http::StatusCode::FOUND,
                [("location", "http://127.0.0.1:1/never")],
            )
        };
        let app = Router::new()
            .route("/token", post(redirect))
            .route("/oauth2/v1/userinfo", get(redirect))
            .route("/v1internal:loadCodeAssist", post(redirect));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for endpoint in [
            TOKEN,
            USERINFO,
            "https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist",
        ] {
            let d = tempfile::tempdir().unwrap();
            let mut auth =
                AntigravityAuth::new(d.path().join("state"), test_registration()).unwrap();
            auth.mock_origin = Some(origin.clone());
            let generation = auth.generation().await.unwrap();
            let client = auth.client();
            let request = if endpoint == USERINFO {
                client.get(auth.endpoint(endpoint))
            } else {
                client.post(auth.endpoint(endpoint))
            };
            let error = auth
                .login_request(
                    request,
                    Instant::now() + Duration::from_secs(2),
                    &generation,
                )
                .await
                .err()
                .unwrap();
            assert_eq!(error.code, "agy_protocol_blocked");
            let status = auth.status().await.unwrap();
            assert_eq!(status["protocol_blocked"], true);
            assert_eq!(status["authenticated"], false);
        }
        server.abort();
    }
    #[tokio::test]
    async fn observation_holds_state_lock_until_epoch_is_published() {
        let (d, auth) = store();
        let generation = seed(&auth).await;
        let _ = auth
            .token(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        let lock = auth
            .lock(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        let delayed_snapshot = lock.read().unwrap();
        let operator = AntigravityAuth::new(d.path().join("state"), test_registration()).unwrap();
        let g = generation.clone();
        let writer = tokio::spawn(async move { operator.resume(&g).await });
        let reader_auth = auth.clone();
        let reader = tokio::spawn(async move { reader_auth.generation().await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!writer.is_finished());
        assert!(!reader.is_finished());
        // The delayed old-generation observation must happen while the durable
        // writer is still excluded, never after its new epoch is published.
        auth.observe_state(&delayed_snapshot, &lock);
        drop(lock);
        writer.await.unwrap().unwrap();
        let _ = reader.await.unwrap().unwrap();
        let recovered = auth
            .token(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        assert_ne!(recovered.generation, generation);
        assert!(!auth.cancellation().is_cancelled());
        assert_eq!(auth.generation().await.unwrap(), recovered.generation);
    }
    #[tokio::test]
    async fn slow_refresh_does_not_abort_a_healthy_active_stream() {
        use crate::antigravity_transport::{AntigravityTransport, Operation};
        use axum::{Json, Router, routing::post};
        let started = Arc::new(tokio::sync::Notify::new());
        let signal = started.clone();
        let app=Router::new().route("/token",post(move||{let signal=signal.clone();async move{signal.notify_one();tokio::time::sleep(Duration::from_millis(350)).await;Json(json!({"access_token":"synthetic-refreshed","refresh_token":"synthetic-rotated","token_type":"Bearer","expires_in":3600}))}})).route("/v1internal:streamGenerateContent",post(||async{
            let stream=async_stream::stream!{for _ in 0..250{tokio::time::sleep(Duration::from_millis(2)).await;yield Ok::<_,std::convert::Infallible>("data: synthetic\n\n");}};
            ([("content-type","text/event-stream")],axum::body::Body::from_stream(stream))
        }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let d = tempfile::tempdir().unwrap();
        let auth = AntigravityAuth::synthetic(d.path().join("state"), origin).await;
        let token = auth
            .token(Instant::now() + Duration::from_secs(3))
            .await
            .unwrap();
        let mut response = AntigravityTransport::new(auth.clone())
            .post_json(
                Operation::Generate,
                &json!({"model":"gemini-4-argon-medium"}),
                &token,
                Instant::now() + Duration::from_secs(3),
            )
            .await
            .unwrap();
        assert!(response.chunk().await.unwrap().is_some());
        let lock = auth
            .lock(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        let mut state = lock.read().unwrap();
        state.session.as_mut().unwrap().expires_at = 0;
        lock.write(&state).unwrap();
        drop(lock);
        let refresh_auth = auth.clone();
        let refresh = tokio::spawn(async move {
            refresh_auth
                .token(Instant::now() + Duration::from_secs(2))
                .await
        });
        started.notified().await;
        assert!(matches!(
            auth.try_validate_generation(
                &token.generation,
                Instant::now() + Duration::from_secs(1)
            )
            .unwrap(),
            GenerationValidation::Busy
        ));
        let mut chunks = 0;
        while response.chunk().await.unwrap().is_some() {
            chunks += 1;
        }
        assert!(chunks > 0);
        assert_eq!(
            refresh.await.unwrap().unwrap().access_token,
            "synthetic-refreshed"
        );
        server.abort();
    }
    #[test]
    fn public_registration_decodes_and_matches_reviewed_fingerprints() {
        let config = reviewed_registration().expect("reviewed public registration must decode");
        assert!(config.validate().is_ok());
        assert!(registration_matches_baseline(&config, &compiled_baseline()));
        assert!(decoded_registration("invalid base64", REVIEWED_CLIENT_SECRET_BASE64).is_none());
        assert!(decoded_registration(REVIEWED_CLIENT_ID_BASE64, "invalid base64").is_none());
        assert!(decoded_registration("/w==", REVIEWED_CLIENT_SECRET_BASE64).is_none());
        assert!(decoded_registration("", REVIEWED_CLIENT_SECRET_BASE64).is_none());
        let mut changed = compiled_baseline();
        changed["authentication"]["registration"]["client_id_sha256"] = json!("0".repeat(64));
        assert!(!baseline_document_valid(&changed.to_string()));
        changed = compiled_baseline();
        changed["authentication"]["registration"]["client_secret_sha256"] = Value::Null;
        assert!(!baseline_document_valid(&changed.to_string()));
    }
    #[tokio::test]
    async fn changed_registration_blocks_unexpired_token_dispatch_and_readiness() {
        let (_d, auth) = store();
        let generation = seed(&auth).await;
        let mut other = auth.clone();
        other.config.client_id = "another-synthetic-client".into();
        let status = other.status().await.unwrap();
        assert_eq!(status["authenticated"], true);
        assert_eq!(status["available"], false);
        assert_eq!(status["session_registration_matches"], false);
        assert_eq!(status["bundled_registration_valid"], true);
        assert_eq!(status["registration_source"], "operator_configured");
        assert_eq!(
            other.availability_error().await.unwrap().code,
            "agy_oauth_registration_changed"
        );
        assert_eq!(
            other
                .token(Instant::now() + Duration::from_secs(1))
                .await
                .err()
                .unwrap()
                .code,
            "agy_oauth_registration_changed"
        );
        assert_eq!(
            other
                .dispatch_guard(&generation, Instant::now() + Duration::from_secs(1))
                .await
                .err()
                .unwrap()
                .code,
            "agy_oauth_registration_changed"
        );
        let status = auth.status().await.unwrap();
        assert_eq!(status["session_registration_matches"], true);
        assert_eq!(status["available"], true);
        assert_eq!(
            auth.token(Instant::now() + Duration::from_secs(1))
                .await
                .unwrap()
                .access_token,
            "synthetic-access"
        );
    }

    #[test]
    fn compiled_baseline_matches_dispatch_constants_and_rejects_drift() {
        let document = include_str!("../docs/antigravity-baseline.json");
        assert!(baseline_valid());
        let value: Value = serde_json::from_str(document).unwrap();
        for (field, op) in [
            (
                "inference_path",
                crate::antigravity_transport::Operation::Generate,
            ),
            (
                "model_path",
                crate::antigravity_transport::Operation::Models,
            ),
            ("quota_path", crate::antigravity_transport::Operation::Quota),
            (
                "project_path",
                crate::antigravity_transport::Operation::LoadCodeAssist,
            ),
        ] {
            assert_eq!(value["destinations"][field], op.path());
        }
        for pointer in [
            "/schema_version",
            "/adapter_revision",
            "/identity/user_agent",
            "/identity/version",
            "/authentication/refresh_skew_seconds",
            "/source/commit",
            "/destinations/inference_origin",
            "/destinations/inference_path",
            "/destinations/model_path",
            "/destinations/quota_path",
            "/destinations/project_path",
            "/destinations/token_url",
            "/identity/automatic_identity_updates",
        ] {
            let mut changed = value.clone();
            *changed.pointer_mut(pointer).unwrap() = Value::Null;
            assert!(!baseline_document_valid(&changed.to_string()), "{pointer}");
        }
        assert!(!baseline_document_valid("not json"));
    }
    #[test]
    fn provisioning_is_never_automatic() {
        assert_eq!(
            existing_project(&json!({"allowedTiers":[{"id":"free-tier"}]}))
                .unwrap_err()
                .code,
            "agy_onboarding_required"
        );
        assert_eq!(
            existing_project(
                &json!({"currentTier":{"id":"free-tier"},"cloudaicompanionProject":"synthetic"})
            )
            .unwrap(),
            "synthetic"
        );
    }
    #[test]
    fn refresh_rotation_keeps_old_refresh_when_omitted() {
        let mut s = Session {
            client_id: "c".into(),
            access_token: "old".into(),
            refresh_token: "refresh".into(),
            expires_at: 0,
            subject: "u".into(),
            project: "p".into(),
        };
        update_tokens(
            &mut s,
            &json!({"access_token":"new","token_type":"Bearer","expires_in":3600}),
            false,
        )
        .unwrap();
        assert_eq!(s.refresh_token, "refresh");
        assert!(update_tokens(&mut s,&json!({"access_token":"new","token_type":"Bearer","expires_in":3600,"scope":"unrelated"}),false).is_err());
    }
}
