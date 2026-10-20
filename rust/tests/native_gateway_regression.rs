#[path = "../src/t3/test_server.rs"]
pub mod test_server;

use futures_util::FutureExt;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU16, Ordering},
    },
};
use t3_code_mcp::{
    config::{GatewayConfig, load_config_from},
    contract::{gateway_tools, native_called_tool_names},
    doctor::run_doctor,
    gateway::T3Gateway,
    mcp::{IncomingMessage, McpServer},
    native_credential::{NativeAccessLevel, NativeCredential, NativeCredentialStore},
};
use test_server::{Reply, Server};

struct Fixture {
    server: Server,
    directory: PathBuf,
    catalog_status: Arc<AtomicU16>,
    upload_status: Arc<AtomicU16>,
}

impl Fixture {
    async fn new(missing: Option<&str>) -> Self {
        let catalog_status = Arc::new(AtomicU16::new(200));
        let upload_status = Arc::new(AtomicU16::new(204));
        let catalog = catalog_status.clone();
        let upload = upload_status.clone();
        let tools = native_called_tool_names()
            .filter(|name| Some(*name) != missing)
            .map(|name| json!({"name":name,"inputSchema":{"type":"object"}}))
            .collect::<Vec<_>>();
        let server = Server::start(move |r| {
            let tools = tools.clone();
            let catalog = catalog.clone();
            let upload = upload.clone();
            async move {
                match r.path.as_str() {
                    "/.well-known/t3/environment" => Reply::json(json!({
                        "environmentId":"fixture","label":"Fixture","serverVersion":"1",
                    })),
                    "/api/auth/session" => Reply::json(json!({
                        "authenticated":true,"scopes":["orchestration:read","orchestration:operate"],
                    })),
                    "/api/orchestration/shell" => Reply::json(json!({
                        "snapshotSequence":1,"projects":[],"threads":[],"updatedAt":"2026-10-19T00:00:00Z",
                    })),
                    path if path.starts_with("/upload?") => {
                        let status = upload.load(Ordering::SeqCst);
                        if status == 0 {
                            Reply { drop_response: true, ..Reply::json(Value::Null) }
                        } else {
                            Reply::status(status, json!({"error":"upload refused"}))
                        }
                    }
                    "/mcp" if r.method == "GET" => Reply::status(405, Value::Null),
                    "/mcp" if r.method == "DELETE" => Reply::json(Value::Null),
                    "/mcp" if r.body["method"] == "initialize" => Reply::json(json!({
                        "jsonrpc":"2.0","id":r.body["id"],"result":{
                            "protocolVersion":"2025-11-25","capabilities":{},
                            "serverInfo":{"name":"regression-fixture","version":"1"},
                        },
                    })),
                    "/mcp" if r.body.get("id").is_none() => Reply::status(202, Value::Null),
                    "/mcp" if r.body["method"] == "tools/list" => {
                        let status = catalog.load(Ordering::SeqCst);
                        if status != 200 {
                            Reply::status(status, json!({"error":"catalog refused"}))
                        } else {
                            Reply::json(json!({"jsonrpc":"2.0","id":r.body["id"],"result":{"tools":tools}}))
                        }
                    }
                    "/mcp" if r.body["method"] == "tools/call" => {
                        let prepared = r.body["params"]["name"] == "t3_attachment_prepare_upload";
                        Reply::json(json!({"jsonrpc":"2.0","id":r.body["id"],"result":{
                            "content":[],"structuredContent":if prepared {
                                json!({"attachmentId":"fixture-attachment","relativeUrl":"/upload?signed=private-url-marker"})
                            } else { json!({}) },
                        }}))
                    }
                    _ => Reply::status(404, Value::Null),
                }
            }.boxed()
        }).await;
        let directory =
            std::env::temp_dir().join(format!("t3-native-gateway-{}", uuid::Uuid::new_v4()));
        let fixture = Self {
            server,
            directory,
            catalog_status,
            upload_status,
        };
        fixture.write_credential("initial-token");
        fixture
    }

    fn write_credential(&self, token: &str) {
        NativeCredentialStore::in_data_dir(&self.directory)
            .write(&NativeCredential {
                resource: format!("{}/mcp", self.server.url),
                client_id: "fixture".into(),
                access_token: token.into(),
                access: NativeAccessLevel::FullAccess,
                scopes: vec!["orchestration:operate".into()],
                obtained_at: "2026-10-19T00:00:00Z".into(),
                expires_at: "2099-10-19T00:00:00Z".into(),
            })
            .unwrap();
    }

    fn config(&self) -> GatewayConfig {
        load_config_from(
            &HashMap::from([
                ("T3_HTTP_BASE_URL".into(), self.server.url.clone()),
                ("T3_ACCESS_TOKEN".into(), "fixture".into()),
                (
                    "T3_MCP_DATA_DIR".into(),
                    self.directory.to_string_lossy().into_owned(),
                ),
                ("MCP_READ_ONLY".into(), "false".into()),
            ]),
            &self.directory,
        )
        .unwrap()
    }

    fn audit_text(&self) -> String {
        std::fs::read_to_string(self.directory.join("audit.jsonl")).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn disabled<'a>(status: &'a Value, operation: &str) -> &'a Value {
    status["disabledOperations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["operation"] == operation)
        .expect("operation must be disabled")
}

async fn upload(gateway: &T3Gateway) -> Value {
    McpServer::gateway()
        .handle_message(
            IncomingMessage::parse(json!({
                "jsonrpc":"2.0","id":1,"method":"tools/call","params":{
                    "name":"t3_attachment_upload","arguments":{
                        "name":"fixture.txt","mimeType":"text/plain","dataBase64":"aGVsbG8=",
                    },
                },
            }))
            .unwrap(),
            gateway,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn native_backed_status_and_doctor_use_required_catalog_tools() {
    for missing in [
        None,
        Some("t3_attachment_prepare_upload"),
        Some("t3_thread_interrupt"),
    ] {
        let fixture = Fixture::new(missing).await;
        let config = fixture.config();
        let gateway = T3Gateway::new(config.clone());
        let status = gateway.connection_status().await;
        for (operation, required) in [
            ("t3_attachment_upload", "t3_attachment_prepare_upload"),
            ("t3_task_cancel", "t3_thread_interrupt"),
        ] {
            let callable = status["callableOperations"]
                .as_array()
                .unwrap()
                .contains(&json!(operation));
            assert_eq!(callable, missing != Some(required), "{operation}: {status}");
            if missing == Some(required) {
                assert_eq!(
                    disabled(&status, operation),
                    &json!({
                        "operation":operation,"reasonCode":"native_unavailable",
                        "reason":format!("Native T3 does not offer {required}, which {operation} needs."),
                    })
                );
            }
        }
        let doctor = run_doctor(&gateway, &config, None, Some(&fixture.server.url)).await;
        assert_eq!(
            doctor["manifest"]["callableOperations"],
            status["callableOperations"]
        );
    }
}

#[tokio::test]
async fn failed_catalog_refresh_disables_native_backed_operations() {
    let fixture = Fixture::new(None).await;
    let gateway = T3Gateway::new(fixture.config());
    let before = gateway.connection_status().await;
    assert!(before["native"]["error"].is_null());
    fixture.catalog_status.store(500, Ordering::SeqCst);
    fixture.write_credential("rotated-token");
    let status = gateway.connection_status().await;
    assert!(status["native"]["availableTools"].as_u64().unwrap() > 0);
    assert!(status["native"]["error"].is_string());
    for operation in ["t3_attachment_upload", "t3_task_cancel"] {
        let blocker = disabled(&status, operation);
        assert_eq!(blocker["reasonCode"], "native_unavailable");
        assert_eq!(blocker["reason"], status["native"]["error"]);
    }
    assert!(
        status["callableOperations"]
            .as_array()
            .unwrap()
            .contains(&json!("t3_thread_read"))
    );
}

#[tokio::test]
async fn rejected_catalog_refresh_disables_native_operations() {
    let fixture = Fixture::new(None).await;
    let gateway = T3Gateway::new(fixture.config());
    let before = gateway.connection_status().await;
    assert!(
        before["callableOperations"]
            .as_array()
            .unwrap()
            .contains(&json!("t3_thread_read"))
    );
    fixture.catalog_status.store(401, Ordering::SeqCst);
    fixture.write_credential("rejected-token");
    let status = gateway.connection_status().await;
    assert_eq!(status["native"]["credential"], "valid");
    assert!(status["native"]["availableTools"].as_u64().unwrap() > 0);
    assert_eq!(
        disabled(&status, "t3_thread_read")["reasonCode"],
        "native_unavailable"
    );
    let schema = &gateway_tools()
        .iter()
        .find(|tool| tool.name == "t3_connection_status")
        .unwrap()
        .output_schema["properties"]["native"]["properties"];
    for key in status["native"].as_object().unwrap().keys() {
        assert!(
            schema.get(key).is_some(),
            "native.{key} is not in the output schema"
        );
    }
    assert_eq!(
        disabled(&status, "t3_thread_read")["reason"],
        status["native"]["error"]
    );
}

#[tokio::test]
async fn attachment_transport_errors_omit_signed_url_from_mcp_and_audit() {
    let fixture = Fixture::new(None).await;
    fixture.upload_status.store(0, Ordering::SeqCst);
    let gateway = T3Gateway::new(fixture.config());
    let response = upload(&gateway).await;
    assert_eq!(response["result"]["isError"], true, "{response}");
    let message = response["result"]["content"][0]["text"].as_str().unwrap();
    assert!(message.contains("Attachment upload failed:"), "{message}");
    assert!(!message.contains("private-url-marker"), "{message}");
    assert!(!message.contains(" for url ("), "{message}");
    assert!(!fixture.audit_text().contains("private-url-marker"));
}

#[tokio::test]
async fn attachment_upload_audits_every_transport_outcome() {
    for status in [204, 400, 302, 0] {
        let fixture = Fixture::new(None).await;
        fixture.upload_status.store(status, Ordering::SeqCst);
        let gateway = T3Gateway::new(fixture.config());
        let response = upload(&gateway).await;
        assert_eq!(
            response["result"]["isError"] == true,
            status != 204,
            "{response}"
        );
        let events = fixture
            .audit_text()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|event| event["operation"] == "attachment.upload")
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 2, "HTTP {status}: {events:?}");
        assert_eq!(events[0]["source"], "t3");
        assert_eq!(events[0]["event"], "upstream.request");
        assert_eq!(events[0]["outcome"], "started");
        assert_eq!(
            events[0]["details"],
            json!({"sizeBytes":5,"mimeType":"text/plain"})
        );
        assert_eq!(events[1]["event"], "upstream.response");
        assert_eq!(
            events[1]["outcome"],
            if status == 204 { "completed" } else { "error" }
        );
        assert_eq!(
            events[1]["details"]["status"],
            if status == 0 {
                Value::Null
            } else {
                json!(status)
            }
        );
        assert!(
            events[1]["durationMs"]
                .as_f64()
                .is_some_and(|duration| duration >= 0.0)
        );
        assert_eq!(events[0]["correlationId"], events[1]["correlationId"]);
        assert_eq!(events[0]["parentOperation"], "t3_attachment_upload");
        assert_eq!(
            events[1]["parentCorrelationId"],
            events[0]["parentCorrelationId"]
        );
    }
}
