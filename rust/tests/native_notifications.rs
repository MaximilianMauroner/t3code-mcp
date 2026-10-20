#[path = "native_notifications_support/mod.rs"]
pub mod support;

use bytes::Bytes;
use http::Request;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use support::*;
use t3_code_mcp::mcp::{HttpBody, HttpOptions, McpServer};
use tokio::time::timeout;

fn options() -> HttpOptions {
    HttpOptions {
        bearer_token: Some("gateway-secret".into()),
        ..Default::default()
    }
}
fn request(method: &str, session: Option<&str>, body: Value) -> Request<Bytes> {
    let mut request = Request::builder()
        .method(method)
        .uri("/mcp")
        .header("authorization", "Bearer gateway-secret")
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json");
    if let Some(session) = session {
        request = request.header("mcp-session-id", session);
    }
    request.body(Bytes::from(body.to_string())).unwrap()
}
async fn frame(body: &mut HttpBody) -> Value {
    let frame = timeout(WAIT, body.frame())
        .await
        .unwrap()
        .expect("missing output")
        .unwrap()
        .into_data()
        .unwrap();
    let text = std::str::from_utf8(&frame).unwrap();
    serde_json::from_str(
        text.lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap(),
    )
    .unwrap()
}
async fn session(server: &McpServer, gateway: &t3_code_mcp::gateway::T3Gateway) -> String {
    let response = server
        .handle_http(request("POST", None, initialize()), &options(), gateway)
        .await;
    response.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .into()
}
async fn list(
    server: &McpServer,
    gateway: &t3_code_mcp::gateway::T3Gateway,
    session: &str,
    id: usize,
) -> Value {
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
        .unwrap()
        .clone()
}
fn completed(fixture: &Fixture) -> usize {
    fixture
        .audit()
        .iter()
        .filter(|event| {
            event["event"] == "upstream.response"
                && event["operation"] == "mcp:t3_thread_send"
                && event["outcome"] == "completed"
        })
        .count()
}

#[tokio::test]
async fn colliding_peer_tokens_route_post_progress_and_get_list_changes_separately() {
    let mut fixture = Fixture::new().await;
    let gateway = fixture.gateway(false);
    let server = McpServer::gateway();
    let a = session(&server, &gateway).await;
    let b = session(&server, &gateway).await;
    let mut a_call = server
        .handle_http_stream(
            request("POST", Some(&a), call(json!(7), json!(0.25), true)),
            &options(),
            gateway.clone(),
        )
        .await
        .into_body();
    let first = receive(&mut fixture.native.calls).await;
    let mut b_call = server
        .handle_http_stream(
            request("POST", Some(&b), call(json!(7), json!(0.25), true)),
            &options(),
            gateway.clone(),
        )
        .await
        .into_body();
    let second = receive(&mut fixture.native.calls).await;
    assert_ne!(first.request.session, second.request.session);
    let mut a_get = server
        .handle_http_stream(
            request("GET", Some(&a), Value::Null),
            &options(),
            gateway.clone(),
        )
        .await
        .into_body();
    let mut b_get = server
        .handle_http_stream(
            request("GET", Some(&b), Value::Null),
            &options(),
            gateway.clone(),
        )
        .await
        .into_body();
    let a_open = receive(&mut fixture.native.opened).await;
    let b_open = receive(&mut fixture.native.opened).await;
    assert_ne!(a_open, b_open);
    let params = json!({"_meta":{"owner":"a"}});
    fixture
        .native
        .notify(&first.request.session, params.clone());
    assert_eq!(frame(&mut a_get).await["params"], params);
    let mut pa = progress(json!(0.25), 1);
    pa["params"]["owner"] = json!("a");
    let mut pb = progress(json!(0.25), 2);
    pb["params"]["owner"] = json!("b");
    fixture.native.listeners.lock().unwrap()[&first.request.session]
        .send(progress(json!("unknown"), 1))
        .unwrap();
    fixture.native.listeners.lock().unwrap()[&first.request.session]
        .send(pa.clone())
        .unwrap();
    second.events.send(pb.clone()).unwrap();
    assert_eq!(frame(&mut a_call).await, pa);
    assert_eq!(frame(&mut b_call).await, pb);
    // A marker from B makes any accidental earlier A broadcast observable without a sleep.
    let b_params = json!({"_meta":{"owner":"b"}});
    fixture
        .native
        .notify(&second.request.session, b_params.clone());
    assert_eq!(frame(&mut b_get).await["params"], b_params);
    for (native, body) in [(&first, &mut a_call), (&second, &mut b_call)] {
        let expected = result(native);
        native.events.send(expected.clone()).unwrap();
        let response = frame(body).await;
        assert_eq!(response["id"], 7);
        assert_eq!(response["result"], expected["result"]);
    }
    server.close();
    timeout(WAIT, server.drain()).await.unwrap();
}

#[tokio::test]
async fn stateless_progress_tokens_and_batches_keep_distinct_native_sessions() {
    let mut fixture = Fixture::new().await;
    let gateway = fixture.gateway(false);
    let server = McpServer::gateway();
    let progress_call = call(json!(7), json!(0.25), true);
    let mut ordinary_call = call(json!(8), json!(0.25), true);
    ordinary_call["params"]["_meta"]
        .as_object_mut()
        .unwrap()
        .remove("progressToken");
    let mut first_body = server
        .handle_http_stream(
            request("POST", None, progress_call.clone()),
            &options(),
            gateway.clone(),
        )
        .await
        .into_body();
    let first = receive(&mut fixture.native.calls).await;
    let mut batch_body = server
        .handle_http_stream(
            request("POST", None, json!([progress_call, ordinary_call])),
            &options(),
            gateway.clone(),
        )
        .await
        .into_body();
    let second = receive(&mut fixture.native.calls).await;
    let third = receive(&mut fixture.native.calls).await;
    let (progress_call_wire, ordinary_call_wire) = if second.request.body["params"]["_meta"]
        .get("progressToken")
        .is_some()
    {
        (second, third)
    } else {
        (third, second)
    };
    assert_ne!(first.request.session, progress_call_wire.request.session);
    assert_eq!(
        progress_call_wire.request.session,
        ordinary_call_wire.request.session
    );
    assert_eq!(first.request.body["params"], progress_call["params"]);
    assert_eq!(
        progress_call_wire.request.body["params"],
        progress_call["params"]
    );
    assert_eq!(
        ordinary_call_wire.request.body["params"],
        ordinary_call["params"]
    );
    for _ in 0..2 {
        receive(&mut fixture.native.opened).await;
    }
    let first_event = progress(json!(0.25), 1);
    let batch_event = progress(json!(0.25), 2);
    {
        let listeners = fixture.native.listeners.lock().unwrap();
        listeners[&first.request.session]
            .send(first_event.clone())
            .unwrap();
        listeners[&progress_call_wire.request.session]
            .send(batch_event.clone())
            .unwrap();
    }
    assert_eq!(frame(&mut first_body).await, first_event);
    assert_eq!(frame(&mut batch_body).await, batch_event);
    let first_result = result(&first);
    first.events.send(first_result.clone()).unwrap();
    assert_eq!(
        frame(&mut first_body).await,
        json!({"jsonrpc":"2.0","id":7,"result":first_result["result"]})
    );
    for native in [&progress_call_wire, &ordinary_call_wire] {
        native.events.send(result(native)).unwrap();
    }
    assert_eq!(
        frame(&mut batch_body).await,
        json!([
            {"jsonrpc":"2.0","id":7,"result":result(&progress_call_wire)["result"]},
            {"jsonrpc":"2.0","id":8,"result":result(&ordinary_call_wire)["result"]}
        ])
    );
    server.close();
    timeout(WAIT, server.drain()).await.unwrap();
}

#[tokio::test]
async fn paginated_invalidation_during_refresh_repeats_and_failed_page_keeps_complete_snapshot() {
    let mut fixture = Fixture::new().await;
    let gateway = fixture.gateway(false);
    let server = McpServer::gateway();
    let a = session(&server, &gateway).await;
    let b = session(&server, &gateway).await;
    let initial = list(&server, &gateway, &a, 1).await;
    let a_native = receive(&mut fixture.native.opened).await;
    let b_initial = list(&server, &gateway, &b, 1).await;
    let _ = receive(&mut fixture.native.opened).await;
    for id in 2..12 {
        assert_eq!(
            list(&server, &gateway, &a, id).await["result"],
            initial["result"]
        );
    }
    let mut output = server
        .handle_http_stream(
            request("GET", Some(&a), Value::Null),
            &options(),
            gateway.clone(),
        )
        .await
        .into_body();
    *fixture.native.revision.lock().unwrap() = 1;
    fixture.native.notify(&a_native, json!({"revision":1}));
    assert_eq!(frame(&mut output).await["params"]["revision"], 1);
    let gate = Arc::new(PageGate::default());
    *fixture.native.page_gate.lock().unwrap() = Some(gate.clone());
    let refresh = {
        let server = server.clone();
        let gateway = gateway.clone();
        let a = a.clone();
        tokio::spawn(async move { list(&server, &gateway, &a, 20).await })
    };
    timeout(WAIT, gate.entered.notified()).await.unwrap();
    *fixture.native.revision.lock().unwrap() = 2;
    fixture.native.notify(&a_native, json!({"revision":2}));
    assert_eq!(frame(&mut output).await["params"]["revision"], 2);
    gate.release.notify_one();
    let refreshed = timeout(WAIT, refresh).await.unwrap().unwrap();
    for name in ["t3_thread_read", "t3_thread_send"] {
        assert_eq!(native_tool(&refreshed, name), tool(name, 2));
    }
    assert_eq!(
        list(&server, &gateway, &b, 2).await["result"],
        b_initial["result"]
    );
    let mut newly_listed = call(json!(30), json!(0), false);
    newly_listed["params"]["name"] = json!("t3_project_read");
    let mut invocation = server
        .handle_http_stream(
            request("POST", Some(&a), newly_listed.clone()),
            &options(),
            gateway.clone(),
        )
        .await
        .into_body();
    let native = receive(&mut fixture.native.calls).await;
    assert_eq!(native.request.body["params"], newly_listed["params"]);
    native.events.send(result(&native)).unwrap();
    assert_eq!(frame(&mut invocation).await["id"], 30);
    *fixture.native.fail_page.lock().unwrap() = true;
    *fixture.native.revision.lock().unwrap() = 3;
    fixture.native.notify(&a_native, json!({"revision":3}));
    let _ = frame(&mut output).await;
    let failed = list(&server, &gateway, &a, 21).await;
    for name in ["t3_thread_read", "t3_thread_send"] {
        assert_eq!(native_tool(&failed, name), tool(name, 2));
    }
    *fixture.native.fail_page.lock().unwrap() = false;
    *fixture.native.revision.lock().unwrap() = 4;
    fixture.native.notify(&a_native, json!({"revision":4}));
    let _ = frame(&mut output).await;
    let recovered = list(&server, &gateway, &a, 22).await;
    for name in ["t3_thread_read", "t3_thread_send"] {
        assert_eq!(native_tool(&recovered, name), tool(name, 4));
    }
    let pages = std::iter::from_fn(|| fixture.native.catalog_requests.try_recv().ok())
        .filter(|r| r.session == a_native)
        .collect::<Vec<_>>();
    assert_eq!(
        pages.len(),
        10,
        "two initial pages, four racing pages, two failed pages, two recovered pages"
    );
    server.close();
    server.drain().await;
}

#[tokio::test]
async fn pressure_and_body_disconnect_keep_mutations_and_audit_completion() {
    let mut fixture = Fixture::new().await;
    let gateway = fixture.gateway(false);
    let server = McpServer::gateway();
    let mut pressured = server
        .handle_http_stream(
            request("POST", None, call(json!(1), json!("same"), true)),
            &options(),
            gateway.clone(),
        )
        .await
        .into_body();
    let native = receive(&mut fixture.native.calls).await;
    for count in 0..256 {
        native.events.send(progress(json!("same"), count)).unwrap();
    }
    native.events.send(result(&native)).unwrap();
    timeout(WAIT, server.drain()).await.unwrap();
    assert!(
        timeout(WAIT, pressured.frame())
            .await
            .unwrap()
            .unwrap()
            .is_err(),
        "overflow must close the output as failed"
    );
    assert_eq!(completed(&fixture), 1);
    let disconnected = server
        .handle_http_stream(
            request("POST", None, call(json!(2), json!("same"), true)),
            &options(),
            gateway.clone(),
        )
        .await;
    let next = receive(&mut fixture.native.calls).await;
    drop(disconnected);
    let other = server
        .handle_http_stream(
            request("POST", None, call(json!(3), json!("same"), true)),
            &options(),
            gateway.clone(),
        )
        .await;
    let third = receive(&mut fixture.native.calls).await;
    let mut other = other.into_body();
    let event = progress(json!("same"), 3);
    third.events.send(event.clone()).unwrap();
    assert_eq!(frame(&mut other).await, event);
    next.events.send(result(&next)).unwrap();
    third.events.send(result(&third)).unwrap();
    assert_eq!(frame(&mut other).await["id"], 3);
    timeout(WAIT, server.drain()).await.unwrap();
    assert_eq!(completed(&fixture), 3);
    assert!(
        fixture.native.calls.try_recv().is_err(),
        "output loss must not replay admitted mutations"
    );
    server.close();
}

#[tokio::test]
async fn delete_stops_delivery_then_releases_native_get_after_admitted_work() {
    let mut fixture = Fixture::new().await;
    let gateway = fixture.gateway(false);
    let server = McpServer::gateway();
    let session = session(&server, &gateway).await;
    let mut output = server
        .handle_http_stream(
            request("GET", Some(&session), Value::Null),
            &options(),
            gateway.clone(),
        )
        .await
        .into_body();
    let mut post = server
        .handle_http_stream(
            request("POST", Some(&session), call(json!(1), json!(1), true)),
            &options(),
            gateway.clone(),
        )
        .await
        .into_body();
    let native = receive(&mut fixture.native.calls).await;
    let upstream = receive(&mut fixture.native.opened).await;
    let deleted = server
        .handle_http(
            request("DELETE", Some(&session), Value::Null),
            &options(),
            gateway.as_ref(),
        )
        .await;
    assert_eq!(deleted.status(), 200);
    assert!(timeout(WAIT, output.frame()).await.unwrap().is_none());
    assert!(timeout(WAIT, post.frame()).await.unwrap().is_none());
    assert!(
        fixture.native.closed.try_recv().is_err(),
        "native listener must survive while admitted work runs"
    );
    let rejected = server
        .handle_http(
            request("POST", Some(&session), call(json!(2), json!(1), true)),
            &options(),
            gateway.as_ref(),
        )
        .await;
    assert_eq!(rejected.status(), 404);
    native.events.send(result(&native)).unwrap();
    timeout(WAIT, server.drain()).await.unwrap();
    assert_eq!(receive(&mut fixture.native.closed).await, upstream);
    assert_eq!(completed(&fixture), 1);
    assert!(fixture.native.calls.try_recv().is_err());
}

#[tokio::test]
async fn access_and_local_validation_never_send_a_native_mutation() {
    let mut fixture = Fixture::new().await;
    let gateway = fixture.gateway(true);
    let server = McpServer::gateway();
    let response = server
        .handle_http(
            request("POST", None, call(json!(1), json!(0.5), true)),
            &options(),
            gateway.as_ref(),
        )
        .await;
    let response: Value = serde_json::from_slice(response.body()).unwrap();
    assert_eq!(response["result"]["isError"], true);
    assert!(fixture.native.calls.try_recv().is_err());
    while fixture.native.catalog_requests.try_recv().is_ok() {}
    for body in [
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":false}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tasks/get","params":{"taskId":"not-supported"}}),
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"delegate_task","arguments":{}}}),
    ] {
        let _ = server
            .handle_http(request("POST", None, body), &options(), gateway.as_ref())
            .await;
    }
    assert!(fixture.native.catalog_requests.try_recv().is_err());
    assert!(fixture.native.calls.try_recv().is_err());
    server.close();
}
