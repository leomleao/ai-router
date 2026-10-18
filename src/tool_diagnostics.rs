//! Bounded tool-validation metadata. Never format validator errors or argument values.
use crate::protocol::{RunRequest, ToolDefinition};
use jsonschema::error::{ValidationError, ValidationErrorKind};
use serde::Serialize;
use serde_json::{Value, json};

const MAX_ERRORS: usize = 8;
const MAX_PATH: usize = 256;
const MAX_FIELDS: usize = 16;
const MAX_FIELD_FEEDBACK_BYTES: usize = 8192;

#[derive(Serialize)]
pub(crate) struct Rejection {
    reason: &'static str,
    selected_tool: Option<String>,
    arguments_type: &'static str,
    validation_errors: Vec<Value>,
    validation_errors_truncated: bool,
    // These may contain dynamic argument keys. Only feedback() exposes them to
    // the same provider that generated them; Serialize/Debug must omit them.
    #[serde(skip)]
    field_feedback: Vec<Value>,
    #[serde(skip)]
    field_feedback_truncated: bool,
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
            .finish_non_exhaustive()
    }
}

impl Rejection {
    pub(crate) fn repairable(&self) -> bool {
        matches!(self.reason, "arguments_not_object" | "schema_mismatch")
    }

    pub(crate) fn feedback(&self, remaining: usize) -> Value {
        json!({"isError":true,"content":[{"type":"text","text":json!({
            "error":"invalid_client_tool_arguments", "diagnostic":self,
            "field_feedback":self.field_feedback,
            "field_feedback_truncated":self.field_feedback_truncated,
            "remaining_corrections":remaining,
            "instruction":"No client tool was executed. Correct arguments using the exact inputSchema from tools/list. Remove the unexpected_fields at the indicated instance_path; allowed_fields lists declared fields at that schema location. If allows_pattern_fields is true, names matching patternProperties are also allowed. Choose a suitable anyOf/oneOf branch before applying its hints; do not combine conflicting branch lists. Include required fields and declared types/values. Consult the full inputSchema when field lists are unavailable or any fields, paths or lists are truncated. Submit a corrected tools/call."
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
            field_feedback: Vec::new(),
            field_feedback_truncated: false,
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

fn bounded_fields<'a>(fields: impl Iterator<Item = &'a str>) -> (Vec<String>, bool) {
    let mut truncated = false;
    let fields: Vec<_> = fields
        .take(MAX_FIELDS + 1)
        .map(|field| {
            if field.chars().count() > 64 {
                truncated = true;
            }
            field.chars().take(64).collect::<String>()
        })
        .collect();
    if fields.len() > MAX_FIELDS {
        truncated = true;
    }
    (fields.into_iter().take(MAX_FIELDS).collect(), truncated)
}

fn schema_location<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = root;
    let mut resource = root;
    for token in path.split('/').skip(1) {
        if current.get("$id").and_then(Value::as_str).is_some() {
            resource = current;
        }
        // jsonschema locations retain the $ref traversal in their path. Follow
        // resolvable local JSON pointers only; anchors/dynamic scopes stay unknown.
        current = if token == "$ref" && current.get("$ref").and_then(Value::as_str).is_some() {
            let reference = current.get("$ref")?.as_str()?.strip_prefix('#')?;
            if !reference.is_empty() && !reference.starts_with('/') {
                return None;
            }
            resource.pointer(reference)?
        } else {
            current.pointer(&format!("/{token}"))?
        };
    }
    Some(current)
}

fn append_field_feedback<'a>(
    error: &ValidationError<'_>,
    schema: &Value,
    rejection: &mut Rejection,
    unexpected: impl Iterator<Item = &'a str>,
) {
    if rejection.field_feedback.len() == MAX_ERRORS {
        rejection.field_feedback_truncated = true;
        return;
    }
    let path = error.schema_path.to_string();
    let instance = error.instance_path.to_string();
    let parent = path
        .rsplit_once('/')
        .and_then(|(parent, _)| schema_location(schema, parent));
    let (unexpected, unexpected_truncated) = bounded_fields(unexpected);
    let mut hint = json!({"schema_path":path.chars().take(MAX_PATH).collect::<String>(),
        "instance_path":instance.chars().take(MAX_PATH).collect::<String>(),
        "paths_truncated":path.chars().count()>MAX_PATH || instance.chars().count()>MAX_PATH,
        "unexpected_fields":unexpected,"unexpected_fields_truncated":unexpected_truncated,
        "allowed_fields_available":parent.is_some_and(Value::is_object)});
    if let Some(parent) = parent.filter(|value| value.is_object()) {
        let (allowed, allowed_truncated) = bounded_fields(
            parent
                .get("properties")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|fields| fields.keys().map(String::as_str)),
        );
        hint["allowed_fields"] = json!(allowed);
        hint["allowed_fields_truncated"] = json!(allowed_truncated);
        hint["allows_pattern_fields"] = json!(
            parent
                .get("patternProperties")
                .and_then(Value::as_object)
                .is_some_and(|patterns| !patterns.is_empty())
        );
    }
    rejection.field_feedback.push(hint);
    if serde_json::to_vec(&rejection.field_feedback)
        .map_or(true, |bytes| bytes.len() > MAX_FIELD_FEEDBACK_BYTES)
    {
        rejection.field_feedback.pop();
        rejection.field_feedback_truncated = true;
    }
}

fn collect_field_feedback(
    error: &ValidationError<'_>,
    schema: &Value,
    arguments: &Value,
    rejection: &mut Rejection,
    visited: &mut usize,
    depth: usize,
) {
    if *visited >= 64 {
        rejection.field_feedback_truncated = true;
        return;
    }
    *visited += 1;
    if depth > 8 {
        rejection.field_feedback_truncated = true;
        return;
    }
    match &error.kind {
        ValidationErrorKind::AdditionalProperties { unexpected } => {
            append_field_feedback(
                error,
                schema,
                rejection,
                unexpected.iter().map(String::as_str),
            );
        }
        ValidationErrorKind::FalseSchema
            if error
                .schema_path
                .to_string()
                .ends_with("/additionalProperties") =>
        {
            // Optimized validator for an object that permits no properties.
            let path = error.schema_path.to_string();
            let parent = path
                .rsplit_once('/')
                .and_then(|(parent, _)| schema_location(schema, parent));
            if parent.is_some_and(|parent| {
                parent.get("additionalProperties") == Some(&Value::Bool(false))
                    && ["properties", "patternProperties"].iter().all(|keyword| {
                        parent
                            .get(*keyword)
                            .and_then(Value::as_object)
                            .is_none_or(|map| map.is_empty())
                    })
            }) {
                // This validator stores the first rejected property's value as
                // its instance. Resolve the object via its instance path instead.
                if let Some(fields) = arguments
                    .pointer(&error.instance_path.to_string())
                    .and_then(Value::as_object)
                {
                    append_field_feedback(
                        error,
                        schema,
                        rejection,
                        fields.keys().map(String::as_str),
                    );
                }
            }
        }
        ValidationErrorKind::AnyOf { context } | ValidationErrorKind::OneOfNotValid { context } => {
            for nested in context.iter().flatten() {
                collect_field_feedback(nested, schema, arguments, rejection, visited, depth + 1);
                if *visited >= 64 {
                    rejection.field_feedback_truncated = true;
                    break;
                }
            }
        }
        _ => {}
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
    let mut visited = 0;
    for error in validator.iter_errors(arguments).take(MAX_ERRORS + 1) {
        if rejection.validation_errors.len() == MAX_ERRORS {
            rejection.validation_errors_truncated = true;
            rejection.field_feedback_truncated = true;
            break;
        }
        collect_field_feedback(
            &error,
            &tool.parameters,
            arguments,
            &mut rejection,
            &mut visited,
            0,
        );
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

    fn feedback(rejection: &Rejection) -> Value {
        serde_json::from_str(
            rejection.feedback(2)["content"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn unexpected_and_allowed_fields_are_provider_only_without_values() {
        let tools = [tool(
            json!({"type":"object","properties":{"createRequest":{"type":"object"},"updateRequest":{"type":"object"}},"additionalProperties":false}),
        )];
        let rejection = validate_call(
            &tools,
            Some(&json!("weather")),
            &json!({"synthetic-private-field":"synthetic-private-value","createRequest":{}}),
        )
        .unwrap_err();
        let hint = feedback(&rejection)["field_feedback"][0].clone();
        assert_eq!(
            hint["unexpected_fields"],
            json!(["synthetic-private-field"])
        );
        assert_eq!(
            hint["allowed_fields"],
            json!(["createRequest", "updateRequest"])
        );
        assert_eq!(hint["instance_path"], "");
        assert!(
            !feedback(&rejection)
                .to_string()
                .contains("synthetic-private-value")
        );
        assert!(
            !serde_json::to_string(&rejection)
                .unwrap()
                .contains("synthetic-private-field")
        );
        assert!(!format!("{rejection:?}").contains("synthetic-private-field"));
    }

    #[test]
    fn nested_local_references_and_schema_branches_keep_their_locations() {
        let scoped = json!({"$defs":{"Request":{"properties":{"wrong":{}}}},
            "properties":{"local":{"$id":"urn:local","$ref":"#/$defs/Request","$defs":{"Request":{"properties":{"right":{}}}}}}});
        assert_eq!(
            schema_location(&scoped, "/properties/local/$ref").unwrap()["properties"],
            json!({"right":{}})
        );
        let tools = [tool(
            json!({"type":"object","properties":{"requests":{"type":"array","items":{"$ref":"#/$defs/Request"}}},"$defs":{"Request":{"type":"object","properties":{"name":{"type":"string"}},"patternProperties":{"^x_":{"type":"boolean"}},"additionalProperties":false}}}),
        )];
        let mut args = json!({"requests":[{"name":"synthetic-value","x_valid":true,"wrong/key":"synthetic-private-value"}]});
        let rejection = validate_call(&tools, Some(&json!("weather")), &args).unwrap_err();
        let hint = feedback(&rejection)["field_feedback"][0].clone();
        assert_eq!(hint["instance_path"], "/requests/0");
        assert_eq!(hint["unexpected_fields"], json!(["wrong/key"]));
        assert_eq!(hint["allowed_fields"], json!(["name"]));
        assert_eq!(hint["allows_pattern_fields"], true);
        args["requests"][0]
            .as_object_mut()
            .unwrap()
            .remove("wrong/key");
        assert!(validate_call(&tools, Some(&json!("weather")), &args).is_ok());
        for keyword in ["anyOf", "oneOf"] {
            let tools = [tool(json!({"type":"object",keyword:[
                {"properties":{"left":{}},"additionalProperties":false},
                {"properties":{"right":{}},"additionalProperties":false}]}))];
            let rejection = validate_call(
                &tools,
                Some(&json!("weather")),
                &json!({"wrong":"synthetic-private-value"}),
            )
            .unwrap_err();
            let feedback = feedback(&rejection);
            let hints = feedback["field_feedback"].as_array().unwrap();
            assert_eq!(hints.len(), 2);
            assert_eq!(hints[0]["allowed_fields"], json!(["left"]));
            assert_eq!(hints[1]["allowed_fields"], json!(["right"]));
            assert_ne!(hints[0]["schema_path"], hints[1]["schema_path"]);
            assert!(!feedback.to_string().contains("synthetic-private-value"));
        }
    }

    #[test]
    fn field_feedback_is_bounded_and_reports_truncation() {
        let properties = (0..30)
            .map(|i| (format!("field_{i}"), json!({})))
            .collect::<serde_json::Map<_, _>>();
        let args = (0..30)
            .map(|i| (format!("unexpected_{i}"), json!("private-value")))
            .collect::<serde_json::Map<_, _>>();
        let tools = [tool(
            json!({"type":"object","properties":properties,"additionalProperties":false}),
        )];
        let rejection = validate_call(&tools, Some(&json!("weather")), &json!(args)).unwrap_err();
        let hint = feedback(&rejection)["field_feedback"][0].clone();
        assert_eq!(hint["allowed_fields"].as_array().unwrap().len(), MAX_FIELDS);
        assert_eq!(
            hint["unexpected_fields"].as_array().unwrap().len(),
            MAX_FIELDS
        );
        assert_eq!(hint["allowed_fields_truncated"], true);
        assert_eq!(hint["unexpected_fields_truncated"], true);
        let tools = [tool(json!({"type":"object","additionalProperties":false}))];
        let rejection = validate_call(
            &tools,
            Some(&json!("weather")),
            &json!({"x".repeat(300):"private-value"}),
        )
        .unwrap_err();
        let hint = feedback(&rejection)["field_feedback"][0].clone();
        assert!(
            hint["unexpected_fields"][0].is_string(),
            "feedback: {}",
            feedback(&rejection)
        );
        assert_eq!(hint["unexpected_fields"][0].as_str().unwrap().len(), 64);
        assert_eq!(hint["unexpected_fields_truncated"], true);
        let tools = [tool(
            json!({"type":"object","anyOf":(0..100).map(|i|json!({"properties":{format!("field_{i}"):{}},"additionalProperties":false})).collect::<Vec<_>>()}),
        )];
        let rejection = validate_call(
            &tools,
            Some(&json!("weather")),
            &json!({"wrong":"private-value"}),
        )
        .unwrap_err();
        assert!(rejection.field_feedback.len() <= MAX_ERRORS);
        assert!(
            serde_json::to_vec(&rejection.field_feedback).unwrap().len()
                <= MAX_FIELD_FEEDBACK_BYTES
        );
        assert!(rejection.field_feedback_truncated);
    }

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
