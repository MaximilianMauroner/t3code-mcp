use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_code_mcp::{
    config::load_config_from,
    gateway::T3Gateway,
    mcp::{IncomingMessage, McpServer},
    t3::{T3HttpClient, http_client::ErrorKind},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Notify,
    task::{JoinHandle, JoinSet},
};
use tokio_tungstenite::tungstenite::{Message, handshake::derive_accept_key, protocol::Role};

const CHECKED: &str = "2026-10-22T10:00:00Z";

fn provider(id: &str) -> Value {
    json!({"instanceId":id,"driver":"codex","displayName":"Shared display name",
        "enabled":true,"installed":true,"status":"ready","checkedAt":CHECKED,
        "models":[{"slug":"default","name":"Default model","isCustom":false,"isDefault":true}]})
}

fn window(percent: f64) -> Value {
    json!({"id":"weekly-opus","kind":"weekly","label":"Weekly Opus","usedPercent":percent})
}

fn with_usage(id: &str, usage: Value) -> Value {
    let mut value = provider(id);
    value["usageLimits"] = usage;
    value
}

#[derive(Clone, Copy)]
enum Mode {
    Success,
    Auth,
    Rpc,
    Disconnect,
    Malformed,
    AfterEnd,
    AfterDisconnect,
    AfterTimeout,
    AfterRpc,
    AfterDefect,
    AfterMalformed,
}

struct Options {
    sources: Value,
    events: Option<Vec<Value>>,
    following_events: Option<Vec<Value>>,
    refresh_error: Option<String>,
    refresh_stall: bool,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            sources: json!([]),
            events: None,
            following_events: None,
            refresh_error: None,
            refresh_stall: false,
        }
    }
}

struct Fixture {
    config: Arc<Mutex<Value>>,
    paths: Arc<Mutex<Vec<String>>>,
    calls: Arc<Mutex<Vec<Value>>>,
    directory: PathBuf,
    gateway: T3Gateway,
    client: T3HttpClient,
    snapshot_ack: Arc<Notify>,
    task: JoinHandle<()>,
}

impl Fixture {
    async fn new(config: Value, mode: Mode, environment: Option<&str>) -> Self {
        Self::with_options(config, mode, environment, Options::default()).await
    }

    async fn with_options(
        config: Value,
        mode: Mode,
        environment: Option<&str>,
        options: Options,
    ) -> Self {
        let options = Arc::new(options);
        let snapshot_ack = Arc::new(Notify::new());
        let snapshot_notice = snapshot_ack.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let config = Arc::new(Mutex::new(config));
        let paths = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (data, recorded, requests) = (config.clone(), paths.clone(), calls.clone());
        let task = tokio::spawn(async move {
            let mut clients = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut socket, _) = accepted.unwrap();
                        let (data, recorded, requests, options, snapshot_notice) = (data.clone(), recorded.clone(), requests.clone(), options.clone(), snapshot_notice.clone());
                        clients.spawn(async move {
                            let mut head = Vec::new();
                            while !head.ends_with(b"\r\n\r\n") {
                                let mut byte = [0];
                                if socket.read_exact(&mut byte).await.is_err() { return; }
                                head.push(byte[0]);
                                assert!(head.len() < 65536);
                            }
                            let head = String::from_utf8(head).unwrap();
                            let path = head.split_whitespace().nth(1).unwrap();
                            recorded.lock().unwrap().push(path.to_owned());
                            if path == "/.well-known/t3/environment" {
                                let body = json!({"environmentId":"environment-test","label":"Test","serverVersion":"1","capabilities":{}}).to_string();
                                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                                return;
                            }
                            assert_eq!(path, "/ws?orchestrationProtocol=2");
                            let headers = head.lines().skip(1).filter_map(|line| line.split_once(':'))
                                .map(|(key,value)| (key.to_ascii_lowercase(),value.trim())).collect::<HashMap<_,_>>();
                            assert_eq!(headers["authorization"], "Bearer fixture-token");
                            if matches!(mode, Mode::Auth) {
                                socket.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                                return;
                            }
                            let key = derive_accept_key(headers["sec-websocket-key"].as_bytes());
                            socket.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {key}\r\n\r\n").as_bytes()).await.unwrap();
                            let mut ws = tokio_tungstenite::WebSocketStream::from_raw_socket(socket, Role::Server, None).await;
                            let request: Value = serde_json::from_slice(&ws.next().await.unwrap().unwrap().into_data()).unwrap();
                            requests.lock().unwrap().push(request.clone());
                            if matches!(mode, Mode::Disconnect) { let _ = ws.close(None).await; return; }
                            if matches!(mode, Mode::Malformed) { let _ = ws.send(Message::Text("not-json".into())).await; return; }
                            if request["tag"] == "server.refreshProviders" && options.refresh_stall { while ws.next().await.is_some() {} return; }
                            if matches!(mode, Mode::Rpc) || request["tag"] == "server.refreshProviders" {
                                let error = if matches!(mode, Mode::Rpc) { Some("requires orchestration:read") } else { options.refresh_error.as_deref() };
                                let exit = match error {
                                    Some(message) => json!({"_tag":"Failure","cause":[{"_tag":"Fail","error":{"message":message}}]}),
                                    None => json!({"_tag":"Success","value":null}),
                                };
                                let _ = ws.send(Message::Text(json!({"_tag":"Exit","requestId":request["id"],"exit":exit}).to_string().into())).await;
                                return;
                            }
                            assert_eq!(request["tag"], "subscribeServerConfig");
                            assert_eq!(request["payload"], json!({"usageLimitSources":true}));
                            let values = options.events.clone().unwrap_or_else(|| vec![json!({"version":1,"type":"snapshot","config":data.lock().unwrap().clone()})]);
                            let _ = ws.send(Message::Text(json!({"_tag":"Chunk","requestId":request["id"],"values":values}).to_string().into())).await;
                            let Some(Ok(ack)) = ws.next().await else { return; };
                            let Ok(ack) = serde_json::from_slice::<Value>(&ack.into_data()) else { return; };
                            if ack["_tag"] != "Ack" { return; }
                            assert_eq!(ack["requestId"], request["id"]);
                            snapshot_notice.notify_one();
                            if matches!(mode, Mode::AfterDisconnect) { let _ = ws.close(None).await; return; }
                            if matches!(mode, Mode::AfterTimeout) { while ws.next().await.is_some() {} return; }
                            let response = match mode {
                                Mode::AfterEnd => json!({"_tag":"Exit","requestId":request["id"],"exit":{"_tag":"Success","value":null}}).to_string(),
                                Mode::AfterRpc => json!({"_tag":"Exit","requestId":request["id"],"exit":{"_tag":"Failure","cause":[{"_tag":"Fail","error":{"message":"source read failed"}}]}}).to_string(),
                                Mode::AfterDefect => json!({"_tag":"Defect"}).to_string(),
                                Mode::AfterMalformed => "not-json".to_owned(),
                                _ => json!({"_tag":"Chunk","requestId":request["id"],"values":options.following_events.clone().unwrap_or_else(||vec![json!({"version":1,"type":"usageLimitSourcesUpdated","payload":{"sources":options.sources}})])}).to_string(),
                            };
                            let _ = ws.send(Message::Text(response.into())).await;
                            while ws.next().await.is_some() {}

                        });
                    }
                    result = clients.join_next(), if !clients.is_empty() => { result.unwrap().unwrap(); }
                }
            }
        });
        let directory = std::env::current_dir()
            .unwrap()
            .join(format!(".models-usage-test-{}", uuid::Uuid::new_v4()));
        let mut env = HashMap::from([
            ("T3_HTTP_BASE_URL".into(), url.clone()),
            ("T3_ACCESS_TOKEN".into(), "fixture-token".into()),
            (
                "T3_MCP_DATA_DIR".into(),
                directory.to_string_lossy().into_owned(),
            ),
            ("MCP_READ_ONLY".into(), "true".into()),
        ]);
        if let Some(id) = environment {
            env.insert("T3_ENVIRONMENT_ID".into(), id.into());
        }
        let client = T3HttpClient::new(url, "fixture-token", Duration::from_millis(200), None);
        let gateway = T3Gateway::new(load_config_from(&env, &directory).unwrap());
        Self {
            config,
            paths,
            calls,
            directory,
            gateway,
            client,
            snapshot_ack,
            task,
        }
    }

    async fn call(&self, args: Value) -> Value {
        let response = McpServer::gateway().handle_message(IncomingMessage::parse(json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"t3_models_usage","arguments":args}
        })).unwrap(),&self.gateway).await.unwrap();
        serde_json::to_value(response).unwrap()["result"].clone()
    }

    fn audit(&self) -> String {
        std::fs::read_to_string(self.directory.join("audit.jsonl")).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[tokio::test]
async fn models_usage_returns_safe_complete_catalogs_and_shared_windows() {
    let mut first = with_usage(
        "first",
        json!({"checkedAt":CHECKED,"windows":[
        {"id":"week","kind":"weekly","label":"Weekly Opus","usedPercent":0,"resetsAt":"2026-10-23T10:00:00+02:00","windowDurationMins":10080},
        {"id":"session","kind":"session","label":"Session","usedPercent":100}
    ],"credentialFingerprint":"secret-fingerprint","resetCredits":{"availableCount":2,"nextExpiresAt":CHECKED,"nextCreditId":"secret-credit"},
    "externalUsage":{"url":"https://secret.example/token"}}),
    );
    first["models"][0]["capabilities"] = json!({"secret":"hidden-capability"});
    first["models"].as_array_mut().unwrap().push(json!({"slug":"custom","name":"Custom","isCustom":true,"isLegacy":true,"aliases":["alias"],"subProvider":"custom-provider","unknown":"hidden-model"}));
    first["auth"] = json!({"token":"secret-auth"});
    first["config"] = json!({"secret":"secret-config"});
    let fixture = Fixture::new(
        json!({"providers":[first,provider("second")],"settings":{"secret":"secret-settings"}}),
        Mode::Success,
        None,
    )
    .await;
    let result = fixture.call(json!({})).await;
    assert_ne!(result["isError"], true, "{result}");
    let output = &result["structuredContent"];
    assert_eq!(output["environmentId"], "environment-test");
    chrono::DateTime::parse_from_rfc3339(output["observedAt"].as_str().unwrap()).unwrap();
    let providers = output["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 2);
    assert_eq!(
        providers[0]["models"],
        json!([
            {"slug":"default","name":"Default model","isCustom":false,"isDefault":true},
            {"slug":"custom","name":"Custom","isCustom":true,"isLegacy":true,"aliases":["alias"],"subProvider":"custom-provider"}
        ])
    );
    assert_eq!(
        providers[0]["usage"],
        json!({"status":"reported","checkedAt":CHECKED,"windows":[
        {"id":"week","kind":"weekly","label":"Weekly Opus","usedPercent":0.0,"remainingPercent":100.0,"resetsAt":"2026-10-23T10:00:00+02:00","windowDurationMins":10080},
        {"id":"session","kind":"session","label":"Session","usedPercent":100.0,"remainingPercent":0.0}
    ],"resetCredits":{"availableCount":2,"nextExpiresAt":CHECKED}})
    );
    assert_eq!(
        providers[1]["usage"],
        json!({"status":"missing","windows":[]})
    );
    for private in [
        "secret-",
        "hidden-",
        "capabilities",
        "credentialFingerprint",
        "nextCreditId",
        "externalUsage",
        "\"auth\"",
        "\"settings\"",
        "\"config\"",
    ] {
        assert!(!result.to_string().contains(private), "{private}");
        assert!(!fixture.audit().contains(private), "{private}");
    }
    let calls = fixture.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["tag"], "subscribeServerConfig");
    assert_eq!(calls[0]["payload"], json!({"usageLimitSources":true}));
    assert_eq!(
        *fixture.paths.lock().unwrap(),
        ["/.well-known/t3/environment", "/ws?orchestrationProtocol=2"]
    );
    assert!(
        fixture
            .audit()
            .contains("\"operation\":\"subscribeServerConfig\"")
    );
}

#[tokio::test]
async fn models_usage_preserves_source_states_and_unavailable_windows() {
    let fixture = Fixture::new(json!({"providers":[provider("missing"),
        with_usage("unsupported",json!({"checkedAt":CHECKED,"windows":[],"unavailable":{"reason":"unsupported"}})),
        with_usage("failed",json!({"checkedAt":CHECKED,"windows":[window(25.5)],"unavailable":{"reason":"probeFailed","message":"secret-provider-error"}})),
        with_usage("empty",json!({"checkedAt":CHECKED,"windows":[]}))
    ]}),Mode::Success,None).await;
    let result = fixture.call(json!({})).await;
    assert_ne!(result["isError"], true, "{result}");
    let providers = &result["structuredContent"]["providers"];
    assert_eq!(
        providers[0]["usage"],
        json!({"status":"missing","windows":[]})
    );
    assert_eq!(
        providers[1]["usage"],
        json!({"status":"unsupported","checkedAt":CHECKED,"windows":[],"unavailable":{"reason":"unsupported"}})
    );
    assert_eq!(
        providers[2]["usage"],
        json!({"status":"probeFailed","checkedAt":CHECKED,"windows":[{"id":"weekly-opus","kind":"weekly","label":"Weekly Opus","usedPercent":25.5,"remainingPercent":74.5}],"unavailable":{"reason":"probeFailed"}})
    );
    assert_eq!(
        providers[3]["usage"],
        json!({"status":"empty","checkedAt":CHECKED,"windows":[]})
    );
    assert!(!result.to_string().contains("secret-provider-error"));
    assert!(!fixture.audit().contains("secret-provider-error"));
}

#[tokio::test]
async fn models_usage_filters_trimmed_exact_ids_rejects_blank_ids_and_reads_anew() {
    let fixture = Fixture::new(
        json!({"providers":[provider("first"),provider("second")]}),
        Mode::Success,
        None,
    )
    .await;
    let result = fixture
        .call(json!({"providerInstanceId":" \tsecond\n"}))
        .await;
    assert_ne!(result["isError"], true, "{result}");
    assert_eq!(
        result["structuredContent"]["providers"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        result["structuredContent"]["providers"][0]["instanceId"],
        "second"
    );
    fixture.config.lock().unwrap()["providers"][0]["displayName"] = json!("Updated name");
    let result = fixture.call(json!({"providerInstanceId":"first"})).await;
    assert_eq!(
        result["structuredContent"]["providers"][0]["displayName"],
        "Updated name"
    );
    let result = fixture.call(json!({"providerInstanceId":"codex"})).await;
    assert_eq!(
        result["structuredContent"]["error"]["code"],
        "provider_instance_not_found"
    );
    for args in [
        json!({"providerInstanceId":" \t\n"}),
        json!({"providerInstanceId":null}),
        json!({"providerInstanceId":4}),
    ] {
        assert_eq!(fixture.call(args).await["isError"], true);
    }
    assert_eq!(fixture.calls.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn models_usage_checks_environment_before_sensitive_rpc() {
    let fixture = Fixture::new(
        json!({"providers":[]}),
        Mode::Success,
        Some("another-environment"),
    )
    .await;
    let result = fixture.call(json!({})).await;
    assert_eq!(
        result["structuredContent"]["error"]["code"], "environment_mismatch",
        "{result}"
    );
    assert_eq!(
        *fixture.paths.lock().unwrap(),
        ["/.well-known/t3/environment"]
    );
    assert!(fixture.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn models_usage_accepts_an_empty_catalog() {
    let fixture = Fixture::new(json!({"providers":[]}), Mode::Success, None).await;
    let result = fixture.call(json!({})).await;
    assert_ne!(result["isError"], true, "{result}");
    assert_eq!(result["structuredContent"]["providers"], json!([]));
}

#[tokio::test]
async fn models_usage_keeps_upstream_failures_as_errors() {
    for (mode, message) in [
        (Mode::Auth, "HTTP 403"),
        (Mode::Rpc, "orchestration:read"),
        (Mode::Disconnect, "disconnected"),
        (Mode::Malformed, "invalid response"),
    ] {
        let fixture = Fixture::new(json!({"providers":[]}), mode, None).await;
        let result = fixture.call(json!({})).await;
        assert_eq!(result["isError"], true, "{result}");
        assert_eq!(
            result["structuredContent"]["error"]["code"],
            "gateway_error"
        );
        assert!(
            result["structuredContent"]["error"]["message"]
                .as_str()
                .unwrap()
                .contains(message),
            "{result}"
        );
        assert!(result["structuredContent"].get("providers").is_none());
    }
}

#[tokio::test]
async fn models_usage_rejects_positional_arrays_as_source_objects() {
    let valid = json!({"providers":[with_usage("first",json!({
        "checkedAt":CHECKED,"windows":[window(25.0)],
        "unavailable":{"reason":"probeFailed"},"resetCredits":{"availableCount":1}
    }))]});
    let mut invalid_auth = valid.clone();
    invalid_auth["providers"][0]["auth"] = json!(["private-label"]);
    let mut invalid = vec![json!([[]]), invalid_auth];
    for (pointer, value) in [
        ("/providers/0", json!([])),
        (
            "/providers/0/models/0",
            json!(["default", "Default model", false]),
        ),
        ("/providers/0/usageLimits", json!([CHECKED, []])),
        (
            "/providers/0/usageLimits/windows/0",
            json!(["weekly", "weekly", "Weekly", 25]),
        ),
        (
            "/providers/0/usageLimits/unavailable",
            json!(["unsupported"]),
        ),
        ("/providers/0/usageLimits/resetCredits", json!([1])),
    ] {
        let mut config = valid.clone();
        *config.pointer_mut(pointer).unwrap() = value;
        invalid.push(config);
    }
    for config in invalid {
        let fixture = Fixture::new(config.clone(), Mode::Success, None).await;
        let result = fixture.call(json!({})).await;
        assert_eq!(result["isError"], true, "accepted {config}: {result}");
        assert_eq!(
            result["structuredContent"]["error"]["code"],
            "gateway_error"
        );
        assert!(result["structuredContent"].get("providers").is_none());
    }
}

#[tokio::test]
async fn models_usage_rejects_malformed_catalogs_and_snapshots() {
    let valid = json!({"providers":[with_usage("bad",json!({"checkedAt":CHECKED,"windows":[window(25.0)],"resetCredits":{"availableCount":1}}))]});
    let mut invalid = vec![
        json!({}),
        json!({"providers":null}),
        json!({"providers":[{}]}),
    ];
    for (pointer, bad) in [
        ("/providers/0/instanceId", json!(" ")),
        ("/providers/0/status", json!("unknown")),
        ("/providers/0/enabled", json!(1)),
        ("/providers/0/checkedAt", json!("2026-02-30T10:00:00Z")),
        ("/providers/0/models/0/isCustom", Value::Null),
        ("/providers/0/models/0/slug", json!(" ")),
        ("/providers/0/usageLimits", Value::Null),
        ("/providers/0/usageLimits/checkedAt", json!("yesterday")),
        (
            "/providers/0/usageLimits/checkedAt",
            json!("2026-10-22T10:00:00"),
        ),
        (
            "/providers/0/usageLimits/checkedAt",
            json!("2026-10-22t10:00:00z"),
        ),
        ("/providers/0/usageLimits/windows", Value::Null),
        (
            "/providers/0/usageLimits/windows/0/usedPercent",
            json!(-0.01),
        ),
        (
            "/providers/0/usageLimits/windows/0/usedPercent",
            json!(100.01),
        ),
        (
            "/providers/0/usageLimits/windows/0/usedPercent",
            json!("NaN"),
        ),
        ("/providers/0/usageLimits/windows/0/kind", json!("daily")),
        (
            "/providers/0/usageLimits/resetCredits/availableCount",
            json!(-1),
        ),
        (
            "/providers/0/usageLimits/resetCredits/availableCount",
            json!(0.5),
        ),
        (
            "/providers/0/usageLimits/resetCredits/availableCount",
            json!(9007199254740992_u64),
        ),
    ] {
        let mut config = valid.clone();
        *config.pointer_mut(pointer).unwrap() = bad;
        invalid.push(config);
    }
    for (field, bad) in [
        ("resetsAt", json!("2026-10-22T10:00Z")),
        ("resetsAt", Value::Null),
        ("windowDurationMins", json!(-1)),
        ("windowDurationMins", json!(1.5)),
    ] {
        let mut config = valid.clone();
        config["providers"][0]["usageLimits"]["windows"][0][field] = bad;
        invalid.push(config);
    }
    for field in ["checkedAt", "windows"] {
        let mut config = valid.clone();
        config["providers"][0]["usageLimits"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        invalid.push(config);
    }
    for reason in ["unknown", ""] {
        let mut config = valid.clone();
        config["providers"][0]["usageLimits"]["unavailable"] = json!({"reason":reason});
        invalid.push(config);
    }
    for config in invalid {
        let fixture = Fixture::new(config.clone(), Mode::Success, None).await;
        let result = fixture.call(json!({})).await;
        assert_eq!(result["isError"], true, "accepted {config}: {result}");
        assert_eq!(
            result["structuredContent"]["error"]["code"],
            "gateway_error"
        );
        assert!(
            result["structuredContent"]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("invalid response"),
            "{result}"
        );
        assert!(result["structuredContent"].get("providers").is_none());
    }
}

fn external_source() -> Value {
    json!({"id":"proxy/private","kind":"future-source","label":"CLIProxy","checkedAt":CHECKED,
        "error":"token=private-token https://private.example/secret",
        "accounts":[{"id":"private@example.com","driver":"future-driver","email":"private@example.com",
            "workspaceId":"private-workspace","plan":"Pro","usageLimits":{"checkedAt":CHECKED,
                "windows":[{"id":"primary","kind":"other","label":"Primary","usedPercent":18,"windowDurationMins":10080}],
                "unavailable":{"reason":"probeFailed","message":"private-probe"},
                "credentialFingerprint":"private-fingerprint",
                "resetCredits":{"availableCount":1,"nextCreditId":"private-credit"}}}]})
}

#[tokio::test]
async fn models_usage_reports_source_only_accounts_safe_labels_and_native_filter() {
    let mut native = with_usage(
        "native",
        external_source()["accounts"][0]["usageLimits"].clone(),
    );
    native["auth"] = json!({"label":"ChatGPT Pro 20x Subscription","token":"private-auth"});
    native["plan"] = json!("private-native-plan");
    let mut other_source = external_source();
    other_source["id"] = json!("other-source");
    let fixture = Fixture::with_options(
        json!({"providers":[native,provider("other")]}),
        Mode::Success,
        None,
        Options {
            sources: json!([external_source(), other_source]),
            ..Default::default()
        },
    )
    .await;
    let result = fixture.call(json!({"providerInstanceId":" native "})).await;
    assert_ne!(result["isError"], true, "{result}");
    let output = &result["structuredContent"];
    assert_eq!(output["refresh"], json!({"status":"notRequested"}));
    assert_eq!(output["providers"].as_array().unwrap().len(), 1);
    assert_eq!(
        output["providers"][0]["accountLabel"],
        "ChatGPT Pro 20x Subscription"
    );
    assert_eq!(output["sources"]["items"].as_array().unwrap().len(), 2);
    assert_eq!(
        output["sources"]["items"][1]["sourceRef"],
        "src_e264ed6acf0e847e"
    );
    assert_eq!(
        output["sources"]["items"][1]["accounts"][0]["accountRef"],
        "acct_732c733411f2e4be"
    );
    let source = &output["sources"]["items"][0];
    assert_eq!(output["sources"]["status"], "reported");
    assert_eq!(source["sourceRef"], "src_558de6b776f54b2e");
    assert_eq!(source["kind"], "future-source");
    assert_eq!(source["label"], "CLIProxy");
    assert_eq!(source["checkedAt"], CHECKED);
    assert_eq!(source["accounts"][0]["accountRef"], "acct_0ed7811796e95495");
    assert_eq!(source["accounts"][0]["driver"], "future-driver");
    assert_eq!(source["accounts"][0]["plan"], "Pro");
    assert_eq!(
        source["accounts"][0]["usage"],
        output["providers"][0]["usage"]
    );
    assert_eq!(source["accounts"][0]["usage"]["status"], "probeFailed");
    assert_eq!(
        source["accounts"][0]["usage"]["windows"],
        json!([
            {"id":"primary","kind":"other","label":"Primary","usedPercent":18.0,"remainingPercent":82.0,"windowDurationMins":10080}
        ])
    );
    for private in [
        "proxy/private",
        "private@",
        "private-",
        "workspaceId",
        "credentialFingerprint",
        "nextCreditId",
        "\"auth\"",
    ] {
        assert!(!result.to_string().contains(private), "{private}");
    }
    assert_eq!(
        fixture.call(json!({})).await["structuredContent"]["sources"],
        output["sources"]
    );
    let only_sources = Fixture::with_options(
        json!({"providers":[]}),
        Mode::Success,
        None,
        Options {
            sources: json!([external_source()]),
            ..Default::default()
        },
    )
    .await;
    let result = only_sources.call(json!({})).await;
    assert_eq!(result["structuredContent"]["providers"], json!([]));
    assert_eq!(
        result["structuredContent"]["sources"]["items"][0]["accounts"][0]["plan"],
        "Pro"
    );
}

#[tokio::test]
async fn models_usage_collects_sources_first_and_all_values_with_provider_updates() {
    let fixture = Fixture::with_options(json!({}), Mode::Success, None, Options {
        events: Some(vec![
            json!({"version":1,"type":"usageLimitSourcesUpdated","payload":{"sources":[]}}),
        ]), following_events: Some(vec![
            json!({"version":1,"type":"snapshot","config":{"providers":[provider("before")]}}),
            json!({"version":1,"type":"providerStatuses","payload":{"providers":[with_usage("after",json!({"checkedAt":CHECKED,"windows":[window(25.0)]}))]}}),
        ]), ..Default::default()
    }).await;
    let result = fixture.call(json!({})).await;
    assert_ne!(result["isError"], true, "{result}");
    assert_eq!(
        result["structuredContent"]["providers"][0]["instanceId"],
        "after"
    );
    assert_eq!(
        result["structuredContent"]["providers"][0]["usage"]["windows"][0]["remainingPercent"],
        75.0
    );
    assert_eq!(
        result["structuredContent"]["sources"],
        json!({"status":"reported","items":[]})
    );
}

#[tokio::test]
async fn models_usage_distinguishes_missing_sources_from_explicit_empty_sources() {
    for (mode, reason) in [
        (Mode::AfterEnd, "streamEnded"),
        (Mode::AfterDisconnect, "disconnected"),
    ] {
        let fixture = Fixture::new(json!({"providers":[]}), mode, None).await;
        let result = fixture.call(json!({})).await;
        assert_ne!(result["isError"], true, "{result}");
        assert_eq!(
            result["structuredContent"]["sources"],
            json!({"status":"unavailable","reason":reason})
        );
    }
    let fixture = Fixture::new(json!({"providers":[]}), Mode::Success, None).await;
    assert_eq!(
        fixture.call(json!({})).await["structuredContent"]["sources"],
        json!({"status":"reported","items":[]})
    );
}

#[tokio::test]
async fn usage_limits_deadline_returns_partial_but_explicit_cancellation_propagates() {
    let fixture = Fixture::new(
        json!({"providers":[provider("native")]}),
        Mode::AfterTimeout,
        None,
    )
    .await;
    let result = fixture
        .client
        .get_usage_limits(false, None)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        result.sources,
        json!({"status":"unavailable","reason":"timeout"})
    );
    let other = Fixture::new(
        json!({"providers":[provider("native")]}),
        Mode::AfterTimeout,
        None,
    )
    .await;
    let cancel = tokio_util::sync::CancellationToken::new();
    let read = other.client.get_usage_limits(false, Some(&cancel));
    let abort = async {
        other.snapshot_ack.notified().await;
        cancel.cancel();
    };
    let (result, _) = tokio::join!(read, abort);
    assert!(matches!(result, Err(error) if error.kind == ErrorKind::Cancelled));
    let absent = Fixture::with_options(
        json!({}),
        Mode::AfterTimeout,
        None,
        Options {
            events: Some(vec![]),
            ..Default::default()
        },
    )
    .await;
    assert!(absent.client.get_usage_limits(false, None).await.is_err());
}

#[tokio::test]
async fn models_usage_does_not_hide_errors_after_provider_snapshot() {
    for mode in [Mode::AfterRpc, Mode::AfterDefect, Mode::AfterMalformed] {
        let fixture = Fixture::new(json!({"providers":[]}), mode, None).await;
        let result = fixture.call(json!({})).await;
        assert_eq!(result["isError"], true, "{result}");
        assert_eq!(
            result["structuredContent"]["error"]["code"],
            "gateway_error"
        );
    }
}

#[tokio::test]
async fn models_usage_refreshes_once_and_preserves_sanitized_failure() {
    for error in [
        None,
        Some("token=private-token https://private.example/secret"),
    ] {
        let fixture = Fixture::with_options(
            json!({"providers":[]}),
            Mode::Success,
            None,
            Options {
                refresh_error: error.map(str::to_owned),
                ..Default::default()
            },
        )
        .await;
        let result = fixture.call(json!({"refresh":true})).await;
        assert_ne!(result["isError"], true, "{result}");
        assert_eq!(
            result["structuredContent"]["refresh"]["status"],
            if error.is_some() {
                "failed"
            } else {
                "completed"
            }
        );
        assert_eq!(
            result["structuredContent"]["sources"],
            json!({"status":"reported","items":[]})
        );
        assert!(!result.to_string().contains("private-"));
        if error.is_some() {
            assert!(
                result["structuredContent"]["refresh"]["error"]
                    .as_str()
                    .unwrap()
                    .contains("[REDACTED]")
            );
        }
        assert_eq!(
            fixture
                .calls
                .lock()
                .unwrap()
                .iter()
                .map(|call| call["tag"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>(),
            ["server.refreshProviders", "subscribeServerConfig"]
        );
        assert!(
            fixture
                .audit()
                .contains("\"operation\":\"server.refreshProviders\"")
        );
    }
}

#[tokio::test]
async fn models_usage_rejects_malformed_external_source_boundaries() {
    let valid = json!([external_source()]);
    let mut invalid = vec![Value::Null, json!([null]), json!([[]])];
    for (pointer, value) in [
        ("/0/accounts/0", json!(["account", "codex"])),
        ("/0/accounts/0/usageLimits", json!([CHECKED, []])),
        (
            "/0/accounts/0/usageLimits/windows/0",
            json!(["week", "weekly", "Weekly", 18]),
        ),
        (
            "/0/accounts/0/usageLimits/unavailable",
            json!(["probeFailed"]),
        ),
        ("/0/accounts/0/usageLimits/resetCredits", json!([1])),
        (
            "/0/accounts/0/usageLimits/windows/0/usedPercent",
            json!(101),
        ),
        ("/0/checkedAt", json!("yesterday")),
        ("/0/accounts/0/usageLimits", Value::Null),
    ] {
        let mut sources = valid.clone();
        *sources.pointer_mut(pointer).unwrap() = value;
        invalid.push(sources);
    }
    for sources in invalid {
        let fixture = Fixture::with_options(json!({}), Mode::Success, None, Options {
            events: Some(vec![json!({"version":1,"type":"snapshot","config":{"providers":[]}}),
                json!({"version":1,"type":"usageLimitSourcesUpdated","payload":{"sources":sources}})]),
            ..Default::default()
        }).await;
        let result = fixture.call(json!({})).await;
        assert_eq!(result["isError"], true, "{result}");
        assert!(
            result["structuredContent"]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("invalid response")
        );
    }
}

#[tokio::test]
async fn models_usage_sanitizes_permitted_labels_and_plans() {
    let private = "token=private-value https://private.example/secret";
    let mut native = provider("native");
    native["auth"] = json!({"label":private});
    let mut source = external_source();
    source["label"] = json!(private);
    source["accounts"][0]["plan"] = json!(private);
    let fixture = Fixture::with_options(
        json!({"providers":[native]}),
        Mode::Success,
        None,
        Options {
            sources: json!([source]),
            ..Default::default()
        },
    )
    .await;
    let result = fixture.call(json!({})).await;
    assert_ne!(result["isError"], true, "{result}");
    let output = &result["structuredContent"];
    for label in [
        &output["providers"][0]["accountLabel"],
        &output["sources"]["items"][0]["label"],
        &output["sources"]["items"][0]["accounts"][0]["plan"],
    ] {
        assert!(label.as_str().unwrap().contains("[REDACTED]"));
    }
    assert!(!result.to_string().contains("private-"));
}

#[tokio::test]
async fn models_usage_keeps_cached_snapshots_after_refresh_timeout() {
    let fixture = Fixture::with_options(
        json!({"providers":[provider("cached")]}),
        Mode::Success,
        None,
        Options {
            refresh_stall: true,
            ..Default::default()
        },
    )
    .await;
    let result = fixture.client.get_usage_limits(true, None).await.unwrap();
    assert_eq!(result.refresh["status"], "failed");
    assert_eq!(result.providers.len(), 1);
    assert_eq!(result.sources, json!({"status":"reported","items":[]}));
    assert_eq!(
        fixture
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call["tag"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        ["server.refreshProviders", "subscribeServerConfig"]
    );
}
