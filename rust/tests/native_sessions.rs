#[path = "../src/t3/test_server.rs"]
pub mod test_server;

use bytes::Bytes;
use futures_util::FutureExt;
use http::{Request, Response};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use t3_code_mcp::{
    config::load_config_from,
    gateway::T3Gateway,
    mcp::{HttpOptions, McpServer},
    native_credential::{NativeAccessLevel, NativeCredential, NativeCredentialStore},
};
use test_server::{Reply, Server};
use tokio::sync::{Mutex, Notify, mpsc};

#[derive(Default)]
struct Gate {
    release: Notify,
}

struct Fixture {
    native: Server,
    gateway: Arc<T3Gateway>,
    server: McpServer,
    directory: PathBuf,
    events: Mutex<mpsc::UnboundedReceiver<test_server::Request>>,
    gates: Arc<Vec<Gate>>,
    block_initialize: Arc<AtomicBool>,
    initialize_release: Arc<Notify>,
}

impl Fixture {
    async fn new() -> Self {
        let (events, receiver) = mpsc::unbounded_channel();
        let gates = Arc::new((0..8).map(|_| Gate::default()).collect::<Vec<_>>());
        let block_initialize = Arc::new(AtomicBool::new(false));
        let initialize_release = Arc::new(Notify::new());
        let held_gates = gates.clone();
        let blocked = block_initialize.clone();
        let release_initialize = initialize_release.clone();
        let native = Server::start(move |request| {
            let events = events.clone();
            let gates = held_gates.clone();
            let blocked = blocked.clone();
            let release_initialize = release_initialize.clone();
            async move {
                let method = request.body["method"].as_str().unwrap_or_default();
                if request.method == "GET" { return Reply::status(405, Value::Null) }
                if method == "initialize" {
                    if blocked.swap(false, Ordering::SeqCst) {
                        events.send(request.clone()).unwrap();
                        release_initialize.notified().await;
                    }
                    let mut reply = Reply::json(json!({"jsonrpc":"2.0","id":request.body["id"],"result":{
                        "protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"sessions","version":"1"}
                    }}));
                    reply.headers.push(("mcp-session-id".into(), uuid::Uuid::new_v4().to_string()));
                    return reply;
                }
                if method == "tools/list" {
                    return Reply::json(json!({"jsonrpc":"2.0","id":request.body["id"],"result":{"tools":[
                        {"name":"t3_thread_read","inputSchema":{"type":"object"}},
                        {"name":"t3_thread_send","inputSchema":{"type":"object"}},
                        {"name":"delegate_task","inputSchema":{"type":"object"}}
                    ]}}));
                }
                if method == "notifications/cancelled" {
                    events.send(request).unwrap();
                    return Reply::status(202, Value::Null);
                }
                if method == "tools/call" {
                    let slot = request.body["params"]["arguments"]["slot"].as_u64().unwrap() as usize;
                    events.send(request.clone()).unwrap();
                    gates[slot].release.notified().await;
                    if request.body["params"]["arguments"]["reject"] == true {
                        return Reply::json(json!({"jsonrpc":"2.0","id":request.body["id"],"error":{
                            "code":-32602,"message":"upstream rejection",
                            "data":{"private":"cancelled-rejection-marker"},
                        }}));
                    }
                    let mut reply = Reply::json(json!({"jsonrpc":"2.0","id":request.body["id"],"result":{
                        "content":[],"structuredContent":{"params":request.body["params"]}
                    }}));
                    reply.drop_response = request.body["params"]["arguments"]["lose"] == true;
                    return reply;
                }
                Reply::status(202, Value::Null)
            }.boxed()
        }).await;
        let directory =
            std::env::temp_dir().join(format!("t3-native-sessions-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let environment = HashMap::from([
            ("T3_HTTP_BASE_URL".into(), native.url.clone()),
            ("T3_ACCESS_TOKEN".into(), "fixture-internal".into()),
            (
                "T3_MCP_DATA_DIR".into(),
                directory.to_string_lossy().into_owned(),
            ),
        ]);
        let config = load_config_from(&environment, &directory).unwrap();
        let fixture = Self {
            native,
            gateway: Arc::new(T3Gateway::new(config)),
            server: McpServer::gateway(),
            directory,
            events: Mutex::new(receiver),
            gates,
            block_initialize,
            initialize_release,
        };
        fixture.credential("old-token");
        fixture
    }

    fn credential(&self, token: &str) {
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

    async fn send(&self, session: Option<&str>, body: Value) -> Response<Bytes> {
        tokio::time::timeout(
            Duration::from_secs(3),
            self.server.handle_http(
                request("POST", session, body),
                &options(),
                self.gateway.as_ref(),
            ),
        )
        .await
        .unwrap()
    }

    async fn session(&self) -> String {
        let response = self.send(None, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}
        }})).await;
        assert_eq!(response.status(), 200);
        response.headers()["mcp-session-id"]
            .to_str()
            .unwrap()
            .to_owned()
    }

    fn call(
        &self,
        session: &str,
        id: Value,
        slot: usize,
        lose: bool,
    ) -> tokio::task::JoinHandle<Response<Bytes>> {
        let server = self.server.clone();
        let gateway = self.gateway.clone();
        let request = request("POST", Some(session), call(id, slot, lose));
        tokio::spawn(async move {
            tokio::time::timeout(
                Duration::from_secs(5),
                server.handle_http(request, &options(), gateway.as_ref()),
            )
            .await
            .unwrap()
        })
    }

    async fn event(&self) -> test_server::Request {
        tokio::time::timeout(Duration::from_secs(3), self.events.lock().await.recv())
            .await
            .unwrap()
            .unwrap()
    }

    fn calls(&self) -> usize {
        self.native
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.body["method"] == "tools/call")
            .count()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn options() -> HttpOptions {
    HttpOptions {
        bearer_token: Some("gateway-secret".into()),
        ..Default::default()
    }
}

fn request(method: &str, session: Option<&str>, body: Value) -> Request<Bytes> {
    let mut request = Request::builder()
        .method(method)
        .uri("/mcp")
        .header("authorization", "Bearer gateway-secret")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(session) = session {
        request = request.header("mcp-session-id", session);
    }
    request.body(Bytes::from(body.to_string())).unwrap()
}

fn body(response: &Response<Bytes>) -> Value {
    serde_json::from_slice(response.body()).unwrap()
}

fn call(id: Value, slot: usize, lose: bool) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{
        "name":"t3_thread_send","arguments":{"slot":slot,"lose":lose},
        "_meta":{"progressToken":0.5,"vendor":{"unchanged":[true,null]}}
    }})
}

fn cancel(id: Value) -> Value {
    json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{
        "requestId":id,"reason":"operator request","vendor":{"unchanged":[1,2]},
        "_meta":{"extension":true}
    }})
}

#[tokio::test]
async fn handles_are_unique_authenticated_and_optional() {
    let fixture = Fixture::new().await;
    let first = fixture.session().await;
    let second = fixture.session().await;
    assert_ne!(first, second);
    for method in ["GET", "POST", "DELETE"] {
        let response = fixture
            .server
            .handle_http(
                request(
                    method,
                    Some("unknown"),
                    json!({"jsonrpc":"2.0","id":2,"method":"ping"}),
                ),
                &options(),
                fixture.gateway.as_ref(),
            )
            .await;
        assert_eq!(response.status(), 404);
        let mut unauthenticated = request(method, Some(&first), Value::Null);
        unauthenticated.headers_mut().remove("authorization");
        assert_eq!(
            fixture
                .server
                .handle_http(unauthenticated, &options(), fixture.gateway.as_ref())
                .await
                .status(),
            401
        );
    }
    let different_domain = HttpOptions {
        bearer_token: Some("another-secret".into()),
        ..Default::default()
    };
    let mut wrong_owner = request(
        "POST",
        Some(&first),
        json!({"jsonrpc":"2.0","id":2,"method":"ping"}),
    );
    wrong_owner
        .headers_mut()
        .insert("authorization", "Bearer another-secret".parse().unwrap());
    assert_eq!(
        fixture
            .server
            .handle_http(wrong_owner, &different_domain, fixture.gateway.as_ref())
            .await
            .status(),
        404
    );
    let ping = fixture
        .send(None, json!({"jsonrpc":"2.0","id":2,"method":"ping"}))
        .await;
    assert_eq!(body(&ping), json!({"jsonrpc":"2.0","id":2,"result":{}}));
    assert!(fixture.native.requests.lock().unwrap().is_empty());
    let deleted = fixture
        .server
        .handle_http(
            request("DELETE", Some(&first), Value::Null),
            &options(),
            fixture.gateway.as_ref(),
        )
        .await;
    assert_eq!(deleted.status(), 200);
    assert_eq!(
        fixture
            .send(
                Some(&first),
                json!({"jsonrpc":"2.0","id":2,"method":"ping"})
            )
            .await
            .status(),
        404
    );
    assert_eq!(
        fixture
            .send(
                Some(&second),
                json!({"jsonrpc":"2.0","id":2,"method":"ping"})
            )
            .await
            .status(),
        200
    );
}

#[tokio::test]
async fn peers_keep_colliding_ids_and_tokens_on_distinct_native_sessions() {
    let fixture = Fixture::new().await;
    let first = fixture.session().await;
    let second = fixture.session().await;
    let one = fixture.call(&first, json!(7), 0, false);
    let native_one = fixture.event().await;
    let two = fixture.call(&second, json!(7), 1, false);
    let native_two = fixture.event().await;
    assert_ne!(
        native_one.headers["mcp-session-id"],
        native_two.headers["mcp-session-id"]
    );
    assert_eq!(
        native_one.body["params"],
        call(json!(7), 0, false)["params"]
    );
    assert_eq!(
        native_two.body["params"],
        call(json!(7), 1, false)["params"]
    );
    let duplicate = fixture.send(Some(&first), call(json!(7), 2, false)).await;
    assert_eq!(body(&duplicate)["error"]["code"], -32600);
    assert_eq!(fixture.calls(), 2);
    assert_eq!(
        fixture
            .send(Some(&first), cancel(json!("7")))
            .await
            .status(),
        202
    );
    assert_eq!(
        fixture
            .send(Some(&first), cancel(json!("unknown")))
            .await
            .status(),
        202
    );
    assert_eq!(fixture.send(None, cancel(json!(7))).await.status(), 202);
    let third = fixture.session().await;
    assert_eq!(
        fixture.send(Some(&third), cancel(json!(7))).await.status(),
        202
    );
    assert_eq!(
        fixture.send(Some(&first), cancel(json!(7))).await.status(),
        202
    );
    let cancellation = fixture.event().await;
    assert_eq!(cancellation.body["method"], "notifications/cancelled");
    let mut expected = cancel(json!(7));
    expected["params"]["requestId"] = native_one.body["id"].clone();
    assert_eq!(cancellation.body, expected);
    assert_eq!(
        cancellation.headers["mcp-session-id"],
        native_one.headers["mcp-session-id"]
    );
    fixture.gates[0].release.notify_one();
    fixture.gates[1].release.notify_one();
    assert!(body(&one.await.unwrap()).get("result").is_some());
    assert!(body(&two.await.unwrap()).get("result").is_some());
    assert_eq!(
        fixture.send(Some(&first), cancel(json!(7))).await.status(),
        202
    );
    let typed = fixture.call(&first, json!("7"), 2, false);
    let native_typed = fixture.event().await;
    assert_eq!(native_typed.body["method"], "tools/call");
    fixture.gates[2].release.notify_one();
    assert_eq!(body(&typed.await.unwrap())["id"], "7");
    let requests = fixture.native.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.body["method"] == "notifications/cancelled")
            .count(),
        1
    );
    let catalog_sessions = requests
        .iter()
        .filter(|r| r.body["method"] == "tools/list")
        .map(|r| r.headers["mcp-session-id"].clone())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(catalog_sessions.len(), 2);
    assert!(
        requests
            .iter()
            .filter(|r| r.body["method"] == "tools/call")
            .all(|r| r.body["params"]["name"] == "t3_thread_send")
    );
}

#[tokio::test]
async fn cancellation_during_discovery_prevents_the_unsent_call() {
    let fixture = Fixture::new().await;
    let session = fixture.session().await;
    fixture.block_initialize.store(true, Ordering::SeqCst);
    let pending = fixture.call(&session, json!(9), 0, false);
    assert_eq!(fixture.event().await.body["method"], "initialize");
    let duplicate = fixture.send(Some(&session), call(json!(9), 2, false)).await;
    assert_eq!(body(&duplicate)["error"]["code"], -32600);
    assert_eq!(
        fixture
            .send(Some(&session), cancel(json!(9)))
            .await
            .status(),
        202
    );
    fixture.initialize_release.notify_one();
    assert_eq!(body(&pending.await.unwrap())["error"]["code"], -32800);
    assert_eq!(fixture.calls(), 0);
    let admitted = fixture.call(&session, json!(9), 1, false);
    assert_eq!(fixture.event().await.body["method"], "tools/call");
    fixture.gates[1].release.notify_one();
    assert!(body(&admitted.await.unwrap()).get("result").is_some());
}

#[tokio::test]
async fn cancellation_uses_the_admitted_credential_generation_and_never_replays() {
    let fixture = Fixture::new().await;
    let session = fixture.session().await;
    let old = fixture.call(&session, json!(1), 0, true);
    let old_wire = fixture.event().await;
    fixture.credential("new-token");
    let new = fixture.call(&session, json!(2), 1, false);
    let new_wire = fixture.event().await;
    assert_eq!(old_wire.headers["authorization"], "Bearer old-token");
    assert_eq!(new_wire.headers["authorization"], "Bearer new-token");
    assert_ne!(
        old_wire.headers["mcp-session-id"],
        new_wire.headers["mcp-session-id"]
    );
    assert_eq!(
        fixture
            .send(Some(&session), cancel(json!(1)))
            .await
            .status(),
        202
    );
    let cancellation = fixture.event().await;
    assert_eq!(
        cancellation.body["params"]["requestId"],
        old_wire.body["id"]
    );
    assert_eq!(cancellation.headers["authorization"], "Bearer old-token");
    assert_eq!(
        cancellation.headers["mcp-session-id"],
        old_wire.headers["mcp-session-id"]
    );
    fixture.gates[0].release.notify_one();
    fixture.gates[1].release.notify_one();
    assert_eq!(
        body(&old.await.unwrap())["result"]["structuredContent"]["error"]["code"],
        "native_outcome_unknown"
    );
    assert!(body(&new.await.unwrap()).get("result").is_some());
    assert_eq!(fixture.calls(), 2);
}

#[tokio::test]
async fn delete_and_lost_response_keep_an_admitted_mutation_alive() {
    let fixture = Fixture::new().await;
    let session = fixture.session().await;
    let admitted = fixture.call(&session, json!(1), 0, false);
    let native = fixture.event().await;
    let response = fixture
        .server
        .handle_http(
            request("DELETE", Some(&session), Value::Null),
            &options(),
            fixture.gateway.as_ref(),
        )
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        fixture
            .send(Some(&session), cancel(json!(1)))
            .await
            .status(),
        404
    );
    admitted.abort();
    assert!(admitted.await.unwrap_err().is_cancelled());
    fixture.gates[0].release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let events = std::fs::read_to_string(fixture.directory.join("audit.jsonl")).unwrap();
            if events
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .any(|event| {
                    event["event"] == "upstream.response"
                        && event["operation"] == "mcp:t3_thread_send"
                        && event["outcome"] == "completed"
                })
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let next_session = fixture.session().await;
    let next = fixture.call(&next_session, json!(1), 1, false);
    let next_wire = fixture.event().await;
    assert_ne!(
        native.headers["mcp-session-id"],
        next_wire.headers["mcp-session-id"]
    );
    fixture.gates[1].release.notify_one();
    assert!(body(&next.await.unwrap()).get("result").is_some());
    assert_eq!(fixture.calls(), 2);
}

#[tokio::test]
async fn cancelled_reads_do_not_retry_a_lost_response() {
    let fixture = Fixture::new().await;
    let session = fixture.session().await;
    let mut value = call(json!(1), 0, true);
    value["params"]["name"] = json!("t3_thread_read");
    let server = fixture.server.clone();
    let gateway = fixture.gateway.clone();
    let request = request("POST", Some(&session), value);
    let pending = tokio::spawn(async move {
        server
            .handle_http(request, &options(), gateway.as_ref())
            .await
    });
    let wire = fixture.event().await;
    fixture.send(Some(&session), cancel(json!(1))).await;
    let cancellation = fixture.event().await;
    assert_eq!(cancellation.body["params"]["requestId"], wire.body["id"]);
    fixture.gates[0].release.notify_one();
    let response = tokio::time::timeout(Duration::from_secs(3), pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        body(&response)["result"]["structuredContent"]["error"]["code"],
        "native_unavailable"
    );
    assert_eq!(fixture.calls(), 1);
}

#[tokio::test]
async fn cancelled_native_rejections_do_not_forward_upstream_data() {
    let fixture = Fixture::new().await;
    let session = fixture.session().await;
    let mut value = call(json!(1), 0, false);
    value["params"]["arguments"]["reject"] = json!(true);
    let server = fixture.server.clone();
    let gateway = fixture.gateway.clone();
    let request = request("POST", Some(&session), value);
    let pending = tokio::spawn(async move {
        server
            .handle_http(request, &options(), gateway.as_ref())
            .await
    });
    let wire = fixture.event().await;
    fixture.send(Some(&session), cancel(json!(1))).await;
    let cancellation = fixture.event().await;
    assert_eq!(cancellation.body["params"]["requestId"], wire.body["id"]);
    fixture.gates[0].release.notify_one();
    let response = tokio::time::timeout(Duration::from_secs(3), pending)
        .await
        .unwrap()
        .unwrap();
    let result = body(&response);
    assert_eq!(result["error"]["code"], -32800, "{result}");
    assert!(result["error"].get("data").is_none(), "{result}");
    assert!(!result.to_string().contains("cancelled-rejection-marker"));
    assert_eq!(fixture.calls(), 1);
}

#[tokio::test]
async fn sessionless_native_calls_need_no_initialize_handshake() {
    let fixture = Fixture::new().await;
    let server = fixture.server.clone();
    let gateway = fixture.gateway.clone();
    let pending = tokio::spawn(async move {
        server
            .handle_http(
                request("POST", None, call(json!(1), 0, false)),
                &options(),
                gateway.as_ref(),
            )
            .await
    });
    assert_eq!(fixture.event().await.body["method"], "tools/call");
    fixture.gates[0].release.notify_one();
    let response = pending.await.unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.headers().get("mcp-session-id").is_none());
    assert_eq!(
        body(&response)["result"]["structuredContent"]["params"],
        call(json!(1), 0, false)["params"]
    );
}

#[cfg(unix)]
struct Process(Option<std::process::Child>);

#[cfg(unix)]
impl Process {
    fn spawn(fixture: &Fixture, transport: &str, port: u16) -> Self {
        use std::process::{Command, Stdio};
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
            .env("T3_ACCESS_TOKEN", "internal-fixture")
            .env("T3_MCP_DATA_DIR", &fixture.directory)
            .env("MCP_BEARER_TOKEN", "gateway-secret")
            .env("MCP_READ_ONLY", "false")
            .env("MCP_TRANSPORT", transport)
            .env("MCP_PORT", port.max(1).to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Self(Some(command.spawn().unwrap()))
    }

    fn signal(&mut self) {
        let child = self.0.as_ref().unwrap();
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    }

    async fn finish(mut self) -> std::process::Output {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.0.as_mut().unwrap().try_wait().unwrap().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        self.0.take().unwrap().wait_with_output().unwrap()
    }
}

#[cfg(unix)]
impl Drop for Process {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(unix)]
fn audit(fixture: &Fixture) -> Vec<Value> {
    std::fs::read_to_string(fixture.directory.join("audit.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_eof_and_signal_drain_an_admitted_mutation() {
    use std::io::Write;
    for signal in [false, true] {
        let fixture = Fixture::new().await;
        let mut process = Process::spawn(&fixture, "stdio", 0);
        writeln!(
            process.0.as_mut().unwrap().stdin.as_mut().unwrap(),
            "{}",
            call(json!(1), 0, false)
        )
        .unwrap();
        assert_eq!(fixture.event().await.body["method"], "tools/call");
        if signal {
            process.signal();
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    assert!(
                        process.0.as_mut().unwrap().try_wait().unwrap().is_none(),
                        "stdio exited before draining its mutation"
                    );
                    if audit(&fixture).iter().any(|event| {
                        event["event"] == "stdio.session" && event["outcome"] == "draining"
                    }) {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        } else {
            drop(process.0.as_mut().unwrap().stdin.take());
        }
        fixture.gates[0].release.notify_one();
        let output = process.finish().await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            response["result"]["structuredContent"]["params"],
            call(json!(1), 0, false)["params"]
        );
        assert_eq!(fixture.calls(), 1);
        assert!(
            audit(&fixture)
                .iter()
                .any(|event| event["event"] == "tool.result"
                    && event["operation"] == "t3_thread_send"
                    && event["outcome"] == "completed")
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn http_disconnect_and_shutdown_drain_the_complete_gateway_worker() {
    let fixture = Fixture::new().await;
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let mut process = Process::spawn(&fixture, "http", port);
    let url = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while client.get(format!("{url}/healthz")).send().await.is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let pending = tokio::spawn(async move {
        client
            .post(format!("{url}/mcp"))
            .bearer_auth("gateway-secret")
            .header("accept", "application/json, text/event-stream")
            .json(&call(json!(1), 0, false))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
    });
    assert_eq!(fixture.event().await.body["method"], "tools/call");
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    process.signal();
    tokio::time::timeout(Duration::from_secs(3), async {
        while tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(process.0.as_mut().unwrap().try_wait().unwrap().is_none());
    fixture.gates[0].release.notify_one();
    let output = process.finish().await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.calls(), 1);
    assert!(
        audit(&fixture)
            .iter()
            .any(|event| event["event"] == "tool.result"
                && event["operation"] == "t3_thread_send"
                && event["outcome"] == "completed")
    );
}

#[tokio::test]
async fn cancellation_while_a_new_connection_opens_keeps_the_call_unsent() {
    let fixture = Fixture::new().await;
    let session = fixture.session().await;
    let listed = fixture
        .send(
            Some(&session),
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        )
        .await;
    assert!(body(&listed)["result"]["tools"].is_array());
    fixture.credential("renewed-token");
    fixture.block_initialize.store(true, Ordering::SeqCst);
    let pending = fixture.call(&session, json!(2), 0, false);
    assert_eq!(fixture.event().await.body["method"], "initialize");
    assert_eq!(
        fixture
            .send(Some(&session), cancel(json!(2)))
            .await
            .status(),
        202
    );
    fixture.initialize_release.notify_one();
    let response = tokio::time::timeout(Duration::from_secs(3), pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(body(&response)["error"]["code"], -32800);
    assert_eq!(fixture.calls(), 0);
}

#[tokio::test]
async fn deleted_peers_keep_their_session_slot_until_work_finishes() {
    let fixture = Fixture::new().await;
    let first = fixture.session().await;
    for _ in 1..1024 {
        fixture.session().await;
    }
    let initialize = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}
    }});
    assert_eq!(fixture.send(None, initialize.clone()).await.status(), 503);
    let pending = fixture.call(&first, json!(2), 0, false);
    assert_eq!(fixture.event().await.body["method"], "tools/call");
    let deleted = fixture
        .server
        .handle_http(
            request("DELETE", Some(&first), Value::Null),
            &options(),
            fixture.gateway.as_ref(),
        )
        .await;
    assert_eq!(deleted.status(), 200);
    assert_eq!(fixture.send(None, initialize.clone()).await.status(), 503);
    assert_eq!(
        fixture
            .send(None, json!({"jsonrpc":"2.0","id":1,"method":"ping"}))
            .await
            .status(),
        200
    );
    fixture.gates[0].release.notify_one();
    assert!(body(&pending.await.unwrap()).get("result").is_some());
    assert_eq!(fixture.send(None, initialize).await.status(), 200);
    assert_eq!(fixture.calls(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_output_loss_drains_other_admitted_mutations() {
    use std::io::Write;
    let fixture = Fixture::new().await;
    let mut process = Process::spawn(&fixture, "stdio", 0);
    write!(
        process.0.as_mut().unwrap().stdin.as_mut().unwrap(),
        "{}\n{}\n",
        call(json!(1), 0, false),
        call(json!(2), 1, false)
    )
    .unwrap();
    assert_eq!(fixture.event().await.body["method"], "tools/call");
    assert_eq!(fixture.event().await.body["method"], "tools/call");
    drop(process.0.as_mut().unwrap().stdout.take());
    fixture.gates[0].release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            assert!(
                process.0.as_mut().unwrap().try_wait().unwrap().is_none(),
                "stdio dropped another admitted mutation after its output failed"
            );
            if audit(&fixture)
                .iter()
                .any(|event| event["event"] == "stdio.session" && event["outcome"] == "draining")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    fixture.gates[1].release.notify_one();
    let output = process.finish().await;
    assert!(!output.status.success());
    assert_eq!(fixture.calls(), 2);
    assert_eq!(
        audit(&fixture)
            .iter()
            .filter(|event| event["event"] == "tool.result"
                && event["operation"] == "t3_thread_send"
                && event["outcome"] == "completed")
            .count(),
        2
    );
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_reserves_and_cancels_before_spawning_the_call() {
    use std::io::Write;
    let fixture = Fixture::new().await;
    let mut process = Process::spawn(&fixture, "stdio", 0);
    let messages = format!("{}\n{}\n", call(json!(7), 0, false), cancel(json!(7)));
    process
        .0
        .as_mut()
        .unwrap()
        .stdin
        .as_mut()
        .unwrap()
        .write_all(messages.as_bytes())
        .unwrap();
    drop(process.0.as_mut().unwrap().stdin.take());
    fixture.gates[0].release.notify_one();
    let output = process.finish().await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["error"]["code"], -32800);
    assert_eq!(fixture.calls(), 0);
}

#[tokio::test]
async fn string_and_numeric_ids_can_be_active_on_the_same_peer() {
    let fixture = Fixture::new().await;
    let session = fixture.session().await;
    let numeric = fixture.call(&session, json!(7), 0, false);
    let numeric_wire = fixture.event().await;
    let string = fixture.call(&session, json!("7"), 1, false);
    let string_wire = fixture.event().await;
    assert_eq!(
        numeric_wire.headers["mcp-session-id"],
        string_wire.headers["mcp-session-id"]
    );
    assert_ne!(numeric_wire.body["id"], string_wire.body["id"]);
    fixture.send(Some(&session), cancel(json!("7"))).await;
    let cancellation = fixture.event().await;
    assert_eq!(
        cancellation.body["params"]["requestId"],
        string_wire.body["id"]
    );
    fixture.gates[0].release.notify_one();
    fixture.gates[1].release.notify_one();
    assert_eq!(body(&numeric.await.unwrap())["id"], 7);
    assert_eq!(body(&string.await.unwrap())["id"], "7");
    assert_eq!(fixture.calls(), 2);
}

#[tokio::test]
async fn a_batch_reserves_all_ids_before_discovery() {
    let fixture = Fixture::new().await;
    let session = fixture.session().await;
    let response = fixture
        .send(
            Some(&session),
            json!([
                call(json!(1), 0, false),
                call(json!(1), 1, false),
                cancel(json!(1))
            ]),
        )
        .await;
    let responses = body(&response);
    assert_eq!(responses.as_array().unwrap().len(), 2);
    assert_eq!(responses[0]["error"]["code"], -32800);
    assert_eq!(responses[1]["error"]["code"], -32600);
    assert_eq!(fixture.calls(), 0);
}
