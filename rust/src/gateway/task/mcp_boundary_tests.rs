use super::{Fixture, dispatch_types, thread};
use crate::{
    gateway::T3Gateway,
    mcp::{IncomingMessage, McpServer},
};
use serde_json::{Value, json};

async fn call(gateway: &T3Gateway, name: &str, arguments: Value) -> Value {
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
    assert!(response.get("error").is_none(), "{response}");
    let result = response["result"].clone();
    let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, result["structuredContent"], "{response}");
    result
}

fn success(result: &Value) -> &Value {
    assert_ne!(result["isError"], true, "{result}");
    &result["structuredContent"]
}

fn error(result: &Value, code: &str) {
    assert_eq!(result["isError"], true, "{result}");
    assert_eq!(
        result["structuredContent"]["error"]["code"], code,
        "{result}"
    );
}

#[tokio::test]
async fn explicit_archived_queries_select_archived_rows_without_changing_default_exclusion() {
    let fixture = Fixture::new().await;
    let mut open = thread();
    open["id"] = json!("open-thread");
    open["latestTurn"] = Value::Null;
    open["session"] = json!({"status":"idle"});
    let mut archived = open.clone();
    archived["id"] = json!("archived-thread");
    archived["archivedAt"] = json!("2026-10-18T00:00:00.000Z");
    let mut other = archived.clone();
    other["id"] = json!("other-project-thread");
    other["projectId"] = json!("other-project");
    *fixture.shell_threads.lock().unwrap() = vec![open, archived, other];

    for include_archived in [None, Some(false), Some(true)] {
        let mut arguments = json!({"projectId":"project-baseline", "status":"archived"});
        if let Some(include_archived) = include_archived {
            arguments["includeArchived"] = json!(include_archived);
        }
        let result = call(&fixture.gateway, "t3_threads_list", arguments).await;
        let value = success(&result);
        assert_eq!(value["scope"], "filtered");
        assert_eq!(value["page"]["total"], 1);
        assert_eq!(value["page"]["items"][0]["id"], "archived-thread");
        assert_eq!(value["page"]["items"][0]["status"], "archived");
    }

    for arguments in [json!({}), json!({"status":"all"})] {
        let result = call(&fixture.gateway, "t3_threads_list", arguments).await;
        let value = success(&result);
        assert_eq!(value["page"]["total"], 1);
        assert_eq!(value["page"]["items"][0]["id"], "open-thread");
        assert_eq!(value["page"]["items"][0]["status"], "open");
    }
    let result = call(
        &fixture.gateway,
        "t3_threads_list",
        json!({"status":"all","includeArchived":true,"sort":"title"}),
    )
    .await;
    assert_eq!(success(&result)["page"]["total"], 3);
}

#[tokio::test]
async fn git_inspector_errors_keep_typed_codes_through_mcp() {
    let fixture = Fixture::new().await;
    let identity = thread();
    let arguments = json!({"projectId":identity["projectId"],"threadId":identity["id"]});
    let missing = fixture.directory.join("absent-worktree");
    assert!(!missing.exists());
    let file = fixture.directory.join("workspace-file");
    std::fs::write(&file, "not a directory").unwrap();
    for (path, code) in [
        (&missing, "workspace_missing"),
        (&file, "workspace_not_directory"),
        (&fixture.directory, "not_git_worktree"),
    ] {
        fixture.shell_threads.lock().unwrap()[0]["worktreePath"] = json!(path);
        let result = call(&fixture.gateway, "t3_git_status", arguments.clone()).await;
        error(&result, code);
    }

    let repository = fixture.directory.join("repository");
    std::fs::create_dir(&repository).unwrap();
    let output = std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&repository)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    fixture.shell_threads.lock().unwrap()[0]["worktreePath"] = json!(repository);
    let result = call(&fixture.gateway, "t3_git_status", arguments.clone()).await;
    assert_eq!(success(&result)["clean"], true);
    for (tool, extra) in [
        ("t3_git_diff", json!({"mode":"unstaged"})),
        ("t3_git_compare", json!({"baseRevision":"HEAD"})),
    ] {
        for path in ["../outside", "/absolute/path", ":(glob)*"] {
            let mut input = arguments.clone();
            input
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            input["paths"] = json!([path]);
            let result = call(&fixture.gateway, tool, input).await;
            error(&result, "invalid_git_path");
        }
    }
    assert!(dispatch_types(&fixture).is_empty());
}

#[tokio::test]
async fn read_only_pending_responses_reject_fresh_invalid_and_replayed_inputs() {
    let fixture = Fixture::new().await;
    let approval = json!({
        "threadId":thread()["id"], "requestId":"approval-request", "kind":"approval",
        "decision":"accept", "idempotencyKey":"accepted-approval",
    });
    let user_input = json!({
        "threadId":thread()["id"], "requestId":"input-request", "kind":"user_input",
        "answers":{"question":"answer"}, "idempotencyKey":"accepted-input",
    });
    for arguments in [&approval, &user_input] {
        let result = call(
            &fixture.gateway,
            "t3_pending_action_respond",
            arguments.clone(),
        )
        .await;
        assert_eq!(success(&result)["status"], "accepted");
        let replay = call(
            &fixture.gateway,
            "t3_pending_action_respond",
            arguments.clone(),
        )
        .await;
        assert_eq!(success(&replay), success(&result));
    }
    assert_eq!(
        dispatch_types(&fixture),
        ["thread.approval.respond", "thread.user-input.respond"]
    );
    let journal = std::fs::read(fixture.directory.join("operations.json")).unwrap();
    let mut config = fixture.gateway.config().clone();
    config.read_only = true;
    let read_only = T3Gateway::new(config);

    for arguments in [&approval, &user_input] {
        let mut fresh = arguments.clone();
        fresh["idempotencyKey"] = json!(format!("fresh-{}", arguments["kind"].as_str().unwrap()));
        let mut invalid = fresh.clone();
        let field = if arguments["kind"] == "approval" {
            "decision"
        } else {
            "answers"
        };
        invalid.as_object_mut().unwrap().remove(field);
        let writable_result = call(
            &fixture.gateway,
            "t3_pending_action_respond",
            invalid.clone(),
        )
        .await;
        error(
            &writable_result,
            if field == "decision" {
                "decision_required"
            } else {
                "answers_required"
            },
        );
        let mut changed_receipt = arguments.clone();
        changed_receipt["requestId"] = json!("changed-request");
        for input in [fresh, invalid, arguments.clone(), changed_receipt] {
            let result = call(&read_only, "t3_pending_action_respond", input).await;
            error(&result, "gateway_read_only");
            assert_eq!(
                dispatch_types(&fixture),
                ["thread.approval.respond", "thread.user-input.respond"]
            );
            assert_eq!(
                std::fs::read(fixture.directory.join("operations.json")).unwrap(),
                journal
            );
        }
    }
}
