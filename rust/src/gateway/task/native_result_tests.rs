use crate::{
    config::{GatewayConfig, NativeEndpointConfig},
    gateway::T3Gateway,
    mcp::{HttpOptions, McpServer},
    native_credential::{NativeAccessLevel, NativeCredential, NativeCredentialStore},
    t3::{
        native_mcp::NativeMcpClient,
        test_server::{Reply, Server},
    },
};
use bytes::Bytes;
use futures_util::FutureExt;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};

const RAW_RESULT: &str = r#"{
    "content":[
        {
            "type":"text","text":"private-native-text",
            "annotations":{
                "audience":["user","assistant"],"priority":0.25,
                "lastModified":"2026-10-18T00:00:00Z",
                "vendor/annotation":{"keep":[null,false,{},[]]}
            },
            "_meta":{"vendor/content":{"keep":true}},
            "vendor/block":{"keep":[null,false,7,"",{},[]]}
        },
        {
            "type":"image","data":"cHJpdmF0ZS1pbWFnZQ==","mimeType":"image/png",
            "annotations":{"vendor/annotation":null},
            "_meta":{"vendor/image":true},"vendor/block":{"image":true}
        },
        {
            "type":"audio","data":"cHJpdmF0ZS1hdWRpbw==","mimeType":"audio/wav",
            "annotations":{"vendor/annotation":[false]},
            "_meta":{"vendor/audio":true},"vendor/block":{"audio":true}
        },
        {
            "type":"resource_link","uri":"file:///private-link","name":"link",
            "title":"Native link","description":"private-native-description",
            "mimeType":"text/plain","size":17,
            "icons":[{
                "src":"data:image/png;base64,AA==","mimeType":"image/png",
                "sizes":["16x16"],"theme":"dark",
                "_meta":{"vendor/icon":true},"vendor/icon":{"keep":null}
            }],
            "annotations":{"vendor/annotation":{"link":true}},
            "_meta":{"vendor/link":true},"vendor/block":{"link":true}
        },
        {
            "type":"resource",
            "resource":{
                "uri":"file:///private-text","mimeType":"text/plain",
                "text":"private-native-resource-text",
                "_meta":{"vendor/resource":{"text":true}},
                "vendor/resource":{"keep":[null,false,{},[]]}
            },
            "annotations":{"vendor/annotation":{"resource":"text"}},
            "_meta":{"vendor/embedded":true},"vendor/block":{"resource":"text"}
        },
        {
            "type":"resource",
            "resource":{
                "uri":"file:///private-blob","mimeType":"application/octet-stream",
                "blob":"cHJpdmF0ZS1ibG9i",
                "_meta":{"vendor/resource":{"blob":true}},
                "vendor/resource":{"keep":[null,false,{},[]]}
            },
            "annotations":{"vendor/annotation":{"resource":"blob"}},
            "_meta":{"vendor/embedded":false},"vendor/block":{"resource":"blob"}
        }
    ],
    "structuredContent":{"privateValue":"private-native-structured","nested":[null,false,{}]},
    "_meta":{"vendor/result":{"private":"private-native-result-meta"}},
    "vendor/result":{"keep":[null,false,7,"",{},[]]},
    "vendor/null":null
}"#;

struct NativeFixture {
    gateway: T3Gateway,
    upstream: Server,
    directory: PathBuf,
}

impl NativeFixture {
    async fn new(reply: Reply) -> Self {
        let upstream = Server::start(move |request| {
            let reply = reply.clone();
            async move {
                if request.method == "GET" {
                    return Reply::status(405, Value::Null);
                }
                let result = match request.body["method"].as_str() {
                    Some("initialize") => json!({
                        "protocolVersion":"2025-11-25","capabilities":{},
                        "serverInfo":{"name":"native-results-fixture","version":"1"},
                    }),
                    Some("tools/list") => json!({"tools":[
                        {"name":"t3_thread_list","inputSchema":{"type":"object"}},
                        {"name":"t3_thread_send","inputSchema":{"type":"object"}},
                    ]}),
                    Some("tools/call") => {
                        let mut reply = reply;
                        if reply.body.is_object() {
                            reply.body["jsonrpc"] = json!("2.0");
                            reply.body["id"] = request.body["id"].clone();
                        }
                        return reply;
                    }
                    _ => return Reply::status(202, Value::Null),
                };
                Reply::json(json!({"jsonrpc":"2.0","id":request.body["id"],"result":result}))
            }
            .boxed()
        })
        .await;
        let directory =
            std::env::temp_dir().join(format!("t3-native-result-{}", uuid::Uuid::new_v4()));
        NativeCredentialStore::in_data_dir(&directory)
            .write(&NativeCredential {
                resource: format!("{}/mcp", upstream.url),
                client_id: "fixture-client".into(),
                access_token: "fixture-native-token".into(),
                access: NativeAccessLevel::FullAccess,
                scopes: vec!["orchestration:read".into(), "orchestration:operate".into()],
                obtained_at: "2026-10-18T00:00:00Z".into(),
                expires_at: "2099-10-18T00:00:00Z".into(),
            })
            .unwrap();
        let gateway = T3Gateway::new(GatewayConfig {
            endpoint: NativeEndpointConfig {
                t3_http_base_url: upstream.url.clone(),
                t3_native_mcp_url: format!("{}/mcp", upstream.url),
                data_dir: directory.clone(),
            },
            t3_access_token: "fixture-http-token".into(),
            mcp_bearer_token: None,
            read_only: false,
            host: "127.0.0.1".into(),
            port: 0,
            max_body_bytes: 2 * 1024 * 1024,
            environment_id: None,
            environment_label: None,
            worktree_root: None,
            stale_after_ms: 30_000,
        });
        Self {
            gateway,
            upstream,
            directory,
        }
    }

    async fn call(&self, name: &str, id: Value) -> Value {
        let request = http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("authorization", "Bearer fixture-downstream-token")
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .body(Bytes::from(
                serde_json::to_vec(&json!({
                    "jsonrpc":"2.0","id":id,"method":"tools/call",
                    "params":{"name":name,"arguments":{}},
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = McpServer::gateway()
            .handle_http(
                request,
                &HttpOptions {
                    bearer_token: Some("fixture-downstream-token".into()),
                    ..HttpOptions::default()
                },
                &self.gateway,
            )
            .await;
        assert_eq!(response.status(), 200);
        serde_json::from_slice(response.body()).unwrap()
    }

    fn calls(&self) -> usize {
        self.upstream
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.body["method"] == "tools/call")
            .count()
    }

    fn audit(&self) -> String {
        std::fs::read_to_string(self.directory.join("audit.jsonl")).unwrap()
    }
}

impl Drop for NativeFixture {
    fn drop(&mut self) {
        self.gateway.native.close();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[tokio::test]
async fn native_success_and_is_error_preserve_every_result_extension_over_http() {
    for is_error in [None, Some(false), Some(true)] {
        let mut expected: Value = serde_json::from_str(RAW_RESULT).unwrap();
        if let Some(is_error) = is_error {
            expected["isError"] = json!(is_error);
        }
        let fixture = NativeFixture::new(Reply::json(json!({"result":expected}))).await;
        let response = fixture.call("t3_thread_list", json!("caller-result")).await;
        assert_eq!(
            response,
            json!({"jsonrpc":"2.0","id":"caller-result","result":expected})
        );
        assert_eq!(fixture.calls(), 1);
        let audit = fixture.audit();
        for private in [
            "private-native-text",
            "private-native-description",
            "private-native-resource-text",
            "private-native-structured",
            "private-native-result-meta",
            "cHJpdmF0ZS1pbWFnZQ==",
            "cHJpdmF0ZS1hdWRpbw==",
            "cHJpdmF0ZS1ibG9i",
        ] {
            assert!(!audit.contains(private), "Audit exposed {private}");
        }
        let events: Vec<Value> = audit
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let result = events
            .iter()
            .find(|event| event["source"] == "mcp" && event["event"] == "tool.result")
            .unwrap();
        assert_eq!(
            result["outcome"],
            if is_error == Some(true) {
                "error"
            } else {
                "completed"
            }
        );
        assert_eq!(
            result["details"]["result"],
            json!({"isError":is_error == Some(true),"keys":["privateValue","nested"]})
        );
    }
}

#[tokio::test]
async fn native_protocol_errors_follow_gateway_error_and_retry_contract() {
    for name in ["t3_thread_list", "t3_thread_send"] {
        for code in [-32602, -32000, -32001, -32800, 42] {
            for id in [json!("caller-protocol"), json!(739)] {
                let message = match code {
                    -32000 => "Connection closed",
                    -32001 => "Request timed out",
                    _ => "Native rejection",
                };
                let error = json!({
                    "code":code,"message":message,
                    "data":{"reason":"private-native-error-data","nested":[null,false,{"value":7}]},
                });
                let fixture = NativeFixture::new(Reply::json(json!({"error":error}))).await;
                let response = fixture.call(name, id.clone()).await;
                assert_eq!(response["id"], id);
                let unknown = matches!(code, -32000 | -32001);
                if code == -32800 {
                    assert_eq!(response, json!({"jsonrpc":"2.0","id":id,"error":error}));
                } else {
                    assert!(response.get("error").is_none(), "{response}");
                    assert_eq!(response["result"]["isError"], true, "{response}");
                    assert_eq!(
                        response["result"]["structuredContent"]["error"]["code"],
                        if unknown {
                            if name == "t3_thread_list" {
                                "native_unavailable"
                            } else {
                                "native_outcome_unknown"
                            }
                        } else {
                            "native_rejected"
                        },
                    );
                }
                assert_eq!(
                    fixture.calls(),
                    if unknown && name == "t3_thread_list" {
                        2
                    } else {
                        1
                    }
                );
                assert!(!fixture.audit().contains("private-native-error-data"));
                let audit = fixture.audit();
                let events: Vec<Value> = audit
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert!(
                    events
                        .iter()
                        .any(|event| event["operation"] == format!("mcp:{name}")
                            && event["details"]["failure"]
                                == if unknown { "unknown" } else { "rejected" })
                );
            }
        }
    }
}

#[tokio::test]
async fn native_rejections_do_not_expose_protocol_error_data() {
    for data in [
        None,
        Some(Value::Null),
        Some(json!([1, false, null])),
        Some(json!("detail")),
    ] {
        let mut error = json!({"code":-32603,"message":"Native error"});
        if let Some(data) = data {
            error["data"] = data;
        }
        let fixture = NativeFixture::new(Reply::json(json!({"error":error}))).await;
        let response = fixture.call("t3_thread_list", json!(93)).await;
        assert_eq!(response["id"], 93);
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(response["result"]["isError"], true);
        assert_eq!(
            response["result"]["structuredContent"]["error"],
            json!({
                "code":"native_rejected",
                "message":"Native T3 rejected t3_thread_list: MCP error -32603: Native error",
            })
        );
        assert_eq!(fixture.calls(), 1);
    }
}

#[tokio::test]
async fn native_policy_denial_remains_a_gateway_tool_error_without_sending_a_call() {
    for read_only in [true, false] {
        let mut fixture = NativeFixture::new(Reply::json(json!({"result":{"content":[]}}))).await;
        fixture.gateway.config.read_only = read_only;
        if !read_only {
            let mut store = NativeCredentialStore::in_data_dir(&fixture.directory);
            let mut credential = match store.read() {
                crate::native_credential::NativeCredentialState::Valid { credential } => credential,
                _ => panic!("Invalid fixture credential"),
            };
            credential.access = NativeAccessLevel::ReadOnly;
            credential.scopes = vec!["orchestration:read".into()];
            store.write(&credential).unwrap();
        }
        let response = fixture.call("t3_thread_send", json!("caller-policy")).await;
        assert_eq!(response["id"], "caller-policy");
        assert!(response.get("error").is_none());
        assert_eq!(response["result"]["isError"], true);
        assert_eq!(
            response["result"]["structuredContent"]["error"]["code"],
            if read_only {
                "gateway_read_only"
            } else {
                "t3_scope_required"
            }
        );
        assert_eq!(fixture.calls(), 0);
    }
}

#[tokio::test]
async fn malformed_native_results_and_error_envelopes_are_rejected_without_mutation_replay() {
    for frame in [
        json!({}),
        json!({"result":null}),
        json!({"result":[]}),
        json!({"result":{}}),
        json!({"result":{"isError":true}}),
        json!({"result":{"content":null}}),
        json!({"result":{"content":[],"isError":"true"}}),
        json!({"result":{"content":[],"structuredContent":null}}),
        json!({"result":{"content":[],"_meta":[]}}),
        json!({"result":{"content":[{"type":"text","text":17}]}}),
        json!({"result":{"content":[{"type":"image","data":"AA=="}]}}),
        json!({"result":{"content":[{"type":"audio","data":17,"mimeType":"audio/wav"}]}}),
        json!({"result":{"content":[{"type":"resource_link","name":"missing-uri"}]}}),
        json!({"result":{"content":[{"type":"resource","resource":{"uri":"file:///missing"}}]}}),
        json!({"result":{"content":[{"type":"text","text":"ok","annotations":{"priority":2}}]}}),
        json!({"result":{"content":[{"type":"text","text":"ok","annotations":{"audience":["invalid"]}}]}}),
        json!({"result":{"content":[{"type":"text","text":"ok","annotations":{"lastModified":"invalid"}}]}}),
        json!({"result":{"content":[{"type":"resource","resource":{"uri":"file:///invalid","text":"ok","_meta":null}}]}}),
        json!({"result":{"content":[{"type":"text","text":"ok","_meta":null}]}}),
        json!({"result":{"content":[{"type":"unknown"}]}}),
        json!({"result":{"content":[]},"error":{"code":-32602,"message":"Ambiguous"}}),
        json!({"error":{"code":"-32602","message":"Invalid code"}}),
        json!({"error":{"code":-32602,"message":17}}),
        json!({"error":null}),
    ] {
        let fixture = NativeFixture::new(Reply::json(frame.clone())).await;
        let response = fixture
            .call("t3_thread_send", json!("caller-malformed"))
            .await;
        assert_eq!(response["id"], "caller-malformed");
        assert!(
            response.get("error").is_none(),
            "{frame} became a protocol error"
        );
        assert_eq!(response["result"]["isError"], true, "{frame} was accepted");
        assert_eq!(
            response["result"]["structuredContent"]["error"]["code"],
            "native_outcome_unknown"
        );
        assert_eq!(fixture.calls(), 1);
    }
}

#[tokio::test]
async fn local_transport_failures_keep_mutation_safety_and_read_retries() {
    for (name, code, attempts) in [
        ("t3_thread_send", "native_outcome_unknown", 1),
        ("t3_thread_list", "native_unavailable", 2),
    ] {
        for reply in [
            Reply {
                drop_response: true,
                ..Reply::json(Value::Null)
            },
            Reply::status(
                500,
                json!({"error":{"code":-32000,"message":"HTTP failure"}}),
            ),
        ] {
            let fixture = NativeFixture::new(reply).await;
            let response = fixture.call(name, json!("caller-transport")).await;
            assert_eq!(response["id"], "caller-transport");
            assert!(response.get("error").is_none());
            assert_eq!(
                response["result"]["structuredContent"]["error"]["code"],
                code
            );
            assert_eq!(fixture.calls(), attempts);
        }
    }
}

#[tokio::test]
async fn local_close_is_distinct_from_an_upstream_minus_32000_error() {
    let fixture = NativeFixture::new(Reply::sse(vec![(
        b": delayed response\n\n".to_vec(),
        Duration::from_millis(500),
    )]))
    .await;
    let (result, ()) = tokio::join!(
        tokio::time::timeout(
            Duration::from_secs(5),
            fixture.gateway.native.call_tool(
                json!({"name":"t3_thread_send","arguments":{}})
                    .as_object()
                    .unwrap()
                    .clone(),
                true,
                None,
            )
        ),
        async {
            tokio::time::timeout(Duration::from_secs(2), async {
                while fixture.calls() == 0 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
            fixture.gateway.native.close();
        }
    );
    let error = result
        .expect("closing the active native client did not complete its call")
        .unwrap_err();
    assert_eq!(error.code, "native_outcome_unknown");
    assert!(error.protocol_error.is_none());
    assert_eq!(fixture.calls(), 1);
}

#[tokio::test]
async fn local_timeout_is_distinct_from_an_upstream_minus_32001_error() {
    for (name, code, attempts) in [
        ("t3_thread_send", "native_outcome_unknown", 1),
        ("t3_thread_list", "native_unavailable", 2),
    ] {
        let mut fixture = NativeFixture::new(Reply::sse(vec![(
            b": delayed response\n\n".to_vec(),
            Duration::from_millis(500),
        )]))
        .await;
        fixture.gateway.native = NativeMcpClient::new(
            format!("{}/mcp", fixture.upstream.url),
            NativeCredentialStore::in_data_dir(&fixture.directory),
            Some(Arc::clone(&fixture.gateway.audit)),
            Duration::from_millis(100),
        );
        let response = fixture.call(name, json!("caller-timeout")).await;
        assert_eq!(response["id"], "caller-timeout");
        assert!(response.get("error").is_none());
        assert_eq!(
            response["result"]["structuredContent"]["error"]["code"],
            code
        );
        assert!(
            response["result"]["structuredContent"]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Request timed out")
        );
        assert_eq!(fixture.calls(), attempts);
    }
}
