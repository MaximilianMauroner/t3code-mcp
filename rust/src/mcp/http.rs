use ::http::{HeaderMap, Method, Request, Response};
use bytes::Bytes;
use futures_util::{future::join_all, stream};
use http_body::Frame;
use http_body_util::{BodyExt, Full, StreamBody, combinators::UnsyncBoxBody};
use serde_json::{Value, json};
use std::{io, sync::Arc, time::Duration};

use super::{
    AdmittedMessage, Delivery, IncomingMessage, McpServer, Output, Peer, RpcError, RpcResponse,
    SUPPORTED_PROTOCOL_VERSIONS, ToolHandler,
};

pub type HttpBody = UnsyncBoxBody<Bytes, io::Error>;

enum HttpPlan {
    Immediate(Response<Bytes>),
    Listen(Peer),
    Post {
        messages: Vec<AdmittedMessage>,
        peer: Peer,
        issued: Option<String>,
        progress: bool,
        scoped: bool,
    },
}

impl From<Response<Bytes>> for HttpPlan {
    fn from(response: Response<Bytes>) -> Self {
        Self::Immediate(response)
    }
}

pub const DEFAULT_HTTP_BODY_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct HttpOptions {
    pub bearer_token: Option<String>,
    pub max_body_bytes: usize,
}

impl Default for HttpOptions {
    fn default() -> Self {
        Self {
            bearer_token: None,
            max_body_bytes: DEFAULT_HTTP_BODY_LIMIT,
        }
    }
}

impl McpServer {
    pub(crate) fn should_read_body(&self, request: &Request<()>, options: &HttpOptions) -> bool {
        let base = url::Url::parse("http://localhost").expect("static URL");
        let path = base
            .join(&request.uri().to_string())
            .map(|url| url.path().to_owned());
        path.as_deref().ok() == Some("/mcp")
            && request.method() == Method::POST
            && authorized(request.headers(), options.bearer_token.as_deref())
    }

    pub async fn handle_http(
        &self,
        request: Request<Bytes>,
        options: &HttpOptions,
        handler: &impl ToolHandler,
    ) -> Response<Bytes> {
        let head = request.method() == Method::HEAD;
        let response = match self.prepare_http(request, options) {
            HttpPlan::Immediate(response) => response,
            HttpPlan::Listen(_) => sse_response(Bytes::new()),
            HttpPlan::Post {
                messages,
                peer,
                issued,
                scoped,
                ..
            } => {
                let response = self.execute_post(messages, issued, handler).await;
                if !scoped {
                    peer.close();
                }
                response
            }
        };
        if head {
            response.map(|_| Bytes::new())
        } else {
            response
        }
    }

    pub async fn handle_http_stream<H>(
        &self,
        request: Request<Bytes>,
        options: &HttpOptions,
        handler: Arc<H>,
    ) -> Response<HttpBody>
    where
        H: ToolHandler + Send + 'static,
    {
        let head = request.method() == Method::HEAD;
        match self.prepare_http(request, options) {
            HttpPlan::Immediate(response) => full(if head {
                response.map(|_| Bytes::new())
            } else {
                response
            }),
            HttpPlan::Listen(peer) => {
                sse_response(sse_body(peer.subscribe(), None, self.shutdown.clone()))
            }
            HttpPlan::Post {
                messages,
                peer,
                issued,
                progress,
                scoped,
            } => {
                let (delivery, mut output) = Delivery::channel();
                for message in &messages {
                    message.set_output(delivery.clone());
                }
                let server = self.clone();
                let (sender, receiver) = tokio::sync::oneshot::channel();
                let task = tokio::spawn(async move {
                    let response = server
                        .execute_post(messages, issued, handler.as_ref())
                        .await;
                    if !response.body().is_empty() {
                        delivery.send_bytes(response.body().clone());
                    }
                    delivery.finish();
                    if !scoped {
                        peer.close();
                    }
                    let _ = sender.send(response);
                });
                self.retain_worker(task);
                if progress {
                    return sse_response(sse_body(
                        output,
                        None,
                        tokio_util::sync::CancellationToken::new(),
                    ));
                }
                let first = output.recv().await;
                if matches!(&first, Some(Err(_))) {
                    return sse_response(sse_body(
                        output,
                        first,
                        tokio_util::sync::CancellationToken::new(),
                    ));
                }
                if let Some(Ok(bytes)) = &first {
                    let fields = serde_json::from_slice::<
                        std::collections::HashMap<String, Box<serde_json::value::RawValue>>,
                    >(bytes);
                    if fields.is_ok_and(|fields| fields.contains_key("method")) {
                        return sse_response(sse_body(
                            output,
                            first,
                            tokio_util::sync::CancellationToken::new(),
                        ));
                    }
                }
                match receiver.await {
                    Ok(response) => full(response),
                    _ => full(protocol_error(500, -32603, "MCP worker failed.")),
                }
            }
        }
    }

    async fn execute_post(
        &self,
        messages: Vec<AdmittedMessage>,
        issued: Option<String>,
        handler: &impl ToolHandler,
    ) -> Response<Bytes> {
        let responses = join_all(
            messages
                .into_iter()
                .map(|message| self.execute_message(message, handler)),
        )
        .await
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        let valid_initialize = matches!(responses.as_slice(), [RpcResponse::Success { .. }]);
        let mut response = match responses.len() {
            0 => empty(202),
            1 => json_response(
                200,
                responses.into_iter().next().expect("one response"),
                "application/json",
            ),
            _ => json_response(200, responses, "application/json"),
        };
        if let Some(id) = issued {
            if valid_initialize {
                response
                    .headers_mut()
                    .insert("mcp-session-id", id.parse().expect("opaque session ID"));
            } else {
                self.sessions.lock().unwrap().delete(&id);
            }
        }
        response
    }

    fn prepare_http(&self, request: Request<Bytes>, options: &HttpOptions) -> HttpPlan {
        let base = url::Url::parse("http://localhost").expect("static URL");
        let url = base.join(&request.uri().to_string());
        let path = url.as_ref().map(|u| u.path()).unwrap_or("/");
        if path == "/healthz" {
            return gateway_json(200, json!({"ok":true,"service":"t3-code-mcp"})).into();
        }
        if path != "/mcp" {
            return gateway_json(404, json!({"error":"not_found"})).into();
        }
        if !authorized(request.headers(), options.bearer_token.as_deref()) {
            let (status, message) = if options.bearer_token.is_none() {
                (503, "MCP_BEARER_TOKEN is not configured.")
            } else {
                (401, "Unauthorized.")
            };
            let mut response = gateway_json(status, json!({"error":message}));
            response
                .headers_mut()
                .insert("www-authenticate", "Bearer".parse().expect("static header"));
            return response.into();
        }
        if self.sessions.lock().unwrap().is_closed() {
            return protocol_error(503, -32000, "MCP server is closing.").into();
        }
        let session_id = request.headers().get("mcp-session-id");
        let auth = options
            .bearer_token
            .as_deref()
            .expect("authenticated request");
        let peer = if let Some(id) = session_id {
            let Some(peer) = id
                .to_str()
                .ok()
                .and_then(|id| self.sessions.lock().unwrap().get(id, auth))
            else {
                return protocol_error(404, -32000, "MCP session not found.").into();
            };
            peer
        } else {
            Peer::default()
        };
        match *request.method() {
            Method::GET => {
                if !header(request.headers(), "accept").contains("text/event-stream") {
                    return protocol_error(
                        406,
                        -32000,
                        "Not Acceptable: Client must accept text/event-stream",
                    )
                    .into();
                }
                if let Some(error) = version_error(request.headers()) {
                    return error.into();
                }
                HttpPlan::Listen(peer)
            }
            Method::DELETE => {
                if let Some(error) = version_error(request.headers()) {
                    return error.into();
                }
                if let Some(id) = session_id.and_then(|id| id.to_str().ok()) {
                    self.sessions.lock().unwrap().delete(id);
                }
                empty(200).into()
            }
            Method::POST => self.prepare_post(request, options.max_body_bytes, peer, auth),
            _ => {
                let mut response = protocol_error(405, -32000, "Method not allowed.");
                response
                    .headers_mut()
                    .insert("allow", "GET, POST, DELETE".parse().expect("static header"));
                response.into()
            }
        }
    }

    fn prepare_post(
        &self,
        request: Request<Bytes>,
        limit: usize,
        mut peer: Peer,
        auth: &str,
    ) -> HttpPlan {
        if request.body().len() > limit {
            return gateway_json(400, json!({"error":"Request body is too large."})).into();
        }
        let parsed = if request.body().is_empty() {
            None
        } else {
            match serde_json::from_slice::<Value>(request.body()) {
                Ok(value) => Some(value),
                Err(_) => return protocol_error(400, -32700, "Parse error: Invalid JSON").into(),
            }
        };
        let accept = header(request.headers(), "accept");
        if !accept.contains("application/json") || !accept.contains("text/event-stream") {
            return protocol_error(
                406,
                -32000,
                "Not Acceptable: Client must accept both application/json and text/event-stream",
            )
            .into();
        }
        if !json_content_type(&header(request.headers(), "content-type")) {
            return protocol_error(
                415,
                -32000,
                "Unsupported Media Type: Content-Type must be application/json",
            )
            .into();
        }
        let Some(parsed) = parsed else {
            return protocol_error(400, -32700, "Parse error: Invalid JSON").into();
        };
        let values = match parsed {
            Value::Array(values) if values.is_empty() => {
                return protocol_error(400, -32600, "Invalid Request: Empty batch").into();
            }
            Value::Array(values) => values,
            value => vec![value],
        };
        let messages: Result<Vec<_>, _> = values.into_iter().map(IncomingMessage::parse).collect();
        let Ok(messages) = messages else {
            return protocol_error(400, -32600, "Invalid Request: Invalid JSON-RPC message").into();
        };
        let initialize = messages.iter().any(IncomingMessage::is_initialize);
        if initialize && messages.len() > 1 {
            return protocol_error(
                400,
                -32600,
                "Invalid Request: Only one initialization request is allowed",
            )
            .into();
        }
        if !initialize && let Some(error) = version_error(request.headers()) {
            return error.into();
        }
        let mut issued = None;
        if initialize && !request.headers().contains_key("mcp-session-id") {
            let Some((id, created)) = self.sessions.lock().unwrap().create(auth) else {
                return protocol_error(503, -32000, "MCP session limit reached.").into();
            };
            peer = created;
            issued = Some(id);
        }
        let scoped = request.headers().contains_key("mcp-session-id") || issued.is_some();
        let progress = messages.iter().any(|message| match message {
            IncomingMessage::Request(request) if request.method == "tools/call" => request
                .params
                .as_ref()
                .and_then(|params| params.get("_meta"))
                .and_then(|meta| meta.get("progressToken"))
                .is_some(),
            _ => false,
        });
        let messages = messages
            .into_iter()
            .map(|message| self.admit(message, peer.clone()))
            .collect();
        HttpPlan::Post {
            messages,
            peer,
            issued,
            progress,
            scoped,
        }
    }
}

pub(crate) fn authorized(headers: &HeaderMap, expected: Option<&str>) -> bool {
    let Some(expected) = expected else {
        return false;
    };
    let Some(supplied) = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };
    if supplied.len() != expected.len() {
        return false;
    }
    supplied
        .bytes()
        .zip(expected.bytes())
        .fold(0_u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

fn header(headers: &HeaderMap, name: &str) -> String {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect::<Vec<_>>()
        .join(", ")
}

fn version_error(headers: &HeaderMap) -> Option<Response<Bytes>> {
    let version = headers.get("mcp-protocol-version")?;
    let version = version.to_str().unwrap_or_default();
    (!SUPPORTED_PROTOCOL_VERSIONS.contains(&version)).then(|| {
        protocol_error(
            400,
            -32000,
            &format!(
                "Bad Request: Unsupported protocol version: {version} (supported versions: {})",
                SUPPORTED_PROTOCOL_VERSIONS.join(", ")
            ),
        )
    })
}

fn json_content_type(value: &str) -> bool {
    let essence = value.split(';').next().unwrap_or_default().trim();
    if !essence.eq_ignore_ascii_case("application/json") {
        return false;
    }
    // A comma inside a quoted parameter is valid. An unquoted comma joins duplicate headers.
    let mut quoted = false;
    let mut escaped = false;
    for c in value.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if quoted && c == '\\' {
            escaped = true;
            continue;
        }
        if c == '"' {
            quoted = !quoted;
        }
        if c == ',' && !quoted {
            return false;
        }
    }
    !quoted || !value.contains(',')
}

fn empty(status: u16) -> Response<Bytes> {
    Response::builder()
        .status(status)
        .body(Bytes::new())
        .expect("static status")
}

fn gateway_json(status: u16, body: Value) -> Response<Bytes> {
    json_response(status, body, "application/json; charset=utf-8")
}

pub(crate) fn body_too_large() -> Response<Bytes> {
    gateway_json(400, json!({"error":"Request body is too large."}))
}

fn json_response(status: u16, body: impl serde::Serialize, content_type: &str) -> Response<Bytes> {
    Response::builder()
        .status(status)
        .header("content-type", content_type)
        .body(Bytes::from(
            serde_json::to_vec(&body).expect("JSON response"),
        ))
        .expect("static response")
}

fn protocol_error(status: u16, code: i64, message: &str) -> Response<Bytes> {
    json_response(
        status,
        RpcError::new(code, message).response(None),
        "application/json",
    )
}

fn full(response: Response<Bytes>) -> Response<HttpBody> {
    response.map(|bytes| {
        Full::new(bytes)
            .map_err(|never| match never {})
            .boxed_unsync()
    })
}

fn sse_response<B>(body: B) -> Response<B> {
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache, no-transform")
        .header("connection", "keep-alive")
        .header("x-accel-buffering", "no")
        .body(body)
        .expect("static SSE response")
}

fn sse_body(
    output: Output,
    first: Option<io::Result<Bytes>>,
    shutdown: tokio_util::sync::CancellationToken,
) -> HttpBody {
    let interval = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(15),
        Duration::from_secs(15),
    );
    let frames = stream::unfold(
        (output, first, interval, shutdown),
        |(mut output, mut first, mut interval, shutdown)| async move {
            let message = if let Some(first) = first.take() {
                Some(first)
            } else {
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => None,
                    message = output.recv() => message,
                    _ = interval.tick() => return Some((Ok(Frame::data(Bytes::from_static(b": keepalive\n\n"))), (output, first, interval, shutdown))),
                }
            };
            message.map(|message| {
                (
                    message.map(|bytes| {
                        let mut frame = Vec::with_capacity(bytes.len() + 24);
                        frame.extend_from_slice(b"event: message\ndata: ");
                        frame.extend_from_slice(&bytes);
                        frame.extend_from_slice(b"\n\n");
                        Frame::data(Bytes::from(frame))
                    }),
                    (output, first, interval, shutdown),
                )
            })
        },
    );
    StreamBody::new(frames).boxed_unsync()
}
