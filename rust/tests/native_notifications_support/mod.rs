use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{BufRead, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_code_mcp::{
    config::load_config_from,
    gateway::T3Gateway,
    native_credential::{NativeAccessLevel, NativeCredential, NativeCredentialStore},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Notify, mpsc, oneshot},
    task::{JoinHandle, JoinSet},
    time::timeout,
};

pub const WAIT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub session: String,
    pub authorization: String,
    pub body: Value,
}

pub struct Call {
    pub request: Request,
    pub events: mpsc::UnboundedSender<Value>,
}

#[derive(Default)]
pub struct PageGate {
    pub entered: Notify,
    pub release: Notify,
}

pub struct Native {
    pub url: String,
    pub calls: mpsc::UnboundedReceiver<Call>,
    pub opened: mpsc::UnboundedReceiver<String>,
    pub closed: mpsc::UnboundedReceiver<String>,
    pub catalog_requests: mpsc::UnboundedReceiver<Request>,
    pub listeners: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<Value>>>>,
    pub revision: Arc<Mutex<usize>>,
    pub fail_page: Arc<Mutex<bool>>,
    pub page_gate: Arc<Mutex<Option<Arc<PageGate>>>>,
    pub catalog_by_token: Arc<Mutex<HashMap<String, (usize, bool)>>>,
    task: JoinHandle<()>,
}

impl Native {
    pub async fn start() -> Self {
        let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", socket.local_addr().unwrap());
        let (calls, call_rx) = mpsc::unbounded_channel();
        let (opened, open_rx) = mpsc::unbounded_channel();
        let (closed, close_rx) = mpsc::unbounded_channel();
        let (catalog_requests, catalog_rx) = mpsc::unbounded_channel();
        let listeners = Arc::new(Mutex::new(HashMap::new()));
        let revision = Arc::new(Mutex::new(0));
        let fail_page = Arc::new(Mutex::new(false));
        let page_gate = Arc::new(Mutex::new(None::<Arc<PageGate>>));
        let catalog_by_token = Arc::new(Mutex::new(HashMap::<String, (usize, bool)>::new()));
        let state = (
            listeners.clone(),
            revision.clone(),
            fail_page.clone(),
            page_gate.clone(),
            catalog_by_token.clone(),
        );
        let task = tokio::spawn(async move {
            let mut clients = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = socket.accept() => {
                        let (mut stream, _) = accepted.unwrap();
                        let (listeners, revision, fail_page, page_gate, catalog_by_token) = state.clone();
                        let (calls, opened, closed, catalog_requests) = (calls.clone(), opened.clone(), closed.clone(), catalog_requests.clone());
                        clients.spawn(async move {
                            let request = read_request(&mut stream).await;
                            if request.method == "GET" {
                                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n: native\n\n").await.unwrap();
                                let (events, mut receiver) = mpsc::unbounded_channel();
                                listeners.lock().unwrap().insert(request.session.clone(), events);
                                let _ = opened.send(request.session.clone());
                                let mut byte = [0];
                                loop {
                                    tokio::select! {
                                        _ = stream.read(&mut byte) => break,
                                        event = receiver.recv() => match event {
                                            Some(event) => if stream.write_all(&sse(&event)).await.is_err() { break; },
                                            None => break,
                                        }
                                    }
                                }
                                listeners.lock().unwrap().remove(&request.session);
                                let _ = closed.send(request.session);
                                return;
                            }
                            let id = request.body["id"].clone();
                            match request.body["method"].as_str().unwrap_or_default() {
                                "initialize" => {
                                    let session = uuid::Uuid::new_v4().to_string();
                                    reply(&mut stream, 200, Some(&session), json!({"jsonrpc":"2.0","id":id,"result":{
                                        "protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":true}},"serverInfo":{"name":"notifications","version":"1"}
                                    }})).await;
                                }
                                "tools/list" => {
                                    let (version, failed) = catalog_by_token.lock().unwrap().get(&request.authorization).copied()
                                        .unwrap_or_else(|| (*revision.lock().unwrap(), *fail_page.lock().unwrap()));
                                    let second = request.body["params"]["cursor"].is_string();
                                    let gate = if second { page_gate.lock().unwrap().take() } else { None };
                                    let _ = catalog_requests.send(request.clone());
                                    if let Some(gate) = gate { gate.entered.notify_one(); gate.release.notified().await; }
                                    if second && failed {
                                        reply(&mut stream, 200, None, json!({"jsonrpc":"2.0","id":id,"result":{"tools":false}})).await;
                                    } else {
                                        let name = if second { "t3_thread_send" } else { "t3_thread_read" };
                                        let mut tools = vec![tool(name, version)];
                                        if second && version > 0 { tools.push(tool("t3_project_read", version)); }
                                        let mut result = json!({"tools":tools});
                                        if !second { result["nextCursor"] = json!(format!("page-{version}")); }
                                        reply(&mut stream, 200, None, json!({"jsonrpc":"2.0","id":id,"result":result})).await;
                                    }
                                }
                                "tools/call" => {
                                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").await.unwrap();
                                    let (events, mut receiver) = mpsc::unbounded_channel();
                                    let _ = calls.send(Call { request, events });
                                    while let Some(event) = receiver.recv().await {
                                        let terminal = event.get("result").is_some() || event.get("error").is_some();
                                        if stream.write_all(&sse(&event)).await.is_err() { break; }
                                        if terminal { break; }
                                    }
                                }
                                _ => reply(&mut stream, 202, None, Value::Null).await,
                            }
                        });
                    }
                    result = clients.join_next(), if !clients.is_empty() => { result.unwrap().unwrap(); }
                }
            }
        });
        Self {
            url,
            calls: call_rx,
            opened: open_rx,
            closed: close_rx,
            catalog_requests: catalog_rx,
            listeners,
            revision,
            fail_page,
            page_gate,
            catalog_by_token,
            task,
        }
    }

    pub fn notify(&self, session: &str, params: Value) {
        self.listeners.lock().unwrap()[session].send(json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed","params":params})).unwrap();
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub fn tool(name: &str, version: usize) -> Value {
    json!({"name":name,"description":format!("revision-{version}"),"inputSchema":{"type":"object"},
        "outputSchema":{"type":"object","properties":{"content":{"type":"array"}}},
        "annotations":{"readOnlyHint":name=="t3_thread_read"},"_meta":{"opaque":{"version":version}}})
}

pub fn progress(token: Value, count: usize) -> Value {
    json!({"jsonrpc":"2.0","method":"notifications/progress","params":{
        "progressToken":token,"progress":count,"total":999,"message":"native progress","_meta":{"opaque":[false,{"keep":"all"}]},"extension":{"count":count}
    }})
}

pub fn result(call: &Call) -> Value {
    json!({"jsonrpc":"2.0","id":call.request.body["id"],"result":{
        "content":[],"structuredContent":{"params":call.request.body["params"]},"_meta":{"opaque":{"result":true}},"extension":[1,null,false]
    }})
}

pub fn call(id: Value, token: Value, mutating: bool) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{
        "name":if mutating {"t3_thread_send"} else {"t3_thread_read"},"arguments":{"threadId":"fixture","extension":[null,false]},
        "_meta":{"progressToken":token,"opaque":{"request":[1,false,null]},"io.modelcontextprotocol/related-task":{"taskId":"opaque-task","extension":[false,null]}},"extension":{"raw":true}
    }})
}

fn sse(value: &Value) -> Vec<u8> {
    format!("event: message\ndata: {value}\n\n").into_bytes()
}

async fn read_request(stream: &mut TcpStream) -> Request {
    let mut data = Vec::new();
    let mut buffer = [0; 4096];
    let end = loop {
        let n = stream.read(&mut buffer).await.unwrap();
        assert_ne!(n, 0);
        data.extend_from_slice(&buffer[..n]);
        if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
        assert!(data.len() < 65536);
    };
    let head = String::from_utf8(data[..end].to_vec()).unwrap();
    let headers = head
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
        .collect::<HashMap<_, _>>();
    let authorization = headers.get("authorization").cloned().unwrap();
    assert!(matches!(
        authorization.as_str(),
        "Bearer fixture-native" | "Bearer fixture-native-rotated"
    ));
    let length = headers
        .get("content-length")
        .map_or(0, |v| v.parse::<usize>().unwrap());
    assert!(length < 65536);
    while data.len() < end + length {
        let n = stream.read(&mut buffer).await.unwrap();
        assert_ne!(n, 0);
        data.extend_from_slice(&buffer[..n]);
    }
    Request {
        method: head.split_whitespace().next().unwrap().into(),
        authorization,
        session: headers.get("mcp-session-id").cloned().unwrap_or_default(),
        body: serde_json::from_slice(&data[end..end + length]).unwrap_or(Value::Null),
    }
}

async fn reply(stream: &mut TcpStream, status: usize, session: Option<&str>, body: Value) {
    let body = if status == 202 {
        String::new()
    } else {
        body.to_string()
    };
    let session = session.map_or(String::new(), |s| format!("Mcp-Session-Id: {s}\r\n"));
    let head = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{session}\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
}

pub struct Fixture {
    pub native: Native,
    pub directory: PathBuf,
}
impl Fixture {
    pub async fn new() -> Self {
        let native = Native::start().await;
        let directory =
            std::env::temp_dir().join(format!("t3-notifications-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let fixture = Self { native, directory };
        fixture.write_credential("fixture-native");
        fixture
    }
    pub fn write_credential(&self, token: &str) {
        NativeCredentialStore::in_data_dir(&self.directory)
            .write(&NativeCredential {
                resource: format!("{}/mcp", self.native.url),
                client_id: "fixture".into(),
                access_token: token.into(),
                access: NativeAccessLevel::FullAccess,
                scopes: vec!["orchestration:operate".into()],
                obtained_at: "2026-10-18T00:00:00.000Z".into(),
                expires_at: "2099-10-18T00:00:00.000Z".into(),
            })
            .unwrap();
    }
    pub fn gateway(&self, read_only: bool) -> Arc<T3Gateway> {
        let environment = HashMap::from([
            ("T3_HTTP_BASE_URL".into(), self.native.url.clone()),
            ("T3_ACCESS_TOKEN".into(), "internal".into()),
            (
                "T3_MCP_DATA_DIR".into(),
                self.directory.to_string_lossy().into_owned(),
            ),
            ("MCP_READ_ONLY".into(), read_only.to_string()),
        ]);
        Arc::new(T3Gateway::new(
            load_config_from(&environment, &self.directory).unwrap(),
        ))
    }
    pub fn audit(&self) -> Vec<Value> {
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

pub async fn receive<T>(receiver: &mut mpsc::UnboundedReceiver<T>) -> T {
    timeout(WAIT, receiver.recv())
        .await
        .expect("fixture event timed out")
        .expect("fixture stopped")
}

pub struct Process {
    child: Child,
    pub lines: mpsc::UnboundedReceiver<Value>,
    output_closed: Option<oneshot::Receiver<()>>,
}
impl Process {
    pub fn command(fixture: &Fixture, transport: &str) -> Command {
        let reservation =
            (transport == "http").then(|| std::net::TcpListener::bind("127.0.0.1:0").unwrap());
        let port = reservation
            .as_ref()
            .map_or(1, |socket| socket.local_addr().unwrap().port());
        let mut command = Command::new(env!("CARGO_BIN_EXE_t3-code-mcp"));
        command
            .env_clear()
            .current_dir(&fixture.directory)
            .arg("serve")
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("T3_HTTP_BASE_URL", &fixture.native.url)
            .env("T3_ACCESS_TOKEN", "internal")
            .env("T3_MCP_DATA_DIR", &fixture.directory)
            .env("MCP_TRANSPORT", transport)
            .env("MCP_READ_ONLY", "false")
            .env("MCP_BEARER_TOKEN", "gateway-secret")
            .env("MCP_HOST", "127.0.0.1")
            .env("MCP_PORT", port.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command
    }
    pub fn spawn(fixture: &Fixture, transport: &str) -> Self {
        let mut child = Self::command(fixture, transport).spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = mpsc::unbounded_channel();
        let (closed, output_closed) = oneshot::channel();
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            for line in (&mut reader).lines() {
                let Ok(line) = line else {
                    break;
                };
                let value =
                    serde_json::from_str(&line).expect("stdio frames must be whole JSON lines");
                if sender.send(value).is_err() {
                    break;
                }
            }
            drop(reader);
            let _ = closed.send(());
        });
        Self {
            child,
            lines,
            output_closed: Some(output_closed),
        }
    }
    pub fn send(&mut self, value: Value) {
        writeln!(self.child.stdin.as_mut().unwrap(), "{value}").unwrap();
    }
    pub fn eof(&mut self) {
        self.child.stdin.take();
    }
    #[cfg(unix)]
    pub fn signal(&self) {
        assert_eq!(
            unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) },
            0
        );
    }
    pub fn disconnect_output(&mut self) {
        self.lines.close();
    }
    pub async fn output_closed(&mut self) {
        timeout(
            WAIT,
            self.output_closed
                .take()
                .expect("reader acknowledgement already consumed"),
        )
        .await
        .expect("stdout reader did not close its pipe")
        .expect("stdout reader panicked");
    }
    pub async fn url(&mut self, fixture: &Fixture) -> String {
        timeout(WAIT, async {
            loop {
                assert!(self.child.try_wait().unwrap().is_none());
                if let Some(port) = fixture
                    .audit()
                    .iter()
                    .find(|e| e["event"] == "http.listen")
                    .and_then(|e| e["details"]["port"].as_u64())
                {
                    return format!("http://127.0.0.1:{port}/mcp");
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }
    pub async fn finish(&mut self, success: bool) {
        let status = timeout(WAIT, async {
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    break status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("binary did not drain");
        assert_eq!(status.success(), success);
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn initialize() -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"notification-test","version":"1"}
    }})
}
