use crate::mcp::{Invocation, RpcError};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::future::Future;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

type Pending = Mutex<HashMap<u64, PendingCall>>;

struct PendingCall {
    result: oneshot::Sender<Result<Value, WireError>>,
    progress: Option<(Value, crate::mcp::Delivery)>,
}

use super::NotificationObserver;

// Native ProgressToken uses Schema.Finite, as does normalize_meta's f64 check.
// Compare finite IEEE-754 values without rewriting the retained JSON tokens.
fn progress_tokens_match(expected: &Value, received: &Value) -> bool {
    match (expected, received) {
        (Value::String(expected), Value::String(received)) => expected == received,
        (Value::Number(expected), Value::Number(received)) => {
            match (expected.as_f64(), received.as_f64()) {
                (Some(expected), Some(received)) => {
                    expected.is_finite() && received.is_finite() && expected == received
                }
                _ => false,
            }
        }
        _ => false,
    }
}

#[derive(Clone)]
struct Transport {
    http: reqwest::Client,
    url: String,
    token: String,
    session: Arc<Mutex<Session>>,
    cancellation: CancellationToken,
}
#[derive(Default)]
struct Session {
    id: Option<String>,
    protocol: Option<String>,
}

pub(super) struct Connection {
    transport: Transport,
    pub(super) token_id: String,
    pub(super) generation: u64,
    next_id: AtomicU64,
    pending: Pending,
    observer: Mutex<Option<NotificationObserver>>,
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.close();
    }
}
impl Connection {
    pub(super) fn new(
        http: reqwest::Client,
        url: String,
        token: String,
        token_id: String,
        generation: u64,
    ) -> Self {
        Self {
            transport: Transport {
                http,
                url,
                token,
                session: Arc::new(Mutex::new(Session::default())),
                cancellation: CancellationToken::new(),
            },
            token_id,
            generation,
            next_id: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            observer: Mutex::new(None),
        }
    }
    pub(super) fn observe(&self, observer: NotificationObserver) {
        *self.observer.lock().unwrap() = Some(observer);
    }

    pub(super) fn set_protocol_version(&self, version: &str) {
        self.transport.session.lock().unwrap().protocol = Some(version.into());
    }
    pub(super) fn close(&self) {
        self.transport.cancellation.cancel();
        self.pending.lock().unwrap().clear();
    }
    pub(super) async fn initialized(self: &Arc<Self>) -> Result<(), WireError> {
        let status = self
            .transport
            .notify(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await?;
        if status == 202 {
            self.listen(None, None);
        }
        Ok(())
    }
    pub(super) async fn request(
        self: &Arc<Self>,
        method: &str,
        params: Value,
        timeout: Duration,
        invocation: Option<&Invocation>,
    ) -> Result<Value, WireError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, mut receiver) = oneshot::channel();
        let progress = params
            .get("_meta")
            .and_then(|meta| meta.get("progressToken"))
            .filter(|token| token.is_string() || token.as_f64().is_some_and(f64::is_finite))
            .and_then(|token| {
                invocation
                    .and_then(Invocation::output)
                    .map(|output| (token.clone(), output))
            });
        self.pending.lock().unwrap().insert(
            id,
            PendingCall {
                result: sender,
                progress,
            },
        );
        let _guard = PendingRequest {
            connection: self,
            id,
        };
        let request = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let work = async {
            let post = self.post(request, id);
            tokio::pin!(post);
            let mut first_complete = false;
            if let Some(invocation) = invocation {
                // The first poll and cancellation use the same admission lock.
                let first = futures_util::future::poll_fn(|cx| {
                    std::task::Poll::Ready(invocation.admit_send(|| post.as_mut().poll(cx)))
                })
                .await;
                match first {
                    None => {
                        return Err(WireError::protocol(RpcError::new(
                            -32800,
                            "Request cancelled.",
                        )));
                    }
                    Some(std::task::Poll::Ready(result)) => {
                        result?;
                        first_complete = true;
                    }
                    Some(std::task::Poll::Pending) => {}
                }
            }
            let cancel = async {
                let Some(invocation) = invocation else {
                    return std::future::pending::<()>().await;
                };
                let mut params = invocation.cancellation().await;
                let active = self.pending.lock().unwrap().contains_key(&id);
                if active {
                    params.insert("requestId".into(), json!(id));
                    let _ = self.transport.notify(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":params})).await;
                }
            };
            tokio::pin!(cancel);
            let mut cancelled = false;
            let mut posted = first_complete;
            loop {
                tokio::select! {
                    biased;
                    result = &mut receiver => break result.unwrap_or_else(|_| Err(WireError::closed())),
                    _ = &mut cancel, if !cancelled => { cancelled = true; },
                    result = &mut post, if !posted => { result?; posted = true; }
                }
            }
        };
        tokio::select! {
            biased;
            _ = self.transport.cancellation.cancelled() => Err(WireError::closed()),
            result = tokio::time::timeout(timeout, work) => match result {
                Ok(result) => result,
                Err(_) => {
                    if !invocation.is_some_and(Invocation::is_cancelled) {
                        let transport = self.transport.clone();
                        tokio::spawn(async move {
                            let _ = transport.notify(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id,"reason":"McpError: MCP error -32001: Request timed out"}})).await;
                        });
                    }
                    Err(WireError::timeout())
                }
            }
        }
    }
    async fn post(self: &Arc<Self>, message: Value, request_id: u64) -> Result<(), WireError> {
        let response = self.transport.post(&message).await?;
        if response.status().as_u16() == 202 {
            return Ok(());
        }
        match media_type(&response).as_str() {
            "application/json" => {
                let value = response
                    .json::<Value>()
                    .await
                    .map_err(WireError::transport)?;
                self.receive_from(value, Some(request_id)).map(|_| ())
            }
            "text/event-stream" => {
                let mut summary = StreamSummary::default();
                let result = read_events(
                    Arc::downgrade(self),
                    response,
                    &mut summary,
                    Some(request_id),
                )
                .await;
                if !summary.received && summary.last_id.is_some() {
                    self.listen(summary.last_id, summary.retry);
                    Ok(())
                } else {
                    result
                }
            }
            _ => Err(WireError::message(
                "Native MCP returned an unexpected content type.",
            )),
        }
    }
    fn receive_from(
        self: &Arc<Self>,
        value: Value,
        origin: Option<u64>,
    ) -> Result<Vec<u64>, WireError> {
        let values = match value {
            Value::Array(values) => values,
            value => vec![value],
        };
        let frames = values
            .into_iter()
            .map(Frame::parse)
            .collect::<Result<Vec<_>, _>>()?;
        let mut received = Vec::new();
        for frame in frames {
            match frame {
                Frame::Response { id, result } => {
                    if let Some(id) = id.as_u64()
                        && let Some(sender) = self.pending.lock().unwrap().remove(&id)
                    {
                        received.push(id);
                        let _ = sender.result.send(result);
                    }
                }
                Frame::Request { id, method } => {
                    let response = if method == "ping" {
                        json!({"jsonrpc":"2.0","id":id,"result":{}})
                    } else {
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}})
                    };
                    let transport = self.transport.clone();
                    tokio::spawn(async move {
                        let _ = transport.notify(response).await;
                    });
                }
                Frame::Notification { method, value } => {
                    if method == "notifications/tools/list_changed" {
                        let observer = self.observer.lock().unwrap().clone();
                        if let Some(observer) = observer {
                            observer(value);
                        }
                    } else if method == "notifications/progress" {
                        let token = &value["params"]["progressToken"];
                        if !(token.is_string() || token.as_f64().is_some_and(f64::is_finite))
                            || !value["params"]["progress"]
                                .as_f64()
                                .is_some_and(f64::is_finite)
                        {
                            continue;
                        }
                        let pending = self.pending.lock().unwrap();
                        let output = if let Some(origin) = origin {
                            pending
                                .get(&origin)
                                .and_then(|call| call.progress.as_ref())
                                .filter(|(expected, _)| progress_tokens_match(expected, token))
                                .map(|(_, output)| output)
                        } else {
                            let mut routes = pending
                                .values()
                                .filter_map(|call| call.progress.as_ref())
                                .filter(|(expected, _)| progress_tokens_match(expected, token));
                            let first = routes.next();
                            if routes.next().is_none() {
                                first.map(|(_, output)| output)
                            } else {
                                None
                            }
                        };
                        if let Some(output) = output {
                            output.send(&value);
                        }
                    }
                }
            }
        }
        Ok(received)
    }
    fn listen(self: &Arc<Self>, last_id: Option<String>, retry: Option<u64>) {
        let connection = Arc::downgrade(self);
        let transport = self.transport.clone();
        tokio::spawn(async move {
            let work = async {
                let mut last_id = last_id;
                let mut retry = retry;
                let mut failures = 0;
                let mut resuming = last_id.is_some();
                loop {
                    if resuming {
                        tokio::time::sleep(Duration::from_millis(
                            retry.unwrap_or(if failures == 0 { 1000 } else { 1500 }),
                        ))
                        .await;
                    }
                    let mut request = transport
                        .headers(transport.http.get(&transport.url))
                        .header("accept", "text/event-stream");
                    if let Some(id) = last_id.as_ref().filter(|id| !id.is_empty()) {
                        request = request.header("last-event-id", id);
                    }
                    match request.send().await {
                        Ok(response) if response.status().as_u16() == 405 => return,
                        Ok(response)
                            if response.status().is_success()
                                && media_type(&response) == "text/event-stream" =>
                        {
                            let mut summary = StreamSummary::default();
                            let _ =
                                read_events(connection.clone(), response, &mut summary, None).await;
                            if summary.received {
                                return;
                            }
                            last_id = summary.last_id.or(last_id);
                            retry = summary.retry.or(retry);
                            failures = 0;
                            resuming = true;
                        }
                        _ => {
                            if !resuming {
                                return;
                            }
                            failures += 1;
                            if failures >= 2 {
                                return;
                            }
                        }
                    }
                }
            };
            tokio::select! { biased; _ = transport.cancellation.cancelled() => {}, _ = work => {} }
        });
    }
}
struct PendingRequest<'a> {
    connection: &'a Connection,
    id: u64,
}
impl Drop for PendingRequest<'_> {
    fn drop(&mut self) {
        self.connection.pending.lock().unwrap().remove(&self.id);
    }
}
impl Transport {
    fn headers(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut request = request.bearer_auth(&self.token);
        let session = self.session.lock().unwrap();
        if let Some(id) = &session.id {
            request = request.header("mcp-session-id", id);
        }
        if let Some(protocol) = &session.protocol {
            request = request.header("mcp-protocol-version", protocol);
        }
        request
    }
    async fn post(&self, message: &Value) -> Result<reqwest::Response, WireError> {
        let request = self
            .headers(self.http.post(&self.url))
            .header("accept", "application/json, text/event-stream")
            .json(message);
        let response = request.send().await.map_err(WireError::transport)?;
        if let Some(id) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .filter(|id| !id.is_empty())
        {
            self.session.lock().unwrap().id = Some(id.into());
        }
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(WireError::http(
                status,
                format!("Streamable HTTP error: Error POSTing to endpoint: {body}"),
            ));
        }
        Ok(response)
    }
    async fn notify(&self, message: Value) -> Result<u16, WireError> {
        tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => Err(WireError::closed()),
            result = tokio::time::timeout(Duration::from_secs(10), self.post(&message)) => result.map_err(|_| WireError::timeout())?.map(|r|r.status().as_u16()),
        }
    }
}
fn media_type(response: &reqwest::Response) -> String {
    response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}
#[derive(Default)]
struct StreamSummary {
    received: bool,
    last_id: Option<String>,
    retry: Option<u64>,
}
async fn read_events(
    connection: Weak<Connection>,
    response: reqwest::Response,
    summary: &mut StreamSummary,
    request_id: Option<u64>,
) -> Result<(), WireError> {
    let mut stream = response.bytes_stream();
    let mut decoder = EventDecoder::default();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(WireError::transport)?;
        for event in decoder.push(&chunk) {
            if let Some(id) = event.id.filter(|id| !id.is_empty()) {
                summary.last_id = Some(id);
            }
            summary.retry = event.retry.or(summary.retry);
            if !matches!(event.name.as_deref(), None | Some("message") | Some("")) {
                continue;
            }
            if let Some(data) = event.data.filter(|data| !data.is_empty()) {
                let value: Value =
                    serde_json::from_str(&data).map_err(|e| WireError::message(e.to_string()))?;
                let Some(connection) = connection.upgrade() else {
                    return Ok(());
                };
                let received = connection.receive_from(value, request_id)?;
                summary.received |= match request_id {
                    Some(request_id) => received.contains(&request_id),
                    None => !received.is_empty(),
                };
            }
        }
    }
    Ok(())
}
#[derive(Default)]
struct Event {
    name: Option<String>,
    data: Option<String>,
    id: Option<String>,
    retry: Option<u64>,
}
#[derive(Default)]
struct EventDecoder {
    line: Vec<u8>,
    event: Event,
    skip_lf: bool,
    started: bool,
}
impl EventDecoder {
    fn push(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut events = Vec::new();
        for &byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\n' || byte == b'\r' {
                self.skip_lf = byte == b'\r';
                if let Some(event) = self.end_line() {
                    events.push(event);
                }
            } else {
                self.line.push(byte);
            }
        }
        events
    }
    fn end_line(&mut self) -> Option<Event> {
        let line = std::mem::take(&mut self.line);
        let line = if !self.started {
            self.started = true;
            line.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&line)
        } else {
            &line
        };
        if line.is_empty() {
            let mut event = std::mem::take(&mut self.event);
            if let Some(data) = &mut event.data {
                data.pop();
            }
            return Some(event);
        }
        let (field, value) = match line.iter().position(|byte| *byte == b':') {
            Some(index) => (
                &line[..index],
                line[index + 1..]
                    .strip_prefix(b" ")
                    .unwrap_or(&line[index + 1..]),
            ),
            None => (line, &[][..]),
        };
        match field {
            b"data" => {
                let data = self.event.data.get_or_insert_default();
                data.push_str(&String::from_utf8_lossy(value));
                data.push('\n');
            }
            b"event" => {
                self.event.name = Some(String::from_utf8_lossy(value).into_owned());
            }
            b"id" if !value.contains(&0) => {
                self.event.id = Some(String::from_utf8_lossy(value).into_owned());
            }
            b"retry" if !value.is_empty() && value.iter().all(u8::is_ascii_digit) => {
                self.event.retry = std::str::from_utf8(value)
                    .ok()
                    .and_then(|value| value.parse().ok());
            }
            _ => {}
        }
        None
    }
}
enum Frame {
    Response {
        id: Value,
        result: Result<Value, WireError>,
    },
    Request {
        id: Value,
        method: String,
    },
    Notification {
        method: String,
        value: Value,
    },
}
impl Frame {
    fn parse(value: Value) -> Result<Self, WireError> {
        let object = value.as_object().ok_or_else(invalid_frame)?;
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(invalid_frame());
        }
        if let Some(method) = object.get("method") {
            let method = method.as_str().ok_or_else(invalid_frame)?.to_owned();
            if object.get("params").is_some_and(|v| !v.is_object()) {
                return Err(invalid_frame());
            }
            return match object.get("id") {
                Some(id) if valid_id(id) => Ok(Self::Request {
                    id: id.clone(),
                    method,
                }),
                None => Ok(Self::Notification { method, value }),
                _ => Err(invalid_frame()),
            };
        }
        let id = object
            .get("id")
            .filter(|id| valid_id(id))
            .ok_or_else(invalid_frame)?
            .clone();
        let result = if let Some(error) = object.get("error") {
            if object.contains_key("result") {
                return Err(invalid_frame());
            }
            let code = error
                .get("code")
                .and_then(Value::as_i64)
                .ok_or_else(invalid_frame)?;
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .ok_or_else(invalid_frame)?;
            Err(WireError::protocol(RpcError {
                code,
                message: message.into(),
                data: error.get("data").cloned(),
            }))
        } else {
            object
                .get("result")
                .filter(|r| r.is_object())
                .cloned()
                .ok_or_else(invalid_frame)
        };
        Ok(Self::Response { id, result })
    }
}
fn valid_id(id: &Value) -> bool {
    id.is_string() || id.as_i64().is_some() || id.as_u64().is_some()
}
fn invalid_frame() -> WireError {
    WireError::message("Native MCP returned an invalid JSON-RPC message.")
}

#[derive(Debug, Clone)]
pub(super) struct WireError {
    pub(super) message: String,
    pub(super) status: Option<u16>,
    pub(super) protocol_error: Option<Box<RpcError>>,
    pub(super) refused: bool,
}
impl WireError {
    pub(super) fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: None,
            protocol_error: None,
            refused: false,
        }
    }
    fn closed() -> Self {
        Self::message("MCP error -32000: Connection closed")
    }
    fn timeout() -> Self {
        Self::message("MCP error -32001: Request timed out")
    }
    fn http(status: u16, message: String) -> Self {
        Self {
            message,
            status: Some(status),
            protocol_error: None,
            refused: false,
        }
    }
    fn protocol(error: RpcError) -> Self {
        Self {
            message: format!("MCP error {}: {}", error.code, error.message),
            status: None,
            protocol_error: Some(Box::new(error)),
            refused: false,
        }
    }
    fn transport(error: reqwest::Error) -> Self {
        let mut result = Self::message(error.to_string());
        let mut source = std::error::Error::source(&error);
        while let Some(error) = source {
            if let Some(error) = error.downcast_ref::<std::io::Error>() {
                result.refused |= error.kind() == std::io::ErrorKind::ConnectionRefused;
            }
            source = error.source();
        }
        result
    }
}
