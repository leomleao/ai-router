//! Exact model ownership, readiness, and bounded historical ownership.
//!
//! Provider probes publish independently. A registry view takes a coherent pair
//! of published statuses and never infers a provider from a model-name prefix.
//! The only prefix rule is the operator-configured fallback, which maps IDs the
//! registry has never seen onto one of two exact live catalogue IDs.
use crate::protocol::{ApiError, ModelInfo, ProviderStatus};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet, VecDeque};

pub const MAX_SNAPSHOT_AGE_SECONDS: i64 = 180;
const MAX_KNOWN_MODELS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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
}
pub struct Snapshot {
    pub entries: Vec<ModelEntry>,
    pub conflicts: HashSet<String>,
    hidden: HashMap<String, ProviderId>,
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
    pub fn remember(&mut self, provider: ProviderId, status: &ProviderStatus) {
        for model in &status.models {
            if !self.known.contains_key(&model.id) {
                self.order.push_back(model.id.clone());
            }
            self.known.insert(model.id.clone(), provider);
        }
        while self.order.len() > MAX_KNOWN_MODELS {
            if let Some(model) = self.order.pop_front() {
                self.known.remove(&model);
            }
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
        for (model, ids) in &owners {
            if ids.len() == 1 {
                let id = *ids.iter().next().unwrap();
                if !self.known.contains_key(*model) {
                    self.order.push_back((*model).to_owned());
                }
                self.known.insert((*model).to_owned(), id);
            }
        }
        while self.order.len() > MAX_KNOWN_MODELS {
            if let Some(model) = self.order.pop_front() {
                self.known.remove(&model);
            }
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
impl Snapshot {
    pub fn owner(&self, model: &str) -> Option<ProviderId> {
        self.hidden.get(model).copied()
    }
    /// Select the served ID for a model the registry has never seen. Exact
    /// live, conflicting, or remembered IDs keep their own result so a known
    /// model's outage is reported rather than hidden behind another model.
    pub fn fallback<'a>(
        &self,
        requested: &str,
        openai: &'a str,
        gemini: &'a str,
    ) -> Option<&'a str> {
        if requested.trim().is_empty()
            || self.conflicts.contains(requested)
            || self.hidden.contains_key(requested)
            || self.entries.iter().any(|entry| entry.model.id == requested)
        {
            return None;
        }
        let gemini_first = requested
            .get(..6)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("gemini"));
        let order = if gemini_first {
            [gemini, openai]
        } else {
            [openai, gemini]
        };
        order
            .into_iter()
            .find(|target| self.entries.iter().any(|entry| entry.model.id == *target))
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
        let pick = |s: &Snapshot, id: &str| s.fallback(id, "gpt-6-luna", "gemini-3.8-flash-medium");
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
}
