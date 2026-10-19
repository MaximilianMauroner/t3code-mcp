use std::{convert::Infallible, future::Future, io, sync::Arc, time::Duration};

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use tokio::{
    io::{AsyncReadExt, BufReader},
    net::TcpListener,
    task::JoinSet,
    time::Instant,
};

use crate::{
    gateway::T3Gateway,
    mcp::{HttpOptions, IncomingMessage, McpServer, StdioCodec},
    operations::audit_log::{AuditEventInput, AuditSource, summarize_for_audit},
};
use serde_json::{Value, json};

type ServerBody = crate::mcp::HttpBody;

pub async fn run_stdio(handler: Arc<T3Gateway>) -> io::Result<()> {
    let correlation_id = format!("stdio_{}", uuid::Uuid::new_v4());
    let started_at = Instant::now();
    let mut event = AuditEventInput::new(AuditSource::Transport, "stdio.session");
    event.correlation_id = Some(correlation_id.clone());
    event.outcome = Some("started".into());
    event.details = Some(json!({"transport":"stdio"}));
    handler.audit_log().record(event);
    let result = stdio_messages(Arc::clone(&handler), &correlation_id).await;
    let mut event = AuditEventInput::new(AuditSource::Transport, "stdio.session");
    event.correlation_id = Some(correlation_id);
    event.outcome = Some("completed".into());
    event.duration_ms = Some(started_at.elapsed().as_secs_f64() * 1000.0);
    event.details = Some(json!({"transport":"stdio"}));
    handler.audit_log().record(event);
    result
}

async fn stdio_messages(handler: Arc<T3Gateway>, correlation_id: &str) -> io::Result<()> {
    let server = McpServer::gateway();
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let mut failure = None;
    let mut input = BufReader::new(tokio::io::stdin());
    let (delivery, mut output) = server.stdio_output();
    let writer_delivery = delivery.clone();
    let mut writer = tokio::spawn(async move {
        let mut stdout = match crate::mcp::FrameWriter::new() {
            Ok(writer) => writer,
            Err(error) => {
                writer_delivery.fail();
                return Err(error);
            }
        };
        while let Some(frame) = output.recv().await {
            let mut frame = frame?.to_vec();
            frame.push(b'\n');
            let result = tokio::select! {
                biased;
                _ = writer_delivery.failed() => Err(io::Error::new(io::ErrorKind::BrokenPipe, "MCP stdio output failed.")),
                result = stdout.write(&frame) => result,
            };
            if let Err(error) = result {
                writer_delivery.fail();
                return Err(error);
            }
        }
        Ok::<(), io::Error>(())
    });
    let mut codec = StdioCodec::default();
    let mut read_buffer = [0_u8; 8192];
    let mut requests = JoinSet::new();
    loop {
        let count = tokio::select! {
            _ = &mut shutdown => break,
            _ = delivery.failed() => { failure = Some(io::Error::new(io::ErrorKind::BrokenPipe, "MCP stdio output closed.")); break; },
            result = requests.join_next(), if !requests.is_empty() => {
                match result.expect("pending request") {
                    Ok(Ok(())) => {},
                    Ok(Err(error)) => { failure = Some(error); break; },
                    Err(error) => { failure = Some(io::Error::other(error)); break; },
                }
                continue;
            },
            count = input.read(&mut read_buffer) => match count {
                Ok(count) => count,
                Err(error) => { failure = Some(error); break; },
            },
        };
        if count == 0 {
            break;
        }
        if let Err(error) = codec.append(&read_buffer[..count]) {
            stdio_error(&handler, correlation_id, &error.to_string());
            continue;
        }
        loop {
            match codec.read_message() {
                Ok(Some(message)) => {
                    let mut event = AuditEventInput::new(AuditSource::Transport, "stdio.message");
                    event.correlation_id = Some(correlation_id.to_owned());
                    event.operation = Some("stdio".into());
                    event.outcome = Some("received".into());
                    event.details = Some(incoming_details(&message));
                    handler.audit_log().record(event);
                    let admitted = server.admit_message(message);
                    admitted.set_output(delivery.clone());
                    let server = server.clone();
                    let handler = Arc::clone(&handler);
                    let delivery = delivery.clone();
                    requests.spawn(async move {
                        if let Some(response) =
                            server.execute_message(admitted, handler.as_ref()).await
                        {
                            delivery.send(&response);
                        }
                        Ok::<(), io::Error>(())
                    });
                }
                Ok(None) => break,
                Err(error) => stdio_error(&handler, correlation_id, &error.to_string()),
            }
        }
    }
    server.close();
    let mut event = AuditEventInput::new(AuditSource::Transport, "stdio.session");
    event.correlation_id = Some(correlation_id.to_owned());
    event.outcome = Some("draining".into());
    event.details = Some(json!({"transport":"stdio"}));
    handler.audit_log().record(event);
    while let Some(result) = requests.join_next().await {
        let result = result.map_err(io::Error::other).and_then(|result| result);
        if let Err(error) = result {
            failure.get_or_insert(error);
        }
    }
    delivery.finish();
    match tokio::time::timeout(Duration::from_secs(5), &mut writer).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(error))) => {
            failure.get_or_insert(error);
        }
        Ok(Err(error)) => {
            failure.get_or_insert(io::Error::other(error));
        }
        Err(_) => {
            writer.abort();
            let _ = writer.await;
            delivery.fail();
            failure.get_or_insert(io::Error::new(
                io::ErrorKind::TimedOut,
                "MCP stdio writer did not drain.",
            ));
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn stdio_error(handler: &T3Gateway, correlation_id: &str, message: &str) {
    let mut event = AuditEventInput::new(AuditSource::Transport, "stdio.error");
    event.correlation_id = Some(correlation_id.to_owned());
    event.operation = Some("stdio".into());
    event.outcome = Some("error".into());
    event.details = Some(json!({"error":message}));
    handler.audit_log().record(event);
}

pub async fn run_http<S>(
    handler: Arc<T3Gateway>,
    options: HttpOptions,
    host: &str,
    port: u64,
    shutdown: S,
) -> io::Result<()>
where
    S: Future<Output = ()> + Send + 'static,
{
    let port = u16::try_from(port).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "MCP_PORT must be between 1 and 65535 to bind an HTTP listener.",
        )
    })?;
    let listener = TcpListener::bind((host, port)).await?;
    let mut event = AuditEventInput::new(AuditSource::System, "http.listen");
    event.operation = Some("http".into());
    event.outcome = Some("started".into());
    event.details = Some(json!({"host":host,"port":listener.local_addr()?.port()}));
    handler.audit_log().record(event);
    eprintln!("t3-code-mcp listening on http://{host}:{port}/mcp");
    let server = McpServer::gateway();
    let mut shutdown = Box::pin(shutdown);
    let mut connections = JoinSet::new();
    let mut maintenance = tokio::time::interval(Duration::from_secs(60));
    let (stop_connections, _) = tokio::sync::watch::channel(());

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = maintenance.tick() => server.expire_sessions(),
            accepted = listener.accept() => {
                let (stream, peer) = accepted?;
                let handler = Arc::clone(&handler);
                let server = server.clone();
                let options = options.clone();
                let mut stop = stop_connections.subscribe();
                connections.spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let handler = Arc::clone(&handler);
                        let server = server.clone();
                        let options = options.clone();
                        let (sender, receiver) = tokio::sync::oneshot::channel();
                        let worker_server = server.clone();
                        let task = tokio::spawn(async move {
                            let response = handle_request(request, &worker_server, &options, handler, peer).await;
                            let _ = sender.send(response);
                        });
                        server.retain_worker(task);
                        async move {
                            let response = receiver.await.unwrap_or_else(|_| full(
                                Response::builder().status(500).body(Bytes::new()).expect("static response")
                            ));
                            Ok::<_, Infallible>(response)
                        }
                    });
                    let builder = http1::Builder::new();
                    let connection = builder.serve_connection(TokioIo::new(stream), service);
                    tokio::pin!(connection);
                    let result = tokio::select! {
                        result = &mut connection => result,
                        _ = stop.changed() => {
                            connection.as_mut().graceful_shutdown();
                            connection.await
                        }
                    };
                    if let Err(error) = result {
                        eprintln!("HTTP connection error: {error}");
                    }
                });
            }
            result = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = result {
                    eprintln!("HTTP connection task failed: {error}");
                }
            }
        }
    }

    drop(listener);
    server.close();
    let _ = stop_connections.send(());
    while connections.join_next().await.is_some() {}
    server.drain().await;
    Ok(())
}

async fn handle_request(
    request: Request<Incoming>,
    server: &McpServer,
    options: &HttpOptions,
    handler: Arc<T3Gateway>,
    peer: std::net::SocketAddr,
) -> Response<ServerBody> {
    let started_at = Instant::now();
    let correlation_id = format!("http_{}", uuid::Uuid::new_v4());
    let method = request.method().to_string();
    let path = request.uri().path().to_owned();
    let mut event = AuditEventInput::new(AuditSource::Transport, "http.request");
    event.correlation_id = Some(correlation_id.clone());
    event.operation = Some(path.clone());
    event.outcome = Some("started".into());
    event.details = Some(json!({
        "method":method,"path":path,
        "contentLength":request.headers().get("content-length").and_then(|value|value.to_str().ok()),
        "remoteAddress":peer.ip().to_string(),
        "userAgent":request.headers().get("user-agent").and_then(|value|value.to_str().ok()),
    }));
    handler.audit_log().record(event);
    let parsed_path = url::Url::parse("http://localhost")
        .expect("static URL")
        .join(&request.uri().to_string())
        .ok();
    let is_mcp = parsed_path.as_ref().is_some_and(|url| url.path() == "/mcp");
    let authorized = crate::mcp::authorized(request.headers(), options.bearer_token.as_deref());
    let auth_outcome = if !is_mcp {
        "not_checked"
    } else if authorized {
        "accepted"
    } else if options.bearer_token.is_none() {
        "not_configured"
    } else {
        "rejected"
    };
    let (parts, body) = request.into_parts();
    let metadata = Request::from_parts(parts, ());
    let should_read = server.should_read_body(&metadata, options);
    let (parts, ()) = metadata.into_parts();
    let body = if should_read {
        read_body(body, options.max_body_bytes).await
    } else {
        Ok(Bytes::new())
    };
    let mut rpc = Value::Null;
    let response = match body {
        Ok(body) => {
            if is_mcp && authorized {
                if method != "POST" {
                    rpc = rpc_details(None);
                } else if let Ok(value) = serde_json::from_slice::<Value>(&body) {
                    rpc = rpc_details(Some(&value));
                }
            }
            server
                .handle_http_stream(Request::from_parts(parts, body), options, handler.clone())
                .await
        }
        Err(()) => full(crate::mcp::body_too_large()),
    };
    let mut event = AuditEventInput::new(AuditSource::Transport, "http.response");
    event.correlation_id = Some(correlation_id);
    event.operation = Some(path.clone());
    event.outcome = Some(
        if response.status().as_u16() >= 400 {
            "error"
        } else {
            "completed"
        }
        .into(),
    );
    event.duration_ms = Some(started_at.elapsed().as_secs_f64() * 1000.0);
    event.details = Some(
        json!({"method":method,"path":path,"status":response.status().as_u16(),"authOutcome":auth_outcome,"rpc":rpc}),
    );
    handler.audit_log().record(event);
    response
}

fn incoming_details(message: &IncomingMessage) -> Value {
    match message {
        IncomingMessage::Request(request) => rpc_details(Some(
            &json!({"method":request.method,"id":request.id,"params":request.params}),
        )),
        IncomingMessage::Notification { method, params } => {
            rpc_details(Some(&json!({"method":method,"params":params})))
        }
        IncomingMessage::Response(value) => rpc_details(Some(value)),
    }
}

fn rpc_details(body: Option<&Value>) -> Value {
    let Some(body) = body else {
        return json!({"batch":false,"validRequestShape":false});
    };
    if let Some(batch) = body.as_array() {
        return json!({"batch":true,"batchLength":batch.len()});
    }
    if !body.is_object() {
        return json!({"batch":false,"validRequestShape":false});
    }
    let params = &body["params"];
    json!({
        "batch":false,"jsonRpcMethod":body["method"].as_str(),
        "requestId":body.get("id").filter(|value|value.is_string() || value.is_number()),
        "toolName":params["name"].as_str(),
        "arguments":params.get("arguments").map(summarize_for_audit),
        "clientInfo":params.get("clientInfo").filter(|value|value.is_object() || value.is_array()).map(summarize_for_audit),
    })
}

async fn read_body(body: Incoming, max_bytes: usize) -> Result<Bytes, ()> {
    let limit = max_bytes.saturating_add(1);
    let body = Limited::new(body, limit);
    let bytes = body.collect().await.map_err(|_| ())?.to_bytes();
    if bytes.len() > max_bytes {
        return Err(());
    }
    Ok(bytes)
}

fn full(response: Response<Bytes>) -> Response<ServerBody> {
    let (parts, body) = response.into_parts();
    Response::from_parts(
        parts,
        Full::new(body)
            .map_err(|never| match never {})
            .boxed_unsync(),
    )
}

#[cfg(unix)]
pub async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}

#[cfg(not(unix))]
pub async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
