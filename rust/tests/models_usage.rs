use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use t3_code_mcp::{
    config::load_config_from,
    gateway::T3Gateway,
    mcp::{IncomingMessage, McpServer},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
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
}

struct Fixture {
    config: Arc<Mutex<Value>>,
    paths: Arc<Mutex<Vec<String>>>,
    calls: Arc<Mutex<Vec<Value>>>,
    directory: PathBuf,
    gateway: T3Gateway,
    task: JoinHandle<()>,
}

impl Fixture {
    async fn new(config: Value, mode: Mode, environment: Option<&str>) -> Self {
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
                        let (data, recorded, requests) = (data.clone(), recorded.clone(), requests.clone());
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
                            let exit = if matches!(mode, Mode::Rpc) {
                                json!({"_tag":"Failure","cause":[{"_tag":"Fail","error":{"message":"requires orchestration:read"}}]})
                            } else { json!({"_tag":"Success","value":data.lock().unwrap().clone()}) };
                            let response = json!({"_tag":"Exit","requestId":request["id"],"exit":exit});
                            let _ = ws.send(Message::Text(response.to_string().into())).await;
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
            ("T3_HTTP_BASE_URL".into(), url),
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
        let gateway = T3Gateway::new(load_config_from(&env, &directory).unwrap());
        Self {
            config,
            paths,
            calls,
            directory,
            gateway,
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
    assert_eq!(calls[0]["tag"], "server.getConfig");
    assert_eq!(calls[0]["payload"], json!({}));
    assert_eq!(
        *fixture.paths.lock().unwrap(),
        ["/.well-known/t3/environment", "/ws?orchestrationProtocol=2"]
    );
    assert!(
        fixture
            .audit()
            .contains("\"operation\":\"server.getConfig\"")
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
    let mut invalid = vec![json!([[]])];
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
