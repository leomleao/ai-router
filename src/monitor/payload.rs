//! Bounded, memory-only call inspection, separate from persisted metadata.
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_DETAILS: usize = 100;
pub const MAX_PAYLOAD_BYTES: usize = 128 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapturedPayload {
    pub json: String,
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestDetail {
    pub request_id: String,
    pub endpoint: String,
    pub request: CapturedPayload,
    pub response: Option<CapturedPayload>,
}

impl CapturedPayload {
    pub fn capture(value: &Value) -> Self {
        let mut value = value.clone();
        redact(&mut value, 0);
        let pretty =
            |value: &Value| serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".into());
        let mut json = pretty(&value);
        let mut truncated = false;
        // Shorten long strings before cutting bytes, so a large conversation
        // stays valid JSON with its tool list, item types and order intact.
        for limit in [8192, 2048, 512, 128] {
            if json.len() <= MAX_PAYLOAD_BYTES {
                break;
            }
            shorten_strings(&mut value, limit, 0);
            json = pretty(&value);
            truncated = true;
        }
        if json.len() > MAX_PAYLOAD_BYTES {
            truncated = true;
            let mut end = MAX_PAYLOAD_BYTES;
            while !json.is_char_boundary(end) {
                end -= 1;
            }
            json.truncate(end);
        }
        Self { json, truncated }
    }
}

/// Keep the first `limit` bytes of each longer string and note what was cut.
fn shorten_strings(value: &mut Value, limit: usize, depth: usize) {
    if depth > 64 {
        return;
    }
    match value {
        Value::String(text) if text.len() > limit => {
            let mut end = limit;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let omitted = text.len() - end;
            text.truncate(end);
            text.push_str(&format!("…[{omitted} bytes omitted]"));
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| shorten_strings(item, limit, depth + 1)),
        Value::Object(map) => map
            .values_mut()
            .for_each(|item| shorten_strings(item, limit, depth + 1)),
        _ => {}
    }
}

fn redact(value: &mut Value, depth: usize) {
    if depth > 64 {
        *value = Value::String("[depth limit]".into());
        return;
    }
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                let key: String = key
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .flat_map(char::to_lowercase)
                    .collect();
                if matches!(
                    key.as_str(),
                    "authorization"
                        | "cookie"
                        | "setcookie"
                        | "password"
                        | "passwd"
                        | "secret"
                        | "apikey"
                        | "token"
                        | "accesstoken"
                        | "refreshtoken"
                        | "idtoken"
                        | "idtokenhint"
                        | "encryptedcontent"
                        | "codeverifier"
                        | "pkceverifier"
                        | "authorizationcode"
                        | "oauthcode"
                        | "devicecode"
                        | "nonce"
                        | "oauthnonce"
                        | "state"
                        | "oauthstate"
                        | "clientsecret"
                        | "credentials"
                ) || key.ends_with("password")
                    || key.ends_with("apikey")
                    || key.ends_with("secret")
                {
                    *value = Value::String("[redacted]".into());
                } else if key == "database64" {
                    *value = Value::String("[file bytes omitted]".into());
                } else {
                    redact(value, depth + 1);
                }
            }
        }
        Value::Array(items) => {
            for value in items {
                redact(value, depth + 1);
            }
        }
        Value::String(text)
            if text
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("https://auth.openai.com") =>
        {
            *text = "[redacted OAuth URL]".into();
        }
        Value::String(text) if matches!(text.trim_start().chars().next(), Some('{' | '[')) => {
            if let Ok(mut embedded) = serde_json::from_str::<Value>(text) {
                let original = embedded.clone();
                redact(&mut embedded, depth + 1);
                if embedded != original {
                    *text = embedded.to_string();
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::Telemetry;
    use serde_json::json;

    #[tokio::test]
    async fn details_are_bounded_redacted_and_never_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let telemetry = Telemetry::open(dir.path(), 30, 100).unwrap();
        let request = json!({"input":"synthetic private prompt", "json_input":"{\n  \"city\": \"London\"\n}", "api_key":"dummy-key", "max_output_tokens":12,
            "arguments":"{\"password\":\"dummy-password\",\"city\":\"London\"}", "data_base64":"dummy-bytes"});
        for i in 0..=MAX_DETAILS {
            telemetry
                .capture_request(&format!("req-{i}"), "/v1/responses", &request)
                .await;
        }
        assert!(telemetry.request_detail("req-0").await.is_none());
        let detail = telemetry.request_detail("req-100").await.unwrap();
        assert!(detail.request.json.contains("synthetic private prompt"));
        assert!(detail.request.json.contains("London"));
        assert!(
            !detail.request.json.contains("dummy-key")
                && !detail.request.json.contains("dummy-password")
        );
        assert!(
            detail.request.json.contains("max_output_tokens")
                && detail.request.json.contains("[file bytes omitted]")
        );
        assert_eq!(
            serde_json::from_str::<Value>(&detail.request.json).unwrap()["json_input"],
            request["json_input"]
        );
        let payload = CapturedPayload::capture(&json!({"output":"界".repeat(MAX_PAYLOAD_BYTES)}));
        assert!(payload.truncated && payload.json.len() <= MAX_PAYLOAD_BYTES);
        telemetry.finish_detail("req-100", Some(payload)).await;
        assert!(
            telemetry
                .request_detail("req-100")
                .await
                .unwrap()
                .response
                .unwrap()
                .truncated
        );
        telemetry.flush().await;
        assert!(
            !std::fs::read_to_string(dir.path().join("telemetry.json"))
                .unwrap()
                .contains("synthetic private prompt")
        );
        assert!(
            Telemetry::open(dir.path(), 30, 100)
                .unwrap()
                .request_detail("req-100")
                .await
                .is_none()
        );
    }

    #[test]
    fn oversized_payloads_keep_valid_json_structure_before_byte_truncation() {
        let tools: Vec<_> = (0..40)
            .map(|i| json!({"type":"function","name":format!("tool_{i}"),"parameters":{"description":"d".repeat(4000)}}))
            .collect();
        let request = json!({"model":"gpt-6-luna","input":[{"role":"user","content":"x".repeat(200_000)}],"tools":tools});
        let payload = CapturedPayload::capture(&request);
        assert!(payload.truncated && payload.json.len() <= MAX_PAYLOAD_BYTES);
        let parsed: Value = serde_json::from_str(&payload.json).unwrap();
        assert_eq!(parsed["tools"].as_array().unwrap().len(), 40);
        assert_eq!(parsed["tools"][39]["name"], "tool_39");
        assert!(
            parsed["input"][0]["content"]
                .as_str()
                .unwrap()
                .contains("bytes omitted]")
        );
        let small = CapturedPayload::capture(&json!({"input":"short"}));
        assert!(!small.truncated);
    }

    #[test]
    fn redacts_oauth_material_and_authorization_urls_without_changing_prompt_text() {
        let payload = CapturedPayload::capture(&json!({
            "code_verifier": "synthetic-verifier",
            "authorization_code": "synthetic-code",
            "id_token_hint": "synthetic-id-token",
            "nonce": "synthetic-nonce",
            "encrypted_content": "synthetic-encrypted-content",
            "callback": "https://auth.openai.com/authorize?code=synthetic-code&id_token_hint=synthetic-id-token",
            "input": "Keep this ordinary prompt intact."
        }));
        for secret in [
            "synthetic-verifier",
            "synthetic-code",
            "synthetic-id-token",
            "synthetic-nonce",
            "synthetic-encrypted-content",
            "auth.openai.com",
        ] {
            assert!(!payload.json.contains(secret), "leaked {secret}");
        }
        assert!(payload.json.contains("[redacted OAuth URL]"));
        assert!(payload.json.contains("Keep this ordinary prompt intact."));
    }
}
