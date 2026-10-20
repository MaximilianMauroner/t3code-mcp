#[path = "native_notifications_support/mod.rs"]
pub mod support;

use serde_json::{Value, json};
use support::*;
use tokio::time::timeout;

#[tokio::test]
async fn stdio_progress_precedes_result_and_keeps_native_params() {
    let mut fixture = Fixture::new().await;
    let mut process = Process::spawn(&fixture, "stdio");
    let request = call(json!("downstream"), json!(0.25), true);
    process.send(request.clone());
    let native = receive(&mut fixture.native.calls).await;
    assert_eq!(native.request.body["params"], request["params"]);
    let event = progress(json!(0.25), 1);
    native.events.send(event.clone()).unwrap();
    assert_eq!(receive(&mut process.lines).await, event);
    let final_result = result(&native);
    native.events.send(final_result.clone()).unwrap();
    let response = receive(&mut process.lines).await;
    assert_eq!(response["id"], request["id"]);
    assert_eq!(response["result"], final_result["result"]);
    process.eof();
    process.finish(true).await;
}

#[tokio::test]
async fn stdio_list_change_keeps_metadata_and_refreshes_all_pages() {
    let mut fixture = Fixture::new().await;
    let mut process = Process::spawn(&fixture, "stdio");
    process.send(json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}));
    let initial = receive(&mut process.lines).await;
    assert!(
        initial["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "t3_thread_send")
    );
    let session = receive(&mut fixture.native.opened).await;
    *fixture.native.revision.lock().unwrap() = 1;
    let params = json!({"_meta":{"opaque":{"owner":"stdio"}},"extension":[false,null]});
    fixture.native.notify(&session, params.clone());
    assert_eq!(
        receive(&mut process.lines).await,
        json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed","params":params})
    );
    process.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
    let refreshed = receive(&mut process.lines).await;
    for name in ["t3_thread_read", "t3_thread_send"] {
        assert_eq!(
            refreshed["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap(),
            &tool(name, 1)
        );
    }
    let mut newly_listed = call(json!(3), json!(0), false);
    newly_listed["params"]["name"] = json!("t3_project_read");
    process.send(newly_listed.clone());
    let native = receive(&mut fixture.native.calls).await;
    assert_eq!(native.request.body["params"], newly_listed["params"]);
    native.events.send(result(&native)).unwrap();
    assert_eq!(receive(&mut process.lines).await["id"], 3);
    process.eof();
    process.finish(true).await;
}

async fn post(
    client: &reqwest::Client,
    url: &str,
    session: Option<&str>,
    body: Value,
) -> reqwest::Response {
    let mut request = client
        .post(url)
        .bearer_auth("gateway-secret")
        .header("accept", "application/json, text/event-stream")
        .json(&body);
    if let Some(session) = session {
        request = request.header("mcp-session-id", session);
    }
    timeout(WAIT, request.send()).await.unwrap().unwrap()
}

async fn event(response: &mut reqwest::Response) -> Value {
    let mut data = Vec::new();
    timeout(WAIT, async {
        loop {
            let chunk = response
                .chunk()
                .await
                .unwrap()
                .expect("stream ended before its event");
            data.extend_from_slice(&chunk);
            if data.windows(2).any(|part| part == b"\n\n") {
                if let Some(line) = std::str::from_utf8(&data)
                    .unwrap()
                    .lines()
                    .find_map(|line| line.strip_prefix("data: "))
                {
                    return serde_json::from_str(line).unwrap();
                }
                data.clear();
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn http_sessionless_post_delivers_requested_progress_before_completion() {
    let mut fixture = Fixture::new().await;
    let mut process = Process::spawn(&fixture, "http");
    let url = process.url(&fixture).await;
    let request = call(json!(7), json!(0.5), true);
    let sent = request.clone();
    let sending =
        tokio::spawn(async move { post(&reqwest::Client::new(), &url, None, sent).await });
    let native = receive(&mut fixture.native.calls).await;
    assert_eq!(native.request.body["params"], request["params"]);
    let progress = progress(json!(0.5), 1);
    native.events.send(progress.clone()).unwrap();
    let mut response = timeout(WAIT, sending).await.unwrap().unwrap();
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(event(&mut response).await, progress);
    let final_result = result(&native);
    native.events.send(final_result.clone()).unwrap();
    let final_response = event(&mut response).await;
    assert_eq!(final_response["id"], request["id"]);
    assert_eq!(final_response["result"], final_result["result"]);
    #[cfg(unix)]
    {
        process.signal();
        process.finish(true).await;
    }
}

#[tokio::test]
async fn http_session_get_delivers_only_its_native_list_change() {
    let mut fixture = Fixture::new().await;
    let mut process = Process::spawn(&fixture, "http");
    let url = process.url(&fixture).await;
    let client = reqwest::Client::new();
    let initialized = post(&client, &url, None, initialize()).await;
    let session = initialized.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let _ = initialized.bytes().await.unwrap();
    let listed = post(
        &client,
        &url,
        Some(&session),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .await;
    assert!(listed.json::<Value>().await.unwrap()["result"]["tools"].is_array());
    let upstream = receive(&mut fixture.native.opened).await;
    let mut stream = client
        .get(&url)
        .bearer_auth("gateway-secret")
        .header("accept", "text/event-stream")
        .header("mcp-session-id", &session)
        .send()
        .await
        .unwrap();
    let params = json!({"_meta":{"opaque":{"session":"owned"}},"extension":false});
    fixture.native.notify(&upstream, params.clone());
    assert_eq!(
        event(&mut stream).await,
        json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed","params":params})
    );
    #[cfg(unix)]
    {
        process.signal();
        process.finish(true).await;
    }
}

#[tokio::test]
async fn buffered_raw_result_and_access_controls_remain_unchanged() {
    use bytes::Bytes;
    use t3_code_mcp::mcp::{HttpOptions, McpServer};
    let mut fixture = Fixture::new().await;
    let server = McpServer::gateway();
    let gateway = fixture.gateway(false);
    let request = call(json!("raw-control"), json!(0.5), true);
    let sent = request.clone();
    let sending = {
        let server = server.clone();
        tokio::spawn(async move {
            server
                .handle_http(
                    http::Request::builder()
                        .method("POST")
                        .uri("/mcp")
                        .header("authorization", "Bearer gateway-secret")
                        .header("accept", "application/json, text/event-stream")
                        .header("content-type", "application/json")
                        .body(Bytes::from(sent.to_string()))
                        .unwrap(),
                    &HttpOptions {
                        bearer_token: Some("gateway-secret".into()),
                        ..Default::default()
                    },
                    gateway.as_ref(),
                )
                .await
        })
    };
    let native = receive(&mut fixture.native.calls).await;
    assert_eq!(native.request.body["params"], request["params"]);
    let expected = result(&native);
    native.events.send(expected.clone()).unwrap();
    let response = timeout(WAIT, sending).await.unwrap().unwrap();
    let body: Value = serde_json::from_slice(response.body()).unwrap();
    assert_eq!(body["id"], request["id"]);
    assert_eq!(body["result"], expected["result"]);
    let read_only = fixture.gateway(true);
    let denied = server
        .handle_http(
            http::Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("authorization", "Bearer gateway-secret")
                .header("accept", "application/json, text/event-stream")
                .header("content-type", "application/json")
                .body(Bytes::from(request.to_string()))
                .unwrap(),
            &HttpOptions {
                bearer_token: Some("gateway-secret".into()),
                ..Default::default()
            },
            read_only.as_ref(),
        )
        .await;
    let denied: Value = serde_json::from_slice(denied.body()).unwrap();
    assert_eq!(denied["result"]["isError"], true);
    assert!(fixture.native.calls.try_recv().is_err());
    server.close();
}

#[cfg(unix)]
#[tokio::test]
async fn dropped_http_consumer_keeps_one_admitted_mutation_and_its_audit_result() {
    let mut fixture = Fixture::new().await;
    let mut process = Process::spawn(&fixture, "http");
    let url = process.url(&fixture).await;
    let sending = tokio::spawn(async move {
        post(
            &reqwest::Client::new(),
            &url,
            None,
            call(json!(1), json!("disconnect"), true),
        )
        .await
    });
    let native = receive(&mut fixture.native.calls).await;
    sending.abort();
    let _ = sending.await;
    native.events.send(result(&native)).unwrap();
    timeout(WAIT, async {
        loop {
            if fixture.audit().iter().any(|event| {
                event["event"] == "upstream.response"
                    && event["operation"] == "mcp:t3_thread_send"
                    && event["outcome"] == "completed"
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("admitted mutation lost its audit completion");
    process.signal();
    process.finish(true).await;
    assert!(
        fixture.native.calls.try_recv().is_err(),
        "downstream disconnect replayed a mutation"
    );
}
