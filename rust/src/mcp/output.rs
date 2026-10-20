use std::{
    io,
    sync::{Arc, Mutex, Weak},
};

use bytes::Bytes;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const QUEUE_CAPACITY: usize = 128;
const QUEUE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct Delivery(Arc<Mutex<State>>, CancellationToken);

struct State {
    sender: Option<mpsc::Sender<Bytes>>,
    queued_bytes: usize,
}

pub(crate) struct Output {
    receiver: mpsc::Receiver<Bytes>,
    state: Weak<Mutex<State>>,
    failed: CancellationToken,
    ended: bool,
}

impl Delivery {
    pub(crate) fn channel() -> (Self, Output) {
        let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
        let delivery = Self(
            Arc::new(Mutex::new(State {
                sender: Some(sender),
                queued_bytes: 0,
            })),
            CancellationToken::new(),
        );
        let output = Output {
            receiver,
            state: Arc::downgrade(&delivery.0),
            failed: delivery.1.clone(),
            ended: false,
        };
        (delivery, output)
    }

    pub(crate) fn send(&self, message: &impl Serialize) -> bool {
        self.send_bytes(Bytes::from(
            serde_json::to_vec(message).expect("JSON output"),
        ))
    }

    pub(crate) fn send_bytes(&self, bytes: Bytes) -> bool {
        let mut state = self.0.lock().unwrap();
        let Some(sender) = state.sender.as_ref() else {
            return false;
        };
        if bytes.len() > QUEUE_BYTES.saturating_sub(state.queued_bytes)
            || sender.try_send(bytes.clone()).is_err()
        {
            state.sender = None;
            self.1.cancel();
            false
        } else {
            state.queued_bytes += bytes.len();
            true
        }
    }

    pub(crate) fn finish(&self) {
        self.0.lock().unwrap().sender = None;
    }

    pub(crate) fn fail(&self) {
        self.finish();
        self.1.cancel();
    }

    pub(crate) async fn failed(&self) {
        self.1.cancelled().await;
    }
}

impl Output {
    pub(crate) async fn recv(&mut self) -> Option<io::Result<Bytes>> {
        if self.ended {
            return None;
        }
        tokio::select! {
            biased;
            _ = self.failed.cancelled() => {
                self.ended = true;
                Some(Err(io::Error::new(io::ErrorKind::BrokenPipe, "MCP output closed or exceeded its bounded queue.")))
            }
            bytes = self.receiver.recv() => {
                if let Some(bytes) = &bytes && let Some(state) = self.state.upgrade() {
                    let mut state = state.lock().unwrap();
                    state.queued_bytes = state.queued_bytes.saturating_sub(bytes.len());
                }
                bytes.map(Ok)
            }
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            state.lock().unwrap().sender = None;
        }
        self.failed.cancel();
    }
}
