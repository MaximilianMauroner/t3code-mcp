#![cfg(unix)]
#[path = "native_notifications_support/mod.rs"]
pub mod support;

use serde_json::json;
use support::*;
use tokio::time::timeout;

async fn draining(fixture: &Fixture) {
    timeout(WAIT, async {
        loop {
            if fixture
                .audit()
                .iter()
                .any(|event| event["event"] == "stdio.session" && event["outcome"] == "draining")
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("stdio did not stop admission");
}
fn completed(fixture: &Fixture) -> usize {
    fixture
        .audit()
        .iter()
        .filter(|event| {
            event["event"] == "upstream.response"
                && event["operation"] == "mcp:t3_thread_send"
                && event["outcome"] == "completed"
        })
        .count()
}

#[tokio::test]
async fn eof_and_sigterm_drain_progress_bearing_mutations_and_native_listeners() {
    for signal in [false, true] {
        let mut fixture = Fixture::new().await;
        let mut process = Process::spawn(&fixture, "stdio");
        process.send(call(json!(11), json!(0.5), true));
        let native = receive(&mut fixture.native.calls).await;
        let upstream = receive(&mut fixture.native.opened).await;
        let progress = progress(json!(0.5), 1);
        native.events.send(progress.clone()).unwrap();
        assert_eq!(receive(&mut process.lines).await, progress);
        if signal {
            process.signal();
        } else {
            process.eof();
        }
        draining(&fixture).await;
        assert!(
            fixture.native.closed.try_recv().is_err(),
            "shutdown closed native listener before admitted work completed"
        );
        let final_result = result(&native);
        native.events.send(final_result.clone()).unwrap();
        let response = receive(&mut process.lines).await;
        assert_eq!(response["id"], 11);
        assert_eq!(response["result"], final_result["result"]);
        process.finish(true).await;
        assert_eq!(receive(&mut fixture.native.closed).await, upstream);
        assert_eq!(completed(&fixture), 1);
        assert!(fixture.native.calls.try_recv().is_err());
    }
}

#[tokio::test]
async fn stdout_loss_drains_the_admitted_mutation_without_replay() {
    let mut fixture = Fixture::new().await;
    let mut process = Process::spawn(&fixture, "stdio");
    process.send(call(json!(1), json!("token"), true));
    let native = receive(&mut fixture.native.calls).await;
    let upstream = receive(&mut fixture.native.opened).await;
    process.disconnect_output();
    native.events.send(progress(json!("token"), 1)).unwrap();
    process.output_closed().await;
    native.events.send(progress(json!("token"), 2)).unwrap();
    draining(&fixture).await;
    assert!(fixture.native.closed.try_recv().is_err());
    native.events.send(result(&native)).unwrap();
    process.finish(false).await;
    assert_eq!(receive(&mut fixture.native.closed).await, upstream);
    assert_eq!(completed(&fixture), 1);
    assert!(fixture.native.calls.try_recv().is_err());
}

#[tokio::test]
async fn concurrent_stdio_calls_keep_complete_frames_and_each_progress_order() {
    let mut fixture = Fixture::new().await;
    let mut process = Process::spawn(&fixture, "stdio");
    process.send(call(json!(1), json!("a"), false));
    let first = receive(&mut fixture.native.calls).await;
    process.send(call(json!(2), json!("b"), false));
    let second = receive(&mut fixture.native.calls).await;
    for count in 1..=32 {
        first.events.send(progress(json!("a"), count)).unwrap();
        second.events.send(progress(json!("b"), count)).unwrap();
    }
    let mut seen =
        std::collections::HashMap::from([("a".to_owned(), 0_u64), ("b".to_owned(), 0_u64)]);
    for _ in 0..64 {
        let event = receive(&mut process.lines).await;
        assert_eq!(event["method"], "notifications/progress");
        let token = event["params"]["progressToken"].as_str().unwrap();
        let count = seen.get_mut(token).unwrap();
        *count += 1;
        assert_eq!(event["params"]["progress"], *count);
    }
    first.events.send(result(&first)).unwrap();
    second.events.send(result(&second)).unwrap();
    let mut ids = vec![
        receive(&mut process.lines).await["id"].as_u64().unwrap(),
        receive(&mut process.lines).await["id"].as_u64().unwrap(),
    ];
    ids.sort_unstable();
    assert_eq!(ids, [1, 2]);
    process.eof();
    process.finish(true).await;
}
