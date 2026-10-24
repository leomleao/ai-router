//! Structured operational log: one JSON object per line on stderr.
//!
//! Every event carries `ts` and `event`. Fields are bounded metadata only:
//! identifiers, counts, sizes, codes and value-free validation summaries.
//! Prompts, tool arguments, model output, credentials and provider bodies are
//! never logged; failures keep those in the memory-only monitor capture.
use axum::http::HeaderMap;
use chrono::{SecondsFormat, Utc};
use serde_json::{Map, Value, json};

pub(crate) fn event(name: &str, fields: Value) {
    let mut line = Map::new();
    line.insert(
        "ts".into(),
        json!(Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)),
    );
    line.insert("event".into(), json!(name));
    if let Value::Object(fields) = fields {
        line.extend(fields);
    }
    eprintln!("{}", Value::Object(line));
}

/// A printable-ASCII header value, cut at `max` bytes, for client identification.
pub(crate) fn header(headers: &HeaderMap, name: &str, max: usize) -> Option<String> {
    let value = headers.get(name)?.to_str().ok()?;
    let text: String = value
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(max)
        .collect();
    (!text.is_empty()).then_some(text)
}

/// Counts of item kinds, e.g. {"message":2,"function_call":1}. A missing or
/// non-string kind counts as `fallback`.
pub(crate) fn kinds<'a>(
    items: impl IntoIterator<Item = &'a Value>,
    key: &str,
    fallback: &str,
) -> Value {
    let mut counts = Map::new();
    for item in items {
        let kind: String = item[key]
            .as_str()
            .unwrap_or(fallback)
            .chars()
            .take(64)
            .collect();
        let count = counts.get(&kind).and_then(Value::as_u64).unwrap_or(0);
        counts.insert(kind, json!(count + 1));
    }
    Value::Object(counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_are_printable_and_bounded() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "user-agent",
            "ai-sdk/openai/4.0.83 runtime/node".parse().unwrap(),
        );
        assert_eq!(
            header(&headers, "user-agent", 12).as_deref(),
            Some("ai-sdk/opena")
        );
        assert_eq!(header(&headers, "missing", 12), None);
    }

    #[test]
    fn kinds_count_types_and_roles() {
        let items = [
            json!({"type":"message"}),
            json!({"role":"user"}),
            json!({"type":"function_call"}),
            json!({"type":"message"}),
        ];
        assert_eq!(
            kinds(&items, "type", "message"),
            json!({"message":3,"function_call":1})
        );
    }
}
