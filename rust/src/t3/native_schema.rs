use super::{NativeCallResult, NativeTool, WireError};
use serde_json::{Map, Value};

pub(super) fn parse_tools_page(
    value: Value,
) -> Result<(Vec<NativeTool>, Option<String>), WireError> {
    let object = value
        .as_object()
        .ok_or_else(|| WireError::message("Invalid native tools/list response."))?;
    validate_meta(object.get("_meta"))?;
    let tools = object
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| WireError::message("Invalid native tools/list response."))?
        .iter()
        .map(parse_native_tool)
        .collect::<Result<Vec<_>, _>>()?;
    let cursor = match object.get("nextCursor") {
        None => None,
        Some(Value::String(value)) => Some(value.clone()),
        Some(_) => return Err(WireError::message("Invalid native tools/list response.")),
    };
    Ok((tools, cursor))
}

fn parse_native_tool(value: &Value) -> Result<NativeTool, WireError> {
    let object = value
        .as_object()
        .ok_or_else(|| WireError::message("Invalid native tools/list response."))?;
    let name = required_string(object, "name")?;
    optional_string(object, "title")?;
    optional_string(object, "description")?;
    validate_tool_schema(object.get("inputSchema"))?;
    if let Some(schema) = object.get("outputSchema") {
        validate_tool_schema(Some(schema))?;
    }
    if let Some(annotations) = object.get("annotations") {
        validate_tool_annotations(annotations)?;
    }
    if let Some(execution) = object.get("execution") {
        validate_execution(execution)?;
    }
    if let Some(icons) = object.get("icons") {
        parse_icons(icons)?;
    }
    validate_meta(object.get("_meta"))?;
    let mut definition = object.clone();
    definition.remove("name");
    Ok(NativeTool { name, definition })
}

fn validate_tool_schema(value: Option<&Value>) -> Result<(), WireError> {
    let object = value
        .and_then(Value::as_object)
        .ok_or_else(|| WireError::message("Invalid native tools/list response."))?;
    if object.get("type") != Some(&Value::String("object".into())) {
        return Err(WireError::message("Invalid native tools/list response."));
    }
    Ok(())
}

fn validate_tool_annotations(value: &Value) -> Result<(), WireError> {
    let object = value
        .as_object()
        .ok_or_else(|| WireError::message("Invalid native tools/list response."))?;
    optional_string(object, "title")?;
    for name in [
        "readOnlyHint",
        "destructiveHint",
        "idempotentHint",
        "openWorldHint",
    ] {
        if let Some(value) = object.get(name)
            && !value.is_boolean()
        {
            return Err(WireError::message("Invalid native tools/list response."));
        }
    }
    Ok(())
}

fn validate_execution(value: &Value) -> Result<(), WireError> {
    let object = value
        .as_object()
        .ok_or_else(|| WireError::message("Invalid native tools/list response."))?;
    if let Some(support) = object.get("taskSupport")
        && !matches!(
            support.as_str(),
            Some("required" | "optional" | "forbidden")
        )
    {
        return Err(WireError::message("Invalid native tools/list response."));
    }
    Ok(())
}

fn parse_icons(value: &Value) -> Result<Value, WireError> {
    let icons = value
        .as_array()
        .ok_or_else(|| WireError::message("Invalid native tools/list response."))?;
    let mut result = Vec::with_capacity(icons.len());
    for icon in icons {
        let object = icon
            .as_object()
            .ok_or_else(|| WireError::message("Invalid native tools/list response."))?;
        let mut output = Map::new();
        output.insert("src".into(), Value::String(required_string(object, "src")?));
        if let Some(mime_type) = optional_string(object, "mimeType")? {
            output.insert("mimeType".into(), Value::String(mime_type));
        }
        if let Some(sizes) = object.get("sizes") {
            if !sizes
                .as_array()
                .is_some_and(|values| values.iter().all(Value::is_string))
            {
                return Err(WireError::message("Invalid native tools/list response."));
            }
            output.insert("sizes".into(), sizes.clone());
        }
        if let Some(theme) = object.get("theme") {
            if !matches!(theme.as_str(), Some("light" | "dark")) {
                return Err(WireError::message("Invalid native tools/list response."));
            }
            output.insert("theme".into(), theme.clone());
        }
        result.push(Value::Object(output));
    }
    Ok(Value::Array(result))
}

fn validate_meta(value: Option<&Value>) -> Result<(), WireError> {
    if value.is_some_and(|value| !value.is_object()) {
        return Err(WireError::message("Invalid native MCP response."));
    }
    Ok(())
}

fn required_string(object: &Map<String, Value>, name: &str) -> Result<String, WireError> {
    object
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| WireError::message("Invalid native MCP response."))
}

fn optional_string(object: &Map<String, Value>, name: &str) -> Result<Option<String>, WireError> {
    object
        .get(name)
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| WireError::message("Invalid native MCP response."))
        })
        .transpose()
}

pub(super) fn parse_native_call_result(value: Value) -> Result<NativeCallResult, WireError> {
    let mut object = value
        .as_object()
        .cloned()
        .ok_or_else(|| WireError::message("Invalid native tool result."))?;
    validate_meta(object.get("_meta"))?;
    let content = match object.remove("content") {
        Some(Value::Array(content)) => {
            for block in &content {
                validate_content_block(block)?;
            }
            content
        }
        _ => return Err(WireError::message("Invalid native tool result.")),
    };
    let is_error = match object.remove("isError") {
        None => None,
        Some(Value::Bool(value)) => Some(value),
        Some(_) => return Err(WireError::message("Invalid native tool result.")),
    };
    if object
        .get("structuredContent")
        .is_some_and(|value| !value.is_object())
    {
        return Err(WireError::message("Invalid native tool result."));
    }
    Ok(NativeCallResult {
        content,
        is_error,
        extra: object,
    })
}

fn validate_content_block(value: &Value) -> Result<(), WireError> {
    let object = value
        .as_object()
        .ok_or_else(|| WireError::message("Invalid native tool result."))?;
    let kind = required_string(object, "type")?;
    match kind.as_str() {
        "text" => {
            required_string(object, "text")?;
        }
        "image" | "audio" => {
            required_string(object, "data")?;
            required_string(object, "mimeType")?;
        }
        "resource_link" => {
            for name in ["name", "uri"] {
                required_string(object, name)?;
            }
            for name in ["title", "description", "mimeType"] {
                optional_string(object, name)?;
            }
            if let Some(size) = object.get("size")
                && !size.is_number()
            {
                return Err(WireError::message("Invalid native tool result."));
            }
            if let Some(icons) = object.get("icons") {
                parse_icons(icons)?;
            }
        }
        "resource" => {
            let resource = object
                .get("resource")
                .ok_or_else(|| WireError::message("Invalid native tool result."))?;
            validate_resource_contents(resource)?;
        }
        _ => return Err(WireError::message("Invalid native tool result.")),
    }
    if let Some(annotations) = object.get("annotations") {
        validate_annotations(annotations)?;
    }
    validate_meta(object.get("_meta"))?;
    Ok(())
}

fn validate_annotations(value: &Value) -> Result<(), WireError> {
    let object = value
        .as_object()
        .ok_or_else(|| WireError::message("Invalid native tool result."))?;
    if let Some(audience) = object.get("audience")
        && !audience.as_array().is_some_and(|values| {
            values
                .iter()
                .all(|value| matches!(value.as_str(), Some("user" | "assistant")))
        })
    {
        return Err(WireError::message("Invalid native tool result."));
    }
    if let Some(priority) = object.get("priority")
        && !priority
            .as_f64()
            .is_some_and(|value| (0.0..=1.0).contains(&value))
    {
        return Err(WireError::message("Invalid native tool result."));
    }
    if let Some(last_modified) = object.get("lastModified")
        && !last_modified
            .as_str()
            .is_some_and(|value| chrono::DateTime::parse_from_rfc3339(value).is_ok())
    {
        return Err(WireError::message("Invalid native tool result."));
    }
    Ok(())
}

fn validate_resource_contents(value: &Value) -> Result<(), WireError> {
    let object = value
        .as_object()
        .ok_or_else(|| WireError::message("Invalid native tool result."))?;
    required_string(object, "uri")?;
    optional_string(object, "mimeType")?;
    validate_meta(object.get("_meta"))?;
    if optional_string(object, "text")?.is_none() && optional_string(object, "blob")?.is_none() {
        return Err(WireError::message("Invalid native tool result."));
    }
    Ok(())
}
