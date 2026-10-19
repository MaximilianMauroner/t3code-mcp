#[path = "native_notifications_support/mod.rs"]
pub mod support;

use bytes::Bytes;
use http::Request;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use support::*;
use t3_code_mcp::{
    gateway::T3Gateway,
    mcp::{HttpBody, HttpOptions, McpServer},
};
use tokio::time::timeout;

fn options() -> HttpOptions {
    HttpOptions {
        bearer_token: Some("gateway-secret".into()),
        ..Default::default()
    }
}

fn request(method: &str, session: Option<&str>, body: Value) -> Request<Bytes> {
    let mut builder = Request::builder()
        .method(method)
        .uri("/mcp")
        .header("authorization", "Bearer gateway-secret")
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json");
    if let Some(session) = session {
        builder = builder.header("mcp-session-id", session);
    }
    builder.body(Bytes::from(body.to_string())).unwrap()
}

async fn frame(body: &mut HttpBody) -> Value {
    let frame = timeout(WAIT, body.frame())
        .await
        .unwrap()
        .expect("missing output")
        .unwrap()
        .into_data()
        .unwrap();
    serde_json::from_str(
        std::str::from_utf8(&frame)
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap(),
    )
    .unwrap()
}

async fn session(server: &McpServer, gateway: &T3Gateway) -> String {
    let response = server
        .handle_http(request("POST", None, initialize()), &options(), gateway)
        .await;
    response.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .into()
}

async fn list(server: &McpServer, gateway: &T3Gateway, session: &str, id: usize) -> Value {
    let response = timeout(
        WAIT,
        server.handle_http(
            request(
                "POST",
                Some(session),
                json!({"jsonrpc":"2.0","id":id,"method":"tools/list"}),
            ),
            &options(),
            gateway,
        ),
    )
    .await
    .unwrap();
    serde_json::from_slice(response.body()).unwrap()
}

fn native_tool(list: &Value, name: &str) -> Value {
    list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == name)
        .expect("native tool missing")
        .clone()
}

async fn complete(native: &Call, body: &mut HttpBody, id: usize) {
    let expected = result(native);
    native.events.send(expected.clone()).unwrap();
    let response = frame(body).await;
    assert_eq!(response["id"], id);
    assert_eq!(response["result"], expected["result"]);
}

async fn numeric_spellings(post: bool) {
    let mut fixture = Fixture::new().await;
    let gateway = fixture.gateway(false);
    let server = McpServer::gateway();
    let session = session(&server, &gateway).await;
    for (id, (requested, emitted)) in [
        ("1.0", "1"),
        ("0.2500", "0.25"),
        ("-0.0", "0"),
        ("1e0", "1.000"),
    ]
    .into_iter()
    .enumerate()
    {
        let sent = call(json!(id), serde_json::from_str(requested).unwrap(), false);
        let mut body = server
            .handle_http_stream(
                request("POST", Some(&session), sent.clone()),
                &options(),
                gateway.clone(),
            )
            .await
            .into_body();
        let native = receive(&mut fixture.native.calls).await;
        assert_eq!(native.request.body["params"], sent["params"]);
        assert_eq!(
            native.request.body["params"]["_meta"]["progressToken"].to_string(),
            sent["params"]["_meta"]["progressToken"].to_string()
        );
        let event = progress(serde_json::from_str(emitted).unwrap(), id + 1);
        if post {
            native.events.send(event.clone()).unwrap();
        } else {
            if id == 0 {
                let _ = receive(&mut fixture.native.opened).await;
            }
            fixture.native.listeners.lock().unwrap()[&native.request.session]
                .send(event.clone())
                .unwrap();
        }
        let received = frame(&mut body).await;
        assert_eq!(received, event);
        assert_eq!(
            received["params"]["progressToken"].to_string(),
            event["params"]["progressToken"].to_string()
        );
        complete(&native, &mut body, id).await;
    }
    server.close();
    timeout(WAIT, server.drain()).await.unwrap();
}

#[tokio::test]
async fn post_progress_matches_native_finite_numbers_without_changing_raw_tokens() {
    numeric_spellings(true).await;
}

#[tokio::test]
async fn get_progress_matches_native_finite_numbers_without_changing_raw_tokens() {
    numeric_spellings(false).await;
}

#[tokio::test]
async fn get_suppresses_numeric_alias_ambiguity_but_keeps_strings_and_distinct_numbers() {
    let mut fixture = Fixture::new().await;
    let gateway = fixture.gateway(false);
    let server = McpServer::gateway();
    let session = session(&server, &gateway).await;
    let mut active = Vec::new();
    for (id, token) in [
        "1.0",
        "1",
        "\"1\"",
        "0.25",
        "0.25000000000000006",
        "9007199254740992",
        "9007199254740993",
        "9007199254740994",
    ]
    .into_iter()
    .enumerate()
    {
        let sent = call(json!(id), serde_json::from_str(token).unwrap(), false);
        let body = server
            .handle_http_stream(
                request("POST", Some(&session), sent.clone()),
                &options(),
                gateway.clone(),
            )
            .await
            .into_body();
        let native = receive(&mut fixture.native.calls).await;
        assert_eq!(native.request.body["params"], sent["params"]);
        active.push((native, body));
    }
    let upstream = receive(&mut fixture.native.opened).await;
    let events = fixture.native.listeners.lock().unwrap()[&upstream].clone();
    events.send(progress(json!(1), 90)).unwrap();
    events
        .send(progress(json!(9007199254740992_u64), 95))
        .unwrap();
    events.send(progress(json!(0.5), 96)).unwrap();
    events.send(progress(json!("unknown"), 91)).unwrap();
    let string_marker = progress(json!("1"), 1);
    events.send(string_marker.clone()).unwrap();
    assert_eq!(frame(&mut active[2].1).await, string_marker);
    // The string marker follows the ambiguous GET event on the same native stream.
    for (id, (native, body)) in active
        .iter_mut()
        .enumerate()
        .filter(|(id, _)| [0, 1, 5, 6].contains(id))
    {
        native.events.send(progress(json!("1"), 93)).unwrap();
        native.events.send(progress(json!(0.25), 94)).unwrap();
        let marker = progress(
            native.request.body["params"]["_meta"]["progressToken"].clone(),
            id + 2,
        );
        native.events.send(marker.clone()).unwrap();
        assert_eq!(
            frame(body).await,
            marker,
            "ambiguous GET progress escaped before the POST marker"
        );
    }
    for (id, token) in [
        (3, "0.25"),
        (4, "0.25000000000000006"),
        (7, "9007199254740994"),
    ] {
        let marker = progress(serde_json::from_str(token).unwrap(), id);
        events.send(marker.clone()).unwrap();
        assert_eq!(frame(&mut active[id].1).await, marker);
    }
    let (native, body) = &mut active[3];
    complete(native, body, 3).await;
    events.send(progress(json!(0.25), 92)).unwrap();
    let marker = progress(json!("1"), 5);
    events.send(marker.clone()).unwrap();
    assert_eq!(frame(&mut active[2].1).await, marker);
    assert!(
        timeout(WAIT, active[3].1.frame()).await.unwrap().is_none(),
        "completed token received progress"
    );
    for (id, (native, body)) in active.iter_mut().enumerate() {
        if id != 3 {
            complete(native, body, id).await;
        }
    }
    server.close();
    timeout(WAIT, server.drain()).await.unwrap();
}

#[tokio::test]
async fn rotated_credential_lookup_rechecks_failed_shared_refresh_and_calls_new_tool() {
    let mut fixture = Fixture::new().await;
    let gateway = fixture.gateway(false);
    let server = McpServer::gateway();
    let session = session(&server, &gateway).await;
    let initial = list(&server, &gateway, &session, 1).await;
    assert!(
        !initial["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "t3_project_read")
    );
    let upstream = receive(&mut fixture.native.opened).await;
    let mut output = server
        .handle_http_stream(
            request("GET", Some(&session), Value::Null),
            &options(),
            gateway.clone(),
        )
        .await
        .into_body();
    fixture
        .native
        .catalog_by_token
        .lock()
        .unwrap()
        .insert("Bearer fixture-native".into(), (1, true));
    fixture
        .native
        .catalog_by_token
        .lock()
        .unwrap()
        .insert("Bearer fixture-native-rotated".into(), (2, false));
    let gate = Arc::new(PageGate::default());
    *fixture.native.page_gate.lock().unwrap() = Some(gate.clone());
    fixture.native.notify(&upstream, json!({"revision":1}));
    assert_eq!(frame(&mut output).await["params"]["revision"], 1);
    let old = {
        let (server, gateway, session) = (server.clone(), gateway.clone(), session.clone());
        tokio::spawn(async move { list(&server, &gateway, &session, 2).await })
    };
    timeout(WAIT, gate.entered.notified()).await.unwrap();
    fixture.write_credential("fixture-native-rotated");
    let mut current = Box::pin(list(&server, &gateway, &session, 3));
    assert!(
        futures_util::poll!(&mut current).is_pending(),
        "lookup must join the gated old refresh"
    );
    gate.release.notify_one();
    let refreshed = timeout(WAIT, current).await.unwrap();
    let _ = timeout(WAIT, old).await.unwrap().unwrap();
    assert_eq!(
        native_tool(&refreshed, "t3_project_read"),
        tool("t3_project_read", 2)
    );
    for name in ["t3_thread_read", "t3_thread_send"] {
        assert_eq!(native_tool(&refreshed, name), tool(name, 2));
    }
    let pages =
        std::iter::from_fn(|| fixture.native.catalog_requests.try_recv().ok()).collect::<Vec<_>>();
    let rotated = pages
        .iter()
        .filter(|page| page.authorization == "Bearer fixture-native-rotated")
        .collect::<Vec<_>>();
    assert_eq!(
        rotated.len(),
        2,
        "rotated credential must fetch both pages despite the old failure backoff"
    );
    assert!(rotated[0].body["params"].get("cursor").is_none());
    assert_eq!(rotated[1].body["params"]["cursor"], "page-2");
    assert_ne!(rotated[0].session, upstream);
    let mut sent = call(json!(4), json!("new-tool"), false);
    sent["params"]["name"] = json!("t3_project_read");
    let mut body = server
        .handle_http_stream(
            request("POST", Some(&session), sent.clone()),
            &options(),
            gateway.clone(),
        )
        .await
        .into_body();
    let native = receive(&mut fixture.native.calls).await;
    assert_eq!(
        native.request.authorization,
        "Bearer fixture-native-rotated"
    );
    assert_eq!(native.request.body["params"], sent["params"]);
    complete(&native, &mut body, 4).await;
    assert!(fixture.native.calls.try_recv().is_err());
    server.close();
    timeout(WAIT, server.drain()).await.unwrap();
}

#[tokio::test]
async fn newer_invalidation_rechecks_failed_refresh_and_current_failure_keeps_backoff() {
    let mut fixture = Fixture::new().await;
    let gateway = fixture.gateway(false);
    let server = McpServer::gateway();
    let session = session(&server, &gateway).await;
    let _ = list(&server, &gateway, &session, 1).await;
    let upstream = receive(&mut fixture.native.opened).await;
    let mut output = server
        .handle_http_stream(
            request("GET", Some(&session), Value::Null),
            &options(),
            gateway.clone(),
        )
        .await
        .into_body();
    *fixture.native.revision.lock().unwrap() = 1;
    *fixture.native.fail_page.lock().unwrap() = true;
    let gate = Arc::new(PageGate::default());
    *fixture.native.page_gate.lock().unwrap() = Some(gate.clone());
    fixture.native.notify(&upstream, json!({"revision":1}));
    assert_eq!(frame(&mut output).await["params"]["revision"], 1);
    let mut lookup = Box::pin(list(&server, &gateway, &session, 2));
    assert!(futures_util::poll!(&mut lookup).is_pending());
    timeout(WAIT, gate.entered.notified()).await.unwrap();
    *fixture.native.revision.lock().unwrap() = 2;
    *fixture.native.fail_page.lock().unwrap() = false;
    fixture.native.notify(&upstream, json!({"revision":2}));
    assert_eq!(frame(&mut output).await["params"]["revision"], 2);
    gate.release.notify_one();
    let refreshed = timeout(WAIT, lookup).await.unwrap();
    for name in ["t3_thread_read", "t3_thread_send", "t3_project_read"] {
        assert_eq!(native_tool(&refreshed, name), tool(name, 2));
    }
    let pages =
        std::iter::from_fn(|| fixture.native.catalog_requests.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(
        pages.len(),
        6,
        "initial, failed, and current catalogs must each fetch two pages"
    );
    *fixture.native.revision.lock().unwrap() = 3;
    *fixture.native.fail_page.lock().unwrap() = true;
    fixture.native.notify(&upstream, json!({"revision":3}));
    let _ = frame(&mut output).await;
    let failed = list(&server, &gateway, &session, 3).await;
    assert_eq!(
        failed["result"], refreshed["result"],
        "failed page must not publish a partial catalog"
    );
    for _ in 0..2 {
        let _ = receive(&mut fixture.native.catalog_requests).await;
    }
    assert_eq!(
        list(&server, &gateway, &session, 4).await["result"],
        refreshed["result"]
    );
    assert!(
        fixture.native.catalog_requests.try_recv().is_err(),
        "current credential must retain its failure backoff"
    );
    *fixture.native.revision.lock().unwrap() = 4;
    *fixture.native.fail_page.lock().unwrap() = false;
    fixture.native.notify(&upstream, json!({"revision":4}));
    let _ = frame(&mut output).await;
    let recovered = list(&server, &gateway, &session, 5).await;
    assert_eq!(
        native_tool(&recovered, "t3_project_read"),
        tool("t3_project_read", 4)
    );
    server.close();
    timeout(WAIT, server.drain()).await.unwrap();
}
