use super::http_client::{ClientError, ErrorKind};
use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use tokio_util::sync::CancellationToken;

pub async fn request_t3_rpc<T: DeserializeOwned>(
    base_url: &str,
    access_token: &str,
    timeout: Duration,
    method: &str,
    payload: Value,
    cancel: Option<&CancellationToken>,
) -> Result<T, Box<ClientError>> {
    let execute = async {
        let mut url = url::Url::parse(&format!("{}/ws", base_url.trim_end_matches('/')))
            .map_err(|e| ClientError::boxed(ErrorKind::Rpc, e.to_string()))?;
        url.set_scheme(if url.scheme() == "https" { "wss" } else { "ws" })
            .map_err(|_| ClientError::boxed(ErrorKind::Rpc, "Invalid WebSocket URL."))?;
        url.query_pairs_mut()
            .append_pair("orchestrationProtocol", "2");
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|e| ClientError::boxed(ErrorKind::Rpc, e.to_string()))?;
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {access_token}")
                .parse()
                .map_err(|_| ClientError::boxed(ErrorKind::Rpc, "Invalid authorization header."))?,
        );
        let (mut socket, _) = connect_async(request).await.map_err(|e| match e {
            tokio_tungstenite::tungstenite::Error::Http(response) => ClientError::boxed(
                ErrorKind::Rpc,
                format!(
                    "T3 RPC upgrade failed with HTTP {}.",
                    response.status().as_u16()
                ),
            ),
            _ => ClientError::boxed(
                ErrorKind::Rpc,
                format!("T3 RPC {method} connection failed."),
            ),
        })?;
        let id = uuid::Uuid::new_v4().to_string();
        socket
            .send(Message::Text(
                json!({"_tag":"Request","id":id,"tag":method,"payload":payload,"headers":[]})
                    .to_string()
                    .into(),
            ))
            .await
            .map_err(|_| {
                ClientError::boxed(
                    ErrorKind::Rpc,
                    format!("T3 RPC {method} connection failed."),
                )
            })?;
        while let Some(message) = socket.next().await {
            let data = match message {
                Ok(Message::Text(t)) => t.to_string(),
                Ok(Message::Binary(b)) => String::from_utf8_lossy(&b).into_owned(),
                Ok(Message::Close(_)) => break,
                Ok(_) => continue,
                Err(_) => {
                    return Err(ClientError::boxed(
                        ErrorKind::Rpc,
                        format!("T3 RPC {method} connection failed."),
                    ));
                }
            };
            let invalid = || {
                ClientError::boxed(
                    ErrorKind::Rpc,
                    format!("T3 RPC {method} returned an invalid response."),
                )
            };
            let frame: Value = serde_json::from_str(&data).map_err(|_| invalid())?;
            let frames = if let Value::Array(v) = frame {
                v
            } else {
                vec![frame]
            };
            for frame in frames {
                let tag = frame["_tag"].as_str().ok_or_else(invalid)?;
                if matches!(tag, "Defect" | "ClientProtocolError") {
                    return Err(ClientError::boxed(
                        ErrorKind::Rpc,
                        format!("T3 RPC {method} returned a protocol error."),
                    ));
                }
                if frame
                    .get("requestId")
                    .is_some_and(|v| !v.is_string() && !v.is_number())
                {
                    return Err(invalid());
                }
                if tag != "Exit" || frame["requestId"] != id || frame.get("exit").is_none() {
                    continue;
                }
                match frame["exit"]["_tag"].as_str() {
                    Some("Success") => {
                        return serde_json::from_value(frame["exit"]["value"].clone())
                            .map_err(|_| invalid());
                    }
                    Some("Failure") => {
                        let causes = frame["exit"]["cause"].as_array().ok_or_else(invalid)?;
                        if causes.iter().any(|c| !c["_tag"].is_string()) {
                            return Err(invalid());
                        }
                        let detail = causes
                            .iter()
                            .find(|c| c["_tag"] == "Fail")
                            .and_then(|c| c["error"]["message"].as_str())
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("T3 RPC {method} failed."));
                        return Err(ClientError::boxed(ErrorKind::Rpc, detail));
                    }
                    _ => return Err(invalid()),
                }
            }
        }
        Err(ClientError::boxed(
            ErrorKind::Rpc,
            format!("T3 RPC {method} disconnected before acknowledgement."),
        ))
    };
    let cancelled = async {
        match cancel {
            Some(c) => c.cancelled().await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {biased; _=cancelled=>Err(ClientError::boxed(ErrorKind::Cancelled,"The operation was aborted.")),result=tokio::time::timeout(timeout,execute)=>result.unwrap_or_else(|_|Err(ClientError::boxed(ErrorKind::Timeout,"The operation was aborted due to timeout")))}
}
