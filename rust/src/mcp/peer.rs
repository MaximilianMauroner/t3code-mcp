use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock, Weak},
    time::{Duration, Instant},
};

use serde_json::{Map, Value};
use tokio::sync::Notify;

use super::{
    RequestId, RpcError,
    output::{Delivery, Output},
};
use crate::t3::NativeMcpClient;

#[derive(Clone)]
pub struct Peer(Arc<PeerState>);

struct PeerState {
    admission: Mutex<Admission>,
    native: NativeState,
    drained: Notify,
    notifications: Mutex<Notifications>,
}

enum NativeState {
    Borrowed,
    Owned(OnceLock<NativeMcpClient>),
}

#[derive(Default)]
struct Notifications {
    output: Option<Delivery>,
    pending: Option<Value>,
    shared: bool,
}

struct Admission {
    closed: bool,
    active: HashMap<RequestId, Weak<InvocationState>>,
}

impl Default for Peer {
    fn default() -> Self {
        Self::new(NativeState::Owned(OnceLock::new()))
    }
}

impl Peer {
    pub(crate) fn borrowing_native() -> Self {
        Self::new(NativeState::Borrowed)
    }

    fn new(native: NativeState) -> Self {
        Self(Arc::new(PeerState {
            admission: Mutex::new(Admission {
                closed: false,
                active: HashMap::new(),
            }),
            native,
            drained: Notify::new(),
            notifications: Mutex::new(Notifications::default()),
        }))
    }

    pub(crate) fn reserve(&self, id: RequestId) -> Result<Invocation, RpcError> {
        let mut admission = self.0.admission.lock().unwrap();
        if admission.closed {
            return Err(RpcError::new(-32000, "MCP session is closed."));
        }
        if admission
            .active
            .get(&id)
            .is_some_and(|entry| entry.strong_count() > 0)
        {
            return Err(RpcError::new(-32600, "Duplicate active request ID."));
        }
        let invocation = Arc::new(InvocationState {
            peer: self.clone(),
            id: id.clone(),
            cancellation: Mutex::new(None),
            cancelled: Notify::new(),
            output: OnceLock::new(),
        });
        admission.active.insert(id, Arc::downgrade(&invocation));
        Ok(Invocation(invocation))
    }

    pub(crate) fn cancel(&self, params: Map<String, Value>) {
        let Some(id) = params.get("requestId").and_then(RequestId::parse) else {
            return;
        };
        let invocation = self
            .0
            .admission
            .lock()
            .unwrap()
            .active
            .get(&id)
            .and_then(Weak::upgrade);
        if let Some(invocation) = invocation {
            let mut cancellation = invocation.cancellation.lock().unwrap();
            if cancellation.is_none() {
                *cancellation = Some(params);
                invocation.cancelled.notify_waiters();
            }
        }
    }

    pub fn native<'a>(&'a self, source: &'a NativeMcpClient) -> &'a NativeMcpClient {
        let NativeState::Owned(native) = &self.0.native else {
            return source;
        };
        native.get_or_init(|| {
            let client = source.fork();
            let peer = Arc::downgrade(&self.0);
            client.observe_notifications(Arc::new(move |notification| {
                if let Some(peer) = peer.upgrade() {
                    let admission = peer.admission.lock().unwrap();
                    if admission.closed {
                        return;
                    }
                    let mut notifications = peer.notifications.lock().unwrap();
                    let delivered = notifications
                        .output
                        .as_ref()
                        .is_some_and(|output| output.send(&notification));
                    if !delivered {
                        notifications.output = None;
                        notifications.pending = Some(notification);
                    }
                }
            }));
            client
        })
    }

    pub(crate) fn subscribe(&self) -> Output {
        let admission = self.0.admission.lock().unwrap();
        let (delivery, output) = Delivery::channel();
        let mut notifications = self.0.notifications.lock().unwrap();
        notifications.shared = false;
        if let Some(previous) = notifications.output.replace(delivery.clone()) {
            previous.finish();
        }
        if admission.closed {
            delivery.finish();
        } else if let Some(pending) = notifications.pending.take() {
            delivery.send(&pending);
        }
        output
    }

    pub(crate) fn set_notification_output(&self, delivery: Delivery) {
        let mut notifications = self.0.notifications.lock().unwrap();
        notifications.shared = true;
        notifications.output = Some(delivery);
    }

    pub fn close(&self) {
        self.stop(true);
    }

    pub(crate) fn shutdown(&self) {
        self.stop(false);
    }

    fn stop(&self, close_calls: bool) {
        let active = {
            let mut admission = self.0.admission.lock().unwrap();
            admission.closed = true;
            admission
                .active
                .values()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>()
        };
        let mut notifications = self.0.notifications.lock().unwrap();
        let shared = notifications.shared;
        if let Some(output) = notifications.output.take()
            && !shared
        {
            output.finish();
        }
        notifications.pending = None;
        drop(notifications);
        if close_calls && !shared {
            for invocation in &active {
                if let Some(output) = invocation.output.get() {
                    output.finish();
                }
            }
        }
        drop(active);
        self.close_native_if_idle();
    }

    fn close_native_if_idle(&self) {
        let admission = self.0.admission.lock().unwrap();
        if admission.closed
            && admission
                .active
                .values()
                .all(|entry| entry.strong_count() == 0)
            && let NativeState::Owned(native) = &self.0.native
            && let Some(native) = native.get()
        {
            native.close();
        }
    }

    pub async fn drain(&self) {
        loop {
            let changed = self.0.drained.notified();
            if self.idle() {
                return;
            }
            changed.await;
        }
    }

    fn idle(&self) -> bool {
        self.0
            .admission
            .lock()
            .unwrap()
            .active
            .values()
            .all(|entry| entry.strong_count() == 0)
    }
}

#[derive(Clone)]
pub struct Invocation(Arc<InvocationState>);

struct InvocationState {
    peer: Peer,
    id: RequestId,
    cancellation: Mutex<Option<Map<String, Value>>>,
    cancelled: Notify,
    output: OnceLock<Delivery>,
}

impl std::fmt::Debug for Invocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Invocation")
            .field("id", &self.0.id)
            .finish()
    }
}

impl Invocation {
    pub(crate) fn set_output(&self, output: Delivery) {
        let admission = self.0.peer.0.admission.lock().unwrap();
        if admission.closed && !self.0.peer.0.notifications.lock().unwrap().shared {
            output.finish();
        }
        let _ = self.0.output.set(output);
    }

    pub(crate) fn output(&self) -> Option<Delivery> {
        self.0.output.get().cloned()
    }

    pub fn peer(&self) -> &Peer {
        &self.0.peer
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancellation.lock().unwrap().is_some()
    }

    pub(crate) fn admit_send<T>(&self, send: impl FnOnce() -> T) -> Option<T> {
        let cancellation = self.0.cancellation.lock().unwrap();
        if cancellation.is_some() {
            None
        } else {
            Some(send())
        }
    }

    pub async fn cancellation(&self) -> Map<String, Value> {
        loop {
            let changed = self.0.cancelled.notified();
            if let Some(params) = self.0.cancellation.lock().unwrap().clone() {
                return params;
            }
            changed.await;
        }
    }
}

impl Drop for InvocationState {
    fn drop(&mut self) {
        let mut admission = self.peer.0.admission.lock().unwrap();
        if admission
            .active
            .get(&self.id)
            .is_some_and(|entry| std::ptr::eq(entry.as_ptr(), self))
        {
            admission.active.remove(&self.id);
        }
        self.peer.0.drained.notify_waiters();
        drop(admission);
        self.peer.close_native_if_idle();
    }
}

#[derive(Default)]
pub(super) struct Sessions {
    entries: HashMap<String, HttpSession>,
    retired: Vec<Peer>,
    closed: bool,
}

struct HttpSession {
    peer: Peer,
    auth: String,
    touched: Instant,
}

impl Sessions {
    pub(super) fn is_closed(&self) -> bool {
        self.closed
    }

    pub(super) fn expire(&mut self) {
        self.retired.retain(|peer| !peer.idle());
        self.entries.retain(|_, session| {
            let keep =
                !session.peer.idle() || session.touched.elapsed() < Duration::from_secs(1800);
            if !keep {
                session.peer.close();
            }
            keep
        });
    }

    pub(super) fn create(&mut self, auth: &str) -> Option<(String, Peer)> {
        self.expire();
        if self.closed || self.entries.len() + self.retired.len() >= 1024 {
            return None;
        }
        let id = uuid::Uuid::new_v4().to_string();
        let peer = Peer::default();
        self.entries.insert(
            id.clone(),
            HttpSession {
                peer: peer.clone(),
                auth: auth.to_owned(),
                touched: Instant::now(),
            },
        );
        Some((id, peer))
    }

    pub(super) fn get(&mut self, id: &str, auth: &str) -> Option<Peer> {
        self.expire();
        let session = self
            .entries
            .get_mut(id)
            .filter(|session| session.auth == auth)?;
        session.touched = Instant::now();
        Some(session.peer.clone())
    }

    pub(super) fn delete(&mut self, id: &str) {
        if let Some(session) = self.entries.remove(id) {
            session.peer.close();
            if !session.peer.idle() {
                self.retired.push(session.peer);
            }
        }
    }

    pub(super) fn close(&mut self) {
        self.closed = true;
        for session in self.entries.values() {
            session.peer.shutdown();
        }
    }
}

#[cfg(test)]
mod expiry_tests {
    use super::*;

    #[tokio::test]
    async fn expired_http_session_ends_its_get_body_and_rejects_later_requests() {
        use crate::mcp::{HttpOptions, McpServer, ToolCall, ToolFailure, ToolHandler};
        use bytes::Bytes;
        use http_body_util::BodyExt;

        struct Handler;
        impl ToolHandler for Handler {
            async fn call_tool(
                &self,
                _: ToolCall,
                _: Option<Invocation>,
            ) -> Result<crate::mcp::ToolResult, ToolFailure> {
                unreachable!("expiry does not invoke a tool")
            }
        }
        let server = McpServer::gateway();
        let options = HttpOptions {
            bearer_token: Some("owner".into()),
            ..Default::default()
        };
        let initialize = ::http::Request::builder().method("POST").uri("/mcp")
            .header("authorization", "Bearer owner").header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .body(Bytes::from(serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"expiry","version":"1"}
            }}).to_string())).unwrap();
        let initialized = server.handle_http(initialize, &options, &Handler).await;
        let id = initialized.headers()["mcp-session-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let request = |method: &str| {
            ::http::Request::builder()
                .method(method)
                .uri("/mcp")
                .header("authorization", "Bearer owner")
                .header("accept", "text/event-stream")
                .header("mcp-session-id", id.as_str())
                .body(Bytes::new())
                .unwrap()
        };
        let mut response = server
            .handle_http_stream(request("GET"), &options, Arc::new(Handler))
            .await;
        assert_eq!(response.status(), 200);
        server
            .sessions
            .lock()
            .unwrap()
            .entries
            .get_mut(&id)
            .unwrap()
            .touched = Instant::now() - Duration::from_secs(31 * 60);
        server.expire_sessions();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), response.body_mut().frame())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            server
                .handle_http(request("GET"), &options, &Handler)
                .await
                .status(),
            404
        );
        server.close();
    }

    #[test]
    fn idle_expiry_closes_admission_and_recovers_capacity() {
        let mut sessions = Sessions::default();
        let (handle, peer) = sessions.create("owner").unwrap();
        let (other_handle, _) = sessions.create("owner").unwrap();
        for _ in 2..1024 {
            sessions.create("owner").unwrap();
        }
        assert!(sessions.create("owner").is_none());
        drop(peer.reserve(RequestId::Number(1)).unwrap());

        sessions.entries.get_mut(&handle).unwrap().touched =
            Instant::now() - Duration::from_secs(29 * 60);
        sessions.expire();
        assert!(sessions.get(&handle, "owner").is_some());
        assert!(sessions.create("owner").is_none());

        sessions.entries.get_mut(&handle).unwrap().touched =
            Instant::now() - Duration::from_secs(31 * 60);
        sessions.expire();
        assert!(sessions.get(&handle, "owner").is_none());
        assert_eq!(peer.reserve(RequestId::Number(2)).unwrap_err().code, -32000);
        assert!(sessions.get(&other_handle, "owner").is_some());
        let (replacement, replacement_peer) = sessions.create("owner").unwrap();
        assert_ne!(replacement, handle);
        assert!(sessions.get(&replacement, "owner").is_some());
        drop(replacement_peer.reserve(RequestId::Number(2)).unwrap());
        assert!(sessions.create("owner").is_none());
    }

    #[test]
    fn active_invocations_survive_expiry_until_the_last_owner_finishes() {
        let mut sessions = Sessions::default();
        let (handle, peer) = sessions.create("owner").unwrap();
        for _ in 1..1024 {
            sessions.create("owner").unwrap();
        }
        let invocation = peer.reserve(RequestId::Number(7)).unwrap();
        let worker = invocation.clone();
        let expired_at = Instant::now() - Duration::from_secs(31 * 60);
        sessions.entries.get_mut(&handle).unwrap().touched = expired_at;

        sessions.expire();
        assert!(sessions.get(&handle, "owner").is_some());
        assert_eq!(peer.reserve(RequestId::Number(7)).unwrap_err().code, -32600);
        drop(peer.reserve(RequestId::Number(8)).unwrap());
        assert!(sessions.create("owner").is_none());

        sessions.entries.get_mut(&handle).unwrap().touched = expired_at;
        drop(invocation);
        assert!(sessions.get(&handle, "owner").is_some());
        assert_eq!(peer.reserve(RequestId::Number(7)).unwrap_err().code, -32600);
        assert!(sessions.create("owner").is_none());

        sessions.entries.get_mut(&handle).unwrap().touched = expired_at;
        drop(worker);
        let (replacement, replacement_peer) = sessions.create("owner").unwrap();
        assert_ne!(replacement, handle);
        assert!(sessions.get(&handle, "owner").is_none());
        assert_eq!(peer.reserve(RequestId::Number(9)).unwrap_err().code, -32000);
        drop(replacement_peer.reserve(RequestId::Number(7)).unwrap());
        assert!(sessions.create("owner").is_none());
    }
}
