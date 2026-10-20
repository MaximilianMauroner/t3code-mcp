use super::{
    commands::T3Command,
    http_client::ErrorKind,
    test_server::{Reply, Server},
    types::*,
    *,
};
use futures_util::FutureExt;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
const TIME: &str = "2026-10-03T08:47:00.000Z";
fn shell_value() -> Value {
    json!({"id":"thread-1","projectId":"project-1","title":"Thread","modelSelection":{"model":"model-1","instanceId":"codex"},"runtimeMode":"full-access","interactionMode":"default","createdAt":TIME,"updatedAt":TIME})
}
fn shell() -> ThreadShell {
    serde_json::from_value(shell_value()).unwrap()
}
fn descriptor(version: u8) -> Value {
    json!({"environmentId":"env","label":"Test","serverVersion":"test","orchestrationProtocolVersion":version})
}
fn command() -> T3Command {
    serde_json::from_value(json!({"type":"thread.approval.respond","commandId":"approve","threadId":"thread-1","requestId":"request-1","decision":"accept","createdAt":TIME})).unwrap()
}
fn http_client(server: &Server) -> T3HttpClient {
    T3HttpClient::new(
        &server.url,
        "<T3_ACCESS_TOKEN>",
        Duration::from_secs(2),
        None,
    )
}

#[test]
fn captured_node_commands_round_trip_and_convert_in_order() {
    let baseline: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/node-compat-baseline.json"
    ))
    .unwrap();
    for entry in baseline["t3"]["dispatches"].as_array().unwrap() {
        let command: T3Command = serde_json::from_value(entry["command"].clone()).unwrap();
        assert_eq!(serde_json::to_value(&command).unwrap(), entry["command"]);
        let converted = command.v2_commands();
        if entry["command"]["type"] == "thread.turn.start" {
            assert_eq!(
                converted
                    .iter()
                    .map(|c| c["type"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                [
                    "thread.runtime-mode.set",
                    "thread.interaction-mode.set",
                    "message.dispatch"
                ]
            );
            assert_eq!(converted[0]["commandId"], "<UUID>:runtime");
            assert_eq!(converted[1]["commandId"], "<UUID>:interaction");
            assert_eq!(converted[2]["messageId"], "user:msg_<ID>");
            assert!(converted[2].get("createdAt").is_none());
        } else {
            assert_eq!(converted[0]["createdBy"], "user");
            assert_eq!(converted[0]["creationSource"], "mcp");
        }
    }
}
#[tokio::test]
async fn captured_node_http_request_order_and_authentication() {
    let baseline: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/node-compat-baseline.json"
    ))
    .unwrap();
    let server = Server::start(|request| {
        async move {
            Reply::json(match request.path.as_str() {
                "/.well-known/t3/environment" => descriptor(1),
                "/api/auth/session" => json!({"authenticated":true}),
                "/api/orchestration/shell" => {
                    json!({"snapshotSequence":1,"projects":[],"threads":[],"updatedAt":TIME})
                }
                "/api/orchestration/dispatch" => json!({"sequence":1}),
                _ => json!({"snapshotSequence":1,"thread":shell_value()}),
            })
        }
        .boxed()
    })
    .await;
    let client = http_client(&server);
    let expected = baseline["t3"]["requests"].as_array().unwrap();
    let mut index = 0;
    while index < expected.len() {
        let entry = &expected[index];
        match entry["path"].as_str().unwrap() {
            "/.well-known/t3/environment"
                if expected
                    .get(index + 1)
                    .is_some_and(|v| v["method"] == "POST") =>
            {
                let command =
                    serde_json::from_value::<T3Command>(expected[index + 1]["body"].clone())
                        .unwrap();
                client.dispatch(&command, None).await.unwrap();
                index += 2;
                continue;
            }
            "/.well-known/t3/environment" => {
                client.get_descriptor(None).await.unwrap();
            }
            "/api/auth/session" => {
                client.get_session(None).await.unwrap();
            }
            "/api/orchestration/shell" => {
                client.get_shell(None).await.unwrap();
            }
            _ => {
                client.get_thread("<UUID>", None).await.unwrap();
            }
        }
        index += 1;
    }
    let actual = server
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| {
            let path = r.path.replace("%3CUUID%3E", "<UUID>");
            let mut v = json!({"method":r.method,"path":path});
            if let Some(auth) = r.headers.get("authorization") {
                v["authorization"] = auth.clone().into();
            }
            if !r.body.is_null() {
                v["body"] = r.body.clone();
            }
            assert!(!r.headers.contains_key("x-t3-orchestration-protocol"));
            v
        })
        .collect::<Vec<_>>();
    assert_eq!(actual, *expected);
}
#[tokio::test]
async fn discovery_is_shared_and_cancelled_waiter_does_not_cancel_survivor() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let release = gate.clone();
    let server = Server::start(move |r| {
        let release = release.clone();
        let count = count.clone();
        async move {
            if r.path == "/.well-known/t3/environment" {
                count.fetch_add(1, Ordering::SeqCst);
                release.notified().await;
                Reply::json(descriptor(1))
            } else {
                Reply::json(
                    json!({"snapshotSequence":1,"projects":[],"threads":[],"updatedAt":TIME}),
                )
            }
        }
        .boxed()
    })
    .await;
    let client = http_client(&server);
    let cancel = tokio_util::sync::CancellationToken::new();
    let c = client.clone();
    let token = cancel.clone();
    let aborted = tokio::spawn(async move { c.dispatch(&command(), Some(&token)).await });
    let c = client.clone();
    let survivor = tokio::spawn(async move { c.get_shell(None).await });
    tokio::time::timeout(Duration::from_secs(1), async {
        while requests.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    assert_eq!(
        aborted.await.unwrap().unwrap_err().kind,
        ErrorKind::Cancelled
    );
    gate.notify_waiters();
    survivor.await.unwrap().unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(server.requests.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn mutation_discovery_failure_is_definitely_unsent() {
    let server =
        Server::start(|_| async { Reply::status(503, json!({"reason":"offline"})) }.boxed()).await;
    let client = http_client(&server);
    let error = client.dispatch(&command(), None).await.unwrap_err();
    assert_eq!(error.reason.as_deref(), Some("command_not_sent"));
    assert_eq!(error.code.as_deref(), Some("protocol_discovery_failed"));
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn errors_keep_evidence_and_unchanged_protocol_does_not_replay() {
    let discoveries = Arc::new(AtomicUsize::new(0));
    let count = discoveries.clone();
    let server=Server::start(move|r|{let count=count.clone();async move{if r.path=="/.well-known/t3/environment"{count.fetch_add(1,Ordering::SeqCst);Reply::json(descriptor(1))}else{Reply::status(400,json!({"code":"scope_missing","requiredScope":"orchestration:read","traceId":"trace-1"}))}}.boxed()}).await;
    let client = http_client(&server);
    let e = client.get_shell(None).await.unwrap_err();
    assert_eq!(e.status, Some(400));
    assert_eq!(e.required_scope.as_deref(), Some("orchestration:read"));
    assert_eq!(e.trace_id.as_deref(), Some("trace-1"));
    assert_eq!(
        e.message,
        "T3 GET /api/orchestration/shell failed with HTTP 400 (requires orchestration:read)."
    );
    assert_eq!(discoveries.load(Ordering::SeqCst), 2);
    assert_eq!(server.requests.lock().unwrap().len(), 3);
}
#[test]
fn node_thread_state_priority_and_freshness() {
    use thread_state::*;
    let now = timestamp(Some(TIME)).unwrap();
    let mut t = shell();
    assert_eq!(
        observe_thread(&t, now).quality,
        ObservationQuality::Incomplete
    );
    assert!(is_in_open_queue(&t, now));
    t.session = Some(serde_json::from_value(json!({"status":"running","updatedAt":TIME})).unwrap());
    assert!(is_thread_working(&t));
    assert!(!is_in_open_queue(&t, now));
    assert_eq!(
        observe_thread(&t, now + 300_001).quality,
        ObservationQuality::Stale
    );
    t.latest_turn = Some(
        serde_json::from_value(json!({"turnId":"run-1","state":"completed","requestedAt":TIME}))
            .unwrap(),
    );
    assert_eq!(
        observe_thread(&t, now).quality,
        ObservationQuality::Inconsistent
    );
    t.has_pending_approvals = Some(true);
    assert_eq!(thread_activity(&t), ThreadActivity::AwaitingApproval);
    assert!(!is_thread_working(&t));
    t.archived_at = Some(TIME.into());
    assert_eq!(thread_status(&t, now), ThreadStatus::Archived);
    t.archived_at = None;
    t.has_pending_approvals = Some(false);
    t.session = None;
    t.snoozed_until = Some(iso(now + 1000));
    assert_eq!(thread_status(&t, now), ThreadStatus::Snoozed);
    t.snoozed_at = Some(iso(now - 1000));
    t.latest_turn.as_mut().unwrap().completed_at = Some(TIME.into());
    assert_eq!(thread_status(&t, now), ThreadStatus::Open);
    t.snoozed_until = None;
    t.settled_override = Some("settled".into());
    assert_eq!(thread_status(&t, now), ThreadStatus::Settled);
    t.latest_user_message_at = Some(iso(now + 1));
    assert_eq!(thread_status(&t, now + 2), ThreadStatus::Open);
    t.settled_at = Some(iso(now + 2));
    assert_eq!(thread_status(&t, now + 2), ThreadStatus::Settled);
}
#[test]
fn captured_node_failure_fixtures_choose_the_same_provider_evidence() {
    use failure::*;
    for (text, category, source) in [
        (
            include_str!("../../../tests/fixtures/failures/v1-credits-required.json"),
            FailureCategory::AuthBilling,
            FailureSource::Message,
        ),
        (
            include_str!("../../../tests/fixtures/failures/v1-auth-unavailable.json"),
            FailureCategory::AuthBilling,
            FailureSource::Message,
        ),
        (
            include_str!("../../../tests/fixtures/failures/v1-claude-rate-limit.json"),
            FailureCategory::RateLimit,
            FailureSource::Message,
        ),
        (
            include_str!("../../../tests/fixtures/failures/v1-codex-usage-limit.json"),
            FailureCategory::Quota,
            FailureSource::Activity,
        ),
    ] {
        let fixture = serde_json::from_str::<Value>(text).unwrap();
        let mut v = shell_value();
        if fixture.get("latestTurn").is_some() {
            for (k, x) in fixture.as_object().unwrap() {
                v[k] = x.clone();
            }
        } else {
            v["latestTurn"] = json!({"turnId":"failed-turn","state":"error","requestedAt":TIME});
            v["messages"] = json!([fixture]);
        }
        let thread: Thread = serde_json::from_value(v).unwrap();
        let failure = failure_info(&thread, None).unwrap();
        assert_eq!(failure.category, category, "{text}");
        assert_eq!(failure.source, source, "{text}");
        assert_eq!(failure.identity.len(), 64);
        assert!(failure_info(&thread, Some("unrelated-turn")).is_none());
    }
}
#[test]
fn failure_redaction_keeps_ids_and_hashes_raw_reasons() {
    assert_eq!(
        failure::sanitize_failure_identifier("claude/token-provider:model\n"),
        "claude/token-provider:model "
    );
    let text = failure::sanitize_failure_text(
        "failed https://private.example/a at /home/user/private api_key=secret Bearer abcdef\n    at file:///path/file.js:1",
        2000,
    );
    assert!(!text.contains("private.example"));
    assert!(!text.contains("/home/user"));
    assert!(!text.contains("secret"));
    assert!(!text.contains("abcdef"));
}

fn v2_thread() -> Value {
    let mut t = shell_value();
    t["providerInstanceId"] = "codex".into();
    t
}
fn run(id: &str, ordinal: u64, status: &str) -> Value {
    json!({"id":id,"ordinal":ordinal,"status":status,"providerInstanceId":"codex","modelSelection":{"model":"model-1"},"userMessageId":format!("user-{id}"),"rootNodeId":format!("root-{id}"),"requestedAt":TIME,"startedAt":TIME,"completedAt":TIME})
}
fn projection(runs: Vec<Value>) -> Value {
    json!({"snapshotSequence":2,"projection":{"thread":v2_thread(),"runs":runs,"messages":[],"providerSessions":[],"runtimeRequests":[],"turnItems":[],"plans":[],"checkpoints":[],"updatedAt":TIME}})
}
#[test]
fn v2_queued_runs_do_not_hide_active_or_usage_limited_runs() {
    let snapshot = projection(vec![
        run("active", 1, "running"),
        run("queued", 2, "queued"),
    ]);
    let normalized = v2::normalize_thread(snapshot).unwrap();
    assert_eq!(
        normalized.thread.shell.latest_turn.unwrap().turn_id,
        "active"
    );
    let mut snapshot = projection(vec![
        run("failed", 1, "failed"),
        run("cancelled", 2, "cancelled"),
        run("queued", 3, "queued"),
    ]);
    snapshot["projection"]["runs"][1]["startedAt"] = Value::Null;
    let fixtures: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/failures/v2-provider-failures.json"
    ))
    .unwrap();
    let item = json!({"id":"error-1","type":"error","runId":"failed","nodeId":"root-failed","updatedAt":TIME,"failure":fixtures["codexUsageLimit"],"title":"Failed","status":"failed","ordinal":1});
    snapshot["projection"]["turnItems"] = json!([item]);
    let normalized = v2::normalize_thread(snapshot).unwrap();
    assert_eq!(
        normalized
            .thread
            .shell
            .latest_turn
            .as_ref()
            .unwrap()
            .turn_id,
        "failed"
    );
    let failure = failure::failure_info(&normalized.thread, None).unwrap();
    assert_eq!(failure.category, failure::FailureCategory::Quota);
    assert_eq!(failure.source, failure::FailureSource::V2TurnItem);
    assert_eq!(
        failure.reset_at.as_deref(),
        Some("2033-05-19T07:20:00.000Z")
    );
    assert_eq!(
        normalized.thread.turn_recoveries.unwrap()[0].turn_id,
        "cancelled"
    );
}
#[test]
fn v2_root_failure_order_filters_child_errors_and_retains_history() {
    let mut snapshot = projection(vec![run("failed", 1, "failed"), run("new", 2, "completed")]);
    snapshot["projection"]["turnItems"] = json!([
        {"id":"z","type":"error","runId":"failed","nodeId":"root-failed","updatedAt":TIME,"ordinal":2,"status":"failed","title":null,"failure":{"class":"provider_error","code":"old","message":"older"}},
        {"id":"a","type":"error","runId":"failed","nodeId":"root-failed","updatedAt":TIME,"ordinal":3,"status":"failed","title":null,"failure":{"class":"provider_error","code":"new","message":"chosen"}},
        {"id":"child","type":"error","runId":"failed","nodeId":"child","updatedAt":TIME,"ordinal":4,"status":"failed","title":null,"failure":{"class":"usage_limit","code":"usageLimitExceeded","message":"child error"}}
    ]);
    let t = v2::normalize_thread(snapshot).unwrap().thread;
    assert_eq!(t.shell.latest_turn.as_ref().unwrap().turn_id, "new");
    assert!(failure::failure_info(&t, None).is_none());
    let failure = failure::failure_info(&t, Some("failed")).unwrap();
    assert_eq!(failure.message, "chosen");
    assert_eq!(
        t.turn_failures.unwrap()[0]
            .order
            .as_ref()
            .unwrap()
            .item
            .as_ref()
            .unwrap()
            .id,
        "a"
    );
}
#[tokio::test]
async fn v2_dispatch_and_archived_shell_use_effect_websocket_envelopes() {
    let server=Server::start(|r|async move{
        if r.path=="/.well-known/t3/environment"{return Reply::json(descriptor(2));}
        if r.path=="/api/orchestration/shell"{
            assert_eq!(r.headers.get("x-t3-orchestration-protocol").map(String::as_str),Some("2"));
            let mut t=v2_thread();t["latestRunId"]=Value::Null;t["activeRunId"]=Value::Null;t["status"]="idle".into();t["pendingRuntimeRequest"]=Value::Null;
            return Reply::json(json!({"schemaVersion":1,"snapshotSequence":2,"projects":[],"threads":[t],"archivedThreads":[]}));
        }
        assert_eq!(r.path,"/ws?orchestrationProtocol=2");assert_eq!(r.headers.get("authorization").unwrap(),"Bearer <T3_ACCESS_TOKEN>");
        assert_eq!(r.body["_tag"],"Request");assert_eq!(r.body["headers"],json!([]));
        if r.body["tag"]=="orchestration.getArchivedShellSnapshot"{Reply::json(json!({"schemaVersion":1,"snapshotSequence":4,"threads":[]}))}else{Reply::json(json!({"sequence":7}))}
    }.boxed()).await;
    let baseline: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/node-compat-baseline.json"
    ))
    .unwrap();
    let command: T3Command =
        serde_json::from_value(baseline["t3"]["dispatches"][1]["command"].clone()).unwrap();
    let client = http_client(&server);
    let result = client.dispatch(&command, None).await.unwrap();
    assert_eq!(result.sequence, 7);
    let snapshot = client.get_shell(None).await.unwrap();
    assert_eq!(snapshot.snapshot_sequence, 4);
    assert_eq!(snapshot.threads.len(), 1);
    assert!(client.telemetry().last_snapshot_at.is_some());
    let requests = server.requests.lock().unwrap();
    let commands: Vec<_> = requests
        .iter()
        .filter(|r| r.body["tag"] == "orchestration.dispatchCommand")
        .map(|r| r.body["payload"].clone())
        .collect();
    assert_eq!(commands, command.v2_commands());
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.path == "/.well-known/t3/environment")
            .count(),
        1
    );
}
#[tokio::test]
async fn v2_non_object_thread_returns_invalid_response_without_replay() {
    for thread in [json!("bad"), Value::Null, json!(true), json!(42), json!([])] {
        let malformed = thread.clone();
        let server = Server::start(move |request| {
            let thread = malformed.clone();
            async move {
                if request.path == "/.well-known/t3/environment" {
                    return Reply::json(descriptor(2));
                }
                let mut snapshot = projection(vec![]);
                snapshot["projection"]["thread"] = thread;
                Reply::json(snapshot)
            }
            .boxed()
        })
        .await;
        let client = http_client(&server);
        let error =
            tokio::time::timeout(Duration::from_secs(5), client.get_thread("thread-1", None))
                .await
                .expect("malformed thread read must finish")
                .unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidResponse, "{thread}");
        assert!(
            error
                .message
                .contains("/api/orchestration/threads/thread-1")
        );
        let telemetry = client.telemetry();
        assert_eq!(
            telemetry.last_error.as_deref(),
            Some(error.message.as_str())
        );
        assert!(telemetry.last_successful_at.is_some());
        assert!(telemetry.last_snapshot_at.is_none());
        let requests = server.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .map(|r| (r.method.as_str(), r.path.as_str()))
                .collect::<Vec<_>>(),
            [
                ("GET", "/.well-known/t3/environment"),
                ("GET", "/api/orchestration/threads/thread-1"),
                ("GET", "/.well-known/t3/environment"),
            ],
            "{thread}"
        );
        assert_eq!(
            requests[1]
                .headers
                .get("x-t3-orchestration-protocol")
                .map(String::as_str),
            Some("2")
        );
    }
}
#[tokio::test]
async fn read_switches_protocol_once_after_schema_mismatch() {
    let count = Arc::new(AtomicUsize::new(0));
    let requests = count.clone();
    let server = Server::start(move |r| {
        let count = requests.clone();
        async move {
            if r.path == "/.well-known/t3/environment" {
                return Reply::json(descriptor(if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    1
                } else {
                    2
                }));
            }
            Reply::json(projection(vec![]))
        }
        .boxed()
    })
    .await;
    let result = http_client(&server)
        .get_thread("thread-1", None)
        .await
        .unwrap();
    assert_eq!(result.snapshot_sequence, 2);
    assert_eq!(count.load(Ordering::SeqCst), 2);
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert!(
        !requests[1]
            .headers
            .contains_key("x-t3-orchestration-protocol")
    );
    assert_eq!(
        requests[3]
            .headers
            .get("x-t3-orchestration-protocol")
            .map(String::as_str),
        Some("2")
    );
}
#[tokio::test]
async fn rpc_protocol_failure_and_disconnect_never_replay() {
    for drop_response in [false, true] {
        let server = Server::start(move |_| {
            async move {
                Reply {
                    drop_response,
                    ..Reply::json(json!({"_tag":"Defect"}))
                }
            }
            .boxed()
        })
        .await;
        let error = rpc_client::request_t3_rpc::<Value>(
            &server.url,
            "token",
            Duration::from_secs(1),
            "mutate",
            json!({}),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Rpc);
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
}

mod v2_behavior_tests {
    include!("v2_behavior_tests.rs");
}

mod retirement_state_tests {
    include!("retirement_state_tests.rs");
}

mod retirement_overview_tests {
    include!("retirement_overview_tests.rs");
}

mod retirement_settings_tests {
    include!("retirement_settings_tests.rs");
}
