use super::Fixture;
use crate::{
    gateway::T3Gateway,
    mcp::{IncomingMessage, McpServer, ToolHandler},
    t3::test_server::{Reply, Server},
};
use futures_util::FutureExt;
use serde_json::{Value, json};

async fn native_fixture() -> Fixture {
    let mut fixture = Fixture::new().await;
    fixture.server = Server::start(|request| {
        async move {
            match request.path.as_str() {
                "/mcp" if request.method == "GET" => Reply::status(405, Value::Null),
                "/mcp" if request.body["method"] == "initialize" => Reply::json(json!({
                    "jsonrpc":"2.0", "id":request.body["id"],
                    "result":{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"native-arguments-fixture","version":"1"}},
                })),
                "/mcp" if request.body.get("id").is_none() => Reply::status(202, Value::Null),
                "/mcp" if request.body["method"] == "tools/list" => Reply::json(json!({
                    "jsonrpc":"2.0", "id":request.body["id"],
                    "result":{"tools":[{
                        "name":"t3_thread_list",
                        "inputSchema":{
                            "type":"object",
                            "properties":{
                                "optionalDefault":{"type":"string","default":"native-only"},
                                "query":{"type":"string"},
                                "options":{
                                    "type":"object",
                                    "properties":{
                                        "known":{"type":"string"},
                                        "nestedDefault":{"type":"boolean","default":true},
                                    },
                                    "additionalProperties":true,
                                },
                                "largeInteger":{"type":"integer","minimum":0},
                                "character":{"type":"string","minLength":1,"maxLength":1},
                            },
                            "additionalProperties":true,
                        },
                    }]},
                })),
                "/mcp" if request.body["method"] == "tools/call" => Reply::json(json!({
                    "jsonrpc":"2.0", "id":request.body["id"],
                    "result":{"content":[],"structuredContent":{"arguments":request.body["params"]["arguments"]}},
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
    fixture.gateway = T3Gateway::new(config);
    fixture.authorize_native();
    fixture
}

async fn assert_forwarded(arguments: Value) {
    let fixture = native_fixture().await;
    let server = McpServer::new(fixture.gateway.tool_catalog().await.unwrap());
    let params = json!({"name":"t3_thread_list","arguments":arguments});
    let mut expected = arguments;
    expected["optionalDefault"] = json!("native-only");
    if expected.get("options").is_some() {
        expected["options"]["nestedDefault"] = json!(true);
    }
    let mut expected_params = params.clone();
    expected_params["arguments"] = expected.clone();
    let response = server
        .handle_message(
            IncomingMessage::parse(json!({
                "jsonrpc":"2.0", "id":1, "method":"tools/call", "params":params,
            }))
            .unwrap(),
            &fixture.gateway,
        )
        .await
        .unwrap();
    assert_eq!(
        response["result"],
        json!({"content":[],"structuredContent":{"arguments":expected}}),
        "{response}"
    );
    let requests = fixture.server.requests.lock().unwrap();
    let calls = requests
        .iter()
        .filter(|request| request.path == "/mcp" && request.body["method"] == "tools/call")
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].body["params"], expected_params);
    let upstream = &calls[0].body["params"]["arguments"];
    assert_eq!(upstream, &expected);
}

#[tokio::test]
async fn native_optional_defaults_are_inserted_before_sending() {
    assert_forwarded(json!({})).await;
}

#[tokio::test]
async fn native_nested_and_unknown_fields_are_preserved() {
    assert_forwarded(json!({
        "query":"  keep this spacing  ",
        "options":{"known":"  keep this too  ","unknown":{"items":[1,true,null]}},
        "unknown":{"nested":[{"value":"unchanged"}]},
    }))
    .await;
}

#[tokio::test]
async fn native_string_limits_count_code_points() {
    assert_forwarded(json!({"character":"\u{1f600}"})).await;
}

#[tokio::test]
async fn native_converted_schema_rejects_invalid_or_missing_arguments() {
    let fixture = native_fixture().await;
    let server = McpServer::new(fixture.gateway.tool_catalog().await.unwrap());
    for params in [
        json!({"name":"t3_thread_list","arguments":{"largeInteger":9_007_199_254_740_992_u64}}),
        json!({"name":"t3_thread_list","arguments":{"character":"ab"}}),
        json!({"name":"t3_thread_list"}),
    ] {
        let response = server
            .handle_message(
                IncomingMessage::parse(json!({
                    "jsonrpc":"2.0", "id":1, "method":"tools/call", "params":params,
                }))
                .unwrap(),
                &fixture.gateway,
            )
            .await
            .unwrap();
        assert_eq!(response["result"]["isError"], true, "{response}");
    }
    assert!(
        fixture
            .server
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.body["method"] != "tools/call")
    );
}

#[tokio::test]
async fn native_arguments_still_require_an_object_envelope() {
    let fixture = native_fixture().await;
    let server = McpServer::new(fixture.gateway.tool_catalog().await.unwrap());
    for arguments in [Value::Null, json!(true), json!(1), json!("text"), json!([])] {
        let response = server
            .handle_message(
                IncomingMessage::parse(json!({
                    "jsonrpc":"2.0", "id":1, "method":"tools/call",
                    "params":{"name":"t3_thread_list","arguments":arguments},
                }))
                .unwrap(),
                &fixture.gateway,
            )
            .await
            .unwrap();
        assert_eq!(response["error"]["code"], -32602, "{response}");
    }
    assert!(
        fixture
            .server
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.body["method"] != "tools/call")
    );
}
