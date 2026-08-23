//! Tool-arguments JSON-schema validation — port of
//! `packages/ai/src/utils/validation.ts` (`validateToolArguments`).
//!
//! Validates raw tool-call arguments against the tool's JSON-schema
//! `parameters`, coercing primitives (string↔number↔boolean) the way the
//! upstream TypeBox `Value.Convert` + `coerceWithJsonSchema` pass does, and
//! formatting failures as `Validation failed for tool "X":\n  - path: msg`.

use serde_json::Value;

/// Coerce a primitive value toward `type` (upstream `coercePrimitiveByType`).
fn coerce_primitive_by_type(value: &Value, type_name: &str) -> Value {
    match type_name {
        "number" => match value {
            Value::Null => Value::Number(0.into()),
            Value::String(s) if !s.trim().is_empty() => {
                if let Ok(parsed) = s.trim().parse::<f64>() {
                    if parsed.is_finite() {
                        return serde_json::Number::from_f64(parsed)
                            .map(Value::Number)
                            .unwrap_or_else(|| value.clone());
                    }
                }
                value.clone()
            }
            Value::Bool(b) => Value::Number(if *b { 1 } else { 0 }.into()),
            _ => value.clone(),
        },
        "integer" => match value {
            Value::Null => Value::Number(0.into()),
            Value::String(s) if !s.trim().is_empty() => {
                if let Ok(parsed) = s.trim().parse::<i64>() {
                    return Value::Number(parsed.into());
                }
                value.clone()
            }
            Value::Bool(b) => Value::Number(if *b { 1 } else { 0 }.into()),
            _ => value.clone(),
        },
        "boolean" => match value {
            Value::Null => Value::Bool(false),
            Value::String(s) => match s.as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => value.clone(),
            },
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    if i == 1 {
                        Value::Bool(true)
                    } else if i == 0 {
                        Value::Bool(false)
                    } else {
                        value.clone()
                    }
                } else {
                    value.clone()
                }
            }
            _ => value.clone(),
        },
        "string" => match value {
            Value::Null => Value::String(String::new()),
            Value::Number(n) => Value::String(n.to_string()),
            Value::Bool(b) => Value::String(b.to_string()),
            _ => value.clone(),
        },
        "null" => match value {
            Value::String(s) if s.is_empty() => Value::Null,
            Value::Number(n) if n.as_i64() == Some(0) => Value::Null,
            Value::Bool(false) => Value::Null,
            _ => value.clone(),
        },
        _ => value.clone(),
    }
}

/// Whether `value` matches the JSON type name (upstream `matchesJsonType`).
fn matches_json_type(value: &Value, type_name: &str) -> bool {
    match type_name {
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some(),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

/// Schema types as a list (upstream `getSchemaTypes`).
fn schema_types(schema: &Value) -> Vec<String> {
    match schema.get("type") {
        Some(Value::String(t)) => vec![t.clone()],
        Some(Value::Array(types)) => types
            .iter()
            .filter_map(|t| t.as_str().map(|s| s.to_string()))
            .collect(),
        _ => Vec::new(),
    }
}

/// Recursively coerce a value against a schema (upstream `coerceWithJsonSchema`).
fn coerce_with_json_schema(value: &Value, schema: &Value) -> Value {
    let mut next = value.clone();

    if let Some(all_of) = schema.get("allOf").and_then(|v| v.as_array()) {
        for nested in all_of {
            next = coerce_with_json_schema(&next, nested);
        }
    }
    if let Some(any_of) = schema.get("anyOf").and_then(|v| v.as_array()) {
        next = coerce_with_union(&next, any_of);
    }
    if let Some(one_of) = schema.get("oneOf").and_then(|v| v.as_array()) {
        next = coerce_with_union(&next, one_of);
    }

    let types = schema_types(schema);
    let matches_union = types.len() > 1 && types.iter().any(|t| matches_json_type(&next, t));
    if !types.is_empty() && !matches_union {
        for type_name in &types {
            let candidate = coerce_primitive_by_type(&next, type_name);
            if candidate != next {
                next = candidate;
                break;
            }
        }
    }

    if types.iter().any(|t| t == "object") && next.is_object() {
        if let Some(properties) = schema.get("properties").and_then(|v| v.as_object()) {
            if let Some(obj) = next.as_object_mut() {
                for (key, property_schema) in properties {
                    if let Some(field) = obj.get_mut(key) {
                        let coerced = coerce_with_json_schema(field, property_schema);
                        *field = coerced;
                    }
                }
            }
        }
        if let Some(additional) = schema.get("additionalProperties") {
            if additional.is_object() {
                if let Some(obj) = next.as_object_mut() {
                    let defined: Vec<String> = schema
                        .get("properties")
                        .and_then(|v| v.as_object())
                        .map(|p| p.keys().cloned().collect())
                        .unwrap_or_default();
                    for (key, field) in obj.iter_mut() {
                        if !defined.contains(key) {
                            let coerced = coerce_with_json_schema(field, additional);
                            *field = coerced;
                        }
                    }
                }
            }
        }
    }

    if types.iter().any(|t| t == "array") && next.is_array() {
        if let Some(items) = schema.get("items") {
            if let Some(arr) = next.as_array_mut() {
                for item in arr.iter_mut() {
                    let coerced = coerce_with_json_schema(item, items);
                    *item = coerced;
                }
            }
        }
    }

    next
}

/// Try each union member; return the first that validates, else the first
/// coercion that validates, else the original (upstream `coerceWithUnionSchema`).
fn coerce_with_union(value: &Value, schemas: &[Value]) -> Value {
    for schema in schemas {
        if validate_value(value, schema).is_ok() {
            return value.clone();
        }
    }
    for schema in schemas {
        let candidate = coerce_with_json_schema(value, schema);
        if validate_value(&candidate, schema).is_ok() {
            return candidate;
        }
    }
    value.clone()
}

/// Remove null values for non-required optional properties (upstream
/// `normalizeOptionalNulls`).
fn normalize_optional_nulls(value: &mut Value, schema: &Value) {
    if let Some(arr) = value.as_array_mut() {
        if let Some(items) = schema.get("items") {
            for item in arr.iter_mut() {
                normalize_optional_nulls(item, items);
            }
        }
        return;
    }
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    let Some(properties) = schema.get("properties").and_then(|v| v.as_object()) else {
        return;
    };
    let required: Vec<String> = schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|r| {
            r.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    for (key, property_schema) in properties {
        if let Some(field) = obj.get_mut(key) {
            if field.is_null() && !required.contains(key) {
                // Optional null: drop it (upstream deletes the key).
                obj.remove(key);
            } else {
                normalize_optional_nulls(field, property_schema);
            }
        }
    }
}

/// Validate a value against a schema, returning a list of (path, message)
/// errors (upstream TypeBox `validator.Errors`).
fn validate_value(value: &Value, schema: &Value) -> Result<(), Vec<(String, String)>> {
    let mut errors = Vec::new();
    validate_value_at(value, schema, "", &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn validate_value_at(
    value: &Value,
    schema: &Value,
    path: &str,
    errors: &mut Vec<(String, String)>,
) {
    let types = schema_types(schema);
    if !types.is_empty() {
        let matches = types.iter().any(|t| matches_json_type(value, t));
        if !matches {
            errors.push((
                path.to_string(),
                format!("Expected type: {}", types.join(" | ")),
            ));
            return;
        }
    }

    if types.iter().any(|t| t == "object") && value.is_object() {
        if let Some(properties) = schema.get("properties").and_then(|v| v.as_object()) {
            for (key, property_schema) in properties {
                if let Some(field) = value.get(key) {
                    let child_path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    validate_value_at(field, property_schema, &child_path, errors);
                }
            }
        }
        if let Some(required) = schema.get("required").and_then(|v| v.as_array()) {
            for req in required.iter().filter_map(|v| v.as_str()) {
                if !value.get(req).is_some() {
                    let child_path = if path.is_empty() {
                        req.to_string()
                    } else {
                        format!("{path}.{req}")
                    };
                    errors.push((child_path, "Required property missing".to_string()));
                }
            }
        }
    }

    if types.iter().any(|t| t == "array") && value.is_array() {
        if let Some(items) = schema.get("items") {
            if let Some(arr) = value.as_array() {
                for (i, item) in arr.iter().enumerate() {
                    let child_path = format!("{path}[{i}]");
                    validate_value_at(item, items, &child_path, errors);
                }
            }
        }
    }
}

/// Format a validation error path (upstream `formatValidationPath`).
fn format_validation_path(path: &str) -> String {
    if path.is_empty() {
        "root".to_string()
    } else {
        path.to_string()
    }
}

/// Validate tool-call arguments against the tool's parameters schema
/// (upstream `validateToolArguments`). Returns the validated (and coerced)
/// arguments, or an error with the upstream message shape.
pub fn validate_tool_arguments(
    tool_name: &str,
    parameters: &Value,
    raw_arguments: &Value,
) -> Result<Value, String> {
    let mut args = raw_arguments.clone();
    normalize_optional_nulls(&mut args, parameters);
    let coerced = coerce_with_json_schema(&args, parameters);
    if coerced != args {
        args = coerced;
    }

    match validate_value(&args, parameters) {
        Ok(()) => Ok(args),
        Err(errors) => {
            let lines: Vec<String> = errors
                .iter()
                .map(|(path, message)| format!("  - {}: {}", format_validation_path(path), message))
                .collect();
            let error_message = format!(
                "Validation failed for tool \"{tool_name}\":\n{}\n\nReceived arguments:\n{}",
                lines.join("\n"),
                serde_json::to_string_pretty(raw_arguments)
                    .unwrap_or_else(|_| raw_arguments.to_string())
            );
            Err(error_message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_required_and_types() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "limit": {"type": "number"}
            },
            "required": ["path"]
        });
        assert!(validate_tool_arguments("read", &schema, &json!({"path": "a.txt"})).is_ok());
        let err = validate_tool_arguments("read", &schema, &json!({"limit": 5})).unwrap_err();
        assert!(
            err.contains("Validation failed for tool \"read\""),
            "got: {err}"
        );
        assert!(
            err.contains("path: Required property missing"),
            "got: {err}"
        );
    }

    #[test]
    fn coerces_string_to_number() {
        let schema = json!({
            "type": "object",
            "properties": {
                "limit": {"type": "number"}
            },
            "required": ["limit"]
        });
        let validated = validate_tool_arguments("read", &schema, &json!({"limit": "42"})).unwrap();
        assert_eq!(validated["limit"], json!(42.0));
    }

    #[test]
    fn coerces_number_to_string() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"}
            },
            "required": ["path"]
        });
        let validated = validate_tool_arguments("read", &schema, &json!({"path": 123})).unwrap();
        assert_eq!(validated["path"], json!("123"));
    }

    #[test]
    fn rejects_wrong_type() {
        let schema = json!({
            "type": "object",
            "properties": {
                "limit": {"type": "number"}
            }
        });
        let err = validate_tool_arguments("read", &schema, &json!({"limit": {"nested": true}}))
            .unwrap_err();
        assert!(err.contains("limit: Expected type: number"), "got: {err}");
    }

    #[test]
    fn validates_array_items() {
        let schema = json!({
            "type": "object",
            "properties": {
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": {"type": "string"},
                            "newText": {"type": "string"}
                        },
                        "required": ["oldText", "newText"]
                    }
                }
            },
            "required": ["edits"]
        });
        assert!(validate_tool_arguments(
            "edit",
            &schema,
            &json!({"edits": [{"oldText": "a", "newText": "b"}]})
        )
        .is_ok());
        let err = validate_tool_arguments("edit", &schema, &json!({"edits": [{"oldText": "a"}]}))
            .unwrap_err();
        assert!(
            err.contains("edits[0].newText: Required property missing"),
            "got: {err}"
        );
    }

    #[test]
    fn drops_optional_nulls() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "limit": {"type": "number"}
            },
            "required": ["path"]
        });
        let validated =
            validate_tool_arguments("read", &schema, &json!({"path": "a", "limit": null})).unwrap();
        assert!(
            validated.get("limit").is_none(),
            "optional null should be dropped: {validated}"
        );
    }
}
