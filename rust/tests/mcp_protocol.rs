use std::sync::Mutex;

use bytes::Bytes;
use http::{Request, Response};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use t3_code_mcp::mcp::{
    HttpOptions, IncomingMessage, McpServer, RequestId, SUPPORTED_PROTOCOL_VERSIONS, StdioCodec,
    StdioError, ToolCall, ToolFailure, ToolHandler, ToolsListPage,
};

struct Handler {
    calls: Mutex<Vec<ToolCall>>,
    result: Result<t3_code_mcp::mcp::ToolResult, ToolFailure>,
}

impl Handler {
    fn returning(value: Value) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            result: Ok(t3_code_mcp::mcp::ToolResult::new(&value).unwrap()),
        }
    }
}

impl ToolHandler for Handler {
    async fn call_tool(
        &self,
        call: ToolCall,
        _invocation: Option<t3_code_mcp::mcp::Invocation>,
    ) -> Result<t3_code_mcp::mcp::ToolResult, ToolFailure> {
        self.calls.lock().unwrap().push(call);
        self.result.clone()
    }
}

fn baseline() -> Value {
    serde_json::from_str(include_str!(
        "../../tests/fixtures/node-compat-baseline.json"
    ))
    .unwrap()
}

fn options() -> HttpOptions {
    HttpOptions {
        bearer_token: Some("gateway-test-token".to_owned()),
        ..Default::default()
    }
}

fn request(method: &str, path: &str, body: impl Into<Bytes>) -> Request<Bytes> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", "Bearer gateway-test-token")
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body(body.into())
        .unwrap()
}

fn json_body(response: &Response<Bytes>) -> Value {
    serde_json::from_slice(response.body()).unwrap()
}

fn initialize(version: &str) -> Value {
    json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{
        "protocolVersion":version, "capabilities":{}, "clientInfo":{"name":"compat-test", "version":"1"}
    }})
}

async fn send(server: &McpServer, handler: &Handler, value: Value) -> Value {
    let response = server
        .handle_message(IncomingMessage::parse(value).unwrap(), handler)
        .await
        .unwrap();
    serde_json::to_value(response).unwrap()
}

#[tokio::test]
async fn initialize_matches_node_http_and_stdio_fixtures() {
    let server = McpServer::gateway();
    let handler = Handler::returning(json!({"content":[]}));
    let value = initialize("2024-10-07");
    let response = server
        .handle_http(
            request("POST", "/mcp", value.to_string()),
            &options(),
            &handler,
        )
        .await;
    let fixture = &baseline()["mcp"]["http"]["initialize"];
    assert_eq!(
        response.status().as_u16(),
        fixture["status"].as_u64().unwrap() as u16
    );
    assert_eq!(
        response.headers()["content-type"],
        fixture["headers"]["content-type"].as_str().unwrap()
    );
    assert_eq!(json_body(&response), fixture["body"]);
    assert!(response.headers().get("mcp-session-id").is_some());
    assert!(response.headers().get("mcp-protocol-version").is_none());

    let mut codec = StdioCodec::default();
    let frame = format!("{}\r\n", value);
    for chunk in frame.as_bytes().chunks(7) {
        codec.append(chunk).unwrap();
    }
    let result = server
        .handle_message(codec.read_message().unwrap().unwrap(), &handler)
        .await
        .unwrap();
    let result = serde_json::to_value(result).unwrap();
    assert_eq!(result, baseline()["mcp"]["stdio"]["initialize"]);
    let encoded = StdioCodec::encode(&result);
    assert_eq!(encoded.last(), Some(&b'\n'));
    assert_eq!(serde_json::from_slice::<Value>(&encoded).unwrap(), result);
    assert!(handler.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn negotiates_versions_and_keeps_sessionless_http_requests_compatible() {
    let server = McpServer::gateway();
    let handler = Handler::returning(json!({"content":[]}));
    for version in SUPPORTED_PROTOCOL_VERSIONS
        .iter()
        .copied()
        .chain(["future-version"])
    {
        let mut init = request("POST", "/mcp", initialize(version).to_string());
        init.headers_mut()
            .insert("mcp-protocol-version", "unsupported".parse().unwrap());
        let result = server.handle_http(init, &options(), &handler).await;
        let expected = if version == "future-version" {
            "2025-11-25"
        } else {
            version
        };
        assert_eq!(result.status(), 200);
        assert_eq!(json_body(&result)["result"]["protocolVersion"], expected);
    }
    for version in SUPPORTED_PROTOCOL_VERSIONS {
        let mut req = request(
            "POST",
            "/mcp",
            json!({"jsonrpc":"2.0","id":2,"method":"ping"}).to_string(),
        );
        req.headers_mut()
            .insert("mcp-protocol-version", version.parse().unwrap());
        let response = server.handle_http(req, &options(), &handler).await;
        assert_eq!(response.status(), 200);
        assert_eq!(
            json_body(&response),
            json!({"result":{},"jsonrpc":"2.0","id":2})
        );
    }
    let mut req = request(
        "POST",
        "/mcp",
        json!({"jsonrpc":"2.0","id":2,"method":"ping"}).to_string(),
    );
    req.headers_mut()
        .insert("mcp-protocol-version", "unsupported".parse().unwrap());
    let response = server.handle_http(req, &options(), &handler).await;
    assert_eq!(response.status(), 400);
    assert_eq!(
        json_body(&response),
        json!({"jsonrpc":"2.0","error":{
        "code":-32000,
        "message":"Bad Request: Unsupported protocol version: unsupported (supported versions: 2025-11-25, 2025-06-18, 2025-03-26, 2024-11-05, 2024-10-07)"
    },"id":null})
    );
}

#[tokio::test]
async fn tool_catalog_matches_every_recorded_node_schema() {
    let expected: Value =
        serde_json::from_str(include_str!("../../tests/fixtures/node-compat-tools.json")).unwrap();
    let server = McpServer::gateway();
    let handler = Handler::returning(json!({"content":[]}));
    for params in [
        json!({}),
        json!({"cursor":""}),
        json!({"cursor":"opaque","ignored":true}),
    ] {
        let result = send(
            &server,
            &handler,
            json!({"jsonrpc":"2.0","id":"catalog","method":"tools/list","params":params}),
        )
        .await;
        assert_eq!(result["result"], json!({"tools":expected}));
        assert_eq!(
            format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&result["result"]["tools"]).unwrap())
            ),
            baseline()["mcp"]["toolsSha256"].as_str().unwrap()
        );
    }
    let null_cursor = send(
        &server,
        &handler,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"cursor":null}}),
    )
    .await;
    assert_eq!(null_cursor["error"]["code"], -32602);
    assert!(
        null_cursor["error"]["message"]
            .as_str()
            .unwrap()
            .contains("expected string, received null")
    );
}

#[tokio::test]
async fn tool_input_defaults_errors_and_unknown_keys_match_node() {
    let server = McpServer::gateway();
    let handler = Handler::returning(json!({"content":[]}));
    let invalid = server.handle_http(request("POST", "/mcp", json!({
        "jsonrpc":"2.0", "id":4, "method":"tools/call", "params":{"name":"t3_threads_list","arguments":{"limit":0}}
    }).to_string()), &options(), &handler).await;
    assert_eq!(
        json_body(&invalid),
        baseline()["mcp"]["http"]["invalidInput"]["body"]
    );
    assert!(handler.calls.lock().unwrap().is_empty());

    send(
        &server,
        &handler,
        json!({"jsonrpc":"2.0","id":"args","method":"tools/call","params":{
            "name":"t3_threads_list", "arguments":{"query":"  repair  ","unknown":"drop"},
            "_meta":{"progressToken":"p-1","extension":{"keep":true}}, "unknown":"ignored"
        }}),
    )
    .await;
    let calls = handler.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].request_id, RequestId::String("args".to_owned()));
    assert_eq!(
        calls[0].arguments,
        json!({
            "includeArchived":false,"query":"repair","sort":"recent","detail":"summary","limit":50
        })
        .as_object()
        .unwrap()
        .clone()
    );
    assert_eq!(
        calls[0].params,
        json!({
            "name":"t3_threads_list", "arguments":{"query":"  repair  ","unknown":"drop"},
            "_meta":{"progressToken":"p-1","extension":{"keep":true}}, "unknown":"ignored"
        })
        .as_object()
        .unwrap()
        .clone()
    );
}

#[tokio::test]
async fn native_tool_results_preserve_upstream_extensions() {
    let expected = json!({
        "content":[{"type":"text","text":"native result","extension":null}],
        "structuredContent":{"upstream":true},
        "_meta":{"progressToken":7,"extension":null}, "isError":false,
        "extension":{"preserve":[null,{},""]}
    });
    let handler = Handler::returning(expected.clone());
    let server = McpServer::new(
        ToolsListPage::parse(json!({"tools":[
            {"name":"native.extension_probe","inputSchema":{"type":"object"}}
        ]}))
        .unwrap(),
    );
    let result = send(
        &server,
        &handler,
        json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{
            "name":"native.extension_probe","arguments":{},"_meta":{"progressToken":0}
        }}),
    )
    .await;
    assert_eq!(result, json!({"result":expected,"jsonrpc":"2.0","id":9}));
    assert_eq!(
        handler.calls.lock().unwrap()[0].params["_meta"]["progressToken"],
        0
    );
}

#[tokio::test]
async fn gateway_output_schema_failures_return_the_node_tool_error_shape() {
    let server = McpServer::gateway();
    let handler = Handler::returning(json!({
        "content":[{"type":"text","text":"status"}],
        "structuredContent":{"connectionStatus":"connected"}
    }));
    let result = send(
        &server,
        &handler,
        json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{
            "name":"t3_connection_status","arguments":{}
        }}),
    )
    .await;
    assert_eq!(result["result"]["isError"], true);
    let text = result["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with(
        "MCP error -32602: Output validation error: Invalid structured content for tool t3_connection_status:"
    ));
    assert_eq!(handler.calls.lock().unwrap().len(), 1);

    let missing = Handler::returning(json!({"content":[]}));
    let result = send(
        &server,
        &missing,
        json!({"jsonrpc":"2.0","id":11,"method":"tools/call","params":{
            "name":"t3_connection_status","arguments":{}
        }}),
    )
    .await;
    assert_eq!(result["result"]["isError"], true);
    assert_eq!(
        result["result"]["content"][0]["text"],
        "MCP error -32602: Output validation error: Tool t3_connection_status has an output schema but no structured content was provided"
    );
}

#[tokio::test]
async fn audit_result_process_ids_follow_the_node_integer_schema() {
    let server = McpServer::gateway();
    for (process_id, valid) in [
        (json!(7), true),
        (json!(-1), true),
        (json!("7"), false),
        (json!(1.5), false),
        (Value::Null, false),
        (json!(9_007_199_254_740_992_u64), false),
    ] {
        let handler = Handler::returning(json!({
            "content":[{"type":"text","text":"audit"}],
            "structuredContent":{
                "environmentId":"fixture", "observedAt":"2026-10-18T00:00:00.000Z", "auditError":null,
                "page":{
                    "items":[{"version":1,"eventId":"event-1","timestamp":"2026-10-18T00:00:00.000Z","processId":process_id,"source":"mcp","event":"tool.result"}],
                    "nextCursor":null,"hasMore":false,"total":1,"invalidLines":0,
                },
            },
        }));
        let result = send(&server, &handler, json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"t3_audit_log","arguments":{}},
        })).await;
        assert_eq!(
            result["result"]["isError"] == true,
            !valid,
            "processId={process_id}: {result}"
        );
        if !valid {
            assert!(
                result["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("processId"),
                "{result}"
            );
        }
    }
}

#[tokio::test]
async fn maps_gateway_failures_and_distinguishes_unknown_tools_from_methods() {
    let handler = Handler {
        calls: Mutex::new(Vec::new()),
        result: Err(ToolFailure::new("task_not_found", "Task was not found.")),
    };
    let server = McpServer::gateway();
    let result = send(
        &server,
        &handler,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":"t3_task_get","arguments":{"taskRef":"missing"}
        }}),
    )
    .await;
    let error = json!({"error":{"code":"task_not_found","message":"Task was not found."}});
    assert_eq!(
        result["result"],
        json!({
            "isError":true,"content":[{"type":"text","text":serde_json::to_string_pretty(&error).unwrap()}],"structuredContent":error
        })
    );
    let unknown = send(&server, &handler, json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"unknown","arguments":{}}})).await;
    assert_eq!(
        unknown,
        json!({"result":{"content":[{"type":"text","text":"MCP error -32602: Tool unknown not found"}],"isError":true},"jsonrpc":"2.0","id":2})
    );
    let method = send(
        &server,
        &handler,
        json!({"jsonrpc":"2.0","id":3,"method":"unknown"}),
    )
    .await;
    assert_eq!(
        method,
        json!({"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"Method not found"}})
    );
    assert_eq!(handler.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn http_batches_return_only_request_responses_and_validate_before_dispatch() {
    let server = McpServer::gateway();
    let handler = Handler::returning(json!({"content":[]}));
    for body in [
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!([
            {"jsonrpc":"2.0","method":"notifications/unknown","params":{"extension":true}},
            {"jsonrpc":"2.0","id":1,"result":{"extension":null}}
        ]),
    ] {
        let response = server
            .handle_http(
                request("POST", "/mcp", body.to_string()),
                &options(),
                &handler,
            )
            .await;
        assert_eq!(response.status(), 202);
        assert!(response.body().is_empty());
        assert!(response.headers().get("content-type").is_none());
    }
    let batch = json!([
        {"jsonrpc":"2.0","id":"first","method":"ping"},
        {"jsonrpc":"2.0","method":"notifications/initialized"},
        {"jsonrpc":"2.0","id":"last","method":"unknown"}
    ]);
    let response = server
        .handle_http(
            request("POST", "/mcp", batch.to_string()),
            &options(),
            &handler,
        )
        .await;
    assert_eq!(
        json_body(&response),
        json!([
            {"result":{},"jsonrpc":"2.0","id":"first"},
            {"jsonrpc":"2.0","id":"last","error":{"code":-32601,"message":"Method not found"}}
        ])
    );
    let single = server
        .handle_http(
            request(
                "POST",
                "/mcp",
                json!([{"jsonrpc":"2.0","id":1,"method":"ping"}]).to_string(),
            ),
            &options(),
            &handler,
        )
        .await;
    assert_eq!(
        json_body(&single),
        json!({"result":{},"jsonrpc":"2.0","id":1})
    );
    let mixed_init = server
        .handle_http(
            request(
                "POST",
                "/mcp",
                json!([initialize("2025-06-18"),{"jsonrpc":"2.0","id":2,"method":"ping"}])
                    .to_string(),
            ),
            &options(),
            &handler,
        )
        .await;
    assert_eq!(mixed_init.status(), 400);
    assert_eq!(json_body(&mixed_init)["error"]["code"], -32600);
    let malformed = server.handle_http(request("POST", "/mcp", json!([
        {"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"t3_connection_status","arguments":{}}},
        {"jsonrpc":"2.0","id":null,"method":"ping"}
    ]).to_string()), &options(), &handler).await;
    assert_eq!(malformed.status(), 400);
    assert_eq!(json_body(&malformed)["error"]["code"], -32600);
    assert!(handler.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn http_routes_auth_and_methods_keep_node_status_headers_and_bodies() {
    let server = McpServer::gateway();
    let handler = Handler::returning(json!({"content":[]}));
    for method in ["GET", "POST", "DELETE", "OPTIONS"] {
        let health = server
            .handle_http(
                request(method, "/healthz?probe=1", ""),
                &HttpOptions::default(),
                &handler,
            )
            .await;
        assert_eq!(health.status(), 200);
        assert_eq!(
            health.headers()["content-type"],
            "application/json; charset=utf-8"
        );
        assert_eq!(
            json_body(&health),
            baseline()["mcp"]["http"]["health"]["body"]
        );
    }
    let missing = server
        .handle_http(request("GET", "/missing", ""), &options(), &handler)
        .await;
    assert_eq!(missing.status(), 404);
    assert_eq!(json_body(&missing), json!({"error":"not_found"}));
    let unconfigured = server
        .handle_http(
            request("POST", "/mcp", "{invalid"),
            &HttpOptions::default(),
            &handler,
        )
        .await;
    assert_eq!(unconfigured.status(), 503);
    assert_eq!(unconfigured.headers()["www-authenticate"], "Bearer");
    assert_eq!(
        json_body(&unconfigured),
        json!({"error":"MCP_BEARER_TOKEN is not configured."})
    );
    for token in [
        None,
        Some("Bearer wrong-token"),
        Some("bearer gateway-test-token"),
        Some("Bearer gateway-test-token "),
    ] {
        let mut req = request("POST", "/mcp", "{invalid");
        req.headers_mut().remove("authorization");
        if let Some(token) = token {
            req.headers_mut()
                .insert("authorization", token.parse().unwrap());
        }
        let response = server.handle_http(req, &options(), &handler).await;
        assert_eq!(response.status(), 401);
        assert_eq!(response.headers()["www-authenticate"], "Bearer");
        assert_eq!(
            json_body(&response),
            baseline()["mcp"]["http"]["unauthorized"]["body"]
        );
    }
    let get = server
        .handle_http(request("GET", "/mcp", ""), &options(), &handler)
        .await;
    assert_eq!(get.status(), 200);
    assert_eq!(get.headers()["content-type"], "text/event-stream");
    assert_eq!(get.headers()["cache-control"], "no-cache, no-transform");
    assert_eq!(get.headers()["connection"], "keep-alive");
    assert_eq!(get.headers()["x-accel-buffering"], "no");
    assert!(get.body().is_empty());
    let delete = server
        .handle_http(request("DELETE", "/mcp", ""), &options(), &handler)
        .await;
    assert_eq!(delete.status(), 200);
    assert!(delete.headers().is_empty());
    assert!(delete.body().is_empty());
    for method in ["PUT", "PATCH", "OPTIONS", "HEAD"] {
        let response = server
            .handle_http(request(method, "/mcp", ""), &options(), &handler)
            .await;
        assert_eq!(response.status(), 405);
        assert_eq!(response.headers()["allow"], "GET, POST, DELETE");
        if method == "HEAD" {
            assert!(response.body().is_empty());
        } else {
            assert_eq!(
                json_body(&response),
                json!({"jsonrpc":"2.0","error":{"code":-32000,"message":"Method not allowed."},"id":null})
            );
        }
    }
}

#[tokio::test]
async fn http_validates_body_before_media_headers_and_enforces_size_in_bytes() {
    let server = McpServer::gateway();
    let handler = Handler::returning(json!({"content":[]}));
    let mut malformed = request("POST", "/mcp", "{not-json");
    malformed.headers_mut().remove("accept");
    let malformed = server.handle_http(malformed, &options(), &handler).await;
    assert_eq!(malformed.status(), 400);
    assert_eq!(malformed.headers()["content-type"], "application/json");
    let malformed_body = json_body(&malformed);
    assert_eq!(malformed_body["jsonrpc"], "2.0");
    assert_eq!(malformed_body["id"], Value::Null);
    assert_eq!(malformed_body["error"]["code"], -32700);
    assert!(malformed_body["error"]["message"].is_string());
    let mut opts = options();
    opts.max_body_bytes = 3;
    let oversized = server
        .handle_http(request("POST", "/mcp", "\"é\""), &opts, &handler)
        .await;
    assert_eq!(oversized.status(), 400);
    assert_eq!(
        json_body(&oversized),
        json!({"error":"Request body is too large."})
    );
    let empty = server
        .handle_http(request("POST", "/mcp", ""), &options(), &handler)
        .await;
    assert_eq!(empty.status(), 400);
    assert_eq!(
        json_body(&empty)["error"]["message"],
        "Parse error: Invalid JSON"
    );
    for (accept, content_type, status) in [
        ("application/json", "application/json", 406),
        ("text/event-stream", "application/json", 406),
        (
            "APPLICATION/JSON, text/event-stream",
            "application/json",
            406,
        ),
        (
            "application/json,text/event-stream",
            "text/plain; a=application/json",
            415,
        ),
        (
            "application/json,text/event-stream",
            "application/json, application/json",
            415,
        ),
        (
            "application/json,text/event-stream",
            "Application/JSON; charset=utf-8",
            200,
        ),
        (
            "application/json,text/event-stream",
            "application/json;",
            200,
        ),
        (
            "application/json,text/event-stream",
            "application/json; a=\"x,y\"",
            200,
        ),
    ] {
        let mut req = request(
            "POST",
            "/mcp",
            json!({"jsonrpc":"2.0","id":1,"method":"ping"}).to_string(),
        );
        req.headers_mut().insert("accept", accept.parse().unwrap());
        req.headers_mut()
            .insert("content-type", content_type.parse().unwrap());
        let response = server.handle_http(req, &options(), &handler).await;
        assert_eq!(response.status(), status, "{accept}, {content_type}");
    }
}

#[test]
fn raw_catalog_pages_validate_before_cursor_conversion_and_keep_opaque_schemas() {
    let page = json!({"tools":[{
        "name":"native", "inputSchema":{"type":"object","properties":{"opaque":[]},"extension":null},
        "outputSchema":{"type":"object","properties":{"huge":{"oneOf":[{},{}]}}},
        "annotations":{"readOnlyHint":true,"unknown":"drop"}, "unknown":"drop"
    }], "nextCursor":"", "extension":null});
    let parsed = ToolsListPage::parse(page.clone()).unwrap();
    assert_eq!(parsed.next_cursor(), Some(""));
    assert_eq!(parsed.as_value()["extension"], Value::Null);
    assert_eq!(
        parsed.tools()[0]["inputSchema"],
        page["tools"][0]["inputSchema"]
    );
    assert_eq!(
        parsed.tools()[0]["outputSchema"],
        page["tools"][0]["outputSchema"]
    );
    assert!(parsed.tools()[0].get("unknown").is_none());
    assert!(parsed.tools()[0]["annotations"].get("unknown").is_none());
    assert_eq!(
        ToolsListPage::parse(json!({"tools":[]}))
            .unwrap()
            .next_cursor(),
        None
    );
    for invalid in [
        json!({"tools":[],"nextCursor":null}),
        json!({"tools":[],"nextCursor":1}),
        json!({"tools":{}}),
        json!({"tools":[{"name":"x"}]}),
        json!({"tools":[{"name":"x","inputSchema":{"type":"array"}}]}),
        json!({"tools":[{"name":"x","inputSchema":{"type":"object"},"outputSchema":null}]}),
    ] {
        assert!(ToolsListPage::parse(invalid.clone()).is_err(), "{invalid}");
    }
}

#[test]
fn stdio_rejects_bad_frames_recovers_on_next_line_and_limits_buffer_bytes() {
    let mut codec = StdioCodec::new(200);
    codec
        .append(b"{bad}\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n")
        .unwrap();
    assert!(matches!(
        codec.read_message(),
        Err(StdioError::InvalidJson(_))
    ));
    assert!(matches!(
        codec.read_message().unwrap(),
        Some(IncomingMessage::Request(_))
    ));
    assert!(codec.read_message().unwrap().is_none());
    for line in [
        "[]\n",
        "\n",
        "{\"jsonrpc\":\"2.0\",\"id\":null,\"method\":\"ping\"}\n",
        "Content-Length: 2\r\n",
    ] {
        codec.append(line.as_bytes()).unwrap();
        assert!(codec.read_message().is_err(), "{line}");
    }
    codec.append(b"unfinished").unwrap();
    assert!(codec.read_message().unwrap().is_none());
    assert_eq!(
        codec.append(&[b'x'; 200]),
        Err(StdioError::BufferLimit(200))
    );
    codec
        .append(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n")
        .unwrap();
    assert!(matches!(
        codec.read_message().unwrap(),
        Some(IncomingMessage::Request(_))
    ));
}

#[test]
fn json_rpc_envelopes_are_strict_and_metadata_extensions_stay_present() {
    for value in [
        json!(null),
        json!({}),
        json!({"jsonrpc":"1.0","id":1,"method":"ping"}),
        json!({"jsonrpc":"2.0","id":null,"method":"ping"}),
        json!({"jsonrpc":"2.0","id":1.5,"method":"ping"}),
        json!({"jsonrpc":"2.0","id":9007199254740992_u64,"method":"ping"}),
        json!({"jsonrpc":"2.0","id":1,"method":"ping","unknown":true}),
        json!({"jsonrpc":"2.0","id":1,"method":"ping","params":null}),
        json!({"jsonrpc":"2.0","id":1,"method":"ping","params":{"_meta":{"progressToken":null}}}),
        json!({"jsonrpc":"2.0","id":1,"result":[]}),
    ] {
        assert!(IncomingMessage::parse(value.clone()).is_err(), "{value}");
    }
    let parsed = IncomingMessage::parse(json!({"jsonrpc":"2.0","id":"","method":"ping","params":{
        "extension":null,"_meta":{"progressToken":"","opaque":null,"io.modelcontextprotocol/related-task":{"taskId":"task-1","drop":true}}
    }})).unwrap();
    let IncomingMessage::Request(parsed) = parsed else {
        panic!("request")
    };
    assert_eq!(parsed.id, RequestId::String(String::new()));
    assert_eq!(parsed.params.unwrap(), json!({"extension":null,"_meta":{
        "progressToken":"","opaque":null,"io.modelcontextprotocol/related-task":{"taskId":"task-1","drop":true}
    }}).as_object().unwrap().clone());
}
#[tokio::test]
async fn invalid_numeric_limits_never_reach_the_tool_handler() {
    let server = McpServer::gateway();
    let handler = Handler::returning(json!({"content":[]}));
    for (name, field, literals) in [
        (
            "t3_audit_log",
            "limit",
            vec![
                "1.5",
                "0",
                "101",
                "9007199254740992",
                "9.007199254740992e15",
                "1e400",
            ],
        ),
        (
            "t3_git_diff",
            "maxBytes",
            vec![
                "1024.5",
                "1023",
                "1000001",
                "9007199254740992",
                "9.007199254740992e15",
                "1e400",
            ],
        ),
    ] {
        for literal in literals {
            let required = if name == "t3_git_diff" {
                r#""projectId":"fixture","mode":"unstaged","#
            } else {
                ""
            };
            let request: Value = serde_json::from_str(&format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{name}","arguments":{{{required}"{field}":{literal}}}}}}}"#
            )).unwrap();
            let result = send(&server, &handler, request).await;
            assert_eq!(
                result["result"]["isError"], true,
                "{name} {literal}: {result}"
            );
            assert!(
                result["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains(field),
                "{result}"
            );
        }
    }
    assert!(handler.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn accepted_native_integer_arguments_keep_their_json_representation() {
    let server = McpServer::new(
        ToolsListPage::parse(json!({"tools":[{
            "name":"native.integer_probe","inputSchema":{
                "type":"object","properties":{"count":{"type":"integer"}},"required":["count"]
            }
        }]}))
        .unwrap(),
    );
    let handler = Handler::returning(json!({"content":[]}));
    for literal in ["1.0", "1e0"] {
        let request: Value = serde_json::from_str(&format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"native.integer_probe","arguments":{{"count":{literal}}}}}}}"#
        )).unwrap();
        let expected = request["params"]["arguments"]["count"].clone();
        let result = send(&server, &handler, request).await;
        assert_ne!(result["result"]["isError"], true, "{result}");
        assert_eq!(
            handler.calls.lock().unwrap().last().unwrap().arguments["count"],
            expected
        );
    }
}

#[tokio::test]
async fn integral_audit_output_numbers_are_validated_without_rewriting_results() {
    let server = McpServer::gateway();
    for literal in ["7.0", "7e0"] {
        let process_id: Value = serde_json::from_str(literal).unwrap();
        let expected = json!({
            "environmentId":"fixture","observedAt":"2026-10-18T00:00:00.000Z","auditError":null,
            "page":{
                "items":[{"version":1,"eventId":"event-1","timestamp":"2026-10-18T00:00:00.000Z","processId":process_id,"source":"mcp","event":"tool.result"}],
                "nextCursor":null,"hasMore":false,"total":1,"invalidLines":0,
            },
        });
        let handler = Handler::returning(json!({"content":[],"structuredContent":expected}));
        let result = send(&server, &handler, json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"t3_audit_log","arguments":{}},
        })).await;
        assert_ne!(result["result"]["isError"], true, "{result}");
        assert_eq!(result["result"]["structuredContent"], expected);
    }
}

#[tokio::test]
async fn invalid_request_parameters_use_the_json_rpc_invalid_params_code() {
    let server = McpServer::gateway();
    let handler = Handler::returning(json!({"content":[]}));
    for request in [
        json!({"jsonrpc":"2.0","id":7,"method":"initialize"}),
        json!({"jsonrpc":"2.0","id":7,"method":"initialize","params":{
            "protocolVersion":1,"capabilities":[],"clientInfo":{"name":1}
        }}),
        json!({"jsonrpc":"2.0","id":7,"method":"tools/call"}),
        json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{}}),
        json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{
            "name":"t3_threads_list","arguments":null
        }}),
        json!({"jsonrpc":"2.0","id":7,"method":"tools/list","params":{"cursor":false}}),
    ] {
        let response = send(&server, &handler, request).await;
        assert_eq!(response["id"], 7);
        assert_eq!(response["error"]["code"], -32602, "{response}");
        assert!(response.get("result").is_none(), "{response}");
    }
    assert!(handler.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unsupported_task_creation_is_a_parameter_error() {
    let server = McpServer::gateway();
    let handler = Handler::returning(json!({"content":[]}));
    let response = send(
        &server,
        &handler,
        json!({"jsonrpc":"2.0","id":"unsupported-task","method":"tools/call","params":{
            "name":"t3_connection_status","arguments":{},"task":{}
        }}),
    )
    .await;
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], "unsupported-task");
    assert_eq!(response["error"]["code"], -32602, "{response}");
    assert!(response.get("result").is_none());
    assert!(handler.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn lossless_audit_results_still_enforce_the_output_schema() {
    use t3_code_mcp::mcp::ToolResult;
    use t3_code_mcp::operations::audit_log::AuditValue;
    let row: AuditValue = serde_json::from_str(r#"{"version":1,"eventId":"event-\ud800","timestamp":"2026-10-18T00:00:00.000Z","processId":1,"source":"mcp","event":"stored"}"#).unwrap();
    let server = McpServer::gateway();
    for (process_id, valid) in [(json!(7), true), (json!(7.5), false)] {
        let mut row = row.clone();
        row.as_object_mut()
            .unwrap()
            .insert("processId".into(), process_id.into());
        let page = AuditValue::object([
            ("items", AuditValue::Array(vec![row.clone()])),
            ("total", json!(1).into()),
            ("invalidLines", json!(0).into()),
            ("nextCursor", AuditValue::Null),
            ("hasMore", AuditValue::Bool(false)),
        ]);
        let structured = AuditValue::object([
            ("environmentId", "fixture".into()),
            ("observedAt", "2026-10-18T00:00:00.000Z".into()),
            ("auditError", AuditValue::Null),
            ("page", page),
        ]);
        let handler = Handler {
            calls: Mutex::new(Vec::new()),
            result: Ok(ToolResult::structured(&structured).unwrap()),
        };
        let response = server.handle_http(request("POST", "/mcp", json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"t3_audit_log","arguments":{}}
        }).to_string()), &options(), &handler).await;
        let response: AuditValue = serde_json::from_slice(response.body()).unwrap();
        assert_eq!(response["result"]["isError"] == true, !valid, "{response}");
        if valid {
            assert_eq!(
                response["result"]["structuredContent"]["page"]["items"][0],
                row
            );
        } else {
            assert!(
                response["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("processId")
            );
        }
    }
}
