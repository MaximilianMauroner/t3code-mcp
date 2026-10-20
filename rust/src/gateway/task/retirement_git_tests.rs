use super::{Fixture, dispatch_types, start_args, thread};
use crate::{
    gateway::T3Gateway,
    mcp::{IncomingMessage, McpServer},
    t3::test_server::{Reply, Server},
};
use futures_util::FutureExt;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};

fn git(repository: &Path, arguments: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=Retirement Fixture",
            "-c",
            "user.email=fixture@example.test",
        ])
        .args(arguments)
        .current_dir(repository)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

async fn workspace_fixture() -> (Fixture, Server) {
    let mut fixture = Fixture::empty().await;
    let repository = fixture.directory.join("project");
    std::fs::create_dir(&repository).unwrap();
    git(&repository, &["init", "-b", "main"]);
    for name in ["selected.txt", "other.txt"] {
        std::fs::write(repository.join(name), "baseline\n").unwrap();
    }
    git(&repository, &["add", "."]);
    git(&repository, &["commit", "-m", "Baseline"]);
    let root = repository.clone();
    let threads = Arc::clone(&fixture.shell_threads);
    let upstream = fixture.server.url.clone();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let proxy =
        Server::start(move |request| {
            let root = root.clone();
            let threads = Arc::clone(&threads);
            let upstream = upstream.clone();
            let client = client.clone();
            async move {
            if request.path == "/api/orchestration/shell" {
                return Reply::json(json!({
                    "snapshotSequence":3,
                    "projects":[{
                        "id":"project-baseline", "title":"Baseline project", "workspaceRoot":root,
                        "defaultModelSelection":{
                            "instanceId":"codex_openai","model":"gpt-6.1-sol"
                        }
                    }],
                    "threads":threads.lock().unwrap().clone(),
                    "updatedAt":"2026-10-18T00:00:00.000Z",
                }));
            }
            let response = client
                .request(
                    request.method.parse().unwrap(),
                    format!("{upstream}{}", request.path),
                )
                .json(&request.body)
                .send()
                .await
                .unwrap();
            let status = response.status().as_u16();
            let bytes = response.bytes().await.unwrap();
            Reply::status(status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
        }
        .boxed()
        })
        .await;
    let mut config = fixture.gateway.config().clone();
    config.endpoint.t3_http_base_url = proxy.url.clone();
    config.endpoint.t3_native_mcp_url = format!("{}/mcp", proxy.url);
    config.worktree_root = Some(fixture.directory.join("managed"));
    fixture.gateway = T3Gateway::new(config);
    (fixture, proxy)
}

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
    let response = serde_json::to_value(response).unwrap();
    assert!(response.get("error").is_none(), "{response}");
    let result = response["result"].clone();
    let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap())
        .unwrap_or_else(|error| panic!("{name}: {error}; MCP response: {response}"));
    assert_eq!(text, result["structuredContent"], "{response}");
    result
}

async fn value(gateway: &T3Gateway, name: &str, arguments: Value) -> Value {
    let result = call(gateway, name, arguments).await;
    assert_ne!(result["isError"], true, "{result}");
    result["structuredContent"].clone()
}

#[tokio::test]
async fn mcp_compare_selects_the_thread_worktree_and_bounds_committed_paths() {
    let (fixture, _proxy) = workspace_fixture().await;
    let repository = fixture.directory.join("project");
    let worktree = fixture.directory.join("selected-worktree");
    let base = git(&repository, &["rev-parse", "HEAD"]);
    git(
        &repository,
        &[
            "worktree",
            "add",
            "-b",
            "thread-branch",
            worktree.to_str().unwrap(),
        ],
    );
    std::fs::write(
        worktree.join("selected.txt"),
        "thread committed line\n".repeat(200),
    )
    .unwrap();
    std::fs::write(worktree.join("other.txt"), "excluded committed line\n").unwrap();
    git(&worktree, &["commit", "-am", "Thread changes"]);
    let head = git(&worktree, &["rev-parse", "HEAD"]);
    assert_ne!(base, head);
    std::fs::write(worktree.join("selected.txt"), "uncommitted thread line\n").unwrap();
    let mut selected = thread();
    selected["branch"] = json!("thread-branch");
    selected["worktreePath"] = json!(worktree);
    *fixture.shell_threads.lock().unwrap() = vec![selected.clone()];
    let mut arguments = json!({
        "projectId":"project-baseline", "baseRevision":base, "headRevision":"HEAD",
        "paths":["selected.txt"], "maxBytes":100_000,
    });
    let project = value(&fixture.gateway, "t3_git_compare", arguments.clone()).await;
    assert_eq!(project["comparison"]["headCommit"], base);
    assert_eq!(project["patch"], "");
    arguments["threadId"] = selected["id"].clone();
    let compared = value(&fixture.gateway, "t3_git_compare", arguments.clone()).await;
    assert_eq!(compared["workspace"]["source"], "thread_worktree");
    assert_eq!(
        compared["workspace"]["projectWorkspaceRoot"],
        json!(repository)
    );
    assert_eq!(compared["workspace"]["selectedPath"], json!(worktree));
    assert_eq!(compared["workspace"]["repositoryRoot"], json!(worktree));
    assert_eq!(compared["workspace"]["threadId"], selected["id"]);
    assert_eq!(
        compared["comparison"],
        json!({"requestedBase":base,"requestedHead":"HEAD","baseCommit":base,"headCommit":head})
    );
    assert_eq!(compared["paths"], json!(["selected.txt"]));
    assert_eq!(compared["attribution"]["quality"], "working_tree_dirty");
    assert_eq!(compared["truncation"]["truncated"], false);
    let patch = compared["patch"].as_str().unwrap();
    assert!(patch.contains("-baseline\n"));
    assert!(patch.contains("+thread committed line\n"));
    assert!(!patch.contains("other.txt"));
    assert!(!patch.contains("excluded committed line"));
    assert!(!patch.contains("uncommitted thread line"));
    let byte_limit = 1024;
    assert!(patch.is_ascii() && patch.len() > byte_limit);
    arguments["maxBytes"] = json!(byte_limit);
    let bounded = value(&fixture.gateway, "t3_git_compare", arguments).await;
    assert_eq!(bounded["patch"], &patch[..byte_limit]);
    assert_eq!(
        bounded["truncation"],
        json!({"truncated":true,"reason":"byte_limit","maxBytes":byte_limit,
            "capturedBytes":byte_limit,"totalBytes":patch.len(),"omittedBytes":patch.len()-byte_limit})
    );
    assert_eq!(git(&repository, &["rev-parse", "HEAD"]), base);
    assert_eq!(
        std::fs::read_to_string(worktree.join("selected.txt")).unwrap(),
        "uncommitted thread line\n"
    );
    assert!(dispatch_types(&fixture).is_empty());
}

#[tokio::test]
async fn mcp_task_origin_selection_reaches_the_created_thread_and_survives_restart() {
    for start_from_origin in [true, false] {
        let (fixture, _proxy) = workspace_fixture().await;
        let repository = fixture.directory.join("project");
        let origin = fixture.directory.join("origin.git");
        let origin_head = git(&repository, &["rev-parse", "HEAD"]);
        git(
            &repository,
            &["clone", "--bare", ".", origin.to_str().unwrap()],
        );
        git(
            &repository,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        std::fs::write(repository.join("selected.txt"), "local committed line\n").unwrap();
        git(&repository, &["commit", "-am", "Local changes"]);
        let local_head = git(&repository, &["rev-parse", "HEAD"]);
        assert_ne!(origin_head, local_head);
        std::fs::write(repository.join("selected.txt"), "local uncommitted line\n").unwrap();
        let mut arguments = Value::Object(start_args());
        arguments["workspaceMode"] = json!("worktree");
        arguments["branch"] = json!("main");
        arguments["startFromOrigin"] = json!(start_from_origin);
        let started = value(&fixture.gateway, "t3_task_start", arguments.clone()).await;
        assert_eq!(started["stage"], "run_accepted");
        let commands: Vec<Value> = fixture
            .server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.path == "/api/orchestration/dispatch")
            .map(|request| request.body.clone())
            .collect();
        assert_eq!(commands.len(), 2);
        let created = &commands[0];
        assert_eq!(created["type"], "thread.create");
        assert_eq!(created["projectId"], "project-baseline");
        assert_eq!(created["threadId"], started["threadId"]);
        assert_eq!(commands[1]["type"], "thread.turn.start");
        assert_eq!(commands[1]["threadId"], created["threadId"]);
        assert_eq!(commands[1]["message"]["text"], arguments["instruction"]);
        let worktree = Path::new(created["worktreePath"].as_str().unwrap());
        assert_ne!(worktree, repository.as_path());
        assert_eq!(
            git(worktree, &["branch", "--show-current"]),
            created["branch"].as_str().unwrap()
        );
        let expected_head = if start_from_origin {
            origin_head
        } else {
            local_head
        };
        assert_eq!(git(worktree, &["rev-parse", "HEAD"]), expected_head);
        assert_eq!(started["baselineRevision"], expected_head);
        assert_eq!(
            std::fs::read_to_string(worktree.join("selected.txt")).unwrap(),
            if start_from_origin {
                "baseline\n"
            } else {
                "local committed line\n"
            }
        );
        let reopened = T3Gateway::new(fixture.gateway.config().clone());
        let retried = value(&reopened, "t3_task_start", arguments.clone()).await;
        for field in [
            "taskRef",
            "threadId",
            "runId",
            "threadOperationId",
            "runOperationId",
        ] {
            assert_eq!(retried[field], started[field], "{field}");
        }
        assert_eq!(retried["stage"], "run_accepted");
        let status = value(
            &reopened,
            "t3_git_status",
            json!({"projectId":"project-baseline","threadId":started["threadId"]}),
        )
        .await;
        assert_eq!(status["workspace"]["selectedPath"], created["worktreePath"]);
        assert_eq!(status["branch"]["headCommit"], expected_head);
        assert_eq!(status["clean"], true);
        arguments["startFromOrigin"] = json!(!start_from_origin);
        let conflict = call(&reopened, "t3_task_start", arguments).await;
        assert_eq!(conflict["isError"], true);
        assert_eq!(
            conflict["structuredContent"]["error"]["code"],
            "idempotency_conflict"
        );
        assert_eq!(
            dispatch_types(&fixture),
            ["thread.create", "thread.turn.start"]
        );
        assert_eq!(
            std::fs::read_to_string(repository.join("selected.txt")).unwrap(),
            "local uncommitted line\n"
        );
    }
}

#[tokio::test]
async fn mcp_failed_worktree_preparation_keeps_a_rejected_receipt_without_dispatch() {
    for missing_revision in [true, false] {
        let (mut fixture, _proxy) = workspace_fixture().await;
        let repository = fixture.directory.join("project");
        let managed = fixture.directory.join("managed");
        if !missing_revision {
            std::fs::write(&managed, "blocks worktree creation\n").unwrap();
        }
        let mut arguments = Value::Object(start_args());
        arguments["workspaceMode"] = json!("worktree");
        arguments["branch"] = json!(if missing_revision { "not-yet" } else { "main" });
        arguments["startFromOrigin"] = json!(true);
        let rejected = value(&fixture.gateway, "t3_task_start", arguments.clone()).await;
        assert_eq!(rejected["stage"], "rejected");
        assert!(rejected["thread"].is_null());
        assert!(rejected["run"].is_null());
        assert!(rejected["runId"].is_null());
        assert!(rejected["runOperationId"].is_null());
        assert!(
            rejected["lastError"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
        );
        assert!(dispatch_types(&fixture).is_empty());
        assert!(fixture.shell_threads.lock().unwrap().is_empty());
        let stored = std::fs::read_to_string(fixture.directory.join("operations.json")).unwrap();
        assert!(!stored.contains(arguments["instruction"].as_str().unwrap()));
        if missing_revision {
            git(&repository, &["branch", "not-yet", "main"]);
        } else {
            std::fs::remove_file(&managed).unwrap();
        }
        fixture.gateway = T3Gateway::new(fixture.gateway.config().clone());
        let recovered = value(
            &fixture.gateway,
            "t3_task_get",
            json!({"taskRef":rejected["taskRef"]}),
        )
        .await;
        assert_eq!(recovered["task"]["stage"], "rejected");
        assert_eq!(
            recovered["task"]["threadOperationId"],
            rejected["threadOperationId"]
        );
        let retried = value(&fixture.gateway, "t3_task_start", arguments.clone()).await;
        for field in ["taskRef", "stage", "threadOperationId", "lastError"] {
            assert_eq!(retried[field], rejected[field], "{field}");
        }
        assert!(retried["thread"].is_null());
        assert!(retried["run"].is_null());
        assert!(dispatch_types(&fixture).is_empty());
        assert!(fixture.shell_threads.lock().unwrap().is_empty());
        let mut changed = arguments.clone();
        changed["startFromOrigin"] = json!(false);
        let conflict = call(&fixture.gateway, "t3_task_start", changed).await;
        assert_eq!(conflict["isError"], true);
        assert_eq!(
            conflict["structuredContent"]["error"]["code"],
            "idempotency_conflict"
        );
        assert!(dispatch_types(&fixture).is_empty());
        arguments["idempotencyKey"] = json!("repaired-workspace-fresh-task");
        let fresh = value(&fixture.gateway, "t3_task_start", arguments).await;
        assert_eq!(fresh["stage"], "run_accepted");
        assert_ne!(fresh["taskRef"], rejected["taskRef"]);
        assert_eq!(
            dispatch_types(&fixture),
            ["thread.create", "thread.turn.start"]
        );
    }
}
