//! Client-scoped Responses output items. Never persisted or sourced from telemetry.
use crate::protocol::ApiError;
use serde_json::Value;
use std::{
    collections::{HashSet, VecDeque},
    sync::Mutex,
    time::{Duration, Instant},
};

pub const TTL_SECONDS: u64 = 24 * 60 * 60;
pub const MAX_ITEMS: usize = 4096;
pub const MAX_BYTES: usize = 64 * 1024 * 1024;
pub const CLIENT_ITEMS: usize = 1024;
pub const CLIENT_BYTES: usize = 16 * 1024 * 1024;

struct Entry {
    scope: Option<String>,
    owner: String,
    id: String,
    item: Value,
    bytes: usize,
    created: Instant,
}
#[derive(Default)]
struct State {
    entries: VecDeque<Entry>,
    bytes: usize,
}

pub struct ItemStore {
    state: Mutex<State>,
    ttl: Duration,
    max_items: usize,
    max_bytes: usize,
    client_items: usize,
    client_bytes: usize,
}
impl Default for ItemStore {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            ttl: Duration::from_secs(TTL_SECONDS),
            max_items: MAX_ITEMS,
            max_bytes: MAX_BYTES,
            client_items: CLIENT_ITEMS,
            client_bytes: CLIENT_BYTES,
        }
    }
}
impl State {
    fn remove(&mut self, index: usize) {
        if let Some(entry) = self.entries.remove(index) {
            self.bytes -= entry.bytes;
        }
    }
    fn prune(&mut self, now: Instant, ttl: Duration) {
        self.entries
            .retain(|e| now.saturating_duration_since(e.created) < ttl);
        self.bytes = self.entries.iter().map(|e| e.bytes).sum();
    }
    fn usage(&self, owner: &str) -> (usize, usize) {
        self.entries
            .iter()
            .filter(|e| e.owner == owner)
            .fold((0, 0), |(count, bytes), e| (count + 1, bytes + e.bytes))
    }
}
impl ItemStore {
    pub fn prune(&self) {
        self.state.lock().unwrap().prune(Instant::now(), self.ttl);
    }

    /// Only completed, router-generated output items enter this store.
    pub fn save(&self, owner: &str, output: &Value) -> Result<(), ApiError> {
        self.save_at(owner, output, Instant::now())
    }
    pub fn save_scoped(&self, owner: &str, output: &Value, scope: &str) -> Result<(), ApiError> {
        self.save_scoped_at(owner, output, Instant::now(), Some(scope))
    }
    fn save_at(&self, owner: &str, output: &Value, now: Instant) -> Result<(), ApiError> {
        self.save_scoped_at(owner, output, now, None)
    }
    fn save_scoped_at(
        &self,
        owner: &str,
        output: &Value,
        now: Instant,
        scope: Option<&str>,
    ) -> Result<(), ApiError> {
        let items = output
            .get("output")
            .and_then(Value::as_array)
            .ok_or_else(|| ApiError::internal("Invalid response output"))?;
        let mut ids = HashSet::new();
        let mut incoming = Vec::new();
        let mut bytes = 0;
        for item in items {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::internal("Invalid response item ID"))?;
            if !ids.insert(id) {
                return Err(ApiError::internal("Duplicate response item ID"));
            }
            let size = size(item)?;
            bytes += size;
            incoming.push(Entry {
                scope: scope.map(str::to_owned),
                owner: owner.into(),
                id: id.into(),
                item: item.clone(),
                bytes: size,
                created: now,
            });
        }
        // Reject oversized groups atomically; never publish dangling references.
        if bytes > self.client_bytes.min(self.max_bytes)
            || incoming.len() > self.client_items.min(self.max_items)
        {
            return Err(ApiError::new(
                503,
                "response_storage_limit",
                "Response exceeds item storage limits; retry with store:false",
            ));
        }
        let mut state = self.state.lock().unwrap();
        state.prune(now, self.ttl);
        if state
            .entries
            .iter()
            .any(|e| e.owner == owner && ids.contains(e.id.as_str()))
        {
            return Err(ApiError::internal("Response item IDs are immutable"));
        }
        loop {
            let (count, used) = state.usage(owner);
            if count + incoming.len() <= self.client_items && used + bytes <= self.client_bytes {
                break;
            }
            let index = state.entries.iter().position(|e| e.owner == owner).unwrap();
            state.remove(index);
        }
        while state.entries.len() + incoming.len() > self.max_items
            || state.bytes + bytes > self.max_bytes
        {
            state.remove(0);
        }
        state.bytes += bytes;
        state.entries.extend(incoming);
        Ok(())
    }

    pub fn resolve(&self, owner: &str, body: &mut Value, max_bytes: usize) -> Result<(), ApiError> {
        self.resolve_at(owner, body, max_bytes, Instant::now())
    }
    pub fn resolve_scoped(
        &self,
        owner: &str,
        body: &mut Value,
        max_bytes: usize,
        scope: &str,
    ) -> Result<(), ApiError> {
        self.resolve_scoped_at(owner, body, max_bytes, Instant::now(), Some(scope))
    }
    fn resolve_at(
        &self,
        owner: &str,
        body: &mut Value,
        max_bytes: usize,
        now: Instant,
    ) -> Result<(), ApiError> {
        self.resolve_scoped_at(owner, body, max_bytes, now, None)
    }
    fn resolve_scoped_at(
        &self,
        owner: &str,
        body: &mut Value,
        max_bytes: usize,
        now: Instant,
        scope: Option<&str>,
    ) -> Result<(), ApiError> {
        let mut bytes = size(body)?;
        let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
            return Ok(());
        };
        if items.len() > 1000 {
            return Err(ApiError::bad_request("Too many input items"));
        }
        let mut state = self.state.lock().unwrap();
        state.prune(now, self.ttl);
        for item in items {
            let implicit = item.get("type").is_none_or(Value::is_null)
                && item.get("id").is_some()
                && item.get("role").is_none();
            if item.get("type").and_then(Value::as_str) != Some("item_reference") && !implicit {
                continue;
            }
            let map = item
                .as_object()
                .ok_or_else(|| ApiError::bad_request("Invalid item reference"))?;
            if map
                .iter()
                .any(|(k, v)| k != "id" && k != "type" && !v.is_null())
            {
                return Err(ApiError::bad_request(
                    "Item references may contain only type and id",
                ));
            }
            let id = map
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| {
                    !id.is_empty() && id.len() <= 256 && id.bytes().all(|b| b.is_ascii_graphic())
                })
                .ok_or_else(|| ApiError::bad_request("Item reference requires a valid ID"))?;
            // Unknown, expired, evicted and foreign IDs are indistinguishable.
            let entry = state.entries.iter().find(|e| e.owner == owner && e.id == id && (e.item["type"] != "reasoning" || e.scope.as_deref() == scope))
                .ok_or_else(|| ApiError::new(400, "item_reference_not_found", "Referenced item is unavailable; resend full history or start a new conversation"))?;
            bytes = bytes
                .saturating_sub(size(item)?)
                .saturating_add(entry.bytes);
            if bytes > max_bytes {
                return Err(ApiError::new(
                    413,
                    "input_too_large",
                    "Expanded input exceeds the request body limit",
                ));
            }
            *item = entry.item.clone();
        }
        Ok(())
    }
}

#[cfg(test)]
mod provider_scope_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn opaque_references_require_same_client_provider_and_registration() {
        let store = ItemStore::default();
        let output = json!({"output":[{"id":"rs_synthetic","type":"reasoning","summary":[],"encrypted_content":"synthetic-opaque"}]});
        store
            .save_scoped("client-a", &output, "openai:generation-a")
            .unwrap();
        for (owner, scope) in [
            ("client-b", "openai:generation-a"),
            ("client-a", "agy:agy"),
            ("client-a", "openai:generation-b"),
        ] {
            let mut body = json!({"input":[{"type":"item_reference","id":"rs_synthetic"}]});
            assert_eq!(
                store
                    .resolve_scoped(owner, &mut body, MAX_BYTES, scope)
                    .unwrap_err()
                    .code,
                "item_reference_not_found"
            );
        }
        let mut body = json!({"input":[{"type":"item_reference","id":"rs_synthetic"}]});
        store
            .resolve_scoped("client-a", &mut body, MAX_BYTES, "openai:generation-a")
            .unwrap();
        assert_eq!(body["input"][0]["encrypted_content"], "synthetic-opaque");
    }
}
fn size(value: &Value) -> Result<usize, ApiError> {
    serde_json::to_vec(value)
        .map(|v| v.len())
        .map_err(|_| ApiError::internal("Could not measure response items"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn output(id: &str, text: &str) -> Value {
        json!({"output":[{"id":id,"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}]})
    }
    fn reference(id: &str) -> Value {
        json!({"input":[{"type":"item_reference","id":id}]})
    }
    fn code(store: &ItemStore, owner: &str, id: &str, now: Instant) -> String {
        store
            .resolve_at(owner, &mut reference(id), MAX_BYTES, now)
            .unwrap_err()
            .code
    }
    #[test]
    fn exact_items_are_owned_expire_and_do_not_survive_a_new_store() {
        let store = ItemStore::default();
        let now = Instant::now();
        let output = output("msg_1", "{\"password\":\"synthetic must not be redacted\"}");
        store.save_at("a", &output, now).unwrap();
        let mut body = reference("msg_1");
        store.resolve_at("a", &mut body, MAX_BYTES, now).unwrap();
        assert_eq!(body["input"], output["output"]);
        assert_eq!(code(&store, "b", "msg_1", now), "item_reference_not_found");
        assert_eq!(
            code(&store, "a", "unknown", now),
            "item_reference_not_found"
        );
        assert_eq!(
            code(&ItemStore::default(), "a", "msg_1", now),
            "item_reference_not_found"
        );
        assert_eq!(
            code(&store, "a", "msg_1", now + store.ttl),
            "item_reference_not_found"
        );
        assert_eq!(store.state.lock().unwrap().bytes, 0);
    }
    #[test]
    fn client_and_global_count_and_byte_limits_evict_oldest_items() {
        let now = Instant::now();
        let item_bytes = size(&output("m1", "text")["output"][0]).unwrap();
        for byte_limited in [false, true] {
            let store = ItemStore {
                max_items: if byte_limited { 99 } else { 3 },
                client_items: if byte_limited { 99 } else { 2 },
                max_bytes: if byte_limited {
                    item_bytes * 3
                } else {
                    MAX_BYTES
                },
                client_bytes: if byte_limited {
                    item_bytes * 2
                } else {
                    CLIENT_BYTES
                },
                ..ItemStore::default()
            };
            for (owner, id) in [("a", "m1"), ("b", "m2"), ("a", "m3"), ("a", "m4")] {
                store.save_at(owner, &output(id, "text"), now).unwrap();
            }
            assert_eq!(code(&store, "a", "m1", now), "item_reference_not_found");
            store
                .resolve_at("b", &mut reference("m2"), MAX_BYTES, now)
                .unwrap();
            store.save_at("c", &output("m5", "text"), now).unwrap();
            assert_eq!(code(&store, "b", "m2", now), "item_reference_not_found");
            let state = store.state.lock().unwrap();
            assert_eq!(state.entries.len(), 3);
            assert_eq!(state.bytes, item_bytes * 3);
        }
    }
    #[test]
    fn oversized_writes_are_atomic_and_reference_expansion_is_bounded() {
        let store = ItemStore {
            client_bytes: 512,
            ..ItemStore::default()
        };
        let now = Instant::now();
        store
            .save_at("a", &output("msg_1", &"x".repeat(100)), now)
            .unwrap();
        let error = store
            .save_at("a", &output("large", &"x".repeat(1000)), now)
            .unwrap_err();
        assert_eq!(error.code, "response_storage_limit");
        store
            .resolve_at("a", &mut reference("msg_1"), MAX_BYTES, now)
            .unwrap();
        let mut body = json!({"input":[{"type":"item_reference","id":"msg_1"},{"type":"item_reference","id":"msg_1"}]});
        assert_eq!(
            store
                .resolve_at("a", &mut body, 300, now)
                .unwrap_err()
                .status,
            413
        );
        assert_eq!(store.state.lock().unwrap().entries.len(), 1);
    }
    #[test]
    fn reference_shapes_are_validated_and_implicit_references_work() {
        let store = ItemStore::default();
        let now = Instant::now();
        let output = output("msg_1", "exact");
        store.save_at("a", &output, now).unwrap();
        for item in [json!({"id":"msg_1"}), json!({"type":null,"id":"msg_1"})] {
            let mut body = json!({"input":[item]});
            store.resolve_at("a", &mut body, MAX_BYTES, now).unwrap();
            assert_eq!(body["input"], output["output"]);
        }
        for item in [
            json!({"type":"item_reference"}),
            json!({"type":"item_reference","id":true}),
            json!({"type":"item_reference","id":"msg_1","role":"system"}),
            json!({"type":"item_reference","id":"msg_1","content":"override"}),
        ] {
            assert_eq!(
                store
                    .resolve_at("a", &mut json!({"input":[item]}), MAX_BYTES, now)
                    .unwrap_err()
                    .code,
                "invalid_request"
            );
        }
        assert!(store.save_at("a", &output, now).is_err());
    }
}
