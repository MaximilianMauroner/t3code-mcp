use futures_util::future::BoxFuture;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: std::collections::HashMap<String, String>,
    pub body: Value,
    pub body_bytes: Vec<u8>,
}
#[derive(Clone)]
pub struct Reply {
    pub status: u16,
    pub body: Value,
    pub headers: Vec<(String, String)>,
    pub drop_response: bool,
    pub stream: Option<Vec<(Vec<u8>, std::time::Duration)>>,
}
impl Reply {
    pub fn json(body: Value) -> Self {
        Self {
            status: 200,
            body,
            headers: vec![],
            drop_response: false,
            stream: None,
        }
    }
    pub fn sse(chunks: Vec<(Vec<u8>, std::time::Duration)>) -> Self {
        Self {
            stream: Some(chunks),
            ..Self::json(Value::Null)
        }
    }
    pub fn status(status: u16, body: Value) -> Self {
        Self {
            status,
            ..Self::json(body)
        }
    }
}
pub struct Server {
    pub url: String,
    pub requests: Arc<Mutex<Vec<Request>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    pub async fn start(
        handler: impl Fn(Request) -> BoxFuture<'static, Reply> + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handler = Arc::new(handler);
        let requests = Arc::new(Mutex::new(vec![]));
        let recorded = requests.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let handler = handler.clone();
                let requests = recorded.clone();
                tokio::spawn(async move {
                    let mut bytes = Vec::new();
                    let mut buffer = [0; 8192];
                    let (head_end, length) = loop {
                        let n = socket.read(&mut buffer).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buffer[..n]);
                        if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&bytes[..end]);
                            let length = head
                                .lines()
                                .find_map(|l| {
                                    l.to_lowercase()
                                        .strip_prefix("content-length:")
                                        .and_then(|v| v.trim().parse::<usize>().ok())
                                })
                                .unwrap_or(0);
                            break (end + 4, length);
                        }
                    };
                    while bytes.len() < head_end + length {
                        let n = socket.read(&mut buffer).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buffer[..n]);
                    }
                    let head = String::from_utf8_lossy(&bytes[..head_end]);
                    let mut lines = head.lines();
                    let first = lines.next().unwrap().split_whitespace().collect::<Vec<_>>();
                    let request = Request {
                        method: first[0].into(),
                        path: first[1].into(),
                        headers: lines
                            .filter_map(|l| {
                                l.split_once(':')
                                    .map(|(k, v)| (k.to_lowercase(), v.trim().into()))
                            })
                            .collect(),
                        body: serde_json::from_slice(&bytes[head_end..head_end + length])
                            .unwrap_or(Value::Null),
                        body_bytes: bytes[head_end..head_end + length].to_vec(),
                    };
                    if request
                        .headers
                        .get("upgrade")
                        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
                    {
                        use futures_util::{SinkExt, StreamExt};
                        use tokio_tungstenite::tungstenite::{
                            Message, handshake::derive_accept_key, protocol::Role,
                        };
                        let key =
                            derive_accept_key(request.headers["sec-websocket-key"].as_bytes());
                        let upgrade = format!(
                            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {key}\r\n\r\n"
                        );
                        socket.write_all(upgrade.as_bytes()).await.unwrap();
                        let mut ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
                            socket,
                            Role::Server,
                            None,
                        )
                        .await;
                        let mut request = request;
                        request.body =
                            serde_json::from_slice(&ws.next().await.unwrap().unwrap().into_data())
                                .unwrap();
                        requests.lock().unwrap().push(request.clone());
                        let id = request.body["id"].clone();
                        let reply = handler(request).await;
                        if reply.drop_response {
                            return;
                        }
                        let body = if reply.body.get("_tag").is_some() || reply.body.is_array() {
                            reply.body
                        } else {
                            serde_json::json!({"_tag":"Exit","requestId":id,"exit":{"_tag":"Success","value":reply.body}})
                        };
                        let _ = ws.send(Message::Text(body.to_string().into())).await;
                        return;
                    }
                    requests.lock().unwrap().push(request.clone());
                    let reply = handler(request).await;
                    if reply.drop_response {
                        return;
                    }
                    if let Some(chunks) = reply.stream {
                        let length: usize = chunks.iter().map(|(chunk, _)| chunk.len()).sum();
                        let mut headers = format!(
                            "HTTP/1.1 {} Reply\r\ncontent-type: text/event-stream\r\ncontent-length: {length}\r\nconnection: close\r\n",
                            reply.status
                        );
                        for (key, value) in reply.headers {
                            headers.push_str(&format!("{key}: {value}\r\n"));
                        }
                        headers.push_str("\r\n");
                        if socket.write_all(headers.as_bytes()).await.is_err() {
                            return;
                        }
                        for (chunk, delay) in chunks {
                            tokio::time::sleep(delay).await;
                            if socket.write_all(&chunk).await.is_err() {
                                return;
                            }
                        }
                        return;
                    }
                    let body = if reply.status == 202 || reply.status == 204 {
                        vec![]
                    } else {
                        serde_json::to_vec(&reply.body).unwrap()
                    };
                    let mut headers = format!(
                        "HTTP/1.1 {} Reply\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
                        reply.status,
                        body.len()
                    );
                    for (key, value) in reply.headers {
                        headers.push_str(&format!("{key}: {value}\r\n"));
                    }
                    headers.push_str("\r\n");
                    let _ = socket.write_all(headers.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                });
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
}
