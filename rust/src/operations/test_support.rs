use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use uuid::Uuid;

pub struct LogFile {
    directory: PathBuf,
    pub path: PathBuf,
}

impl LogFile {
    pub fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("t3-rust-audit-{}", Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        Self {
            path: directory.join("audit.jsonl"),
            directory,
        }
    }

    pub fn write(&self, rows: &[Value]) {
        fs::write(
            &self.path,
            rows.iter()
                .map(|row| format!("{row}\n"))
                .collect::<String>(),
        )
        .unwrap();
    }
}

impl Drop for LogFile {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.directory).unwrap();
    }
}

pub fn event(id: usize, overrides: Value) -> Value {
    let mut row = json!({"version":1, "eventId":format!("event-{id}"), "timestamp":"2026-09-30T12:00:00.000Z",
        "processId":1, "source":"mcp", "event":"tool.call", "operation":"t3_thread_send"});
    row.as_object_mut()
        .unwrap()
        .extend(overrides.as_object().unwrap().clone());
    row
}

pub fn baseline() -> Value {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/node-compat-baseline.json"
    ))
    .unwrap()
}

pub fn baseline_rows() -> Vec<Value> {
    baseline()["files"]["audit"]
        .as_array()
        .unwrap()
        .iter()
        .cloned()
        .map(|mut row| {
            row["processId"] = json!(1);
            row["timestamp"] = json!("2026-09-30T12:00:00.000Z");
            if row.get("durationMs").is_some() {
                row["durationMs"] = json!(12);
            }
            row
        })
        .collect()
}
