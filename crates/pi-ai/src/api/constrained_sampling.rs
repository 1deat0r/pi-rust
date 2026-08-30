//! Shared constrained-sampling helpers.
//!
//! This is the Rust port of `packages/ai/src/api/constrained-sampling.ts`.
//! Providers use the same JSON-schema subset and OpenAI grammar-tool contract;
//! keeping the resolver here prevents adaptor-specific fallback drift.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::types::{ConstrainedSampling, StrictPreference, Tool};

const UNSUPPORTED_STRICT_SCHEMA_KEYS: &[&str] = &[
    "$ref",
    "$defs",
    "definitions",
    "allOf",
    "oneOf",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
    "unevaluatedProperties",
    "propertyNames",
    "contains",
    "prefixItems",
    "not",
    "if",
    "then",
    "else",
];

fn is_structured_schema(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };
    let type_is_structured = match object.get("type") {
        Some(Value::String(value)) => matches!(value.as_str(), "object" | "array"),
        Some(Value::Array(values)) => values.iter().any(|value| {
            value
                .as_str()
                .is_some_and(|value| matches!(value, "object" | "array"))
        }),
        _ => false,
    };
    type_is_structured || object.contains_key("properties") || object.contains_key("items")
}

fn schema_allows_null(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };
    match object.get("type") {
        Some(Value::String(value)) if value == "null" => return true,
        Some(Value::Array(values)) if values.iter().any(|value| value.as_str() == Some("null")) => {
            return true
        }
        _ => {}
    }
    if object.get("const") == Some(&Value::Null) {
        return true;
    }
    if object
        .get("enum")
        .and_then(Value::as_array)
        .is_some_and(|values| values.iter().any(Value::is_null))
    {
        return true;
    }
    object
        .get("anyOf")
        .and_then(Value::as_array)
        .is_some_and(|values| values.iter().any(schema_allows_null))
}

fn make_json_schema_node_strict(schema: &mut Value) -> Result<(), String> {
    let Some(object) = schema.as_object_mut() else {
        return Err("boolean schemas are unsupported".to_string());
    };

    for key in UNSUPPORTED_STRICT_SCHEMA_KEYS {
        if object.contains_key(*key) {
            return Err(format!("{key} schemas are unsupported"));
        }
    }

    if let Some(any_of) = object.get("anyOf") {
        let variants = any_of
            .as_array()
            .ok_or_else(|| "anyOf must contain at least one schema".to_string())?;
        if variants.is_empty() {
            return Err("anyOf must contain at least one schema".to_string());
        }
        if variants.iter().any(is_structured_schema) {
            return Err("object and array unions are unsupported".to_string());
        }
    }
    if let Some(any_of) = object.get_mut("anyOf") {
        let variants = any_of
            .as_array_mut()
            .ok_or_else(|| "anyOf must contain at least one schema".to_string())?;
        for variant in variants {
            make_json_schema_node_strict(variant)?;
        }
    }

    if let Some(items) = object.get("items") {
        if items.is_array() {
            return Err("tuple schemas are unsupported".to_string());
        }
    }
    if let Some(items) = object.get_mut("items") {
        make_json_schema_node_strict(items)?;
    }

    let is_object_schema = object.get("type").and_then(Value::as_str) == Some("object");
    if object.contains_key("properties") && !is_object_schema {
        return Err("properties require type object".to_string());
    }
    if !is_object_schema {
        return Ok(());
    }

    if let Some(additional_properties) = object.get("additionalProperties") {
        if additional_properties != &Value::Bool(false) {
            return Err("schema-valued or true additionalProperties is unsupported".to_string());
        }
    }
    if let Some(properties) = object.get("properties") {
        if !properties.is_object() {
            return Err("object properties must be a schema map".to_string());
        }
    }
    if let Some(required) = object.get("required") {
        if !required
            .as_array()
            .is_some_and(|values| values.iter().all(Value::is_string))
        {
            return Err("object required must be a string array".to_string());
        }
    }

    let had_properties = object.contains_key("properties");
    let properties: Vec<(String, Value)> = object
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| {
            properties
                .iter()
                .map(|(name, schema)| (name.clone(), schema.clone()))
                .collect()
        })
        .unwrap_or_default();
    let property_names: Vec<String> = properties.iter().map(|(name, _)| name.clone()).collect();
    let required: BTreeSet<String> = object
        .get("required")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if required
        .iter()
        .any(|name| !property_names.iter().any(|property| property == name))
    {
        return Err("required contains an unknown property".to_string());
    }

    let mut strict_properties = serde_json::Map::new();
    for (name, mut property) in properties {
        make_json_schema_node_strict(&mut property)?;
        if !required.contains(&name) && !schema_allows_null(&property) {
            property = json!({ "anyOf": [property, { "type": "null" }] });
        }
        strict_properties.insert(name, property);
    }
    if had_properties {
        object.insert("properties".to_string(), Value::Object(strict_properties));
    }
    object.insert("required".to_string(), json!(property_names));
    object.insert("additionalProperties".to_string(), Value::Bool(false));
    Ok(())
}

/// Convert a schema to the strict JSON-schema subset accepted by providers.
pub fn make_strict_json_schema(schema: &Value) -> Result<Value, String> {
    let mut cloned = schema.clone();
    if !cloned.is_object() {
        return Err("root schema must have type object".to_string());
    }
    make_json_schema_node_strict(&mut cloned)?;
    if cloned.get("type").and_then(Value::as_str) != Some("object") {
        return Err("root schema must have type object".to_string());
    }
    Ok(cloned)
}

/// Return provider parameters without mutating the caller-owned tool schema.
pub fn get_json_schema_tool_parameters(tool: &Tool, strict: Option<bool>) -> Result<Value, String> {
    if strict == Some(true) {
        make_strict_json_schema(&tool.parameters)
    } else {
        Ok(tool.parameters.clone())
    }
}

/// Resolve a tool's JSON-schema constrained-sampling request.
pub fn resolve_json_schema_strict_sampling(
    tool: &Tool,
    supports_strict_mode: bool,
) -> Result<Option<bool>, String> {
    let Some(ConstrainedSampling::JsonSchema { strict }) = &tool.constrained_sampling else {
        return Ok(None);
    };

    if supports_strict_mode {
        match make_strict_json_schema(&tool.parameters) {
            Ok(_) => Ok(Some(true)),
            Err(_) if *strict == StrictPreference::Prefer => Ok(None),
            Err(error) => Err(format!(
                "Tool \"{}\" requires JSON-schema constrained sampling, but {}.",
                tool.name, error
            )),
        }
    } else if *strict == StrictPreference::Require {
        Err(format!(
            "Tool \"{}\" requires JSON-schema constrained sampling, but strict tools are unsupported.",
            tool.name
        ))
    } else {
        Ok(None)
    }
}

/// OpenAI custom grammar tool configuration selected from a tool's variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrammarConstrainedSampling {
    pub format: String,
    pub definition: String,
    pub input_property: String,
}

/// State used to turn streamed grammar input into JSON tool-call deltas.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrammarToolInputJsonBuffer {
    pub input: String,
    pub started: bool,
    pub closed: bool,
}

/// Extract the single string argument required by an OpenAI grammar tool.
pub fn get_grammar_tool_input(
    tool_name: &str,
    arguments: &Value,
    input_property: &str,
) -> Result<String, String> {
    let input = arguments
        .as_object()
        .and_then(|arguments| arguments.get(input_property))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            format!(
                "Grammar tool call \"{tool_name}\" requires argument \"{input_property}\" to be a string."
            )
        })?;
    Ok(input.to_string())
}

#[allow(clippy::expect_used)] // invariant: serializing a &str cannot fail
fn json_string_fragment(value: &str) -> String {
    let encoded = serde_json::to_string(value).expect("JSON strings are serializable");
    encoded[1..encoded.len() - 1].to_string()
}

/// Append an OpenAI grammar tool input delta while enforcing monotonic input.
pub fn append_grammar_tool_input_json_delta(
    buffer: &mut GrammarToolInputJsonBuffer,
    input_property: &str,
    next_input: &str,
    close: bool,
) -> Result<Option<String>, String> {
    if buffer.closed {
        if close && next_input == buffer.input {
            return Ok(None);
        }
        return Err(format!(
            "grammar tool input for property \"{input_property}\" changed after it was closed"
        ));
    }
    if !next_input.starts_with(&buffer.input) {
        return Err(format!(
            "grammar tool input for property \"{input_property}\" changed non-monotonically"
        ));
    }

    let input_delta = &next_input[buffer.input.len()..];
    if !close && input_delta.is_empty() {
        return Ok(None);
    }

    let mut delta = String::new();
    if !buffer.started {
        delta.push('{');
        #[allow(clippy::expect_used)] // invariant: serializing a &str cannot fail
        delta.push_str(
            &serde_json::to_string(input_property).expect("JSON strings are serializable"),
        );
        delta.push_str(":\"");
        buffer.started = true;
    }
    delta.push_str(&json_string_fragment(input_delta));
    buffer.input = next_input.to_string();

    if close {
        delta.push_str("\"}");
        buffer.closed = true;
    }
    Ok(Some(delta))
}

fn infer_grammar_input_property(tool: &Tool) -> Result<String, String> {
    let schema = tool.parameters.as_object().ok_or_else(|| {
        "grammar constrained sampling requires an object parameter schema".to_string()
    })?;
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err("grammar constrained sampling requires an object parameter schema".to_string());
    }
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            "grammar constrained sampling requires exactly one required string property".to_string()
        })?;
    if required.len() != 1 || !required[0].is_string() {
        return Err(
            "grammar constrained sampling requires exactly one required string property"
                .to_string(),
        );
    }
    #[allow(clippy::expect_used)] // invariant: string-ness checked directly above
    let input_property = required[0]
        .as_str()
        .expect("required[0] was checked as a string")
        .to_string();
    let property = schema
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get(&input_property))
        .ok_or_else(|| {
            format!("grammar constrained sampling requires a properties entry for {input_property}")
        })?;
    if property.get("type").and_then(Value::as_str) != Some("string") {
        return Err(format!(
            "grammar constrained sampling property {input_property} must have type string"
        ));
    }
    Ok(input_property)
}

/// Resolve an OpenAI Lark/regex grammar when the provider advertises support.
pub fn resolve_grammar_constrained_sampling(
    tool: &Tool,
    supports_openai_grammar_tools: bool,
) -> Result<Option<GrammarConstrainedSampling>, String> {
    let Some(ConstrainedSampling::Grammar { variants }) = &tool.constrained_sampling else {
        return Ok(None);
    };
    if !supports_openai_grammar_tools {
        return Ok(None);
    }

    let lark = variants
        .get("openai_lark")
        .filter(|definition| !definition.trim().is_empty());
    let regex = variants
        .get("openai_regex")
        .filter(|definition| !definition.trim().is_empty());
    let (format, definition) = match (lark, regex) {
        (Some(definition), _) => ("lark", definition.clone()),
        (None, Some(definition)) => ("regex", definition.clone()),
        (None, None) => {
            return Err(format!(
                "Tool \"{}\" cannot use grammar constrained sampling: no supported grammar variant was provided.",
                tool.name
            ))
        }
    };
    infer_grammar_input_property(tool)
        .map(|input_property| {
            Some(GrammarConstrainedSampling {
                format: format.to_string(),
                definition,
                input_property,
            })
        })
        .map_err(|error| {
            format!(
                "Tool \"{}\" cannot use grammar constrained sampling: {error}.",
                tool.name
            )
        })
}

/// Build the tool-name → input-property map used by OpenAI custom tool replay.
pub fn create_grammar_tool_input_properties(
    tools: &[Tool],
    supports_openai_grammar_tools: bool,
) -> Result<BTreeMap<String, String>, String> {
    let mut properties = BTreeMap::new();
    for tool in tools {
        if let Some(grammar) =
            resolve_grammar_constrained_sampling(tool, supports_openai_grammar_tools)?
        {
            properties.insert(tool.name.clone(), grammar.input_property);
        }
    }
    Ok(properties)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn tool(parameters: Value, constrained_sampling: Option<ConstrainedSampling>) -> Tool {
        Tool {
            name: "sample_tool".to_string(),
            description: "Sample tool".to_string(),
            parameters,
            constrained_sampling,
        }
    }

    fn json_schema_tool(strict: StrictPreference, parameters: Value) -> Tool {
        tool(parameters, Some(ConstrainedSampling::JsonSchema { strict }))
    }

    #[test]
    fn strictifies_optional_nested_properties_without_mutating_source() {
        let parameters = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "metadata": {"type": "object", "properties": {"enabled": {"type": "boolean"}}}
            },
            "required": ["path"]
        });
        let strict = make_strict_json_schema(&parameters).unwrap();
        assert_eq!(parameters["required"], json!(["path"]));
        assert_eq!(strict["required"], json!(["path", "metadata"]));
        assert_eq!(strict["additionalProperties"], json!(false));
        assert_eq!(
            strict["properties"]["metadata"]["anyOf"][0]["required"],
            json!(["enabled"])
        );
        assert_eq!(
            strict["properties"]["metadata"]["anyOf"][0]["additionalProperties"],
            json!(false)
        );
        assert_eq!(
            strict["properties"]["metadata"]["anyOf"][0]["properties"]["enabled"]["anyOf"][1],
            json!({"type":"null"})
        );
    }

    #[test]
    fn strict_schema_rejects_unsupported_shapes_and_preserves_prefer_fallback() {
        let cases = [
            (
                json!({"type":"object","properties":{},"additionalProperties":true}),
                "schema-valued or true additionalProperties is unsupported",
            ),
            (
                json!({"allOf": [{"type":"object"}]}),
                "allOf schemas are unsupported",
            ),
            (
                json!({"type":"object","properties":{"value":{"anyOf":[{"type":"object"},{"type":"null"}]}}}),
                "object and array unions are unsupported",
            ),
            (
                json!({"type":"object","properties":{"child":{"$ref":"x"}},"required":["child"]}),
                "$ref schemas are unsupported",
            ),
            (
                json!({"type":"object","properties":{},"anyOf":[]}),
                "anyOf must contain at least one schema",
            ),
            (
                json!({"type":"object","properties":{"items":{"type":"array","items":[{"type":"string"}]}}}),
                "tuple schemas are unsupported",
            ),
        ];
        for (parameters, reason) in cases {
            let prefer = json_schema_tool(StrictPreference::Prefer, parameters.clone());
            assert_eq!(
                resolve_json_schema_strict_sampling(&prefer, true).unwrap(),
                None
            );
            assert_eq!(make_strict_json_schema(&parameters).unwrap_err(), reason);
            let require = json_schema_tool(StrictPreference::Require, parameters);
            assert_eq!(
                resolve_json_schema_strict_sampling(&require, true).unwrap_err(),
                format!(
                    "Tool \"sample_tool\" requires JSON-schema constrained sampling, but {reason}."
                )
            );
        }
    }

    #[test]
    fn strict_require_rejects_unsupported_provider() {
        let tool = json_schema_tool(StrictPreference::Require, json!({"type":"object"}));
        assert_eq!(
            resolve_json_schema_strict_sampling(&tool, false).unwrap_err(),
            "Tool \"sample_tool\" requires JSON-schema constrained sampling, but strict tools are unsupported."
        );
    }

    #[test]
    fn grammar_prefers_lark_and_falls_back_to_regex() {
        let mut variants = BTreeMap::new();
        variants.insert("openai_regex".to_string(), "[a-z]+".to_string());
        variants.insert("openai_lark".to_string(), "start: /[a-z]+/".to_string());
        let grammar = tool(
            json!({"type":"object","properties":{"payload":{"type":"string"}},"required":["payload"]}),
            Some(ConstrainedSampling::Grammar { variants }),
        );
        assert_eq!(
            resolve_grammar_constrained_sampling(&grammar, true).unwrap(),
            Some(GrammarConstrainedSampling {
                format: "lark".to_string(),
                definition: "start: /[a-z]+/".to_string(),
                input_property: "payload".to_string()
            })
        );
        let mut regex_variants = BTreeMap::new();
        regex_variants.insert("openai_regex".to_string(), "[a-z]+".to_string());
        let regex_tool = Tool {
            constrained_sampling: Some(ConstrainedSampling::Grammar {
                variants: regex_variants,
            }),
            ..grammar
        };
        assert_eq!(
            resolve_grammar_constrained_sampling(&regex_tool, true)
                .unwrap()
                .unwrap()
                .format,
            "regex"
        );
    }

    #[test]
    fn grammar_rejects_missing_variant_and_bad_schema_with_upstream_text() {
        let mut variants = BTreeMap::new();
        variants.insert("other".to_string(), "x".to_string());
        let missing = tool(
            json!({"type":"object","properties":{"payload":{"type":"string"}},"required":["payload"]}),
            Some(ConstrainedSampling::Grammar { variants }),
        );
        assert_eq!(
            resolve_grammar_constrained_sampling(&missing, true).unwrap_err(),
            "Tool \"sample_tool\" cannot use grammar constrained sampling: no supported grammar variant was provided."
        );
        let mut grammar_variants = BTreeMap::new();
        grammar_variants.insert("openai_lark".to_string(), "start: /x/".to_string());
        let bad_schema = tool(
            json!({"type":"object","properties":{"payload":{"type":"number"}},"required":["payload"]}),
            Some(ConstrainedSampling::Grammar {
                variants: grammar_variants,
            }),
        );
        assert_eq!(
            resolve_grammar_constrained_sampling(&bad_schema, true).unwrap_err(),
            "Tool \"sample_tool\" cannot use grammar constrained sampling: grammar constrained sampling property payload must have type string."
        );
    }

    #[test]
    fn grammar_input_delta_is_escaped_append_only_and_closed() {
        let mut buffer = GrammarToolInputJsonBuffer::default();
        let first = append_grammar_tool_input_json_delta(&mut buffer, "payload", "a\"", false)
            .unwrap()
            .unwrap();
        let second = append_grammar_tool_input_json_delta(&mut buffer, "payload", "a\"\nb", true)
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&format!("{first}{second}")).unwrap(),
            json!({"payload":"a\"\nb"})
        );
        assert_eq!(
            append_grammar_tool_input_json_delta(&mut buffer, "payload", "a\"\nb", true).unwrap(),
            None
        );
        assert_eq!(
            append_grammar_tool_input_json_delta(&mut buffer, "payload", "changed", true)
                .unwrap_err(),
            "grammar tool input for property \"payload\" changed after it was closed"
        );
    }
}
