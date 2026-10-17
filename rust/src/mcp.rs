use bytes::Bytes;
use http::{HeaderMap, Request};
use http_body::Body;
use http_body_util::{BodyExt, Full, Limited};
use serde_json::Value;

const LEGACY_PROTOCOL_VERSION: &str = "2024-10-07";
const RMCP_COMPAT_PROTOCOL_VERSION: &str = "2024-11-05";
const VERSION_META_KEY: &str = "io.modelcontextprotocol/protocolVersion";

pub const DEFAULT_HTTP_BODY_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpBodyError {
    Invalid,
}

pub async fn normalize_legacy_protocol_request<B>(
    request: Request<B>,
    max_body_bytes: usize,
) -> Result<Request<Full<Bytes>>, HttpBodyError>
where
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let (mut parts, body) = request.into_parts();
    let mut bytes = Limited::new(body, max_body_bytes)
        .collect()
        .await
        .map_err(|_| HttpBodyError::Invalid)?
        .to_bytes();

    if parts
        .headers
        .get("MCP-Protocol-Version")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|version| version == LEGACY_PROTOCOL_VERSION)
    {
        bytes = normalize_legacy_message(&mut parts.headers, &bytes);
    }

    Ok(Request::from_parts(parts, Full::new(bytes)))
}

fn normalize_legacy_message(headers: &mut HeaderMap, bytes: &[u8]) -> Bytes {
    let Ok(mut message) = serde_json::from_slice::<Value>(bytes) else {
        return Bytes::copy_from_slice(bytes);
    };
    let includes_initialize = match &message {
        Value::Array(messages) => messages.iter().any(is_initialize_message),
        message => is_initialize_message(message),
    };
    if includes_initialize {
        headers.remove("MCP-Protocol-Version");
        return Bytes::copy_from_slice(bytes);
    }

    headers.insert(
        "MCP-Protocol-Version",
        http::HeaderValue::from_static(RMCP_COMPAT_PROTOCOL_VERSION),
    );
    let mut modified = false;
    match &mut message {
        Value::Array(messages) => {
            for message in messages {
                modified |= normalize_meta_version(message);
            }
        }
        message => modified = normalize_meta_version(message),
    }
    if modified {
        serde_json::to_vec(&message)
            .map(Bytes::from)
            .unwrap_or_else(|_| Bytes::copy_from_slice(bytes))
    } else {
        Bytes::copy_from_slice(bytes)
    }
}

fn is_initialize_message(message: &Value) -> bool {
    message
        .get("method")
        .and_then(Value::as_str)
        .is_some_and(|method| method == "initialize")
}

fn normalize_meta_version(message: &mut Value) -> bool {
    let Some(message) = message.as_object_mut() else {
        return false;
    };
    let Some(params) = message.get_mut("params").and_then(Value::as_object_mut) else {
        return false;
    };
    let Some(meta) = params.get_mut("_meta").and_then(Value::as_object_mut) else {
        return false;
    };
    if meta.get(VERSION_META_KEY).and_then(Value::as_str) != Some(LEGACY_PROTOCOL_VERSION) {
        return false;
    }
    meta.insert(
        VERSION_META_KEY.to_owned(),
        Value::String(RMCP_COMPAT_PROTOCOL_VERSION.to_owned()),
    );
    true
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use bytes::Bytes;
    use http::{Request, Response, header};
    use http_body_util::{BodyExt, Full, combinators::BoxBody};
    use rmcp::{
        ServerHandler,
        model::{Implementation, ProtocolVersion, ServerCapabilities, ServerConfig},
        transport::streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
        },
    };
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::{
        DEFAULT_HTTP_BODY_LIMIT, LEGACY_PROTOCOL_VERSION, normalize_legacy_protocol_request,
    };

    type ResponseBody = BoxBody<Bytes, std::convert::Infallible>;

    #[derive(Clone)]
    struct ProtocolProbe;

    impl ServerHandler for ProtocolProbe {
        fn get_info(&self) -> ServerConfig {
            ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
                .with_server_info(Implementation::new("t3-code-mcp", "0.1.0"))
                .with_protocol_version(protocol_version(LEGACY_PROTOCOL_VERSION))
                .with_instructions("compatibility probe")
        }

        fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
            Cow::Owned(vec![protocol_version(LEGACY_PROTOCOL_VERSION)])
        }
    }

    fn protocol_version(value: &str) -> ProtocolVersion {
        serde_json::from_value(Value::String(value.to_owned())).expect("version is a string")
    }

    fn service() -> StreamableHttpService<ProtocolProbe, NeverSessionManager> {
        let mut config = StreamableHttpServerConfig::default();
        config.legacy_session_mode = false;
        config.json_response = true;
        config.allowed_hosts = vec!["localhost".to_owned()];
        StreamableHttpService::new(
            || Ok(ProtocolProbe),
            std::sync::Arc::new(NeverSessionManager::default()),
            config,
        )
    }

    async fn post(
        service: &StreamableHttpService<ProtocolProbe, NeverSessionManager>,
        body: Value,
        version: Option<&str>,
    ) -> Response<ResponseBody> {
        let mut request = Request::builder()
            .method("POST")
            .uri("http://localhost/mcp")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream");
        if let Some(version) = version {
            request = request.header("MCP-Protocol-Version", version);
        }
        let request = request
            .body(Full::new(Bytes::from(body.to_string())))
            .expect("request");
        let request = normalize_legacy_protocol_request(request, DEFAULT_HTTP_BODY_LIMIT)
            .await
            .expect("small request body");
        service
            .clone()
            .oneshot(request)
            .await
            .expect("HTTP service is infallible")
    }

    #[tokio::test]
    async fn legacy_http_version_adapter_preserves_initialize_and_follow_up_requests() {
        let service = service();
        let initialize = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": LEGACY_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "compat-test", "version": "1"}
            }
        });
        let initialized = post(&service, initialize, None).await;
        assert_eq!(initialized.status(), http::StatusCode::OK);
        let body = initialized
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let body: Value = serde_json::from_slice(&body).expect("JSON response");
        assert_eq!(body["result"]["protocolVersion"], LEGACY_PROTOCOL_VERSION);

        let notification = post(
            &service,
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            Some(LEGACY_PROTOCOL_VERSION),
        )
        .await;
        assert_eq!(notification.status(), http::StatusCode::ACCEPTED);

        let tools = post(
            &service,
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
            Some(LEGACY_PROTOCOL_VERSION),
        )
        .await;
        assert_eq!(tools.status(), http::StatusCode::OK);
        assert!(tools.headers().get("Mcp-Session-Id").is_none());
        assert!(tools.headers().get("MCP-Protocol-Version").is_none());
    }
}
