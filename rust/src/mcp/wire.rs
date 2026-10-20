use std::fmt;

use serde::Serialize;
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(untagged)]
pub enum RequestId {
    String(String),
    Number(i64),
}

impl RequestId {
    pub(super) fn parse(value: &Value) -> Option<Self> {
        match value {
            Value::String(id) => Some(Self::String(id.clone())),
            Value::Number(_) => integer(value).map(Self::Number),
            _ => None,
        }
    }
}

fn integer(value: &Value) -> Option<i64> {
    let n = value.as_f64()?;
    (n.is_finite() && n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0).then_some(n as i64)
}

#[derive(Debug, Clone, PartialEq)]
pub struct RpcRequest {
    pub id: RequestId,
    pub method: String,
    pub params: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IncomingMessage {
    Request(RpcRequest),
    Notification {
        method: String,
        params: Option<Map<String, Value>>,
    },
    Response(Value),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageError;

impl fmt::Display for MessageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Invalid JSON-RPC message")
    }
}

impl std::error::Error for MessageError {}

impl IncomingMessage {
    pub fn parse(value: Value) -> Result<Self, MessageError> {
        let object = value.as_object().ok_or(MessageError)?;
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(MessageError);
        }
        let id = match object.get("id") {
            Some(id) => Some(RequestId::parse(id).ok_or(MessageError)?),
            None => None,
        };
        if let Some(method) = object.get("method") {
            let method = method.as_str().ok_or(MessageError)?.to_owned();
            if object
                .keys()
                .any(|key| !["jsonrpc", "id", "method", "params"].contains(&key.as_str()))
            {
                return Err(MessageError);
            }
            let params = match object.get("params") {
                None => None,
                Some(Value::Object(params)) => Some(params.clone()),
                _ => return Err(MessageError),
            };
            if let Some(params) = params.as_ref() {
                normalize_meta(params)?;
            }
            return Ok(match id {
                Some(id) => Self::Request(RpcRequest { id, method, params }),
                None => Self::Notification { method, params },
            });
        }
        if let Some(Value::Object(result)) = object.get("result") {
            if id.is_none()
                || object
                    .keys()
                    .any(|k| !["jsonrpc", "id", "result"].contains(&k.as_str()))
            {
                return Err(MessageError);
            }
            normalize_meta(result)?;
            return Ok(Self::Response(
                json!({"jsonrpc":"2.0", "id":id, "result":result}),
            ));
        }
        if let Some(Value::Object(error)) = object.get("error") {
            if object
                .keys()
                .any(|k| !["jsonrpc", "id", "error"].contains(&k.as_str()))
                || !error.get("code").is_some_and(|v| integer(v).is_some())
                || !error.get("message").is_some_and(Value::is_string)
            {
                return Err(MessageError);
            }
            let mut error = error.clone();
            error.retain(|k, _| ["code", "message", "data"].contains(&k.as_str()));
            let mut response = json!({"jsonrpc":"2.0", "error":error});
            if let Some(id) = id {
                response["id"] = serde_json::to_value(id).expect("request ID");
            }
            return Ok(Self::Response(response));
        }
        Err(MessageError)
    }

    pub fn is_initialize(&self) -> bool {
        matches!(self, Self::Request(request) if request.method == "initialize")
    }
}

pub(super) fn normalize_meta(params: &Map<String, Value>) -> Result<(), MessageError> {
    let Some(meta) = params.get("_meta") else {
        return Ok(());
    };
    let meta = meta.as_object().ok_or(MessageError)?;
    if let Some(token) = meta.get("progressToken")
        && !token.is_string()
        && !token.as_f64().is_some_and(f64::is_finite)
    {
        return Err(MessageError);
    }
    if let Some(task) = meta.get("io.modelcontextprotocol/related-task") {
        let object = task.as_object().ok_or(MessageError)?;
        object
            .get("taskId")
            .and_then(Value::as_str)
            .ok_or(MessageError)?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn response(&self, id: Option<&RequestId>) -> RpcResponse {
        RpcResponse::Failure {
            jsonrpc: "2.0",
            error: self.clone(),
            id: id.cloned(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum RpcResponse {
    Success {
        result: Box<serde_json::value::RawValue>,
        jsonrpc: &'static str,
        id: RequestId,
    },
    Failure {
        jsonrpc: &'static str,
        error: RpcError,
        id: Option<RequestId>,
    },
}

impl RpcResponse {
    pub(super) fn success(id: RequestId, result: Box<serde_json::value::RawValue>) -> Self {
        Self::Success {
            result,
            jsonrpc: "2.0",
            id,
        }
    }
}

pub(super) fn value_type(value: Option<&Value>) -> &'static str {
    match value {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

pub(super) fn invalid_type(expected: &str, value: Option<&Value>, path: &[&str]) -> RpcError {
    invalid_params(vec![type_issue(expected, value, path)])
}

pub(super) fn type_issue(expected: &str, value: Option<&Value>, path: &[&str]) -> Value {
    json!({
        "expected": expected,
        "code": "invalid_type",
        "path": path,
        "message": format!("Invalid input: expected {expected}, received {}", value_type(value))
    })
}

pub(super) fn invalid_params(issues: Vec<Value>) -> RpcError {
    RpcError::new(
        -32602,
        serde_json::to_string_pretty(&issues).expect("JSON issues"),
    )
}
