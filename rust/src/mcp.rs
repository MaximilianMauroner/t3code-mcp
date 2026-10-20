mod arguments;
mod catalog;
mod http;
mod output;
mod peer;
mod stdio;
mod wire;

use std::{
    future::Future,
    sync::{Arc, Mutex},
};

use serde_json::{Map, Value, json};

pub use catalog::{CatalogError, ToolsListPage};
pub(crate) use http::authorized;
pub(crate) use http::body_too_large;
pub use http::{DEFAULT_HTTP_BODY_LIMIT, HttpBody, HttpOptions};
pub(crate) use output::{Delivery, Output};
pub use peer::{Invocation, Peer};
pub(crate) use stdio::FrameWriter;
pub use stdio::{DEFAULT_STDIO_BUFFER_LIMIT, StdioCodec, StdioError};
pub use wire::{IncomingMessage, MessageError, RequestId, RpcError, RpcRequest};

pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
    "2024-10-07",
];

const INSTRUCTIONS: &str = concat!(
    "This gateway controls one configured T3 Code environment for an orchestrator outside T3. T3 remains authoritative for projects, threads, messages, runs, and workspaces. ",
    "Native T3 tools (t3_project_*, t3_thread_*, t3_queue_*, schedules, pull requests, orchestrator_capabilities) are forwarded unchanged. As an outside client, pass projectId or threadId explicitly. ",
    "Use t3_threads_list and t3_threads_overview to see what needs attention across projects. By default they show the open queue: open threads that are not working, without subagent children. ",
    "To supervise working threads, pass status=all or onlyRunning=true to t3_threads_list, or read a known thread with t3_thread_read (view=activity for tool calls) and t3_thread_wait. Use t3_thread_send with mode=steer, queue, or restart to redirect running work. ",
    "Prefer t3_task_start for delegated work: it is retry-safe with an idempotencyKey, can prepare a worktree, and pairs with t3_task_get, t3_result_get, and t3_task_cancel. Monitor it with t3_thread_wait using the task's threadId. ",
    "Answer approvals with t3_pending_action_respond; native tools cannot. Interrupts are requests: confirm them with t3_thread_read or t3_thread_wait. ",
    "Usage is recorded in a local redacted audit trail; use t3_audit_log to inspect tool calls, upstream requests, Git commands, outcomes, and timings."
);

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub request_id: RequestId,
    pub name: String,
    pub arguments: Map<String, Value>,
    pub params: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolFailure {
    pub code: String,
    pub message: String,
    pub protocol_error: Option<Box<RpcError>>,
}

impl ToolFailure {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            protocol_error: None,
        }
    }
}

pub trait ToolHandler: Sync {
    fn native_client(&self) -> Option<&crate::t3::NativeMcpClient> {
        None
    }

    fn tool_catalog(&self) -> impl Future<Output = Option<ToolsListPage>> + Send {
        async { None }
    }

    fn call_tool(
        &self,
        call: ToolCall,
        invocation: Option<Invocation>,
    ) -> impl Future<Output = Result<Map<String, Value>, ToolFailure>> + Send;
}

#[derive(Clone)]
pub struct McpServer {
    catalog: ToolsListPage,
    peer: Peer,
    sessions: Arc<Mutex<peer::Sessions>>,
    workers: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    shutdown: tokio_util::sync::CancellationToken,
}

pub(crate) struct AdmittedMessage {
    message: IncomingMessage,
    peer: Peer,
    reserved: Option<Result<Invocation, RpcError>>,
}

impl AdmittedMessage {
    pub(crate) fn set_output(&self, output: Delivery) {
        if let Some(Ok(invocation)) = &self.reserved {
            invocation.set_output(output);
        }
    }
}

impl McpServer {
    pub fn new(catalog: ToolsListPage) -> Self {
        Self {
            catalog,
            peer: Peer::default(),
            sessions: Arc::new(Mutex::new(peer::Sessions::default())),
            workers: Arc::new(Mutex::new(Vec::new())),
            shutdown: tokio_util::sync::CancellationToken::new(),
        }
    }

    pub fn gateway() -> Self {
        Self::new(ToolsListPage::gateway())
    }

    pub(crate) fn expire_sessions(&self) {
        self.sessions.lock().unwrap().expire();
    }

    pub fn close(&self) {
        self.shutdown.cancel();
        self.peer.close();
        self.sessions.lock().unwrap().close();
    }

    pub(crate) fn retain_worker(&self, task: tokio::task::JoinHandle<()>) {
        let mut workers = self.workers.lock().unwrap();
        workers.retain(|worker| !worker.is_finished());
        workers.push(task);
    }

    pub async fn drain(&self) {
        loop {
            let workers = std::mem::take(&mut *self.workers.lock().unwrap());
            if workers.is_empty() {
                break;
            }
            for worker in workers {
                let _ = worker.await;
            }
        }
        self.peer.drain().await;
    }

    pub fn handle_message<'a>(
        &'a self,
        message: IncomingMessage,
        handler: &'a impl ToolHandler,
    ) -> impl Future<Output = Option<Value>> + Send + 'a {
        self.handle_peer_message(message, self.peer.clone(), handler)
    }

    fn handle_peer_message<'a>(
        &'a self,
        message: IncomingMessage,
        peer: Peer,
        handler: &'a impl ToolHandler,
    ) -> impl Future<Output = Option<Value>> + Send + 'a {
        let admitted = self.admit(message, peer);
        self.execute_message(admitted, handler)
    }

    pub(crate) fn stdio_output(&self) -> (Delivery, Output) {
        let (delivery, output) = Delivery::channel();
        self.peer.set_notification_output(delivery.clone());
        (delivery, output)
    }

    pub(crate) fn admit_message(&self, message: IncomingMessage) -> AdmittedMessage {
        self.admit(message, self.peer.clone())
    }

    fn admit(&self, message: IncomingMessage, peer: Peer) -> AdmittedMessage {
        let sessions = self.sessions.lock().unwrap();
        let reserved = match &message {
            IncomingMessage::Request(request) => Some(if sessions.is_closed() {
                Err(RpcError::new(-32000, "MCP server is closing."))
            } else {
                peer.reserve(request.id.clone())
            }),
            IncomingMessage::Notification {
                method,
                params: Some(params),
            } if method == "notifications/cancelled" => {
                peer.cancel(params.clone());
                None
            }
            _ => None,
        };
        AdmittedMessage {
            message,
            peer,
            reserved,
        }
    }

    pub(crate) fn execute_message<'a>(
        &'a self,
        admitted: AdmittedMessage,
        handler: &'a impl ToolHandler,
    ) -> impl Future<Output = Option<Value>> + Send + 'a {
        let AdmittedMessage {
            message,
            peer,
            reserved,
        } = admitted;
        async move {
            let IncomingMessage::Request(request) = message else {
                return None;
            };
            let invocation = match reserved.expect("request reservation") {
                Ok(invocation) => invocation,
                Err(error) => return Some(error.response(Some(&request.id))),
            };
            let result = async {
                if invocation.is_cancelled() {
                    return Err(RpcError::new(-32800, "Request cancelled."));
                }
                let catalog = if matches!(request.method.as_str(), "tools/list" | "tools/call") {
                    if let Some(source) = handler.native_client() {
                        let needs_native_catalog = request.method == "tools/list"
                            || request
                                .params
                                .as_ref()
                                .and_then(|params| params.get("name"))
                                .and_then(Value::as_str)
                                .is_some_and(crate::contract::is_native_tool_name);
                        if needs_native_catalog {
                            let snapshot = peer.native(source).snapshot().await;
                            let mut tools = self
                                .catalog
                                .tools()
                                .iter()
                                .filter(|tool| {
                                    !tool["name"]
                                        .as_str()
                                        .is_some_and(crate::contract::is_native_tool_name)
                                })
                                .cloned()
                                .collect::<Vec<_>>();
                            tools.extend(
                                snapshot
                                    .tools
                                    .into_iter()
                                    .map(|tool| serde_json::to_value(tool).expect("native tool")),
                            );
                            ToolsListPage::parse(json!({"tools":tools})).expect("validated catalog")
                        } else {
                            self.catalog.clone()
                        }
                    } else {
                        handler
                            .tool_catalog()
                            .await
                            .unwrap_or_else(|| self.catalog.clone())
                    }
                } else {
                    self.catalog.clone()
                };
                if invocation.is_cancelled() {
                    return Err(RpcError::new(-32800, "Request cancelled."));
                }
                let server = Self {
                    catalog,
                    peer: peer.clone(),
                    sessions: self.sessions.clone(),
                    workers: self.workers.clone(),
                    shutdown: self.shutdown.clone(),
                };
                server.dispatch(&request, handler, &invocation).await
            }
            .await;
            Some(match result {
                Ok(result) => json!({"result": result, "jsonrpc": "2.0", "id": request.id}),
                Err(error) => error.response(Some(&request.id)),
            })
        }
    }

    async fn dispatch(
        &self,
        request: &RpcRequest,
        handler: &impl ToolHandler,
        invocation: &Invocation,
    ) -> Result<Value, RpcError> {
        if matches!(
            request.method.as_str(),
            "initialize" | "ping" | "tools/list" | "tools/call"
        ) && request
            .params
            .as_ref()
            .and_then(|p| p.get("task"))
            .is_some_and(|task| {
                task.as_object().is_some_and(|task| {
                    task.get("ttl")
                        .is_none_or(|ttl| ttl.as_f64().is_some_and(f64::is_finite))
                })
            })
        {
            return Err(RpcError::new(
                -32602,
                format!(
                    "Server does not support task creation (required for {})",
                    request.method
                ),
            ));
        }
        match request.method.as_str() {
            "initialize" => initialize(request.params.as_ref()),
            "ping" => Ok(json!({})),
            "tools/list" => {
                if let Some(cursor) = request.params.as_ref().and_then(|p| p.get("cursor"))
                    && !cursor.is_string()
                {
                    return Err(wire::invalid_type(
                        "string",
                        Some(cursor),
                        &["params", "cursor"],
                    ));
                }
                Ok(self.catalog.as_value().clone())
            }
            "tools/call" => self.call_tool(request, handler, invocation).await,
            _ => Err(RpcError::new(-32601, "Method not found")),
        }
    }

    async fn call_tool(
        &self,
        request: &RpcRequest,
        handler: &impl ToolHandler,
        invocation: &Invocation,
    ) -> Result<Value, RpcError> {
        let params = request
            .params
            .as_ref()
            .ok_or_else(|| wire::invalid_type("object", None, &["params"]))?;
        let mut issues = Vec::new();
        if let Some(task) = params.get("task") {
            if let Some(task) = task.as_object() {
                if let Some(ttl) = task.get("ttl")
                    && !ttl.as_f64().is_some_and(f64::is_finite)
                {
                    issues.push(wire::type_issue(
                        "number",
                        Some(ttl),
                        &["params", "task", "ttl"],
                    ));
                }
            } else {
                issues.push(wire::type_issue("object", Some(task), &["params", "task"]));
            }
        }
        if !params.get("name").is_some_and(Value::is_string) {
            issues.push(wire::type_issue(
                "string",
                params.get("name"),
                &["params", "name"],
            ));
        }
        if let Some(args) = params.get("arguments")
            && !args.is_object()
        {
            issues.push(wire::type_issue(
                "record",
                Some(args),
                &["params", "arguments"],
            ));
        }
        if !issues.is_empty() {
            return Err(wire::invalid_params(issues));
        }
        let name = params["name"].as_str().expect("checked tool name");
        let Some(tool) = self
            .catalog
            .tools()
            .iter()
            .find(|tool| tool["name"] == name)
        else {
            return Ok(tool_error(format!(
                "MCP error -32602: Tool {name} not found"
            )));
        };
        let arguments = match arguments::parse(tool, params.get("arguments")) {
            Ok(args) => args,
            Err(message) => {
                return Ok(tool_error(format!(
                    "MCP error -32602: Input validation error: Invalid arguments for tool {name}: {message}"
                )));
            }
        };
        let mut params = params.clone();
        if crate::contract::is_native_tool_name(name) {
            params.insert("arguments".into(), Value::Object(arguments.clone()));
        }
        let call = ToolCall {
            request_id: request.id.clone(),
            name: name.to_owned(),
            arguments,
            params,
        };
        let result = match handler.call_tool(call, Some(invocation.clone())).await {
            Ok(result) => Value::Object(result),
            Err(failure) => {
                if let Some(error) = failure.protocol_error {
                    return Err(*error);
                }
                let error = json!({"error": {"code": failure.code, "message": failure.message}});
                json!({
                    "isError": true,
                    "content": [{"type": "text", "text": serde_json::to_string_pretty(&error).expect("JSON value")}],
                    "structuredContent": error
                })
            }
        };
        if let Value::Object(result) = &result
            && let Some(message) = arguments::output_error(tool, result)
        {
            return Ok(tool_error(message));
        }
        Ok(result)
    }
}

fn tool_error(message: String) -> Value {
    json!({"content": [{"type": "text", "text": message}], "isError": true})
}

fn initialize(params: Option<&Map<String, Value>>) -> Result<Value, RpcError> {
    let params = params.ok_or_else(|| wire::invalid_type("object", None, &["params"]))?;
    let mut issues = Vec::new();
    if !params.get("protocolVersion").is_some_and(Value::is_string) {
        issues.push(wire::type_issue(
            "string",
            params.get("protocolVersion"),
            &["params", "protocolVersion"],
        ));
    }
    for field in ["capabilities", "clientInfo"] {
        if !params.get(field).is_some_and(Value::is_object) {
            issues.push(wire::type_issue(
                "object",
                params.get(field),
                &["params", field],
            ));
        }
    }
    if let Some(client) = params.get("clientInfo").and_then(Value::as_object) {
        for field in ["name", "title", "version", "websiteUrl", "description"] {
            let required = matches!(field, "name" | "version");
            if (required || client.contains_key(field))
                && !client.get(field).is_some_and(Value::is_string)
            {
                issues.push(wire::type_issue(
                    "string",
                    client.get(field),
                    &["params", "clientInfo", field],
                ));
            }
        }
    }
    if !issues.is_empty() {
        return Err(wire::invalid_params(issues));
    }
    let version = params["protocolVersion"]
        .as_str()
        .expect("checked protocol version");
    let version = if SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
        version
    } else {
        SUPPORTED_PROTOCOL_VERSIONS[0]
    };
    Ok(json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": true}},
        "serverInfo": {"name": "t3-code-mcp", "version": crate::contract::GATEWAY_VERSION},
        "instructions": INSTRUCTIONS
    }))
}
