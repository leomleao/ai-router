//! Bounded, sanitized validation metadata shared by provider adapters.
use crate::protocol::ToolDefinition;
use serde::Serialize;
use serde_json::{Value, json};

const MAX_ERRORS: usize = 8;
const MAX_PATH: usize = 256;

#[derive(Serialize)]
pub(crate) struct Rejection {
    reason: &'static str,
    selected_tool: Option<String>,
    arguments_type: &'static str,
    validation_errors: Vec<Value>,
    validation_errors_truncated: bool,
}

impl std::fmt::Debug for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rejection")
            .field("reason", &self.reason)
            .field("selected_tool", &self.selected_tool)
            .field("arguments_type", &self.arguments_type)
            .field("validation_errors", &self.validation_errors)
            .field(
                "validation_errors_truncated",
                &self.validation_errors_truncated,
            )
            .finish()
    }
}

impl Rejection {
    fn new(reason: &'static str, name: Option<&Value>, arguments: &Value) -> Self {
        Self {
            reason,
            selected_tool: name
                .and_then(Value::as_str)
                .filter(|name| {
                    !name.is_empty()
                        && name.len() <= 64
                        && name
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
                })
                .map(str::to_owned),
            arguments_type: value_type(arguments),
            validation_errors: Vec::new(),
            validation_errors_truncated: false,
        }
    }
}

fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

pub(crate) fn validate_call(
    tools: &[ToolDefinition],
    name: Option<&Value>,
    arguments: &Value,
) -> Result<(), Rejection> {
    let mut rejection = Rejection::new("unknown_tool", name, arguments);
    let Some(selected) = rejection.selected_tool.as_deref() else {
        rejection.reason = "invalid_tool_name";
        return Err(rejection);
    };
    let Some(tool) = tools.iter().find(|tool| tool.name == selected) else {
        return Err(rejection);
    };
    if !arguments.is_object() {
        rejection.reason = "arguments_not_object";
        return Err(rejection);
    }
    let Ok(validator) = jsonschema::validator_for(&tool.parameters) else {
        rejection.reason = "invalid_tool_schema";
        return Err(rejection);
    };
    rejection.reason = "schema_mismatch";
    (
        rejection.validation_errors,
        rejection.validation_errors_truncated,
    ) = schema_errors(&tool.parameters, &validator, arguments);
    if rejection.validation_errors.is_empty() {
        Ok(())
    } else {
        Err(rejection)
    }
}

/// Bounded, value-free descriptions of why `instance` fails `schema`: schema
/// keyword paths, depth and JSON types only. Shared by tool-call and
/// structured-output diagnostics.
pub(crate) fn schema_errors(
    schema: &Value,
    validator: &jsonschema::Validator,
    instance: &Value,
) -> (Vec<Value>, bool) {
    let mut errors = Vec::new();
    for error in validator.iter_errors(instance).take(MAX_ERRORS + 1) {
        if errors.len() == MAX_ERRORS {
            return (errors, true);
        }
        // The schema path contains schema keywords, while instance paths and
        // validator formatting can expose caller-provided keys or values.
        let path = error.schema_path.to_string();
        let keyword = path.rsplit('/').next().unwrap_or("");
        let mut detail = json!({
            "schema_path":path.chars().take(MAX_PATH).collect::<String>(),
            "schema_path_truncated":path.chars().count() > MAX_PATH,
            "instance_depth":error.instance_path.to_string().split('/').skip(1).count(),
            "actual_type":value_type(&error.instance),
        });
        if keyword == "type" {
            let expected = schema.pointer(&path);
            let types: Vec<_> = expected
                .into_iter()
                .flat_map(|value| {
                    value
                        .as_array()
                        .map_or_else(|| vec![value], |values| values.iter().collect())
                })
                .filter_map(Value::as_str)
                .filter(|kind| {
                    matches!(
                        *kind,
                        "null" | "boolean" | "object" | "array" | "number" | "integer" | "string"
                    )
                })
                .take(7)
                .collect();
            detail["expected_types"] = json!(types);
        }
        if keyword == "required" {
            let fields: Vec<_> = schema
                .pointer(&path)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|field| error.instance.get(*field).is_none())
                .take(MAX_ERRORS)
                .map(|field| field.chars().take(64).collect::<String>())
                .collect();
            detail["missing_required_fields"] = json!(fields);
        }
        errors.push(detail);
    }
    (errors, false)
}

/// Decide whether a provider call may be handed to the client.
///
/// A call that passes validation is always admitted. When `relay` is set
/// (tool_choice absent or auto), a call that fails only because it names an
/// undeclared tool, while no declared tool is strict, or because its object
/// arguments mismatch a non-strict tool's schema, is admitted too: OpenAI
/// returns such calls for non-strict tools, and clients such as Twenty repair
/// them or answer with a tool error. `Ok(Some(_))` carries the sanitized
/// rejection for logs. Malformed names and non-object arguments always fail.
pub(crate) fn admit_call(
    tools: &[ToolDefinition],
    name: Option<&Value>,
    arguments: &Value,
    relay: bool,
) -> Result<Option<Value>, Rejection> {
    let Err(rejection) = validate_call(tools, name, arguments) else {
        return Ok(None);
    };
    let relayable = relay
        && match rejection.reason {
            "unknown_tool" => {
                arguments.is_object() && !tools.is_empty() && tools.iter().all(|tool| !tool.strict)
            }
            "schema_mismatch" => tools.iter().any(|tool| {
                Some(tool.name.as_str()) == rejection.selected_tool.as_deref() && !tool.strict
            }),
            _ => false,
        };
    if !relayable {
        return Err(rejection);
    }
    let mut diagnostics = serde_json::to_value(&rejection).unwrap_or(Value::Null);
    diagnostics["policy"] = json!("relayed_non_strict");
    Ok(Some(diagnostics))
}

impl Rejection {
    pub(crate) fn reason(&self) -> &'static str {
        self.reason
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(schema: Value) -> ToolDefinition {
        ToolDefinition {
            name: "weather".into(),
            description: "private description".into(),
            parameters: schema,
            strict: false,
        }
    }

    #[test]
    fn distinguishes_name_shape_and_schema_failures_without_values() {
        let tools = [tool(
            json!({"type":"object","properties":{"count":{"type":"integer"},"city":{"enum":["private expected value"]}},"required":["count"],"additionalProperties":false}),
        )];
        for (name, args, reason) in [
            (json!("missing"), json!({}), "unknown_tool"),
            (
                json!("private name\nnot an identifier"),
                json!({}),
                "invalid_tool_name",
            ),
            (
                json!("weather"),
                json!("private argument string"),
                "arguments_not_object",
            ),
            (
                json!("weather"),
                json!({"count":"private wrong type","city":"private enum value","private dynamic key":"private value"}),
                "schema_mismatch",
            ),
            (json!("weather"), json!({}), "schema_mismatch"),
        ] {
            let rejection = validate_call(&tools, Some(&name), &args).unwrap_err();
            let encoded = serde_json::to_value(&rejection).unwrap();
            assert_eq!(encoded["reason"], reason);
            assert!(!encoded.to_string().contains("private"));
            assert!(!format!("{rejection:?}").contains("private"));
            if reason == "invalid_tool_name" {
                assert!(encoded["selected_tool"].is_null());
            }
        }
        let errors = serde_json::to_value(
            validate_call(&tools, Some(&json!("weather")), &json!({"count":"wrong"})).unwrap_err(),
        )
        .unwrap();
        assert_eq!(
            errors["validation_errors"][0]["schema_path"],
            "/properties/count/type"
        );
        assert_eq!(
            errors["validation_errors"][0]["expected_types"],
            json!(["integer"])
        );
        assert_eq!(errors["validation_errors"][0]["actual_type"], "string");
        assert!(validate_call(&tools, Some(&json!("weather")), &json!({"count":1})).is_ok());
    }

    #[test]
    fn validates_nested_local_references_without_exposing_input() {
        let tools = [tool(json!({
            "type":"object",
            "properties":{"requests":{"type":"array","items":{"$ref":"#/$defs/Request"}}},
            "$defs":{"Request":{"type":"object","properties":{"name":{"type":"string"}},"additionalProperties":false}}
        }))];
        assert!(
            validate_call(
                &tools,
                Some(&json!("weather")),
                &json!({"requests":[{"name":"ok"}]})
            )
            .is_ok()
        );
        let rejection = validate_call(
            &tools,
            Some(&json!("weather")),
            &json!({"requests":[{"name":123,"private":"value"}]}),
        )
        .unwrap_err();
        assert!(
            !serde_json::to_string(&rejection)
                .unwrap()
                .contains("private")
        );
        assert!(!format!("{rejection:?}").contains("private"));
    }

    #[test]
    fn validation_errors_and_schema_paths_are_bounded() {
        let fields = (0..20)
            .map(|i| (format!("field_{i}"), json!({"type":"integer"})))
            .collect::<serde_json::Map<_, _>>();
        let args = (0..20)
            .map(|i| (format!("field_{i}"), json!("private")))
            .collect::<serde_json::Map<_, _>>();
        let tools = [tool(json!({"type":"object","properties":fields}))];
        let rejection = serde_json::to_value(
            validate_call(&tools, Some(&json!("weather")), &json!(args)).unwrap_err(),
        )
        .unwrap();
        assert_eq!(
            rejection["validation_errors"].as_array().unwrap().len(),
            MAX_ERRORS
        );
        assert_eq!(rejection["validation_errors_truncated"], true);
        assert!(!rejection.to_string().contains("private"));

        let long = "x".repeat(500);
        let tools = [tool(
            json!({"type":"object","properties":{long.clone():{"type":"integer"}}}),
        )];
        let rejection = serde_json::to_value(
            validate_call(&tools, Some(&json!("weather")), &json!({long:"private"})).unwrap_err(),
        )
        .unwrap();
        assert_eq!(
            rejection["validation_errors"][0]["schema_path"]
                .as_str()
                .unwrap()
                .len(),
            MAX_PATH
        );
        assert_eq!(
            rejection["validation_errors"][0]["schema_path_truncated"],
            true
        );
        assert!(!rejection.to_string().contains("private"));
    }
}
