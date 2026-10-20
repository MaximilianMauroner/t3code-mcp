#[path = "native_schema.rs"]
mod schema;
#[path = "native_wire.rs"]
mod wire;
use super::{http_client::record, iso, now};
use crate::{
    contract,
    native_credential::{
        NativeCredential, NativeCredentialState, NativeCredentialStore, token_identity,
    },
    operations::audit_log::{
        AuditLog, AuditValue, current_audit_context, summarize_for_audit, with_audit_context,
    },
};
use futures_util::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use schema::{parse_native_call_result, parse_tools_page};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};
use wire::{Connection, WireError};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NativeError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol_error: Option<Box<crate::mcp::RpcError>>,
}
impl NativeError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            protocol_error: None,
        }
    }
}
impl std::fmt::Display for NativeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for NativeError {}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeTool {
    pub name: String,
    #[serde(flatten)]
    pub definition: Map<String, Value>,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeSnapshot {
    pub tools: Vec<NativeTool>,
    pub missing: Vec<String>,
    pub fetched_at: Option<String>,
    pub error: Option<NativeError>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeCallResult {
    pub content: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
type NotificationObserver = Arc<dyn Fn(Value) + Send + Sync>;

type Connecting = Shared<BoxFuture<'static, Result<Arc<Connection>, WireError>>>;
type Refreshing = Shared<BoxFuture<'static, NativeSnapshot>>;
struct Catalog {
    snapshot: NativeSnapshot,
    token_id: String,
    expires_at: Instant,
    generation: u64,
    revision: u64,
}
#[derive(Default)]
struct State {
    connection: Option<Arc<Connection>>,
    connections: Vec<Weak<Connection>>,
    connecting: Option<Connecting>,
    catalog: Option<Catalog>,
    refreshing: Option<Refreshing>,
    generation: u64,
    dirty: u64,
    refresh_error: Option<(String, NativeError, Instant)>,
    closed: bool,
}
struct Inner {
    url: String,
    credentials: Arc<Mutex<CredentialSource>>,
    audit: Option<Arc<AuditLog>>,
    timeout: Duration,
    state: Mutex<State>,
    http: reqwest::Client,
    upload: reqwest::Client,
    observer: Mutex<Option<NotificationObserver>>,
}

enum CredentialSource {
    Store(NativeCredentialStore),
    Memory(NativeCredential),
}
#[derive(Clone)]
pub struct NativeMcpClient(Arc<Inner>);
impl NativeMcpClient {
    pub fn new(
        url: impl Into<String>,
        credentials: NativeCredentialStore,
        audit: Option<Arc<AuditLog>>,
        request_timeout: Duration,
    ) -> Self {
        Self::with_source(
            url,
            CredentialSource::Store(credentials),
            audit,
            request_timeout,
        )
    }

    pub fn with_credential(
        url: impl Into<String>,
        credential: NativeCredential,
        audit: Option<Arc<AuditLog>>,
        request_timeout: Duration,
    ) -> Self {
        Self::with_source(
            url,
            CredentialSource::Memory(credential),
            audit,
            request_timeout,
        )
    }

    fn with_source(
        url: impl Into<String>,
        credentials: CredentialSource,
        audit: Option<Arc<AuditLog>>,
        request_timeout: Duration,
    ) -> Self {
        Self(Arc::new(Inner {
            url: url.into(),
            credentials: Arc::new(Mutex::new(credentials)),
            audit,
            timeout: request_timeout,
            state: Mutex::new(State::default()),
            observer: Mutex::new(None),
            http: reqwest::Client::builder()
                .retry(reqwest::retry::never())
                .build()
                .expect("native HTTP client configuration"),
            upload: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("HTTP client configuration"),
        }))
    }
    pub(crate) fn fork(&self) -> Self {
        Self(Arc::new(Inner {
            url: self.0.url.clone(),
            credentials: self.0.credentials.clone(),
            audit: self.0.audit.clone(),
            timeout: self.0.timeout,
            state: Mutex::new(State::default()),
            observer: Mutex::new(None),
            http: self.0.http.clone(),
            upload: self.0.upload.clone(),
        }))
    }
    pub fn credential_state(&self) -> NativeCredentialState {
        match &mut *self.0.credentials.lock().unwrap() {
            CredentialSource::Store(store) => store.read(),
            CredentialSource::Memory(credential) => NativeCredentialState::Valid {
                credential: credential.clone(),
            },
        }
    }
    fn require_credential(&self) -> Result<NativeCredential, NativeError> {
        match self.credential_state() {
            NativeCredentialState::Valid { credential } => Ok(credential),
            state => Err(credential_error(state)),
        }
    }
    pub(crate) fn observe_notifications(&self, observer: NotificationObserver) {
        *self.0.observer.lock().unwrap() = Some(observer);
    }

    pub async fn snapshot(&self) -> NativeSnapshot {
        loop {
            let credential = match self.require_credential() {
                Ok(c) => c,
                Err(e) => return self.failed_snapshot(e),
            };
            let token_id = token_identity(&credential.access_token);
            let (known, refresh, explicit) = {
                let mut state = self.0.state.lock().unwrap();
                let explicit = state.catalog.as_ref().is_none_or(|catalog| {
                    catalog.token_id != token_id
                        || catalog.generation != state.generation
                        || catalog.revision != state.dirty
                });
                if let Some(catalog) = &state.catalog
                    && !explicit
                    && catalog.expires_at > Instant::now()
                {
                    return catalog.snapshot.clone();
                }
                if let Some((failed_token, error, retry_at)) = &state.refresh_error
                    && failed_token == &token_id
                    && *retry_at > Instant::now()
                {
                    let error = error.clone();
                    drop(state);
                    return self.failed_snapshot(error);
                }
                let known = state
                    .catalog
                    .as_ref()
                    .map(|catalog| catalog.snapshot.clone());
                let refresh = state
                    .refreshing
                    .get_or_insert_with(|| {
                        let client = self.clone();
                        let task = tokio::spawn(async move {
                            let (snapshot, generation, revision) =
                                client.read_catalog(credential).await;
                            let mut state = client.0.state.lock().unwrap();
                            if generation == state.generation {
                                if let Some(error) = &snapshot.error {
                                    if revision == state.dirty {
                                        state.refresh_error = Some((
                                            token_id.clone(),
                                            error.clone(),
                                            Instant::now() + Duration::from_secs(10),
                                        ));
                                    }
                                } else {
                                    state.catalog = Some(Catalog {
                                        expires_at: Instant::now() + Duration::from_secs(60),
                                        snapshot: snapshot.clone(),
                                        token_id,
                                        generation,
                                        revision,
                                    });
                                    state.refresh_error = None;
                                }
                            }
                            state.refreshing = None;
                            snapshot
                        });
                        async move { task.await.expect("catalog task must not panic") }
                            .boxed()
                            .shared()
                    })
                    .clone();
                (known, refresh, explicit)
            };
            if !explicit && let Some(snapshot) = known {
                return snapshot;
            }
            refresh.await;
            // Success and failure both need the current credential, generation, and dirty revision.
            // A current failure returns through its credential-keyed backoff above.
        }
    }
    async fn read_catalog(&self, credential: NativeCredential) -> (NativeSnapshot, u64, u64) {
        let mut connection = None;
        let (mut generation, mut revision) = {
            let state = self.0.state.lock().unwrap();
            (state.generation, state.dirty)
        };
        let result = async {
            let connected = self.connect(&credential).await?;
            connection = Some(connected.clone());
            {
                let state = self.0.state.lock().unwrap();
                generation = connected.generation;
                revision = state.dirty;
            }
            let mut listed = Vec::new();
            let mut offered = std::collections::HashSet::new();
            let mut cursor = None;
            loop {
                let params = cursor
                    .as_ref()
                    .map(|c| json!({"cursor":c}))
                    .unwrap_or(json!({}));
                let page = connected
                    .request("tools/list", params, Duration::from_secs(10), None)
                    .await?;
                let (tools, next_cursor) = parse_tools_page(page)?;
                offered.extend(tools.iter().map(|tool| tool.name.clone()));
                listed.extend(tools);
                cursor = next_cursor;
                if cursor.is_none() {
                    break;
                }
            }
            let tools = listed
                .iter()
                .filter(|tool| contract::is_native_tool_name(&tool.name))
                .cloned()
                .collect();
            Ok(NativeSnapshot {
                tools,
                missing: contract::native_called_tool_names()
                    .filter(|n| !offered.contains(*n))
                    .map(str::to_owned)
                    .collect(),
                fetched_at: Some(iso(now())),
                error: None,
            })
        }
        .await;
        let snapshot = match result {
            Ok(v) => v,
            Err(error) => {
                if connection.is_none() {
                    let state = self.0.state.lock().unwrap();
                    generation = state.generation;
                    revision = state.dirty;
                }
                self.drop_if_invalid(connection.as_ref(), &error);
                self.failed_snapshot(if matches!(error.status, Some(401 | 403)) {
                    rejected_credential()
                } else {
                    NativeError::new(
                        "native_unavailable",
                        format!("Native T3 tool catalog is unavailable: {}", error.message),
                    )
                })
            }
        };
        (snapshot, generation, revision)
    }
    fn failed_snapshot(&self, error: NativeError) -> NativeSnapshot {
        let state = self.0.state.lock().unwrap();
        NativeSnapshot {
            tools: state
                .catalog
                .as_ref()
                .map(|c| c.snapshot.tools.clone())
                .unwrap_or_default(),
            missing: state
                .catalog
                .as_ref()
                .map(|c| c.snapshot.missing.clone())
                .unwrap_or_else(|| {
                    contract::native_called_tool_names()
                        .map(str::to_owned)
                        .collect()
                }),
            fetched_at: state
                .catalog
                .as_ref()
                .and_then(|c| c.snapshot.fetched_at.clone()),
            error: Some(error),
        }
    }
    async fn connect(&self, credential: &NativeCredential) -> Result<Arc<Connection>, WireError> {
        let token_id = token_identity(&credential.access_token);
        loop {
            let connecting = {
                let mut state = self.0.state.lock().unwrap();
                if state.closed {
                    return Err(WireError::message("Native MCP client is closed."));
                }
                if let Some(c) = state.connection.as_ref().filter(|c| c.token_id == token_id) {
                    return Ok(c.clone());
                }
                state.connection = None;
                state
                    .connecting
                    .get_or_insert_with(|| {
                        let this = self.clone();
                        let token = credential.access_token.clone();
                        let token_id = token_id.clone();
                        let task = tokio::spawn(async move {
                            let result = this.open(token, token_id).await;
                            let mut state = this.0.state.lock().unwrap();
                            if let Ok(connection) = &result
                                && !state.closed
                            {
                                state.connection = Some(connection.clone());
                            }
                            state.connecting = None;
                            result
                        });
                        async move {
                            task.await
                                .unwrap_or_else(|e| Err(WireError::message(e.to_string())))
                        }
                        .boxed()
                        .shared()
                    })
                    .clone()
            };
            let connection = connecting.await?;
            if connection.token_id == token_id {
                return Ok(connection);
            }
        }
    }
    async fn open(&self, token: String, token_id: String) -> Result<Arc<Connection>, WireError> {
        let generation = {
            let mut state = self.0.state.lock().unwrap();
            if state.closed {
                return Err(WireError::message("Native MCP client is closed."));
            }
            state.generation += 1;
            state.dirty += 1;
            state.refresh_error = None;
            state.generation
        };
        let connection = Arc::new(Connection::new(
            self.0.http.clone(),
            self.0.url.clone(),
            token,
            token_id,
            generation,
        ));
        {
            let mut state = self.0.state.lock().unwrap();
            if state.closed {
                return Err(WireError::message("Native MCP client is closed."));
            }
            state
                .connections
                .retain(|connection| connection.strong_count() > 0);
            state.connections.push(Arc::downgrade(&connection));
        }
        let client = Arc::downgrade(&self.0);
        connection.observe(Arc::new(move |notification| {
            let Some(client) = client.upgrade() else {
                return;
            };
            {
                let mut state = client.state.lock().unwrap();
                if state.closed || state.generation != generation {
                    return;
                }
                state.dirty += 1;
                state.refresh_error = None;
            }
            let observer = client.observer.lock().unwrap().clone();
            if let Some(observer) = observer {
                observer(notification);
            }
        }));
        let result = connection
            .request(
                "initialize",
                json!({
                    "protocolVersion":"2025-11-25",
                    "capabilities":{},
                    "clientInfo":{"name":"t3-code-mcp","version":contract::GATEWAY_VERSION}
                }),
                Duration::from_secs(10),
                None,
            )
            .await?;
        if !result.get("capabilities").is_some_and(Value::is_object)
            || !result
                .get("serverInfo")
                .is_some_and(|info| info["name"].is_string() && info["version"].is_string())
        {
            return Err(WireError::message(
                "Native MCP returned an invalid initialize response.",
            ));
        }
        let protocol_version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                WireError::message("Native MCP returned an invalid initialize response.")
            })?;
        if !matches!(
            protocol_version,
            "2025-11-25" | "2025-06-18" | "2025-03-26" | "2024-11-05" | "2024-10-07"
        ) {
            return Err(WireError::message(
                "Native MCP returned an invalid initialize response.",
            ));
        }
        connection.set_protocol_version(protocol_version);
        connection.initialized().await?;
        Ok(connection)
    }
    fn drop_if_invalid(&self, connection: Option<&Arc<Connection>>, error: &WireError) -> bool {
        let Some(connection) = connection.filter(|_| matches!(error.status, Some(401 | 403 | 404)))
        else {
            return false;
        };
        let mut state = self.0.state.lock().unwrap();
        if state
            .connection
            .as_ref()
            .is_some_and(|c| Arc::ptr_eq(c, connection))
        {
            state.connection = None;
        }
        connection.close();
        true
    }
    pub async fn call_tool(
        &self,
        params: Map<String, Value>,
        mutating: bool,
        invocation: Option<crate::mcp::Invocation>,
    ) -> Result<NativeCallResult, NativeError> {
        let client = self.clone();
        let context = current_audit_context();
        tokio::spawn(async move {
            let action = client.execute(params, mutating, invocation);
            match context {
                Some(context) => {
                    with_audit_context(context.correlation_id, context.operation, action).await
                }
                None => action.await,
            }
        })
        .await
        .map_err(|error| NativeError::new("native_unavailable", error.to_string()))?
    }

    async fn execute(
        &self,
        params: Map<String, Value>,
        mutating: bool,
        invocation: Option<crate::mcp::Invocation>,
    ) -> Result<NativeCallResult, NativeError> {
        let name = params["name"].as_str().expect("validated native tool name");
        let empty = Map::new();
        let args = params
            .get("arguments")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        for attempt in 1..=3 {
            if invocation
                .as_ref()
                .is_some_and(crate::mcp::Invocation::is_cancelled)
            {
                return Err(NativeError {
                    code: "request_cancelled".into(),
                    message: "Request cancelled.".into(),
                    protocol_error: Some(Box::new(crate::mcp::RpcError::new(
                        -32800,
                        "Request cancelled.",
                    ))),
                });
            }
            let credential = self.require_credential()?;
            let id = format!("native_{}", uuid::Uuid::new_v4());
            let operation = format!("mcp:{name}");
            let started = now();
            record(
                self.0.audit.as_deref(),
                &id,
                &operation,
                "started",
                None,
                Some(AuditValue::object([
                    ("attempt", json!(attempt).into()),
                    (
                        "arguments",
                        summarize_for_audit(Value::Object(args.clone())),
                    ),
                ])),
            );
            let mut connection = None;
            let result = async {
                let c = self.connect(&credential).await?;
                connection = Some(c.clone());
                let result = c
                    .request(
                        "tools/call",
                        Value::Object(params.clone()),
                        timeout_for(name, args, self.0.timeout),
                        invocation.as_ref(),
                    )
                    .await?;
                parse_native_call_result(result)
            }
            .await;
            match result {
                Ok(result) => {
                    record(
                        self.0.audit.as_deref(),
                        &id,
                        &operation,
                        if result.is_error == Some(true) {
                            "error"
                        } else {
                            "completed"
                        },
                        Some(now() - started),
                        Some(
                            json!({"attempt":attempt,"isError":result.is_error==Some(true)}).into(),
                        ),
                    );
                    return Ok(result);
                }
                Err(error) => {
                    let failure = if error
                        .protocol_error
                        .as_ref()
                        .is_some_and(|error| !matches!(error.code, -32000 | -32001))
                    {
                        "rejected"
                    } else if matches!(error.status, Some(401 | 403)) {
                        "unauthorized"
                    } else if connection.is_none() || error.status == Some(404) || error.refused {
                        "unprocessed"
                    } else {
                        "unknown"
                    };
                    record(
                        self.0.audit.as_deref(),
                        &id,
                        &operation,
                        "error",
                        Some(now() - started),
                        Some(
                            json!({"attempt":attempt,"failure":failure,"error":error.message})
                                .into(),
                        ),
                    );
                    let dropped = self.drop_if_invalid(connection.as_ref(), &error);
                    if failure == "rejected" {
                        return Err(NativeError {
                            code: "native_rejected".into(),
                            message: format!("Native T3 rejected {name}: {}", error.message),
                            protocol_error: invocation
                                .as_ref()
                                .filter(|invocation| invocation.is_cancelled())
                                .map(|_| {
                                    Box::new(crate::mcp::RpcError::new(
                                        -32800,
                                        "Request cancelled.",
                                    ))
                                }),
                        });
                    }
                    if failure == "unauthorized" {
                        return Err(rejected_credential());
                    }
                    let processed = failure == "unknown";
                    let retry = if attempt == 1 {
                        !processed || !mutating
                    } else {
                        dropped && attempt < 3
                    };
                    if retry
                        && !invocation
                            .as_ref()
                            .is_some_and(crate::mcp::Invocation::is_cancelled)
                    {
                        continue;
                    }
                    if processed && mutating {
                        return Err(NativeError::new(
                            "native_outcome_unknown",
                            format!(
                                "{name} was sent to native T3, but its result was lost ({}). Check the target with t3_thread_read before retrying. Tools with clientRequestId are safe to retry with the same value.",
                                error.message
                            ),
                        ));
                    }
                    return Err(NativeError::new(
                        "native_unavailable",
                        format!("Native T3 MCP is unavailable: {}", error.message),
                    ));
                }
            }
        }
        unreachable!("retry budget returns on its last attempt")
    }
    pub async fn upload_attachment(
        &self,
        relative_url: &str,
        bytes: Vec<u8>,
        mime_type: &str,
    ) -> Result<(), NativeError> {
        let base = url::Url::parse(&self.0.url)
            .map_err(|e| NativeError::new("native_upload_rejected", e.to_string()))?;
        let origin = base.origin();
        let origin_url = url::Url::parse(&format!("{}/", origin.ascii_serialization()))
            .map_err(|e| NativeError::new("native_upload_rejected", e.to_string()))?;
        let target = origin_url
            .join(relative_url)
            .map_err(|e| NativeError::new("native_upload_rejected", e.to_string()))?;
        if target.origin() != origin {
            return Err(NativeError::new(
                "native_upload_rejected",
                "Native T3 returned an upload URL outside its own origin.",
            ));
        }
        let id = format!("native_{}", uuid::Uuid::new_v4());
        let started = now();
        record(
            self.0.audit.as_deref(),
            &id,
            "attachment.upload",
            "started",
            None,
            Some(json!({"sizeBytes":bytes.len(),"mimeType":mime_type}).into()),
        );
        let mut status = None;
        let result = async {
            let response = self
                .0
                .upload
                .post(target)
                .header("content-type", mime_type)
                .body(bytes)
                .timeout(self.0.timeout)
                .send()
                .await
                .map_err(|error| {
                    NativeError::new(
                        "native_upload_failed",
                        format!("Attachment upload failed: {}", error.without_url()),
                    )
                })?;
            status = Some(response.status().as_u16());
            if response.status().is_redirection() {
                return Err(NativeError::new(
                    "native_upload_failed",
                    "Attachment upload failed: fetch failed",
                ));
            }
            if !response.status().is_success() {
                let status = response.status().as_u16();
                let detail = response
                    .text()
                    .await
                    .unwrap_or_default()
                    .chars()
                    .take(200)
                    .collect::<String>();
                return Err(NativeError::new(
                    "native_upload_failed",
                    format!("Native T3 refused the upload (HTTP {status}): {detail}"),
                ));
            }
            Ok(())
        }
        .await;
        record(
            self.0.audit.as_deref(),
            &id,
            "attachment.upload",
            if status.is_some_and(|status| status < 300) {
                "completed"
            } else {
                "error"
            },
            Some(now() - started),
            Some(json!({"status":status}).into()),
        );
        result
    }
    pub fn close(&self) {
        let mut state = self.0.state.lock().unwrap();
        state.closed = true;
        for connection in state.connections.iter().filter_map(Weak::upgrade) {
            connection.close();
        }
        state.connection = None;
    }
}
pub fn timeout_for(name: &str, args: &Map<String, Value>, default: Duration) -> Duration {
    if let Some(budget) = args
        .get("timeoutMs")
        .and_then(Value::as_f64)
        .filter(|b| b.is_finite() && *b > 0.0)
    {
        return Duration::from_secs_f64(
            ((default.as_secs_f64() * 1000.0)
                .max(budget + 15_000.0)
                .min(2_147_483_647.0))
                / 1000.0,
        );
    }
    if name == "t3_thread_wait" {
        Duration::from_millis(3_615_000)
    } else {
        default
    }
}
fn credential_error(state: NativeCredentialState) -> NativeError {
    match state {
        NativeCredentialState::Missing { reason } => {
            NativeError::new("native_credential_missing", reason)
        }
        NativeCredentialState::Expired { credential } => NativeError::new(
            "native_credential_expired",
            format!(
                "The native MCP credential expired at {}. Run `t3-code-mcp native-login` with a new pairing code.",
                credential.expires_at
            ),
        ),
        NativeCredentialState::Valid { .. } => {
            unreachable!("only failed credentials have an error")
        }
    }
}
fn rejected_credential() -> NativeError {
    NativeError::new(
        "native_credential_rejected",
        "Native T3 rejected the stored MCP credential. Run `t3-code-mcp native-login` with a new pairing code.",
    )
}

#[cfg(test)]
#[path = "native_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "native_wire_tests.rs"]
mod wire_tests;
