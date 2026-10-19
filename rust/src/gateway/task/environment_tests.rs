use super::{Fixture, RUN, TASK, start_args};
use crate::{
    gateway::T3Gateway,
    mcp::{IncomingMessage, McpServer},
    native_credential::{NativeAccessLevel, NativeCredentialState, NativeCredentialStore},
    t3::test_server::{Reply, Server},
};
use futures_util::FutureExt;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

fn descriptor(environment_id: &str) -> Reply {
    Reply::json(json!({
        "environmentId":environment_id, "label":"Test", "serverVersion":"test",
    }))
}

struct EnvironmentFixture {
    fixture: Fixture,
    descriptors: Arc<Mutex<VecDeque<Reply>>>,
}

impl EnvironmentFixture {
    async fn new() -> Self {
        let mut fixture = Fixture::new().await;
        let descriptors = Arc::new(Mutex::new(VecDeque::from([descriptor("environment-test")])));
        let plan = Arc::clone(&descriptors);
        let thread_reply = Arc::clone(&fixture.reply);
        let shell_threads = Arc::clone(&fixture.shell_threads);
        fixture.server = Server::start(move |request| {
            let plan = Arc::clone(&plan);
            let thread_reply = Arc::clone(&thread_reply);
            let shell_threads = Arc::clone(&shell_threads);
            async move {
                match request.path.as_str() {
                    "/.well-known/t3/environment" => {
                        let mut replies = plan.lock().unwrap();
                        if replies.len() > 1 {
                            replies.pop_front().unwrap()
                        } else {
                            replies.front().unwrap().clone()
                        }
                    }
                    "/api/orchestration/shell" => Reply::json(json!({
                        "snapshotSequence":3,
                        "projects":[{"id":"project-baseline","title":"Baseline project","workspaceRoot":"/missing-fixture"}],
                        "threads":shell_threads.lock().unwrap().clone(),
                        "updatedAt":"2026-10-18T00:00:00.000Z",
                    })),
                    path if path.starts_with("/api/orchestration/threads/") => {
                        thread_reply.lock().unwrap().clone()
                    }
                    _ => Reply::status(404, json!({"code":"unexpected_request"})),
                }
            }
            .boxed()
        })
        .await;
        let mut config = fixture.gateway.config().clone();
        config.endpoint.t3_http_base_url = fixture.server.url.clone();
        config.endpoint.t3_native_mcp_url = format!("{}/mcp", fixture.server.url);
        config.environment_id = Some("environment-test".into());
        fixture.gateway = T3Gateway::new(config);
        Self {
            fixture,
            descriptors,
        }
    }

    fn discovery(&self, replies: impl IntoIterator<Item = Reply>) {
        *self.descriptors.lock().unwrap() = replies.into_iter().collect();
    }

    fn reopen(&mut self) {
        self.fixture.gateway = T3Gateway::new(self.fixture.gateway.config().clone());
    }
}

async fn call(fixture: &Fixture, name: &str, arguments: Value) -> Value {
    let response = McpServer::gateway()
        .handle_message(
            IncomingMessage::parse(json!({
                "jsonrpc":"2.0", "id":1, "method":"tools/call",
                "params":{"name":name,"arguments":arguments},
            }))
            .unwrap(),
            &fixture.gateway,
        )
        .await
        .unwrap();
    let result = &response["result"];
    let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, result["structuredContent"], "{response}");
    result.clone()
}

fn assert_error(result: &Value, code: &str) {
    assert_eq!(result["isError"], true, "{result}");
    assert_eq!(
        result["structuredContent"]["error"]["code"], code,
        "{result}"
    );
    assert!(result["structuredContent"].get("task").is_none());
}

#[tokio::test]
async fn reopening_accepted_tasks_against_another_environment_fails_through_mcp() {
    let mut environment = EnvironmentFixture::new().await;
    let initial = call(&environment.fixture, "t3_task_get", json!({"taskRef":TASK})).await;
    assert_ne!(initial["isError"], true, "{initial}");
    assert_eq!(
        initial["structuredContent"]["task"]["stage"],
        "run_accepted"
    );
    assert_eq!(initial["structuredContent"]["task"]["run"]["runId"], RUN);
    let journal_path = environment.fixture.directory.join("operations.json");
    let original = std::fs::read(&journal_path).unwrap();
    environment.discovery([descriptor("other-environment")]);
    environment.reopen();
    environment.fixture.authorize_native();
    let before = environment.fixture.server.requests.lock().unwrap().len();

    for (name, arguments) in [
        ("t3_task_get", json!({"taskRef":TASK})),
        ("t3_result_get", json!({"taskRef":TASK})),
        ("t3_tasks_list", json!({})),
        ("t3_tasks_list", json!({"cursor":"1","limit":1})),
        ("t3_task_cancel", json!({"taskRef":TASK})),
    ] {
        let result = call(&environment.fixture, name, arguments).await;
        assert_error(&result, "environment_mismatch");
    }
    assert_eq!(std::fs::read(journal_path).unwrap(), original);
    let requests = environment.fixture.server.requests.lock().unwrap();
    assert!(requests[before..].iter().all(|request| {
        request.method == "GET" && request.path == "/.well-known/t3/environment"
    }));
}

#[tokio::test]
async fn task_detail_preserves_child_environment_errors_after_discovery_succeeds() {
    for missing_thread in [false, true] {
        let environment = EnvironmentFixture::new().await;
        environment.discovery([
            descriptor("environment-test"),
            descriptor("other-environment"),
        ]);
        if missing_thread {
            environment.fixture.missing(404, "thread_not_found", None);
        }
        let result = call(&environment.fixture, "t3_task_get", json!({"taskRef":TASK})).await;
        assert_error(&result, "environment_mismatch");
    }
}

#[tokio::test]
async fn task_reads_reject_a_cached_mismatch_when_discovery_becomes_unavailable() {
    let environment = EnvironmentFixture::new().await;
    environment.discovery([descriptor("other-environment")]);
    environment
        .fixture
        .gateway
        .client
        .get_descriptor(None)
        .await
        .unwrap();
    environment.discovery([Reply::status(503, json!({"code":"unavailable"}))]);
    for (name, arguments) in [
        ("t3_task_get", json!({"taskRef":TASK})),
        ("t3_tasks_list", json!({"cursor":"1","limit":1})),
    ] {
        let result = call(&environment.fixture, name, arguments).await;
        assert_error(&result, "environment_mismatch");
    }
}

#[tokio::test]
async fn accepted_legacy_tasks_keep_transient_missing_detail_and_stored_settings() {
    for (status, code, connection) in [
        (404, "thread_not_found", "connected"),
        (503, "unavailable", "disconnected"),
    ] {
        let environment = EnvironmentFixture::new().await;
        environment.fixture.missing(status, code, None);
        let result = call(&environment.fixture, "t3_task_get", json!({"taskRef":TASK})).await;
        assert_ne!(result["isError"], true, "{result}");
        let value = &result["structuredContent"];
        assert_eq!(value["environmentId"], "environment-test");
        assert_eq!(value["task"]["stage"], "run_accepted");
        assert!(value["task"]["thread"].is_null());
        assert_eq!(value["task"]["run"]["runStatus"], "unknown");
        assert_eq!(value["task"]["run"]["connectionStatus"], connection);
        assert_eq!(
            value["task"]["settings"]["resolved"]["runtimeMode"],
            "approval-required"
        );
    }
    let environment = EnvironmentFixture::new().await;
    let journal_path = environment.fixture.directory.join("operations.json");
    let mut journal: Value =
        serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
    journal["operations"]
        .as_array_mut()
        .unwrap()
        .retain(|operation| operation["kind"] != "thread.turn.start");
    std::fs::write(journal_path, journal.to_string()).unwrap();
    let result = call(&environment.fixture, "t3_task_get", json!({"taskRef":TASK})).await;
    assert_ne!(result["isError"], true, "{result}");
    assert!(result["structuredContent"]["task"]["run"].is_null());
    assert!(result["structuredContent"]["task"]["thread"].is_object());
}

#[tokio::test]
async fn task_journal_reads_remain_available_without_discovery_or_an_environment_pin() {
    let environment = EnvironmentFixture::new().await;
    environment.discovery([Reply::status(503, json!({"code":"unavailable"}))]);
    let result = call(&environment.fixture, "t3_task_get", json!({"taskRef":TASK})).await;
    assert_ne!(result["isError"], true, "{result}");
    assert_eq!(
        result["structuredContent"]["environmentId"],
        "environment-test"
    );
    assert_eq!(result["structuredContent"]["task"]["stage"], "run_accepted");

    let mut environment = EnvironmentFixture::new().await;
    environment.fixture.gateway.config.environment_id = None;
    environment.discovery([descriptor("other-environment")]);
    let result = call(&environment.fixture, "t3_task_get", json!({"taskRef":TASK})).await;
    assert_ne!(result["isError"], true, "{result}");
    assert_eq!(
        result["structuredContent"]["environmentId"],
        "other-environment"
    );
    assert_eq!(result["structuredContent"]["task"]["stage"], "run_accepted");
}

#[tokio::test]
async fn task_mutation_policy_still_precedes_environment_reads() {
    let mut environment = EnvironmentFixture::new().await;
    environment.fixture.gateway.config.read_only = true;
    environment.discovery([descriptor("other-environment")]);
    for (name, arguments) in [
        ("t3_task_start", Value::Object(start_args())),
        ("t3_task_cancel", json!({"taskRef":TASK})),
    ] {
        let result = call(&environment.fixture, name, arguments).await;
        assert_error(&result, "gateway_read_only");
    }
    assert!(
        environment
            .fixture
            .server
            .requests
            .lock()
            .unwrap()
            .is_empty()
    );

    environment.fixture.gateway.config.read_only = false;
    environment.fixture.authorize_native();
    let mut store = NativeCredentialStore::in_data_dir(&environment.fixture.directory);
    let NativeCredentialState::Valid { mut credential } = store.read() else {
        panic!("expected fixture credential");
    };
    credential.access = NativeAccessLevel::ReadOnly;
    credential.scopes = vec!["orchestration:read".into()];
    store.write(&credential).unwrap();
    let result = call(
        &environment.fixture,
        "t3_task_cancel",
        json!({"taskRef":TASK}),
    )
    .await;
    assert_error(&result, "t3_scope_required");
    assert!(
        environment
            .fixture
            .server
            .requests
            .lock()
            .unwrap()
            .is_empty()
    );
}
