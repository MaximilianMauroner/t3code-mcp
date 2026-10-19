use super::{Credentials, initialize, response};
use crate::{
    config::{GatewayConfig, NativeEndpointConfig},
    gateway::T3Gateway,
    mcp::{IncomingMessage, McpServer, ToolHandler, ToolsListPage},
    t3::test_server::Server,
};
use futures_util::FutureExt;
use serde_json::{Value, json};

fn raw_tools() -> Vec<Value> {
    vec![
        json!({
            "name":"t3_thread_list",
            "title":"Native threads",
            "description":"Native description",
            "inputSchema":{
                "type":"object",
                "properties":{
                    "allowed":true,
                    "forbidden":false,
                    "nested":{
                        "type":"object",
                        "properties":{"allowed":true,"forbidden":false},
                        "additionalProperties":false,
                    },
                },
                "$defs":{"anything":true,"nothing":false},
                "allOf":[true,{"additionalProperties":true}],
                "vendor/schema":{"preserve":[null,{},[]]},
            },
            "outputSchema":{
                "type":"object",
                "properties":{"allowed":true,"forbidden":false},
                "unevaluatedProperties":false,
            },
            "annotations":{
                "title":"Native hints",
                "readOnlyHint":false,
                "destructiveHint":true,
                "idempotentHint":false,
                "openWorldHint":true,
                "vendor/hint":{"preserve":[null,false]},
            },
            "execution":{
                "taskSupport":"optional",
                "vendor/execution":{"preserve":true},
            },
            "icons":[{
                "src":"data:image/png;base64,AA==",
                "mimeType":"image/png",
                "sizes":["16x16"],
                "theme":"dark",
                "_meta":{"vendor/icon":true},
                "vendor/icon":{"preserve":null},
            }],
            "_meta":{
                "io.modelcontextprotocol/related-task":{"taskId":"task-1","extension":true},
                "vendor/meta":{"preserve":[1,true,null]},
            },
            "vendor/tool":{"preserve":[null,false,7,"",{},[]]},
        }),
        json!({
            "name":"t3_thread_send",
            "inputSchema":{"type":"object"},
            "annotations":{"readOnlyHint":true,"vendor/hint":"native"},
            "execution":{"taskSupport":"required"},
        }),
        json!({
            "name":"t3_thread_interrupt",
            "inputSchema":{"type":"object"},
            "vendor/tool":null,
        }),
    ]
}

async fn fixture(read_only: bool) -> (Server, Credentials, T3Gateway) {
    let server = Server::start(|request| {
        async move {
            if let Some(reply) = initialize(&request) {
                return reply;
            }
            if request.body["method"] == "tools/list" {
                let mut tools = raw_tools();
                for name in [
                    "delegate_task",
                    "t3_attachment_prepare_upload",
                    "unknown_native_tool",
                    "t3_connection_status",
                ] {
                    tools.push(json!({"name":name,"inputSchema":{"type":"object"}}));
                }
                response(&request, json!({"tools":tools}))
            } else {
                response(&request, json!({"content":[]}))
            }
        }
        .boxed()
    })
    .await;
    let credentials = Credentials::new();
    credentials.write("one");
    let gateway = T3Gateway::new(GatewayConfig {
        endpoint: NativeEndpointConfig {
            t3_http_base_url: server.url.clone(),
            t3_native_mcp_url: format!("{}/mcp", server.url),
            data_dir: credentials.dir.clone(),
        },
        t3_access_token: "fixture-token".into(),
        mcp_bearer_token: None,
        read_only,
        host: "127.0.0.1".into(),
        port: 0,
        max_body_bytes: 2 * 1024 * 1024,
        environment_id: None,
        environment_label: None,
        worktree_root: None,
        stale_after_ms: 30_000,
    });
    (server, credentials, gateway)
}

#[tokio::test]
async fn downstream_tools_list_preserves_allowed_native_definitions() {
    for read_only in [false, true] {
        let (_upstream, _credentials, gateway) = fixture(read_only).await;
        let server = McpServer::new(gateway.tool_catalog().await.unwrap());
        let result = server
            .handle_message(
                IncomingMessage::parse(json!({
                    "jsonrpc":"2.0","id":1,"method":"tools/list",
                }))
                .unwrap(),
                &gateway,
            )
            .await
            .unwrap();
        let mut expected = ToolsListPage::gateway().tools().to_vec();
        expected.extend(raw_tools());
        assert_eq!(result["result"], json!({"tools":expected}));
    }
}

#[tokio::test]
async fn native_hints_do_not_control_mutation_access() {
    for (read_only, expected_code) in [(false, "t3_scope_required"), (true, "gateway_read_only")] {
        let (upstream, _credentials, gateway) = fixture(read_only).await;
        let server = McpServer::new(gateway.tool_catalog().await.unwrap());
        let denied = server
            .handle_message(
                IncomingMessage::parse(json!({
                    "jsonrpc":"2.0","id":1,"method":"tools/call",
                    "params":{"name":"t3_thread_send","arguments":{}},
                }))
                .unwrap(),
                &gateway,
            )
            .await
            .unwrap();
        assert_eq!(denied["result"]["isError"], true);
        assert_eq!(
            denied["result"]["structuredContent"]["error"]["code"],
            expected_code
        );
        let allowed = server
            .handle_message(
                IncomingMessage::parse(json!({
                    "jsonrpc":"2.0","id":2,"method":"tools/call",
                    "params":{"name":"t3_thread_list","arguments":{}},
                }))
                .unwrap(),
                &gateway,
            )
            .await
            .unwrap();
        assert_eq!(allowed["result"], json!({"content":[]}));
        let calls = upstream
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.body["method"] == "tools/call")
            .map(|request| request.body["params"]["name"].clone())
            .collect::<Vec<_>>();
        assert_eq!(calls, [json!("t3_thread_list")]);
    }
}
