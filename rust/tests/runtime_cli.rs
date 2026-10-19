#[path = "../src/t3/test_server.rs"]
pub mod test_server;

use futures_util::FutureExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    sync::Arc,
    time::Duration,
};
use test_server::{Reply, Server};

struct Fixture {
    server: Server,
    directory: PathBuf,
    mutation_release: Arc<tokio::sync::Notify>,
}

impl Fixture {
    async fn new(descriptor_delay: Duration) -> Self {
        let mutation_release = Arc::new(tokio::sync::Notify::new());
        let release = Arc::clone(&mutation_release);
        let server = Server::start(move |request| {
            let release = Arc::clone(&release);
            async move {
            match request.path.as_str() {
                "/.well-known/t3/environment" => {
                    tokio::time::sleep(descriptor_delay).await;
                    Reply::json(json!({"environmentId":"fixture-environment","label":"Fixture","serverVersion":"fixture-1"}))
                },
                "/api/auth/session" => Reply::json(json!({"authenticated":true,"scopes":["orchestration:read","orchestration:operate"]})),
                "/api/orchestration/shell" => Reply::json(json!({"snapshotSequence":1,"projects":[],"threads":[],"updatedAt":"2026-10-18T00:00:00.000Z"})),
                "/mcp" if request.method == "GET" => Reply::status(405,Value::Null),
                "/mcp" if request.body["method"] == "initialize" => Reply::json(json!({
                    "jsonrpc":"2.0","id":request.body["id"],"result":{
                        "protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"runtime-fixture","version":"1"},
                    },
                })),
                "/mcp" if request.body.get("id").is_none() => Reply::status(202,Value::Null),
                "/mcp" if request.body["method"] == "tools/list" => Reply::json(json!({
                    "jsonrpc":"2.0","id":request.body["id"],"result":{"tools":[{
                        "name":"t3_thread_send","inputSchema":{"type":"object"},
                    }]},
                })),
                "/mcp" if request.body["method"] == "tools/call" => {
                    release.notified().await;
                    Reply::json(json!({"jsonrpc":"2.0","id":request.body["id"],"result":{
                        "content":[],"structuredContent":{"status":"accepted"},
                    }}))
                },
                _ => Reply::status(404,json!({"code":"not_found"})),
            }
        }.boxed()}).await;
        let directory =
            std::env::temp_dir().join(format!("t3-runtime-cli-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        Self {
            server,
            directory,
            mutation_release,
        }
    }

    fn command(&self, name: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_t3-code-mcp"));
        command.env_clear();
        for name in ["PATH", "SystemRoot", "WINDIR", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .current_dir(&self.directory)
            .arg(name)
            .env("T3_HTTP_BASE_URL", &self.server.url)
            .env("T3_ACCESS_TOKEN", "fixture-access")
            .env("T3_MCP_DATA_DIR", self.directory.join("data"))
            .env("MCP_BEARER_TOKEN", "fixture-bearer")
            .env("MCP_READ_ONLY", "true")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn audit(&self) -> Vec<Value> {
        fs::read_to_string(self.directory.join("data/audit.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

struct Process(Option<Child>);

impl Process {
    fn finish(mut self) -> Output {
        let child = self.0.as_mut().unwrap();
        let input = child.stdin.take();
        #[cfg(unix)]
        unsafe {
            libc::kill(child.id() as i32, libc::SIGTERM);
        }
        #[cfg(not(unix))]
        {
            child.kill().unwrap();
        }
        let started = std::time::Instant::now();
        while child.try_wait().unwrap().is_none() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "Gateway did not stop after shutdown signal"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(input);
        self.0.take().unwrap().wait_with_output().unwrap()
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

async fn output(mut command: Command) -> Output {
    tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap()
}

fn initialize() -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2024-10-07","capabilities":{},"clientInfo":{"name":"compat-test","version":"1"},
    }})
}

#[tokio::test]
async fn stdio_binary_matches_node_initialize_and_discovery_and_records_its_session() {
    let fixture = Fixture::new(Duration::ZERO).await;
    let mut command = fixture.command("serve");
    command.stdin(Stdio::piped());
    let child = tokio::task::spawn_blocking(move || {
        let mut child = command.spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        for request in [
            initialize(),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":3,"method":"ping"}),
        ] {
            writeln!(input, "{request}").unwrap();
        }
        drop(input);
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap();
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    assert!(child.stderr.is_empty());
    let responses = String::from_utf8(child.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    let baseline: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/node-compat-baseline.json"
    ))
    .unwrap();
    assert_eq!(responses.len(), 3);
    assert_eq!(
        *responses
            .iter()
            .find(|response| response["id"] == 1)
            .unwrap(),
        baseline["mcp"]["stdio"]["initialize"]
    );
    let tools = &responses
        .iter()
        .find(|response| response["id"] == 2)
        .unwrap()["result"]["tools"];
    assert_eq!(
        format!("{:x}", Sha256::digest(serde_json::to_vec(tools).unwrap())),
        baseline["mcp"]["stdio"]["toolsSha256"]
    );
    assert_eq!(
        responses
            .iter()
            .find(|response| response["id"] == 3)
            .unwrap()["result"],
        json!({})
    );
    let events = fixture.audit();
    assert!(
        events
            .iter()
            .any(|event| event["event"] == "stdio.session" && event["outcome"] == "started")
    );
    assert!(
        events
            .iter()
            .any(|event| event["event"] == "stdio.session" && event["outcome"] == "completed")
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "stdio.message")
            .count(),
        3
    );
}

#[tokio::test]
async fn stdio_binary_can_answer_ping_while_an_earlier_tool_waits_for_t3() {
    let fixture = Fixture::new(Duration::from_millis(100)).await;
    let mut command = fixture.command("serve");
    command.stdin(Stdio::piped());
    let child = tokio::task::spawn_blocking(move || {
        let mut child = command.spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        writeln!(input,"{}",json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"t3_connection_status","arguments":{}}})).unwrap();
        writeln!(input,"{}",json!({"jsonrpc":"2.0","id":3,"method":"ping"})).unwrap();
        drop(input);
        child.wait_with_output().unwrap()
    }).await.unwrap();
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    let responses = String::from_utf8(child.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["id"], 3);
    assert_eq!(
        responses[1]["result"]["structuredContent"]["connectionStatus"],
        "connected"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_binary_stops_on_sigterm_with_idle_open_stdin() {
    let fixture = Fixture::new(Duration::ZERO).await;
    let mut command = fixture.command("serve");
    command.stdin(Stdio::piped());
    let mut child = Process(Some(command.spawn().unwrap()));
    writeln!(
        child.0.as_mut().unwrap().stdin.as_mut().unwrap(),
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"ping"})
    )
    .unwrap();
    let started = tokio::time::Instant::now();
    loop {
        if fs::read_to_string(fixture.directory.join("data/audit.jsonl"))
            .is_ok_and(|events| events.contains("stdio.message"))
        {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "Stdio startup timed out"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let result = tokio::task::spawn_blocking(move || child.finish())
        .await
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        fixture
            .audit()
            .iter()
            .any(|event| event["event"] == "stdio.session" && event["outcome"] == "completed")
    );
}

#[tokio::test]
async fn diagnostic_commands_keep_node_exit_codes_and_do_not_dispatch_mutations() {
    let fixture = Fixture::new(Duration::ZERO).await;
    for command in ["status", "doctor", "smoke", "spike"] {
        let result = output(fixture.command(command)).await;
        let value: Value = serde_json::from_slice(&result.stdout).unwrap();
        if command == "doctor" {
            assert_eq!(result.status.code(), Some(1));
            assert!(result.stderr.is_empty());
            assert_eq!(value["ok"], false);
            let checks = value["checks"].as_array().unwrap();
            assert!(
                checks
                    .iter()
                    .any(|check| check["name"] == "t3_identity" && check["ok"] == true)
            );
            assert!(
                checks
                    .iter()
                    .any(|check| check["name"] == "native_mcp" && check["ok"] == false)
            );
            assert!(
                checks
                    .iter()
                    .any(|check| check["name"] == "mcp_discovery" && check["ok"] == false)
            );
            assert!(
                checks
                    .iter()
                    .any(|check| check["name"] == "overview_read" && check["ok"] == true)
            );
        } else {
            assert!(
                result.status.success(),
                "{command}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            let status = if command == "status" {
                &value
            } else {
                &value["status"]
            };
            assert_eq!(
                status["environment"]["environmentId"],
                "fixture-environment"
            );
            assert_eq!(status["connectionStatus"], "connected");
            if command != "status" {
                assert_eq!(value["overview"]["total"], 0);
            }
            if command == "smoke" {
                assert!(
                    String::from_utf8_lossy(&result.stderr).starts_with("Smoke ok: fingerprint=")
                );
            }
            if command == "spike" {
                assert!(
                    String::from_utf8_lossy(&result.stderr)
                        .starts_with("Read-only connection spike complete.")
                );
            }
        }
    }
    assert!(
        fixture
            .server
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.method == "GET")
    );
}

#[tokio::test]
async fn doctor_binary_compares_supplied_actual_host_inventory_without_mutations() {
    let declared = t3_code_mcp::contract::tool_names()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert!(declared.iter().any(|name| name == "t3_thread_read"));
    assert!(
        !declared
            .iter()
            .any(|name| name == "issue19_unexpected_host_tool")
    );
    let missing = declared
        .iter()
        .filter(|name| name.as_str() != "t3_thread_read")
        .cloned()
        .collect::<Vec<_>>();
    let mut unexpected = declared.clone();
    unexpected.push("issue19_unexpected_host_tool".into());
    let mut missing_and_unexpected = missing.clone();
    missing_and_unexpected.push("issue19_unexpected_host_tool".into());
    let mut reordered_duplicates = declared.clone();
    reordered_duplicates.reverse();
    reordered_duplicates.extend(["t3_thread_read".into(), "t3_connection_status".into()]);

    for (case, observed, host_ok, evidence) in [
        ("matching", declared, true, &[] as &[&str]),
        ("missing", missing, false, &["t3_thread_read"]),
        (
            "unexpected",
            unexpected,
            false,
            &["issue19_unexpected_host_tool"],
        ),
        (
            "missing_and_unexpected",
            missing_and_unexpected,
            false,
            &["t3_thread_read", "issue19_unexpected_host_tool"],
        ),
        ("reordered_duplicates", reordered_duplicates, true, &[]),
    ] {
        let fixture = Fixture::new(Duration::ZERO).await;
        let mut command = fixture.command("doctor");
        command.env(
            "MCP_HOST_TOOL_NAMES_JSON",
            serde_json::to_string(&observed).unwrap(),
        );
        let result = output(command).await;
        assert_eq!(result.status.code(), Some(1), "{case}");
        assert!(
            result.stderr.is_empty(),
            "{case}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let value: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(value["ok"], false, "{case}");
        let checks = value["checks"].as_array().unwrap();
        let host_checks = checks
            .iter()
            .filter(|check| check["name"] == "host_discovery")
            .collect::<Vec<_>>();
        assert_eq!(host_checks.len(), 1, "{case}");
        let host = host_checks[0];
        assert_eq!(host["ok"], host_ok, "{case}: {host}");
        for &name in evidence {
            assert!(
                host["detail"].as_str().unwrap().contains(name),
                "{case}: missing evidence for {name}: {host}"
            );
        }
        assert!(
            checks
                .iter()
                .any(|check| check["name"] == "native_mcp" && check["ok"] == false),
            "{case}"
        );
        assert!(
            checks
                .iter()
                .any(|check| check["name"] == "t3_identity" && check["ok"] == true),
            "{case}"
        );
        let mut manifest_names = value["manifest"]["hostDiscoveredOperations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|name| name.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        manifest_names.sort();
        manifest_names.dedup();
        let mut expected_names = observed;
        expected_names.sort();
        expected_names.dedup();
        assert_eq!(manifest_names, expected_names, "{case}");
        let requests = fixture.server.requests.lock().unwrap();
        assert!(!requests.is_empty(), "{case}");
        assert!(
            requests.iter().all(|request| request.method == "GET"),
            "{case}: {requests:?}"
        );
    }
}

#[tokio::test]
async fn cli_errors_preserve_command_gates_and_native_login_needs_no_access_token() {
    let fixture = Fixture::new(Duration::ZERO).await;
    let mut unknown = fixture.command("unknown");
    unknown.env_remove("T3_ACCESS_TOKEN");
    let result = output(unknown).await;
    assert_eq!(result.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(result.stderr).unwrap(),
        "T3_ACCESS_TOKEN must be set.\n"
    );
    for (command, flags, expected) in [
        (
            "serve",
            vec![("MCP_TRANSPORT", "unsupported")],
            "MCP_TRANSPORT must be stdio or http, received unsupported.",
        ),
        (
            "doctor",
            vec![("MCP_HOST_TOOL_NAMES_JSON", "{}")],
            "MCP_HOST_TOOL_NAMES_JSON must be a JSON array of non-empty tool names captured from the actual host.",
        ),
        (
            "spike",
            vec![("T3_SPIKE_ENABLE_MUTATIONS", "true")],
            "Mutation spike requires T3_SPIKE_CONFIRM=I_UNDERSTAND.",
        ),
        (
            "spike",
            vec![
                ("T3_SPIKE_ENABLE_MUTATIONS", "true"),
                ("T3_SPIKE_CONFIRM", "I_UNDERSTAND"),
            ],
            "Mutation spike requires T3_SPIKE_PROJECT_ID and T3_SPIKE_PROMPT.",
        ),
    ] {
        let mut process = fixture.command(command);
        process.envs(flags);
        let result = output(process).await;
        assert_eq!(result.status.code(), Some(1));
        assert_eq!(
            String::from_utf8(result.stderr).unwrap(),
            format!("{expected}\n")
        );
    }
    let mut login = fixture.command("native-login");
    login.env_remove("T3_ACCESS_TOKEN");
    let result = output(login).await;
    assert_eq!(result.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(result.stderr).unwrap(),
        "No pairing code was supplied on stdin.\n"
    );
    assert!(
        fixture
            .server
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.method == "GET")
    );
}

#[tokio::test]
async fn http_binary_matches_node_protocol_auth_and_records_request_outcomes() {
    let fixture = Fixture::new(Duration::ZERO).await;
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    let url = format!("http://127.0.0.1:{port}");
    drop(reservation);
    let child = Process(Some(
        fixture
            .command("serve")
            .env("MCP_TRANSPORT", "http")
            .env("MCP_PORT", port.to_string())
            .spawn()
            .unwrap(),
    ));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let started = tokio::time::Instant::now();
    loop {
        if let Ok(response) = client.get(format!("{url}/healthz")).send().await {
            assert_eq!(response.status().as_u16(), 200);
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "HTTP startup timed out"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let unauthorized = client
        .post(format!("{url}/mcp"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status().as_u16(), 401);
    assert_eq!(unauthorized.headers()["www-authenticate"], "Bearer");
    let response = client
        .post(format!("{url}/mcp"))
        .bearer_auth("fixture-bearer")
        .header("accept", "application/json, text/event-stream")
        .json(&initialize())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert!(response.headers().get("mcp-session-id").is_some());
    let baseline: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/node-compat-baseline.json"
    ))
    .unwrap();
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        baseline["mcp"]["http"]["initialize"]["body"]
    );
    let response = client.post(format!("{url}/mcp")).bearer_auth("fixture-bearer")
        .header("accept","application/json, text/event-stream").json(&json!({
            "jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"t3_task_get","arguments":{"taskRef":"missing-task"}},
        })).send().await.unwrap().json::<Value>().await.unwrap();
    assert_eq!(response["result"]["isError"], true);
    assert_eq!(
        response["result"]["structuredContent"]["error"]["code"],
        "task_not_found"
    );
    let ended = tokio::task::spawn_blocking(move || child.finish())
        .await
        .unwrap();
    #[cfg(unix)]
    assert!(
        ended.status.success(),
        "{}",
        String::from_utf8_lossy(&ended.stderr)
    );
    assert!(ended.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&ended.stderr)
            .contains("t3-code-mcp listening on http://127.0.0.1:")
    );
    let events = fixture.audit();
    assert!(events.iter().any(|event| event["event"] == "http.listen"));
    assert!(events.iter().any(|event| event["event"] == "http.response"
        && event["details"]["status"] == 401
        && event["details"]["authOutcome"] == "rejected"));
    assert!(events.iter().any(|event| event["event"] == "http.response"
        && event["details"]["rpc"]["toolName"] == "t3_task_get"
        && event["details"]["authOutcome"] == "accepted"));
}

#[cfg(unix)]
#[tokio::test]
async fn http_shutdown_drains_an_accepted_native_mutation_and_records_its_result() {
    use t3_code_mcp::native_credential::{
        NativeAccessLevel, NativeCredential, NativeCredentialStore,
    };

    let fixture = Fixture::new(Duration::ZERO).await;
    NativeCredentialStore::in_data_dir(fixture.directory.join("data"))
        .write(&NativeCredential {
            resource: format!("{}/mcp", fixture.server.url),
            client_id: "fixture-client".into(),
            access_token: "fixture-native-access".into(),
            access: NativeAccessLevel::FullAccess,
            scopes: vec!["orchestration:operate".into()],
            obtained_at: "2026-10-18T00:00:00.000Z".into(),
            expires_at: "2099-10-18T00:00:00.000Z".into(),
        })
        .unwrap();
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let child = Process(Some(
        fixture
            .command("serve")
            .env("MCP_TRANSPORT", "http")
            .env("MCP_READ_ONLY", "false")
            .env("MCP_PORT", port.to_string())
            .spawn()
            .unwrap(),
    ));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let url = format!("http://127.0.0.1:{port}");
    let started = tokio::time::Instant::now();
    while client.get(format!("{url}/healthz")).send().await.is_err() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "HTTP startup timed out"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let sse = client
        .get(format!("{url}/mcp"))
        .bearer_auth("fixture-bearer")
        .header("accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(sse.status().as_u16(), 200);
    assert_eq!(sse.headers()["content-type"], "text/event-stream");
    let request = tokio::spawn(async move {
        client.post(format!("{url}/mcp")).bearer_auth("fixture-bearer")
            .header("accept", "application/json, text/event-stream")
            .json(&json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                "name":"t3_thread_send","arguments":{"threadId":"fixture-thread","text":"fixture instruction"},
            }})).send().await.unwrap().json::<Value>().await.unwrap()
    });
    let started = tokio::time::Instant::now();
    loop {
        if fixture
            .server
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.body["method"] == "tools/call")
        {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "Native mutation was not received"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let stopped = tokio::task::spawn_blocking(move || child.finish());
    let started = tokio::time::Instant::now();
    while tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_ok()
    {
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "HTTP listener did not close on shutdown"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        !stopped.is_finished(),
        "Gateway exited before the native reply was released"
    );
    assert!(sse.bytes().await.unwrap().is_empty());
    fixture.mutation_release.notify_one();
    assert_eq!(
        request.await.unwrap()["result"]["structuredContent"]["status"],
        "accepted"
    );
    let result = stopped.await.unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let events = fixture.audit();
    assert!(events.iter().any(|event| event["event"] == "tool.result"
        && event["operation"] == "t3_thread_send"
        && event["outcome"] == "completed"));
    assert!(events.iter().any(|event| event["event"] == "http.response"
        && event["details"]["rpc"]["toolName"] == "t3_thread_send"));
}
