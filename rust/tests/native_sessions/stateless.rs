use super::*;

fn ordinary_call(slot: usize) -> Value {
    let mut value = call(json!(7), slot, false);
    value["params"]["_meta"]
        .as_object_mut()
        .unwrap()
        .remove("progressToken");
    value
}

fn pending(fixture: &Fixture, slot: usize) -> tokio::task::JoinHandle<Response<Bytes>> {
    let server = fixture.server.clone();
    let gateway = fixture.gateway.clone();
    tokio::spawn(async move {
        tokio::time::timeout(
            Duration::from_secs(5),
            server.handle_http(
                request("POST", None, ordinary_call(slot)),
                &options(),
                gateway.as_ref(),
            ),
        )
        .await
        .unwrap()
    })
}

fn assert_response(response: &Response<Bytes>, slot: usize) {
    assert_eq!(response.status(), 200);
    assert!(response.headers().get("mcp-session-id").is_none());
    let response = body(response);
    assert_eq!(response["id"], 7);
    assert_eq!(
        response["result"]["structuredContent"]["params"],
        ordinary_call(slot)["params"]
    );
}

fn assert_discovery(fixture: &Fixture, count: usize) {
    let requests = fixture.native.requests.lock().unwrap();
    for method in ["initialize", "tools/list"] {
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.body["method"] == method)
                .count(),
            count,
            "{method} count"
        );
    }
}

#[tokio::test]
async fn sessionless_native_calls_reuse_discovery_without_a_downstream_handshake() {
    let fixture = Fixture::new().await;
    for slot in 0..3 {
        fixture.gates[slot].release.notify_one();
        assert_response(&fixture.send(None, ordinary_call(slot)).await, slot);
    }
    assert_eq!(fixture.calls(), 3);
    assert_discovery(&fixture, 1);
}

#[tokio::test]
async fn concurrent_sessionless_calls_keep_equal_downstream_ids_independent() {
    let fixture = Fixture::new().await;
    let first = pending(&fixture, 0);
    let held = pending(&fixture, 1);
    let first_wire = fixture.event().await;
    let held_wire = fixture.event().await;
    let (first_wire, held_wire) = if first_wire.body["params"]["arguments"]["slot"] == 0 {
        (first_wire, held_wire)
    } else {
        (held_wire, first_wire)
    };
    assert_ne!(first_wire.body["id"], held_wire.body["id"]);
    assert_eq!(
        first_wire.headers["mcp-session-id"],
        held_wire.headers["mcp-session-id"]
    );
    fixture.gates[0].release.notify_one();
    assert_response(&first.await.unwrap(), 0);
    assert!(!held.is_finished());

    fixture.send(None, cancel(json!(7))).await;
    let next = pending(&fixture, 2);
    let next_wire = fixture.event().await;
    assert_eq!(next_wire.body["method"], "tools/call");
    assert_eq!(
        next_wire.headers["mcp-session-id"],
        held_wire.headers["mcp-session-id"]
    );
    assert_ne!(next_wire.body["id"], held_wire.body["id"]);
    fixture.gates[2].release.notify_one();
    assert_response(&next.await.unwrap(), 2);
    fixture.gates[1].release.notify_one();
    assert_response(&held.await.unwrap(), 1);
    assert_eq!(fixture.calls(), 3);
    assert_discovery(&fixture, 1);
}

#[tokio::test]
async fn sessionless_credential_renewal_keeps_the_held_generation_alive() {
    let fixture = Fixture::new().await;
    let held = pending(&fixture, 0);
    let old_wire = fixture.event().await;
    fixture.credential("new-token");
    let renewed = pending(&fixture, 1);
    let new_wire = fixture.event().await;
    assert_eq!(old_wire.headers["authorization"], "Bearer old-token");
    assert_eq!(new_wire.headers["authorization"], "Bearer new-token");
    assert_ne!(
        old_wire.headers["mcp-session-id"],
        new_wire.headers["mcp-session-id"]
    );
    fixture.gates[1].release.notify_one();
    assert_response(&renewed.await.unwrap(), 1);
    let next = pending(&fixture, 2);
    let next_wire = fixture.event().await;
    assert_eq!(next_wire.headers["authorization"], "Bearer new-token");
    assert_eq!(
        next_wire.headers["mcp-session-id"],
        new_wire.headers["mcp-session-id"]
    );
    fixture.gates[2].release.notify_one();
    assert_response(&next.await.unwrap(), 2);
    fixture.gates[0].release.notify_one();
    assert_response(&held.await.unwrap(), 0);
    assert_eq!(fixture.calls(), 3);
    assert_discovery(&fixture, 2);
}
