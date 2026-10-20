use super::Fixture;
use crate::{
    gateway::T3Gateway,
    mcp::{IncomingMessage, McpServer, ToolHandler},
    t3::test_server::{Reply, Server},
};
use futures_util::FutureExt;
use serde_json::{Value, json};

async fn native_fixture() -> Fixture {
    native_fixture_with_schema(json!({
        "type":"object",
        "properties":{
            "optionalDefault":{"type":"string","default":"native-only"},
            "defaultedOptions":{
                "type":"object","default":{},
                "properties":{"mode":{"type":"string","default":"fast"}},
            },
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
    }))
    .await
}

async fn native_fixture_with_schema(input_schema: Value) -> Fixture {
    let mut fixture = Fixture::new().await;
    fixture.server = Server::start(move |request| {
        let input_schema = input_schema.clone();
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
                        "inputSchema":input_schema,
                        "outputSchema":{"type":"object","properties":{"retained":{"type":"boolean"}}},
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
    expected["defaultedOptions"] = if expected.get("defaultedOptions").is_some() {
        json!({"mode":"fast"})
    } else {
        json!({})
    };
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
    let response = serde_json::to_value(response).unwrap();
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
    assert_forwarded(json!({"defaultedOptions":{}})).await;
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
        let response = serde_json::to_value(response).unwrap();
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
        let response = serde_json::to_value(response).unwrap();
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

#[tokio::test]
async fn native_conversion_fallback_forwards_raw_arguments_and_keeps_advertisement() {
    let cases: Vec<Value> = serde_json::from_str(include_str!(
        "../../../tests/fixtures/native-input-conversion.json"
    ))
    .unwrap();
    for case in cases {
        let fixture = native_fixture_with_schema(case["inputSchema"].clone()).await;
        let server = McpServer::gateway();
        let listed = server
            .handle_message(
                IncomingMessage::parse(json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
                    .unwrap(),
                &fixture.gateway,
            )
            .await
            .unwrap();
        let listed = serde_json::to_value(listed).unwrap();
        let tool = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "t3_thread_list")
            .unwrap();
        assert_eq!(tool["inputSchema"], case["inputSchema"], "{}", case["name"]);
        assert_eq!(
            tool["outputSchema"],
            json!({"type":"object","properties":{"retained":{"type":"boolean"}}})
        );
        let arguments = json!({"n":"delegated-to-upstream","extra":true});
        let params = json!({"name":"t3_thread_list","arguments":arguments});
        let response = server
            .handle_message(
                IncomingMessage::parse(json!({
                    "jsonrpc":"2.0","id":2,"method":"tools/call","params":params,
                }))
                .unwrap(),
                &fixture.gateway,
            )
            .await
            .unwrap();
        let response = serde_json::to_value(response).unwrap();
        let requests = fixture.server.requests.lock().unwrap();
        let calls = requests
            .iter()
            .filter(|request| request.body["method"] == "tools/call")
            .collect::<Vec<_>>();
        if case["fallback"] == true {
            assert_eq!(
                response["result"]["structuredContent"]["arguments"], arguments,
                "{}: {response}",
                case["name"]
            );
            assert_eq!(calls.len(), 1, "{}", case["name"]);
            assert_eq!(calls[0].body["params"], params);
        } else {
            assert_eq!(
                response["result"]["isError"], true,
                "{}: {response}",
                case["name"]
            );
            assert!(calls.is_empty(), "{}", case["name"]);
        }
    }
}

#[tokio::test]
async fn split_native_argument_preview_keeps_forwarding_and_auditing() {
    use crate::operations::audit_log::AuditValue;
    let fixture = native_fixture().await;
    let server = McpServer::gateway();
    let long = format!("{}😀", "a".repeat(511));
    for id in [1, 2] {
        let reply = server
            .handle_message(
                IncomingMessage::parse(json!({
                    "jsonrpc":"2.0","id":id,"method":"tools/call","params":{
                        "name":"t3_thread_list","arguments":{"query":long}
                    }
                }))
                .unwrap(),
                &fixture.gateway,
            )
            .await
            .unwrap();
        let reply = serde_json::to_value(reply).unwrap();
        assert_ne!(reply["result"]["isError"], true, "{reply}");
    }
    let requests = fixture.server.requests.lock().unwrap();
    let calls = requests
        .iter()
        .filter(|request| request.body["method"] == "tools/call")
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2);
    assert!(
        calls
            .iter()
            .all(|request| request.body["params"]["arguments"]["query"] == long)
    );
    let rows: Vec<AuditValue> = std::fs::read_to_string(fixture.directory.join("audit.jsonl"))
        .unwrap()
        .lines()
        .map(|row| serde_json::from_str(row).unwrap())
        .collect();
    let preview = "a"
        .repeat(511)
        .encode_utf16()
        .chain([0xd83d])
        .collect::<Vec<_>>();
    for source in ["mcp", "t3"] {
        assert!(rows.iter().any(|row| row["source"] == source
            && row["details"]["arguments"]["query"]["preview"].as_utf16()
                == Some(preview.clone())));
    }
}
