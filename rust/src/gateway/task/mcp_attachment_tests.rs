use super::Fixture;
use crate::{
    gateway::T3Gateway,
    mcp::{IncomingMessage, McpServer},
    t3::test_server::{Reply, Server},
};
use futures_util::FutureExt;
use serde_json::{Value, json};

async fn fixture(upload_reply: Reply) -> Fixture {
    let server = Server::start(move |request| {
        let upload_reply = upload_reply.clone();
        async move {
            if request.path == "/api/attachments/upload/signed-token" {
                return upload_reply;
            }
            if request.path != "/mcp" {
                return Reply::status(404, Value::Null);
            }
            if request.method == "GET" {
                return Reply::status(405, Value::Null);
            }
            if request.body.get("id").is_none() {
                return Reply::status(202, Value::Null);
            }
            let result = match request.body["method"].as_str() {
                Some("initialize") => json!({
                    "protocolVersion":"2025-11-25", "capabilities":{},
                    "serverInfo":{"name":"attachment-fixture","version":"1"},
                }),
                Some("tools/list") => json!({"tools":[
                    {"name":"t3_attachment_prepare_upload","inputSchema":{"type":"object"}},
                    {"name":"t3_attachment_discard","inputSchema":{"type":"object"}},
                ]}),
                Some("tools/call") => match request.body["params"]["name"].as_str() {
                    Some("t3_attachment_prepare_upload") => json!({
                        "content":[], "structuredContent":{
                            "attachmentId":"attachment-prepared",
                            "relativeUrl":"/api/attachments/upload/signed-token",
                        },
                    }),
                    Some("t3_attachment_discard") => json!({"content":[]}),
                    _ => return Reply::status(404, Value::Null),
                },
                _ => return Reply::status(404, Value::Null),
            };
            Reply::json(json!({"jsonrpc":"2.0", "id":request.body["id"], "result":result}))
        }
        .boxed()
    })
    .await;
    let mut fixture = Fixture::new().await;
    let mut config = fixture.gateway.config().clone();
    config.endpoint.t3_http_base_url = server.url.clone();
    config.endpoint.t3_native_mcp_url = format!("{}/mcp", server.url);
    fixture.server = server;
    fixture.gateway = T3Gateway::new(config);
    fixture.authorize_native();
    fixture
}

async fn upload(fixture: &Fixture, arguments: Value) -> Value {
    let response = McpServer::gateway()
        .handle_message(
            IncomingMessage::parse(json!({
                "jsonrpc":"2.0", "id":1, "method":"tools/call",
                "params":{"name":"t3_attachment_upload","arguments":arguments},
            }))
            .unwrap(),
            &fixture.gateway,
        )
        .await
        .unwrap();
    assert!(response.get("error").is_none(), "{response}");
    let result = &response["result"];
    let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, result["structuredContent"]);
    result.clone()
}

fn native_calls(fixture: &Fixture) -> Vec<Value> {
    fixture
        .server
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.path == "/mcp" && request.body["method"] == "tools/call")
        .map(|request| {
            assert_eq!(
                request.headers.get("authorization").map(String::as_str),
                Some("Bearer fixture-access-token")
            );
            request.body["params"].clone()
        })
        .collect()
}

fn assert_upload(fixture: &Fixture, mime_type: &str, bytes: &[u8]) {
    let requests = fixture.server.requests.lock().unwrap();
    let uploads = requests
        .iter()
        .filter(|request| request.path.starts_with("/api/attachments/upload/"))
        .collect::<Vec<_>>();
    assert_eq!(uploads.len(), 1, "{requests:?}");
    let request = uploads[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/api/attachments/upload/signed-token");
    assert_eq!(
        request.headers.get("content-type").map(String::as_str),
        Some(mime_type)
    );
    assert_eq!(request.body_bytes, bytes);
    assert!(!request.headers.contains_key("authorization"));
}

#[tokio::test]
async fn mcp_attachment_upload_preserves_metadata_bytes_and_signed_upload_authentication() {
    for (arguments, attachment, bytes) in [
        (
            json!({"name":"shot.png","mimeType":"image/png","dataBase64":"AAEC/w=="}),
            json!({"type":"image","id":"attachment-prepared","name":"shot.png","mimeType":"image/png","sizeBytes":4}),
            b"\x00\x01\x02\xff".as_slice(),
        ),
        (
            json!({"name":"notes.txt","mimeType":"text/plain","dataBase64":"aGk="}),
            json!({"type":"file","id":"attachment-prepared","name":"notes.txt","mimeType":"text/plain","sizeBytes":2}),
            b"hi".as_slice(),
        ),
    ] {
        let fixture = fixture(Reply::status(204, Value::Null)).await;
        let result = upload(&fixture, arguments).await;
        assert_ne!(result["isError"], true, "{result}");
        assert_eq!(result["structuredContent"]["attachment"], attachment);
        assert_eq!(
            native_calls(&fixture),
            vec![json!({
                "name":"t3_attachment_prepare_upload", "arguments":{"upload":{
                    "type":attachment["type"], "name":attachment["name"],
                    "mimeType":attachment["mimeType"], "sizeBytes":attachment["sizeBytes"],
                }},
            })]
        );
        assert_upload(&fixture, attachment["mimeType"].as_str().unwrap(), bytes);
    }
}

#[tokio::test]
async fn mcp_attachment_upload_discards_the_prepared_attachment_after_http_failure() {
    let fixture = fixture(Reply::status(400, json!({"message":"upload refused"}))).await;
    let result = upload(
        &fixture,
        json!({"name":"notes.txt","mimeType":"text/plain","dataBase64":"aGk="}),
    )
    .await;
    assert_eq!(result["isError"], true, "{result}");
    assert_eq!(
        result["structuredContent"]["error"]["code"],
        "native_upload_failed"
    );
    assert_eq!(
        native_calls(&fixture),
        vec![
            json!({"name":"t3_attachment_prepare_upload","arguments":{"upload":{
                "type":"file","name":"notes.txt","mimeType":"text/plain","sizeBytes":2,
            }}}),
            json!({"name":"t3_attachment_discard","arguments":{"attachmentId":"attachment-prepared"}}),
        ]
    );
    assert_upload(&fixture, "text/plain", b"hi");
}

#[tokio::test]
async fn mcp_attachment_upload_rejects_invalid_base64_before_any_native_request() {
    let fixture = fixture(Reply::status(204, Value::Null)).await;
    let result = upload(
        &fixture,
        json!({"name":"notes.txt","mimeType":"text/plain","dataBase64":"not base64!"}),
    )
    .await;
    assert_eq!(result["isError"], true, "{result}");
    assert_eq!(
        result["structuredContent"]["error"]["code"],
        "invalid_attachment"
    );
    assert!(fixture.server.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn mcp_attachment_upload_refuses_redirects_discards_and_never_contacts_the_target() {
    let elsewhere = Server::start(|_| async { Reply::status(204, Value::Null) }.boxed()).await;
    for status in [302, 307] {
        let mut reply = Reply::status(status, Value::Null);
        reply.headers.push((
            "location".into(),
            format!("{}/api/attachments/upload/elsewhere", elsewhere.url),
        ));
        let fixture = fixture(reply).await;
        let result = upload(
            &fixture,
            json!({"name":"notes.txt","mimeType":"text/plain","dataBase64":"aGk="}),
        )
        .await;
        assert_eq!(result["isError"], true, "{status}: {result}");
        assert_eq!(
            result["structuredContent"]["error"]["code"],
            "native_upload_failed"
        );
        assert_eq!(
            native_calls(&fixture),
            vec![
                json!({"name":"t3_attachment_prepare_upload","arguments":{"upload":{
                    "type":"file","name":"notes.txt","mimeType":"text/plain","sizeBytes":2,
                }}}),
                json!({"name":"t3_attachment_discard","arguments":{"attachmentId":"attachment-prepared"}}),
            ]
        );
        assert_upload(&fixture, "text/plain", b"hi");
        assert!(elsewhere.requests.lock().unwrap().is_empty());
    }
}
