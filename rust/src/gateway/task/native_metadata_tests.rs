use super::Fixture;
use crate::{
    gateway::T3Gateway,
    mcp::{HttpOptions, McpServer},
    native_credential::{NativeAccessLevel, NativeCredential, NativeCredentialStore},
    t3::test_server::{Reply, Server},
};
use bytes::Bytes;
use futures_util::FutureExt;
use serde_json::{Value, json};

async fn native_fixture(read_only: bool, operate: bool) -> Fixture {
    let mut fixture = Fixture::new().await;
    fixture.server = Server::start(|request| {
        async move {
            match request.path.as_str() {
                "/mcp" if request.method == "GET" => Reply::status(405, Value::Null),
                "/mcp" if request.body["method"] == "initialize" => Reply::json(json!({
                    "jsonrpc":"2.0", "id":request.body["id"],
                    "result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"native-metadata-fixture","version":"1"}},
                })),
                "/mcp" if request.body.get("id").is_none() => Reply::status(202, Value::Null),
                "/mcp" if request.body["method"] == "tools/list" => {
                    let schema = json!({
                        "type":"object", "required":["threadId"], "additionalProperties":false,
                        "properties":{
                            "threadId":{"type":"string","minLength":1},
                            "query":{"type":"string","default":"native default"},
                            "count":{"type":"integer"},
                        },
                    });
                    Reply::json(json!({
                        "jsonrpc":"2.0", "id":request.body["id"],
                        "result":{"tools":[
                            {"name":"t3_thread_list","inputSchema":schema},
                            {"name":"t3_thread_send","inputSchema":schema},
                        ]},
                    }))
                }
                "/mcp" if request.body["method"] == "tools/call" => Reply::json(json!({
                    "jsonrpc":"2.0", "id":request.body["id"],
                    "result":{"content":[],"structuredContent":{"accepted":true}},
                })),
                _ => Reply::status(404, Value::Null),
            }
        }
        .boxed()
    })
    .await;
    let mut config = fixture.gateway.config().clone();
    config.endpoint.t3_http_base_url = fixture.server.url.clone();
    config.endpoint.t3_native_mcp_url = format!("{}/mcp", fixture.server.url);
    config.read_only = read_only;
    fixture.gateway = T3Gateway::new(config);
    let mut scopes = vec!["orchestration:read".into()];
    if operate {
        scopes.push("orchestration:operate".into());
    }
    NativeCredentialStore::in_data_dir(&fixture.directory)
        .write(&NativeCredential {
            resource: format!("{}/mcp", fixture.server.url),
            client_id: "fixture-client".into(),
            access_token: "fixture-access-token".into(),
            access: NativeAccessLevel::FullAccess,
            scopes,
            obtained_at: "2026-10-18T00:00:00.000Z".into(),
            expires_at: "2099-10-18T00:00:00.000Z".into(),
        })
        .unwrap();
    fixture
}

async fn http_call(fixture: &Fixture, params: Value) -> (u16, Value) {
    let request = http::Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("authorization", "Bearer fixture-downstream-token")
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body(Bytes::from(
            json!({"jsonrpc":"2.0","id":"metadata-call","method":"tools/call","params":params})
                .to_string(),
        ))
        .unwrap();
    let response = McpServer::gateway()
        .handle_http(
            request,
            &HttpOptions {
                bearer_token: Some("fixture-downstream-token".into()),
                ..HttpOptions::default()
            },
            &fixture.gateway,
        )
        .await;
    (
        response.status().as_u16(),
        serde_json::from_slice(response.body()).unwrap(),
    )
}

fn native_calls(fixture: &Fixture) -> Vec<Value> {
    fixture
        .server
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.path == "/mcp" && request.body["method"] == "tools/call")
        .map(|request| request.body["params"].clone())
        .collect()
}

async fn assert_forwarded(fixture: &Fixture, mut params: Value) {
    let before = native_calls(fixture).len();
    let (status, response) = http_call(fixture, params.clone()).await;
    assert_eq!(status, 200, "{response}");
    assert_eq!(
        response,
        json!({
            "jsonrpc":"2.0", "id":"metadata-call",
            "result":{"content":[],"structuredContent":{"accepted":true}},
        })
    );
    let calls = native_calls(fixture);
    assert_eq!(calls.len(), before + 1);
    if params["arguments"].get("query").is_none() {
        params["arguments"]["query"] = json!("native default");
    }
    assert_eq!(calls[before], params);
}

#[tokio::test]
async fn native_http_rejects_invalid_arguments_before_sending() {
    let fixture = native_fixture(false, true).await;
    for arguments in [
        json!({"threadId":false}),
        json!({"threadId":"thread-1","count":0.125}),
        json!({"threadId":"thread-1","unknown":true}),
        json!({"query":"missing thread"}),
        json!({}),
    ] {
        let (status, response) = http_call(
            &fixture,
            json!({"name":"t3_thread_list","arguments":arguments}),
        )
        .await;
        assert_eq!(status, 200, "{response}");
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(
            response["result"]["isError"], true,
            "{arguments}: {response}"
        );
        assert!(native_calls(&fixture).is_empty());
    }
}

#[tokio::test]
async fn authorized_native_mutation_preserves_the_same_parameter_object() {
    let fixture = native_fixture(false, true).await;
    assert_forwarded(
        &fixture,
        json!({
            "name":"t3_thread_send", "arguments":{"threadId":"thread-1","query":"  keep  "},
            "extension":[null,{"preserve":true}],
            "_meta":{
                "progressToken":0,
                "io.modelcontextprotocol/related-task":{"taskId":"task-1","extension":true},
            },
        }),
    )
    .await;
}

#[tokio::test]
async fn native_http_rejects_missing_required_arguments() {
    let fixture = native_fixture(false, true).await;
    for params in [
        json!({"name":"t3_thread_list"}),
        json!({"name":"t3_thread_list","extension":{"keep":null}}),
        json!({"name":"t3_thread_list","arguments":{}}),
        json!({"name":"t3_thread_list","_meta":{}}),
    ] {
        let (_, response) = http_call(&fixture, params).await;
        assert_eq!(response["result"]["isError"], true, "{response}");
        assert!(native_calls(&fixture).is_empty());
    }
}

#[tokio::test]
async fn native_http_preserves_fractional_zero_exponent_large_and_string_tokens() {
    let fixture = native_fixture(false, true).await;
    for literal in [
        "0",
        "0.0",
        "-0.0",
        "0.125",
        "-2.5",
        "1e-3",
        "1.25e2",
        "9007199254740992",
        "1e100",
        "\"  token  \"",
        "\"\"",
    ] {
        let token: Value = serde_json::from_str(literal).unwrap();
        assert_forwarded(
            &fixture,
            json!({
                "name":"t3_thread_list","arguments":{"threadId":"thread-1"},
                "_meta":{
                    "progressToken":token,
                    "io.modelcontextprotocol/related-task":{
                        "taskId":"task-1", "extension":{"keep":[1,true,null]},
                    },
                },
            }),
        )
        .await;
    }
}

#[tokio::test]
async fn malformed_known_metadata_is_rejected_before_native_send() {
    let fixture = native_fixture(false, true).await;
    let mut metadata = vec![
        Value::Null,
        json!(false),
        json!(0),
        json!("text"),
        json!([]),
    ];
    for token in [
        Value::Null,
        json!(true),
        json!({}),
        json!([]),
        serde_json::from_str("1e400").unwrap(),
    ] {
        metadata.push(json!({"progressToken":token}));
    }
    for task in [
        Value::Null,
        json!(true),
        json!([]),
        json!("task"),
        json!({}),
        json!({"taskId":null}),
        json!({"taskId":1}),
        json!({"taskId":[]}),
    ] {
        metadata.push(json!({"io.modelcontextprotocol/related-task":task}));
    }
    for meta in metadata {
        let (status, response) = http_call(
            &fixture,
            json!({
                "name":"t3_thread_list","arguments":{},"_meta":meta,
            }),
        )
        .await;
        assert_eq!(status, 400, "{meta}: {response}");
        assert_eq!(response["error"]["code"], -32600, "{meta}: {response}");
    }
    assert!(native_calls(&fixture).is_empty());
}

#[tokio::test]
async fn opaque_metadata_does_not_authorize_native_mutations() {
    for (read_only, operate, code) in [
        (true, true, "gateway_read_only"),
        (false, false, "t3_scope_required"),
    ] {
        let fixture = native_fixture(read_only, operate).await;
        let (status, response) = http_call(
            &fixture,
            json!({
                "name":"t3_thread_send", "arguments":{"threadId":"thread-1"},
                "_meta":{
                    "progressToken":0.25,
                    "scopes":["orchestration:operate"], "readOnly":false,
                    "io.modelcontextprotocol/related-task":{
                        "taskId":"task-1", "authorized":true,
                        "scopes":["orchestration:operate"], "access":"full-access",
                    },
                },
                "authorization":{"scopes":["orchestration:operate"]},
            }),
        )
        .await;
        assert_eq!(status, 200, "{response}");
        assert_eq!(response["result"]["isError"], true, "{response}");
        assert_eq!(
            response["result"]["structuredContent"]["error"]["code"],
            code
        );
        assert!(native_calls(&fixture).is_empty());
    }
}

#[tokio::test]
async fn related_task_extensions_do_not_enable_unsupported_task_execution() {
    let fixture = native_fixture(false, true).await;
    let (status, response) = http_call(
        &fixture,
        json!({
            "name":"t3_thread_send", "arguments":{}, "task":{},
            "_meta":{"io.modelcontextprotocol/related-task":{"taskId":"task-1","authorized":true}},
        }),
    )
    .await;
    assert_eq!(status, 200, "{response}");
    assert_eq!(response["error"]["code"], -32602);
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Server does not support task creation")
    );
    assert!(native_calls(&fixture).is_empty());
}
