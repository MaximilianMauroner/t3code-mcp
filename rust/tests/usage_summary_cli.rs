use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde_json::{Value, json};
use uuid::Uuid;

struct AuditFile(PathBuf);

impl AuditFile {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("t3-usage-cli-{}.jsonl", Uuid::new_v4())))
    }
}

impl Drop for AuditFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[test]
fn usage_summary_cli_matches_the_node_capture() {
    let baseline: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/node-compat-baseline.json"
    ))
    .unwrap();
    let mut rows = baseline["files"]["audit"].as_array().unwrap().clone();
    for row in &mut rows {
        row["processId"] = json!(1);
        row["timestamp"] = json!("2026-09-30T12:00:00.000Z");
        if row.get("durationMs").is_some() {
            row["durationMs"] = json!(12);
        }
    }
    let file = AuditFile::new();
    fs::write(
        &file.0,
        rows.iter()
            .map(|row| format!("{row}\n"))
            .collect::<String>(),
    )
    .unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_t3-code-mcp"))
        .args(["usage-summary", "--file"])
        .arg(&file.0)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );

    let stdout = String::from_utf8(result.stdout).unwrap();
    let mut actual: Value = serde_json::from_str(&stdout).unwrap();
    let expected = baseline["cli"]["usageSummary"]["stdout"].as_str().unwrap();
    let expected: Value = serde_json::from_str(expected).unwrap();
    replace_dynamic_values(&mut actual, &expected);
    assert_eq!(
        format!("{}\n", serde_json::to_string_pretty(&actual).unwrap()),
        baseline["cli"]["usageSummary"]["stdout"].as_str().unwrap()
    );
}

fn replace_dynamic_values(actual: &mut Value, expected: &Value) {
    match expected {
        Value::String(value)
            if matches!(
                value.as_str(),
                "<ISO_TIME>" | "<NUMBER>" | "<UUID>" | "<PID>"
            ) =>
        {
            *actual = expected.clone();
        }
        Value::Object(expected_fields) => {
            let Some(actual_fields) = actual.as_object_mut() else {
                return;
            };
            for (name, expected_value) in expected_fields {
                if let Some(actual_value) = actual_fields.get_mut(name) {
                    replace_dynamic_values(actual_value, expected_value);
                }
            }
        }
        Value::Array(expected_items) => {
            let Some(actual_items) = actual.as_array_mut() else {
                return;
            };
            for (actual_item, expected_item) in actual_items.iter_mut().zip(expected_items) {
                replace_dynamic_values(actual_item, expected_item);
            }
        }
        _ => {}
    }
}
