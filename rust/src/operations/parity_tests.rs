use super::audit_json::AuditValue;
use super::audit_log::{
    AuditEvent, AuditEventInput, AuditLog, AuditQueryInput, AuditSource, parse_filter_time,
    summarize_for_audit,
};
use super::test_support::{LogFile, event};
use super::usage_summary::{UsageWindow, summarize_usage};
use serde_json::{Value, json};
use std::fs;
use std::process::Command;

#[test]
fn legacy_dates_and_local_time_match_node_in_isolated_timezone_processes() {
    let fixture: Value = serde_json::from_str(include_str!("node_date_cases.json")).unwrap();
    let probe = format!(
        "{}::timezone_probe",
        module_path!().split_once("::").unwrap().1
    );
    for (zone, cases) in fixture["timezoneCases"].as_object().unwrap() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &probe, "--nocapture"])
            .env("TZ", zone)
            .env("T3_AUDIT_DATE_CASES", cases.to_string())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{zone}\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "timezone probe did not run"
        );
    }
}

#[test]
fn timezone_probe() {
    let Ok(cases) = std::env::var("T3_AUDIT_DATE_CASES") else {
        return;
    };
    let cases: Vec<(String, Option<i64>)> = serde_json::from_str(&cases).unwrap();
    let file = LogFile::new();
    for (text, expected) in cases {
        assert_eq!(
            parse_filter_time(Some(&text), "since").ok().flatten(),
            expected,
            "{text}"
        );
        if let Some(timestamp) = expected {
            let instant = chrono::DateTime::from_timestamp_millis(timestamp)
                .unwrap()
                .to_rfc3339();
            file.write(&[event(0, json!({"timestamp":text}))]);
            let query = AuditQueryInput {
                since: Some(instant.clone()),
                until: Some(instant.clone()),
                ..AuditQueryInput::default()
            };
            assert_eq!(
                AuditLog::new(&file.path).query(&query).unwrap().total,
                1,
                "query {text}"
            );
            let report = summarize_usage(
                &file.path,
                &UsageWindow {
                    since: Some(instant.clone()),
                    until: Some(instant),
                },
            )
            .unwrap();
            assert_eq!(report["scan"]["includedEvents"], 1, "usage {text}");
            assert_eq!(report["window"]["firstEventAt"], text);
        }
    }
}

#[test]
fn surrogate_boundary_preview_survives_jsonl_query_and_usage() {
    let value = format!("{}😀", "a".repeat(511));
    let details = json!({"longValue":value});
    let summary = summarize_for_audit(&details);
    let encoded = summary.to_string();
    assert!(encoded.contains(&format!("\"preview\":\"{}\\ud83d\"", "a".repeat(511))));
    assert!(!encoded.contains('\u{fffd}'));
    assert!(serde_json::to_value(&summary).is_err());
    let parsed: AuditValue = serde_json::from_str(&encoded).unwrap();
    let mut expected: Vec<u16> = "a".repeat(511).encode_utf16().collect();
    expected.push(0xd83d);
    assert_eq!(parsed["longValue"]["preview"].as_utf16().unwrap(), expected);
    let file = LogFile::new();
    let audit = AuditLog::new(&file.path);
    let mut input = AuditEventInput::new(AuditSource::Mcp, "tool.call");
    input.operation = Some("preview-test".into());
    input.details = Some(details);
    audit.record(input);
    let line = fs::read_to_string(&file.path).unwrap();
    assert!(line.contains(r"\ud83d"));
    assert!(!line.contains('\u{fffd}'));
    let row: AuditEvent = serde_json::from_str(line.trim_end()).unwrap();
    assert_eq!(
        row.details.as_ref().unwrap()["longValue"]["preview"]
            .as_utf16()
            .unwrap(),
        expected
    );
    let page = audit.query(&AuditQueryInput::default()).unwrap();
    assert_eq!((page.total, page.invalid_lines), (1, 0));
    let encoded_page = serde_json::to_string(&page).unwrap();
    let decoded_page: AuditValue = serde_json::from_str(&encoded_page).unwrap();
    assert_eq!(
        decoded_page["items"][0]["details"]["longValue"]["preview"]
            .as_utf16()
            .unwrap(),
        expected
    );
    let report = summarize_usage(&file.path, &UsageWindow::default()).unwrap();
    assert_eq!(report["scan"]["validLines"], 1);
    assert_eq!(report["scan"]["invalidLines"], 0);
    assert_eq!(report["tools"]["preview-test"]["calls"], 1);
}

#[test]
fn node_surrogates_stay_distinct_in_thread_signatures_and_query_results() {
    let file = LogFile::new();
    let mut lines = String::new();
    for (id, response) in [
        r"\ud83d",
        r"\ud83d",
        r"\ud83e",
        r"\ufffd",
        r"\udc00",
        r"\ud83d\ude00",
    ]
    .iter()
    .enumerate()
    {
        let mut row = event(
            id,
            json!({"operation":"t3_thread_get", "event":"tool.result", "outcome":"completed"}),
        );
        row["details"] = json!({"result":{"thread":{"id":"thread", "latestResponse":"RESPONSE"}}});
        lines.push_str(&row.to_string().replace("RESPONSE", response));
        lines.push('\n');
    }
    fs::write(&file.path, lines).unwrap();
    let page = AuditLog::new(&file.path)
        .query(&AuditQueryInput::default())
        .unwrap();
    assert_eq!((page.total, page.invalid_lines), (6, 0));
    let encoded = serde_json::to_string(&page).unwrap();
    assert!(encoded.contains(r"\ud83d"));
    assert!(encoded.contains(r"\udc00"));
    assert!(encoded.contains('😀'));
    let report = summarize_usage(&file.path, &UsageWindow::default()).unwrap();
    assert_eq!(report["denominators"]["comparableThreadPairs"], 5);
    assert_eq!(report["monitoring"]["unchangedThreadPairs"], 1);
}

#[test]
fn lone_surrogates_in_audit_metadata_and_object_keys_round_trip() {
    let file = LogFile::new();
    fs::write(
        &file.path,
        r#"{"version":1,"eventId":"event-\ud800","timestamp":"2026-10-17T00:00:00.000Z","processId":1,"source":"system","event":"system-\udfff","operation":"operation-\ud800","details":{"key-\udfff":"value-\ud800"}}"#,
    )
    .unwrap();

    let page = AuditLog::new(&file.path)
        .query(&AuditQueryInput::default())
        .unwrap();
    assert_eq!((page.total, page.invalid_lines), (1, 0));
    let encoded = serde_json::to_string(&page).unwrap();
    assert!(encoded.contains(r#""eventId":"event-\ud800""#));
    assert!(encoded.contains(r#""event":"system-\udfff""#));
    assert!(encoded.contains(r#""operation":"operation-\ud800""#));
    assert!(encoded.contains(r#""key-\udfff":"value-\ud800""#));

    let decoded: AuditValue = serde_json::from_str(&encoded).unwrap();
    let metadata_key = "key-".encode_utf16().chain([0xdfff]).collect::<Vec<_>>();
    assert_eq!(
        decoded["items"][0]["details"]
            .get_utf16(&metadata_key)
            .unwrap()
            .as_utf16()
            .unwrap(),
        "value-".encode_utf16().chain([0xd800]).collect::<Vec<_>>()
    );
}
