use super::*;
use crate::mcp::{IncomingMessage, McpServer};

async fn mcp_result(gateway: &T3Gateway, name: &str, arguments: Value) -> Value {
    let response = McpServer::gateway()
        .handle_message(
            IncomingMessage::parse(json!({
                "jsonrpc":"2.0", "id":1, "method":"tools/call",
                "params":{"name":name,"arguments":arguments},
            }))
            .unwrap(),
            gateway,
        )
        .await
        .unwrap();
    let response = serde_json::to_value(response).unwrap();
    assert!(response.get("error").is_none(), "{response}");
    response["result"].clone()
}

async fn mcp_value(gateway: &T3Gateway, name: &str, arguments: Value) -> Value {
    let result = mcp_result(gateway, name, arguments).await;
    assert_ne!(result["isError"], true, "{name}: {result}");
    let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, result["structuredContent"]);
    text
}

fn dispatches(fixture: &Fixture) -> Vec<Value> {
    fixture
        .server
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.path == "/api/orchestration/dispatch")
        .map(|request| request.body.clone())
        .collect()
}

fn request_count(fixture: &Fixture, path: &str) -> usize {
    let expected = url::Url::parse("http://fixture.invalid")
        .unwrap()
        .join(path)
        .unwrap();
    fixture
        .server
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.path == expected.path())
        .count()
}

fn ids(rows: &Value) -> Vec<String> {
    rows.as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .collect()
}

fn idle_thread(id: &str) -> Value {
    let mut value = thread();
    value["id"] = json!(id);
    value["title"] = json!(id);
    value["latestTurn"] = Value::Null;
    value["session"] = json!({"status":"ready"});
    value["messages"] = json!([]);
    value
}

#[tokio::test]
async fn pending_actions_extract_details_and_send_validated_commands_through_mcp() {
    let fixture = Fixture::new().await;
    let mut value = thread();
    value["hasPendingApprovals"] = json!(true);
    value["hasPendingUserInput"] = json!(true);
    value["activities"] = json!([
        {"tone":"approval","kind":"tool.approval","summary":"Allow test command",
         "payload":{"requestId":"approval-1","command":"pnpm test"}},
        {"kind":"user.question","summary":"Which branch?",
         "payload":{"requestId":"question-1","field":"branch"}},
        {"kind":"noise","payload":{"irrelevant":true}},
    ]);
    let thread_id = value["id"].clone();
    fixture.snapshot(value, 4);
    let before = request_count(
        &fixture,
        &format!("/api/orchestration/threads/{}", thread_id.as_str().unwrap()),
    );
    let pending = mcp_value(
        &fixture.gateway,
        "t3_pending_actions_list",
        json!({"threadId":thread_id}),
    )
    .await;
    assert_eq!(pending["hasPendingApprovals"], true);
    assert_eq!(pending["hasPendingUserInput"], true);
    assert_eq!(pending["detailsAvailable"], true);
    assert_eq!(
        pending["actions"],
        json!([
            {"kind":"approval","requestId":"approval-1","summary":"Allow test command",
             "payload":{"requestId":"approval-1","command":"pnpm test"}},
            {"kind":"user_input","requestId":"question-1","summary":"Which branch?",
             "payload":{"requestId":"question-1","field":"branch"}},
        ])
    );
    assert_eq!(
        request_count(
            &fixture,
            &format!("/api/orchestration/threads/{}", thread_id.as_str().unwrap()),
        ),
        before + 1,
    );

    for (kind, request_id, key, code) in [
        (
            "approval",
            "approval-1",
            "missing-decision",
            "decision_required",
        ),
        (
            "user_input",
            "question-1",
            "missing-answers",
            "answers_required",
        ),
    ] {
        let result = mcp_result(
            &fixture.gateway,
            "t3_pending_action_respond",
            json!({"threadId":thread_id,"requestId":request_id,"kind":kind,
                   "idempotencyKey":key}),
        )
        .await;
        assert_eq!(result["isError"], true, "{result}");
        assert_eq!(result["structuredContent"]["error"]["code"], code);
        assert!(
            fixture
                .gateway
                .journal
                .get_by_idempotency_key(key)
                .unwrap()
                .is_none()
        );
        assert!(dispatches(&fixture).is_empty());
    }

    let approval = mcp_value(
        &fixture.gateway,
        "t3_pending_action_respond",
        json!({"threadId":thread_id,"requestId":"approval-1","kind":"approval",
               "decision":"accept","idempotencyKey":"approval-response"}),
    )
    .await;
    let question = mcp_value(
        &fixture.gateway,
        "t3_pending_action_respond",
        json!({"threadId":thread_id,"requestId":"question-1","kind":"user_input",
               "answers":{"branch":"main"},"idempotencyKey":"question-response"}),
    )
    .await;
    assert_eq!(approval["status"], "accepted");
    assert_eq!(question["status"], "accepted");
    let sent = dispatches(&fixture);
    assert_eq!(sent.len(), 2);
    for (command, receipt, command_type, request_id) in [
        (&sent[0], &approval, "thread.approval.respond", "approval-1"),
        (
            &sent[1],
            &question,
            "thread.user-input.respond",
            "question-1",
        ),
    ] {
        assert_eq!(command["type"], command_type);
        assert_eq!(command["threadId"], thread_id);
        assert_eq!(command["requestId"], request_id);
        assert_eq!(command["commandId"], receipt["commandId"]);
        assert!(!command["commandId"].as_str().unwrap().is_empty());
        assert!(
            crate::operations::parse_timestamp(command["createdAt"].as_str().unwrap()).is_some()
        );
    }
    assert_eq!(sent[0]["decision"], "accept");
    assert_eq!(sent[1]["answers"], json!({"branch":"main"}));
}

#[tokio::test]
async fn pending_flags_without_activity_details_do_not_invent_actions() {
    let fixture = Fixture::new().await;
    let mut value = thread();
    value["hasPendingApprovals"] = json!(true);
    value["hasPendingUserInput"] = json!(true);
    let thread_id = value["id"].clone();
    fixture.snapshot(value, 4);
    let pending = mcp_value(
        &fixture.gateway,
        "t3_pending_actions_list",
        json!({"threadId":thread_id}),
    )
    .await;
    assert_eq!(pending["hasPendingApprovals"], true);
    assert_eq!(pending["hasPendingUserInput"], true);
    assert_eq!(pending["detailsAvailable"], false);
    assert_eq!(pending["actions"], json!([]));
    assert_eq!(
        request_count(
            &fixture,
            &format!("/api/orchestration/threads/{}", thread_id.as_str().unwrap())
        ),
        1
    );
    assert!(dispatches(&fixture).is_empty());
}

#[tokio::test]
async fn open_queue_excludes_background_work_and_children_but_explicit_review_keeps_them() {
    let fixture = Fixture::new().await;
    let observed_at = now_iso();
    let mut rows = Vec::new();
    for id in [
        "idle", "approval", "question", "failed", "running", "parent", "watched", "child",
        "settled",
    ] {
        let mut value = idle_thread(id);
        match id {
            "approval" => {
                value["session"] = json!({"status":"running","updatedAt":observed_at});
                value["hasPendingApprovals"] = json!(true);
            }
            "question" => {
                value["backgroundLiveness"] = json!("working");
                value["hasPendingUserInput"] = json!(true);
            }
            "failed" => {
                value["backgroundLiveness"] = json!("working");
                value["session"] = json!({"status":"error","updatedAt":observed_at});
                value["latestTurn"] =
                    json!({"turnId":"failed-turn","state":"error","requestedAt":observed_at});
            }
            "running" => value["session"] = json!({"status":"running","updatedAt":observed_at}),
            "parent" => value["backgroundLiveness"] = json!("working"),
            "watched" => value["backgroundLiveness"] = json!("monitoring"),
            "child" => {
                value["lineage"] =
                    json!({"parentThreadId":"parent","relationshipToParent":"subagent"})
            }
            "settled" => value["settledOverride"] = json!("settled"),
            _ => {}
        }
        value["createdAt"] = json!(observed_at);
        value["updatedAt"] = json!(observed_at);
        rows.push(value);
    }
    *fixture.shell_threads.lock().unwrap() = rows;
    fixture.missing(503, "unavailable", None);
    for (arguments, scope, expected) in [
        (
            json!({"limit":20,"sort":"title"}),
            "open_queue",
            vec!["approval", "failed", "idle", "question"],
        ),
        (
            json!({"status":"all","limit":20,"sort":"title"}),
            "filtered",
            vec![
                "approval", "child", "failed", "idle", "parent", "question", "running", "settled",
                "watched",
            ],
        ),
        (
            json!({"onlyRunning":true,"limit":20,"sort":"title"}),
            "filtered",
            vec!["approval", "running"],
        ),
        (
            json!({"status":"open","query":"child","limit":20}),
            "filtered",
            vec!["child"],
        ),
    ] {
        let before = request_count(&fixture, "/api/orchestration/shell");
        let list = mcp_value(&fixture.gateway, "t3_threads_list", arguments).await;
        assert_eq!(list["scope"], scope);
        assert_eq!(ids(&list["page"]["items"]), expected);
        assert_eq!(
            request_count(&fixture, "/api/orchestration/shell"),
            before + 1
        );
    }
    let hidden = mcp_value(
        &fixture.gateway,
        "t3_threads_list",
        json!({"query":"parent","limit":20}),
    )
    .await;
    assert_eq!(hidden["page"]["total"], 0);
    assert!(
        hidden["resolutionHint"]
            .as_str()
            .unwrap()
            .contains("status=all")
    );
    for (limit, running) in [(0, vec![]), (5, vec!["approval", "running"])] {
        let before = request_count(&fixture, "/api/orchestration/shell");
        let overview = mcp_value(
            &fixture.gateway,
            "t3_threads_overview",
            json!({"runningLimit":limit}),
        )
        .await;
        assert_eq!(overview["total"], 9);
        assert_eq!(overview["runningCount"], 2);
        assert_eq!(ids(&overview["running"]), running);
        assert_eq!(
            ids(&overview["highlights"]),
            ["approval", "question", "failed", "idle"]
        );
        assert_eq!(
            request_count(&fixture, "/api/orchestration/shell"),
            before + 1
        );
    }
    assert!(dispatches(&fixture).is_empty());
}

#[tokio::test]
async fn failed_session_with_a_stale_running_turn_stays_in_the_queue_and_ranks_before_idle() {
    let fixture = Fixture::new().await;
    let mut stale = idle_thread("stale");
    stale["createdAt"] = json!("2026-01-01T00:00:00Z");
    stale["updatedAt"] = json!("2026-01-01T00:00:00Z");
    stale["session"] = json!({"status":"error","updatedAt":"2026-01-01T00:00:00Z"});
    stale["latestTurn"] =
        json!({"turnId":"stale-turn","state":"running","requestedAt":"2026-01-01T00:00:00Z"});
    *fixture.shell_threads.lock().unwrap() = vec![idle_thread("idle"), stale];
    fixture.missing(503, "unavailable", None);
    let list = mcp_value(
        &fixture.gateway,
        "t3_threads_list",
        json!({"sort":"title","limit":20}),
    )
    .await;
    assert_eq!(ids(&list["page"]["items"]), ["idle", "stale"]);
    let overview = mcp_value(
        &fixture.gateway,
        "t3_threads_overview",
        json!({"runningLimit":0}),
    )
    .await;
    assert_eq!(ids(&overview["highlights"]), ["stale", "idle"]);
    assert_eq!(request_count(&fixture, "/api/orchestration/shell"), 2);
}

#[derive(Clone, Copy, Debug)]
enum FailureReader {
    Summary,
    Full,
    Overview,
}

impl FailureReader {
    async fn read(self, gateway: &T3Gateway) -> Value {
        match self {
            Self::Overview => {
                mcp_value(gateway, "t3_threads_overview", json!({"runningLimit":5})).await
            }
            Self::Summary | Self::Full => mcp_value(
                gateway,
                "t3_threads_list",
                json!({"detail":if matches!(self, Self::Full) {"full"} else {"summary"},"limit":5}),
            )
            .await,
        }
    }

    fn row(self, result: &Value) -> &Value {
        match self {
            Self::Overview => &result["highlights"][0],
            Self::Summary | Self::Full => &result["page"]["items"][0],
        }
    }
}

fn failed_thread(message: &str) -> Value {
    let mut value = thread();
    value["latestTurn"]["state"] = json!("error");
    value["session"] = json!({"status":"error","activeTurnId":TURN,"lastError":message});
    value
}

fn stored_journal(fixture: &Fixture) -> OperationJournal {
    OperationJournal::new(fixture.directory.join("operations.json"), None)
}

#[tokio::test]
async fn list_and_overview_sanitize_provider_error_excerpts() {
    for reader in [FailureReader::Full, FailureReader::Overview] {
        for text in [
            "API Error: failed https://private.example/request at /home/user/private token=secret",
            "See https://public.example/help for details",
        ] {
            let fixture = Fixture::new().await;
            let mut value = failed_thread("Provider failed");
            value["messages"] = json!([{
                "id":"response", "role":"assistant", "text":text, "turnId":TURN,
            }]);
            fixture.snapshot(value.clone(), 3);
            *fixture.shell_threads.lock().unwrap() = vec![value];
            let result = reader.read(&fixture.gateway).await;
            let excerpt = reader.row(&result)["latestResponseExcerpt"]
                .as_str()
                .unwrap();
            if text.starts_with("API Error:") {
                assert!(excerpt.starts_with("API Error:"), "{excerpt}");
                for secret in ["private.example", "/home/user/private", "secret"] {
                    assert!(!excerpt.contains(secret), "{secret} leaked in {excerpt}");
                }
                assert!(excerpt.contains("[REDACTED URL]"), "{excerpt}");
                assert!(excerpt.contains("[REDACTED]"), "{excerpt}");
            } else {
                assert_eq!(excerpt, text);
            }
        }
    }
}

#[tokio::test]
async fn task_get_accepts_a_latest_response_without_optional_metadata() {
    let fixture = Fixture::new().await;
    let mut value = thread();
    let response = json!({
        "id":"response", "role":"assistant", "text":"Answer", "turnId":TURN,
    });
    value["messages"]
        .as_array_mut()
        .unwrap()
        .push(response.clone());
    fixture.snapshot(value, 3);
    let result = mcp_value(&fixture.gateway, "t3_task_get", json!({"taskRef":TASK})).await;
    assert_eq!(result["task"]["run"]["latestResponse"], response);
}

#[tokio::test]
async fn list_and_overview_retain_shell_failures_even_when_full_enrichment_is_unavailable() {
    for reader in [
        FailureReader::Summary,
        FailureReader::Full,
        FailureReader::Overview,
    ] {
        let fixture = Fixture::new().await;
        let value = failed_thread(
            "Codex usage limit reached. Send the message again once the limit resets.",
        );
        let thread_id = value["id"].as_str().unwrap().to_owned();
        *fixture.shell_threads.lock().unwrap() = vec![value];
        fixture.shell_sequence.store(100, Ordering::SeqCst);
        fixture.missing(503, "unavailable", None);
        let result = reader.read(&fixture.gateway).await;
        let row = reader.row(&result);
        assert_eq!(row["failure"]["category"], "quota", "{reader:?}: {result}");
        assert_eq!(row["failure"]["source"], "t3_session");
        assert_eq!(row["failure"]["turnId"], TURN);
        assert_eq!(request_count(&fixture, "/api/orchestration/shell"), 1);
        assert_eq!(
            request_count(&fixture, &format!("/api/orchestration/threads/{thread_id}")),
            usize::from(!matches!(reader, FailureReader::Summary))
        );
        let journal = stored_journal(&fixture);
        let retained = journal
            .get_failure_by_turn_id(&thread_id, TURN)
            .unwrap()
            .unwrap();
        assert_eq!(retained["category"], "quota", "{reader:?}");
        assert_eq!(retained["source"], "t3_session");
        let order = journal
            .get_failure_evidence_order(&thread_id, TURN)
            .unwrap()
            .unwrap();
        assert_eq!(order["scope"], "shell");
        assert_eq!(order["snapshotSequence"], 100);
        let reopened = T3Gateway::new(fixture.gateway.config().clone());
        let unavailable = reopened.run_get(RUN).await.unwrap();
        assert_eq!(unavailable["runStatus"], "failed");
        assert_eq!(unavailable["connectionStatus"], "disconnected");
        assert_eq!(unavailable["failure"]["category"], "quota");
    }
}

#[tokio::test]
async fn full_list_and_overview_retain_activity_evidence_first_seen_during_enrichment() {
    for reader in [FailureReader::Full, FailureReader::Overview] {
        let fixture = Fixture::new().await;
        let mut value = failed_thread("unused");
        let thread_id = value["id"].as_str().unwrap().to_owned();
        value["session"] = json!({"status":"stopped"});
        value["activities"] = json!([{
            "kind":"runtime.error","turnId":TURN,
            "payload":{"class":"usage_limit","message":"Codex usage limit reached. Send the message again once the limit resets."},
        }]);
        fixture.snapshot(value.clone(), 100);
        *fixture.shell_threads.lock().unwrap() = vec![value.clone()];
        fixture.shell_sequence.store(100, Ordering::SeqCst);
        let result = reader.read(&fixture.gateway).await;
        assert_eq!(
            request_count(&fixture, &format!("/api/orchestration/threads/{thread_id}")),
            1
        );
        let row = reader.row(&result);
        assert_eq!(row["failure"]["category"], "quota", "{reader:?}: {result}");
        assert_eq!(row["failure"]["source"], "t3_activity");
        assert_eq!(row["failure"]["turnId"], TURN);
        assert!(row["latestResponseExcerpt"].is_null());
        let journal = stored_journal(&fixture);
        assert_eq!(
            journal
                .get_failure_by_turn_id(&thread_id, TURN)
                .unwrap()
                .unwrap()["source"],
            "t3_activity"
        );
        let order = journal
            .get_failure_evidence_order(&thread_id, TURN)
            .unwrap()
            .unwrap();
        assert_eq!(order["scope"], "full");
        assert_eq!(order["snapshotSequence"], 100);
        value["activities"] = json!([]);
        value["session"] = json!({"status":"ready"});
        fixture.snapshot(value, 100);
        let reopened = T3Gateway::new(fixture.gateway.config().clone());
        let retained = reopened.run_get(RUN).await.unwrap();
        assert_eq!(retained["failure"]["category"], "quota");
        assert_eq!(retained["failure"]["source"], "t3_activity");
    }
}

#[tokio::test]
async fn list_and_overview_keep_the_shell_turn_separate_from_successor_responses_and_failures() {
    for reader in [FailureReader::Full, FailureReader::Overview] {
        for state in ["running", "completed", "interrupted", "error"] {
            let fixture = Fixture::new().await;
            fixture.gateway.run_get(RUN).await.unwrap();
            let old = failed_thread("Older failure");
            let thread_id = old["id"].as_str().unwrap().to_owned();
            let mut successor = old.clone();
            successor["latestTurn"] =
                json!({"turnId":"new-turn","state":state,"requestedAt":"2099-01-01T00:00:00Z"});
            successor["session"] = json!({"status":if state == "error" {"error"} else {"ready"},
                "activeTurnId":"new-turn","lastError":if state == "error" {Some("Successor failure")} else {None}});
            successor["messages"] = json!([{"id":"successor-answer","role":"assistant","text":"Successor response","turnId":"new-turn"}]);
            *fixture.shell_threads.lock().unwrap() = vec![old];
            fixture.shell_sequence.store(100, Ordering::SeqCst);
            fixture.snapshot(successor, 101);
            let before =
                request_count(&fixture, &format!("/api/orchestration/threads/{thread_id}"));
            let result = reader.read(&fixture.gateway).await;
            assert_eq!(
                request_count(&fixture, &format!("/api/orchestration/threads/{thread_id}")),
                before + 1
            );
            let row = reader.row(&result);
            assert_eq!(
                row["latestTurn"]["turnId"], TURN,
                "{reader:?}, {state}: {result}"
            );
            assert_eq!(row["activity"], "failed");
            assert_eq!(row["failure"]["message"], "Older failure");
            assert_eq!(row["failure"]["turnId"], TURN);
            assert!(row["latestResponseExcerpt"].is_null());
            if matches!(reader, FailureReader::Overview) {
                assert_eq!(result["executionCounts"]["failed"], 1);
                assert_eq!(result["runningCount"], 0);
            }
            fixture.missing(503, "unavailable", None);
            let reopened = T3Gateway::new(fixture.gateway.config().clone());
            let retained = reopened.run_get(RUN).await.unwrap();
            assert_eq!(
                retained["failure"]["message"], "Older failure",
                "{reader:?}, {state}: {retained}"
            );
            assert_eq!(retained["failure"]["turnId"], TURN);
        }
    }
}

#[tokio::test]
async fn list_and_overview_replace_same_reason_metadata_only_with_newer_full_evidence() {
    for reader in [FailureReader::Full, FailureReader::Overview] {
        let fixture = Fixture::new().await;
        let mut older = failed_thread("Provider refusal");
        let thread_id = older["id"].as_str().unwrap().to_owned();
        older["session"] = json!({"status":"error","activeTurnId":TURN,"lastError":"Provider refusal",
            "lastErrorClass":"provider_error","failureCode":"rate_limit_error","resetAt":"2026-10-04T00:00:00Z","retryAfter":"300"});
        fixture.snapshot(older.clone(), 100);
        assert_eq!(
            fixture.gateway.run_get(RUN).await.unwrap()["failure"]["code"],
            "rate_limit_error"
        );
        let mut newer = older.clone();
        newer["session"]["failureCode"] = json!("credits_required");
        newer["session"]["resetAt"] = json!("2026-10-05T00:00:00Z");
        newer["session"]["retryAfter"] = json!("600");
        let mut generic_shell = older.clone();
        generic_shell["session"] = json!({"status":"ready"});
        *fixture.shell_threads.lock().unwrap() = vec![generic_shell];
        fixture.shell_sequence.store(99, Ordering::SeqCst);
        fixture.snapshot(newer, 101);
        let before = request_count(&fixture, &format!("/api/orchestration/threads/{thread_id}"));
        let result = reader.read(&fixture.gateway).await;
        assert_eq!(
            request_count(&fixture, &format!("/api/orchestration/threads/{thread_id}")),
            before + 1
        );
        let failure = &reader.row(&result)["failure"];
        assert_eq!(
            failure["message"], "Provider refusal",
            "{reader:?}: {result}"
        );
        assert_eq!(failure["source"], "t3_session");
        assert_eq!(failure["category"], "auth_billing");
        assert_eq!(failure["code"], "credits_required");
        assert_eq!(failure["resetAt"], "2026-10-05T00:00:00.000Z");
        assert_eq!(failure["retryAfter"], "600");
        let journal = stored_journal(&fixture);
        assert_eq!(
            journal
                .get_failure_by_turn_id(&thread_id, TURN)
                .unwrap()
                .unwrap()["code"],
            "credits_required"
        );
        let order = journal
            .get_failure_evidence_order(&thread_id, TURN)
            .unwrap()
            .unwrap();
        assert_eq!(order["scope"], "full");
        assert_eq!(order["snapshotSequence"], 101);
        fixture.snapshot(older, 100);
        let reopened = T3Gateway::new(fixture.gateway.config().clone());
        assert_eq!(
            reopened.run_get(RUN).await.unwrap()["failure"]["code"],
            "credits_required"
        );
    }
}

#[tokio::test]
async fn newer_full_recovery_clears_a_shell_failure_without_relabeling_the_shell_turn() {
    for reader in [FailureReader::Full, FailureReader::Overview] {
        let fixture = Fixture::new().await;
        let old = failed_thread("Older failure");
        let thread_id = old["id"].as_str().unwrap().to_owned();
        fixture.snapshot(old.clone(), 100);
        assert_eq!(
            fixture.gateway.run_get(RUN).await.unwrap()["failure"]["message"],
            "Older failure"
        );
        *fixture.shell_threads.lock().unwrap() = vec![old];
        fixture.shell_sequence.store(100, Ordering::SeqCst);
        let mut recovered = thread();
        recovered["latestTurn"]["state"] = json!("completed");
        recovered["session"] = json!({"status":"ready"});
        fixture.snapshot(recovered, 101);
        let before = request_count(&fixture, &format!("/api/orchestration/threads/{thread_id}"));
        let result = reader.read(&fixture.gateway).await;
        assert_eq!(
            request_count(&fixture, &format!("/api/orchestration/threads/{thread_id}")),
            before + 1
        );
        let row = reader.row(&result);
        assert_eq!(row["latestTurn"]["state"], "error");
        assert!(row["failure"].is_null(), "{reader:?}: {result}");
        let journal = stored_journal(&fixture);
        assert!(
            journal
                .get_failure_by_turn_id(&thread_id, TURN)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            journal
                .get_failure_evidence_order(&thread_id, TURN)
                .unwrap()
                .unwrap()["snapshotSequence"],
            101
        );
    }
}

#[tokio::test]
async fn recovery_is_recorded_before_list_and_overview_filters_hide_the_thread() {
    for overview in [false, true] {
        let fixture = Fixture::new().await;
        let old = failed_thread("Old failure");
        let thread_id = old["id"].as_str().unwrap().to_owned();
        fixture.snapshot(old.clone(), 100);
        assert_eq!(
            fixture.gateway.run_get(RUN).await.unwrap()["failure"]["message"],
            "Old failure"
        );
        let mut recovered = old;
        recovered["latestTurn"]["state"] = json!("completed");
        recovered["session"] = json!({"status":"ready"});
        *fixture.shell_threads.lock().unwrap() = vec![recovered];
        fixture.shell_sequence.store(101, Ordering::SeqCst);
        fixture.missing(503, "unavailable", None);
        if overview {
            let result = mcp_value(
                &fixture.gateway,
                "t3_threads_overview",
                json!({"query":"no match","runningLimit":5}),
            )
            .await;
            assert_eq!(result["total"], 0);
        } else {
            let result = mcp_value(
                &fixture.gateway,
                "t3_threads_list",
                json!({"activity":"failed","limit":5}),
            )
            .await;
            assert_eq!(result["page"]["items"], json!([]));
        }
        assert_eq!(request_count(&fixture, "/api/orchestration/shell"), 1);
        assert_eq!(
            request_count(&fixture, &format!("/api/orchestration/threads/{thread_id}")),
            1
        );
        let journal = stored_journal(&fixture);
        assert!(
            journal
                .get_failure_by_turn_id(&thread_id, TURN)
                .unwrap()
                .is_none()
        );
        let order = journal
            .get_failure_evidence_order(&thread_id, TURN)
            .unwrap()
            .unwrap();
        assert_eq!(order["scope"], "shell");
        assert_eq!(order["snapshotSequence"], 101);
        let reopened = T3Gateway::new(fixture.gateway.config().clone());
        let unavailable = reopened.run_get(RUN).await.unwrap();
        assert!(unavailable["failure"].is_null(), "{unavailable}");
    }
}

impl Fixture {
    async fn with_discovery_replies(replies: Arc<Mutex<VecDeque<Reply>>>) -> Self {
        let mut fixture = Self::new().await;
        let server = Server::start(move |request| {
            let replies = Arc::clone(&replies);
            async move {
                match request.path.as_str() {
                    "/.well-known/t3/environment" => replies.lock().unwrap().pop_front().unwrap_or_else(|| {
                        Reply::json(json!({"environmentId":"environment-test","label":"Test","serverVersion":"test"}))
                    }),
                    "/api/auth/session" => Reply::json(json!({"authenticated":true,"scopes":["orchestration:read","orchestration:operate"]})),
                    "/api/orchestration/dispatch" => Reply::json(json!({"sequence":7})),
                    path if path.starts_with("/api/orchestration/threads/") => Reply::json(json!({"snapshotSequence":3,"thread":idle_thread("thread-preflight")})),
                    _ => Reply::status(404, json!({"code":"not_found"})),
                }
            }.boxed()
        }).await;
        let mut config = fixture.gateway.config().clone();
        config.endpoint.t3_http_base_url = server.url.clone();
        config.endpoint.t3_native_mcp_url = format!("{}/mcp", server.url);
        fixture.gateway = T3Gateway::new(config);
        fixture.server = server;
        fixture
    }
}

#[tokio::test]
async fn preflight_rejection_is_durable_same_key_never_dispatches_and_fresh_key_succeeds() {
    for (label, failure) in [
        (
            "network",
            Reply {
                drop_response: true,
                ..Reply::json(Value::Null)
            },
        ),
        (
            "HTTP",
            Reply::status(503, json!({"message":"descriptor offline"})),
        ),
        ("schema", Reply::json(json!({"invalid":true}))),
        (
            "unsupported",
            Reply::json(
                json!({"environmentId":"environment-test","label":"Test","serverVersion":"test","orchestrationProtocolVersion":3}),
            ),
        ),
    ] {
        let replies = Arc::new(Mutex::new(VecDeque::new()));
        let fixture = Fixture::with_discovery_replies(Arc::clone(&replies)).await;
        fixture.gateway.client.get_descriptor(None).await.unwrap();
        assert_eq!(request_count(&fixture, "/.well-known/t3/environment"), 1);
        replies.lock().unwrap().push_back(failure);
        let input = |key: &str| ThreadSendInput {
            thread_id: "thread-preflight".into(),
            message: "x".into(),
            model_selection: None,
            runtime_mode: None,
            interaction_mode: None,
            title_seed: None,
            initial_settings: None,
            idempotency_key: key.into(),
        };
        let rejected = fixture
            .gateway
            .thread_send(input("not-sent"))
            .await
            .unwrap();
        assert_eq!(rejected["status"], "rejected", "{label}: {rejected}");
        assert!(
            rejected["reason"]
                .as_str()
                .unwrap()
                .contains("command not sent")
        );
        assert!(
            rejected["reason"]
                .as_str()
                .unwrap()
                .contains("new idempotency key")
        );
        assert!(
            replies.lock().unwrap().is_empty(),
            "{label} was never consumed"
        );
        assert_eq!(request_count(&fixture, "/api/auth/session"), 1);
        assert_eq!(
            request_count(&fixture, "/api/orchestration/threads/thread-preflight"),
            1
        );
        assert_eq!(request_count(&fixture, "/.well-known/t3/environment"), 3);
        assert!(dispatches(&fixture).is_empty());
        let persisted = stored_journal(&fixture)
            .get_by_idempotency_key("not-sent")
            .unwrap()
            .unwrap();
        assert_eq!(persisted["status"], "rejected");
        assert_eq!(persisted["operationId"], rejected["operationId"]);

        let reopened = T3Gateway::new(fixture.gateway.config().clone());
        let retry = reopened.thread_send(input("not-sent")).await.unwrap();
        assert_eq!(retry["status"], "rejected", "{label}: {retry}");
        assert_eq!(retry["operationId"], rejected["operationId"]);
        assert_eq!(retry["commandId"], rejected["commandId"]);
        assert_eq!(request_count(&fixture, "/api/auth/session"), 1);
        assert_eq!(
            request_count(&fixture, "/api/orchestration/threads/thread-preflight"),
            1
        );
        assert!(dispatches(&fixture).is_empty());
        let accepted = reopened.thread_send(input("safe-retry")).await.unwrap();
        assert_eq!(accepted["status"], "accepted", "{label}: {accepted}");
        assert_ne!(accepted["operationId"], rejected["operationId"]);
        let sent = dispatches(&fixture);
        assert_eq!(sent.len(), 1, "{label}: {sent:?}");
        assert_eq!(sent[0]["type"], "thread.turn.start");
        assert_eq!(sent[0]["threadId"], "thread-preflight");
        assert_eq!(sent[0]["message"]["text"], "x");
        assert_eq!(sent[0]["commandId"], accepted["commandId"]);
        assert_ne!(sent[0]["commandId"], rejected["commandId"]);
        let journal = stored_journal(&fixture);
        assert_eq!(
            journal.get_by_idempotency_key("not-sent").unwrap().unwrap()["status"],
            "rejected"
        );
        assert_eq!(
            journal
                .get_by_idempotency_key("safe-retry")
                .unwrap()
                .unwrap()["status"],
            "accepted"
        );
    }
}
