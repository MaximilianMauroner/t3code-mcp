use super::{TIME, descriptor, shell_value};
use crate::{
    config::{GatewayConfig, NativeEndpointConfig},
    gateway::T3Gateway,
    mcp::{IncomingMessage, McpServer},
    t3::test_server::{Reply, Server},
};
use futures_util::FutureExt;
use serde_json::{Value, json};
use std::{collections::HashMap, path::PathBuf, sync::{Arc, Mutex}};

struct SettingsHost {
    default_model: Value,
    threads: HashMap<String, Value>,
    readable: bool,
}

struct SettingsFixture {
    gateway: Option<T3Gateway>,
    server: Server,
    host: Arc<Mutex<SettingsHost>>,
    directory: PathBuf,
}

impl SettingsFixture {
    async fn new() -> Self {
        let host = Arc::new(Mutex::new(SettingsHost {
            default_model: json!({"instanceId":"codex","model":"project-model"}),
            threads: HashMap::new(),
            readable: true,
        }));
        let source = Arc::clone(&host);
        let server = Server::start(move |request| {
            let source = Arc::clone(&source);
            async move {
                let mut host = source.lock().unwrap();
                match request.path.as_str() {
                    "/.well-known/t3/environment" => Reply::json(descriptor(1)),
                    "/api/auth/session" => Reply::json(json!({"authenticated":true,"scopes":["orchestration:read","orchestration:operate"]})),
                    "/api/orchestration/shell" => Reply::json(json!({
                        "snapshotSequence":10,"updatedAt":TIME,
                        "projects":[{"id":"project-1","title":"Settings project","workspaceRoot":"/missing-settings-fixture","defaultModelSelection":host.default_model}],
                        "threads":host.threads.values().collect::<Vec<_>>(),
                    })),
                    "/api/orchestration/dispatch" => {
                        let command = &request.body;
                        let id = command["threadId"].as_str().unwrap().to_owned();
                        match command["type"].as_str().unwrap() {
                            "thread.create" => {
                                let mut thread = shell_value();
                                for field in ["projectId","title","modelSelection","runtimeMode","interactionMode"] {
                                    thread[field] = command[field].clone();
                                }
                                thread["id"] = json!(id);
                                thread["session"] = json!({"status":"ready","updatedAt":TIME});
                                thread["messages"] = json!([]);
                                host.threads.insert(id, thread);
                            }
                            "thread.turn.start" => {
                                let thread = host.threads.get_mut(&id).unwrap();
                                if !command["modelSelection"].is_null() {
                                    thread["modelSelection"] = command["modelSelection"].clone();
                                }
                                thread["runtimeMode"] = command["runtimeMode"].clone();
                                let turn = format!("turn-{}", command["message"]["messageId"].as_str().unwrap());
                                let mut message = command["message"].clone();
                                message["id"] = command["message"]["messageId"].clone();
                                message["turnId"] = json!(turn);
                                thread["messages"] = json!([message]);
                                thread["latestTurn"] = json!({"turnId":turn,"state":"running","requestedAt":TIME,"startedAt":TIME});
                                thread["session"] = json!({"status":"running","updatedAt":TIME});
                            }
                            other => panic!("unexpected settings dispatch {other}"),
                        }
                        Reply::json(json!({"sequence":10}))
                    }
                    path if path.starts_with("/api/orchestration/threads/") => {
                        if !host.readable {
                            return Reply::status(503, json!({"code":"unavailable"}));
                        }
                        let id = path.rsplit('/').next().unwrap();
                        Reply::json(json!({"snapshotSequence":10,"thread":host.threads.get(id).unwrap()}))
                    }
                    _ => panic!("unexpected settings request {} {}", request.method, request.path),
                }
            }.boxed()
        }).await;
        let directory = std::env::temp_dir().join(format!("t3-retirement-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let gateway = T3Gateway::new(GatewayConfig {
            endpoint: NativeEndpointConfig {
                t3_http_base_url: server.url.clone(),
                t3_native_mcp_url: format!("{}/mcp", server.url),
                data_dir: directory.clone(),
            },
            t3_access_token: "settings-fixture-token".into(),
            mcp_bearer_token: None,
            read_only: false,
            host: "127.0.0.1".into(),
            port: 8787,
            max_body_bytes: 2 * 1024 * 1024,
            environment_id: Some("env".into()),
            environment_label: None,
            worktree_root: None,
            stale_after_ms: 30_000,
        });
        Self { gateway: Some(gateway), server, host, directory }
    }

    async fn call(&self, name: &str, arguments: Value) -> Value {
        let response = McpServer::gateway().handle_message(
            IncomingMessage::parse(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}})).unwrap(),
            self.gateway.as_ref().unwrap(),
        ).await.unwrap();
        assert!(response.get("error").is_none(), "{response}");
        let result = &response["result"];
        assert_ne!(result["isError"], true, "{result}");
        let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, result["structuredContent"]);
        text
    }

    fn restart(&mut self) {
        let config = self.gateway.as_ref().unwrap().config().clone();
        drop(self.gateway.take());
        self.gateway = Some(T3Gateway::new(config));
    }

    fn dispatches(&self) -> Vec<Value> {
        self.server.requests.lock().unwrap().iter()
            .filter(|request| request.path == "/api/orchestration/dispatch")
            .map(|request| request.body.clone()).collect()
    }

    fn stored_receipts(&self) -> Vec<Value> {
        let journal: Value = serde_json::from_slice(&std::fs::read(self.directory.join("operations.json")).unwrap()).unwrap();
        journal["operations"].as_array().unwrap().iter()
            .filter_map(|operation| operation.get("settings").cloned()).collect()
    }
}

impl Drop for SettingsFixture {
    fn drop(&mut self) {
        drop(self.gateway.take());
        std::fs::remove_dir_all(&self.directory).unwrap();
    }
}

fn task_arguments(key: &str) -> Value {
    json!({"projectId":"project-1","title":"Settings task","instruction":"Check configured settings.","runtimeMode":"approval-required","idempotencyKey":key})
}

fn assert_intent(settings: &Value, model: &Value, source: &str, requested_model: Value) {
    assert_eq!(settings["requested"], json!({"modelSelection":requested_model,"runtimeMode":"approval-required"}));
    assert_eq!(settings["resolved"], json!({"modelSelection":model,"runtimeMode":"approval-required"}));
    assert_eq!(settings["modelSource"], source);
    assert_eq!(settings["runtimeSource"], "explicit");
}

#[tokio::test]
async fn explicit_settings_drive_dispatch_and_survive_a_fresh_gateway_from_disk() {
    let mut fixture = SettingsFixture::new().await;
    let model = json!({"instanceId":"codex","model":"explicit-model"});
    let mut args = task_arguments("explicit-settings");
    args["modelSelection"] = json!({"instanceId":"codex","model":"explicit-model","options":{"secret":"settings-must-not-persist","reasoningEffort":"high"}});
    let started = fixture.call("t3_task_start", args.clone()).await;
    assert_eq!(started["stage"], "run_accepted");
    let sent = fixture.dispatches();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0]["type"], "thread.create");
    assert_eq!(sent[1]["type"], "thread.turn.start");
    for command in &sent {
        assert_eq!(command["modelSelection"]["model"], "explicit-model");
        assert_eq!(command["modelSelection"]["instanceId"], "codex");
        assert_eq!(command["runtimeMode"], "approval-required");
    }
    assert_intent(&started["settings"], &model, "explicit", model.clone());
    assert_eq!(started["settings"]["matchesResolved"], true);
    assert_eq!(started["settings"]["effective"]["modelSelection"], model);
    let stored = fixture.stored_receipts();
    assert_eq!(stored.len(), 2);
    for receipt in &stored {
        assert_intent(receipt, &model, "explicit", model.clone());
    }
    let bytes = std::fs::read_to_string(fixture.directory.join("operations.json")).unwrap();
    assert!(!bytes.contains("settings-must-not-persist"));
    assert!(!bytes.contains("reasoningEffort"));
    fixture.restart();
    let reopened = fixture.call("t3_task_get", json!({"taskRef":started["taskRef"]})).await;
    let reopened = &reopened["task"];
    assert_eq!(reopened["runId"], started["runId"]);
    assert_intent(&reopened["settings"], &model, "explicit", model.clone());
    assert_eq!(reopened["settings"]["state"], "observed");
    assert_eq!(reopened["settings"]["matchesResolved"], true);
    let retried = fixture.call("t3_task_start", args).await;
    assert_eq!(retried["taskRef"], started["taskRef"]);
    assert_eq!(retried["runId"], started["runId"]);
    assert_eq!(fixture.dispatches(), sent);
    assert_eq!(fixture.stored_receipts(), stored);
}

#[tokio::test]
async fn inherited_project_default_is_dispatched_and_retained_when_defaults_change_after_restart() {
    let mut fixture = SettingsFixture::new().await;
    let model = json!({"instanceId":"codex","model":"project-model"});
    let started = fixture.call("t3_task_start", task_arguments("inherited-settings")).await;
    assert_eq!(started["stage"], "run_accepted");
    let sent = fixture.dispatches();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0]["modelSelection"], model);
    assert!(sent[1]["modelSelection"].is_null());
    assert_eq!(sent[1]["runtimeMode"], "approval-required");
    assert_intent(&started["settings"], &model, "project_default", Value::Null);
    assert_eq!(started["settings"]["matchesResolved"], true);
    let stored = fixture.stored_receipts();
    assert_eq!(stored.len(), 2);
    fixture.host.lock().unwrap().default_model = json!({"instanceId":"codex","model":"new-project-model"});
    fixture.host.lock().unwrap().readable = false;
    fixture.restart();
    let unavailable = fixture.call("t3_task_get", json!({"taskRef":started["taskRef"]})).await;
    let unavailable = &unavailable["task"];
    assert_intent(&unavailable["settings"], &model, "project_default", Value::Null);
    assert_eq!(unavailable["settings"]["state"], "unresolved");
    assert!(unavailable["settings"]["effective"].is_null());
    assert!(unavailable["settings"]["matchesResolved"].is_null());
    fixture.host.lock().unwrap().readable = true;
    let observed = fixture.call("t3_task_get", json!({"taskRef":started["taskRef"]})).await;
    let observed = &observed["task"];
    assert_intent(&observed["settings"], &model, "project_default", Value::Null);
    assert_eq!(observed["settings"]["matchesResolved"], true);
    assert_eq!(observed["settings"]["effective"]["modelSelection"], model);
    assert_eq!(fixture.dispatches(), sent);
    assert_eq!(fixture.stored_receipts(), stored);
}

#[tokio::test]
async fn observed_model_and_runtime_mismatches_do_not_rewrite_durable_intent_or_dispatch() {
    for (field, changed) in [
        ("modelSelection", json!({"instanceId":"codex","model":"observed-other-model"})),
        ("runtimeMode", json!("full-access")),
    ] {
        let mut fixture = SettingsFixture::new().await;
        let model = json!({"instanceId":"codex","model":"explicit-model"});
        let mut args = task_arguments(&format!("mismatch-{field}"));
        args["modelSelection"] = model.clone();
        let started = fixture.call("t3_task_start", args).await;
        assert_eq!(started["settings"]["matchesResolved"], true);
        let stored = fixture.stored_receipts();
        let sent = fixture.dispatches();
        fixture.host.lock().unwrap().threads.get_mut(started["threadId"].as_str().unwrap()).unwrap()[field] = changed.clone();
        fixture.restart();
        let mismatch = fixture.call("t3_task_get", json!({"taskRef":started["taskRef"]})).await;
        let mismatch = &mismatch["task"];
        assert_intent(&mismatch["settings"], &model, "explicit", model.clone());
        assert_eq!(mismatch["settings"]["state"], "observed");
        assert_eq!(mismatch["settings"]["matchesResolved"], false, "{field}");
        assert_eq!(mismatch["settings"]["effective"][field], changed);
        assert!(crate::t3::timestamp(mismatch["settings"]["effective"]["observedAt"].as_str()).is_some());
        assert_eq!(mismatch["runtimeMode"], "approval-required");
        assert_eq!(fixture.stored_receipts(), stored);
        assert_eq!(fixture.dispatches(), sent);
        fixture.restart();
        let persisted = fixture.call("t3_task_get", json!({"taskRef":started["taskRef"]})).await;
        let persisted = &persisted["task"];
        assert_intent(&persisted["settings"], &model, "explicit", model.clone());
        assert_eq!(persisted["settings"]["matchesResolved"], false);
        assert_eq!(fixture.dispatches(), sent);
    }
}
