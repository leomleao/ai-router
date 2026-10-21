//! Exact model ownership, readiness, and bounded historical ownership.
//!
//! Provider probes publish independently. A registry view takes a coherent pair
//! of published statuses and never infers a provider from a model-name prefix.
//! The only prefix rule is the operator-configured fallback, which maps IDs the
//! registry has never seen onto one of two exact catalogue IDs.
//!
//! Historical ownership is persisted, so a model the router has served keeps
//! reporting its own outage after a restart instead of being cross-served.
use crate::protocol::{ApiError, ModelInfo, ProviderStatus};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const MAX_SNAPSHOT_AGE_SECONDS: i64 = 180;
const MAX_KNOWN_MODELS: usize = 4096;
/// File name of the remembered-ownership store inside the telemetry directory.
pub const KNOWN_MODELS_FILE: &str = "known-models.json";
const MAX_KNOWN_STORE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderId {
    Agy,
    OpenAi,
}
impl ProviderId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agy => "agy",
            Self::OpenAi => "openai",
        }
    }
    pub fn owned_by(self) -> &'static str {
        match self {
            Self::Agy => "antigravity",
            Self::OpenAi => "openai",
        }
    }
}
#[derive(Clone, Debug)]
pub struct ModelEntry {
    pub model: ModelInfo,
    pub provider: ProviderId,
}
#[derive(Default)]
pub struct Registry {
    known: HashMap<String, ProviderId>,
    order: VecDeque<String>,
    store: Option<PathBuf>,
}
pub struct Snapshot {
    pub entries: Vec<ModelEntry>,
    pub conflicts: HashSet<String>,
    hidden: HashMap<String, ProviderId>,
}
/// The target chosen for an unseen model ID. `crossed` means the first-choice
/// target was not live, so a provider other than the usual one serves it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FallbackTarget<'a> {
    pub served: &'a str,
    pub crossed: bool,
}
pub fn ready(status: &ProviderStatus, now: DateTime<Utc>) -> bool {
    status.authenticated
        && DateTime::parse_from_rfc3339(&status.checked_at).is_ok_and(|checked| {
            let age = now
                .signed_duration_since(checked.with_timezone(&Utc))
                .num_seconds();
            (0..=MAX_SNAPSHOT_AGE_SECONDS).contains(&age)
        })
}
impl Registry {
    /// Restores remembered ownership from `path` and keeps it current there.
    /// A missing, unreadable, or corrupt store starts empty: it only narrows
    /// fallback, so it must never become a dependency of serving.
    pub fn open(path: PathBuf) -> Self {
        let mut registry = Self::default();
        match load_known(&path) {
            Ok(saved) => {
                for (model, provider) in saved {
                    if crate::config::valid_model_id(&model) {
                        registry.learn(&model, provider);
                    }
                }
                registry.trim();
            }
            Err(reason) => eprintln!(
                "{}",
                serde_json::json!({"event":"known_models_store_ignored","reason":reason})
            ),
        }
        registry.store = Some(path);
        registry
    }
    fn learn(&mut self, model: &str, provider: ProviderId) -> bool {
        match self.known.insert(model.to_owned(), provider) {
            None => {
                self.order.push_back(model.to_owned());
                true
            }
            Some(previous) => previous != provider,
        }
    }
    fn trim(&mut self) {
        while self.order.len() > MAX_KNOWN_MODELS {
            if let Some(model) = self.order.pop_front() {
                self.known.remove(&model);
            }
        }
    }
    fn persist(&self) {
        let Some(path) = &self.store else {
            return;
        };
        let saved: Vec<(&str, ProviderId)> = self
            .order
            .iter()
            .filter_map(|model| Some((model.as_str(), *self.known.get(model)?)))
            .collect();
        if store_known(path, &saved).is_err() {
            eprintln!(
                "{}",
                serde_json::json!({"event":"known_models_store_failed"})
            );
        }
    }
    pub fn remember(&mut self, provider: ProviderId, status: &ProviderStatus) {
        let mut changed = false;
        for model in &status.models {
            changed |= self.learn(&model.id, provider);
        }
        self.trim();
        if changed {
            self.persist();
        }
    }
    pub fn snapshot(
        &mut self,
        agy: &ProviderStatus,
        openai: &ProviderStatus,
        openai_enabled: bool,
        now: DateTime<Utc>,
    ) -> Snapshot {
        let mut owners: HashMap<&str, HashSet<ProviderId>> = HashMap::new();
        for (id, status, enabled) in [
            (ProviderId::Agy, agy, true),
            (ProviderId::OpenAi, openai, openai_enabled),
        ] {
            if enabled {
                for model in &status.models {
                    owners.entry(&model.id).or_default().insert(id);
                }
            }
        }
        let conflicts: HashSet<String> = owners
            .iter()
            .filter(|(_, ids)| ids.len() > 1)
            .map(|(model, _)| (*model).to_owned())
            .collect();
        let mut changed = false;
        for (model, ids) in &owners {
            if ids.len() == 1 {
                changed |= self.learn(model, *ids.iter().next().unwrap());
            }
        }
        self.trim();
        if changed {
            self.persist();
        }
        let mut entries = Vec::new();
        for (id, status, enabled) in [
            (ProviderId::Agy, agy, true),
            (ProviderId::OpenAi, openai, openai_enabled),
        ] {
            if enabled && ready(status, now) {
                let mut seen = HashSet::new();
                entries.extend(
                    status
                        .models
                        .iter()
                        .filter(|model| {
                            !conflicts.contains(&model.id) && seen.insert(model.id.clone())
                        })
                        .map(|model| ModelEntry {
                            model: model.clone(),
                            provider: id,
                        }),
                );
            }
        }
        Snapshot {
            entries,
            conflicts,
            hidden: self.known.clone(),
        }
    }
}

fn load_known(path: &Path) -> Result<Vec<(String, ProviderId)>, &'static str> {
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err("unreadable"),
    };
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Err("not_a_regular_file");
    }
    let mut data = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_KNOWN_STORE_BYTES + 1)
        .read_to_end(&mut data)
        .map_err(|_| "unreadable")?;
    if data.len() as u64 > MAX_KNOWN_STORE_BYTES {
        return Err("too_large");
    }
    serde_json::from_slice(&data).map_err(|_| "corrupt")
}

fn store_known(path: &Path, saved: &[(&str, ProviderId)]) -> Result<(), ()> {
    let tmp = path.with_file_name(format!("known-models-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let data = serde_json::to_vec(saved).map_err(|_| ())?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)
            .map_err(|_| ())?;
        file.write_all(&data).map_err(|_| ())?;
        file.sync_all().map_err(|_| ())?;
        std::fs::rename(&tmp, path).map_err(|_| ())
    })();
    let _ = std::fs::remove_file(tmp);
    result
}

/// Non-chat model families. A misdirected embedding, image, audio, or
/// moderation client must get a configuration error, not paid chat output.
fn non_chat_model(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    [
        "text-embedding-",
        "dall-e",
        "gpt-image",
        "whisper",
        "tts-",
        "omni-moderation",
    ]
    .iter()
    .any(|prefix| model.starts_with(prefix))
        || model
            .split('-')
            .any(|part| matches!(part, "transcribe" | "realtime" | "audio" | "tts"))
}

impl Snapshot {
    pub fn owner(&self, model: &str) -> Option<ProviderId> {
        self.hidden.get(model).copied()
    }
    /// Select the served ID for a chat model ID the registry has never seen.
    /// Exact live, conflicting, or remembered IDs, and the targets themselves,
    /// keep their own result so a known model's outage is reported rather than
    /// hidden behind another model. A live target is preferred; with neither
    /// live, a remembered target is still selected so its availability error
    /// (with retry guidance) reaches the client. `Ok(None)` leaves the ID to
    /// `resolve`, which reports `model_not_found` for an ID never seen.
    pub fn fallback<'a>(
        &self,
        requested: &str,
        openai: &'a str,
        gemini: &'a str,
    ) -> Result<Option<FallbackTarget<'a>>, ApiError> {
        let seen = self.conflicts.contains(requested)
            || self.hidden.contains_key(requested)
            || self.live(requested);
        if (requested == openai || requested == gemini) && !seen {
            // A configured target that no catalogue has published yet (first
            // start during its provider's outage) is unavailable, not unknown.
            return Err(crate::api::error(
                503,
                "provider_unavailable",
                "The configured fallback target is not in a live provider catalogue",
            ));
        }
        if seen || !crate::config::valid_model_id(requested) {
            return Ok(None);
        }
        if non_chat_model(requested) {
            return Err(crate::api::error(
                400,
                "model_not_found",
                "This router serves chat models only; send embedding, image, audio, and moderation requests to a service that provides them",
            ));
        }
        let gemini_first = requested
            .get(..6)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("gemini"));
        let order = if gemini_first {
            [gemini, openai]
        } else {
            [openai, gemini]
        };
        let pick = |position: usize| FallbackTarget {
            served: order[position],
            crossed: position == 1,
        };
        Ok((0..2)
            .find(|&position| self.live(order[position]))
            .or_else(|| (0..2).find(|&position| self.hidden.contains_key(order[position])))
            .map(pick))
    }
    fn live(&self, model: &str) -> bool {
        self.entries.iter().any(|entry| entry.model.id == model)
    }
    pub fn resolve(&self, model: &str) -> Result<ProviderId, ApiError> {
        if self.conflicts.contains(model) {
            return Err(crate::api::error(
                503,
                "model_conflict",
                "The model ID is advertised by multiple providers",
            ));
        }
        if let Some(entry) = self.entries.iter().find(|entry| entry.model.id == model) {
            return Ok(entry.provider);
        }
        if let Some(provider) = self.hidden.get(model) {
            return Err(crate::api::error(
                503,
                "provider_unavailable",
                &format!("{} is unavailable for this model", provider.as_str()),
            ));
        }
        Err(crate::api::error(
            400,
            "model_not_found",
            "Choose a model returned by /v1/models",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn status(ids: &[&str], now: DateTime<Utc>) -> ProviderStatus {
        ProviderStatus {
            authenticated: true,
            checked_at: now.to_rfc3339(),
            models: ids
                .iter()
                .map(|id| ModelInfo {
                    id: (*id).into(),
                    name: (*id).into(),
                })
                .collect(),
            ..Default::default()
        }
    }
    #[test]
    fn exact_union_conflicts_and_no_prefix_inference() {
        let now = Utc::now();
        let mut registry = Registry::default();
        let snapshot = registry.snapshot(
            &status(&["gpt-owned-by-agy", "duplicate"], now),
            &status(&["other", "duplicate"], now),
            true,
            now,
        );
        assert_eq!(snapshot.entries.len(), 2);
        assert_eq!(
            snapshot.resolve("gpt-owned-by-agy").unwrap(),
            ProviderId::Agy
        );
        assert_eq!(snapshot.resolve("other").unwrap(), ProviderId::OpenAi);
        assert_eq!(
            snapshot.resolve("duplicate").unwrap_err().code,
            "model_conflict"
        );
        assert_eq!(
            snapshot.resolve("unknown").unwrap_err().code,
            "model_not_found"
        );
    }
    #[test]
    fn fallback_maps_only_unseen_ids_and_crosses_to_the_available_target() {
        let now = Utc::now();
        let mut registry = Registry::default();
        let agy = status(&["gemini-3.8-flash-medium", "claude-x-high"], now);
        let openai = status(&["gpt-6-luna", "gpt-exact"], now);
        let both = registry.snapshot(&agy, &openai, true, now);
        let pick = |s: &Snapshot, id: &str| {
            s.fallback(id, "gpt-6-luna", "gemini-3.8-flash-medium")
                .unwrap()
                .map(|target| target.served)
        };
        assert_eq!(pick(&both, "gpt-5.4-nano"), Some("gpt-6-luna"));
        assert_eq!(pick(&both, "gpt-5.3-codex"), Some("gpt-6-luna"));
        assert_eq!(pick(&both, "o3"), Some("gpt-6-luna"));
        assert_eq!(pick(&both, "claude-unknown"), Some("gpt-6-luna"));
        assert_eq!(
            pick(&both, "gemini-9-ultra"),
            Some("gemini-3.8-flash-medium")
        );
        assert_eq!(pick(&both, "GeMiNi-9"), Some("gemini-3.8-flash-medium"));
        for exact in [
            "gpt-exact",
            "claude-x-high",
            "gpt-6-luna",
            "gemini-3.8-flash-medium",
        ] {
            assert_eq!(pick(&both, exact), None, "{exact}");
        }
        assert_eq!(pick(&both, ""), None);
        // OpenAI disabled: its remembered IDs keep provider_unavailable, while
        // unseen IDs cross over to the Gemini target.
        let disabled = registry.snapshot(&agy, &openai, false, now);
        assert_eq!(
            pick(&disabled, "gpt-5.4-nano"),
            Some("gemini-3.8-flash-medium")
        );
        assert_eq!(pick(&disabled, "gpt-exact"), None);
        assert_eq!(
            disabled.resolve("gpt-exact").unwrap_err().code,
            "provider_unavailable"
        );
        let no_gemini = Registry::default().snapshot(&status(&["other"], now), &openai, true, now);
        assert_eq!(pick(&no_gemini, "gemini-9-ultra"), Some("gpt-6-luna"));
        let neither =
            Registry::default().snapshot(&status(&["other"], now), &status(&[], now), true, now);
        assert_eq!(pick(&neither, "gpt-5.4-nano"), None);
        assert_eq!(pick(&neither, "gemini-9-ultra"), None);
    }
    #[test]
    fn stale_hidden_removed_and_disabled_models_never_reroute() {
        let now = Utc::now();
        let mut registry = Registry::default();
        let agy = status(&["a"], now);
        let openai = status(&["o"], now);
        registry.snapshot(&agy, &openai, true, now);
        let stale = registry.snapshot(&agy, &openai, true, now + chrono::Duration::seconds(181));
        assert!(stale.entries.is_empty());
        assert_eq!(stale.resolve("o").unwrap_err().code, "provider_unavailable");
        let disabled = registry.snapshot(&agy, &openai, false, now);
        assert_eq!(disabled.entries.len(), 1);
        assert_eq!(
            disabled.resolve("o").unwrap_err().code,
            "provider_unavailable"
        );
    }
    #[test]
    fn outage_retains_hidden_ownership_and_other_provider() {
        let now = Utc::now();
        let mut registry = Registry::default();
        let a = status(&["a"], now);
        registry.snapshot(&a, &status(&["o"], now), true, now);
        let snapshot = registry.snapshot(&a, &ProviderStatus::default(), true, now);
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(
            snapshot.resolve("o").unwrap_err().code,
            "provider_unavailable"
        );
        assert_eq!(snapshot.resolve("a").unwrap(), ProviderId::Agy);
    }

    fn targets(s: &Snapshot, id: &str) -> Result<Option<FallbackTarget<'static>>, ApiError> {
        s.fallback(id, "gpt-6-luna", "gemini-3.8-flash-medium")
    }
    #[test]
    fn targets_never_fall_back_and_remembered_targets_report_their_outage() {
        let now = Utc::now();
        let agy = status(&["gemini-3.8-flash-medium"], now);
        let openai = status(&["gpt-6-luna"], now);
        // A fresh registry whose OpenAI probe failed has never seen the
        // OpenAI target, but the target ID itself is never cross-served: it
        // is reported unavailable, with retry guidance.
        let mut fresh = Registry::default();
        let failed = fresh.snapshot(&agy, &status(&[], now), true, now);
        let error = targets(&failed, "gpt-6-luna").unwrap_err();
        assert_eq!(
            (error.status, error.code.as_str()),
            (503, "provider_unavailable")
        );
        assert_eq!(error.retry_header(), Some(("retry-after", "30")));
        let agy_down = fresh.snapshot(&status(&[], now), &openai, true, now);
        assert_eq!(targets(&agy_down, "gemini-3.8-flash-medium").unwrap(), None);
        // Neither target live: a remembered target is still selected, first
        // choice first, so its 503 reaches the client with the disclosure.
        let mut registry = Registry::default();
        registry.snapshot(&agy, &openai, true, now);
        let down = registry.snapshot(&status(&[], now), &status(&[], now), true, now);
        assert_eq!(
            targets(&down, "gpt-5.4-nano").unwrap(),
            Some(FallbackTarget {
                served: "gpt-6-luna",
                crossed: false
            })
        );
        assert_eq!(
            targets(&down, "gemini-9").unwrap().unwrap().served,
            "gemini-3.8-flash-medium"
        );
        assert_eq!(
            down.resolve("gpt-6-luna").unwrap_err().code,
            "provider_unavailable"
        );
        // Only the second target remembered: it is selected, as a crossing.
        let mut only_agy = Registry::default();
        only_agy.snapshot(&agy, &status(&[], now), true, now);
        let down = only_agy.snapshot(&status(&[], now), &status(&[], now), true, now);
        assert_eq!(
            targets(&down, "gpt-5.4-nano").unwrap(),
            Some(FallbackTarget {
                served: "gemini-3.8-flash-medium",
                crossed: true
            })
        );
        // A live target still wins over a remembered first choice.
        let openai_down = registry.snapshot(&agy, &status(&[], now), true, now);
        assert_eq!(
            targets(&openai_down, "gpt-5.4-nano").unwrap(),
            Some(FallbackTarget {
                served: "gemini-3.8-flash-medium",
                crossed: true
            })
        );
    }
    #[test]
    fn fallback_rejects_invalid_and_non_chat_model_ids() {
        let now = Utc::now();
        let both = Registry::default().snapshot(
            &status(&["gemini-3.8-flash-medium"], now),
            &status(&["gpt-6-luna"], now),
            true,
            now,
        );
        for junk in [
            "gpt\r\nx",
            "gpt 4o mini",
            "gpt-\u{fc}nicode",
            "<b>gpt</b>",
            "openai/gpt-4o",
            &"x".repeat(129),
        ] {
            assert_eq!(targets(&both, junk).unwrap(), None, "{junk:?}");
            assert_eq!(both.resolve(junk).unwrap_err().code, "model_not_found");
        }
        for non_chat in [
            "text-embedding-3-large",
            "dall-e-3",
            "gpt-image-1",
            "whisper-1",
            "tts-1-hd",
            "gpt-4o-mini-tts",
            "omni-moderation-latest",
            "gpt-4o-transcribe",
            "gpt-4o-mini-transcribe",
            "gpt-4o-realtime-preview",
            "gpt-realtime",
            "gpt-4o-audio-preview",
            "TEXT-EMBEDDING-ADA-002",
        ] {
            let error = targets(&both, non_chat).unwrap_err();
            assert_eq!(error.code, "model_not_found", "{non_chat}");
            assert_eq!(error.status, 400);
            assert!(error.message.contains("chat models only"));
        }
        // Chat IDs that merely contain these words still fall back.
        for chat in ["gpt-4o-mini", "gpt-audiobook-writer", "o4-mini"] {
            assert!(targets(&both, chat).unwrap().is_some(), "{chat}");
        }
    }
    #[test]
    fn remembered_ownership_survives_restart_and_tolerates_bad_stores() {
        let now = Utc::now();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(KNOWN_MODELS_FILE);
        let agy = status(&["gemini-3.8-flash-medium"], now);
        let mut registry = Registry::open(path.clone());
        registry.remember(
            ProviderId::OpenAi,
            &status(&["gpt-6-luna", "gpt-exact"], now),
        );
        registry.snapshot(&agy, &status(&["gpt-6-luna", "gpt-exact"], now), true, now);
        drop(registry);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // Restart during an OpenAI outage: its probe publishes no models.
        let mut restarted = Registry::open(path.clone());
        let snapshot = restarted.snapshot(&agy, &ProviderStatus::default(), true, now);
        for known in ["gpt-6-luna", "gpt-exact"] {
            assert_eq!(targets(&snapshot, known).unwrap(), None);
            assert_eq!(
                snapshot.resolve(known).unwrap_err().code,
                "provider_unavailable"
            );
        }
        assert_eq!(
            targets(&snapshot, "gpt-5.4-nano").unwrap().unwrap().served,
            "gemini-3.8-flash-medium"
        );
        // Corrupt, oversized, or non-file stores start empty and are replaced.
        for bad in [b"not json".to_vec(), vec![b' '; 1024 * 1024 + 1]] {
            std::fs::write(&path, bad).unwrap();
            let mut registry = Registry::open(path.clone());
            assert!(registry.known.is_empty());
            registry.remember(ProviderId::Agy, &agy);
            assert_eq!(Registry::open(path.clone()).known.len(), 1);
        }
        std::fs::write(
            &path,
            r#"[["gpt ok","openai"],["gpt-ok","openai"],["x","martian"]]"#,
        )
        .unwrap();
        assert!(Registry::open(path.clone()).known.is_empty());
        std::fs::write(&path, r#"[["gpt bad","openai"],["gpt-ok","openai"]]"#).unwrap();
        let loaded = Registry::open(path.clone());
        assert_eq!(loaded.known.len(), 1);
        assert_eq!(loaded.known["gpt-ok"], ProviderId::OpenAi);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let mut registry = Registry::open(path.clone());
        assert!(registry.known.is_empty());
        // Persistence failure keeps in-memory ownership.
        registry.remember(ProviderId::Agy, &agy);
        assert_eq!(registry.known.len(), 1);
    }
}
