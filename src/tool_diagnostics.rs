//! Bounded tool-validation metadata. Never format validator errors or argument values.
use crate::protocol::{RunRequest, ToolDefinition};
use serde::Serialize;
use serde_json::{Value, json};

const MAX_ERRORS: usize = 8;
const MAX_PATH: usize = 256;

#[derive(Debug, Serialize)]
pub(crate) struct Rejection {
    reason: &'static str,
    selected_tool: Option<String>,
    arguments_type: &'static str,
    validation_errors: Vec<Value>,
    validation_errors_truncated: bool,
}

impl Rejection {
    pub(crate) fn repairable(&self) -> bool {
        matches!(self.reason, "arguments_not_object" | "schema_mismatch")
    }

    pub(crate) fn feedback(&self, remaining: usize) -> Value {
        json!({"isError":true,"content":[{"type":"text","text":json!({
            "error":"invalid_client_tool_arguments", "diagnostic":self,
            "remaining_corrections":remaining,
            "instruction":"No client tool was executed. Correct the arguments to match this tool's exact inputSchema from tools/list: include required fields, remove undeclared fields, and use the declared types and values. Submit a corrected tools/call."
        }).to_string()}]})
    }

    pub(crate) fn log_attempt(&self, request: &RunRequest, attempt: usize, limit: usize) {
        eprintln!(
            "{}",
            json!({"event":"client_tool_rejected", "request_id":request.request_id,
            "model":request.model, "stage":"relay", "diagnostic":self,
            "rejection_attempt":attempt, "correction_offered":attempt<=limit,
            "remaining_corrections":limit.saturating_sub(attempt.saturating_sub(1))})
        );
    }

    pub(crate) fn new(reason: &'static str, name: Option<&Value>, arguments: &Value) -> Self {
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

    pub(crate) fn log(&self, request: &RunRequest, stage: &'static str) {
        eprintln!(
            "{}",
            json!({"event":"client_tool_rejected", "request_id":request.request_id,
            "model":request.model, "stage":stage, "diagnostic":self})
        );
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
    for error in validator.iter_errors(arguments).take(MAX_ERRORS + 1) {
        if rejection.validation_errors.len() == MAX_ERRORS {
            rejection.validation_errors_truncated = true;
            break;
        }
        // schema_path contains schema field names, never dynamic argument keys.
        // Display/Debug on the error and instance_path can expose argument values.
        let path = error.schema_path.to_string();
        let keyword = path.rsplit('/').next().unwrap_or("");
        let mut detail = json!({
            "schema_path":path.chars().take(MAX_PATH).collect::<String>(),
            "schema_path_truncated":path.chars().count() > MAX_PATH,
            "instance_depth":error.instance_path.to_string().split('/').skip(1).count(),
            "actual_type":value_type(&error.instance),
        });
        if keyword == "type" {
            // Only standardized type names; never copy arbitrary schema values.
            let expected = tool.parameters.pointer(&path);
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
            let fields: Vec<_> = tool
                .parameters
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
        rejection.validation_errors.push(detail);
    }
    if rejection.validation_errors.is_empty() {
        Ok(())
    } else {
        Err(rejection)
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
            let encoded = serde_json::to_value(rejection).unwrap();
            assert_eq!(encoded["reason"], reason);
            assert!(!encoded.to_string().contains("private"));
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
    fn errors_paths_and_dynamic_keys_are_bounded() {
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
        let tools = [tool(
            json!({"type":"object","additionalProperties":{"type":"integer"}}),
        )];
        let rejection = serde_json::to_value(
            validate_call(
                &tools,
                Some(&json!("weather")),
                &json!({"private dynamic key":"private value"}),
            )
            .unwrap_err(),
        )
        .unwrap();
        assert_eq!(
            rejection["validation_errors"][0]["schema_path"],
            "/additionalProperties/type"
        );
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
    }
}
