//! Public, unauthenticated advisory checks. This module cannot change account
//! restrictions, the pinned identity, dispatch, or provider readiness.
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

pub const PINNED_VERSION: &str = "2.19.1";
pub const PINNED_REVISION: &str = "2220e58146ebe31508e7a216242840d373e082d0";
pub const MANIFEST_URL: &str = "https://antigravity-hub-auto-updater-974169037036.us-central1.run.app/manifest/latest-arm64-mac.yml";
pub const REVISION_URL: &str = "https://api.github.com/repos/can1357/oh-my-pi/commits/main";
const MAX_BYTES: usize = 64 * 1024;
const TIMEOUT: Duration = Duration::from_secs(5);
const STALE_AFTER: Duration = Duration::from_secs(3 * 3600);

#[derive(Default)]
struct SourceState {
    checked_at: Option<DateTime<Utc>>,
    success_at: Option<DateTime<Utc>>,
    success_instant: Option<Instant>,
    observed: Option<String>,
    error: Option<&'static str>,
}
#[derive(Default)]
struct State {
    checked_at: Option<DateTime<Utc>>,
    manifest: SourceState,
    revision: SourceState,
}

#[derive(Clone)]
pub struct ReleaseMonitor {
    client: reqwest::Client,
    state: Arc<Mutex<State>>,
    refresh: Arc<Mutex<()>>,
}
impl Default for ReleaseMonitor {
    fn default() -> Self {
        Self::new()
    }
}
impl ReleaseMonitor {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .connect_timeout(TIMEOUT)
                .timeout(TIMEOUT)
                .user_agent("ai-router-release-advisory/1")
                .build()
                .expect("fixed TLS client configuration"),
            state: Arc::new(Mutex::new(State::default())),
            refresh: Arc::new(Mutex::new(())),
        }
    }

    /// Background/operator operation only. No inference path needs to await it.
    pub async fn refresh(&self) -> Value {
        let _guard = self.refresh.lock().await;
        let (manifest, revision) = tokio::join!(
            fetch(&self.client, Source::Manifest),
            fetch(&self.client, Source::Revision)
        );
        let checked = Utc::now();
        let now = Instant::now();
        let mut state = self.state.lock().await;
        state.checked_at = Some(checked);
        update(&mut state.manifest, manifest, checked, now);
        update(&mut state.revision, revision, checked, now);
        render(&state, now)
    }

    pub async fn snapshot(&self) -> Value {
        render(&*self.state.lock().await, Instant::now())
    }
}

#[derive(Clone, Copy)]
enum Source {
    Manifest,
    Revision,
}
impl Source {
    fn url(self) -> &'static str {
        match self {
            Self::Manifest => MANIFEST_URL,
            Self::Revision => REVISION_URL,
        }
    }
}
async fn fetch(client: &reqwest::Client, source: Source) -> Result<String, &'static str> {
    // Constants are the only destinations; neither provider inputs nor response
    // metadata can select a URL. This client never receives account headers.
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let mut response = tokio::time::timeout_at(
        deadline,
        client
            .get(source.url())
            .header(
                "Accept",
                match source {
                    Source::Manifest => "text/yaml, text/plain, application/octet-stream",
                    Source::Revision => "application/vnd.github+json",
                },
            )
            .send(),
    )
    .await
    .map_err(|_| "timeout")?
    .map_err(|_| "transport_failed")?;
    if response.status().is_redirection() {
        return Err("redirect_refused");
    }
    if !response.status().is_success() {
        return Err("http_rejected");
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BYTES as u64)
    {
        return Err("body_too_large");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = tokio::time::timeout_at(deadline, response.chunk())
        .await
        .map_err(|_| "timeout")?
        .map_err(|_| "transport_failed")?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_BYTES {
            return Err("body_too_large");
        }
        bytes.extend_from_slice(&chunk);
    }
    match source {
        Source::Manifest => {
            parse_manifest(std::str::from_utf8(&bytes).map_err(|_| "invalid_utf8")?)
        }
        Source::Revision => parse_revision(&bytes),
    }
}

fn update(
    state: &mut SourceState,
    result: Result<String, &'static str>,
    checked: DateTime<Utc>,
    now: Instant,
) {
    state.checked_at = Some(checked);
    match result {
        Ok(observed) => {
            state.observed = Some(observed);
            state.success_at = Some(checked);
            state.success_instant = Some(now);
            state.error = None;
        }
        Err(error) => state.error = Some(error),
    }
}
fn stale(state: &SourceState, now: Instant) -> bool {
    state
        .success_instant
        .is_none_or(|at| now.saturating_duration_since(at) > STALE_AFTER)
}
fn source_json(state: &SourceState, url: &str, now: Instant) -> Value {
    json!({"url":url,"checked_at":state.checked_at,"last_success_at":state.success_at,
        "error":state.error,"stale":stale(state,now)})
}
fn render(state: &State, now: Instant) -> Value {
    let mut alerts = Vec::new();
    if state
        .manifest
        .observed
        .as_deref()
        .is_some_and(|version| version != PINNED_VERSION)
    {
        alerts.push("manifest_version_changed");
    }
    if state
        .revision
        .observed
        .as_deref()
        .is_some_and(|revision| revision != PINNED_REVISION)
    {
        alerts.push("upstream_revision_changed");
    }
    if state.manifest.error.is_some() {
        alerts.push("manifest_check_failed");
    }
    if state.revision.error.is_some() {
        alerts.push("revision_check_failed");
    }
    if stale(&state.manifest, now) {
        alerts.push("manifest_check_stale");
    }
    if stale(&state.revision, now) {
        alerts.push("revision_check_stale");
    }
    json!({"advisory":true,"affects_readiness":false,"checked_at":state.checked_at,
        "pinned_version":PINNED_VERSION,"pinned_revision":PINNED_REVISION,
        "observed_version":state.manifest.observed,"observed_revision":state.revision.observed,
        "stale_after_seconds":STALE_AFTER.as_secs(),"alerts":alerts,
        "sources":{"manifest":source_json(&state.manifest,MANIFEST_URL,now),"revision":source_json(&state.revision,REVISION_URL,now)}})
}

fn valid_version(version: &str) -> bool {
    if version.len() > 32 {
        return false;
    }
    let parts: Vec<_> = version.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.bytes().all(|b| b.is_ascii_digit())
                && part
                    .parse::<u32>()
                    .is_ok_and(|number| number.to_string() == *part)
        })
}
fn parse_manifest(text: &str) -> Result<String, &'static str> {
    if text.len() > MAX_BYTES {
        return Err("body_too_large");
    }
    let mut found = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let key = key
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| key.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(key);
        if key != "version" {
            continue;
        }
        if found.is_some() {
            return Err("duplicate_manifest_version");
        }
        let value = value.trim();
        let version =
            if let Some(quote) = value.chars().next().filter(|ch| matches!(ch, '\'' | '"')) {
                let rest = &value[1..];
                let Some((version, tail)) = rest.split_once(quote) else {
                    return Err("invalid_manifest_version");
                };
                if !tail.trim().is_empty() && !tail.trim().starts_with('#') {
                    return Err("invalid_manifest_version");
                }
                version
            } else {
                value.split('#').next().unwrap_or("").trim()
            };
        if !valid_version(version) {
            return Err("invalid_manifest_version");
        }
        found = Some(version.to_owned());
    }
    found.ok_or("missing_manifest_version")
}
fn parse_revision(bytes: &[u8]) -> Result<String, &'static str> {
    // Derive rejects a repeated known `sha` key while permitting GitHub's other
    // public commit fields. No body or commit message enters monitor metadata.
    #[derive(Deserialize)]
    struct Revision {
        sha: String,
    }
    if bytes.len() > MAX_BYTES {
        return Err("body_too_large");
    }
    let revision: Revision =
        serde_json::from_slice(bytes).map_err(|_| "invalid_revision_metadata")?;
    if revision.sha.len() != 40 || !revision.sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid_revision_metadata");
    }
    Ok(revision.sha.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manifest_allows_only_one_numeric_triplet() {
        for text in [
            "version: 2.19.1\nfiles:\n - url: public-file",
            "version: '2.19.1' # release",
            "  version : \"2.19.1\"\r\n",
        ] {
            assert_eq!(parse_manifest(text).unwrap(), PINNED_VERSION);
        }
        for text in [
            "version: 2.19.1\nversion: 2.19.1",
            "version: 2.19.1\n'Version': 2.19.1\n\"version\": 2.19.1",
            "version: latest",
            "version: 2.19",
            "version: 2.19.1-beta",
            "version: 02.19.1",
            "version: 2.19.1 extra",
            "version: \"2.19.1\" extra",
            "version: 2.19.4294967296",
            "files: []",
            "version: \"2.19.1",
            "version: 2..1",
        ] {
            assert!(parse_manifest(text).is_err(), "{text}");
        }
    }
    #[test]
    fn revision_is_exact_hex_and_duplicate_fields_fail() {
        assert_eq!(
            parse_revision(
                format!(
                    "{{\"sha\":\"{}\",\"commit\":{{\"message\":\"ignored\"}}}}",
                    PINNED_REVISION
                )
                .as_bytes()
            )
            .unwrap(),
            PINNED_REVISION
        );
        for text in [
            "{}",
            "[]",
            "{\"sha\":null}",
            "{\"sha\":\"abc\"}",
            "{\"sha\":\"zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz\"}",
        ] {
            assert!(parse_revision(text.as_bytes()).is_err());
        }
        assert!(
            parse_revision(
                format!("{{\"sha\":\"{PINNED_REVISION}\",\"sha\":\"{PINNED_REVISION}\"}}")
                    .as_bytes()
            )
            .is_err()
        );
    }
    #[test]
    fn failed_and_stale_checks_are_advisory_and_retain_last_observation() {
        let mut state = State::default();
        let now = Instant::now();
        let checked = Utc::now();
        update(&mut state.manifest, Ok(PINNED_VERSION.into()), checked, now);
        update(
            &mut state.revision,
            Ok(PINNED_REVISION.into()),
            checked,
            now,
        );
        assert_eq!(render(&state, now)["alerts"], json!([]));
        update(&mut state.manifest, Err("timeout"), checked, now);
        let status = render(&state, now + STALE_AFTER + Duration::from_secs(1));
        assert_eq!(status["observed_version"], PINNED_VERSION);
        assert_eq!(status["advisory"], true);
        assert_eq!(status["affects_readiness"], false);
        assert_eq!(
            status["alerts"],
            json!([
                "manifest_check_failed",
                "manifest_check_stale",
                "revision_check_stale"
            ])
        );
    }
    #[test]
    fn drift_alert_does_not_mutate_pinned_identity() {
        let mut state = State::default();
        let now = Instant::now();
        update(&mut state.manifest, Ok("2.20.0".into()), Utc::now(), now);
        update(
            &mut state.revision,
            Ok("0123456789abcdef0123456789abcdef01234567".into()),
            Utc::now(),
            now,
        );
        let status = render(&state, now);
        assert_eq!(status["pinned_version"], PINNED_VERSION);
        assert_eq!(status["pinned_revision"], PINNED_REVISION);
        assert_eq!(
            status["alerts"],
            json!(["manifest_version_changed", "upstream_revision_changed"])
        );
    }
    #[test]
    fn oversized_metadata_is_rejected_before_parsing() {
        assert_eq!(
            parse_manifest(&"x".repeat(MAX_BYTES + 1)),
            Err("body_too_large")
        );
        assert_eq!(
            parse_revision(&vec![b'x'; MAX_BYTES + 1]),
            Err("body_too_large")
        );
    }
}
