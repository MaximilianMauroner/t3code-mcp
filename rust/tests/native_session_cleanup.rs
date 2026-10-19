#![cfg(unix)]

use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{self, BufRead, Write},
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    time::Duration,
};
use t3_code_mcp::native_credential::{NativeAccessLevel, NativeCredential, NativeCredentialStore};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task::{JoinHandle, JoinSet},
    time::timeout,
};

const WAIT: Duration = Duration::from_secs(5);

#[derive(Debug)]
enum StreamEvent {
    Opened(String),
    Closed(String),
    Failed(String),
}

struct HeldNative {
    url: String,
    events: mpsc::UnboundedReceiver<StreamEvent>,
    task: JoinHandle<()>,
}

impl HeldNative {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (events, receiver) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut clients = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (socket, _) = accepted.unwrap();
                        clients.spawn(serve_native(socket, events.clone()));
                    }
                    result = clients.join_next(), if !clients.is_empty() => {
                        match result.unwrap() {
                            Ok(Ok(())) => {},
                            failure => {
                                let _ = events.send(StreamEvent::Failed(format!("{failure:?}")));
                            }
                        }
                    }
                }
            }
        });
        Self {
            url,
            events: receiver,
            task,
        }
    }

    async fn event(&mut self) -> StreamEvent {
        let event = timeout(WAIT, self.events.recv())
            .await
            .expect("native GET did not reach the expected socket state")
            .expect("native fixture stopped");
        if let StreamEvent::Failed(error) = &event {
            panic!("native fixture failed: {error}");
        }
        event
    }

    async fn opened(&mut self) -> String {
        match self.event().await {
            StreamEvent::Opened(session) => session,
            other => panic!("expected an open native GET, got {other:?}"),
        }
    }

    async fn closed(&mut self, expected: &str) {
        match self.event().await {
            StreamEvent::Closed(session) => assert_eq!(session, expected),
            other => panic!("expected remote close of native GET, got {other:?}"),
        }
    }

    fn assert_held(&mut self) {
        assert!(
            matches!(
                self.events.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ),
            "native GET closed before the lifecycle action"
        );
    }
}

impl Drop for HeldNative {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct WireRequest {
    method: String,
    session: Option<String>,
    body: Value,
}

async fn read_request(socket: &mut TcpStream) -> io::Result<WireRequest> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let head_end = loop {
        let count = socket.read(&mut buffer).await?;
        assert_ne!(count, 0, "native client closed before request headers");
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break end + 4;
        }
        assert!(
            bytes.len() < 64 * 1024,
            "native request headers exceeded fixture limit"
        );
    };
    let head = String::from_utf8(bytes[..head_end].to_vec()).unwrap();
    let mut lines = head.lines();
    let mut first = lines.next().unwrap().split_whitespace();
    let method = first.next().unwrap().to_owned();
    assert_eq!(first.next(), Some("/mcp"));
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect::<HashMap<_, _>>();
    assert_eq!(
        headers.get("authorization").map(String::as_str),
        Some("Bearer fixture-native")
    );
    let length = headers
        .get("content-length")
        .map_or(0, |value| value.parse::<usize>().unwrap());
    assert!(
        length < 64 * 1024,
        "native request body exceeded fixture limit"
    );
    while bytes.len() < head_end + length {
        let count = socket.read(&mut buffer).await?;
        assert_ne!(count, 0, "native client closed before request body");
        bytes.extend_from_slice(&buffer[..count]);
    }
    let body = if length == 0 {
        Value::Null
    } else {
        serde_json::from_slice(&bytes[head_end..head_end + length]).unwrap()
    };
    Ok(WireRequest {
        method,
        session: headers.get("mcp-session-id").cloned(),
        body,
    })
}

async fn serve_native(
    mut socket: TcpStream,
    events: mpsc::UnboundedSender<StreamEvent>,
) -> io::Result<()> {
    let request = timeout(WAIT, read_request(&mut socket))
        .await
        .expect("native client did not finish its request")?;
    if request.method == "GET" {
        let session = request.session.expect("native GET omitted its session");
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n: held\n\n").await?;
        socket.flush().await?;
        let _ = events.send(StreamEvent::Opened(session.clone()));
        let mut byte = [0];
        match socket.read(&mut byte).await {
            Ok(0) => {}
            Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
            other => panic!("expected remote close on held native GET, got {other:?}"),
        }
        let _ = events.send(StreamEvent::Closed(session));
        return Ok(());
    }
    assert_eq!(request.method, "POST");
    let (status, extra_headers, result) = match request.body["method"].as_str() {
        Some("initialize") => (
            200,
            format!("Mcp-Session-Id: {}\r\n", uuid::Uuid::new_v4()),
            Some(json!({
                "protocolVersion":"2025-06-18", "capabilities":{},
                "serverInfo":{"name":"held-native-get","version":"1"}
            })),
        ),
        Some("notifications/initialized") => (202, String::new(), None),
        Some("tools/list") => {
            assert!(request.session.is_some());
            (
                200,
                String::new(),
                Some(json!({"tools":[{
                    "name":"t3_thread_read", "inputSchema":{"type":"object"}
                }]})),
            )
        }
        other => panic!("unexpected native method {other:?}"),
    };
    let body = result.map_or_else(String::new, |result| {
        json!({"jsonrpc":"2.0","id":request.body["id"],"result":result}).to_string()
    });
    let head = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
        body.len()
    );
    socket.write_all(head.as_bytes()).await?;
    socket.write_all(body.as_bytes()).await?;
    socket.shutdown().await
}

struct Fixture {
    native: HeldNative,
    directory: PathBuf,
}

impl Fixture {
    async fn new() -> Self {
        let native = HeldNative::start().await;
        let directory =
            std::env::temp_dir().join(format!("t3-native-cleanup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        NativeCredentialStore::in_data_dir(&directory)
            .write(&NativeCredential {
                resource: format!("{}/mcp", native.url),
                client_id: "fixture".into(),
                access_token: "fixture-native".into(),
                access: NativeAccessLevel::FullAccess,
                scopes: vec!["orchestration:operate".into()],
                obtained_at: "2026-10-18T00:00:00.000Z".into(),
                expires_at: "2099-10-18T00:00:00.000Z".into(),
            })
            .unwrap();
        Self { native, directory }
    }

    fn audit(&self) -> Vec<Value> {
        std::fs::read_to_string(self.directory.join("audit.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

struct Process(Option<Child>);

impl Process {
    fn spawn(fixture: &Fixture, transport: &str) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_t3-code-mcp"));
        command.env_clear();
        for name in ["PATH", "SystemRoot", "WINDIR", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .current_dir(&fixture.directory)
            .arg("serve")
            .env("T3_HTTP_BASE_URL", &fixture.native.url)
            .env("T3_NATIVE_MCP_URL", format!("{}/mcp", fixture.native.url))
            .env("T3_ACCESS_TOKEN", "fixture-internal")
            .env("T3_MCP_DATA_DIR", &fixture.directory)
            .env("MCP_BEARER_TOKEN", "gateway-secret")
            .env("MCP_READ_ONLY", "true")
            .env("MCP_HOST", "127.0.0.1")
            .env("MCP_TRANSPORT", transport)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if transport == "http" {
            let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            command.env(
                "MCP_PORT",
                reservation.local_addr().unwrap().port().to_string(),
            );
            drop(reservation);
        }
        Self(Some(command.spawn().unwrap()))
    }

    async fn http_url(&mut self, fixture: &Fixture) -> String {
        timeout(WAIT, async {
            loop {
                assert!(
                    self.0.as_mut().unwrap().try_wait().unwrap().is_none(),
                    "HTTP binary exited before listening"
                );
                if let Some(port) = fixture
                    .audit()
                    .iter()
                    .find(|event| event["event"] == "http.listen" && event["outcome"] == "started")
                    .and_then(|event| event["details"]["port"].as_u64())
                {
                    break format!("http://127.0.0.1:{port}/mcp");
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("HTTP binary did not report its listener")
    }

    fn send_stdio(&mut self, body: Value) {
        writeln!(self.0.as_mut().unwrap().stdin.as_mut().unwrap(), "{body}").unwrap();
    }

    async fn stdio_response(&mut self) -> (Value, io::BufReader<std::process::ChildStdout>) {
        let stdout = self.0.as_mut().unwrap().stdout.take().unwrap();
        let (line, reader) = timeout(
            WAIT,
            tokio::task::spawn_blocking(move || {
                let mut reader = io::BufReader::new(stdout);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                (line, reader)
            }),
        )
        .await
        .expect("stdio binary did not return its catalog")
        .unwrap();
        (serde_json::from_str(&line).unwrap(), reader)
    }

    fn signal(&self) {
        assert_eq!(
            unsafe { libc::kill(self.0.as_ref().unwrap().id() as i32, libc::SIGTERM) },
            0
        );
    }

    fn eof(&mut self) {
        drop(self.0.as_mut().unwrap().stdin.take());
    }

    async fn finish(mut self) {
        timeout(WAIT, async {
            while self.0.as_mut().unwrap().try_wait().unwrap().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("binary did not exit after shutdown");
        let output: Output = self.0.take().unwrap().wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

async fn post(
    client: &reqwest::Client,
    url: &str,
    session: Option<&str>,
    body: Value,
) -> reqwest::Response {
    let mut request = client
        .post(url)
        .bearer_auth("gateway-secret")
        .header("accept", "application/json, text/event-stream")
        .json(&body);
    if let Some(session) = session {
        request = request.header("mcp-session-id", session);
    }
    request.send().await.unwrap()
}

fn assert_catalog(response: &Value) {
    assert!(
        response["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "t3_thread_read"),
        "native catalog was not forwarded: {response}"
    );
}

async fn open_session(client: &reqwest::Client, url: &str) -> String {
    let response = post(client, url, None, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"cleanup","version":"1"}
    }})).await;
    assert_eq!(response.status(), 200);
    let session = response.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        response
            .json::<Value>()
            .await
            .unwrap()
            .get("result")
            .is_some()
    );
    let response = post(
        client,
        url,
        Some(&session),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_catalog(&response.json::<Value>().await.unwrap());
    session
}

#[tokio::test]
async fn http_delete_and_shutdown_close_held_native_get_streams() {
    let mut fixture = Fixture::new().await;
    let mut process = Process::spawn(&fixture, "http");
    let url = process.http_url(&fixture).await;
    let client = reqwest::Client::builder().timeout(WAIT).build().unwrap();
    let session = open_session(&client, &url).await;
    let native_session = fixture.native.opened().await;
    fixture.native.assert_held();

    let deleted = client
        .delete(&url)
        .bearer_auth("gateway-secret")
        .header("mcp-session-id", &session)
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 200);
    drop(deleted);
    fixture.native.closed(&native_session).await;
    let ping = json!({"jsonrpc":"2.0","id":3,"method":"ping"});
    assert_eq!(
        post(&client, &url, Some(&session), ping.clone())
            .await
            .status(),
        404
    );
    assert_eq!(post(&client, &url, None, ping).await.status(), 200);

    open_session(&client, &url).await;
    let next_native_session = fixture.native.opened().await;
    assert_ne!(next_native_session, native_session);
    fixture.native.assert_held();
    process.signal();
    process.finish().await;
    fixture.native.closed(&next_native_session).await;
}

#[tokio::test]
async fn stdio_eof_and_signal_close_held_native_get_streams() {
    for signal in [false, true] {
        let mut fixture = Fixture::new().await;
        let mut process = Process::spawn(&fixture, "stdio");
        process.send_stdio(json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}));
        let (response, stdout) = process.stdio_response().await;
        assert_catalog(&response);
        let native_session = fixture.native.opened().await;
        fixture.native.assert_held();
        if signal {
            process.signal();
        } else {
            process.eof();
        }
        process.finish().await;
        fixture.native.closed(&native_session).await;
        drop(stdout);
    }
}
