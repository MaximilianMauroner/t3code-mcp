use std::fmt;

use serde_json::{Map, Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogError(pub String);

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CatalogError {}

#[derive(Debug, Clone)]
pub struct ToolsListPage {
    value: Value,
}

impl ToolsListPage {
    pub fn parse(mut value: Value) -> Result<Self, CatalogError> {
        let page = value
            .as_object_mut()
            .ok_or_else(|| invalid("tools/list result"))?;
        if page
            .get("nextCursor")
            .is_some_and(|cursor| !cursor.is_string())
        {
            return Err(invalid("nextCursor"));
        }
        super::wire::normalize_meta(page).map_err(|_| invalid("_meta"))?;
        let tools = page
            .get_mut("tools")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| invalid("tools"))?;
        for tool in tools {
            normalize_tool(tool)?;
        }
        Ok(Self { value })
    }

    pub fn gateway() -> Self {
        let mut tools =
            serde_json::to_value(crate::contract::gateway_tools()).expect("gateway tools");
        for tool in tools.as_array_mut().expect("tool array") {
            let tool = tool.as_object_mut().expect("tool object");
            let output = tool
                .shift_remove("outputSchema")
                .expect("gateway output schema");
            tool.insert("outputSchema".to_owned(), output);
        }
        Self::parse(json!({"tools": tools})).expect("embedded gateway tools")
    }

    pub fn tools(&self) -> &[Value] {
        self.value["tools"].as_array().expect("validated tools")
    }

    pub fn next_cursor(&self) -> Option<&str> {
        self.value.get("nextCursor").and_then(Value::as_str)
    }

    pub fn as_value(&self) -> &Value {
        &self.value
    }

    pub fn into_value(self) -> Value {
        self.value
    }
}

fn invalid(field: &str) -> CatalogError {
    CatalogError(format!("Invalid tools/list field: {field}"))
}

fn normalize_tool(tool: &mut Value) -> Result<(), CatalogError> {
    let tool = tool.as_object_mut().ok_or_else(|| invalid("tool"))?;
    if !tool.get("name").is_some_and(Value::is_string) {
        return Err(invalid("name"));
    }
    let native = crate::contract::is_native_tool_name(tool["name"].as_str().expect("tool name"));
    for key in ["title", "description"] {
        if tool.get(key).is_some_and(|v| !v.is_string()) {
            return Err(invalid(key));
        }
    }
    schema(
        tool.get("inputSchema")
            .ok_or_else(|| invalid("inputSchema"))?,
        "inputSchema",
        native,
    )?;
    if let Some(output) = tool.get("outputSchema") {
        schema(output, "outputSchema", native)?;
    }
    if let Some(meta) = tool.get("_meta")
        && !meta.is_object()
    {
        return Err(invalid("_meta"));
    }
    if let Some(annotations) = tool.get_mut("annotations") {
        let annotations = annotations
            .as_object_mut()
            .ok_or_else(|| invalid("annotations"))?;
        if annotations.get("title").is_some_and(|v| !v.is_string()) {
            return Err(invalid("annotations.title"));
        }
        for key in [
            "readOnlyHint",
            "destructiveHint",
            "idempotentHint",
            "openWorldHint",
        ] {
            if annotations.get(key).is_some_and(|v| !v.is_boolean()) {
                return Err(invalid(key));
            }
        }
        if !native {
            retain(
                annotations,
                &[
                    "title",
                    "readOnlyHint",
                    "destructiveHint",
                    "idempotentHint",
                    "openWorldHint",
                ],
            );
        }
    }
    if let Some(execution) = tool.get_mut("execution") {
        let execution = execution
            .as_object_mut()
            .ok_or_else(|| invalid("execution"))?;
        if execution
            .get("taskSupport")
            .is_some_and(|v| !matches!(v.as_str(), Some("required" | "optional" | "forbidden")))
        {
            return Err(invalid("execution.taskSupport"));
        }
        if !native {
            retain(execution, &["taskSupport"]);
        }
    }
    if let Some(icons) = tool.get_mut("icons") {
        let icons = icons.as_array_mut().ok_or_else(|| invalid("icons"))?;
        for icon in icons {
            let icon = icon.as_object_mut().ok_or_else(|| invalid("icon"))?;
            if !icon.get("src").is_some_and(Value::is_string)
                || icon.get("mimeType").is_some_and(|v| !v.is_string())
                || icon
                    .get("sizes")
                    .is_some_and(|v| !v.as_array().is_some_and(|s| s.iter().all(Value::is_string)))
                || icon
                    .get("theme")
                    .is_some_and(|v| !matches!(v.as_str(), Some("light" | "dark")))
            {
                return Err(invalid("icon"));
            }
            if !native {
                retain(icon, &["src", "mimeType", "sizes", "theme"]);
            }
        }
    }
    if !native {
        retain(
            tool,
            &[
                "name",
                "title",
                "description",
                "inputSchema",
                "outputSchema",
                "annotations",
                "execution",
                "icons",
                "_meta",
            ],
        );
    }
    Ok(())
}

fn schema(value: &Value, field: &str, native: bool) -> Result<(), CatalogError> {
    let Some(object) = value.as_object() else {
        return Err(invalid(field));
    };
    if object.get("type").and_then(Value::as_str) != Some("object") {
        return Err(invalid(field));
    }
    if native {
        return Ok(());
    }
    if let Some(properties) = object.get("properties") {
        let Some(properties) = properties.as_object() else {
            return Err(invalid(field));
        };
        if properties.values().any(|v| !v.is_object() && !v.is_array()) {
            return Err(invalid(field));
        }
    }
    if object
        .get("required")
        .is_some_and(|v| !v.as_array().is_some_and(|a| a.iter().all(Value::is_string)))
    {
        return Err(invalid(field));
    }
    Ok(())
}

fn retain(object: &mut Map<String, Value>, fields: &[&str]) {
    object.retain(|key, _| fields.contains(&key.as_str()));
}
