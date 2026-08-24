//! Tool-arguments JSON-schema validation — port of
//! `packages/ai/src/utils/validation.ts` (`validateToolArguments`).
//!
//! Validates raw tool-call arguments against the tool's JSON-schema
//! `parameters`, coercing primitives (string↔number↔boolean) the way the
//! upstream TypeBox `Value.Convert` + `coerceWithJsonSchema` pass does, and
//! formatting failures as `Validation failed for tool "X":\n  - path: msg`.

use regex::Regex;
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
                if let Ok(parsed) = s.trim().parse::<f64>() {
                    if parsed.is_finite() && parsed.fract() == 0.0 {
                        if let Ok(integer) = parsed.to_string().parse::<i64>() {
                            return Value::Number(integer.into());
                        }
                        if let Some(number) = serde_json::Number::from_f64(parsed) {
                            return Value::Number(number);
                        }
                    }
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
        "integer" => value
            .as_f64()
            .is_some_and(|number| number.is_finite() && number.fract() == 0.0),
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
fn coerce_with_json_schema(value: &Value, schema: &Value, root: &Value) -> Value {
    let schema = resolve_local_ref(schema, root).unwrap_or(schema);
    let mut next = value.clone();

    if let Some(all_of) = schema.get("allOf").and_then(|v| v.as_array()) {
        for nested in all_of {
            next = coerce_with_json_schema(&next, nested, root);
        }
    }
    if let Some(any_of) = schema.get("anyOf").and_then(|v| v.as_array()) {
        next = coerce_with_union(&next, any_of, root);
    }
    if let Some(one_of) = schema.get("oneOf").and_then(|v| v.as_array()) {
        next = coerce_with_union(&next, one_of, root);
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
                        let coerced = coerce_with_json_schema(field, property_schema, root);
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
                            let coerced = coerce_with_json_schema(field, additional, root);
                            *field = coerced;
                        }
                    }
                }
            }
        }
    }

    if types.iter().any(|t| t == "array") && next.is_array() {
        if let Some(items) = schema.get("items") {
            if let Some(tuple) = items.as_array() {
                if let Some(arr) = next.as_array_mut() {
                    for (index, item_schema) in tuple.iter().enumerate() {
                        if let Some(item) = arr.get_mut(index) {
                            *item = coerce_with_json_schema(item, item_schema, root);
                        }
                    }
                }
            } else if let Some(arr) = next.as_array_mut() {
                for item in arr.iter_mut() {
                    *item = coerce_with_json_schema(item, items, root);
                }
            }
        }
    }

    next
}

/// Try each union member; return the first that validates, else the first
/// coercion that validates, else the original (upstream `coerceWithUnionSchema`).
fn coerce_with_union(value: &Value, schemas: &[Value], root: &Value) -> Value {
    for schema in schemas {
        if schema_matches(value, schema, root) {
            return value.clone();
        }
    }
    for schema in schemas {
        let candidate = coerce_with_json_schema(value, schema, root);
        if schema_matches(&candidate, schema, root) {
            return candidate;
        }
    }
    value.clone()
}

fn resolve_local_ref<'a>(schema: &'a Value, root: &'a Value) -> Option<&'a Value> {
    let reference = schema.get("$ref")?.as_str()?;
    let mut resolved = root;
    for segment in reference.strip_prefix("#/")?.split('/') {
        let segment = segment.replace("~1", "/").replace("~0", "~");
        resolved = resolved.get(segment)?;
    }
    Some(resolved)
}

fn schema_accepts_null(schema: &Value, root: &Value) -> bool {
    if let Some(resolved) = resolve_local_ref(schema, root) {
        return schema_accepts_null(resolved, root);
    }
    if schema_types(schema)
        .iter()
        .any(|type_name| type_name == "null")
    {
        return true;
    }
    ["anyOf", "oneOf", "allOf"].iter().any(|key| {
        schema
            .get(*key)
            .and_then(Value::as_array)
            .is_some_and(|schemas| {
                schemas
                    .iter()
                    .any(|nested| schema_accepts_null(nested, root))
            })
    })
}

/// Remove null values for non-required optional properties (upstream
/// `normalizeOptionalNulls`).
fn normalize_optional_nulls(value: &mut Value, schema: &Value, root: &Value) {
    if let Some(arr) = value.as_array_mut() {
        if let Some(items) = schema.get("items") {
            if let Some(tuple) = items.as_array() {
                for (index, item_schema) in tuple.iter().enumerate() {
                    if let Some(item) = arr.get_mut(index) {
                        normalize_optional_nulls(item, item_schema, root);
                    }
                }
            } else {
                for item in arr.iter_mut() {
                    normalize_optional_nulls(item, items, root);
                }
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
            if field.is_null()
                && !required.contains(key)
                && !schema_accepts_null(property_schema, root)
            {
                // Optional null: drop it (upstream deletes the key).
                obj.remove(key);
            } else {
                normalize_optional_nulls(field, property_schema, root);
            }
        }
    }
}

/// Validate a value against a schema, returning a list of (path, message)
/// errors (upstream TypeBox `validator.Errors`).
fn validate_value(value: &Value, schema: &Value) -> Result<(), Vec<(String, String)>> {
    let mut errors = Vec::new();
    validate_value_at(value, schema, "", &mut errors, schema);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn child_path(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

fn array_path(path: &str, index: usize) -> String {
    format!("{path}[{index}]")
}

fn schema_matches(value: &Value, schema: &Value, root: &Value) -> bool {
    let mut errors = Vec::new();
    validate_value_at(value, schema, "", &mut errors, root);
    errors.is_empty()
}

fn number_keyword(schema: &Value, key: &str) -> Option<f64> {
    schema.get(key).and_then(Value::as_f64)
}

fn string_length(value: &str) -> usize {
    // JavaScript's String.length counts UTF-16 code units. TypeBox follows
    // that convention, so Rust's Unicode scalar count would drift for astral
    // characters.
    value.encode_utf16().count()
}

fn valid_format(value: &str, format: &str) -> bool {
    match format {
        "email" => Regex::new(r"^[^\s@]+@[^\s@]+\.[^\s@]+$")
            .map(|re| re.is_match(value))
            .unwrap_or(false),
        "uuid" => Regex::new(
            r"(?i)^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$",
        )
        .map(|re| re.is_match(value))
        .unwrap_or(false),
        "date" => Regex::new(r"^\d{4}-\d{2}-\d{2}$")
            .map(|re| re.is_match(value))
            .unwrap_or(false),
        "time" => Regex::new(r"^\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})$")
            .map(|re| re.is_match(value))
            .unwrap_or(false),
        "date-time" => value
            .split_once('T')
            .is_some_and(|(date, time)| valid_format(date, "date") && valid_format(time, "time")),
        "uri" | "uri-reference" => {
            !value.chars().any(char::is_whitespace)
                && (format == "uri-reference"
                    || Regex::new(r"^[A-Za-z][A-Za-z0-9+.-]*:[^\s]+$")
                        .map(|re| re.is_match(value))
                        .unwrap_or(false))
        }
        "hostname" => {
            value.len() <= 253
                && Regex::new(
                    r"^(?:[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?\.)*[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?$",
                )
                .map(|re| re.is_match(value))
                .unwrap_or(false)
        }
        "ipv4" => value.parse::<std::net::Ipv4Addr>().is_ok(),
        "ipv6" => value.parse::<std::net::Ipv6Addr>().is_ok(),
        "regex" => Regex::new(value).is_ok(),
        // TypeBox's compiler permits unknown format names when no custom
        // format registry is installed. Keep those schemas non-blocking.
        _ => true,
    }
}

fn validate_value_at(
    value: &Value,
    schema: &Value,
    path: &str,
    errors: &mut Vec<(String, String)>,
    root: &Value,
) {
    if let Some(resolved) = resolve_local_ref(schema, root) {
        validate_value_at(value, resolved, path, errors, root);
        return;
    }
    if let Some(constant) = schema.get("const") {
        if value != constant {
            errors.push((path.to_string(), "must be equal to constant".to_string()));
        }
    }
    if let Some(enum_values) = schema.get("enum").and_then(Value::as_array) {
        if !enum_values.iter().any(|candidate| candidate == value) {
            errors.push((
                path.to_string(),
                "must be equal to one of the allowed values".to_string(),
            ));
        }
    }

    if let Some(all_of) = schema.get("allOf").and_then(Value::as_array) {
        for nested in all_of {
            validate_value_at(value, nested, path, errors, root);
        }
    }
    if let Some(any_of) = schema.get("anyOf").and_then(Value::as_array) {
        if !any_of
            .iter()
            .any(|nested| schema_matches(value, nested, root))
        {
            errors.push((
                path.to_string(),
                "must match at least one schema in anyOf".to_string(),
            ));
        }
    }
    if let Some(one_of) = schema.get("oneOf").and_then(Value::as_array) {
        let matches = one_of
            .iter()
            .filter(|nested| schema_matches(value, nested, root))
            .count();
        if matches != 1 {
            errors.push((
                path.to_string(),
                "must match exactly one schema in oneOf".to_string(),
            ));
        }
    }
    if let Some(not) = schema.get("not") {
        if schema_matches(value, not, root) {
            errors.push((
                path.to_string(),
                "must not match the schema in not".to_string(),
            ));
        }
    }
    if let Some(if_schema) = schema.get("if") {
        let branch = if schema_matches(value, if_schema, root) {
            schema.get("then")
        } else {
            schema.get("else")
        };
        if let Some(branch) = branch {
            validate_value_at(value, branch, path, errors, root);
        }
    }

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

    if let Some(object) = value.as_object() {
        if let Some(minimum) = schema.get("minProperties").and_then(Value::as_u64) {
            if object.len() < minimum as usize {
                errors.push((
                    path.to_string(),
                    format!("must NOT have fewer than {minimum} properties"),
                ));
            }
        }
        if let Some(maximum) = schema.get("maxProperties").and_then(Value::as_u64) {
            if object.len() > maximum as usize {
                errors.push((
                    path.to_string(),
                    format!("must NOT have more than {maximum} properties"),
                ));
            }
        }

        if let Some(properties) = schema.get("properties").and_then(|v| v.as_object()) {
            for (key, property_schema) in properties {
                if let Some(field) = object.get(key) {
                    let child_path = child_path(path, key);
                    validate_value_at(field, property_schema, &child_path, errors, root);
                }
            }
        }
        if let Some(required) = schema.get("required").and_then(|v| v.as_array()) {
            for req in required.iter().filter_map(|v| v.as_str()) {
                if !object.contains_key(req) {
                    errors.push((
                        child_path(path, req),
                        "Required property missing".to_string(),
                    ));
                }
            }
        }

        let defined: std::collections::HashSet<&str> = schema
            .get("properties")
            .and_then(Value::as_object)
            .map(|properties| properties.keys().map(String::as_str).collect())
            .unwrap_or_default();
        let patterns = schema
            .get("patternProperties")
            .and_then(Value::as_object)
            .map(|patterns| {
                patterns
                    .iter()
                    .filter_map(|(pattern, nested)| {
                        Regex::new(pattern).ok().map(|regex| (regex, nested))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for (key, field) in object {
            let mut matched_pattern = false;
            for (pattern, nested) in &patterns {
                if pattern.is_match(key) {
                    matched_pattern = true;
                    validate_value_at(field, nested, &child_path(path, key), errors, root);
                }
            }
            if !defined.contains(key.as_str()) && !matched_pattern {
                match schema.get("additionalProperties") {
                    Some(Value::Bool(false)) => errors.push((
                        child_path(path, key),
                        "must NOT have additional properties".to_string(),
                    )),
                    Some(nested) if nested.is_object() => {
                        validate_value_at(field, nested, &child_path(path, key), errors, root)
                    }
                    _ => {}
                }
            }
        }
        if let Some(property_names) = schema.get("propertyNames") {
            for key in object.keys() {
                validate_value_at(
                    &Value::String(key.clone()),
                    property_names,
                    &child_path(path, key),
                    errors,
                    root,
                );
            }
        }
    }

    if let Some(array) = value.as_array() {
        if let Some(minimum) = schema.get("minItems").and_then(Value::as_u64) {
            if array.len() < minimum as usize {
                errors.push((
                    path.to_string(),
                    format!("must NOT have fewer than {minimum} items"),
                ));
            }
        }
        if let Some(maximum) = schema.get("maxItems").and_then(Value::as_u64) {
            if array.len() > maximum as usize {
                errors.push((
                    path.to_string(),
                    format!("must NOT have more than {maximum} items"),
                ));
            }
        }
        if schema
            .get("uniqueItems")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            for (index, item) in array.iter().enumerate() {
                if array[..index].iter().any(|previous| previous == item) {
                    errors.push((
                        array_path(path, index),
                        "must NOT have duplicate items".to_string(),
                    ));
                    break;
                }
            }
        }
        if let Some(items) = schema.get("items") {
            if let Some(tuple) = items.as_array() {
                for (index, item_schema) in tuple.iter().enumerate() {
                    if let Some(item) = array.get(index) {
                        validate_value_at(
                            item,
                            item_schema,
                            &array_path(path, index),
                            errors,
                            root,
                        );
                    }
                }
                if array.len() > tuple.len() {
                    match schema.get("additionalItems") {
                        Some(Value::Bool(false)) => errors.push((
                            array_path(path, tuple.len()),
                            "must NOT have additional items".to_string(),
                        )),
                        Some(nested) if nested.is_object() => {
                            for (index, item) in array.iter().enumerate().skip(tuple.len()) {
                                validate_value_at(
                                    item,
                                    nested,
                                    &array_path(path, index),
                                    errors,
                                    root,
                                );
                            }
                        }
                        _ => {}
                    }
                }
            } else {
                for (index, item) in array.iter().enumerate() {
                    validate_value_at(item, items, &array_path(path, index), errors, root);
                }
            }
        }
        if let Some(contains) = schema.get("contains") {
            if !array
                .iter()
                .any(|item| schema_matches(item, contains, root))
            {
                errors.push((
                    path.to_string(),
                    "must contain at least one valid item".to_string(),
                ));
            }
        }
    }

    if let Some(string) = value.as_str() {
        let length = string_length(string);
        if let Some(minimum) = schema.get("minLength").and_then(Value::as_u64) {
            if length < minimum as usize {
                errors.push((
                    path.to_string(),
                    format!("must NOT be shorter than {minimum} characters"),
                ));
            }
        }
        if let Some(maximum) = schema.get("maxLength").and_then(Value::as_u64) {
            if length > maximum as usize {
                errors.push((
                    path.to_string(),
                    format!("must NOT be longer than {maximum} characters"),
                ));
            }
        }
        if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
            match Regex::new(pattern) {
                Ok(regex) if !regex.is_match(string) => {
                    errors.push((path.to_string(), "must match pattern".to_string()))
                }
                Err(_) => errors.push((
                    path.to_string(),
                    "schema contains an invalid pattern".to_string(),
                )),
                _ => {}
            }
        }
        if let Some(format) = schema.get("format").and_then(Value::as_str) {
            if !valid_format(string, format) {
                errors.push((path.to_string(), format!("must match format: {format}")));
            }
        }
    }

    if let Some(number) = value.as_f64() {
        if let Some(minimum) = number_keyword(schema, "minimum") {
            if number < minimum {
                errors.push((path.to_string(), format!("must be >= {minimum}")));
            }
        }
        if let Some(maximum) = number_keyword(schema, "maximum") {
            if number > maximum {
                errors.push((path.to_string(), format!("must be <= {maximum}")));
            }
        }
        if let Some(exclusive) = schema.get("exclusiveMinimum") {
            let bound = exclusive.as_f64().or_else(|| {
                exclusive
                    .as_bool()
                    .filter(|enabled| *enabled)
                    .and_then(|_| number_keyword(schema, "minimum"))
            });
            if let Some(bound) = bound {
                if number <= bound {
                    errors.push((path.to_string(), format!("must be > {bound}")));
                }
            }
        }
        if let Some(exclusive) = schema.get("exclusiveMaximum") {
            let bound = exclusive.as_f64().or_else(|| {
                exclusive
                    .as_bool()
                    .filter(|enabled| *enabled)
                    .and_then(|_| number_keyword(schema, "maximum"))
            });
            if let Some(bound) = bound {
                if number >= bound {
                    errors.push((path.to_string(), format!("must be < {bound}")));
                }
            }
        }
        if let Some(divisor) = number_keyword(schema, "multipleOf") {
            if divisor == 0.0 || ((number / divisor) - (number / divisor).round()).abs() > 1e-9 {
                errors.push((path.to_string(), format!("must be a multiple of {divisor}")));
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
    normalize_optional_nulls(&mut args, parameters, parameters);
    let coerced = coerce_with_json_schema(&args, parameters, parameters);
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

    #[test]
    fn preserves_optional_nulls_for_nullable_unions() {
        let schema = json!({
            "type": "object",
            "properties": {
                "value": {"anyOf": [{"type": "number"}, {"type": "null"}]},
                "other": {"oneOf": [{"type": "string"}, {"type": "null"}]}
            }
        });
        let validated =
            validate_tool_arguments("nullable", &schema, &json!({"value": null, "other": null}))
                .unwrap();
        assert_eq!(validated, json!({"value": null, "other": null}));
    }

    #[test]
    fn resolves_local_refs_for_validation_and_null_normalization() {
        let schema = json!({
            "type": "object",
            "properties": {
                "value": {"$ref": "#/$defs/value"},
                "count": {"$ref": "#/$defs/count"}
            },
            "$defs": {
                "value": {"anyOf": [{"type": "number"}, {"type": "null"}]},
                "count": {"type": "integer", "minimum": 1}
            }
        });
        let validated =
            validate_tool_arguments("refs", &schema, &json!({"value": null, "count": "2"}))
                .unwrap();
        assert_eq!(validated, json!({"value": null, "count": 2}));

        let err = validate_tool_arguments("refs", &schema, &json!({"count": 0})).unwrap_err();
        assert!(err.contains("minimum") || err.contains(">="), "got: {err}");
    }

    #[test]
    fn validates_unions_and_all_of_instead_of_only_coercing_them() {
        let schema = json!({
            "type": "object",
            "properties": {
                "value": {
                    "anyOf": [{"type": "number"}, {"type": "null"}]
                },
                "bounded": {
                    "allOf": [{"type": "number"}, {"minimum": 1, "maximum": 3}]
                }
            },
            "required": ["value", "bounded"]
        });
        let validated =
            validate_tool_arguments("union", &schema, &json!({"value": "2", "bounded": 2}))
                .unwrap();
        assert_eq!(validated["value"], json!(2.0));

        let err = validate_tool_arguments(
            "union",
            &schema,
            &json!({"value": {"nested": true}, "bounded": 4}),
        )
        .unwrap_err();
        assert!(err.contains("anyOf"), "got: {err}");
        assert!(
            err.contains("maximum") || err.contains("<=") || err.contains(">="),
            "got: {err}"
        );
    }

    #[test]
    fn validates_tuple_arrays_and_additional_items() {
        let schema = json!({
            "type": "array",
            "items": [{"type": "string"}, {"type": "integer"}],
            "additionalItems": false,
            "minItems": 2,
            "maxItems": 2
        });
        let validated = validate_tool_arguments("tuple", &schema, &json!(["name", "2"])).unwrap();
        assert_eq!(validated, json!(["name", 2]));

        let err = validate_tool_arguments("tuple", &schema, &json!(["name", 2, true])).unwrap_err();
        assert!(
            err.contains("additional items") || err.contains("more than 2"),
            "got: {err}"
        );
    }

    #[test]
    fn validates_object_properties_bounds_formats_and_unknown_keys() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "minLength": 3, "pattern": "^[A-Z]"},
                "age": {"type": "integer", "minimum": 18, "maximum": 120},
                "email": {"type": "string", "format": "email"}
            },
            "required": ["name", "age", "email"],
            "additionalProperties": false
        });
        assert!(validate_tool_arguments(
            "profile",
            &schema,
            &json!({"name": "Ada", "age": 37, "email": "ada@example.com"})
        )
        .is_ok());

        let err = validate_tool_arguments(
            "profile",
            &schema,
            &json!({"name": "ada", "age": 17, "email": "not-an-email", "extra": true}),
        )
        .unwrap_err();
        assert!(err.contains("pattern"), "got: {err}");
        assert!(err.contains("minimum") || err.contains(">="), "got: {err}");
        assert!(err.contains("format"), "got: {err}");
        assert!(err.contains("additional"), "got: {err}");
    }

    #[test]
    fn validates_enum_const_unique_items_and_numeric_bounds() {
        let schema = json!({
            "type": "object",
            "properties": {
                "mode": {"enum": ["fast", "safe"]},
                "kind": {"const": "profile"},
                "values": {"type": "array", "items": {"type": "integer"}, "uniqueItems": true},
                "step": {"type": "number", "multipleOf": 0.5, "exclusiveMinimum": 0}
            },
            "required": ["mode", "kind", "values", "step"]
        });
        assert!(validate_tool_arguments(
            "constraints",
            &schema,
            &json!({"mode": "fast", "kind": "profile", "values": [1, 2], "step": 1.5})
        )
        .is_ok());

        let err = validate_tool_arguments(
            "constraints",
            &schema,
            &json!({"mode": "slow", "kind": "other", "values": [1, 1], "step": 1.25}),
        )
        .unwrap_err();
        assert!(err.contains("allowed values"), "got: {err}");
        assert!(err.contains("constant"), "got: {err}");
        assert!(err.contains("duplicate"), "got: {err}");
        assert!(err.contains("multiple"), "got: {err}");
    }
}
