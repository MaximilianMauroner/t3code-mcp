use super::super::test_support::{LogFile, baseline_rows, event};
use super::*;
use chrono::DateTime;
use serde_json::json;
use std::sync::Arc;

#[test]
fn persisted_records_redact_content_and_keep_node_field_order() {
    let log_file = LogFile::new();
    let audit = AuditLog::new(&log_file.path);
    let mut input = AuditEventInput::new(AuditSource::Mcp, "tool.call");
    input.correlation_id = Some("call".into());
    input.operation = Some("t3_thread_send".into());
    input.outcome = Some("started".into());
    input.duration_ms = Some(12.5);
    input.details = Some(
        json!({"arguments":{"threadId":"thread-1", "message":"private prompt", "authorization":"Bearer hidden-secret"}}),
    );
    audit.record(input);
    let raw = fs::read_to_string(&log_file.path).unwrap();
    assert!(!raw.contains("private prompt"));
    assert!(!raw.contains("hidden-secret"));
    assert!(raw.contains("thread-1"));
    let row: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        row.as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "version",
            "eventId",
            "timestamp",
            "processId",
            "source",
            "event",
            "correlationId",
            "operation",
            "outcome",
            "durationMs",
            "details"
        ]
    );
    assert_eq!(row["durationMs"], 13);
    assert!(row["eventId"].as_str().unwrap().starts_with("evt_"));
    assert!(DateTime::parse_from_rfc3339(row["timestamp"].as_str().unwrap()).is_ok());
    assert_eq!(audit.query(&AuditQueryInput::default()).unwrap().total, 1);
    assert!(audit.diagnostic_error().is_none());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&log_file.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn absent_details_are_omitted_and_null_details_are_wrapped() {
    let file = LogFile::new();
    let audit = AuditLog::new(&file.path);
    audit.record(AuditEventInput::new(AuditSource::System, "no-details"));
    let mut input = AuditEventInput::new(AuditSource::System, "null-details");
    input.details = Some(Value::Null);
    audit.record(input);
    let rows: Vec<Value> = fs::read_to_string(&file.path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(rows[0].get("details").is_none());
    assert_eq!(rows[1]["details"], json!({"value":null}));
}

#[test]
fn filtered_pages_count_all_invalid_rows_and_new_appends() {
    let file = LogFile::new();
    let mut rows = vec![
        event(0, json!({})),
        event(1, json!({"source":"git"})),
        event(2, json!({"timestamp":"2026-09-30T12:00:02Z"})),
        json!({}),
        event(3, json!({"timestamp":"2026-09-30T12:00:03Z"})),
        event(4, json!({"timestamp":"invalid"})),
    ];
    rows.push(event(
        5,
        json!({"timestamp":"2026-09-30T12:00:04Z", "operation":"different"}),
    ));
    file.write(&rows);
    OpenOptions::new()
        .append(true)
        .open(&file.path)
        .unwrap()
        .write_all(b"bad-json\n\n")
        .unwrap();
    let audit = AuditLog::new(&file.path);
    let mut input = AuditQueryInput {
        source: Some(AuditSource::Mcp),
        operation: Some("t3_thread_send".into()),
        since: Some("2026-09-30T12:00:02Z".into()),
        limit: 1,
        ..AuditQueryInput::default()
    };
    let page = audit.query(&input).unwrap();
    assert_eq!(
        (
            page.total,
            page.invalid_lines,
            page.next_cursor.as_deref(),
            page.has_more
        ),
        (2, 2, Some("1"), true)
    );
    assert_eq!(page.items[0].event_id, "event-2");
    input.cursor = page.next_cursor;
    let page = audit.query(&input).unwrap();
    assert_eq!(page.items[0].event_id, "event-3");
    assert_eq!(
        (page.total, page.invalid_lines, page.has_more),
        (2, 2, false)
    );
    assert!(page.next_cursor.is_none());
    input.cursor = Some("9".into());
    assert!(audit.query(&input).unwrap().items.is_empty());
    input.cursor = Some("2".into());
    OpenOptions::new()
        .append(true)
        .open(&file.path)
        .unwrap()
        .write_all(
            format!(
                "{}\n",
                event(6, json!({"timestamp":"2026-09-30T12:00:05Z"}))
            )
            .as_bytes(),
        )
        .unwrap();
    assert_eq!(audit.query(&input).unwrap().items[0].event_id, "event-6");
}

#[test]
fn scanning_uses_fixed_bytes_and_accepts_all_node_line_endings() {
    let file = LogFile::new();
    let first = event(0, json!({}));
    let second = event(1, json!({}));
    fs::write(&file.path, format!("{first}\r\n \r{second}")).unwrap();
    let bytes = fs::metadata(&file.path).unwrap().len();
    let mut seen = vec![];
    let scan = scan_audit_file(&file.path, |row| {
        seen.push(row.event_id.as_str().unwrap().to_owned());
        OpenOptions::new()
            .append(true)
            .open(&file.path)
            .unwrap()
            .write_all(format!("\n{}", event(2, json!({}))).as_bytes())
            .unwrap();
    })
    .unwrap();
    assert_eq!(seen, ["event-0", "event-1"]);
    assert_eq!(
        (scan.bytes, scan.valid_lines, scan.invalid_lines),
        (bytes, 2, 0)
    );
    assert_eq!(
        AuditLog::new(&file.path)
            .query(&AuditQueryInput::default())
            .unwrap()
            .total,
        4
    );
}

#[test]
fn node_validation_rejects_null_optional_fields_but_keeps_unknown_fields() {
    let file = LogFile::new();
    let mut rows = vec![event(
        0,
        json!({"unknownField":{"keep":true}, "processId":1.5, "durationMs":-1.5, "timestamp":"bad-date"}),
    )];
    for overrides in [
        json!({"version":2}),
        json!({"source":"unknown"}),
        json!({"processId":"1"}),
        json!({"correlationId":null}),
        json!({"parentCorrelationId":null}),
        json!({"parentOperation":null}),
        json!({"operation":null}),
        json!({"outcome":null}),
        json!({"durationMs":null}),
        json!({"details":null}),
        json!({"details":[]}),
    ] {
        rows.push(event(1, overrides));
    }
    file.write(&rows);
    let page = AuditLog::new(&file.path)
        .query(&AuditQueryInput::default())
        .unwrap();
    assert_eq!((page.total, page.invalid_lines), (1, 11));
    assert_eq!(serde_json::to_value(&page.items[0]).unwrap(), rows[0]);
}

#[test]
fn scan_preserves_node_fixture_rows_and_key_order() {
    let file = LogFile::new();
    let rows = baseline_rows();
    file.write(&rows);
    let audit = AuditLog::new(&file.path);
    let page = audit
        .query(&AuditQueryInput {
            limit: 100,
            ..AuditQueryInput::default()
        })
        .unwrap();
    assert_eq!((page.total, page.invalid_lines), (81, 0));
    for (row, expected) in page.items.iter().zip(rows) {
        assert_eq!(serde_json::to_string(row).unwrap(), expected.to_string());
    }
}

#[test]
fn redaction_matches_node_container_rules_and_utf16_lengths() {
    let result = summarize_for_audit(&json!({
        "api_key":"hidden", "body":{"threadId":"keep", "text":"hidden"},
        "answers":{"question":"hidden"}, "payload":42, "options":null,
        "content":["hidden"], "message":{"id":"keep"}, "prompt":["hidden"],
        "text":"😀", "query":"Bearer secret; api_key=hidden", "long":"😀".repeat(300)
    }));
    assert_eq!(result["body"]["threadId"], "keep");
    assert_eq!(result["body"]["text"]["redacted"], true);
    assert_eq!(
        result["answers"],
        json!({"redacted":true,"type":"object","keyCount":1})
    );
    assert_eq!(result["options"], json!({"redacted":true,"type":"object"}));
    assert_eq!(
        result["content"],
        json!({"redacted":true,"type":"array","length":1})
    );
    assert_eq!(result["prompt"][0]["redacted"], true);
    assert_eq!(result["message"]["id"], "keep");
    assert_eq!(
        result["text"],
        json!({"redacted":true,"type":"string","length":2,"sha256":"f0443a342c5ef54783a111b51ba56c938e474c32324d90c3a60c9c8e3a37e2d9"})
    );
    assert_eq!(result["query"], "Bearer [REDACTED]; api_key=[REDACTED]");
    assert_eq!(result["long"]["length"], 600);
    assert_eq!(
        result["long"]["preview"]
            .as_str()
            .unwrap()
            .encode_utf16()
            .count(),
        512
    );
    assert!(!result.to_string().contains("hidden"));
}

#[test]
fn redaction_bounds_arrays_keys_and_depth_and_sorts_like_javascript() {
    let many = json!((0..30).collect::<Vec<_>>());
    let summary = summarize_for_audit(&many);
    assert_eq!(summary["items"].as_array().unwrap().len(), 25);
    assert_eq!(summary["length"], 30);
    let mut keys = Map::new();
    for i in 0..60 {
        keys.insert(format!("k{i:02}"), json!(i));
    }
    let result = summarize_for_audit(&json!(keys));
    assert_eq!(result["_truncatedKeys"], 10);
    assert!(result.get("k50").is_none());
    let result = summarize_for_audit(&json!({"a":{"b":{"c":{"d":{"e":{"f":{"g":1}}}}}}}));
    assert_eq!(
        result["a"]["b"]["c"]["d"]["e"]["f"],
        json!({"type":"object","truncated":true})
    );
    let result = summarize_for_audit(&json!({"10":1, "2":2, "z":3, "😀":4, "\u{e000}":5}));
    assert_eq!(
        result.to_string(),
        "{\"2\":2,\"10\":1,\"z\":3,\"😀\":4,\"\u{e000}\":5}"
    );
}

#[test]
fn inline_secrets_use_node_case_and_word_boundary_rules() {
    assert_eq!(
        sanitize_error("bEaReR\u{00a0}secret, API-KEY : hidden; sk-123456789012, password=hidden"),
        "Bearer [REDACTED], API-KEY=[REDACTED]; [REDACTED], password=[REDACTED]"
    );
    assert_eq!(
        sanitize_error("ésecret=hidden xsecret=keep"),
        "ésecret=[REDACTED] xsecret=keep"
    );
}

#[test]
fn filters_and_cursors_keep_node_numeric_coercion() {
    let file = LogFile::new();
    file.write(&(0..3).map(|id| event(id, json!({}))).collect::<Vec<_>>());
    let audit = AuditLog::new(&file.path);
    for (cursor, expected) in [
        ("", Some("event-0")),
        (" ", Some("event-0")),
        ("0", Some("event-0")),
        ("-0", Some("event-0")),
        ("1.0", Some("event-1")),
        ("1e2", None),
        ("0x10", None),
        ("0b10", Some("event-2")),
        ("0o10", None),
        ("+2", Some("event-2")),
    ] {
        let page = audit
            .query(&AuditQueryInput {
                cursor: Some(cursor.into()),
                limit: 1,
                ..AuditQueryInput::default()
            })
            .unwrap();
        assert_eq!(
            page.items.first().and_then(|row| row.event_id.as_str()),
            expected,
            "{cursor}"
        );
    }
    for cursor in [
        "-1",
        "1.5",
        "bad",
        "9007199254740992",
        "Infinity",
        "0x+1",
        "0b+1",
        "0o+1",
    ] {
        assert!(
            audit
                .query(&AuditQueryInput {
                    cursor: Some(cursor.into()),
                    ..AuditQueryInput::default()
                })
                .is_err(),
            "{cursor}"
        );
    }
    assert_eq!(
        parse_filter_time(Some("2026-09-30"), "since").unwrap(),
        Some(1_790_726_400_000)
    );
    assert!(
        parse_filter_time(Some("bad"), "since")
            .unwrap_err()
            .to_string()
            .contains("since")
    );
    assert_eq!(parse_filter_time(Some("\u{feff}"), "since").unwrap(), None);
}

#[tokio::test]
async fn concurrent_task_contexts_stay_isolated_and_suppress_self_parents() {
    let file = LogFile::new();
    let audit = Arc::new(AuditLog::new(&file.path));
    let mut handles = vec![];
    for id in ["A", "B"] {
        let audit = audit.clone();
        handles.push(tokio::spawn(with_audit_context(
            format!("mcp-{id}"),
            format!("tool-{id}"),
            async move {
                tokio::task::yield_now().await;
                for source in [AuditSource::T3, AuditSource::Git, AuditSource::Journal] {
                    let mut input = AuditEventInput::new(source, "child");
                    input.operation = Some(id.into());
                    audit.record(input);
                }
                let mut own = AuditEventInput::new(AuditSource::Mcp, "self");
                own.correlation_id = Some(format!("mcp-{id}"));
                audit.record(own);
            },
        )));
    }
    for handle in handles {
        handle.await.unwrap();
    }
    audit.record(AuditEventInput::new(AuditSource::System, "outside"));
    let page = audit.query(&AuditQueryInput::default()).unwrap();
    assert_eq!(page.total, 9);
    for event in page.items {
        if event.event == "child" {
            assert_eq!(
                event
                    .parent_correlation_id
                    .as_ref()
                    .and_then(|value| value.as_str()),
                Some(
                    format!(
                        "mcp-{}",
                        event.operation.as_ref().unwrap().to_string_lossy()
                    )
                    .as_str()
                )
            );
            assert_eq!(
                event
                    .parent_operation
                    .as_ref()
                    .and_then(|value| value.as_str()),
                Some(
                    format!(
                        "tool-{}",
                        event.operation.as_ref().unwrap().to_string_lossy()
                    )
                    .as_str()
                )
            );
        } else {
            assert!(event.parent_correlation_id.is_none());
        }
    }
}

#[test]
fn filesystem_errors_are_best_effort_for_records_and_diagnostic_for_queries() {
    let file = LogFile::new();
    fs::write(&file.path, "blocking parent").unwrap();
    let audit = AuditLog::new(file.path.join("audit.jsonl"));
    audit.record(AuditEventInput::new(AuditSource::System, "test"));
    assert!(audit.diagnostic_error().is_some());
    assert!(audit.query(&AuditQueryInput::default()).is_err());
    let directory_log = AuditLog::new(&file.path);
    fs::remove_file(&file.path).unwrap();
    fs::create_dir(&file.path).unwrap();
    let page = directory_log.query(&AuditQueryInput::default()).unwrap();
    assert_eq!(page.total, 0);
    assert!(directory_log.diagnostic_error().is_some());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&file.path, fs::Permissions::from_mode(0o700)).unwrap();
    }
}

#[test]
fn iso_filter_dates_match_node_rollovers_offsets_and_rejections() {
    for (input, expected) in [
        ("2026-02-30", Some(1_772_409_600_000)),
        ("2026-09-30T24:00:00Z", Some(1_790_812_800_000)),
        ("2026-09-30T12:00:60Z", None),
        ("2026-09-30T12:00Z", Some(1_790_769_600_000)),
        ("2026-09-30T12:00:00+0200", Some(1_790_762_400_000)),
        ("2026-09-30T12:00:00z", Some(1_790_769_600_000)),
        ("2026-09-30Z", Some(1_790_726_400_000)),
        ("2026-09-30T12:00:00.0000001Z", Some(1_790_769_600_000)),
        (" 2026-09-30T12:00Z ", None),
        ("2026-09-30T24:00:00.001Z", None),
        ("2026-09-30T12:00:00ZZ", None),
    ] {
        assert_eq!(
            parse_filter_time(Some(input), "since").ok().flatten(),
            expected,
            "{input}"
        );
    }
}

#[test]
fn persisted_numbers_and_duration_rounding_match_javascript() {
    let file = LogFile::new();
    let audit = AuditLog::new(&file.path);
    for (id, duration) in [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        -0.5,
        0.49999999999999994,
        0.5,
    ]
    .iter()
    .enumerate()
    {
        let mut input = AuditEventInput::new(AuditSource::System, id.to_string());
        input.duration_ms = Some(*duration);
        audit.record(input);
    }
    let rows: Vec<Value> = fs::read_to_string(&file.path)
        .unwrap()
        .lines()
        .map(|v| serde_json::from_str(v).unwrap())
        .collect();
    assert_eq!(
        rows.iter()
            .map(|v| v["durationMs"].clone())
            .collect::<Vec<_>>(),
        vec![
            Value::Null,
            Value::Null,
            json!(0),
            json!(0),
            json!(0),
            json!(1)
        ]
    );
    assert_eq!(
        audit
            .query(&AuditQueryInput::default())
            .unwrap()
            .invalid_lines,
        2
    );
    assert_eq!(
        summarize_for_audit(&json!([1.0, -0.0, 1e-6, 1e-7, 1e20, 1e21])).to_string(),
        "[1,0,0.000001,1e-7,100000000000000000000,1e+21]"
    );
}

#[test]
fn object_assignment_and_secret_patterns_match_javascript_edge_cases() {
    let summary = summarize_for_audit(
        &serde_json::from_str::<Value>(r#"{"__proto__":{"hidden":"value"},"safe":1}"#).unwrap(),
    );
    assert_eq!(summary, json!({"safe":1}));
    assert_eq!(
        sanitize_error("ſecret=keep; secret\u{feff}:hidden"),
        "ſecret=keep; secret=[REDACTED]"
    );
}
