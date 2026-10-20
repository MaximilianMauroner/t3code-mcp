use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use http::Request;
use serde_json::{Value, json};
use t3_code_mcp::mcp::{HttpOptions, McpServer, ToolCall, ToolFailure, ToolHandler, ToolsListPage};

#[derive(Default)]
struct Handler(AtomicUsize);

impl ToolHandler for Handler {
    async fn call_tool(
        &self,
        call: ToolCall,
        _invocation: Option<t3_code_mcp::mcp::Invocation>,
    ) -> Result<t3_code_mcp::mcp::ToolResult, ToolFailure> {
        assert_eq!(call.name, "fixture.echo");
        assert!(call.arguments.is_empty());
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(t3_code_mcp::mcp::ToolResult::new(&json!({"content":[]})).unwrap())
    }
}

async fn post(handler: &Handler, body: impl Into<Bytes>) -> (u16, Value) {
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("authorization", "Bearer protocol-error-test")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body.into())
        .unwrap();
    let server = McpServer::new(
        ToolsListPage::parse(json!({"tools":[{
            "name":"fixture.echo","inputSchema":{"type":"object"}
        }]}))
        .unwrap(),
    );
    let response = server
        .handle_http(
            request,
            &HttpOptions {
                bearer_token: Some("protocol-error-test".into()),
                ..HttpOptions::default()
            },
            handler,
        )
        .await;
    (
        response.status().as_u16(),
        serde_json::from_slice(response.body()).unwrap(),
    )
}

fn assert_error(response: &Value, code: i64) {
    assert_eq!(response["jsonrpc"], "2.0", "{response}");
    assert_eq!(response.get("id"), Some(&Value::Null), "{response}");
    assert_eq!(response["error"]["code"], code, "{response}");
    assert!(response["error"]["message"].is_string(), "{response}");
    assert!(response.get("result").is_none(), "{response}");
}

#[tokio::test]
async fn malformed_json_returns_a_json_rpc_parse_error() {
    let handler = Handler::default();
    for body in ["", "{not-json", "{\"id\":", "true trailing", "{\"x\":1,}"] {
        let (status, response) = post(&handler, body).await;
        assert_eq!(status, 400, "{body}: {response}");
        assert_error(&response, -32700);
    }
    assert_eq!(handler.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn malformed_utf8_is_not_replaced_with_a_valid_request_character() {
    let handler = Handler::default();
    let mut body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"fixture.echo","arguments":{},"extension":""#
        .to_vec();
    body.push(0xff);
    body.extend_from_slice(br#""}}"#);
    let (status, response) = post(&handler, body).await;
    assert_eq!(status, 400, "{response}");
    assert_error(&response, -32700);
    assert_eq!(handler.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn valid_json_with_an_invalid_envelope_returns_invalid_request() {
    let handler = Handler::default();
    for value in [
        Value::Null,
        json!({}),
        json!([]),
        json!(42),
        json!({"jsonrpc":"1.0","id":1,"method":"ping"}),
        json!({"jsonrpc":"2.0","id":null,"method":"ping"}),
        json!({"jsonrpc":"2.0","id":1,"method":42}),
    ] {
        let (status, response) = post(&handler, value.to_string()).await;
        assert_eq!(status, 400, "{value}: {response}");
        assert_error(&response, -32600);
    }
    let (status, response) = post(
        &handler,
        json!({"jsonrpc":"2.0","id":"still-usable","method":"tools/call","params":{"name":"fixture.echo","arguments":{}}})
            .to_string(),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["id"], "still-usable");
    assert_eq!(response["result"]["content"], json!([]));
    assert_eq!(handler.0.load(Ordering::SeqCst), 1);
}
