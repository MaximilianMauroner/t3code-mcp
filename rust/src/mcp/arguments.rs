use serde_json::{Map, Value};

use super::ToolResult;
use crate::operations::audit_log::{AuditValue, JsString};

#[derive(Clone, Copy, PartialEq, Eq)]
enum SchemaUse {
    GatewayInput,
    NativeInput,
    GatewayOutput,
}

pub(super) fn parse(tool: &Value, value: Option<&Value>) -> Result<Map<String, Value>, String> {
    let gateway =
        crate::contract::GATEWAY_TOOL_NAMES.contains(&tool["name"].as_str().unwrap_or_default());
    let schema_use = if gateway {
        SchemaUse::GatewayInput
    } else {
        SchemaUse::NativeInput
    };
    let mut errors = Vec::new();
    let value = value.map(AuditValue::from);
    let fallback = (!gateway
        && !super::native_input_schema::converts_to_object(&tool["inputSchema"]))
    .then(|| serde_json::json!({"type":"object"}));
    let parsed = parse_value(
        fallback.as_ref().unwrap_or(&tool["inputSchema"]),
        value.as_ref(),
        "",
        schema_use,
        &mut errors,
    );
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    Ok(parsed
        .map(|value| serde_json::to_value(value).expect("UTF-8 tool arguments"))
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default())
}

pub(super) fn output_error(tool: &Value, result: &ToolResult) -> Option<String> {
    let name = tool["name"].as_str().unwrap_or_default();
    if !crate::contract::GATEWAY_TOOL_NAMES.contains(&name) {
        return None;
    }
    let result: AuditValue =
        serde_json::from_str(result.as_raw().get()).expect("serialized tool result");
    if result.get("content").is_none() || result.get("isError") == Some(&AuditValue::Bool(true)) {
        return None;
    }
    let schema = tool.get("outputSchema")?;
    let Some(structured) = result
        .get("structuredContent")
        .filter(|value| js_truthy(value))
    else {
        return Some(format!(
            "MCP error -32602: Output validation error: Tool {name} has an output schema but no structured content was provided"
        ));
    };
    let mut errors = Vec::new();
    parse_value(
        schema,
        Some(structured),
        "",
        SchemaUse::GatewayOutput,
        &mut errors,
    );
    (!errors.is_empty()).then(|| {
        format!(
            "MCP error -32602: Output validation error: Invalid structured content for tool {name}: {}",
            errors.join("\n")
        )
    })
}

fn js_truthy(value: &AuditValue) -> bool {
    match value {
        AuditValue::Null => false,
        AuditValue::Bool(value) => *value,
        AuditValue::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        AuditValue::String(value) => !value.is_empty(),
        AuditValue::Utf16String(value) => !value.is_empty(),
        AuditValue::Array(_) | AuditValue::Object(_) => true,
    }
}

fn parse_value(
    schema: &Value,
    input: Option<&AuditValue>,
    path: &str,
    schema_use: SchemaUse,
    errors: &mut Vec<String>,
) -> Option<AuditValue> {
    let default = schema.get("default").map(AuditValue::from);
    if input.is_none() && schema_use == SchemaUse::NativeInput && default.is_some() {
        return default;
    }
    let input = input.or(default.as_ref());
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
        if !input.is_some_and(|v| allowed.iter().any(|allowed| v == allowed)) {
            let choices = allowed
                .iter()
                .map(|v| serde_json::to_string(v).expect("JSON enum"))
                .collect::<Vec<_>>()
                .join("|");
            let message = if allowed.len() == 1 {
                format!("Invalid input: expected {choices}")
            } else {
                format!("Invalid option: expected one of {choices}")
            };
            issue(errors, path, message);
        }
        return input.cloned();
    }
    if let Some(variants) = schema
        .get("anyOf")
        .or_else(|| schema.get("oneOf"))
        .and_then(Value::as_array)
    {
        for variant in variants {
            let mut issues = Vec::new();
            let value = parse_value(variant, input, path, schema_use, &mut issues);
            if issues.is_empty() {
                return value;
            }
        }
        issue(errors, path, "Invalid input".to_owned());
        return input.cloned();
    }
    if let Some(expected) = schema.get("const")
        && input.is_none_or(|input| input != expected)
    {
        issue(
            errors,
            path,
            format!(
                "Invalid input: expected {}",
                serde_json::to_string(expected).expect("JSON schema const")
            ),
        );
        return input.cloned();
    }
    let expected_types = match schema.get("type") {
        Some(Value::String(expected)) => vec![expected.as_str()],
        Some(Value::Array(types)) => types.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let expected = expected_types
        .iter()
        .copied()
        .find(|expected| *expected != "null")
        .or_else(|| expected_types.first().copied());
    if input.is_none()
        && let Some(expected) = expected
    {
        issue(
            errors,
            path,
            format!("Invalid input: expected {expected}, received undefined"),
        );
        return None;
    }
    let matches = expected_types.is_empty()
        || expected_types.iter().any(|expected| {
            matches!(
                (*expected, input),
                ("object", Some(AuditValue::Object(_)))
                    | ("array", Some(AuditValue::Array(_)))
                    | (
                        "string",
                        Some(AuditValue::String(_) | AuditValue::Utf16String(_))
                    )
                    | ("number" | "integer", Some(AuditValue::Number(_)))
                    | ("boolean", Some(AuditValue::Bool(_)))
                    | ("null", Some(AuditValue::Null))
            )
        });
    if !matches {
        issue(
            errors,
            path,
            format!(
                "Invalid input: expected {}, received {}",
                if expected_types.contains(&"integer") {
                    "number"
                } else {
                    expected.unwrap_or("unknown")
                },
                value_type(input)
            ),
        );
        return input.cloned();
    }
    let mut value = input.cloned()?;
    match &mut value {
        AuditValue::Object(object) => {
            let properties = schema.get("properties").and_then(Value::as_object);
            let required = schema.get("required").and_then(Value::as_array);
            let additional = schema.get("additionalProperties");
            let strip = schema_use == SchemaUse::GatewayInput
                && (additional.is_none() || additional == Some(&Value::Bool(false)));
            let mut parsed = if strip {
                indexmap::IndexMap::new()
            } else {
                object.clone()
            };
            if schema_use == SchemaUse::NativeInput && additional == Some(&Value::Bool(false)) {
                for key in object.keys() {
                    if !properties.is_some_and(|properties| {
                        key.as_str().is_some_and(|key| properties.contains_key(key))
                    }) {
                        issue(
                            errors,
                            path,
                            format!(
                                "Unrecognized key: \"{}\"",
                                key.as_str().expect("UTF-8 input key")
                            ),
                        );
                    }
                }
            }
            if let Some(properties) = properties {
                for (key, field_schema) in properties {
                    let field = object.get(&JsString::from(key.as_str()));
                    let required =
                        required.is_some_and(|r| r.iter().any(|v| v.as_str() == Some(key)));
                    if field.is_none() && !required && field_schema.get("default").is_none() {
                        continue;
                    }
                    let field_path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    if let Some(v) =
                        parse_value(field_schema, field, &field_path, schema_use, errors)
                    {
                        parsed.insert(key.clone().into(), v);
                    }
                }
            }
            *object = parsed;
        }
        AuditValue::String(text) => {
            if schema_use == SchemaUse::GatewayInput
                && matches!(
                    path,
                    "since"
                        | "until"
                        | "event"
                        | "operation"
                        | "outcome"
                        | "projectId"
                        | "threadId"
                        | "providerInstanceId"
                        | "baseRevision"
                        | "headRevision"
                        | "query"
                        | "sessionStatus"
                        | "title"
                        | "branch"
                        | "worktreePath"
                        | "idempotencyKey"
                        | "taskRef"
                        | "reason"
                        | "name"
                        | "mimeType"
                        | "requestId"
                )
            {
                *text = text.trim_matches(js_whitespace).to_owned();
            }
            let length = text.chars().count() as f64;
            bounds(
                schema,
                length,
                "minLength",
                "maxLength",
                "string",
                path,
                errors,
            );
            if let Some(pattern) = schema.get("pattern").and_then(Value::as_str)
                && let Ok(regex) = regex::RegexBuilder::new(pattern).unicode(false).build()
                && !regex.is_match(text)
            {
                issue(
                    errors,
                    path,
                    format!("Invalid string: must match pattern /{pattern}/"),
                );
            }
        }
        AuditValue::Utf16String(units) => {
            bounds(
                schema,
                char::decode_utf16(units.iter().copied()).count() as f64,
                "minLength",
                "maxLength",
                "string",
                path,
                errors,
            );
            if let Some(pattern) = schema.get("pattern").and_then(Value::as_str)
                && regex::RegexBuilder::new(pattern)
                    .unicode(false)
                    .build()
                    .is_ok()
                && let Ok(regex) = regex::bytes::RegexBuilder::new(pattern)
                    .unicode(false)
                    .build()
                && !regex.is_match(&wtf8(units))
            {
                issue(
                    errors,
                    path,
                    format!("Invalid string: must match pattern /{pattern}/"),
                );
            }
        }
        AuditValue::Number(number) => {
            if let Some(n) = number.as_f64().filter(|value| value.is_finite()) {
                if expected_types.contains(&"integer")
                    && (n.fract() != 0.0 || n.abs() > 9_007_199_254_740_991.0)
                {
                    issue(
                        errors,
                        path,
                        "Invalid input: expected int, received number".to_owned(),
                    );
                } else {
                    bounds(schema, n, "minimum", "maximum", "number", path, errors);
                    if schema_use == SchemaUse::GatewayInput && expected_types.contains(&"integer")
                    {
                        *number = (n as i64).into();
                    }
                }
            } else if expected_types
                .iter()
                .any(|expected| matches!(*expected, "number" | "integer"))
            {
                issue(
                    errors,
                    path,
                    "Invalid input: expected finite number".to_owned(),
                );
            }
        }
        AuditValue::Array(items) => {
            bounds(
                schema,
                items.len() as f64,
                "minItems",
                "maxItems",
                "array",
                path,
                errors,
            );
            if let Some(item_schema) = schema.get("items") {
                for (i, item) in items.iter_mut().enumerate() {
                    if let Some(v) = parse_value(
                        item_schema,
                        Some(item),
                        &format!("{path}.{i}"),
                        schema_use,
                        errors,
                    ) {
                        *item = v;
                    }
                }
            }
        }
        _ => {}
    }
    Some(value)
}

fn bounds(
    schema: &Value,
    n: f64,
    min: &str,
    max: &str,
    kind: &str,
    path: &str,
    errors: &mut Vec<String>,
) {
    for (key, small, exclusive, comparison) in [
        (min, true, false, ">="),
        ("exclusiveMinimum", true, true, ">"),
        (max, false, false, "<="),
        ("exclusiveMaximum", false, true, "<"),
    ] {
        if let Some(bound) = schema.get(key).and_then(Value::as_f64)
            && ((small && (n < bound || (exclusive && n == bound)))
                || (!small && (n > bound || (exclusive && n == bound))))
        {
            let units = match kind {
                "string" => " characters",
                "array" => " items",
                _ => "",
            };
            let verb = if kind == "number" { "be" } else { "have" };
            issue(
                errors,
                path,
                format!(
                    "Too {}: expected {kind} to {verb} {comparison}{bound}{units}",
                    if small { "small" } else { "big" }
                ),
            );
        }
    }
}

fn issue(errors: &mut Vec<String>, path: &str, message: String) {
    errors.push(if path.is_empty() {
        message
    } else {
        format!("{message} at {path}")
    });
}

fn js_whitespace(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

fn value_type(value: Option<&AuditValue>) -> &'static str {
    match value {
        None => "undefined",
        Some(AuditValue::Null) => "null",
        Some(AuditValue::Bool(_)) => "boolean",
        Some(AuditValue::Number(_)) => "number",
        Some(AuditValue::String(_) | AuditValue::Utf16String(_)) => "string",
        Some(AuditValue::Array(_)) => "array",
        Some(AuditValue::Object(_)) => "object",
    }
}

fn wtf8(units: &[u16]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for character in char::decode_utf16(units.iter().copied()) {
        match character {
            Ok(character) => bytes.extend_from_slice(character.encode_utf8(&mut [0; 4]).as_bytes()),
            Err(error) => {
                let unit = error.unpaired_surrogate();
                bytes.extend_from_slice(&[
                    0xe0 | (unit >> 12) as u8,
                    0x80 | ((unit >> 6) & 0x3f) as u8,
                    0x80 | (unit & 0x3f) as u8,
                ]);
            }
        }
    }
    bytes
}
