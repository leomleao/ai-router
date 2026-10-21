//! CCA accepts the legacy Google Schema dialect, not arbitrary JSON Schema.
//! Project provider hints; the unchanged client schema remains authoritative at
//! tool handoff. Rich constraints also remain in the tool description. This is
//! deliberately not a claim of provider-side constrained decoding.
//! See the pinned OMP normalizeSchemaForCCA source in THIRD_PARTY_NOTICES.md.
use serde_json::{Map, Value, json};

pub(crate) fn parameters(schema: &Value) -> Value {
    project(schema, schema, 0, &mut 4096)
}

fn project(value: &Value, root: &Value, depth: usize, budget: &mut usize) -> Value {
    // Recursive schemas cannot be expanded indefinitely. An unconstrained
    // object hint terminates expansion; local validation still checks the ref.
    if depth >= 24 || *budget == 0 {
        return json!({"type":"object","properties":{}});
    }
    *budget -= 1;
    let Some(input) = value.as_object() else {
        return json!({"type":"object","properties":{}});
    };
    let mut source = Map::new();
    if let Some(reference) = input.get("$ref").and_then(Value::as_str)
        && let Some(target) = reference.strip_prefix('#').and_then(|p| root.pointer(p))
        && let Some(resolved) = project(target, root, depth + 1, budget).as_object()
    {
        source.extend(resolved.clone());
    }
    // CCA has no combiners. Combine object hints and optional properties;
    // retain full branch semantics in the original client contract.
    for keyword in ["allOf", "anyOf", "oneOf"] {
        if let Some(branches) = input.get(keyword).and_then(Value::as_array) {
            for branch in branches {
                let hint = project(branch, root, depth + 1, budget);
                if let Some(fields) = hint.as_object() {
                    for (key, value) in fields {
                        if key == "properties" {
                            let target = source.entry(key.clone()).or_insert_with(|| json!({}));
                            if let (Some(target), Some(fields)) =
                                (target.as_object_mut(), value.as_object())
                            {
                                target.extend(fields.clone());
                            }
                        } else if key != "required" {
                            source.entry(key.clone()).or_insert_with(|| value.clone());
                        }
                    }
                }
            }
        }
    }
    for (key, value) in input {
        match key.as_str() {
            "type" => {
                let kind = value.as_str().or_else(|| {
                    value
                        .as_array()?
                        .iter()
                        .filter_map(Value::as_str)
                        .find(|s| *s != "null")
                });
                if let Some(kind) = kind.filter(|s| {
                    matches!(
                        *s,
                        "object" | "array" | "string" | "boolean" | "integer" | "number"
                    )
                }) {
                    source.insert(key.clone(), json!(kind));
                }
            }
            "title" | "description" => {
                source.insert(key.clone(), value.clone());
            }
            "enum"
                if value
                    .as_array()
                    .is_some_and(|a| !a.is_empty() && a.iter().all(Value::is_string)) =>
            {
                source.insert(key.clone(), value.clone());
                source.entry("type").or_insert(json!("string"));
            }
            "const" if value.is_string() => {
                source.insert("enum".into(), json!([value]));
                source.entry("type").or_insert(json!("string"));
            }
            "properties" => {
                let mut properties = source
                    .remove("properties")
                    .and_then(|v| v.as_object().cloned())
                    .unwrap_or_default();
                if let Some(fields) = value.as_object() {
                    for (name, schema) in fields {
                        // Property names are data, including '$schema' or 'type'.
                        properties.insert(name.clone(), project(schema, root, depth + 1, budget));
                    }
                }
                source.insert(key.clone(), Value::Object(properties));
                source.insert("type".into(), json!("object"));
            }
            "items" if value.is_object() || value.is_boolean() => {
                source.insert(key.clone(), project(value, root, depth + 1, budget));
                source.insert("type".into(), json!("array"));
            }
            "required" => {
                source.insert(key.clone(), value.clone());
            }
            _ => {}
        }
    }
    if !source.contains_key("type") {
        source.insert("type".into(), json!("object"));
    }
    if source["type"] == "object" {
        source.entry("properties").or_insert_with(|| json!({}));
    } else {
        // Mixed unions and reference siblings can contribute fields from a
        // different branch type. CCA rejects these even though JSON Schema
        // permits type-specific keywords to be ignored on other types.
        source.remove("properties");
        source.remove("required");
    }
    if source["type"] == "array" {
        source
            .entry("items")
            .or_insert_with(|| json!({"type":"string"}));
    } else {
        source.remove("items");
    }
    if source["type"] != "string" {
        source.remove("enum");
    }
    Value::Object(source)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn projects_n8n_refs_nullable_arrays_and_preserves_property_names() {
        let schema = json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object",
            "$defs":{"Query":{"type":"string","minLength":1}},
            "properties":{"$schema":{"$ref":"#/$defs/Query"},"type":{"type":["string","null"]},
            "requests":{"type":"array","items":{"type":"object","properties":{"q":{"$ref":"#/$defs/Query"}},"additionalProperties":false}}},
            "required":["$schema"],"additionalProperties":false});
        let result = parameters(&schema);
        assert_eq!(result["properties"]["$schema"], json!({"type":"string"}));
        assert_eq!(result["properties"]["type"], json!({"type":"string"}));
        assert_eq!(result["required"], json!(["$schema"]));
        assert!(!result.to_string().contains("additionalProperties"));
        assert!(!result.to_string().contains("$ref"));
        assert!(schema.get("additionalProperties").is_some());
    }
    #[test]
    fn recursion_and_combiners_are_bounded_and_original_validation_remains_strict() {
        let schema = json!({"type":"object","properties":{"node":{"$ref":"#"},"q":{"anyOf":[{"type":"string","minLength":1},{"type":"null"}]}},"required":["q"],"additionalProperties":false});
        let result = parameters(&schema);
        assert!(result.to_string().len() < 20000);
        assert!(!result.to_string().contains("anyOf"));
        let validator = jsonschema::validator_for(&schema).unwrap();
        assert!(validator.is_valid(&json!({"q":null})));
        assert!(!validator.is_valid(&json!({"q":"","extra":1})));
    }

    #[test]
    fn mixed_union_items_emit_only_fields_allowed_for_the_projected_type() {
        let schema = json!({"type":"object","properties":{"nodeTypes":{"type":"array",
            "items":{"anyOf":[{"type":"string"},{"type":"object",
                "properties":{"name":{"type":"string"}},"required":["name"]}]}}},
            "required":["nodeTypes"],"additionalProperties":false});
        let result = parameters(&schema);
        assert_eq!(
            result["properties"]["nodeTypes"]["items"],
            json!({"type":"string"})
        );

        // Projection only narrows provider hints; both original branches remain
        // valid locally, and unrelated/invalid arguments still fail validation.
        let validator = jsonschema::validator_for(&schema).unwrap();
        assert!(validator.is_valid(&json!({"nodeTypes":["http",{"name":"code"}]})));
        assert!(!validator.is_valid(&json!({"nodeTypes":[42]})));
        assert!(!validator.is_valid(&json!({"nodeTypes":[],"extra":true})));
    }

    #[test]
    fn incompatible_union_fields_are_removed_recursively_in_either_order() {
        let branches = [
            json!({"type":"string","enum":["http","code"]}),
            json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}),
            json!({"type":"array","items":{"type":"number"}}),
        ];
        for keyword in ["anyOf", "oneOf", "allOf"] {
            for first in &branches {
                for second in &branches {
                    let schema = json!({(keyword):[first,second]});
                    let result = parameters(&schema);
                    let kind = result["type"].as_str().unwrap();
                    assert_eq!(
                        result.get("properties").is_some(),
                        kind == "object",
                        "{result}"
                    );
                    assert!(
                        result.get("required").is_none() || kind == "object",
                        "{result}"
                    );
                    assert_eq!(result.get("items").is_some(), kind == "array", "{result}");
                    assert!(result.get("enum").is_none() || kind == "string", "{result}");
                }
            }
        }
        let schema = json!({"$defs":{"Object":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}},
            "$ref":"#/$defs/Object","type":"string"});
        assert_eq!(parameters(&schema), json!({"type":"string"}));
    }
}
