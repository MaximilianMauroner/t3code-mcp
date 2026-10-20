use super::*;
use std::sync::Mutex;

#[tokio::test]
async fn provider_item_responses_keep_authoritative_thread_and_run_ownership() {
    let text = "Native item result 🙂. ".repeat(50) + "Final result.";
    let mut snapshot = projection(vec![
        run("current", 2, "completed"),
        run("previous", 1, "completed"),
    ]);
    snapshot["projection"]["messages"] = json!([
        {"id":"user-current","threadId":"thread-1","runId":"current","nodeId":null,
         "role":"user","text":"Check the result","streaming":false,"attachments":[],
         "createdAt":TIME,"updatedAt":TIME},
        {"id":"previous-answer","threadId":"thread-1","runId":"previous","nodeId":"previous-provider-item",
         "role":"assistant","text":"Previous result","streaming":false,"attachments":[],
         "createdAt":TIME,"updatedAt":TIME},
        {"id":"native-answer","threadId":"thread-1","runId":"current","nodeId":"provider-item-node",
         "role":"assistant","text":text,"streaming":false,"attachments":[{"id":"attachment-1"}],
         "createdAt":TIME,"updatedAt":TIME},
        {"id":"unbound-parent-note","threadId":"thread-1","runId":null,"nodeId":null,
         "role":"assistant","text":"An unbound note is not the current run response",
         "streaming":false,"attachments":[],"createdAt":TIME,"updatedAt":TIME},
        {"id":"child-answer","threadId":"delegated-child-thread","runId":null,"nodeId":null,
         "role":"assistant","text":"Child result must not replace the parent result",
         "streaming":false,"attachments":[],"createdAt":TIME,"updatedAt":TIME},
        {"id":"foreign-answer","threadId":"foreign-thread","runId":"current","nodeId":"root-current",
         "role":"assistant","text":"Matching run and root do not establish thread ownership",
         "streaming":false,"attachments":[],"createdAt":TIME,"updatedAt":TIME},
        {"id":"foreign-user","threadId":"foreign-thread","runId":"current","nodeId":null,
         "role":"user","text":"Foreign user message","streaming":false,"attachments":[],
         "createdAt":"2026-10-03T09:00:00.000Z","updatedAt":"2026-10-03T09:00:00.000Z"}
    ]);
    let response = snapshot;
    let server = Server::start(move |request| {
        let snapshot = response.clone();
        async move {
            match request.path.as_str() {
                "/.well-known/t3/environment" => Reply::json(descriptor(2)),
                "/api/orchestration/threads/thread-1" => Reply::json(snapshot),
                _ => Reply::status(404, json!({"code":"unexpected_request"})),
            }
        }
        .boxed()
    })
    .await;
    let observed = http_client(&server)
        .get_thread("thread-1", None)
        .await
        .unwrap();
    assert_eq!(observed.snapshot_sequence, 2);
    let thread = observed.thread;
    let turn = thread.shell.latest_turn.as_ref().unwrap();
    assert_eq!(turn.turn_id, "current");
    assert_eq!(turn.state, TurnState::Completed);
    assert_eq!(turn.assistant_message_id.as_deref(), Some("native-answer"));
    assert_eq!(thread.shell.latest_user_message_at.as_deref(), Some(TIME));
    assert_eq!(
        thread
            .messages
            .iter()
            .map(|message| (message.id.as_str(), message.turn_id.as_deref()))
            .collect::<Vec<_>>(),
        [
            ("user-current", Some("current")),
            ("previous-answer", Some("previous")),
            ("native-answer", Some("current")),
            ("unbound-parent-note", None),
        ]
    );
    let answer = &thread.messages[2];
    assert_eq!(answer.role, MessageRole::Assistant);
    assert_eq!(answer.text, text);
    assert_eq!(answer.streaming, Some(false));
    assert_eq!(answer.extra["nodeId"], "provider-item-node");
    assert_eq!(answer.extra["attachments"], json!([{"id":"attachment-1"}]));
    assert!(failure::failure_info(&thread, None).is_none());
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1]
            .headers
            .get("x-t3-orchestration-protocol")
            .map(String::as_str),
        Some("2")
    );
}

#[tokio::test]
async fn provider_item_message_without_thread_owner_is_invalid() {
    let server = Server::start(|request| {
        async move {
            if request.path == "/.well-known/t3/environment" {
                return Reply::json(descriptor(2));
            }
            let mut snapshot = projection(vec![run("current", 1, "completed")]);
            snapshot["projection"]["messages"] = json!([
                {"id":"unowned","runId":"current","nodeId":"root-current",
                 "role":"assistant","text":"Unknown owner","streaming":false,
                 "attachments":[],"createdAt":TIME,"updatedAt":TIME}
            ]);
            Reply::json(snapshot)
        }
        .boxed()
    })
    .await;
    let client = http_client(&server);
    let error = client.get_thread("thread-1", None).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidResponse);
    assert_eq!(
        client.telemetry().last_error.as_deref(),
        Some(error.message.as_str())
    );
    let requests = server.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.path.as_str())
            .collect::<Vec<_>>(),
        [
            "/.well-known/t3/environment",
            "/api/orchestration/threads/thread-1",
            "/.well-known/t3/environment",
        ]
    );
}

#[tokio::test]
async fn actionable_runtime_requests_round_trip_literal_v2_commands() {
    let mut snapshot = projection(vec![run("active", 1, "waiting")]);
    snapshot["projection"]["runs"][0]["completedAt"] = Value::Null;
    snapshot["projection"]["runtimeRequests"] = json!([
        {"id":"dead","nodeId":"root-active","kind":"user_input","status":"pending",
         "responseCapability":{"type":"not_resumable","reason":"restart"},"createdAt":TIME},
        {"id":"resolved","nodeId":"root-active","kind":"command_approval","status":"resolved",
         "responseCapability":{"type":"live"},"createdAt":TIME},
        {"id":"expired","nodeId":"root-active","kind":"user_input","status":"expired",
         "responseCapability":{"type":"message"},"createdAt":TIME},
        {"id":"cancelled","nodeId":"root-active","kind":"command_approval","status":"cancelled",
         "responseCapability":{"type":"live"},"createdAt":TIME},
        {"id":"approve","nodeId":"root-active","kind":"command_approval","status":"pending",
         "responseCapability":{"type":"live"},"createdAt":TIME},
        {"id":"input","nodeId":"root-active","kind":"user_input","status":"pending",
         "responseCapability":{"type":"message"},"createdAt":TIME}
    ]);
    snapshot["projection"]["turnItems"] = json!([
        {"id":"dead-item","ordinal":0,"type":"user_input_request","status":"waiting",
         "requestId":"dead","runId":"active","nodeId":"root-active","title":"Old question","updatedAt":TIME},
        {"id":"resolved-item","ordinal":1,"type":"approval_request","status":"completed",
         "requestId":"resolved","runId":"active","nodeId":"root-active","title":"Old approval","updatedAt":TIME},
        {"id":"approval-item","ordinal":2,"type":"approval_request","status":"waiting",
         "requestId":"approve","runId":"active","nodeId":"root-active","title":"Approval",
         "prompt":"Allow the command?","command":"git status","updatedAt":TIME},
        {"id":"question-item","ordinal":3,"type":"user_input_request","status":"waiting",
         "requestId":"input","runId":"active","nodeId":"root-active","title":"Question",
         "questions":[{"id":"q1","question":"Which?","options":["first","second"]}],"updatedAt":TIME}
    ]);
    let upstream = Arc::new(Mutex::new(snapshot));
    let server = Server::start(move |request| {
        let upstream = Arc::clone(&upstream);
        async move {
            match request.path.as_str() {
                "/.well-known/t3/environment" => Reply::json(descriptor(2)),
                "/api/orchestration/threads/thread-1" => {
                    Reply::json(upstream.lock().unwrap().clone())
                }
                "/ws?orchestrationProtocol=2" => {
                    let command = &request.body["payload"];
                    let mut snapshot = upstream.lock().unwrap();
                    let pending = snapshot["projection"]["runtimeRequests"]
                        .as_array_mut()
                        .unwrap()
                        .iter_mut()
                        .find(|entry| entry["id"] == command["requestId"])
                        .unwrap();
                    pending["status"] = "resolved".into();
                    snapshot["snapshotSequence"] =
                        (snapshot["snapshotSequence"].as_u64().unwrap() + 1).into();
                    Reply::json(json!({"sequence":snapshot["snapshotSequence"]}))
                }
                _ => Reply::status(404, json!({"code":"unexpected_request"})),
            }
        }
        .boxed()
    })
    .await;
    let client = http_client(&server);
    let thread = client.get_thread("thread-1", None).await.unwrap().thread;
    assert_eq!(thread.shell.has_pending_approvals, Some(true));
    assert_eq!(thread.shell.has_pending_user_input, Some(true));
    assert_eq!(
        thread_state::thread_activity(&thread.shell),
        thread_state::ThreadActivity::AwaitingApproval
    );
    assert_eq!(thread.activities.len(), 2);
    assert_eq!(thread.activities[0]["kind"], "approval");
    assert_eq!(thread.activities[0]["summary"], "Allow the command?");
    assert_eq!(thread.activities[0]["payload"]["command"], "git status");
    assert_eq!(thread.activities[0]["payload"]["runId"], "active");
    assert_eq!(thread.activities[0]["payload"]["nodeId"], "root-active");
    assert_eq!(
        thread.activities[0]["payload"]["responseCapability"],
        json!({"type":"live"})
    );
    assert_eq!(thread.activities[1]["kind"], "user_input");
    assert_eq!(thread.activities[1]["payload"]["requestId"], "input");
    assert_eq!(
        thread.activities[1]["payload"]["questions"],
        json!([{"id":"q1","question":"Which?","options":["first","second"]}])
    );
    assert_eq!(
        thread.activities[1]["payload"]["responseCapability"],
        json!({"type":"message"})
    );
    let approval: T3Command = serde_json::from_value(json!({
        "type":"thread.approval.respond","commandId":"approve-command","threadId":"thread-1",
        "requestId":"approve","decision":"acceptForSession","createdAt":TIME
    }))
    .unwrap();
    assert_eq!(client.dispatch(&approval, None).await.unwrap().sequence, 3);
    let thread = client.get_thread("thread-1", None).await.unwrap().thread;
    assert_eq!(thread.shell.has_pending_approvals, Some(false));
    assert_eq!(thread.shell.has_pending_user_input, Some(true));
    assert_eq!(thread.activities.len(), 1);
    assert_eq!(thread.activities[0]["payload"]["requestId"], "input");
    assert_eq!(
        thread_state::thread_activity(&thread.shell),
        thread_state::ThreadActivity::AwaitingInput
    );
    let input: T3Command = serde_json::from_value(json!({
        "type":"thread.user-input.respond","commandId":"input-command","threadId":"thread-1",
        "requestId":"input","answers":{"q1":"first","freeText":"Keep spaces 🙂"},"createdAt":TIME
    }))
    .unwrap();
    assert_eq!(client.dispatch(&input, None).await.unwrap().sequence, 4);
    let thread = client.get_thread("thread-1", None).await.unwrap().thread;
    assert_eq!(thread.shell.has_pending_approvals, Some(false));
    assert_eq!(thread.shell.has_pending_user_input, Some(false));
    assert!(thread.activities.is_empty());
    assert_eq!(
        thread_state::thread_activity(&thread.shell),
        thread_state::ThreadActivity::Running
    );
    let requests = server.requests.lock().unwrap();
    let dispatches = requests
        .iter()
        .filter(|request| request.path.starts_with("/ws?"))
        .collect::<Vec<_>>();
    assert_eq!(dispatches.len(), 2);
    for request in &dispatches {
        assert_eq!(request.path, "/ws?orchestrationProtocol=2");
        assert_eq!(request.body["_tag"], "Request");
        assert!(request.body["id"].as_str().is_some_and(|id| !id.is_empty()));
        assert_eq!(request.body["tag"], "orchestration.dispatchCommand");
        assert_eq!(request.body["headers"], json!([]));
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            Some("Bearer <T3_ACCESS_TOKEN>")
        );
    }
    assert_eq!(
        dispatches[0].body["payload"],
        json!({"type":"runtime-request.respond","commandId":"approve-command","threadId":"thread-1",
               "requestId":"approve","decision":"acceptForSession"})
    );
    assert_eq!(
        dispatches[1].body["payload"],
        json!({"type":"runtime-request.respond","commandId":"input-command","threadId":"thread-1",
               "requestId":"input","answers":{"q1":"first","freeText":"Keep spaces 🙂"}})
    );
    assert!(requests.iter().all(|request| request.method != "POST"));
}

#[tokio::test]
async fn runtime_response_native_failures_keep_the_reason_and_do_not_replay() {
    for (command, expected) in [
        (
            json!({"type":"thread.approval.respond","commandId":"reject-approval","threadId":"thread-1",
                   "requestId":"approve","decision":"decline","createdAt":TIME}),
            json!({"type":"runtime-request.respond","commandId":"reject-approval","threadId":"thread-1",
                   "requestId":"approve","decision":"decline"}),
        ),
        (
            json!({"type":"thread.user-input.respond","commandId":"reject-input","threadId":"thread-1",
                   "requestId":"input","answers":{"q1":"first"},"createdAt":TIME}),
            json!({"type":"runtime-request.respond","commandId":"reject-input","threadId":"thread-1",
                   "requestId":"input","answers":{"q1":"first"}}),
        ),
    ] {
        let server = Server::start(|request| {
            async move {
                if request.path == "/.well-known/t3/environment" {
                    return Reply::json(descriptor(2));
                }
                Reply::json(json!({
                    "_tag":"Exit","requestId":request.body["id"],
                    "exit":{"_tag":"Failure","cause":[{"_tag":"Fail","error":{
                        "_tag":"OrchestrationV2DispatchCommandError",
                        "message":"Runtime request is no longer pending"
                    }}]}
                }))
            }
            .boxed()
        })
        .await;
        let client = http_client(&server);
        let command: T3Command = serde_json::from_value(command).unwrap();
        let error = client.dispatch(&command, None).await.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Rpc);
        assert_eq!(error.message, "Runtime request is no longer pending");
        assert_eq!(
            client.telemetry().last_error.as_deref(),
            Some("Runtime request is no longer pending")
        );
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].path, "/.well-known/t3/environment");
        assert_eq!(requests[1].path, "/ws?orchestrationProtocol=2");
        assert_eq!(requests[1].body["tag"], "orchestration.dispatchCommand");
        assert_eq!(requests[1].body["payload"], expected);
    }
}

#[tokio::test]
async fn v1_unbound_start_error_does_not_attach_to_an_earlier_completed_turn() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/failures/v1-unbound-start-error.json"
    ))
    .unwrap();
    let previous = json!({"id":"previous-answer","role":"assistant","text":"previous success",
        "turnId":"old-turn","createdAt":"2026-09-29T20:00:00.000Z"});
    let mut failed = shell_value();
    for (key, value) in fixture.as_object().unwrap() {
        failed[key] = value.clone();
    }
    failed["messages"] = json!([previous]);
    let mut completed = shell_value();
    completed["latestTurn"] = json!({"turnId":"old-turn","state":"completed",
        "requestedAt":"2026-09-29T19:59:00.000Z","completedAt":"2026-09-29T20:00:00.000Z",
        "assistantMessageId":"previous-answer"});
    completed["session"] = json!({"status":"ready","activeTurnId":null,"lastError":null});
    completed["messages"] = json!([previous]);
    let reads = Arc::new(AtomicUsize::new(0));
    let observed_reads = Arc::clone(&reads);
    let server = Server::start(move |request| {
        let reads = Arc::clone(&observed_reads);
        let completed = completed.clone();
        let failed = failed.clone();
        async move {
            match request.path.as_str() {
                "/.well-known/t3/environment" => Reply::json(descriptor(1)),
                "/api/orchestration/threads/thread-1" => {
                    let first = reads.fetch_add(1, Ordering::SeqCst) == 0;
                    Reply::json(json!({"snapshotSequence":if first {1} else {2},
                        "thread":if first {completed} else {failed}}))
                }
                _ => Reply::status(404, json!({"code":"unexpected_request"})),
            }
        }
        .boxed()
    })
    .await;
    let client = http_client(&server);
    let completed = client.get_thread("thread-1", None).await.unwrap().thread;
    assert!(failure::failure_info(&completed, None).is_none());
    assert_eq!(completed.messages[0].text, "previous success");
    let failed = client.get_thread("thread-1", None).await.unwrap().thread;
    assert_eq!(
        failed.shell.latest_turn.as_ref().unwrap().turn_id,
        "failed-turn"
    );
    assert_eq!(
        failed.shell.latest_turn.as_ref().unwrap().assistant_message_id,
        None
    );
    let failure = failure::failure_info(&failed, Some("failed-turn")).unwrap();
    assert_eq!(failure.category, failure::FailureCategory::Unknown);
    assert_eq!(failure.source, failure::FailureSource::Turn);
    assert_eq!(failure.turn_id, "failed-turn");
    assert!(!failure.message.is_empty());
    assert_eq!(failure.code, None);
    assert!(!failure.message.contains("ProviderUnsupportedError"));
    assert!(failure::failure_info(&failed, Some("old-turn")).is_none());
    assert_eq!(failed.messages.len(), 1);
    assert_eq!(failed.messages[0].turn_id.as_deref(), Some("old-turn"));
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    assert!(server.requests.lock().unwrap().iter().all(|request| {
        !request.headers.contains_key("x-t3-orchestration-protocol")
    }));
}
