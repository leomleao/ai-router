//! Bounded request constants supplied by the serving account's model roster.
//! Never infer a profile from an unrelated model, account or static fallback.
use serde_json::{Value, json};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct WireProfile {
    pub model_enum: Option<String>,
    pub max_output_tokens: Option<u64>,
    pub thinking_level: Option<&'static str>,
    pub thinking_budget: Option<i64>,
    pub supports_thinking: bool,
}
impl WireProfile {
    pub fn from_row(row: &Value) -> Option<Self> {
        let profile = Self {
            model_enum: row["model"]
                .as_str()
                .filter(|s| {
                    !s.is_empty()
                        && s.len() <= 128
                        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                })
                .map(str::to_owned),
            max_output_tokens: row["maxOutputTokens"]
                .as_u64()
                .filter(|n| (1..=1_048_576).contains(n)),
            thinking_level: match row["thinkingLevel"].as_u64() {
                Some(1) => Some("LOW"),
                Some(2) => Some("MEDIUM"),
                Some(3) => Some("HIGH"),
                _ => None,
            },
            thinking_budget: row["thinkingBudget"]
                .as_i64()
                .filter(|n| (-1..=1_048_576).contains(n)),
            supports_thinking: row["supportsThinking"] == true,
        };
        if profile.model_enum.is_none()
            && profile.max_output_tokens.is_none()
            && profile.thinking_level.is_none()
            && profile.thinking_budget.is_none()
        {
            None
        } else {
            Some(profile)
        }
    }
    pub fn apply(&self, body: &mut Value) {
        if let Some(model_enum) = &self.model_enum {
            body["labels"]["model_enum"] = json!(model_enum);
        }
        if self.max_output_tokens.is_some()
            || (self.supports_thinking
                && (self.thinking_level.is_some() || self.thinking_budget.is_some()))
        {
            if !body["generationConfig"].is_object() {
                body["generationConfig"] = json!({});
            }
            if let Some(cap) = self.max_output_tokens {
                body["generationConfig"]["maxOutputTokens"] = json!(cap);
            }
            if self.supports_thinking {
                // A level takes precedence over the roster's numeric budget,
                // including -1 (dynamic), as in OMP's tier-SKU serialization.
                if let Some(level) = self.thinking_level {
                    body["generationConfig"]["thinkingConfig"] =
                        json!({"includeThoughts":true,"thinkingLevel":level});
                } else if let Some(budget) = self.thinking_budget {
                    body["generationConfig"]["thinkingConfig"] =
                        json!({"includeThoughts":true,"thinkingBudget":budget});
                }
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn levels_override_budgets_and_roster_caps_are_preserved() {
        let p=WireProfile::from_row(&json!({"model":"MODEL_PLACEHOLDER_TEST","maxOutputTokens":65536,"thinkingLevel":3,"thinkingBudget":-1,"supportsThinking":true})).unwrap();
        let mut body = json!({"labels":{}});
        p.apply(&mut body);
        assert_eq!(body["labels"]["model_enum"], "MODEL_PLACEHOLDER_TEST");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 65536);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"],
            json!({"includeThoughts":true,"thinkingLevel":"HIGH"})
        );
        let p =
            WireProfile::from_row(&json!({"thinkingBudget":-1,"supportsThinking":true})).unwrap();
        let mut body = json!({"labels":{}});
        p.apply(&mut body);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            -1
        );
    }
    #[test]
    fn invalid_optional_fields_do_not_invent_provider_constants() {
        for row in [
            json!({}),
            json!({"model":"private arbitrary value","maxOutputTokens":0,"thinkingBudget":-2,"thinkingLevel":4}),
            json!({"maxOutputTokens":1e99}),
        ] {
            assert!(WireProfile::from_row(&row).is_none());
        }
        let p = WireProfile::from_row(
            &json!({"maxOutputTokens":123,"thinkingBudget":0,"supportsThinking":false}),
        )
        .unwrap();
        let mut body = json!({"labels":{}});
        p.apply(&mut body);
        assert!(body["generationConfig"].get("thinkingConfig").is_none());
    }
}
