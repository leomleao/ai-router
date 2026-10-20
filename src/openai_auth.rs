//! Operator-owned ChatGPT plan authorization. Never logs token records or HTTP bodies.
//! All disk/network access is lazy; tests use only isolated synthetic state.
use crate::protocol::ApiError;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    ffi::CString,
    fs::File,
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const ISSUER: &str = "https://auth.openai.com";
const DISCOVERY: &str = "https://auth.openai.com/.well-known/openid-configuration";
const AUTHORIZE: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN: &str = "https://auth.openai.com/api/accounts/oauth/token";
const RESOURCE: &str = "https://api.openai.com/v1";
const DIRECT_SCOPE: &str = "chatgpt.tokens.use.direct";
const REQUESTED_SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const USAGE_URL: &str = "https://chatgpt.com/settings/usage";
const MAX_RECORD: usize = 2 * 1024 * 1024;
const MAX_HTTP: usize = 512 * 1024;

/// Deliberately does not implement Debug: it contains a bearer credential.
pub struct AccessToken {
    pub access_token: String,
    pub generation: String,
}

#[derive(Clone)]
pub struct AuthStore {
    directory: PathBuf,
    #[cfg(test)]
    mock: Option<MockEndpoints>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    host_id: String,
    generation: String,
    selected: Option<String>,
    registrations: Vec<Registration>,
    #[serde(default)]
    pending_registration: Option<PendingRegistration>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingRegistration {
    id: String,
    client_id: String,
}
impl Default for State {
    fn default() -> Self {
        Self {
            version: 1,
            host_id: format!("urn:uuid:{}", uuid::Uuid::new_v4()),
            generation: new_generation(),
            selected: None,
            registrations: Vec::new(),
            pending_registration: None,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    id: String,
    client_id: String,
    subject: String,
    email: Option<String>,
    tokens: Option<Tokens>,
    #[serde(default)]
    block: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_at: u64,
    scopes: Vec<String>,
}
#[derive(Deserialize)]
struct TokenReply {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: u64,
    token_type: String,
    scope: Option<String>,
}
#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    revocation_endpoint: Option<String>,
}
#[derive(Clone, Deserialize)]
struct Identity {
    sub: String,
    iat: u64,
    aud: serde_json::Value,
    #[serde(default)]
    azp: Option<String>,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    email: Option<String>,
}
struct OAuthFailure {
    terminal: bool,
    message: &'static str,
}

impl AuthStore {
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            #[cfg(test)]
            mock: None,
        }
    }

    /// Serializes refresh and every state mutation across processes. Rotating a
    /// refresh token never changes the registration generation used by routing.
    pub async fn token(&self) -> Result<AccessToken, ApiError> {
        self.renew(false).await
    }

    async fn renew(&self, ignore_block: bool) -> Result<AccessToken, ApiError> {
        // A caller timeout/disconnect must not cancel a rotating refresh between
        // upstream success and durable replacement. Dropping a JoinHandle leaves
        // its bounded task running; the cross-process lock still serializes it.
        let store = self.clone();
        tokio::spawn(async move { store.token_inner(ignore_block).await })
            .await
            .map_err(|_| ApiError::unavailable("OpenAI authorization renewal could not complete"))?
    }

    async fn token_inner(&self, ignore_block: bool) -> Result<AccessToken, ApiError> {
        let locked = self.lock().await?;
        let mut state = locked.read()?;
        let index = selected_index(&state)?;
        if !ignore_block {
            if let Some(code) = &state.registrations[index].block {
                return Err(block_error(code));
            }
        }
        let current = state.registrations[index]
            .tokens
            .clone()
            .ok_or_else(auth_required)?;
        require_scopes(&current.scopes)?;
        if current.expires_at > now().saturating_add(90) {
            return Ok(AccessToken {
                access_token: current.access_token,
                generation: state.generation,
            });
        }
        let refresh = current.refresh_token.as_deref().ok_or_else(auth_required)?;
        let client_id = state.registrations[index].client_id.clone();
        let reply = self
            .exchange(&[
                ("grant_type", "refresh_token"),
                ("client_id", &client_id),
                ("refresh_token", refresh),
                ("resource", RESOURCE),
            ])
            .await;
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) => {
                if error.terminal {
                    state.registrations[index].tokens = None;
                    state.registrations[index].block = None;
                    state.generation = new_generation();
                    locked.write(&state)?;
                    return Err(auth_required());
                }
                return Err(ApiError::unavailable(error.message));
            }
        };
        let tokens = match self
            .validate_reply(
                reply,
                Some(&current),
                &client_id,
                None,
                Some(&state.registrations[index].subject),
                false,
            )
            .await
        {
            Ok(tokens) => tokens,
            Err(error) => {
                if matches!(
                    error.code.as_str(),
                    "openai_auth_required" | "openai_identity_invalid" | "openai_identity_mismatch"
                ) {
                    state.registrations[index].tokens = None;
                    state.registrations[index].block = None;
                    state.generation = new_generation();
                    locked.write(&state)?;
                }
                return Err(error);
            }
        };
        state.registrations[index].tokens = Some(tokens.clone());
        locked.write(&state)?;
        Ok(AccessToken {
            access_token: tokens.access_token,
            generation: state.generation,
        })
    }

    /// Reads the current session generation without renewing it. Dispatchers use
    /// this immediately before sending a request and to cancel explicit logout.
    pub async fn generation(&self) -> Result<String, ApiError> {
        let state = self.lock().await?.read()?;
        Ok(state.generation)
    }

    /// Durable plan/eligibility restriction. A successful catalogue refresh does
    /// not clear it; only an operator action may do so.
    pub async fn block(&self, code: &str) -> Result<(), ApiError> {
        self.persist_block(code, None).await
    }

    /// Bind failures to the session that dispatched the request, so a delayed
    /// error cannot restrict an account selected after that request began.
    pub async fn block_for_generation(&self, code: &str, generation: &str) -> Result<(), ApiError> {
        self.persist_block(code, Some(generation)).await
    }

    async fn persist_block(&self, code: &str, generation: Option<&str>) -> Result<(), ApiError> {
        // A request deadline may expire while rotating refresh owns the state
        // lock. Preserve the restriction after the caller disconnects, with the
        // generation check performed only after this owned task acquires flock.
        let store = self.clone();
        let code = code.to_owned();
        let generation = generation.map(str::to_owned);
        tokio::spawn(async move { store.block_inner(&code, generation.as_deref()).await })
            .await
            .map_err(|_| {
                ApiError::unavailable("OpenAI availability restriction could not be persisted")
            })?
    }

    async fn block_inner(&self, code: &str, generation: Option<&str>) -> Result<(), ApiError> {
        let code = match code {
            "subscription_sharing_usage_limit_exceeded" | "usage_blocked" => "usage_blocked",
            "user_not_eligible" | "subscription_sharing_user_not_eligible" | "ineligible" => {
                "ineligible"
            }
            "auth_required"
            | "openai_auth_required"
            | "subscription_sharing_invalid_user"
            | "chatpass_v2_scope_not_authorized"
            | "chatpass_v2_invalid_authorization_context" => "auth_required",
            _ => {
                return Err(ApiError::bad_request(
                    "Unknown OpenAI availability restriction",
                ));
            }
        };
        let locked = self.lock().await?;
        let mut state = locked.read()?;
        if generation.is_some_and(|expected| expected != state.generation) {
            return Ok(());
        }
        let index = selected_index(&state)?;
        state.registrations[index].block = Some(code.to_owned());
        locked.write(&state)
    }

    pub async fn availability_error(&self) -> Option<ApiError> {
        let locked = match self.lock().await {
            Ok(lock) => lock,
            Err(error) => return Some(error),
        };
        let state = match locked.read() {
            Ok(state) => state,
            Err(error) => return Some(error),
        };
        let index = match selected_index(&state) {
            Ok(index) => index,
            Err(error) => return Some(error),
        };
        let registration = &state.registrations[index];
        if let Some(code) = &registration.block {
            return Some(block_error(code));
        }
        match registration.tokens.as_ref() {
            Some(tokens) => require_scopes(&tokens.scopes).err(),
            None => Some(auth_required()),
        }
    }

    async fn lock(&self) -> Result<LockedState, ApiError> {
        let directory = private_directory(&self.directory)?;
        let lock = open_private(&directory, ".session-lock", true)?;
        let started = tokio::time::Instant::now();
        loop {
            // LOCK_NB avoids blocking a Tokio executor thread while another
            // process owns rotating-session state.
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(LockedState {
                    directory,
                    _lock: lock,
                });
            }
            let error = std::io::Error::last_os_error();
            if !matches!(error.raw_os_error(), Some(libc::EWOULDBLOCK)) {
                return Err(storage_error());
            }
            if started.elapsed() >= Duration::from_secs(20) {
                return Err(ApiError::unavailable(
                    "OpenAI authorization state is busy; retry shortly",
                ));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn client(&self) -> Result<reqwest::Client, ApiError> {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .pool_max_idle_per_host(2)
            .build()
            .map_err(|_| {
                ApiError::unavailable("Unable to initialize OpenAI authorization transport")
            })
    }
    fn endpoint(&self, endpoint: &str) -> String {
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            return match endpoint {
                DISCOVERY => format!("{}/discovery", mock.base),
                TOKEN => format!("{}/token", mock.base),
                other => other.to_owned(),
            };
        }
        endpoint.to_owned()
    }
    async fn discovery(&self) -> Result<Discovery, ApiError> {
        let response = self
            .client()?
            .get(self.endpoint(DISCOVERY))
            .send()
            .await
            .map_err(|_| ApiError::unavailable("OpenAI identity discovery failed"))?;
        let discovery: Discovery = read_json(response).await?;
        let official = discovery.issuer == ISSUER
            && discovery.authorization_endpoint == AUTHORIZE
            && discovery.token_endpoint == TOKEN
            && official_auth_url(&discovery.jwks_uri)
            && discovery
                .revocation_endpoint
                .as_ref()
                .is_none_or(|url| official_auth_url(url));
        #[cfg(test)]
        let official = official
            || self.mock.as_ref().is_some_and(|mock| {
                discovery.issuer == ISSUER
                    && discovery.authorization_endpoint == AUTHORIZE
                    && discovery.token_endpoint == TOKEN
                    && discovery.jwks_uri == format!("{}/jwks", mock.base)
                    && discovery
                        .revocation_endpoint
                        .as_ref()
                        .is_none_or(|url| url == &format!("{}/revoke", mock.base))
            });
        if !official {
            return Err(ApiError::unavailable(
                "OpenAI identity discovery contained untrusted endpoints",
            ));
        }
        Ok(discovery)
    }
    async fn exchange(&self, form: &[(&str, &str)]) -> Result<TokenReply, OAuthFailure> {
        let client = self.client().map_err(|_| OAuthFailure {
            terminal: false,
            message: "OpenAI authorization transport unavailable",
        })?;
        let response = client
            .post(self.endpoint(TOKEN))
            .form(form)
            .send()
            .await
            .map_err(|_| OAuthFailure {
                terminal: false,
                message: "OpenAI token request failed; saved session retained",
            })?;
        let status = response.status();
        let bytes = bounded_body(response, MAX_HTTP)
            .await
            .map_err(|_| OAuthFailure {
                terminal: false,
                message: "OpenAI token response was invalid; saved session retained",
            })?;
        if !status.is_success() {
            let error = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|value| {
                    value
                        .get("error")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned)
                });
            let terminal = status.is_client_error()
                && matches!(
                    error.as_deref(),
                    Some(
                        "invalid_grant"
                            | "invalid_refresh_token"
                            | "token_expired"
                            | "refresh_token_expired"
                            | "refresh_token_invalidated"
                            | "refresh_token_reused"
                    )
                );
            return Err(OAuthFailure {
                terminal,
                message: if matches!(
                    error.as_deref(),
                    Some("invalid_client" | "unauthorized_client")
                ) {
                    "OpenAI token client configuration was rejected; fix the client configuration; saved session retained"
                } else {
                    "OpenAI token request failed; authorize again if the grant was revoked"
                },
            });
        }
        serde_json::from_slice(&bytes).map_err(|_| OAuthFailure {
            terminal: false,
            message: "OpenAI token response was invalid; saved session retained",
        })
    }
    async fn identity(
        &self,
        token: &str,
        client_id: &str,
        nonce: Option<&str>,
    ) -> Result<Identity, ApiError> {
        let discovery = self.discovery().await?;
        let response = self
            .client()?
            .get(discovery.jwks_uri)
            .send()
            .await
            .map_err(|_| ApiError::unavailable("OpenAI signing keys unavailable"))?;
        let keys: JwkSet = read_json(response).await?;
        validate_identity(token, client_id, nonce, &keys)
    }
    async fn validate_reply(
        &self,
        reply: TokenReply,
        previous: Option<&Tokens>,
        client_id: &str,
        nonce: Option<&str>,
        subject: Option<&str>,
        allow_missing_plan_scope: bool,
    ) -> Result<Tokens, ApiError> {
        if !reply.token_type.eq_ignore_ascii_case("Bearer")
            || reply.access_token.is_empty()
            || reply.access_token.len() > 64 * 1024
            || reply.access_token.chars().any(char::is_control)
            || reply.expires_in == 0
            || reply.expires_in > 7 * 24 * 3600
            || reply
                .refresh_token
                .as_ref()
                .is_some_and(|v| v.is_empty() || v.len() > 64 * 1024)
        {
            return Err(ApiError::unavailable(
                "OpenAI returned an invalid token record",
            ));
        }
        let scopes = match reply.scope.as_deref() {
            Some(scope) => scope.split_whitespace().map(str::to_owned).collect(),
            None => previous
                .map(|record| record.scopes.clone())
                .unwrap_or_default(),
        };
        if !allow_missing_plan_scope {
            require_scopes(&scopes)?;
        }
        if let Some(id_token) = &reply.id_token {
            let identity = self.identity(id_token, client_id, nonce).await?;
            if subject.is_some_and(|expected| expected != identity.sub) {
                return Err(ApiError::new(
                    403,
                    "openai_identity_mismatch",
                    "OpenAI returned a different account identity",
                ));
            }
        } else if nonce.is_some() {
            return Err(ApiError::unavailable(
                "OpenAI login did not return an ID token",
            ));
        }
        Ok(Tokens {
            access_token: reply.access_token,
            refresh_token: reply
                .refresh_token
                .or_else(|| previous.and_then(|p| p.refresh_token.clone())),
            id_token: reply
                .id_token
                .or_else(|| previous.and_then(|p| p.id_token.clone())),
            expires_at: now().saturating_add(reply.expires_in),
            scopes,
        })
    }

    async fn login(&self, args: &[String]) -> Result<(), String> {
        let options = LoginOptions::parse(args)?;
        let (host, initial_generation, returning, pending) = {
            let locked = self.lock().await.map_err(display_error)?;
            let state = locked.read().map_err(display_error)?;
            let chosen = if options.new {
                None
            } else {
                options.account.as_ref().or(state.selected.as_ref())
            };
            let returning = if let Some(id) = chosen {
                Some(
                    state
                        .registrations
                        .iter()
                        .find(|r| &r.id == id)
                        .cloned()
                        .ok_or_else(|| "Unknown saved OpenAI account ID".to_owned())?,
                )
            } else {
                None
            };
            locked.write(&state).map_err(display_error)?;
            let pending = if returning.is_none() {
                state.pending_registration.clone()
            } else {
                None
            };
            (state.host_id, state.generation, returning, pending)
        };
        let listener = TcpListener::bind((options.listen, options.port))
            .await
            .map_err(|_| "Unable to bind the OpenAI callback listener".to_owned())?;
        let port = listener
            .local_addr()
            .map_err(|_| "Unable to inspect callback listener".to_owned())?
            .port();
        let redirect = format!("http://127.0.0.1:{port}/auth/callback");
        let transaction = Transaction::new(redirect);
        // Verify metadata before asking the operator to authorize anything.
        self.discovery().await.map_err(display_error)?;
        let client = returning
            .as_ref()
            .map(|r| r.client_id.as_str())
            .or_else(|| pending.as_ref().map(|r| r.client_id.as_str()))
            .unwrap_or("dynamic_agent_client");
        let request_consent = returning
            .as_ref()
            .and_then(|registration| registration.tokens.as_ref())
            .is_some_and(|tokens| require_scopes(&tokens.scopes).is_err());
        let authorization_url = transaction.authorization_url(client, &host, request_consent);
        println!(
            "Continue with ChatGPT\n{authorization_url}\n\nThis uses your shared ChatGPT plan allowance; the router adds no allowance. Manage usage: {USAGE_URL}\nWaiting up to 10 minutes for the browser callback."
        );
        open_browser(&authorization_url);
        let callback = tokio::time::timeout(
            Duration::from_secs(600),
            receive_callback(&listener, &transaction),
        )
        .await
        .map_err(|_| "OpenAI login timed out; start a fresh login".to_owned())??;
        let client_id = if client != "dynamic_agent_client" {
            if callback.client_id.as_deref().is_some_and(|id| id != client) {
                return Err(
                    "OpenAI callback changed the pending registration's client ID".to_owned(),
                );
            }
            client.to_owned()
        } else {
            callback
                .client_id
                .filter(|id| !id.is_empty() && id != "dynamic_agent_client")
                .ok_or_else(|| {
                    "OpenAI registration callback did not supply an issued client ID".to_owned()
                })?
        };
        let registration_id = returning
            .as_ref()
            .map(|r| r.id.clone())
            .or_else(|| pending.as_ref().map(|r| r.id.clone()))
            .unwrap_or_else(new_generation);
        // Browser interaction never holds the state lock. Code exchange and its
        // token commit do, preventing reauthorization from racing a rotating
        // refresh in the server or another helper process.
        let locked = self.lock().await.map_err(display_error)?;
        let mut state = locked.read().map_err(display_error)?;
        if state.host_id != host || state.generation != initial_generation {
            return Err("OpenAI state changed during login; retry login".to_owned());
        }
        if returning.is_none() {
            // Preserve the issued registration even if code redemption expires or
            // fails transiently. The next login --new reuses it with fresh PKCE;
            // no unverified account identity or token becomes active here.
            if state
                .pending_registration
                .as_ref()
                .is_some_and(|existing| existing.client_id != client_id)
            {
                return Err("Another OpenAI registration is pending; retry login --new".to_owned());
            }
            state.pending_registration = Some(PendingRegistration {
                id: registration_id.clone(),
                client_id: client_id.clone(),
            });
            locked.write(&state).map_err(display_error)?;
        }
        let reply = self
            .exchange(&[
                ("grant_type", "authorization_code"),
                ("client_id", &client_id),
                ("code", &callback.code),
                ("code_verifier", &transaction.verifier),
                ("redirect_uri", &transaction.redirect),
                ("resource", RESOURCE),
            ])
            .await
            .map_err(|error| error.message.to_owned())?;
        let identity = self
            .identity(
                reply
                    .id_token
                    .as_deref()
                    .ok_or_else(|| "OpenAI login did not return an ID token".to_owned())?,
                &client_id,
                Some(&transaction.nonce),
            )
            .await
            .map_err(display_error)?;
        if returning
            .as_ref()
            .is_some_and(|registration| registration.subject != identity.sub)
        {
            return Err(
                "OpenAI login returned a different account; the saved session was not changed"
                    .to_owned(),
            );
        }
        let tokens = self
            .validate_reply(
                reply,
                None,
                &client_id,
                Some(&transaction.nonce),
                Some(&identity.sub),
                true,
            )
            .await
            .map_err(display_error)?;
        let id = registration_id;
        // A client registration belongs to one identity, even when email matches.
        if state
            .registrations
            .iter()
            .any(|r| r.client_id == client_id && (r.id != id || r.subject != identity.sub))
        {
            return Err(
                "OpenAI client registration is already associated with another saved account"
                    .to_owned(),
            );
        }
        let plan_enabled = require_scopes(&tokens.scopes).is_ok();
        let registration = Registration {
            id: id.clone(),
            client_id,
            subject: identity.sub,
            email: identity.email,
            tokens: Some(tokens),
            block: None,
        };
        if let Some(index) = state.registrations.iter().position(|r| r.id == id) {
            state.registrations[index] = registration;
        } else {
            state.registrations.push(registration);
        }
        if state
            .pending_registration
            .as_ref()
            .is_some_and(|pending| pending.id == id)
        {
            state.pending_registration = None;
        }
        state.selected = Some(id.clone());
        state.generation = new_generation();
        locked.write(&state).map_err(display_error)?;
        if plan_enabled {
            println!(
                "OpenAI account {id} authorized and selected. Eligibility and models will be checked by the router. Manage usage: {USAGE_URL}"
            );
        } else {
            println!(
                "OpenAI account {id} signed in and selected; ChatGPT plan use is disabled. Run ai-router openai login --account {id} to request plan-use consent. Manage usage: {USAGE_URL}"
            );
        }
        Ok(())
    }

    async fn logout(&self) -> Result<(), String> {
        let registration = {
            let locked = self.lock().await.map_err(display_error)?;
            let mut state = locked.read().map_err(display_error)?;
            let index = selected_index(&state).map_err(display_error)?;
            let registration = state.registrations[index].clone();
            // Immediately stop queued/new requests and notify generation watchers.
            // Keep the renewable session only in memory for the bounded revoke
            // attempt, outside the file lock, so cancellation never waits on HTTP.
            state.registrations[index].tokens = None;
            state.registrations[index].block = None;
            state.generation = new_generation();
            locked.write(&state).map_err(display_error)?;
            registration
        };
        let confirmed = if let Some(refresh) = registration
            .tokens
            .as_ref()
            .and_then(|t| t.refresh_token.as_ref())
        {
            self.revoke(&registration.client_id, refresh).await
        } else {
            true
        };
        if confirmed {
            println!("OpenAI signed out. Saved registration and host identity retained.");
            Ok(())
        } else {
            Err("OpenAI signed out locally; remote revocation was not confirmed. Disconnect the app in ChatGPT Settings to end the remote session.".to_owned())
        }
    }
    async fn revoke(&self, client_id: &str, refresh: &str) -> bool {
        let Ok(discovery) = self.discovery().await else {
            return false;
        };
        let Some(endpoint) = discovery.revocation_endpoint else {
            return false;
        };
        let Ok(client) = self.client() else {
            return false;
        };
        // One bounded retry only. No token content or upstream error body escapes.
        for attempt in 0..2 {
            match client
                .post(&endpoint)
                .form(&[
                    ("token", refresh),
                    ("token_type_hint", "refresh_token"),
                    ("client_id", client_id),
                ])
                .send()
                .await
            {
                Ok(response) if response.status() == reqwest::StatusCode::OK => return true,
                Ok(response) if !response.status().is_server_error() => return false,
                _ => {}
            }
            if attempt == 0 {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
        false
    }
}

/// Operator commands intentionally read no application dotenv or API-key files.
pub async fn operator(args: &[String]) -> Result<(), String> {
    let directory = std::env::var_os("AI_ROUTER_OPENAI_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/ai-router/openai-auth"));
    let store = AuthStore::new(directory);
    let Some(command) = args.first().map(String::as_str) else {
        return Err(operator_usage());
    };
    match command {
        "login" => store.login(&args[1..]).await,
        "logout" if args.len() == 1 => store.logout().await,
        "status" | "accounts" if args.len() == 1 => {
            let state = store
                .lock()
                .await
                .map_err(display_error)?
                .read()
                .map_err(display_error)?;
            println!(
                "OpenAI ChatGPT plan accounts (session readiness does not guarantee inference allowance):"
            );
            for registration in &state.registrations {
                // Only labels and booleans; never bearer/refresh/ID tokens, OAuth
                // URLs, verified subjects, or account email in routine diagnostics.
                println!(
                    "{} {} session={}{}",
                    registration.id,
                    if state.selected.as_ref() == Some(&registration.id) {
                        "selected"
                    } else {
                        "saved"
                    },
                    match registration.tokens.as_ref() {
                        Some(tokens) if require_scopes(&tokens.scopes).is_ok() => "authorized",
                        Some(_) => "plan-use-disabled",
                        None => "login-required",
                    },
                    if registration
                        .tokens
                        .as_ref()
                        .is_some_and(|t| t.expires_at <= now().saturating_add(90))
                    {
                        " renewal-due"
                    } else {
                        ""
                    }
                );
            }
            if state.registrations.is_empty() {
                println!("No accounts; run openai login.");
            }
            if state.pending_registration.is_some() {
                println!(
                    "An incomplete registration is saved; run openai login --new to authorize it."
                );
            }
            println!("Manage shared plan usage: {USAGE_URL}");
            Ok(())
        }
        "select" if args.len() == 2 => {
            let locked = store.lock().await.map_err(display_error)?;
            let mut state = locked.read().map_err(display_error)?;
            if !state.registrations.iter().any(|r| r.id == args[1]) {
                return Err("Unknown saved OpenAI account ID".to_owned());
            }
            let index = state
                .registrations
                .iter()
                .position(|r| r.id == args[1])
                .expect("verified registration");
            state.registrations[index].block = None;
            state.selected = Some(args[1].clone());
            state.generation = new_generation();
            locked.write(&state).map_err(display_error)?;
            println!(
                "OpenAI account {} selected; the router will revalidate availability.",
                args[1]
            );
            Ok(())
        }
        "resume" if args.len() == 1 => {
            // Ensure the current grant remains usable before clearing the
            // provider's quota latch via a durable generation change.
            let resumed = store.renew(true).await.map_err(display_error)?;
            let locked = store.lock().await.map_err(display_error)?;
            let mut state = locked.read().map_err(display_error)?;
            let index = selected_index(&state).map_err(display_error)?;
            if state.generation != resumed.generation {
                return Err("OpenAI account changed during resume; retry".to_owned());
            }
            state.registrations[index].block = None;
            state.generation = new_generation();
            locked.write(&state).map_err(display_error)?;
            println!(
                "OpenAI availability will be rechecked; the next inference may reach the plan limit again. Manage usage: {USAGE_URL}"
            );
            Ok(())
        }
        _ => Err(operator_usage()),
    }
}
fn operator_usage() -> String {
    "Usage: ai-router openai login [--new | --account ID] [--port N] [--listen IP] | status | accounts | select ID | logout | resume".to_owned()
}
fn display_error(error: ApiError) -> String {
    error.to_string()
}
fn new_generation() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn block_error(code: &str) -> ApiError {
    match code {
        "usage_blocked" => ApiError::new(
            429,
            "openai_usage_blocked",
            "ChatGPT plan usage is blocked; review ChatGPT Usage and run ai-router openai resume",
        ),
        "ineligible" => ApiError::new(
            403,
            "openai_ineligible",
            "This ChatGPT registration is not eligible for plan inference; correct authorization and run ai-router openai resume",
        ),
        _ => auth_required(),
    }
}
fn auth_required() -> ApiError {
    ApiError::new(
        503,
        "openai_auth_required",
        "OpenAI ChatGPT authorization required; run ai-router openai login",
    )
}
fn storage_error() -> ApiError {
    ApiError::unavailable(
        "OpenAI authorization storage is unsafe or unavailable; use a private owner-only state directory",
    )
}
fn selected_index(state: &State) -> Result<usize, ApiError> {
    state
        .selected
        .as_ref()
        .and_then(|id| state.registrations.iter().position(|r| &r.id == id))
        .ok_or_else(auth_required)
}
fn require_scopes(scopes: &[String]) -> Result<(), ApiError> {
    if ![DIRECT_SCOPE, "resource.invoke"]
        .iter()
        .all(|required| scopes.iter().any(|scope| scope == required))
    {
        return Err(ApiError::new(
            503,
            "openai_auth_required",
            "OpenAI authorization lacks ChatGPT plan-use permission; authorize again",
        ));
    }
    Ok(())
}
fn official_auth_url(value: &str) -> bool {
    reqwest::Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("auth.openai.com")
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
    })
}
async fn bounded_body(
    mut response: reqwest::Response,
    maximum: usize,
) -> Result<Vec<u8>, ApiError> {
    if response
        .content_length()
        .is_some_and(|size| size > maximum as u64)
    {
        return Err(ApiError::unavailable(
            "OpenAI authorization response exceeded the size limit",
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ApiError::unavailable("OpenAI authorization response interrupted"))?
    {
        if bytes.len().saturating_add(chunk.len()) > maximum {
            return Err(ApiError::unavailable(
                "OpenAI authorization response exceeded the size limit",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
async fn read_json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, ApiError> {
    if !response.status().is_success() {
        return Err(ApiError::unavailable(
            "OpenAI identity service returned an error",
        ));
    }
    serde_json::from_slice(&bounded_body(response, MAX_HTTP).await?)
        .map_err(|_| ApiError::unavailable("OpenAI identity service returned invalid data"))
}
fn validate_identity(
    token: &str,
    client_id: &str,
    nonce: Option<&str>,
    keys: &JwkSet,
) -> Result<Identity, ApiError> {
    let failure = || {
        ApiError::new(
            503,
            "openai_identity_invalid",
            "OpenAI ID token could not be verified",
        )
    };
    let header = decode_header(token).map_err(|_| failure())?;
    if !matches!(header.alg, Algorithm::RS256 | Algorithm::ES256) {
        return Err(failure());
    }
    let key = keys
        .find(header.kid.as_deref().ok_or_else(failure)?)
        .ok_or_else(failure)?;
    let decoding_key = DecodingKey::from_jwk(key).map_err(|_| failure())?;
    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[ISSUER]);
    validation.set_audience(&[client_id]);
    validation.set_required_spec_claims(&["iss", "aud", "sub", "exp", "iat"]);
    validation.leeway = 5;
    validation.validate_nbf = true;
    let identity = decode::<Identity>(token, &decoding_key, &validation)
        .map_err(|_| failure())?
        .claims;
    if identity.sub.is_empty()
        || identity.sub.len() > 1024
        || identity.iat > now().saturating_add(5)
        || identity
            .azp
            .as_ref()
            .is_some_and(|authorized| authorized != client_id)
        || (identity
            .aud
            .as_array()
            .is_some_and(|audiences| audiences.len() > 1)
            && identity.azp.as_deref() != Some(client_id))
        || nonce.is_some_and(|expected| identity.nonce.as_deref() != Some(expected))
    {
        return Err(failure());
    }
    Ok(identity)
}

// openat walks every component with O_NOFOLLOW, so neither ancestor aliases nor
// final-component symlinks can redirect runtime token access. The returned fd
// also pins the directory through atomic replacement and locking.
fn private_directory(path: &Path) -> Result<File, ApiError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| storage_error())?
            .join(path)
    };
    let mut components = Vec::new();
    for component in absolute.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                let text = name.to_str().ok_or_else(storage_error)?;
                if forbidden_component(text) {
                    return Err(storage_error());
                }
                components.push(name.to_owned());
            }
            _ => return Err(storage_error()),
        }
    }
    if components.is_empty() {
        return Err(storage_error());
    }
    let root = CString::new("/").expect("literal");
    let fd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(storage_error());
    }
    let mut directory = unsafe { File::from_raw_fd(fd) };
    for (index, component) in components.iter().enumerate() {
        let name = CString::new(component.as_bytes()).map_err(|_| storage_error())?;
        let last = index == components.len() - 1;
        let mut next = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if next < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) } < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(storage_error());
            }
            next = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
        }
        if next < 0 {
            return Err(storage_error());
        }
        directory = unsafe { File::from_raw_fd(next) };
        let metadata = directory.metadata().map_err(|_| storage_error())?;
        if last && (metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0) {
            return Err(storage_error());
        }
    }
    Ok(directory)
}
fn forbidden_component(name: &str) -> bool {
    name.starts_with(".env")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || matches!(name, "secrets" | "credentials")
}
fn open_private(directory: &File, name: &str, create: bool) -> Result<File, ApiError> {
    let name = CString::new(name).map_err(|_| storage_error())?;
    let flags =
        libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC | if create { libc::O_CREAT } else { 0 };
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    if fd < 0 {
        return Err(storage_error());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata().map_err(|_| storage_error())?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() > MAX_RECORD as u64
    {
        return Err(storage_error());
    }
    Ok(file)
}
struct LockedState {
    directory: File,
    _lock: File,
}
impl LockedState {
    fn read(&self) -> Result<State, ApiError> {
        let path = CString::new("session.json").expect("literal");
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        let exists = unsafe {
            libc::fstatat(
                self.directory.as_raw_fd(),
                path.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if exists < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            return Ok(State::default());
        }
        if exists < 0 {
            return Err(storage_error());
        }
        let mut bytes = Vec::new();
        open_private(&self.directory, "session.json", false)?
            .take((MAX_RECORD + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| storage_error())?;
        if bytes.len() > MAX_RECORD {
            return Err(storage_error());
        }
        let state: State = serde_json::from_slice(&bytes).map_err(|_| storage_error())?;
        validate_state(&state)?;
        Ok(state)
    }
    fn write(&self, state: &State) -> Result<(), ApiError> {
        validate_state(state)?;
        // Validate an existing target before replacement; refuse malicious links
        // even though rename itself would not follow them.
        let target = CString::new("session.json").expect("literal");
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                self.directory.as_raw_fd(),
                target.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
        {
            open_private(&self.directory, "session.json", false)?;
        } else if std::io::Error::last_os_error().kind() != std::io::ErrorKind::NotFound {
            return Err(storage_error());
        }
        let bytes = serde_json::to_vec(state).map_err(|_| storage_error())?;
        if bytes.len() > MAX_RECORD {
            return Err(storage_error());
        }
        let temporary = CString::new(format!(".session-{}.tmp", new_generation())).expect("uuid");
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                temporary.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(storage_error());
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        let result = (|| {
            file.write_all(&bytes).map_err(|_| storage_error())?;
            file.sync_all().map_err(|_| storage_error())?;
            if unsafe {
                libc::renameat(
                    self.directory.as_raw_fd(),
                    temporary.as_ptr(),
                    self.directory.as_raw_fd(),
                    target.as_ptr(),
                )
            } != 0
            {
                return Err(storage_error());
            }
            self.directory.sync_all().map_err(|_| storage_error())
        })();
        if result.is_err() {
            unsafe { libc::unlinkat(self.directory.as_raw_fd(), temporary.as_ptr(), 0) };
        }
        result
    }
}
fn validate_state(state: &State) -> Result<(), ApiError> {
    let mut ids = HashSet::new();
    let mut clients = HashSet::new();
    if state.version != 1
        || state
            .host_id
            .strip_prefix("urn:uuid:")
            .is_none_or(|id| uuid::Uuid::parse_str(id).is_err())
        || uuid::Uuid::parse_str(&state.generation).is_err()
        || state.registrations.len() > 100
    {
        return Err(storage_error());
    }
    for registration in &state.registrations {
        if uuid::Uuid::parse_str(&registration.id).is_err()
            || !ids.insert(&registration.id)
            || !clients.insert(&registration.client_id)
            || registration.client_id.is_empty()
            || registration.client_id == "dynamic_agent_client"
            || registration.subject.is_empty()
        {
            return Err(storage_error());
        }
    }
    if state.selected.as_ref().is_some_and(|id| !ids.contains(id)) {
        return Err(storage_error());
    }
    if let Some(pending) = &state.pending_registration {
        if uuid::Uuid::parse_str(&pending.id).is_err()
            || pending.client_id.is_empty()
            || pending.client_id == "dynamic_agent_client"
            || clients.contains(&pending.client_id)
            || ids.contains(&pending.id)
        {
            return Err(storage_error());
        }
    }
    Ok(())
}

struct LoginOptions {
    new: bool,
    account: Option<String>,
    port: u16,
    listen: IpAddr,
}
impl LoginOptions {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut result = Self {
            new: false,
            account: None,
            port: 1455,
            listen: IpAddr::V4(Ipv4Addr::LOCALHOST),
        };
        let mut index = 0;
        let mut seen = HashSet::new();
        while index < args.len() {
            if !seen.insert(args[index].as_str()) {
                return Err("Duplicate login option".to_owned());
            }
            match args[index].as_str() {
                "--new" => result.new = true,
                "--account" | "--port" | "--listen" => {
                    let value = args.get(index + 1).ok_or_else(operator_usage)?;
                    match args[index].as_str() {
                        "--account" => result.account = Some(value.clone()),
                        "--port" => {
                            result.port = value
                                .parse()
                                .map_err(|_| "Invalid callback port".to_owned())?
                        }
                        _ => {
                            result.listen = value
                                .parse()
                                .map_err(|_| "Invalid callback listen IP".to_owned())?
                        }
                    }
                    index += 1;
                }
                _ => return Err(operator_usage()),
            }
            index += 1;
        }
        if result.new && result.account.is_some() {
            return Err("Choose --new or --account, not both".to_owned());
        }
        // Nonloopback is needed in a container, whose port must be published on
        // host 127.0.0.1 only; the redirect URI always remains 127.0.0.1.
        if !result.listen.is_loopback() && !result.listen.is_unspecified() {
            return Err(
                "Callback listen IP must be loopback or an unspecified container interface"
                    .to_owned(),
            );
        }
        Ok(result)
    }
}
struct Transaction {
    state: String,
    nonce: String,
    verifier: String,
    redirect: String,
}
impl Transaction {
    fn new(redirect: String) -> Self {
        Self {
            state: random_value(),
            nonce: random_value(),
            verifier: random_value(),
            redirect,
        }
    }
    fn authorization_url(&self, client_id: &str, host: &str, request_consent: bool) -> String {
        let mut url = reqwest::Url::parse(AUTHORIZE).expect("official URL");
        {
            let mut query = url.query_pairs_mut();
            query.extend_pairs([
                ("client_id", client_id),
                ("ext_agent_host_id", host),
                ("response_type", "code"),
                ("redirect_uri", &self.redirect),
                ("scope", REQUESTED_SCOPES),
                ("resource", RESOURCE),
                ("state", &self.state),
                ("nonce", &self.nonce),
                ("code_challenge_method", "S256"),
                (
                    "code_challenge",
                    &URL_SAFE_NO_PAD.encode(Sha256::digest(self.verifier.as_bytes())),
                ),
            ]);
            if client_id == "dynamic_agent_client" {
                query.append_pair("agent_name_hint", "ai-router");
            }
            if request_consent {
                query.append_pair("prompt", "consent");
            }
        }
        url.into()
    }
}
fn random_value() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}
struct Callback {
    code: String,
    client_id: Option<String>,
}
fn parse_callback(target: &str, transaction: &Transaction) -> Result<Callback, String> {
    if !target.starts_with("/auth/callback?") || target.len() > 16 * 1024 {
        return Err("Invalid OpenAI callback path".to_owned());
    }
    let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|_| "Invalid OpenAI callback".to_owned())?;
    if url.path() != "/auth/callback" || url.fragment().is_some() {
        return Err("Invalid OpenAI callback path".to_owned());
    }
    let mut query = HashMap::new();
    for (key, value) in url.query_pairs() {
        if query.insert(key.into_owned(), value.into_owned()).is_some() {
            return Err("Duplicate OpenAI callback parameter".to_owned());
        }
    }
    use subtle::ConstantTimeEq;
    if !query
        .get("state")
        .is_some_and(|state| bool::from(state.as_bytes().ct_eq(transaction.state.as_bytes())))
    {
        return Err("OpenAI callback state did not match; sign-in stopped".to_owned());
    }
    if query.contains_key("error") {
        return Err("OpenAI authorization was denied or could not be completed".to_owned());
    }
    let code = query
        .remove("code")
        .filter(|code| !code.is_empty())
        .ok_or_else(|| "OpenAI callback omitted the authorization code".to_owned())?;
    Ok(Callback {
        code,
        client_id: query.remove("client_id"),
    })
}
async fn receive_callback(
    listener: &TcpListener,
    transaction: &Transaction,
) -> Result<Callback, String> {
    receive_callback_with_timeout(listener, transaction, Duration::from_secs(5)).await
}

struct CallbackRequest {
    method: String,
    target: String,
}

async fn read_callback_request(
    stream: &mut tokio::net::TcpStream,
    timeout: Duration,
) -> Option<CallbackRequest> {
    tokio::time::timeout(timeout, async {
        let mut request = Vec::new();
        let mut buffer = [0u8; 2048];
        loop {
            let count = stream.read(&mut buffer).await.ok()?;
            if count == 0 || request.len().saturating_add(count) > 20 * 1024 {
                return None;
            }
            request.extend_from_slice(&buffer[..count]);
            if request.windows(4).any(|part| part == b"\r\n\r\n") {
                break;
            }
        }
        let first_end = request.windows(2).position(|part| part == b"\r\n")?;
        let first = std::str::from_utf8(&request[..first_end]).ok()?;
        let mut words = first.split_whitespace();
        let method = words.next()?;
        let target = words.next()?;
        let version = words.next()?;
        if words.next().is_some() || !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
            return None;
        }
        Some(CallbackRequest {
            method: method.to_owned(),
            target: target.to_owned(),
        })
    })
    .await
    .ok()
    .flatten()
}

async fn write_callback_reply(stream: &mut tokio::net::TcpStream, reply: &[u8]) {
    // A disconnected or unread browser response must not consume the attempt's
    // remaining time. The callback itself is validated independently of delivery.
    let _ = tokio::time::timeout(Duration::from_secs(1), stream.write_all(reply)).await;
}

async fn receive_callback_with_timeout(
    listener: &TcpListener,
    transaction: &Transaction,
    connection_timeout: Duration,
) -> Result<Callback, String> {
    // The caller applies the overall ten-minute authorization deadline. Browser
    // preconnects and unrelated requests are not OAuth results. Bound their
    // readers independently so idle sockets cannot block the actual callback.
    // Dropping this JoinSet on timeout/completion aborts all remaining readers.
    let mut readers = tokio::task::JoinSet::new();
    loop {
        let completed = tokio::select! {
            accepted = listener.accept(), if readers.len() < 16 => {
                let (mut stream, peer) = accepted.map_err(|_| "OpenAI callback listener failed".to_owned())?;
                if !peer.ip().is_loopback()
                    && !listener.local_addr().is_ok_and(|address| address.ip().is_unspecified()) {
                    continue;
                }
                readers.spawn(async move {
                    let request = read_callback_request(&mut stream, connection_timeout).await;
                    (stream, request)
                });
                continue;
            }
            completed = readers.join_next(), if !readers.is_empty() => completed,
        };
        let Some(Ok((mut stream, Some(request)))) = completed else {
            continue;
        };
        if request.target.split('?').next() != Some("/auth/callback") {
            write_callback_reply(
                &mut stream,
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await;
            continue;
        }
        if request.method != "GET" {
            write_callback_reply(&mut stream, b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
            continue;
        }
        // A complete GET to the exact callback path consumes this attempt once,
        // including state mismatch, duplicate fields, and explicit OAuth denial.
        let result = parse_callback(&request.target, transaction);
        let body = if result.is_ok() {
            "Sign-in received. Return to ai-router to check completion."
        } else {
            "Sign-in could not be verified. Return to ai-router and start a new login."
        };
        let status = if result.is_ok() {
            "200 OK"
        } else {
            "400 Bad Request"
        };
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        write_callback_reply(&mut stream, reply.as_bytes()).await;
        return result;
    }
}
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";
    // Argument passing, never a shell. Headless operators can use the printed URL.
    let _ = std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
#[derive(Clone)]
struct MockEndpoints {
    base: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::State as HttpState,
        http::StatusCode,
        routing::{get, post},
    };
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    // Synthetic test signing material, generated for these fixtures only.
    const DUMMY_RSA_DER: &str = "MIIEpAIBAAKCAQEAvAsCbtNQus2JuNGSnEduWDlMvVR+F3IuNR6hyp3s0HypX2u4/ug38po83JSr/t7UB/ID7IfMHlBlI5NWum0Y3lMnNtO+BfaltTWMrwZ6payvlNRzsyn0Am7H53D7hRtLDs+rcvxneOHRcY3MsMBnaHxs/CxCv9ZfbOMOH0X+9KsHA+Lmm2T5goaxXLJcZXJBh8IEDq7Uj8cavdJgkpQUFp0wbIFugM/x7n8mCkIcDdaLiKpgfewMCP+j4LrFynBy3b+EqbvyJBDAVTNcWPILq/T1P/MBBSeGm4end4oC7YxxneKCwiGp/sIBbF1uLzxI3xYxLYASqtOw8lknjI+GiwIDAQABAoIBAD2jNjbxmWLCAMTOHUqQt4C/t4HP8kzpJnBPDCWIQpnMYxBeFROpNr2VBSIgsYgnrkS0dcjQcn31SFGy+Bi/rgkiJJZ6La1yvbp6RRUSAJ3RzvPv98TgmZU6jtg0Vp245o+CTuawCZK8IpUgL9o4Fy1Oq8K1F2QedsUQYA6sOPgljsrjSLtGJcpMPOQKVF+KJpmxF21HmfzqzxTTTmD2TlCdWXPnHEsfdvUAtEuH6gwIeZ2CXJ4ZlMBV1EgwFoyUkJIm87Qss5pLvdJtuEfQ9wHm/1YsWcibF/KF+WM//XgRniaMuPEQZ6cudpMKhi7T0d1HXjnBuA2rXSbcLymRFyECgYEA9DrMw5S3Xx6VTpM9PCPcha3m+NaMJaB64cNVFEjFMJfePN3BU8Sm9OkFBoQrKj/z7RoerT40Nc/0FxyjqMk2824sMO+MJY4RfG8VKoCdqopv9TPrih3HcRIPIHX2BYaGdbDnqNRQTgLHIuPi+JfOeZbvvZ32eMHjsckBvwWoF9MCgYEAxRsAsNrigG4SbeoABmv6qzFmuJUruaFHpX+lFhY5Mk2IaypCi4nBGGzA3twOWHUfHQ/CLtiWICH9pbhJwGbvN7YIwikCciXwGv91d011WxEZEMOybPQjzTSlmS1F5sJv8nAUbl59DNxTv7Ks1GL+i3YWWHjTD0BkGxF92iVrm2kCgYAZHgL5Ck5hhe+tCQ9/I+EGDoB2jvoS1JJOrZwOXf2JpdTBRb0gNFVHUr+lj+owiis38zu8bW1JK33a8qMSNL9mAaqhzcEIC7hYp8PxfxC3/UzOVuyeY7mjM0xC9l666r7hU7Q/dLZdj4OTXwR0tIgTkSZOxfQXLzmPizCWyKok6QKBgQCzuZGBsbSCgtuPGaV4NZFV9C/D81DxbJwVzjRL1WvF8GiJvvj6n1tQ2CltjJ4QDouWRj471QXeut+78jRdmf5agEOwNxCh4PH0zCbh5+EwboC6hpb1PhA/FthAucVZSsYk/AnFMac4yNYcn+Fzr+iGZeCuP03ERGSDQgnNlf1usQKBgQDoTo5sSdWkYR4zrNfoEn+5jEy3H11KRdvyOdk6k3ld+vA163Do+pkVaImqBTYjRRlM05RhECV73KUS13c84VYPhQcga3sgjUQw7abDPYSGlKefcQ5LvnVqkDi9/nQ7jYG8flONVH+geGNMq+jAH11L9+SXwsygacWNe701QpxHwQ==";
    const DUMMY_MODULUS: &str = "vAsCbtNQus2JuNGSnEduWDlMvVR-F3IuNR6hyp3s0HypX2u4_ug38po83JSr_t7UB_ID7IfMHlBlI5NWum0Y3lMnNtO-BfaltTWMrwZ6payvlNRzsyn0Am7H53D7hRtLDs-rcvxneOHRcY3MsMBnaHxs_CxCv9ZfbOMOH0X-9KsHA-Lmm2T5goaxXLJcZXJBh8IEDq7Uj8cavdJgkpQUFp0wbIFugM_x7n8mCkIcDdaLiKpgfewMCP-j4LrFynBy3b-EqbvyJBDAVTNcWPILq_T1P_MBBSeGm4end4oC7YxxneKCwiGp_sIBbF1uLzxI3xYxLYASqtOw8lknjI-Giw";

    fn temporary() -> tempfile::TempDir {
        let root = std::env::temp_dir()
            .canonicalize()
            .expect("test temporary directory");
        tempfile::tempdir_in(root).unwrap()
    }
    fn store() -> (tempfile::TempDir, AuthStore) {
        let temporary = temporary();
        let store = AuthStore::new(temporary.path().join("auth"));
        (temporary, store)
    }
    fn synthetic_state(expired: bool) -> State {
        let mut state = State::default();
        let registration = Registration {
            id: uuid::Uuid::new_v4().to_string(),
            client_id: "oaiapp_synthetic".to_owned(),
            subject: "synthetic-account".to_owned(),
            email: Some("fixture@example.invalid".to_owned()),
            block: None,
            tokens: Some(Tokens {
                access_token: "synthetic-access-old".to_owned(),
                refresh_token: Some("synthetic-refresh-old".to_owned()),
                id_token: None,
                expires_at: if expired {
                    now().saturating_sub(1)
                } else {
                    now() + 3600
                },
                scopes: vec![DIRECT_SCOPE.to_owned(), "resource.invoke".to_owned()],
            }),
        };
        state.selected = Some(registration.id.clone());
        state.registrations.push(registration);
        state
    }
    async fn save(store: &AuthStore, state: &State) {
        store.lock().await.unwrap().write(state).unwrap();
    }
    fn keys() -> JwkSet {
        serde_json::from_value(json!({"keys":[{"kty":"RSA", "kid":"synthetic", "use":"sig", "alg":"RS256", "n":DUMMY_MODULUS,"e":"AQAB"}]})).unwrap()
    }
    fn signed(mut overrides: serde_json::Value) -> String {
        let mut claims = json!({"iss":ISSUER,"aud":"oaiapp_synthetic","sub":"synthetic-account","exp":now()+3600,"iat":now(),"nonce":"synthetic-nonce"});
        for (key, value) in overrides.as_object_mut().unwrap() {
            claims[key] = value.clone();
        }
        let mut header = jsonwebtoken::Header::new(Algorithm::RS256);
        header.kid = Some("synthetic".to_owned());
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(DUMMY_RSA_DER)
            .unwrap();
        jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_rsa_der(&bytes),
        )
        .unwrap()
    }

    #[test]
    fn jwt_verifies_signature_identity_nonce_and_required_claims() {
        let keyset = keys();
        let token = signed(json!({}));
        assert_eq!(
            validate_identity(&token, "oaiapp_synthetic", Some("synthetic-nonce"), &keyset)
                .unwrap()
                .sub,
            "synthetic-account"
        );
        for change in [
            json!({"iss":"https://attacker.invalid"}),
            json!({"aud":"wrong-client"}),
            json!({"exp":now()-60}),
            json!({"nonce":"wrong"}),
            json!({"sub":""}),
            json!({"iat":null}),
            json!({"iat":now()+60}),
        ] {
            assert!(
                validate_identity(
                    &signed(change),
                    "oaiapp_synthetic",
                    Some("synthetic-nonce"),
                    &keyset
                )
                .is_err()
            );
        }
        let mut pieces: Vec<_> = token.split('.').map(str::to_owned).collect();
        pieces[1] = URL_SAFE_NO_PAD.encode(br#"{"sub":"tampered"}"#);
        assert!(
            validate_identity(
                &pieces.join("."),
                "oaiapp_synthetic",
                Some("synthetic-nonce"),
                &keyset
            )
            .is_err()
        );
        let mut header = jsonwebtoken::Header::new(Algorithm::HS256);
        header.kid = Some("synthetic".to_owned());
        let forged = jsonwebtoken::encode(
            &header,
            &json!({"sub":"forged","exp":now()+3600}),
            &jsonwebtoken::EncodingKey::from_secret(b"dummy"),
        )
        .unwrap();
        assert!(validate_identity(&forged, "oaiapp_synthetic", None, &keyset).is_err());
    }

    #[test]
    fn callback_is_bound_to_state_and_rejects_duplicates_and_denial() {
        let transaction = Transaction::new("http://127.0.0.1:1455/auth/callback".to_owned());
        let target = format!(
            "/auth/callback?state={}&code=synthetic&client_id=oaiapp_synthetic",
            transaction.state
        );
        assert_eq!(
            parse_callback(&target, &transaction).unwrap().code,
            "synthetic"
        );
        for target in [
            format!("{target}&state=duplicate"),
            format!("{target}&error=access_denied"),
            "/auth/callback?state=wrong&code=synthetic".to_owned(),
            format!("/callback?state={}&code=synthetic", transaction.state),
            format!("/auth/callback?state={}", transaction.state),
        ] {
            assert!(parse_callback(&target, &transaction).is_err());
        }
    }

    async fn synthetic_callback_connection(
        listener: &TcpListener,
        request: &[u8],
    ) -> tokio::net::TcpStream {
        let mut stream = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        if !request.is_empty() {
            stream.write_all(request).await.unwrap();
        }
        stream
    }

    fn callback_http_request(transaction: &Transaction, query: &str) -> Vec<u8> {
        format!(
            "GET /auth/callback?state={}&{query} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            transaction.state
        )
        .into_bytes()
    }

    #[tokio::test]
    async fn idle_browser_preconnect_does_not_block_a_valid_callback() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let transaction = Transaction::new("http://127.0.0.1:1455/auth/callback".to_owned());
        let _idle = synthetic_callback_connection(&listener, b"").await;
        let request =
            callback_http_request(&transaction, "code=synthetic&client_id=oaiapp_synthetic");
        let callback = synthetic_callback_connection(&listener, &request);
        let receive = receive_callback(&listener, &transaction);
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            let (_callback, result) = tokio::join!(callback, receive);
            result
        })
        .await
        .expect("idle preconnect must not delay callback until its five-second timeout")
        .unwrap();
        assert_eq!(result.code, "synthetic");
    }

    #[tokio::test]
    async fn expired_closed_malformed_and_irrelevant_connections_do_not_end_authorization() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let transaction = Transaction::new("http://127.0.0.1:1455/auth/callback".to_owned());
        let receive =
            receive_callback_with_timeout(&listener, &transaction, Duration::from_millis(40));
        let clients = async {
            let mut idle = synthetic_callback_connection(&listener, b"").await;
            drop(
                synthetic_callback_connection(&listener, b"GET /auth/callback?code=partial").await,
            );
            let _malformed =
                synthetic_callback_connection(&listener, b"\xff /not-http HTTP/1.1\r\n\r\n").await;
            for request in [
                b"GET /favicon.ico HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n".as_slice(),
                b"GET /auth/callback-other?state=wrong HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
                    .as_slice(),
                b"POST /auth/callback HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n"
                    .as_slice(),
            ] {
                let mut stream = synthetic_callback_connection(&listener, request).await;
                let mut response = Vec::new();
                stream.read_to_end(&mut response).await.unwrap();
                assert!(
                    response.starts_with(b"HTTP/1.1 404") || response.starts_with(b"HTTP/1.1 405")
                );
            }
            // Observe the idle reader's expiry before supplying the real result.
            let mut byte = [0u8; 1];
            assert_eq!(idle.read(&mut byte).await.unwrap(), 0);
            let request = callback_http_request(&transaction, "code=synthetic");
            let mut callback = synthetic_callback_connection(&listener, &request).await;
            let mut response = Vec::new();
            callback.read_to_end(&mut response).await.unwrap();
            assert!(response.starts_with(b"HTTP/1.1 200"));
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            let (_, result) = tokio::join!(clients, receive);
            assert_eq!(result.unwrap().code, "synthetic");
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn callback_listener_still_consumes_state_failure_and_obeys_overall_deadline() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let transaction = Transaction::new("http://127.0.0.1:1455/auth/callback".to_owned());
        let _invalid = synthetic_callback_connection(
            &listener,
            b"GET /auth/callback?state=wrong&code=synthetic HTTP/1.1\r\n\r\n",
        )
        .await;
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            receive_callback(&listener, &transaction),
        )
        .await
        .unwrap();
        match result {
            Err(error) => assert!(error.contains("state did not match")),
            Ok(_) => panic!("invalid callback state must fail closed"),
        };
        let mut idle = synthetic_callback_connection(&listener, b"").await;
        assert!(
            tokio::time::timeout(
                Duration::from_millis(60),
                receive_callback_with_timeout(&listener, &transaction, Duration::from_secs(5))
            )
            .await
            .is_err()
        );
        let mut byte = [0u8; 1];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), idle.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }

    #[test]
    fn pkce_and_registration_reuse_follow_official_parameters() {
        let transaction = Transaction::new("http://127.0.0.1:1455/auth/callback".to_owned());
        assert!(transaction.verifier.len() >= 43);
        let url = reqwest::Url::parse(&transaction.authorization_url(
            "dynamic_agent_client",
            "urn:uuid:synthetic",
            false,
        ))
        .unwrap();
        let query: HashMap<_, _> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(
            query["code_challenge"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(transaction.verifier.as_bytes()))
        );
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["resource"], RESOURCE);
        assert_eq!(query["agent_name_hint"], "ai-router");
        let returning =
            transaction.authorization_url("oaiapp_synthetic", "urn:uuid:synthetic", false);
        assert!(!returning.contains("agent_name_hint"));
        assert!(!returning.contains("id_token_hint"));
        assert_ne!(
            transaction.state,
            Transaction::new(transaction.redirect.clone()).state
        );
    }

    #[tokio::test]
    async fn persistence_is_private_atomic_and_constructor_is_lazy() {
        let (temporary, store) = store();
        assert!(!temporary.path().join("auth").exists());
        let state = synthetic_state(false);
        save(&store, &state).await;
        let restarted = AuthStore::new(store.directory.clone());
        let token = restarted.token().await.unwrap();
        assert_eq!(token.access_token, "synthetic-access-old");
        assert_eq!(token.generation, state.generation);
        assert_eq!(
            std::fs::metadata(&store.directory).unwrap().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(store.directory.join("session.json"))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
        let names: Vec<_> = std::fs::read_dir(&store.directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2);
    }

    #[tokio::test]
    async fn storage_rejects_synthetic_symlinks_hardlinks_permissions_and_forbidden_paths() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (temporary, store) = store();
        let dummy = temporary.path().join("dummy.json");
        std::fs::write(&dummy, b"synthetic dummy").unwrap();
        std::fs::set_permissions(&dummy, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::create_dir(&store.directory).unwrap();
        std::fs::set_permissions(&store.directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&dummy, store.directory.join("session.json")).unwrap();
        assert!(store.token().await.is_err());
        std::fs::remove_file(store.directory.join("session.json")).unwrap();
        std::fs::hard_link(&dummy, store.directory.join("session.json")).unwrap();
        assert!(store.token().await.is_err());
        std::fs::remove_file(store.directory.join("session.json")).unwrap();
        symlink(&store.directory, temporary.path().join("alias")).unwrap();
        assert!(
            AuthStore::new(temporary.path().join("alias"))
                .token()
                .await
                .is_err()
        );
        std::fs::set_permissions(&store.directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(store.token().await.is_err());
        for component in [
            ".env.synthetic",
            "dummy.pem",
            "dummy.key",
            "secrets",
            "credentials",
        ] {
            assert!(
                AuthStore::new(temporary.path().join(component))
                    .token()
                    .await
                    .is_err()
            );
            assert!(!temporary.path().join(component).exists());
        }
        assert!(
            AuthStore::new(temporary.path().join("auth/../other"))
                .token()
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn pending_registration_reuses_issued_client_without_activating_unverified_identity() {
        let (_temporary, store) = store();
        let state = State {
            pending_registration: Some(PendingRegistration {
                id: new_generation(),
                client_id: "oaiapp_pending_synthetic".to_owned(),
            }),
            ..Default::default()
        };
        save(&store, &state).await;
        let restarted = AuthStore::new(store.directory.clone());
        let saved = restarted.lock().await.unwrap().read().unwrap();
        assert_eq!(saved.host_id, state.host_id);
        assert!(saved.selected.is_none());
        assert!(saved.registrations.is_empty());
        let pending = saved.pending_registration.unwrap();
        let transaction = Transaction::new("http://127.0.0.1:1455/auth/callback".to_owned());
        let url = transaction.authorization_url(&pending.client_id, &saved.host_id, false);
        assert!(url.contains("client_id=oaiapp_pending_synthetic"));
        assert!(!url.contains("dynamic_agent_client"));
        assert!(!url.contains("agent_name_hint"));
        assert!(restarted.token().await.is_err());
    }

    #[tokio::test]
    async fn durable_block_survives_restart_and_does_not_change_generation() {
        let (_temporary, store) = store();
        let state = synthetic_state(false);
        save(&store, &state).await;
        store
            .block("subscription_sharing_usage_limit_exceeded")
            .await
            .unwrap();
        let restarted = AuthStore::new(store.directory.clone());
        assert_eq!(restarted.generation().await.unwrap(), state.generation);
        assert_eq!(
            restarted.availability_error().await.unwrap().code,
            "openai_usage_blocked"
        );
        assert!(matches!(restarted.token().await,Err(error) if error.status == 429));
        assert!(restarted.token_inner(true).await.is_ok());
        assert!(restarted.block("arbitrary-untrusted-code").await.is_err());
        let old_generation = state.generation.clone();
        let locked = restarted.lock().await.unwrap();
        let mut state = locked.read().unwrap();
        state.generation = new_generation();
        state.registrations[0].block = None;
        locked.write(&state).unwrap();
        drop(locked);
        restarted
            .block_for_generation("user_not_eligible", &old_generation)
            .await
            .unwrap();
        assert!(restarted.availability_error().await.is_none());
    }

    #[tokio::test]
    async fn canceled_restriction_caller_still_persists_after_state_lock_release() {
        let (_temporary, store) = store();
        let state = synthetic_state(false);
        save(&store, &state).await;
        let locked = store.lock().await.unwrap();
        let writer_store = store.clone();
        let generation = state.generation.clone();
        let caller = tokio::spawn(async move {
            writer_store
                .block_for_generation("subscription_sharing_usage_limit_exceeded", &generation)
                .await
        });
        // The caller has spawned its owned write and is waiting for flock.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!caller.is_finished());
        caller.abort();
        let _ = caller.await;
        drop(locked);
        let restarted = AuthStore::new(store.directory.clone());
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if restarted
                    .availability_error()
                    .await
                    .is_some_and(|error| error.code == "openai_usage_blocked")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(restarted.generation().await.unwrap(), state.generation);
        assert!(
            matches!(restarted.token().await, Err(error) if error.code == "openai_usage_blocked")
        );
    }

    #[tokio::test]
    async fn canceled_restriction_write_cannot_block_a_replaced_session() {
        let (_temporary, store) = store();
        let mut state = synthetic_state(false);
        save(&store, &state).await;
        let locked = store.lock().await.unwrap();
        let writer_store = store.clone();
        let old_generation = state.generation.clone();
        let caller = tokio::spawn(async move {
            writer_store
                .block_for_generation("subscription_sharing_usage_limit_exceeded", &old_generation)
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!caller.is_finished());
        caller.abort();
        let _ = caller.await;
        state.generation = new_generation();
        locked.write(&state).unwrap();
        drop(locked);
        // The old owned task is bounded and retries flock every25ms. Also write
        // a current-generation restriction to prove the new session stays intact.
        store
            .block_for_generation("user_not_eligible", &state.generation)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let restarted = AuthStore::new(store.directory.clone());
        assert_eq!(restarted.generation().await.unwrap(), state.generation);
        assert_eq!(
            restarted.availability_error().await.unwrap().code,
            "openai_ineligible"
        );
    }

    #[tokio::test]
    async fn documented_availability_codes_map_to_the_correct_recovery() {
        let (_temporary, store) = store();
        let state = synthetic_state(false);
        save(&store, &state).await;
        for (upstream, expected) in [
            (
                "subscription_sharing_usage_limit_exceeded",
                "openai_usage_blocked",
            ),
            (
                "subscription_sharing_user_not_eligible",
                "openai_ineligible",
            ),
            ("subscription_sharing_invalid_user", "openai_auth_required"),
            ("chatpass_v2_scope_not_authorized", "openai_auth_required"),
            (
                "chatpass_v2_invalid_authorization_context",
                "openai_auth_required",
            ),
        ] {
            store
                .block_for_generation(upstream, &state.generation)
                .await
                .unwrap();
            assert_eq!(store.availability_error().await.unwrap().code, expected);
            assert_eq!(store.generation().await.unwrap(), state.generation);
        }
    }

    #[derive(Clone)]
    struct MockState {
        base: String,
        calls: Arc<AtomicUsize>,
        mode: Arc<AtomicUsize>,
    }
    async fn mock_server() -> (MockState, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let state = MockState {
            base: format!("http://{}", listener.local_addr().unwrap()),
            calls: Arc::new(AtomicUsize::new(0)),
            mode: Arc::new(AtomicUsize::new(0)),
        };
        async fn discovery(HttpState(state): HttpState<MockState>) -> Json<serde_json::Value> {
            Json(
                json!({"issuer":ISSUER,"authorization_endpoint":AUTHORIZE,"token_endpoint":TOKEN,
                "jwks_uri":format!("{}/jwks",state.base),"revocation_endpoint":format!("{}/revoke",state.base)}),
            )
        }
        async fn exchange(
            HttpState(state): HttpState<MockState>,
            body: String,
        ) -> (StatusCode, Json<serde_json::Value>) {
            state.calls.fetch_add(1, Ordering::SeqCst);
            assert!(body.contains("client_id=oaiapp_synthetic"));
            assert!(!body.contains("dynamic_agent_client"));
            assert!(!body.contains("scope="));
            tokio::time::sleep(Duration::from_millis(40)).await;
            let mode = state.mode.load(Ordering::SeqCst);
            if mode >= 4 {
                let code = match mode {
                    4 => "invalid_client",
                    5 => "invalid_refresh_token",
                    6 => "token_expired",
                    7 => "refresh_token_expired",
                    8 => "refresh_token_invalidated",
                    9 => "refresh_token_reused",
                    _ => "unauthorized_client",
                };
                return (
                    StatusCode::BAD_REQUEST,
                    Json(
                        json!({"error":code,"error_description":"do not expose synthetic-token-value"}),
                    ),
                );
            }
            match mode {
                1 => (
                    StatusCode::BAD_REQUEST,
                    Json(
                        json!({"error":"invalid_grant","error_description":"do not expose synthetic-token-value"}),
                    ),
                ),
                2 => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(
                        json!({"error":"server_error","error_description":"do not expose synthetic-token-value"}),
                    ),
                ),
                3 => (
                    StatusCode::OK,
                    Json(
                        json!({"access_token":"synthetic-access-new","refresh_token":"synthetic-refresh-new", "expires_in":3600,"token_type":"Bearer","scope":"openid"}),
                    ),
                ),
                _ => (
                    StatusCode::OK,
                    Json(
                        json!({"access_token":"synthetic-access-new","refresh_token":"synthetic-refresh-new", "expires_in":3600,"token_type":"Bearer"}),
                    ),
                ),
            }
        }
        async fn revoke(body: String) -> StatusCode {
            assert!(body.contains("token=synthetic-refresh-old"));
            assert!(body.contains("token_type_hint=refresh_token"));
            StatusCode::OK
        }
        let router = Router::new()
            .route("/discovery", get(discovery))
            .route("/token", post(exchange))
            .route(
                "/jwks",
                get(|| async { Json(serde_json::to_value(keys()).unwrap()) }),
            )
            .route("/revoke", post(revoke))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (state, task)
    }
    async fn mocked(
        expired: bool,
    ) -> (
        tempfile::TempDir,
        AuthStore,
        State,
        MockState,
        tokio::task::JoinHandle<()>,
    ) {
        let (temporary, mut store) = store();
        let (mock, task) = mock_server().await;
        store.mock = Some(MockEndpoints {
            base: mock.base.clone(),
        });
        let state = synthetic_state(expired);
        save(&store, &state).await;
        (temporary, store, state, mock, task)
    }

    #[tokio::test]
    async fn rotating_refresh_is_serialized_and_stable_across_store_instances() {
        let (_temporary, store, state, mock, task) = mocked(true).await;
        let second = AuthStore {
            directory: store.directory.clone(),
            mock: store.mock.clone(),
        };
        let (first, next) = tokio::join!(store.token(), second.token());
        for token in [first.unwrap(), next.unwrap()] {
            assert_eq!(token.access_token, "synthetic-access-new");
            assert_eq!(token.generation, state.generation);
        }
        assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
        let saved = store.lock().await.unwrap().read().unwrap();
        assert_eq!(
            saved.registrations[0]
                .tokens
                .as_ref()
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("synthetic-refresh-new")
        );
        task.abort();
    }

    #[tokio::test]
    async fn caller_cancellation_does_not_lose_a_rotating_refresh() {
        let (_temporary, store, state, mock, task) = mocked(true).await;
        let caller_store = store.clone();
        let caller = tokio::spawn(async move { caller_store.token().await });
        while mock.calls.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        caller.abort();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let token = store.token().await.unwrap();
        assert_eq!(token.access_token, "synthetic-access-new");
        assert_eq!(token.generation, state.generation);
        assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
        task.abort();
    }

    #[tokio::test]
    async fn refresh_terminal_failure_clears_tokens_and_transient_or_client_configuration_failure_preserves_them()
     {
        for mode in [1, 2, 4, 5, 6, 7, 8, 9, 10] {
            let (_temporary, store, state, mock, task) = mocked(true).await;
            mock.mode.store(mode, Ordering::SeqCst);
            let error = match store.token().await {
                Ok(_) => panic!("refresh should fail"),
                Err(error) => error,
            };
            assert!(!error.message.contains("synthetic-token-value"));
            let saved = store.lock().await.unwrap().read().unwrap();
            if matches!(mode, 1 | 5 | 6 | 7 | 8 | 9) {
                assert!(saved.registrations[0].tokens.is_none());
                assert_ne!(saved.generation, state.generation);
                assert_eq!(
                    saved.registrations[0].client_id,
                    state.registrations[0].client_id
                );
                assert_eq!(saved.host_id, state.host_id);
            } else {
                if matches!(mode, 4 | 10) {
                    assert!(error.message.contains("client configuration"));
                }
                assert_eq!(saved.generation, state.generation);
                assert_eq!(
                    saved.registrations[0].tokens.as_ref().unwrap().access_token,
                    "synthetic-access-old"
                );
            }
            task.abort();
        }
    }

    #[tokio::test]
    async fn identity_only_login_is_retained_but_cannot_infer_and_reuses_issued_client_for_consent()
    {
        let (_temporary, store, mut state, mock, task) = mocked(false).await;
        let id_token = signed(json!({}));
        let identity = store
            .identity(&id_token, "oaiapp_synthetic", Some("synthetic-nonce"))
            .await
            .unwrap();
        let reply = TokenReply {
            access_token: "synthetic-identity-access".to_owned(),
            refresh_token: Some("synthetic-identity-refresh".to_owned()),
            id_token: Some(id_token),
            expires_in: 3600,
            token_type: "Bearer".to_owned(),
            scope: Some("openid profile email offline_access resource.invoke".to_owned()),
        };
        let tokens = store
            .validate_reply(
                reply,
                None,
                "oaiapp_synthetic",
                Some("synthetic-nonce"),
                Some(&identity.sub),
                true,
            )
            .await
            .unwrap();
        state.registrations[0].subject = identity.sub;
        state.registrations[0].email = identity.email;
        state.registrations[0].tokens = Some(tokens);
        state.generation = new_generation();
        save(&store, &state).await;
        let mut restarted = AuthStore::new(store.directory.clone());
        restarted.mock = store.mock.clone();
        assert_eq!(
            restarted.availability_error().await.unwrap().code,
            "openai_auth_required"
        );
        assert!(
            matches!(restarted.token().await, Err(error) if error.code == "openai_auth_required")
        );
        assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
        let saved = restarted.lock().await.unwrap().read().unwrap();
        let account = &saved.registrations[0];
        assert_eq!(account.subject, "synthetic-account");
        assert_eq!(
            account.tokens.as_ref().unwrap().access_token,
            "synthetic-identity-access"
        );
        let transaction = Transaction::new("http://127.0.0.1:1455/auth/callback".to_owned());
        let consent = transaction.authorization_url(&account.client_id, &saved.host_id, true);
        let url = reqwest::Url::parse(&consent).unwrap();
        let query: HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(query["client_id"], "oaiapp_synthetic");
        assert_eq!(query["prompt"], "consent");
        assert!(
            query["scope"]
                .split_whitespace()
                .any(|scope| scope == DIRECT_SCOPE)
        );
        assert!(!consent.contains("dynamic_agent_client"));
        assert!(!consent.contains("agent_name_hint"));
        task.abort();
    }

    #[tokio::test]
    async fn refreshed_scope_loss_is_rejected_without_exposing_token() {
        let (_temporary, store, _state, mock, task) = mocked(true).await;
        mock.mode.store(3, Ordering::SeqCst);
        let error = match store.token().await {
            Ok(_) => panic!("scope loss must fail"),
            Err(error) => error,
        };
        assert_eq!(error.code, "openai_auth_required");
        assert!(!error.message.contains("synthetic-access-new"));
        task.abort();
    }

    #[tokio::test]
    async fn logout_revokes_and_preserves_registration_and_host() {
        let (_temporary, store, state, _mock, task) = mocked(false).await;
        store.logout().await.unwrap();
        let saved = store.lock().await.unwrap().read().unwrap();
        assert!(saved.registrations[0].tokens.is_none());
        assert_eq!(saved.host_id, state.host_id);
        assert_eq!(
            saved.registrations[0].client_id,
            state.registrations[0].client_id
        );
        assert_ne!(saved.generation, state.generation);
        task.abort();
    }

    #[tokio::test]
    async fn remote_revocation_failure_still_clears_local_session() {
        let (_temporary, store, state, _mock, task) = mocked(false).await;
        task.abort();
        let error = store.logout().await.unwrap_err();
        assert!(error.contains("remote revocation was not confirmed"));
        let saved = store.lock().await.unwrap().read().unwrap();
        assert!(saved.registrations[0].tokens.is_none());
        assert_ne!(saved.generation, state.generation);
    }

    #[test]
    fn login_options_reject_ambiguous_and_unintended_interfaces() {
        let args = |values: &[&str]| values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        assert!(LoginOptions::parse(&args(&["--new", "--account", "id"])).is_err());
        assert!(LoginOptions::parse(&args(&["--port", "1", "--port", "2"])).is_err());
        assert!(LoginOptions::parse(&args(&["--listen", "192.168.1.1"])).is_err());
        assert!(LoginOptions::parse(&args(&["--listen", "0.0.0.0", "--port", "0"])).is_ok());
        assert!(!official_auth_url(
            "https://auth.openai.com.attacker.invalid/jwks"
        ));
        assert!(!official_auth_url("https://auth.openai.com:444/jwks"));
        assert!(!official_auth_url("http://auth.openai.com/jwks"));
        assert!(!official_auth_url("https://token@auth.openai.com/jwks"));
    }
}
