use super::tests::{Credentials, initialize, response};
use super::*;
use crate::t3::test_server::{Reply, Server};
use std::sync::atomic::{AtomicUsize, Ordering};

fn sse(data: String) -> Reply {
    Reply::sse(
        data.as_bytes()
            .chunks(7)
            .map(|chunk| (chunk.to_vec(), Duration::ZERO))
            .collect(),
    )
}

#[tokio::test]
async fn sse_batches_notifications_and_server_ping_preserve_raw_results() {
    let server = Server::start(|r| async move {
        if let Some(reply) = initialize(&r) { return reply; }
        if r.body.get("result").is_some() { return Reply::status(202, Value::Null); }
        let batch = json!([
            {"jsonrpc":"2.0","method":"notifications/tools/list_changed"},
            {"jsonrpc":"2.0","id":"unrelated","result":{"content":[]}},
            {"jsonrpc":"2.0","id":r.body["id"],"result":{"content":[{"type":"text","text":"café","unknown":"strip"}],"nativeExtension":{"trace":"keep"}}}
        ]);
        sse(format!(": keep-alive\r\nunknown-field: ignored\r\nevent: old\r\nevent: ignored\r\ndata: not-json\r\n\r\nevent: message\r\ndata: {{\"jsonrpc\":\"2.0\",\"id\":\"server-ping\",\"method\":\"ping\"}}\r\n\r\ndata: {batch}\r\n\r\n"))
    }.boxed()).await;
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
    assert_eq!(result.extra["nativeExtension"]["trace"], "keep");
    assert_eq!(
        result.content,
        vec![json!({"type":"text","text":"café","unknown":"strip"})]
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if server
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.body["id"] == "server-ping" && r.body["result"] == json!({}))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let requests = server.requests.lock().unwrap();
    let call = requests
        .iter()
        .find(|r| r.body["method"] == "tools/call")
        .unwrap();
    assert_eq!(call.headers["mcp-protocol-version"], "2025-11-25");
    assert_eq!(call.headers["mcp-session-id"], "session-Bearer-one");
    assert_eq!(
        call.body["params"],
        json!({"name":"t3_thread_send","arguments":{}})
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.body["method"] == "tools/call")
            .count(),
        1
    );
    drop(requests);
    client.close();
}

#[tokio::test]
async fn accepted_request_receives_its_response_from_the_get_stream() {
    let admitted = Arc::new(tokio::sync::Notify::new());
    let gate = admitted.clone();
    let server = Server::start(move |r| { let gate = gate.clone(); async move {
        if r.method == "GET" {
            gate.notified().await;
            return sse(format!("data: {}\n\n", json!({"jsonrpc":"2.0","id":1,"result":{"content":[],"nativeExtension":"from-get"}})));
        }
        if let Some(reply) = initialize(&r) { return reply; }
        gate.notify_one(); Reply::status(202, Value::Null)
    }.boxed() }).await;
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
    assert_eq!(result.extra["nativeExtension"], "from-get");
    assert_eq!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.body["method"] == "tools/call")
            .count(),
        1
    );
    client.close();
}

#[tokio::test]
async fn primed_post_stream_resumes_with_get_without_replaying_the_mutation() {
    let server = Server::start(|r| async move {
        if r.method == "GET" && r.headers.get("last-event-id").map(String::as_str) == Some("event-1") {
            return sse(format!("data: {}\n\n", json!({"jsonrpc":"2.0","id":1,"result":{"content":[],"nativeExtension":"resumed"}})));
        }
        if let Some(reply) = initialize(&r) { return reply; }
        sse("id: event-1\nretry: 0\n\n".into())
    }.boxed()).await;
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
    assert_eq!(result.extra["nativeExtension"], "resumed");
    let requests = server.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.body["method"] == "tools/call")
            .count(),
        1
    );
    assert!(requests.iter().any(|r| r.method == "GET"
        && r.headers.get("last-event-id").map(String::as_str) == Some("event-1")));
    drop(requests);
    client.close();
}

#[tokio::test]
async fn unrelated_post_response_does_not_hide_the_pending_get_response() {
    let server = Server::start(|r| {
        async move {
            if r.method == "GET" {
                assert_eq!(
                    r.headers.get("last-event-id").map(String::as_str),
                    Some("event-1")
                );
                return sse(format!(
                    "data: {}\n\n",
                    json!({"jsonrpc":"2.0","id":1,"result":{"content":[],"responsePath":"get"}})
                ));
            }
            if r.body["method"] == "initialize" {
                return initialize(&r).unwrap();
            }
            if r.body["method"] == "notifications/initialized" {
                return Reply::status(200, json!({}));
            }
            sse(format!(
                "id: event-1\nretry: 0\n\ndata: {}\n\n",
                json!({"jsonrpc":"2.0","id":99,"result":{"content":[]}})
            ))
        }
        .boxed()
    })
    .await;
    let credentials = Credentials::new();
    credentials.write("one");
    let client = credentials.client(&server);
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        client.call_tool(
            json!({"name":"t3_thread_send","arguments":{}})
                .as_object()
                .unwrap()
                .clone(),
            true,
            None,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.extra["responsePath"], "get");
    assert_eq!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.body["method"] == "tools/call")
            .count(),
        1
    );
    client.close();
}

#[tokio::test]
async fn sse_catalog_uses_every_page_and_rejects_an_explicit_null_cursor() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let server = Server::start(move |r| { let count = count.clone(); async move {
        if let Some(reply) = initialize(&r) { return reply; }
        let result = if count.fetch_add(1, Ordering::SeqCst) == 0 {
            json!({"tools":[{"name":"t3_thread_list","inputSchema":{"type":"object"},"outputSchema":{"type":"object"}}],"nextCursor":""})
        } else { assert_eq!(r.body["params"], json!({"cursor":""})); json!({"tools":[],"nextCursor":null}) };
        sse(format!("data: {}\n\n", json!({"jsonrpc":"2.0","id":r.body["id"],"result":result})))
    }.boxed() }).await;
    let credentials = Credentials::new();
    credentials.write("one");
    let client = credentials.client(&server);
    let result = client.snapshot().await;
    assert_eq!(result.error.unwrap().code, "native_unavailable");
    assert!(result.tools.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    client.close();
}

#[tokio::test]
async fn malformed_tool_result_is_uncertain_after_one_mutation_send() {
    for invalid in [
        json!({"content":null}),
        json!({"content":[],"structuredContent":null}),
        json!({"content":[{"type":"text","text":17}]}),
        json!({"content":[],"isError":null}),
    ] {
        let server = Server::start(move |r| {
            let invalid = invalid.clone();
            async move {
                if let Some(reply) = initialize(&r) {
                    return reply;
                }
                response(&r, invalid)
            }
            .boxed()
        })
        .await;
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
            "native_outcome_unknown"
        );
        assert_eq!(
            server
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.body["method"] == "tools/call")
                .count(),
            1
        );
        client.close();
    }
}
