use super::super::test_support::{LogFile, baseline, baseline_rows, event};
use super::*;
use std::fs::{self, OpenOptions};
use std::io::Write;

#[test]
fn node_usage_fixture_matches_report_values_and_serialized_field_order() {
    let file = LogFile::new();
    file.write(&baseline_rows());
    let mut report = summarize_usage(&file.path, &UsageWindow::default()).unwrap();
    report["window"]["firstEventAt"] = json!("<ISO_TIME>");
    report["window"]["lastEventAt"] = json!("<ISO_TIME>");
    report["scan"]["bytes"] = json!("<NUMBER>");
    for section in ["tools", "endpoints"] {
        for entry in report[section].as_object_mut().unwrap().values_mut() {
            for (key, value) in entry["latency"].as_object_mut().unwrap() {
                if key != "count" && value.is_number() {
                    *value = json!("<NUMBER>");
                }
            }
        }
    }
    let fixture = baseline();
    let expected = fixture["cli"]["usageSummary"]["stdout"].as_str().unwrap();
    assert_eq!(
        format!("{}\n", serde_json::to_string_pretty(&report).unwrap()),
        expected
    );
}

#[test]
fn summary_counts_dispatches_errors_observations_and_attribution() {
    let file = LogFile::new();
    let thread = json!({"id":"thread", "activity":"idle", "latestTurn":{"turnId":"turn"}, "updatedAt":"unchanged"});
    file.write(&[
        event(0, json!({"operation":"t3_task_start", "details":{"arguments":{}}})),
        event(1, json!({"details":{"arguments":{"modelSelection":{"model":"chosen"}}}})),
        event(2, json!({"event":"tool.result", "outcome":"error", "durationMs":12, "details":{"errorCode":"thread_busy"}})),
        event(3, json!({"operation":"t3_thread_get", "event":"tool.result", "outcome":"completed", "details":{"result":{"thread":thread}}})),
        event(4, json!({"operation":"t3_thread_get", "event":"tool.result", "outcome":"completed", "details":{"result":{"thread":thread}}})),
        event(5, json!({"operation":"t3_run_wait", "event":"tool.result", "outcome":"completed", "durationMs":30002,
            "details":{"result":{"timedOut":true, "runStatus":"running", "monitoring":{"observations":18}}}})),
        event(6, json!({"source":"t3", "event":"upstream.request", "operation":"GET /api/orchestration/threads/thread", "parentCorrelationId":"mcp-call", "parentOperation":"t3_run_wait"})),
        event(7, json!({"source":"t3", "event":"upstream.response", "operation":"GET /api/orchestration/threads/thread", "durationMs":33, "outcome":"completed"})),
        event(8, json!({"source":"t3", "event":"upstream.request", "operation":"GET /.well-known/t3/environment"})),
    ]);
    OpenOptions::new()
        .append(true)
        .open(&file.path)
        .unwrap()
        .write_all(b"invalid\n")
        .unwrap();
    let report = summarize_usage(
        &file.path,
        &UsageWindow {
            since: Some("2026-09-30T12:00:00Z".into()),
            until: Some("2026-09-30T12:00:08Z".into()),
        },
    )
    .unwrap();
    assert_eq!(report["scan"]["includedEvents"], 9);
    assert_eq!(report["scan"]["invalidLines"], 1);
    assert_eq!(
        report["denominators"],
        json!({"toolCalls":2,"dispatchCalls":2,"modelSelectionCalls":2,"upstreamRequests":2,"waits":1,"comparableThreadPairs":1})
    );
    assert_eq!(report["launches"]["omittedModelSelection"], 1);
    assert_eq!(report["monitoring"]["unchangedActiveWaits"], 1);
    assert_eq!(report["monitoring"]["measuredWaits"], 1);
    assert_eq!(report["monitoring"]["waitObservations"], 18);
    assert_eq!(report["monitoring"]["unchangedThreadPairs"], 1);
    assert_eq!(
        report["attribution"],
        json!({"attributedRequests":1,"unattributedRequests":1,"requestsByTool":{"t3_run_wait":1}})
    );
    assert_eq!(
        report["errorCodes"],
        json!({"t3_thread_send:thread_busy":1})
    );
    assert_eq!(
        report["endpoints"]["GET /api/orchestration/threads/:threadId"]["latency"],
        json!({"count":1,"meanMs":33,"minMs":33,"maxMs":33,"medianUpperBoundMs":50,"p95UpperBoundMs":50})
    );
    assert!(!report.to_string().contains("\"id\":\"thread\""));
}

#[test]
fn native_send_model_rules_and_native_and_legacy_waits_match_node() {
    let file = LogFile::new();
    let mut rows = vec![
        event(0, json!({"details":{"arguments":{},"native":true}})),
        event(1, json!({"details":{"arguments":{"modelSelection":null}}})),
        event(
            2,
            json!({"operation":"t3_thread_launch", "details":{"arguments":{},"native":true}}),
        ),
        event(
            3,
            json!({"operation":"t3_thread_create", "details":{"arguments":{"modelSelection":false}}}),
        ),
    ];
    for (i, status) in [
        "preparing",
        "queued",
        "starting",
        "running",
        "waiting",
        "idle",
        "completed",
    ]
    .iter()
    .enumerate()
    {
        rows.push(event(i + 4, json!({"operation":"t3_thread_wait", "event":"tool.result", "outcome":"completed", "details":{"result":{"timedOut":true,"status":status}}})));
    }
    rows.push(event(11, json!({"operation":"t3_thread_wait", "event":"tool.result", "outcome":"error", "details":{"result":{"timedOut":true,"status":"running"}}})));
    rows.push(event(12, json!({"operation":"t3_run_wait", "event":"tool.result", "outcome":"completed", "details":{"result":{"timedOut":true,"runStatus":"accepted", "monitoring":{"observations":2.5}}}})));
    file.write(&rows);
    let report = summarize_usage(&file.path, &UsageWindow::default()).unwrap();
    assert_eq!(report["denominators"]["toolCalls"], 4);
    assert_eq!(report["denominators"]["dispatchCalls"], 4);
    assert_eq!(report["denominators"]["modelSelectionCalls"], 3);
    assert_eq!(report["launches"]["omittedModelSelection"], 2);
    assert_eq!(report["denominators"]["waits"], 8);
    assert_eq!(report["monitoring"]["unchangedActiveWaits"], 6);
    assert_eq!(report["monitoring"]["waitObservations"], 2.5);
}

#[test]
fn latency_histogram_includes_edges_ignores_negative_and_keeps_empty_nulls() {
    let file = LogFile::new();
    let mut rows = vec![event(0, json!({"operation":"empty"}))];
    for (id, duration) in [
        0.0, 1.0, 1.5, 5.0, 10.0, 50.0, 100.0, 500.0, 1000.0, 5000.0, 10000.0, 30000.0, 60000.0,
        60001.0, -1.0,
    ]
    .iter()
    .enumerate()
    {
        rows.push(event(
            id + 1,
            json!({"operation":"measure", "event":"tool.result", "durationMs":duration}),
        ));
    }
    file.write(&rows);
    let report = summarize_usage(&file.path, &UsageWindow::default()).unwrap();
    assert_eq!(
        report["tools"]["empty"]["latency"],
        json!({"count":0,"meanMs":null,"minMs":null,"maxMs":null,"medianUpperBoundMs":null,"p95UpperBoundMs":null})
    );
    let latency = &report["tools"]["measure"]["latency"];
    assert_eq!(latency["count"], 14);
    assert_eq!(latency["minMs"], 0);
    assert_eq!(latency["maxMs"], 60001);
    assert_eq!(latency["medianUpperBoundMs"], 100);
    assert_eq!(latency["p95UpperBoundMs"], Value::Null);
    assert_eq!(latency["meanMs"].as_f64().unwrap(), 166668.5 / 14.0);
}

#[test]
fn thread_signatures_keep_selected_state_and_object_order_and_bridge_failed_reads() {
    let file = LogFile::new();
    let mut rows = vec![];
    let snapshots = [
        json!({"id":"thread", "latestTurn":{"a":1,"b":2}, "ignored":"first"}),
        json!({"id":"thread", "latestTurn":{"a":1,"b":2}, "ignored":"changed"}),
        json!({"id":"thread", "latestTurn":{"b":2,"a":1}}),
        json!({"id":"thread", "latestTurn":{"b":2,"a":1}, "latestResponse":null}),
        json!({"id":"thread", "latestTurn":{"b":2,"a":1}, "messageCount":1}),
    ];
    for (id, thread) in snapshots.iter().enumerate() {
        rows.push(event(id, json!({"operation":"t3_thread_get","event":"tool.result","outcome":"completed","details":{"result":{"thread":thread}}})));
        rows.push(event(
            100 + id,
            json!({"operation":"t3_thread_get","event":"tool.result","outcome":"error"}),
        ));
    }
    file.write(&rows);
    let report = summarize_usage(&file.path, &UsageWindow::default()).unwrap();
    assert_eq!(report["denominators"]["comparableThreadPairs"], 4);
    assert_eq!(report["monitoring"]["unchangedThreadPairs"], 2);
}

#[test]
fn cardinality_caps_fold_groups_and_lru_refreshes_thread_retention() {
    let file = LogFile::new();
    let mut rows = vec![];
    for id in 0..1005 {
        rows.push(event(id, json!({"operation":format!("tool-{id}"),"event":"tool.result","outcome":"error","details":{"errorCode":"test"}})));
        rows.push(event(id, json!({"source":"t3", "event":"upstream.request", "operation":format!("GET /endpoint-{id}"), "parentCorrelationId":"parent", "parentOperation":format!("tool-{id}")})));
    }
    for id in 0..10000 {
        rows.push(event(id, json!({"operation":"t3_thread_get", "event":"tool.result", "outcome":"completed", "details":{"result":{"thread":{"id":format!("thread-{id}")}}}})));
    }
    for id in [0, 10000, 0, 1] {
        rows.push(event(id, json!({"operation":"t3_thread_get", "event":"tool.result", "outcome":"completed", "details":{"result":{"thread":{"id":format!("thread-{id}")}}}})));
    }
    file.write(&rows);
    let report = summarize_usage(&file.path, &UsageWindow::default()).unwrap();
    for field in ["tools", "endpoints", "errorCodes"] {
        assert_eq!(report[field].as_object().unwrap().len(), 1000, "{field}");
    }
    assert_eq!(
        report["attribution"]["requestsByTool"]
            .as_object()
            .unwrap()
            .len(),
        1000
    );
    assert_eq!(report["tools"]["other"]["errors"], 6);
    assert_eq!(report["tools"]["other"]["completed"], 10004);
    assert_eq!(report["endpoints"]["other"]["requests"], 6);
    assert_eq!(report["errorCodes"]["other"], 6);
    assert_eq!(report["monitoring"]["evictedThreadStates"], 2);
    assert_eq!(report["denominators"]["comparableThreadPairs"], 2);
    assert_eq!(report["monitoring"]["unchangedThreadPairs"], 2);
}

#[test]
fn time_windows_include_edges_skip_bad_times_and_preserve_original_extrema() {
    let file = LogFile::new();
    file.write(&[
        event(0, json!({"timestamp":"2026-09-30T14:00:02+02:00"})),
        event(1, json!({"timestamp":"invalid"})),
        event(2, json!({"timestamp":"2026-09-30T12:00:00Z"})),
        event(3, json!({"timestamp":"2026-09-30T12:00:02.000Z"})),
        event(4, json!({"timestamp":"2026-09-30T12:00:01Z"})),
    ]);
    let all = summarize_usage(&file.path, &UsageWindow::default()).unwrap();
    assert_eq!(all["scan"]["includedEvents"], 5);
    assert_eq!(all["window"]["firstEventAt"], "2026-09-30T12:00:00Z");
    assert_eq!(all["window"]["lastEventAt"], "2026-09-30T14:00:02+02:00");
    let filtered = summarize_usage(
        &file.path,
        &UsageWindow {
            since: Some("2026-09-30T12:00:01Z".into()),
            until: Some("2026-09-30T12:00:02Z".into()),
        },
    )
    .unwrap();
    assert_eq!(filtered["scan"]["includedEvents"], 3);
    assert_eq!(filtered["scan"]["validLines"], 5);
    assert_eq!(filtered["window"]["firstEventAt"], "2026-09-30T12:00:01Z");
}

#[test]
fn missing_files_are_empty_but_read_failures_and_bad_windows_return_errors() {
    let file = LogFile::new();
    let report = summarize_usage(&file.path, &UsageWindow::default()).unwrap();
    assert_eq!(
        report["scan"],
        json!({"invalidLines":0,"bytes":0,"validLines":0,"includedEvents":0})
    );
    assert_eq!(report["window"]["firstEventAt"], Value::Null);
    assert_eq!(report["tools"], json!({}));
    assert!(
        summarize_usage(
            &file.path,
            &UsageWindow {
                since: Some("bad".into()),
                until: None
            }
        )
        .unwrap_err()
        .to_string()
        .contains("since")
    );
    assert!(
        summarize_usage(
            &file.path,
            &UsageWindow {
                since: Some("2026-10-01".into()),
                until: Some("2026-09-30".into())
            }
        )
        .unwrap_err()
        .to_string()
        .contains("before")
    );
    fs::create_dir(&file.path).unwrap();
    assert!(summarize_usage(&file.path, &UsageWindow::default()).is_err());
}

#[test]
fn endpoint_normalization_only_replaces_first_path_and_requires_real_parent_strings() {
    let file = LogFile::new();
    file.write(&[
        event(0, json!({"source":"t3","event":"upstream.request","operation":"GET /api/orchestration/threads/one?query=two","parentCorrelationId":"","parentOperation":"tool"})),
        event(1, json!({"source":"t3","event":"upstream.request","operation":"GET /api/orchestration/threads/two?query=two","parentCorrelationId":"call","parentOperation":""})),
        event(2, json!({"source":"t3","event":"upstream.response","operation":"GET /api/orchestration/threads/one?query=two","outcome":"error","durationMs":0})),
    ]);
    let report = summarize_usage(&file.path, &UsageWindow::default()).unwrap();
    assert_eq!(report["endpoints"].as_object().unwrap().len(), 1);
    assert_eq!(
        report["endpoints"]["GET /api/orchestration/threads/:threadId?query=two"]["requests"],
        2
    );
    assert_eq!(
        report["endpoints"]["GET /api/orchestration/threads/:threadId?query=two"]["errors"],
        1
    );
    assert_eq!(report["attribution"]["attributedRequests"], 0);
}

#[test]
fn group_order_uses_node_entry_sorting_and_integer_property_order() {
    let file = LogFile::new();
    file.write(&[
        event(0, json!({"operation":"a"})),
        event(1, json!({"operation":"a!"})),
        event(2, json!({"operation":"10"})),
        event(3, json!({"operation":"2"})),
    ]);
    let report = summarize_usage(&file.path, &UsageWindow::default()).unwrap();
    assert_eq!(
        report["tools"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["2", "10", "a!", "a"]
    );
}
