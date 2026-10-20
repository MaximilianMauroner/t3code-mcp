use super::*;
use crate::{
    config::{GatewayConfig, NativeEndpointConfig},
    t3::test_server::{Reply, Server},
};
use futures_util::FutureExt;
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

const RUN: &str = "run_<UUID>";
const TASK: &str = "task_<UUID>";
const TURN: &str = "turn-user:msg_<ID>";

fn baseline() -> Value {
    serde_json::from_str(include_str!(
        "../../../../tests/fixtures/node-compat-baseline.json"
    ))
    .unwrap()
}

fn thread() -> Value {
    let source = &baseline()["t3"]["task"]["thread"];
    let observed_at = now_iso();
    json!({
        "id": source["id"],
        "projectId": source["projectId"],
        "title": source["title"],
        "modelSelection": source["modelSelection"],
        "runtimeMode": source["runtimeMode"],
        "interactionMode": source["interactionMode"],
        "createdAt": observed_at,
        "updatedAt": observed_at,
        "latestTurn": {
            "turnId": TURN, "state": "running",
            "requestedAt": observed_at, "startedAt": observed_at,
            "completedAt": null, "assistantMessageId": null,
        },
        "session": { "status": "running" },
        "messages": [{"id": "user:msg_<ID>", "role": "user", "text": "instruction", "turnId": TURN}],
    })
}

struct Fixture {
    gateway: T3Gateway,
    server: Server,
    reply: Arc<Mutex<Reply>>,
    shell_threads: Arc<Mutex<Vec<Value>>>,
    shell_sequence: Arc<AtomicU64>,
    dispatch_plan: Arc<Mutex<VecDeque<(bool, Reply)>>>,
    directory: PathBuf,
}

impl Fixture {
    async fn new() -> Self {
        let value = thread();
        let reply = Arc::new(Mutex::new(Reply::json(
            json!({"snapshotSequence":3,"thread":value}),
        )));
        let thread_reply = Arc::clone(&reply);
        let shell_threads = Arc::new(Mutex::new(vec![value]));
        let threads = Arc::clone(&shell_threads);
        let shell_sequence = Arc::new(AtomicU64::new(3));
        let sequence = Arc::clone(&shell_sequence);
        let dispatch_plan = Arc::new(Mutex::new(VecDeque::<(bool, Reply)>::new()));
        let plan = Arc::clone(&dispatch_plan);
        let server = Server::start(move |request| {
            let reply = Arc::clone(&thread_reply);
            let threads = Arc::clone(&threads);
            let plan = Arc::clone(&plan);
            let sequence = Arc::clone(&sequence);
            async move {
                match request.path.as_str() {
                    "/.well-known/t3/environment" => Reply::json(json!({
                        "environmentId":"environment-test", "label":"Test", "serverVersion":"test",
                    })),
                    "/api/auth/session" => Reply::json(json!({"authenticated":true,"scopes":["orchestration:read","orchestration:operate"]})),
                    "/mcp" if request.method == "GET" => Reply::status(405,Value::Null),
                    "/mcp" if request.body["method"] == "initialize" => Reply::json(json!({
                        "jsonrpc":"2.0", "id":request.body["id"],
                        "result":{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"task-fixture","version":"1"}},
                    })),
                    "/mcp" if request.body.get("id").is_none() => Reply::status(202,Value::Null),
                    "/mcp" if request.body["method"] == "tools/list" => Reply::json(json!({
                        "jsonrpc":"2.0", "id":request.body["id"],
                        "result":{"tools":[
                            {"name":"t3_thread_list","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":false,"title":"Read threads"}},
                            {"name":"t3_thread_send","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":true}},
                            {"name":"t3_thread_interrupt","inputSchema":{"type":"object"}},
                        ]},
                    })),
                    "/mcp" if request.body["method"] == "tools/call" => Reply::json(json!({
                        "jsonrpc":"2.0", "id":request.body["id"],
                        "result":{"content":[],"structuredContent":{"status":"interrupt_requested"}},
                    })),
                    "/api/orchestration/shell" => Reply::json(json!({
                        "snapshotSequence":sequence.load(Ordering::SeqCst),
                        "projects":[{"id":"project-baseline","title":"Baseline project","workspaceRoot":"/missing-fixture", "defaultModelSelection":{"instanceId":"codex_openai","model":"gpt-5.3-codex-spark"}}],
                        "threads":threads.lock().unwrap().clone(),
                        "updatedAt":"2026-10-18T00:00:00.000Z",
                    })),
                    "/api/orchestration/dispatch" => {
                        let (apply, response) = plan.lock().unwrap().pop_front()
                            .unwrap_or((true, Reply::json(json!({"sequence":3}))));
                        if apply {
                            let command = &request.body;
                            let mut values = threads.lock().unwrap();
                            let mut value = if command["type"] == "thread.create" {
                                json!({
                                    "id": command["threadId"], "projectId": command["projectId"], "title":command["title"],
                                    "modelSelection":command["modelSelection"], "runtimeMode":command["runtimeMode"],
                                    "interactionMode":command["interactionMode"], "branch":command["branch"], "worktreePath":command["worktreePath"],
                                    "createdAt":command["createdAt"], "updatedAt":command["createdAt"], "messages":[],
                                })
                            } else {
                                values[0].clone()
                            };
                            if command["type"] == "thread.turn.start" {
                                let turn = format!("turn-{}", command["message"]["messageId"].as_str().unwrap());
                                value["latestTurn"] = json!({
                                    "turnId":turn, "state":"running", "requestedAt":command["createdAt"],
                                    "startedAt":command["createdAt"], "completedAt":null, "assistantMessageId":null,
                                });
                                value["session"] = json!({"status":"running"});
                                value["messages"] = json!([{
                                    "id":command["message"]["messageId"], "role":"user", "text":command["message"]["text"], "turnId":turn,
                                }]);
                            }
                            *values = vec![value.clone()];
                            *reply.lock().unwrap() = Reply::json(json!({"snapshotSequence":3,"thread":value}));
                        }
                        response
                    },
                    _ => reply.lock().unwrap().clone(),
                }
            }.boxed()
        }).await;
        let directory =
            std::env::temp_dir().join(format!("t3-task-parity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let mut journal = baseline()["files"]["operations"].clone();
        journal["operations"][0]["operationId"] = json!("op-create");
        journal["operations"][1]["operationId"] = json!("op-run");
        journal["tasks"][0]["threadOperationId"] = json!("op-create");
        journal["tasks"][0]["runOperationId"] = json!("op-run");
        let raw = journal
            .to_string()
            .replace("<ISO_TIME>", "2026-10-18T00:00:00.000Z");
        std::fs::write(directory.join("operations.json"), raw).unwrap();
        let gateway = T3Gateway::new(GatewayConfig {
            endpoint: NativeEndpointConfig {
                t3_http_base_url: server.url.clone(),
                t3_native_mcp_url: format!("{}/mcp", server.url),
                data_dir: directory.clone(),
            },
            t3_access_token: "fixture-token".into(),
            mcp_bearer_token: None,
            read_only: false,
            host: "127.0.0.1".into(),
            port: 0,
            max_body_bytes: 2 * 1024 * 1024,
            environment_id: None,
            environment_label: None,
            worktree_root: None,
            stale_after_ms: 30_000,
        });
        Self {
            gateway,
            server,
            reply,
            shell_threads,
            shell_sequence,
            dispatch_plan,
            directory,
        }
    }

    async fn empty() -> Self {
        let fixture = Self::new().await;
        std::fs::write(
            fixture.directory.join("operations.json"),
            "{\"operations\":[]}",
        )
        .unwrap();
        fixture.shell_threads.lock().unwrap().clear();
        fixture.missing(404, "thread_not_found", None);
        fixture
    }

    fn snapshot(&self, thread: Value, sequence: u64) {
        *self.reply.lock().unwrap() =
            Reply::json(json!({"snapshotSequence":sequence,"thread":thread}));
    }

    fn missing(&self, status: u16, code: &str, reason: Option<&str>) {
        let mut body = json!({"code":code,"message":"Target read failed"});
        if let Some(reason) = reason {
            body["reason"] = json!(reason);
        }
        *self.reply.lock().unwrap() = Reply::status(status, body);
    }

    fn authorize_native(&self) {
        use crate::native_credential::{NativeAccessLevel, NativeCredential};
        NativeCredentialStore::in_data_dir(&self.directory)
            .write(&NativeCredential {
                resource: format!("{}/mcp", self.server.url),
                client_id: "fixture-client".into(),
                access_token: "fixture-access-token".into(),
                access: NativeAccessLevel::FullAccess,
                scopes: vec!["orchestration:read".into(), "orchestration:operate".into()],
                obtained_at: "2026-10-18T00:00:00.000Z".into(),
                expires_at: "2099-10-18T00:00:00.000Z".into(),
            })
            .unwrap();
    }
}

fn start_args() -> Map<String, Value> {
    json!({
        "projectId":"project-baseline", "title":"Baseline task", "instruction":"Report status only.",
        "runtimeMode":"approval-required", "idempotencyKey":"baseline-task",
    }).as_object().unwrap().clone()
}

#[tokio::test]
async fn native_advertisement_preserves_upstream_hints() {
    let fixture = Fixture::new().await;
    fixture.authorize_native();
    let catalog = fixture.gateway.tool_catalog().await.unwrap();
    for (name, annotations) in [
        (
            "t3_thread_list",
            Some(json!({"readOnlyHint":false,"title":"Read threads"})),
        ),
        ("t3_thread_send", Some(json!({"readOnlyHint":true}))),
        ("t3_thread_interrupt", None),
    ] {
        let tool = catalog
            .tools()
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap();
        assert_eq!(tool.get("annotations"), annotations.as_ref());
    }
}

fn dispatch_types(fixture: &Fixture) -> Vec<String> {
    fixture
        .server
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.path == "/api/orchestration/dispatch")
        .map(|request| request.body["type"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn starting_and_retrying_a_task_matches_the_captured_node_contract() {
    let fixture = Fixture::empty().await;
    let started = fixture.gateway.task_start(&start_args()).await.unwrap();
    assert_eq!(started["stage"], "run_accepted");
    let retried = fixture.gateway.task_start(&start_args()).await.unwrap();
    for field in [
        "taskRef",
        "threadId",
        "runId",
        "messageId",
        "threadOperationId",
        "runOperationId",
    ] {
        assert_eq!(retried[field], started[field]);
    }
    assert_eq!(
        dispatch_types(&fixture),
        ["thread.create", "thread.turn.start"]
    );
    let mut serialized = started.to_string();
    for (field, placeholder) in [
        ("taskRef", "task_<UUID>"),
        ("threadId", "<UUID>"),
        ("runId", "run_<UUID>"),
        ("threadOperationId", "op_<UUID>"),
        ("runOperationId", "op_<UUID>"),
        ("messageId", "user:msg_<ID>"),
    ] {
        serialized = serialized.replace(started[field].as_str().unwrap(), placeholder);
    }
    let mut normalized: Value = serde_json::from_str(&serialized).unwrap();
    normalize_times(&mut normalized);
    assert_eq!(normalized, baseline()["t3"]["task"]);
    let stored = std::fs::read_to_string(fixture.directory.join("operations.json")).unwrap();
    assert!(!stored.contains("Report status only."));
}

#[tokio::test]
async fn concurrent_starts_dispatch_each_child_once_and_reject_a_changed_instruction() {
    let fixture = Fixture::empty().await;
    let args = start_args();
    let (first, second) = tokio::join!(
        fixture.gateway.task_start(&args),
        fixture.gateway.task_start(&args)
    );
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(first["taskRef"], second["taskRef"]);
    let final_state = fixture.gateway.task_start(&args).await.unwrap();
    assert_eq!(final_state["stage"], "run_accepted");
    assert_eq!(
        dispatch_types(&fixture),
        ["thread.create", "thread.turn.start"]
    );
    let mut changed = args;
    changed.insert("instruction".into(), json!("Different instruction"));
    assert_eq!(
        fixture.gateway.task_start(&changed).await.unwrap_err().code,
        "idempotency_conflict"
    );
}

#[tokio::test]
async fn uncertain_creation_does_not_dispatch_an_instruction_and_applied_timeouts_reconcile() {
    let fixture = Fixture::empty().await;
    fixture
        .dispatch_plan
        .lock()
        .unwrap()
        .push_back((false, Reply::status(503, json!({"message":"unavailable"}))));
    let uncertain = fixture.gateway.task_start(&start_args()).await.unwrap();
    assert_eq!(uncertain["stage"], "thread_create_uncertain");
    assert!(uncertain["runId"].is_null());
    assert_eq!(dispatch_types(&fixture), ["thread.create"]);

    let fixture = Fixture::empty().await;
    fixture.dispatch_plan.lock().unwrap().push_back((
        true,
        Reply {
            drop_response: true,
            ..Reply::json(Value::Null)
        },
    ));
    let uncertain = fixture.gateway.task_start(&start_args()).await.unwrap();
    assert_eq!(uncertain["stage"], "thread_create_uncertain");
    assert_eq!(dispatch_types(&fixture), ["thread.create"]);
    let recovered = fixture.gateway.task_start(&start_args()).await.unwrap();
    assert_eq!(recovered["stage"], "run_accepted");
    assert_eq!(
        dispatch_types(&fixture),
        ["thread.create", "thread.turn.start"]
    );
}

#[tokio::test]
async fn a_rejected_instruction_keeps_the_native_rejection_reason() {
    let fixture = Fixture::empty().await;
    fixture.dispatch_plan.lock().unwrap().extend([
        (true, Reply::json(json!({"sequence":2}))),
        (
            false,
            Reply::status(
                403,
                json!({"message":"The requested model is unavailable."}),
            ),
        ),
    ]);
    let result = fixture.gateway.task_start(&start_args()).await.unwrap();
    assert_eq!(result["stage"], "dispatch_rejected");
    assert_eq!(result["lastError"], "The requested model is unavailable.");
    assert_eq!(
        dispatch_types(&fixture),
        ["thread.create", "thread.turn.start"]
    );
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn normalize_times(value: &mut Value) {
    match value {
        Value::String(text) if matches!(text.as_str(), "op-create" | "op-run") => {
            *text = "op_<UUID>".into()
        }
        Value::String(text) if crate::operations::parse_timestamp(text).is_some() => {
            *text = "<ISO_TIME>".into()
        }
        Value::Array(items) => items.iter_mut().for_each(normalize_times),
        Value::Object(fields) => fields.values_mut().for_each(normalize_times),
        _ => {}
    }
}

#[tokio::test]
async fn captured_node_task_detail_and_settings_match() {
    let fixture = Fixture::new().await;
    let mut actual = fixture
        .gateway
        .task_get(&Map::from_iter([("taskRef".into(), json!(TASK))]))
        .await
        .unwrap();
    normalize_times(&mut actual);
    assert_eq!(actual["task"], baseline()["t3"]["task"]);
    assert_eq!(actual["environmentId"], "environment-test");
}

#[tokio::test]
async fn run_binding_does_not_follow_a_later_turn_or_expose_its_response() {
    let fixture = Fixture::new().await;
    let mut later = thread();
    later["latestTurn"]["turnId"] = json!("later-turn");
    later["messages"].as_array_mut().unwrap().push(json!({
        "id":"later-response", "role":"assistant", "text":"unrelated answer", "turnId":"later-turn",
    }));
    fixture.snapshot(later, 4);
    let result = fixture.gateway.run_get(RUN).await.unwrap();
    assert_eq!(result["t3TurnId"], TURN);
    assert_eq!(result["runStatus"], "accepted");
    assert_eq!(result["latestResponse"], Value::Null);
    assert_eq!(result["settings"]["state"], "unresolved");
    assert_eq!(result["settings"]["effective"], Value::Null);
}

#[tokio::test]
async fn learns_the_turn_only_from_the_recorded_message_and_keeps_it_on_disconnect() {
    let fixture = Fixture::new().await;
    let path = fixture.directory.join("operations.json");
    let mut raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    raw["operations"][1]
        .as_object_mut()
        .unwrap()
        .remove("turnId");
    std::fs::write(path, raw.to_string()).unwrap();
    let observed = fixture.gateway.run_get(RUN).await.unwrap();
    assert_eq!(observed["t3TurnId"], TURN);
    assert_eq!(
        fixture.gateway.journal.get_by_run_id(RUN).unwrap().unwrap()["turnId"],
        TURN
    );
    fixture.missing(503, "unavailable", None);
    let offline = fixture.gateway.run_get(RUN).await.unwrap();
    assert_eq!(offline["t3TurnId"], TURN);
    assert_eq!(offline["connectionStatus"], "disconnected");
    assert_eq!(offline["runStatus"], "unknown");
}

#[tokio::test]
async fn run_status_tracks_pending_approval_before_pending_input() {
    let fixture = Fixture::new().await;
    for (approvals, input, expected) in [
        (true, true, "awaiting_approval"),
        (false, true, "awaiting_input"),
        (false, false, "running"),
    ] {
        let mut value = thread();
        value["hasPendingApprovals"] = json!(approvals);
        value["hasPendingUserInput"] = json!(input);
        fixture.snapshot(value, 4);
        assert_eq!(
            fixture.gateway.run_get(RUN).await.unwrap()["runStatus"],
            expected
        );
    }
}

#[tokio::test]
async fn only_a_proven_missing_target_has_connected_unknown_freshness() {
    let fixture = Fixture::new().await;
    fixture.gateway.run_get(RUN).await.unwrap();
    for (status, code, reason, present, expected) in [
        (404, "not_found", None, false, "connected"),
        (404, "not_found", None, true, "disconnected"),
        (
            404,
            "not_found",
            Some("route_not_found"),
            false,
            "disconnected",
        ),
        (
            404,
            "NotFoundError",
            Some("thread_not_found"),
            false,
            "connected",
        ),
        (
            401,
            "unauthorized",
            Some("thread_not_found"),
            false,
            "disconnected",
        ),
        (
            503,
            "unavailable",
            Some("thread_not_found"),
            false,
            "disconnected",
        ),
    ] {
        *fixture.shell_threads.lock().unwrap() = if present { vec![thread()] } else { vec![] };
        fixture.missing(status, code, reason);
        let last_observed = fixture.gateway.client.telemetry().last_snapshot_at;
        let result = fixture.gateway.run_get(RUN).await.unwrap();
        assert_eq!(
            result["connectionStatus"], expected,
            "{status} {code} {reason:?} {present}"
        );
        assert_eq!(result["runStatus"], "unknown");
        if expected == "connected" {
            assert_eq!(result["stateFreshness"], "unknown");
        }
        assert_eq!(result["lastObservedAt"], json!(last_observed));
    }
}

#[tokio::test]
async fn terminal_failure_survives_removal_and_clears_with_newer_recovery() {
    let fixture = Fixture::new().await;
    let mut value = thread();
    value["latestTurn"]["state"] = json!("error");
    value["session"] = json!({"status":"error","activeTurnId":TURN,"lastError":"Codex usage limit reached. Send the message again once the limit resets."});
    fixture.snapshot(value.clone(), 4);
    let failed = fixture.gateway.run_get(RUN).await.unwrap();
    assert_eq!(failed["runStatus"], "failed");
    assert_eq!(failed["failure"]["category"], "quota");
    fixture.shell_threads.lock().unwrap().clear();
    fixture.missing(404, "thread_not_found", None);
    let removed = fixture.gateway.run_get(RUN).await.unwrap();
    assert_eq!(removed["runStatus"], "failed");
    assert_eq!(removed["failure"], failed["failure"]);
    value["latestTurn"]["state"] = json!("completed");
    value["session"] = json!({"status":"ready"});
    fixture.snapshot(value, 5);
    let recovered = fixture.gateway.run_get(RUN).await.unwrap();
    assert_eq!(recovered["runStatus"], "completed");
    assert!(recovered["failure"].is_null());
}

#[tokio::test]
async fn task_detail_clears_an_ordered_failure_with_a_newer_completed_snapshot() {
    let fixture = Fixture::new().await;
    let mut value = thread();
    value["latestTurn"]["state"] = json!("error");
    value["session"] = json!({"status":"error","activeTurnId":TURN,"lastError":"Codex usage limit reached. Send the message again once the limit resets."});
    fixture.snapshot(value.clone(), 4);
    assert_eq!(
        fixture.gateway.run_get(RUN).await.unwrap()["runStatus"],
        "failed"
    );
    value["latestTurn"]["state"] = json!("completed");
    value["session"] = json!({"status":"ready"});
    fixture.snapshot(value, 5);
    let detail = fixture
        .gateway
        .task_get(&Map::from_iter([("taskRef".into(), json!(TASK))]))
        .await
        .unwrap();
    assert_eq!(detail["task"]["thread"]["latestTurn"]["state"], "completed");
    assert!(detail["task"]["thread"]["failure"].is_null());
    assert_eq!(detail["task"]["run"]["runStatus"], "completed");
}

#[tokio::test]
async fn task_detail_selects_a_newer_shell_state_for_the_same_turn() {
    let fixture = Fixture::new().await;
    fixture.snapshot(thread(), 4);
    let mut summary = thread();
    summary["latestTurn"]["state"] = json!("completed");
    summary["session"] = json!({"status":"ready"});
    *fixture.shell_threads.lock().unwrap() = vec![summary];
    fixture.shell_sequence.store(5, Ordering::SeqCst);
    let detail = fixture
        .gateway
        .task_get(&Map::from_iter([("taskRef".into(), json!(TASK))]))
        .await
        .unwrap();
    assert_eq!(detail["task"]["thread"]["latestTurn"]["state"], "completed");
    assert_eq!(detail["task"]["run"]["runStatus"], "completed");
}

#[tokio::test]
async fn merged_shell_sequence_does_not_replace_the_live_rows_own_evidence_order() {
    let fixture = Fixture::new().await;
    let mut completed = thread();
    completed["orchestrationProtocolVersion"] = json!(2);
    completed["latestTurn"]["state"] = json!("completed");
    completed["session"] = json!({"status":"ready"});
    fixture.snapshot(completed, 5);
    let mut old_error = thread();
    old_error["orchestrationProtocolVersion"] = json!(2);
    old_error["latestTurn"]["state"] = json!("error");
    old_error["session"] = json!({"status":"error","activeTurnId":TURN,"lastError":"Codex usage limit reached. Send the message again once the limit resets."});
    old_error["evidenceOrder"] = json!({"protocolVersion":2,"scope":"shell","snapshotSequence":2});
    *fixture.shell_threads.lock().unwrap() = vec![old_error];
    fixture.shell_sequence.store(10, Ordering::SeqCst);
    let detail = fixture
        .gateway
        .task_get(&Map::from_iter([("taskRef".into(), json!(TASK))]))
        .await
        .unwrap();
    assert_eq!(detail["task"]["thread"]["latestTurn"]["state"], "completed");
    assert!(detail["task"]["thread"]["failure"].is_null());
    assert_eq!(detail["task"]["run"]["runStatus"], "completed");
}

#[tokio::test]
async fn run_observation_propagates_environment_mismatch_including_missing_targets() {
    let mut fixture = Fixture::new().await;
    fixture.gateway.config.environment_id = Some("another-environment".into());
    assert_eq!(
        fixture.gateway.run_get(RUN).await.unwrap_err().code,
        "environment_mismatch"
    );
    fixture.missing(404, "thread_not_found", None);
    assert_eq!(
        fixture.gateway.run_get(RUN).await.unwrap_err().code,
        "environment_mismatch"
    );
}

#[tokio::test]
async fn cancellation_always_targets_the_tasks_proven_turn_with_a_stable_request_id() {
    let fixture = Fixture::new().await;
    fixture.authorize_native();
    let mut later = thread();
    later["latestTurn"]["turnId"] = json!("later-turn");
    fixture.snapshot(later, 4);
    let args = Map::from_iter([
        ("taskRef".into(), json!(TASK)),
        ("reason".into(), json!("Superseded")),
    ]);
    for _ in 0..2 {
        let result = fixture.gateway.task_cancel(&args).await.unwrap();
        assert_eq!(result["t3RunId"], TURN);
        assert_eq!(result["status"], "interrupt_requested");
        assert_eq!(
            result["nextAction"],
            "Confirm the stop with t3_thread_wait or t3_task_get. Interrupt acceptance alone does not prove the run stopped."
        );
    }
    let requests = fixture.server.requests.lock().unwrap();
    let interrupts = requests
        .iter()
        .filter(|request| request.body["method"] == "tools/call")
        .collect::<Vec<_>>();
    assert_eq!(interrupts.len(), 2);
    for interrupt in interrupts {
        assert_eq!(
            interrupt.body["params"],
            json!({
                "name":"t3_thread_interrupt", "arguments":{
                    "threadId":"<UUID>", "runId":TURN, "reason":"Superseded", "clientRequestId":format!("task-cancel:{TASK}:{TURN}"),
                },
            })
        );
    }
}

#[tokio::test]
async fn results_keep_baseline_limitations_and_indicate_the_observed_state_source() {
    let fixture = Fixture::new().await;
    let args = Map::from_iter([("taskRef".into(), json!(TASK))]);
    let result = fixture.gateway.result_get(&args).await.unwrap();
    assert_eq!(
        result["evidence"],
        json!({"taskStateSource":"t3_observed","git":null})
    );
    assert_eq!(
        result["limitations"],
        json!(["The gateway could not capture a Git baseline after T3 accepted the workspace."])
    );
    fixture.missing(503, "unavailable", None);
    let result = fixture.gateway.result_get(&args).await.unwrap();
    assert_eq!(result["evidence"]["taskStateSource"], "gateway_journal");
}

#[tokio::test]
async fn results_compose_real_git_evidence_with_path_and_byte_limits() {
    let fixture = Fixture::new().await;
    let repository = fixture.directory.join("repository");
    std::fs::create_dir(&repository).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=Task Fixture",
                "-c",
                "user.email=fixture@example.test",
            ])
            .args(args)
            .current_dir(&repository)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-b", "main"]);
    for name in ["selected.txt", "other.txt"] {
        std::fs::write(repository.join(name), "baseline\n").unwrap();
    }
    git(&["add", "."]);
    git(&["commit", "-m", "Baseline"]);
    let base = git(&["rev-parse", "HEAD"]);
    for name in ["selected.txt", "other.txt"] {
        std::fs::write(repository.join(name), "committed\n").unwrap();
    }
    git(&["commit", "-am", "Task changes"]);
    for name in ["selected.txt", "other.txt"] {
        std::fs::write(repository.join(name), "staged\n").unwrap();
    }
    git(&["add", "."]);
    for name in ["selected.txt", "other.txt"] {
        std::fs::write(repository.join(name), "unstaged\n").unwrap();
    }
    std::fs::write(repository.join("untracked.txt"), "untracked\n").unwrap();
    fixture.shell_threads.lock().unwrap()[0]["worktreePath"] = json!(repository);
    fixture
        .gateway
        .journal
        .update_task(
            TASK,
            TaskPatch {
                baseline_attribution: Some(crate::operations::journal::BaselineAttribution::Clean),
                baseline_revision: Some(base.clone()),
                ..TaskPatch::default()
            },
        )
        .unwrap();
    let mut args = Map::from_iter([
        ("taskRef".into(), json!(TASK)),
        ("paths".into(), json!(["selected.txt"])),
        ("maxBytes".into(), json!(100_000)),
    ]);
    let result = fixture.gateway.result_get(&args).await.unwrap();
    let evidence = &result["evidence"]["git"];
    assert_eq!(evidence["source"], "git_observed");
    assert_eq!(evidence["baselineRevision"], base);
    assert_eq!(evidence["status"]["clean"], false);
    assert_eq!(evidence["status"]["untracked"]["count"], 1);
    for (field, text) in [
        ("committed", "+committed"),
        ("staged", "+staged"),
        ("unstaged", "+unstaged"),
    ] {
        assert_eq!(evidence[field]["paths"], json!(["selected.txt"]));
        let patch = evidence[field]["patch"].as_str().unwrap();
        assert!(patch.contains(text), "{field}: {patch}");
        assert!(!patch.contains("other.txt"));
    }
    assert_eq!(
        result["limitations"],
        json!([
            "The current workspace has uncommitted or untracked changes; inspect staged, unstaged, and status evidence separately.",
        ])
    );
    args.insert("maxBytes".into(), json!(32));
    let bounded = fixture.gateway.result_get(&args).await.unwrap();
    for field in ["committed", "staged", "unstaged"] {
        assert_eq!(
            bounded["evidence"]["git"][field]["truncation"]["truncated"],
            true
        );
        assert!(
            bounded["evidence"]["git"][field]["patch"]
                .as_str()
                .unwrap()
                .len()
                <= 32
        );
    }
}

#[tokio::test]
async fn task_list_does_not_reconcile_records_outside_the_requested_page() {
    let fixture = Fixture::new().await;
    fixture
        .gateway
        .journal
        .update(
            "op-create",
            OperationPatch {
                status: Some(OperationStatus::Uncertain),
                ..OperationPatch::default()
            },
        )
        .unwrap();
    let before = fixture.server.requests.lock().unwrap().len();
    let page = fixture
        .gateway
        .tasks_list(&Map::from_iter([
            ("cursor".into(), json!("1")),
            ("limit".into(), json!(1)),
        ]))
        .await
        .unwrap();
    assert_eq!(page["page"]["total"], 1);
    assert_eq!(page["page"]["items"], json!([]));
    let requests = fixture.server.requests.lock().unwrap();
    assert!(
        requests[before..]
            .iter()
            .all(|request| !request.path.starts_with("/api/orchestration/threads/"))
    );
}

#[tokio::test]
async fn local_task_results_have_the_node_mcp_envelope_and_audit_shape() {
    use crate::mcp::{IncomingMessage, McpServer};
    let fixture = Fixture::new().await;
    let server = McpServer::gateway();
    let response = server
        .handle_message(
            IncomingMessage::parse(json!({
                "jsonrpc":"2.0", "id":1, "method":"tools/call",
                "params":{"name":"t3_task_get","arguments":{"taskRef":TASK}},
            }))
            .unwrap(),
            &fixture.gateway,
        )
        .await
        .unwrap();
    let response = serde_json::to_value(response).unwrap();
    let result = &response["result"];
    assert!(result.get("isError").is_none(), "{response}");
    assert_eq!(result["structuredContent"]["task"]["stage"], "run_accepted");
    let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, result["structuredContent"]);
    let events = std::fs::read_to_string(fixture.directory.join("audit.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    let event = events
        .iter()
        .find(|event| event["source"] == "mcp" && event["event"] == "tool.result")
        .unwrap();
    assert_eq!(event["outcome"], "completed");
    assert_eq!(event["details"]["result"]["task"]["stage"], "run_accepted");
    assert!(event["details"]["result"].get("content").is_none());
}

#[tokio::test]
async fn native_results_pass_through_once_and_audit_only_the_result_shape() {
    use crate::mcp::RequestId;
    let fixture = Fixture::new().await;
    fixture.authorize_native();
    let result = fixture
        .gateway
        .call_tool(
            ToolCall {
                request_id: RequestId::Number(1),
                name: "t3_thread_wait".into(),
                arguments: Map::from_iter([("threadId".into(), json!("thread"))]),
                params: json!({"name":"t3_thread_wait","arguments":{"threadId":"thread"}})
                    .as_object()
                    .unwrap()
                    .clone(),
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(result).unwrap(),
        json!({"content":[],"structuredContent":{"status":"interrupt_requested"}})
    );
    let events = std::fs::read_to_string(fixture.directory.join("audit.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    let call = events
        .iter()
        .find(|event| event["source"] == "mcp" && event["event"] == "tool.call")
        .unwrap();
    assert_eq!(call["details"]["native"], true);
    let event = events
        .iter()
        .find(|event| event["source"] == "mcp" && event["event"] == "tool.result")
        .unwrap();
    assert_eq!(
        event["details"]["result"],
        json!({"isError":false,"keys":["status"]})
    );
}

#[path = "mcp_limit_tests.rs"]
mod mcp_limit_tests;

#[path = "native_passthrough_tests.rs"]
mod native_passthrough_tests;

#[path = "mcp_boundary_tests.rs"]
mod mcp_boundary_tests;

#[path = "mcp_attachment_tests.rs"]
mod mcp_attachment_tests;

#[path = "environment_tests.rs"]
mod environment_tests;

#[path = "native_result_tests.rs"]
mod native_result_tests;

#[path = "workflow_tests.rs"]
mod workflow_tests;

#[path = "native_metadata_tests.rs"]
mod native_metadata_tests;

#[path = "retirement_git_tests.rs"]
mod retirement_git_tests;
