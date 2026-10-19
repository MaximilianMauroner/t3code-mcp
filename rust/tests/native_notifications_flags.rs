#![cfg(unix)]
#[path = "native_notifications_support/mod.rs"]
pub mod support;

use serde_json::{Value, json};
use std::{
    io::Write,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    process::Stdio,
};
use support::*;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    time::timeout,
};

fn flags(socket: &UnixStream) -> libc::c_int {
    let flags = unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    flags
}

async fn shared_stdout_flags(nonblocking: bool, broken_pipe: bool) {
    let fixture = Fixture::new().await;
    let (reader, retained_stdout) = UnixStream::pair().unwrap();
    let original = if nonblocking {
        flags(&retained_stdout) | libc::O_NONBLOCK
    } else {
        flags(&retained_stdout) & !libc::O_NONBLOCK
    };
    assert_eq!(
        unsafe { libc::fcntl(retained_stdout.as_raw_fd(), libc::F_SETFL, original) },
        0
    );
    reader.set_nonblocking(true).unwrap();
    let mut reader = BufReader::new(tokio::net::UnixStream::from_std(reader).unwrap());
    let stdout = OwnedFd::from(retained_stdout.try_clone().unwrap());
    let mut child = Process::command(&fixture, "stdio")
        .stdout(Stdio::from(stdout))
        .spawn()
        .unwrap();
    writeln!(child.stdin.as_mut().unwrap(), "{}", initialize()).unwrap();
    let mut line = String::new();
    assert!(
        timeout(WAIT, reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap()
            > 0
    );
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["id"], 1);
    assert!(response["result"]["capabilities"].is_object());
    assert_ne!(
        flags(&retained_stdout) & libc::O_NONBLOCK,
        0,
        "active stdio writer must remain nonblocking"
    );
    if broken_pipe {
        drop(reader);
        writeln!(
            child.stdin.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc":"2.0","id":2,"method":"ping"})
        )
        .unwrap();
    } else {
        child.stdin.take();
    }
    let status = timeout(WAIT, async {
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("stdio subprocess did not exit");
    assert_eq!(status.success(), !broken_pipe);
    assert_eq!(
        flags(&retained_stdout),
        original,
        "stdio changed the shared open file description after exit"
    );
}

#[tokio::test]
async fn eof_restores_original_blocking_stdout_flags() {
    shared_stdout_flags(false, false).await;
}

#[tokio::test]
async fn eof_preserves_original_nonblocking_stdout_flags() {
    shared_stdout_flags(true, false).await;
}

#[tokio::test]
async fn broken_pipe_restores_original_blocking_stdout_flags() {
    shared_stdout_flags(false, true).await;
}

#[tokio::test]
async fn broken_pipe_preserves_original_nonblocking_stdout_flags() {
    shared_stdout_flags(true, true).await;
}
