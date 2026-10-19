use super::*;
use crate::t3::test_server::{Reply, Request, Server};
use std::sync::atomic::{AtomicUsize, Ordering};

#[path = "native_advertisement_tests.rs"]
mod advertisement;

pub(super) struct Credentials {
    dir: std::path::PathBuf,
}
impl Credentials {
    pub(super) fn new() -> Self {
        Self {
            dir: std::env::temp_dir().join(format!("t3-native-test-{}", uuid::Uuid::new_v4())),
        }
    }
    pub(super) fn store(&self) -> NativeCredentialStore {
        NativeCredentialStore::in_data_dir(&self.dir)
    }
    pub(super) fn write(&self, token: &str) {
        self.store()
            .write(&NativeCredential {
                resource: "http://localhost/mcp".into(),
                client_id: "client".into(),
                access_token: token.into(),
                access: crate::native_credential::NativeAccessLevel::FullAccess,
                scopes: vec!["mcp".into()],
                obtained_at: "2026-01-01T00:00:00.000Z".into(),
                expires_at: "2099-01-01T00:00:00.000Z".into(),
            })
            .unwrap();
    }
    pub(super) fn client(&self, server: &Server) -> NativeMcpClient {
        NativeMcpClient::new(
            format!("{}/mcp", server.url),
            self.store(),
            None,
            Duration::from_secs(2),
        )
    }
}
impl Drop for Credentials {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
pub(super) fn initialize(r: &Request) -> Option<Reply> {
    if r.method == "GET" {
        return Some(Reply::status(405, json!({})));
    }
    if r.method == "DELETE" {
        return Some(Reply::json(json!({})));
    }
    if r.body["method"] == "initialize" {
        let mut reply = Reply::json(
            json!({"jsonrpc":"2.0","id":r.body["id"],"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{"listChanged":true}},"serverInfo":{"name":"native-test","version":"1"}}}),
        );
        reply.headers.push((
            "mcp-session-id".into(),
            format!(
                "session-{}",
                r.headers
                    .get("authorization")
                    .cloned()
                    .unwrap_or_default()
                    .replace(' ', "-")
            ),
        ));
        return Some(reply);
    }
    if r.body.get("id").is_none() {
        return Some(Reply::status(202, Value::Null));
    }
    None
}
pub(super) fn response(r: &Request, result: Value) -> Reply {
    Reply::json(json!({"jsonrpc":"2.0","id":r.body["id"],"result":result}))
}
fn expire(client: &NativeMcpClient) {
    client
        .0
        .state
        .lock()
        .unwrap()
        .catalog
        .as_mut()
        .unwrap()
        .expires_at = Instant::now() - Duration::from_secs(1);
}
async fn refreshed(client: &NativeMcpClient, old: Option<String>) -> NativeSnapshot {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = client.snapshot().await;
            if snapshot.fetched_at != old || snapshot.error.is_some() {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn issue_19_lists_every_page_and_preserves_output_schemas_on_refresh() {
    let tools: Vec<Value> = serde_json::from_str(include_str!(
        "../../../tests/fixtures/node-compat-tools.json"
    ))
    .unwrap();
    let server=Server::start(move|r|{let schemas=tools.clone();async move{
        if let Some(reply)=initialize(&r){return reply;}
        if r.body["method"]=="tools/list"{
            if r.body["params"].get("cursor").is_none(){
                let mut page=schemas;for i in 0..75{page.push(json!({"name":format!("output-schema-{i}"),"inputSchema":{"type":"object"},"outputSchema":{"type":"object","properties":{"value":{"type":"string","description":format!("schema-{i}")}}}}));}
                page.push(json!({"name":"t3_thread_list","inputSchema":{"type":"object"},"outputSchema":{"type":"object"}}));response(&r,json!({"tools":page,"nextCursor":""}))
            }else{assert_eq!(r.body["params"]["cursor"],"");response(&r,json!({"tools":[{"name":"t3_thread_send","inputSchema":{"type":"object"}}]}))}
        }else{response(&r,json!({"content":[{"type":"text","text":"ok"}],"structuredContent":{"threads":[]}}))}
    }.boxed()}).await;
    let credentials = Credentials::new();
    credentials.write("one");
    let client = credentials.client(&server);
    let initial = client.snapshot().await;
    assert_eq!(initial.error, None);
    assert_eq!(
        initial
            .tools
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        ["t3_thread_list", "t3_thread_send"]
    );
    assert_eq!(
        serde_json::to_value(&initial.tools).unwrap(),
        json!([
            {"name":"t3_thread_list","inputSchema":{"type":"object"},"outputSchema":{"type":"object"}},
            {"name":"t3_thread_send","inputSchema":{"type":"object"}}
        ])
    );
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(2)).await;
        let before = client.snapshot().await.fetched_at;
        expire(&client);
        let stale = client.snapshot().await;
        assert_eq!(stale.tools, initial.tools);
        let fresh = refreshed(&client, before).await;
        assert!(fresh.error.is_none());
        assert_eq!(fresh.tools, initial.tools);
    }
    let pages: Vec<_> = server
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.body["method"] == "tools/list")
        .map(|r| r.body["params"].clone())
        .collect();
    assert_eq!(pages.len(), 22);
    for pair in pages.as_chunks::<2>().0 {
        assert_eq!(*pair, [json!({}), json!({"cursor":""})]);
    }
    assert_eq!(
        client
            .call_tool(
                json!({"name":"t3_thread_list","arguments":{}})
                    .as_object()
                    .unwrap()
                    .clone(),
                false,
                None,
            )
            .await
            .unwrap()
            .extra["structuredContent"],
        json!({"threads":[]})
    );
    client.close();
}
#[tokio::test]
async fn session_404_is_retried_only_by_native_client() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let server = Server::start(move |r| {
        let count = count.clone();
        async move {
            if let Some(reply) = initialize(&r) {
                return reply;
            }
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                Reply::status(404, json!({"error":"unknown session"}))
            } else {
                response(
                    &r,
                    json!({"content":[],"structuredContent":{"status":"running"}}),
                )
            }
        }
        .boxed()
    })
    .await;
    let credentials = Credentials::new();
    credentials.write("one");
    let client = credentials.client(&server);
    let result = client
        .call_tool(
            json!({"name":"t3_thread_send","arguments":{}})
                .as_object()
                .unwrap()
                .clone(),
            true,
            None,
        )
        .await
        .unwrap();
    assert_eq!(result.extra["structuredContent"]["status"], "running");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let requests = server.requests.lock().unwrap();
    let methods = requests
        .iter()
        .filter_map(|r| r.body["method"].as_str())
        .filter(|m| *m == "initialize" || *m == "tools/call")
        .collect::<Vec<_>>();
    assert_eq!(
        methods,
        ["initialize", "tools/call", "initialize", "tools/call"]
    );
    drop(requests);
    client.close();
}
#[tokio::test]
async fn lost_mutation_is_uncertain_and_lost_read_retries_once() {
    for mutating in [true, false] {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let server = Server::start(move |r| {
            let count = count.clone();
            async move {
                if let Some(reply) = initialize(&r) {
                    return reply;
                }
                if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    Reply {
                        drop_response: true,
                        ..Reply::json(Value::Null)
                    }
                } else {
                    response(&r, json!({"content":[]}))
                }
            }
            .boxed()
        })
        .await;
        let credentials = Credentials::new();
        credentials.write("one");
        let client = credentials.client(&server);
        let result = client
            .call_tool(
                json!({"name":"t3_thread_send","arguments":{}})
                    .as_object()
                    .unwrap()
                    .clone(),
                mutating,
                None,
            )
            .await;
        if mutating {
            assert_eq!(result.unwrap_err().code, "native_outcome_unknown");
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        } else {
            result.unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 2);
        }
        client.close();
    }
}
#[tokio::test]
async fn restart_after_lost_read_uses_third_attempt_but_lost_mutation_stops() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let server = Server::start(move |r| {
        let count = count.clone();
        async move {
            if let Some(reply) = initialize(&r) {
                return reply;
            }
            match count.fetch_add(1, Ordering::SeqCst) {
                0 => Reply {
                    drop_response: true,
                    ..Reply::json(Value::Null)
                },
                1 => Reply::status(404, json!({})),
                _ => response(&r, json!({"content":[]})),
            }
        }
        .boxed()
    })
    .await;
    let credentials = Credentials::new();
    credentials.write("one");
    let client = credentials.client(&server);
    client
        .call_tool(
            json!({"name":"t3_thread_read","arguments":{}})
                .as_object()
                .unwrap()
                .clone(),
            false,
            None,
        )
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    client.close();
}
#[tokio::test]
async fn credential_rejection_at_initialize_has_no_retry() {
    let server =
        Server::start(|_| async { Reply::status(403, json!({"error":"revoked"})) }.boxed()).await;
    let credentials = Credentials::new();
    credentials.write("one");
    let client = credentials.client(&server);
    assert_eq!(
        client
            .call_tool(
                json!({"name":"t3_thread_send","arguments":{}})
                    .as_object()
                    .unwrap()
                    .clone(),
                true,
                None,
            )
            .await
            .unwrap_err()
            .code,
        "native_credential_rejected"
    );
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    client.close();
}
#[tokio::test]
async fn renewals_keep_inflight_calls_and_catalog_visible() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let e = entered.clone();
    let g = release.clone();
    let server = Server::start(move |r| {
        let entered = e.clone();
        let release = g.clone();
        async move {
            if let Some(reply) = initialize(&r) {
                return reply;
            }
            if r.body["method"] == "tools/list" {
                return response(
                    &r,
                    json!({"tools":[{"name":"t3_thread_list","inputSchema":{"type":"object"}}]}),
                );
            }
            if r.body["params"]["name"] == "t3_thread_wait" {
                entered.notify_one();
                release.notified().await;
            }
            response(
                &r,
                json!({"content":[],"structuredContent":{"token":r.headers.get("authorization")}}),
            )
        }
        .boxed()
    })
    .await;
    let credentials = Credentials::new();
    credentials.write("one");
    let client = credentials.client(&server);
    let initial = client.snapshot().await;
    assert!(initial.error.is_none());
    let c = client.clone();
    let old = tokio::spawn(async move {
        c.call_tool(
            json!({"name":"t3_thread_wait","arguments":{}})
                .as_object()
                .unwrap()
                .clone(),
            false,
            None,
        )
        .await
    });
    entered.notified().await;
    credentials.write("two");
    let stale = client.snapshot().await;
    assert_eq!(stale.tools, initial.tools);
    let result = client
        .call_tool(
            json!({"name":"t3_thread_read","arguments":{}})
                .as_object()
                .unwrap()
                .clone(),
            false,
            None,
        )
        .await
        .unwrap();
    assert_eq!(result.extra["structuredContent"]["token"], "Bearer two");
    assert!(!server.requests.lock().unwrap().iter().any(|r| {
        r.method == "DELETE"
            && r.headers
                .get("authorization")
                .is_some_and(|v| v == "Bearer one")
    }));
    release.notify_one();
    let result = old.await.unwrap().unwrap();
    assert_eq!(result.extra["structuredContent"]["token"], "Bearer one");
    std::fs::remove_file(credentials.store().file_path).unwrap();
    let missing = client.snapshot().await;
    assert_eq!(missing.tools, initial.tools);
    assert_eq!(missing.error.unwrap().code, "native_credential_missing");
    client.close();
}
#[test]
fn native_wait_budget_matches_node_timer_limits() {
    let default = Duration::from_millis(120_000);
    assert_eq!(
        timeout_for("t3_thread_wait", &Map::new(), default),
        Duration::from_millis(3_615_000)
    );
    let args = json!({"timeoutMs":1e30}).as_object().unwrap().clone();
    assert_eq!(
        timeout_for("t3_thread_wait", &args, default),
        Duration::from_millis(2_147_483_647)
    );
    let args = json!({"timeoutMs":600_000}).as_object().unwrap().clone();
    assert_eq!(
        timeout_for("t3_thread_wait", &args, default),
        Duration::from_millis(615_000)
    );
    let args = json!({"timeoutMs":"Infinity"}).as_object().unwrap().clone();
    assert_eq!(
        timeout_for("t3_thread_wait", &args, default),
        Duration::from_millis(3_615_000)
    );
}

#[tokio::test]
async fn node_contract_preserves_unknown_result_fields() {
    let upstream = json!({"content":[{"type":"text","text":"ok","nativeExtension":{"value":7}}],"structuredContent":{"status":"running"},"nativeExtension":{"trace":"original"}});
    let expected = json!({"content":[{"type":"text","text":"ok","nativeExtension":{"value":7}}],"structuredContent":{"status":"running"},"nativeExtension":{"trace":"original"}});
    let server = Server::start(move |r| {
        let upstream = upstream.clone();
        async move {
            if let Some(reply) = initialize(&r) {
                return reply;
            }
            response(&r, upstream)
        }
        .boxed()
    })
    .await;
    let credentials = Credentials::new();
    credentials.write("one");
    let client = credentials.client(&server);
    let result = client
        .call_tool(
            json!({"name":"t3_thread_send","arguments":{}})
                .as_object()
                .unwrap()
                .clone(),
            true,
            None,
        )
        .await
        .unwrap();
    client.close();
    assert_eq!(
        serde_json::to_value(result).unwrap(),
        expected,
        "The TypeScript proxy returns native results unchanged."
    );
}
#[tokio::test]
async fn node_contract_rejects_null_catalog_cursor() {
    let server=Server::start(|r|async move{if let Some(reply)=initialize(&r){return reply;}response(&r,json!({"tools":[{"name":"t3_thread_list","inputSchema":{"type":"object"}}],"nextCursor":null}))}.boxed()).await;
    let credentials = Credentials::new();
    credentials.write("one");
    let client = credentials.client(&server);
    let snapshot = client.snapshot().await;
    client.close();
    assert_eq!(
        snapshot.error.map(|e| e.code).as_deref(),
        Some("native_unavailable"),
        "The Node ListToolsResultSchema rejects a present null cursor."
    );
}

#[tokio::test]
async fn cold_catalog_is_single_flight_across_concurrent_callers() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    let gate = release.clone();
    let started = entered.clone();
    let server = Server::start(move |r| {
        let count = observed.clone();
        let release = gate.clone();
        let entered = started.clone();
        async move {
            if let Some(reply) = initialize(&r) {
                return reply;
            }
            count.fetch_add(1, Ordering::SeqCst);
            entered.notify_one();
            release.notified().await;
            response(
                &r,
                json!({"tools":[{"name":"t3_thread_list","inputSchema":{"type":"object"}}]}),
            )
        }
        .boxed()
    })
    .await;
    let credentials = Credentials::new();
    credentials.write("one");
    let client = credentials.client(&server);
    let mut reads = Vec::new();
    for _ in 0..4 {
        let client = client.clone();
        reads.push(tokio::spawn(async move { client.snapshot().await }));
    }
    entered.notified().await;
    release.notify_one();
    for read in reads {
        let snapshot = read.await.unwrap();
        assert!(snapshot.error.is_none());
        assert_eq!(snapshot.tools[0].name, "t3_thread_list");
    }
    assert_eq!(count.load(Ordering::SeqCst), 1);
    client.close();
}

#[tokio::test]
async fn failed_refresh_preserves_the_last_catalog_and_reads_new_credentials() {
    let server = Server::start(|r| {
        async move {
            if r.headers.get("authorization").map(String::as_str) == Some("Bearer two") {
                return Reply::status(403, json!({"reason":"revoked"}));
            }
            if let Some(reply) = initialize(&r) {
                return reply;
            }
            response(
                &r,
                json!({"tools":[{"name":"t3_thread_list","inputSchema":{"type":"object"}}]}),
            )
        }
        .boxed()
    })
    .await;
    let credentials = Credentials::new();
    credentials.write("one");
    let client = credentials.client(&server);
    let initial = client.snapshot().await;
    credentials.write("two");
    assert_eq!(client.snapshot().await.tools, initial.tools);
    let snapshot = refreshed(&client, initial.fetched_at.clone()).await;
    assert_eq!(snapshot.tools, initial.tools);
    assert_eq!(snapshot.fetched_at, initial.fetched_at);
    assert_eq!(snapshot.error.unwrap().code, "native_credential_rejected");
    assert_eq!(
        client
            .call_tool(
                json!({"name":"t3_thread_send","arguments":{}})
                    .as_object()
                    .unwrap()
                    .clone(),
                true,
                None,
            )
            .await
            .unwrap_err()
            .code,
        "native_credential_rejected"
    );
    client.close();
}

#[tokio::test]
async fn malformed_catalog_page_keeps_the_last_complete_snapshot() {
    let pages = Arc::new(AtomicUsize::new(0));
    let count = pages.clone();
    let server = Server::start(move |r| { let count = count.clone(); async move {
        if let Some(reply) = initialize(&r) { return reply; }
        let result = if count.fetch_add(1, Ordering::SeqCst) == 0 { json!({"tools":[{"name":"t3_thread_list","inputSchema":{"type":"object"}}]}) }
        else if r.body["params"].get("cursor").is_none() { json!({"tools":[{"name":"t3_thread_send","inputSchema":{"type":"object"}}],"nextCursor":""}) }
        else { json!({"tools":[],"nextCursor":null}) };
        response(&r, result)
    }.boxed() }).await;
    let credentials = Credentials::new();
    credentials.write("one");
    let client = credentials.client(&server);
    let initial = client.snapshot().await;
    expire(&client);
    client.snapshot().await;
    let failed = refreshed(&client, initial.fetched_at.clone()).await;
    assert_eq!(failed.tools, initial.tools);
    assert_eq!(failed.missing, initial.missing);
    assert_eq!(failed.fetched_at, initial.fetched_at);
    assert_eq!(failed.error.unwrap().code, "native_unavailable");
    assert_eq!(pages.load(Ordering::SeqCst), 3);
    client.close();
}

#[tokio::test]
async fn protocol_rejections_and_http_failures_keep_distinct_retry_outcomes() {
    for (status, protocol, expected, mutating, attempts) in [
        (200, true, "native_rejected", true, 1),
        (500, false, "native_outcome_unknown", true, 1),
        (500, false, "native_unavailable", false, 2),
        (401, false, "native_credential_rejected", true, 1),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let server = Server::start(move |r| { let calls = observed.clone(); async move {
            if let Some(reply) = initialize(&r) { return reply; }
            calls.fetch_add(1, Ordering::SeqCst);
            let body = if protocol { json!({"jsonrpc":"2.0","id":r.body["id"],"error":{"code":-32602,"message":"bad arguments"}}) }
            else { json!({"jsonrpc":"2.0","id":r.body["id"],"error":{"code":-32602,"message":"HTTP evidence"}}) };
            Reply::status(status, body)
        }.boxed() }).await;
        let credentials = Credentials::new();
        credentials.write("one");
        let client = credentials.client(&server);
        let error = client
            .call_tool(
                json!({"name":"t3_thread_send","arguments":{}})
                    .as_object()
                    .unwrap()
                    .clone(),
                mutating,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, expected);
        assert_eq!(calls.load(Ordering::SeqCst), attempts);
        if protocol {
            assert!(error.message.contains("-32602"));
        } else if status == 500 {
            assert!(error.message.contains("HTTP evidence"));
        }
        client.close();
    }
}

#[tokio::test]
async fn timeout_does_not_close_the_shared_connection_or_repeat_a_mutation() {
    let server = Server::start(|r| {
        async move {
            if let Some(reply) = initialize(&r) {
                return reply;
            }
            if r.body["params"]["name"] == "t3_thread_send" {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            response(&r, json!({"content":[]}))
        }
        .boxed()
    })
    .await;
    let credentials = Credentials::new();
    credentials.write("one");
    let client = NativeMcpClient::new(
        format!("{}/mcp", server.url),
        credentials.store(),
        None,
        Duration::from_millis(40),
    );
    let send = client.call_tool(
        json!({"name":"t3_thread_send","arguments":{}})
            .as_object()
            .unwrap()
            .clone(),
        true,
        None,
    );
    let read = client.call_tool(
        json!({"name":"t3_thread_read","arguments":{}})
            .as_object()
            .unwrap()
            .clone(),
        false,
        None,
    );
    let (send, read) = tokio::join!(send, read);
    assert_eq!(send.unwrap_err().code, "native_outcome_unknown");
    read.unwrap();
    client
        .call_tool(
            json!({"name":"t3_thread_read","arguments":{}})
                .as_object()
                .unwrap()
                .clone(),
            false,
            None,
        )
        .await
        .unwrap();
    let requests = server.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.body["method"] == "initialize")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.body["params"]["name"] == "t3_thread_send")
            .count(),
        1
    );
    drop(requests);
    client.close();
}
