use super::{TIME, descriptor, shell_value};
use crate::{
    config::{GatewayConfig, NativeEndpointConfig},
    gateway::T3Gateway,
    mcp::{IncomingMessage, McpServer},
    t3::test_server::{Reply, Server},
};
use futures_util::FutureExt;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::{Arc, Mutex}, time::Duration};

struct OverviewFixture {
    gateway: T3Gateway,
    server: Server,
    rows: Arc<Mutex<Vec<Value>>>,
    directory: PathBuf,
}

impl OverviewFixture {
    async fn new(rows: Vec<Value>, enrichment_delay: Duration) -> Self {
        let rows = Arc::new(Mutex::new(rows));
        let source = Arc::clone(&rows);
        let server = Server::start(move |request| {
            let source = Arc::clone(&source);
            async move {
                match request.path.as_str() {
                    "/.well-known/t3/environment" => Reply::json(descriptor(1)),
                    "/api/orchestration/shell" => Reply::json(json!({
                        "snapshotSequence":10, "projects":[],
                        "threads":source.lock().unwrap().clone(), "updatedAt":TIME,
                    })),
                    path if path.starts_with("/api/orchestration/threads/") => {
                        tokio::time::sleep(enrichment_delay).await;
                        let id = path.rsplit('/').next().unwrap();
                        let thread = source.lock().unwrap().iter().find(|row| row["id"] == id).cloned().unwrap();
                        Reply::json(json!({"snapshotSequence":10,"thread":thread}))
                    }
                    _ => panic!("unexpected overview request {} {}", request.method, request.path),
                }
            }.boxed()
        }).await;
        let directory = std::env::temp_dir().join(format!("t3-retirement-overview-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let gateway = T3Gateway::new(GatewayConfig {
            endpoint: NativeEndpointConfig {
                t3_http_base_url: server.url.clone(),
                t3_native_mcp_url: format!("{}/mcp", server.url),
                data_dir: directory.clone(),
            },
            t3_access_token: "overview-fixture-token".into(),
            mcp_bearer_token: None,
            read_only: true,
            host: "127.0.0.1".into(),
            port: 8787,
            max_body_bytes: 2 * 1024 * 1024,
            environment_id: Some("env".into()),
            environment_label: None,
            worktree_root: None,
            stale_after_ms: 30_000,
        });
        Self { gateway, server, rows, directory }
    }

    async fn overview(&self, arguments: Value) -> Value {
        let response = McpServer::gateway().handle_message(
            IncomingMessage::parse(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"t3_threads_overview","arguments":arguments}})).unwrap(),
            &self.gateway,
        ).await.unwrap();
    let response = serde_json::to_value(response).unwrap();
        assert!(response.get("error").is_none(), "{response}");
        let result = &response["result"];
        assert_ne!(result["isError"], true, "{result}");
        let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, result["structuredContent"]);
        text
    }
}

impl Drop for OverviewFixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.directory).unwrap();
    }
}

fn completed_row(id: &str) -> Value {
    let mut row = shell_value();
    row["id"] = json!(id);
    row["title"] = json!(id);
    row["session"] = json!({"status":"ready"});
    row["latestTurn"] = json!({"turnId":format!("turn-{id}"),"state":"completed","requestedAt":TIME,"completedAt":TIME});
    row["messages"] = json!([
        {"id":format!("response-{id}"),"role":"assistant","turnId":format!("turn-{id}"),"text":format!("Release note for {id}. {} OMITTED_END", "Useful result details. ".repeat(40))},
        {"id":format!("unrelated-{id}"),"role":"assistant","turnId":"another-turn","text":"WRONG_TURN_RESPONSE"},
    ]);
    row
}

#[tokio::test]
async fn populated_mixed_overview_caps_highlights_and_keeps_useful_bounded_excerpts() {
    let mut rows = (0..7).map(|i| completed_row(&format!("done-{i}"))).collect::<Vec<_>>();
    for (id, field, value) in [
        ("approval", "hasPendingApprovals", json!(true)),
        ("question", "hasPendingUserInput", json!(true)),
        ("failed", "session", json!({"status":"error","updatedAt":TIME})),
        ("running", "session", json!({"status":"running","updatedAt":chrono::Utc::now().to_rfc3339()})),
        ("snoozed", "snoozedUntil", json!("2099-01-01T00:00:00.000Z")),
        ("settled", "settledOverride", json!("settled")),
        ("archived", "archivedAt", json!(TIME)),
    ] {
        let mut row = completed_row(id);
        row[field] = value;
        if id == "running" || id == "failed" {
            row["latestTurn"]["state"] = json!(if id == "running" {"running"} else {"error"});
        }
        rows.push(row);
    }
    let fixture = OverviewFixture::new(rows, Duration::ZERO).await;
    let overview = fixture.overview(json!({"includeArchived":true,"runningLimit":1})).await;
    assert_eq!(overview["total"], 14);
    assert_eq!(overview["counts"], json!({"open":11,"snoozed":1,"settled":1,"archived":1}));
    assert_eq!(overview["executionCounts"], json!({"running":1,"starting":0,"awaiting_approval":1,"awaiting_input":1,"failed":1,"idle":10}));
    assert_eq!(overview["runningCount"], 1);
    assert_eq!(overview["running"].as_array().unwrap().len(), 1);
    assert_eq!(overview["running"][0]["id"], "running");
    let highlights = overview["highlights"].as_array().unwrap();
    assert_eq!(highlights.len(), 5);
    for required in ["approval", "question", "failed"] {
        assert!(highlights.iter().any(|row| row["id"] == required));
    }
    for row in highlights {
        let id = row["id"].as_str().unwrap();
        assert!(id.starts_with("done-") || ["approval","question","failed"].contains(&id));
        assert_eq!(row["status"], "open");
        let excerpt = row["latestResponseExcerpt"].as_str().expect("highlight must carry a useful response");
        assert!(excerpt.starts_with(&format!("Release note for {id}.")), "{excerpt}");
        assert!(excerpt.contains("Useful result details."));
        assert!(excerpt.encode_utf16().count() <= 201);
        assert!(!excerpt.contains("OMITTED_END"));
        assert!(!excerpt.contains("WRONG_TURN_RESPONSE"));
    }
    let no_running = fixture.overview(json!({"includeArchived":false,"runningLimit":0})).await;
    assert_eq!(no_running["total"], 13);
    assert_eq!(no_running["counts"]["archived"], 0);
    assert_eq!(no_running["runningCount"], 1);
    assert_eq!(no_running["running"], json!([]));
    assert_eq!(no_running["highlights"].as_array().unwrap().len(), 5);
    assert!(fixture.server.requests.lock().unwrap().iter().all(|request| request.method == "GET"));
}

#[tokio::test]
async fn successive_public_overviews_select_current_lifecycle_and_report_conflicting_signals() {
    let mut row = completed_row("changing");
    row["session"] = json!({"status":"running","updatedAt":chrono::Utc::now().to_rfc3339()});
    let fixture = OverviewFixture::new(vec![row.clone()], Duration::ZERO).await;
    let first = fixture.overview(json!({"runningLimit":1})).await;
    assert_eq!(first["counts"]["open"], 1);
    assert_eq!(first["runningCount"], 1);
    assert_eq!(first["running"][0]["quality"], "inconsistent");
    assert_eq!(first["running"][0]["hasConflictingSignals"], true);
    assert!(first["running"][0]["warning"].as_str().unwrap().contains("disagree"));
    assert_eq!(first["highlights"], json!([]));
    row["session"] = json!({"status":"ready"});
    row["settledOverride"] = json!("settled");
    *fixture.rows.lock().unwrap() = vec![row.clone()];
    let settled = fixture.overview(json!({"runningLimit":1})).await;
    assert_eq!(settled["counts"], json!({"open":0,"settled":1,"snoozed":0,"archived":0}));
    assert_eq!(settled["runningCount"], 0);
    assert_eq!(settled["highlights"], json!([]));
    row["pinnedAt"] = json!(TIME);
    row["latestTurn"]["state"] = json!("running");
    *fixture.rows.lock().unwrap() = vec![row.clone()];
    let opposite = fixture.overview(json!({"runningLimit":1})).await;
    assert_eq!(opposite["counts"]["open"], 1);
    assert_eq!(opposite["running"][0]["sessionStatus"], "ready");
    assert_eq!(opposite["running"][0]["latestTurn"]["state"], "running");
    assert_eq!(opposite["running"][0]["quality"], "inconsistent");
    assert_eq!(opposite["highlights"], json!([]));
    row["latestTurn"]["state"] = json!("completed");
    *fixture.rows.lock().unwrap() = vec![row];
    let recovered = fixture.overview(json!({"runningLimit":1})).await;
    assert_eq!(recovered["runningCount"], 0);
    assert_eq!(recovered["counts"]["open"], 1);
    assert_eq!(recovered["highlights"][0]["quality"], "fresh");
    assert_eq!(recovered["highlights"][0]["hasConflictingSignals"], false);
    assert!(recovered["highlights"][0]["warning"].is_null());
    assert!(recovered["highlights"][0]["latestResponseExcerpt"].as_str().unwrap().starts_with("Release note for changing."));
    assert_eq!(fixture.server.requests.lock().unwrap().iter().filter(|request| request.path == "/api/orchestration/shell").count(), 4);
}

#[tokio::test]
async fn overview_rows_and_targets_share_the_aggregate_observation_timestamp() {
    let mut running = completed_row("running");
    running["session"] = json!({"status":"running","updatedAt":chrono::Utc::now().to_rfc3339()});
    running["latestTurn"]["state"] = json!("running");
    let fixture = OverviewFixture::new(vec![completed_row("one"),completed_row("two"),running], Duration::from_millis(25)).await;
    let overview = fixture.overview(json!({"runningLimit":1})).await;
    assert_eq!(overview["highlights"].as_array().unwrap().len(), 2);
    assert_eq!(overview["running"].as_array().unwrap().len(), 1);
    assert!(crate::t3::timestamp(overview["observedAt"].as_str()).is_some());
    for row in overview["highlights"].as_array().unwrap().iter().chain(overview["running"].as_array().unwrap()) {
        assert_eq!(row["observedAt"], overview["observedAt"], "all rows must describe the same overview observation");
        assert_eq!(row["observedTarget"]["observedAt"], overview["observedAt"]);
        assert_eq!(row["observedTarget"]["threadId"], row["id"]);
        assert_eq!(row["observedTarget"]["turnId"], row["observedTurnId"]);
        assert_eq!(row["observedTarget"]["environmentId"], "env");
    }
}
