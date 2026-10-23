use super::{commands::T3Command, iso, now, rpc_client::request_t3_rpc, types::*, v2};
use crate::operations::audit_log::{
    AuditEventInput, AuditLog, AuditSource, AuditValue, summarize_for_audit,
};
use futures_util::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Http,
    InvalidResponse,
    Rpc,
    Timeout,
    Cancelled,
    Transport,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientError {
    pub kind: ErrorKind,
    pub message: String,
    pub status: Option<u16>,
    pub method: Option<String>,
    pub path: Option<String>,
    pub code: Option<String>,
    pub reason: Option<String>,
    pub required_scope: Option<String>,
    pub trace_id: Option<String>,
}
impl ClientError {
    pub(crate) fn boxed(kind: ErrorKind, message: impl Into<String>) -> Box<Self> {
        Box::new(Self {
            kind,
            message: message.into(),
            status: None,
            method: None,
            path: None,
            code: None,
            reason: None,
            required_scope: None,
            trace_id: None,
        })
    }
}
impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ClientError {}
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionTelemetry {
    pub last_successful_at: Option<String>,
    pub last_snapshot_at: Option<String>,
    pub last_error: Option<String>,
}
type Discovery = Shared<BoxFuture<'static, Result<Descriptor, Box<ClientError>>>>;
#[derive(Default)]
struct State {
    descriptor: Option<Descriptor>,
    discovery: Option<Discovery>,
    telemetry: ConnectionTelemetry,
}
struct Inner {
    url: String,
    token: String,
    timeout: Duration,
    http: reqwest::Client,
    state: Mutex<State>,
    audit: Option<Arc<AuditLog>>,
}
#[derive(Clone)]
pub struct T3HttpClient(Arc<Inner>);
impl T3HttpClient {
    pub fn new(
        base_url: impl Into<String>,
        access_token: impl Into<String>,
        timeout: Duration,
        audit: Option<Arc<AuditLog>>,
    ) -> Self {
        Self(Arc::new(Inner {
            url: base_url.into(),
            token: access_token.into(),
            timeout,
            http: reqwest::Client::new(),
            state: Mutex::new(State::default()),
            audit,
        }))
    }
    pub fn telemetry(&self) -> ConnectionTelemetry {
        self.0.state.lock().unwrap().telemetry.clone()
    }
    pub fn cached_descriptor(&self) -> Option<Descriptor> {
        self.0.state.lock().unwrap().descriptor.clone()
    }
    pub async fn get_descriptor(
        &self,
        cancel: Option<&CancellationToken>,
    ) -> Result<Descriptor, Box<ClientError>> {
        let request = {
            let mut state = self.0.state.lock().unwrap();
            state
                .discovery
                .get_or_insert_with(|| {
                    let client = self.clone();
                    let task = tokio::spawn(async move {
                        let result = client
                            .request::<Descriptor>(
                                "GET",
                                "/.well-known/t3/environment",
                                None,
                                false,
                                None,
                                None,
                            )
                            .await;
                        let mut state = client.0.state.lock().unwrap();
                        if let Ok(d) = &result {
                            state.descriptor = Some(d.clone());
                        }
                        state.discovery = None;
                        result
                    });
                    async move {
                        task.await.unwrap_or_else(|e| {
                            Err(ClientError::boxed(ErrorKind::Transport, e.to_string()))
                        })
                    }
                    .boxed()
                    .shared()
                })
                .clone()
        };
        tokio::select! {biased;_=cancelled(cancel)=>{let error=ClientError::boxed(ErrorKind::Cancelled,"The operation was aborted.");self.failed(&error);Err(error)}, result=request=>result}
    }
    pub async fn get_usage_limits(
        &self,
        refresh_requested: bool,
        cancel: Option<&CancellationToken>,
    ) -> Result<super::models_usage::UsageLimitsRead, Box<ClientError>> {
        let refresh = if refresh_requested {
            match self
                .rpc::<Value>("server.refreshProviders", json!({}), cancel)
                .await
            {
                Ok(_) => json!({"status":"completed"}),
                Err(error) if error.kind == ErrorKind::Cancelled => return Err(error),
                Err(error) => {
                    json!({"status":"failed","error":super::failure::sanitize_failure_text(&error.message, 2000)})
                }
            }
        } else {
            json!({"status":"notRequested"})
        };
        let mut collector = super::models_usage::UsageLimitsCollector::default();
        let payload = json!({"usageLimitSources":true});
        let mut result = self
            .audited_rpc(
                "subscribeServerConfig",
                &payload,
                Box::pin(super::rpc_client::collect_t3_rpc(
                    &self.0.url,
                    &self.0.token,
                    self.0.timeout,
                    "subscribeServerConfig",
                    payload.clone(),
                    cancel,
                    |event| collector.collect(event),
                )),
            )
            .await?;
        result.refresh = refresh;
        Ok(result)
    }
    pub async fn get_usage_history(
        &self,
        input: Value,
        cancel: Option<&CancellationToken>,
    ) -> Result<Value, Box<ClientError>> {
        let summary = self.rpc("server.getUsageSummary", input, cancel).await?;
        super::usage_history::parse_summary(summary).map_err(|_| {
            ClientError::boxed(
                ErrorKind::InvalidResponse,
                "T3 RPC server.getUsageSummary returned an invalid response.",
            )
        })
    }

    pub async fn get_session(
        &self,
        cancel: Option<&CancellationToken>,
    ) -> Result<AuthSession, Box<ClientError>> {
        self.request("GET", "/api/auth/session", None, true, None, cancel)
            .await
    }
    async fn protocol(
        &self,
        refresh: bool,
        cancel: Option<&CancellationToken>,
    ) -> Result<u8, Box<ClientError>> {
        let descriptor = if refresh {
            self.get_descriptor(cancel).await?
        } else {
            match self.cached_descriptor() {
                Some(d) => d,
                None => self.get_descriptor(cancel).await?,
            }
        };
        let version = descriptor
            .orchestration_protocol_version
            .map(|v| v.get())
            .unwrap_or(1);
        match version {
            1 | 2 => Ok(version as u8),
            _ => Err(ClientError::boxed(
                ErrorKind::InvalidResponse,
                format!("Unsupported T3 orchestration protocol {version}."),
            )),
        }
    }
    pub async fn get_shell(
        &self,
        cancel: Option<&CancellationToken>,
    ) -> Result<ShellSnapshot, Box<ClientError>> {
        let result = self.read_protocol(None, cancel).await?;
        serde_json::from_value(result)
            .map_err(|e| ClientError::boxed(ErrorKind::InvalidResponse, e.to_string()))
    }
    pub async fn get_thread(
        &self,
        thread_id: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<ThreadSnapshot, Box<ClientError>> {
        let result = self.read_protocol(Some(thread_id), cancel).await?;
        serde_json::from_value(result)
            .map_err(|e| ClientError::boxed(ErrorKind::InvalidResponse, e.to_string()))
    }
    async fn read_protocol(
        &self,
        thread: Option<&str>,
        cancel: Option<&CancellationToken>,
    ) -> Result<Value, Box<ClientError>> {
        let version = self.protocol(false, cancel).await?;
        let result = self.read(version, thread, cancel).await;
        if let Err(error) = &result {
            if error.kind != ErrorKind::InvalidResponse && error.status != Some(400) {
                return result;
            }
            let refreshed = async {
                let cached = self.protocol(false, cancel).await?;
                if cached == version {
                    self.protocol(true, cancel).await
                } else {
                    Ok(cached)
                }
            }
            .await;
            match refreshed {
                Ok(new) if new != version => return self.read(new, thread, cancel).await,
                Err(error) if error.kind == ErrorKind::Cancelled => return Err(error),
                _ => {
                    self.failed(error);
                }
            }
        }
        result
    }
    async fn read(
        &self,
        version: u8,
        thread: Option<&str>,
        cancel: Option<&CancellationToken>,
    ) -> Result<Value, Box<ClientError>> {
        if let Some(thread) = thread {
            let encoded = url::form_urlencoded::byte_serialize(thread.as_bytes())
                .collect::<String>()
                .replace('+', "%20");
            let path = format!("/api/orchestration/threads/{encoded}");
            if version == 1 {
                return self
                    .request::<ThreadSnapshot>("GET", &path, None, true, Some(1), cancel)
                    .await
                    .map(|v| serde_json::to_value(v).unwrap());
            }
            let value = self
                .request::<Value>("GET", &path, None, true, Some(2), cancel)
                .await?;
            return v2::normalize_thread(value)
                .map(|v| serde_json::to_value(v).unwrap())
                .map_err(|_| self.invalid("GET", &path));
        }
        if version == 1 {
            let value = self
                .request::<ShellSnapshot>(
                    "GET",
                    "/api/orchestration/shell",
                    None,
                    true,
                    Some(1),
                    cancel,
                )
                .await?;
            self.0.state.lock().unwrap().telemetry.last_snapshot_at = Some(iso(now()));
            return Ok(serde_json::to_value(value).unwrap());
        }
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Shell {
            snapshot_sequence: u64,
            projects: Vec<Project>,
            threads: Vec<Value>,
            archived_threads: Vec<Value>,
        }
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Archived {
            snapshot_sequence: u64,
            threads: Vec<Value>,
        }
        let shell: Shell = self
            .request(
                "GET",
                "/api/orchestration/shell",
                None,
                true,
                Some(2),
                cancel,
            )
            .await?;
        let archived: Archived = self
            .rpc("orchestration.getArchivedShellSnapshot", json!({}), cancel)
            .await?;
        let mut threads = indexmap::IndexMap::new();
        for (values, seq) in [
            (shell.threads, shell.snapshot_sequence),
            (shell.archived_threads, shell.snapshot_sequence),
            (archived.threads, archived.snapshot_sequence),
        ] {
            for value in values {
                let t = v2::normalize_shell_thread(value, Some(seq))
                    .map_err(|_| self.invalid("GET", "/api/orchestration/shell"))?;
                threads.insert(t.id.clone(), t);
            }
        }
        let at = iso(now());
        self.0.state.lock().unwrap().telemetry.last_snapshot_at = Some(at.clone());
        Ok(serde_json::to_value(ShellSnapshot {
            snapshot_sequence: shell.snapshot_sequence.max(archived.snapshot_sequence),
            projects: shell.projects,
            threads: threads.into_values().collect(),
            updated_at: at,
            extra: Default::default(),
        })
        .unwrap())
    }
    pub async fn dispatch(
        &self,
        command: &T3Command,
        cancel: Option<&CancellationToken>,
    ) -> Result<DispatchResult, Box<ClientError>> {
        let version = match self.protocol(true, cancel).await {
            Ok(v) => v,
            Err(e) if e.kind == ErrorKind::Cancelled => return Err(e),
            Err(e) => {
                let mut error = ClientError::boxed(
                    ErrorKind::Http,
                    format!(
                        "T3 command not sent: {e} Retry after discovery recovers with a new idempotency key."
                    ),
                );
                error.status = Some(400);
                error.method = Some("GET".into());
                error.path = Some("/.well-known/t3/environment".into());
                error.code = Some("protocol_discovery_failed".into());
                error.reason = Some("command_not_sent".into());
                self.failed(&error);
                return Err(error);
            }
        };
        if version == 1 {
            return self
                .request(
                    "POST",
                    "/api/orchestration/dispatch",
                    Some(serde_json::to_value(command).unwrap()),
                    true,
                    Some(1),
                    cancel,
                )
                .await;
        }
        let mut result = None;
        for command in command.v2_commands() {
            result = Some(
                self.rpc("orchestration.dispatchCommand", command, cancel)
                    .await?,
            );
        }
        Ok(result.expect("every command has a V2 dispatch"))
    }
    async fn rpc<T: DeserializeOwned>(
        &self,
        method: &str,
        payload: Value,
        cancel: Option<&CancellationToken>,
    ) -> Result<T, Box<ClientError>> {
        self.audited_rpc(
            method,
            &payload,
            Box::pin(request_t3_rpc(
                &self.0.url,
                &self.0.token,
                self.0.timeout,
                method,
                payload.clone(),
                cancel,
            )),
        )
        .await
    }
    async fn audited_rpc<T>(
        &self,
        method: &str,
        payload: &Value,
        request: std::pin::Pin<Box<impl std::future::Future<Output = Result<T, Box<ClientError>>>>>,
    ) -> Result<T, Box<ClientError>> {
        let id = format!("t3_{}", uuid::Uuid::new_v4());
        let started = now();
        self.audit(
            &id,
            method,
            "started",
            None,
            Some(AuditValue::object([
                ("body", summarize_for_audit(payload)),
                ("transport", "websocket".into()),
            ])),
        );
        let result = request.await;
        match &result {
            Ok(_) => {
                self.succeeded();
                self.audit(&id, method, "completed", Some(now() - started), None);
            }
            Err(e) => {
                self.failed(e);
                self.audit(
                    &id,
                    method,
                    "error",
                    Some(now() - started),
                    Some(json!({"error":e.message}).into()),
                );
            }
        }
        result
    }
    async fn request<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        authenticated: bool,
        version: Option<u8>,
        cancel: Option<&CancellationToken>,
    ) -> Result<T, Box<ClientError>> {
        let id = format!("t3_{}", uuid::Uuid::new_v4());
        let started = now();
        let operation = format!("{method} {path}");
        self.audit(
            &id,
            &operation,
            "started",
            None,
            Some(AuditValue::object([
                ("method", method.into()),
                ("path", path.into()),
                ("authenticated", AuditValue::Bool(authenticated)),
                (
                    "body",
                    body.as_ref()
                        .map(summarize_for_audit)
                        .unwrap_or(AuditValue::Null),
                ),
            ])),
        );
        let execute = async {
            let mut request = self
                .0
                .http
                .request(
                    if method == "GET" {
                        reqwest::Method::GET
                    } else {
                        reqwest::Method::POST
                    },
                    format!(
                        "{}/{}",
                        self.0.url.trim_end_matches('/'),
                        path.trim_start_matches('/')
                    ),
                )
                .header("accept", "application/json");
            if authenticated {
                request = request.bearer_auth(&self.0.token);
            }
            if version == Some(2) {
                request = request.header("x-t3-orchestration-protocol", "2");
            }
            if let Some(body) = body {
                request = request.json(&body);
            }
            let response = request
                .send()
                .await
                .map_err(|e| ClientError::boxed(ErrorKind::Transport, e.to_string()))?;
            let status = response.status().as_u16();
            let body = response.json::<Value>().await.unwrap_or(Value::Null);
            if !(200..300).contains(&status) {
                return Err(http_error(status, method, path, &body));
            }
            serde_json::from_value(body)
                .map(|v| (v, status))
                .map_err(|_| self.invalid(method, path))
        };
        let result = tokio::select! {biased;_=cancelled(cancel)=>Err(ClientError::boxed(ErrorKind::Cancelled,"The operation was aborted.")),r=tokio::time::timeout(self.0.timeout,execute)=>r.unwrap_or_else(|_|Err(ClientError::boxed(ErrorKind::Timeout,"The operation was aborted due to timeout")))};
        match result {
            Ok((value, status)) => {
                self.succeeded();
                self.audit(
                    &id,
                    &operation,
                    "completed",
                    Some(now() - started),
                    Some(json!({"status":status}).into()),
                );
                Ok(value)
            }
            Err(e) => {
                self.failed(&e);
                self.audit(
                    &id,
                    &operation,
                    "error",
                    Some(now() - started),
                    Some(json!({"status":e.status,"code":e.code,"error":e.message}).into()),
                );
                Err(e)
            }
        }
    }
    fn invalid(&self, method: &str, path: &str) -> Box<ClientError> {
        let e = ClientError::boxed(
            ErrorKind::InvalidResponse,
            format!("T3 {method} {path} returned an invalid response."),
        );
        self.failed(&e);
        e
    }
    fn succeeded(&self) {
        let mut state = self.0.state.lock().unwrap();
        state.telemetry.last_successful_at = Some(iso(now()));
        state.telemetry.last_error = None;
    }
    fn failed(&self, error: &ClientError) {
        self.0.state.lock().unwrap().telemetry.last_error = Some(error.message.clone());
    }
    fn audit(
        &self,
        id: &str,
        operation: &str,
        outcome: &str,
        duration: Option<i64>,
        details: Option<AuditValue>,
    ) {
        record(
            self.0.audit.as_deref(),
            id,
            operation,
            outcome,
            duration,
            details,
        );
    }
}
pub(crate) fn record(
    audit: Option<&AuditLog>,
    id: &str,
    operation: &str,
    outcome: &str,
    duration: Option<i64>,
    details: Option<AuditValue>,
) {
    if let Some(audit) = audit {
        let mut input = AuditEventInput::new(
            AuditSource::T3,
            if outcome == "started" {
                "upstream.request"
            } else {
                "upstream.response"
            },
        );
        input.correlation_id = Some(id.into());
        input.operation = Some(operation.into());
        input.outcome = Some(outcome.into());
        input.duration_ms = duration.map(|v| v as f64);
        input.details = details;
        audit.record(input);
    }
}
async fn cancelled(cancel: Option<&CancellationToken>) {
    match cancel {
        Some(c) => c.cancelled().await,
        None => std::future::pending().await,
    }
}
fn http_error(status: u16, method: &str, path: &str, body: &Value) -> Box<ClientError> {
    let field = |key: &str| {
        body[key]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let code = body["code"].as_str().map(str::to_owned);
    let reason = field("reason");
    let scope = field("requiredScope");
    let detail = reason
        .clone()
        .or_else(|| scope.as_ref().map(|s| format!("requires {s}")))
        .or_else(|| code.clone())
        .filter(|v| !v.is_empty());
    let message = field("message").unwrap_or_else(|| {
        format!(
            "T3 {method} {path} failed with HTTP {status}{}.",
            detail.map(|v| format!(" ({v})")).unwrap_or_default()
        )
    });
    Box::new(ClientError {
        kind: ErrorKind::Http,
        message,
        status: Some(status),
        method: Some(method.into()),
        path: Some(path.into()),
        code,
        reason,
        required_scope: scope,
        trace_id: field("traceId"),
    })
}
