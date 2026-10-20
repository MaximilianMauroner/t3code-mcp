use super::audit_log::{AuditEventInput, AuditLog, AuditSource, AuditValue, summarize_for_audit};
use super::{audit_json::normalize_json, parse_timestamp};
use crate::settings::{RuntimeMode, SettingsReceipt};
use chrono::{SecondsFormat, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use uuid::Uuid;

#[derive(Debug)]
pub enum JournalError {
    Io(io::Error),
    Invalid(String),
    IdempotencyConflict(String),
    NotFound(String),
}
impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::Invalid(message)
            | Self::IdempotencyConflict(message)
            | Self::NotFound(message) => f.write_str(message),
        }
    }
}
impl std::error::Error for JournalError {}
impl From<io::Error> for JournalError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
type Result<T> = std::result::Result<T, JournalError>;
fn invalid(message: &str) -> JournalError {
    JournalError::Invalid(message.into())
}

macro_rules! record {
    ($name:ident, $validate:ident, $message:literal) => {
        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
        #[serde(try_from = "Value", into = "Value")]
        pub struct $name(Value);
        impl TryFrom<Value> for $name {
            type Error = JournalError;
            fn try_from(mut value: Value) -> Result<Self> {
                normalize_json(&mut value);
                if $validate(&value) {
                    Ok(Self(value))
                } else {
                    Err(invalid($message))
                }
            }
        }
        impl From<$name> for Value {
            fn from(value: $name) -> Value {
                value.0
            }
        }
        impl Deref for $name {
            type Target = Value;
            fn deref(&self) -> &Value {
                &self.0
            }
        }
    };
}
record!(
    OperationRecord,
    valid_operation,
    "Operation journal contains an invalid operation."
);
record!(
    TaskRecord,
    valid_task,
    "Operation journal contains an invalid task."
);
record!(
    FailureEvidenceOrder,
    valid_order,
    "Operation journal contains an invalid failure order."
);

impl FailureEvidenceOrder {
    pub fn compare(&self, other: &Self) -> Ordering {
        compare_order(&self.0, &other.0)
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub enum WritableOperationKind {
    #[serde(rename = "thread.create")]
    ThreadCreate,
    #[serde(rename = "thread.turn.start")]
    ThreadTurnStart,
    #[serde(rename = "thread.approval.respond")]
    ThreadApprovalRespond,
    #[serde(rename = "thread.user-input.respond")]
    ThreadUserInputRespond,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OperationStatus {
    Prepared,
    Accepted,
    Uncertain,
    Rejected,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStage {
    Prepared,
    ThreadCreateUncertain,
    ThreadCreated,
    DispatchRejected,
    DispatchUncertain,
    RunAccepted,
    Rejected,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BaselineAttribution {
    Clean,
    Dirty,
    Unavailable,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RecoveryState {
    Completed,
    Interrupted,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalRunStatus {
    Failed,
}

#[derive(Clone, Debug)]
pub struct BeginOperationInput {
    pub kind: WritableOperationKind,
    pub idempotency_key: String,
    pub payload_hash: String,
    pub command_id: Option<String>,
    pub project_id: Option<String>,
    pub thread_id: Option<String>,
    pub run_id: Option<String>,
    pub message_id: Option<String>,
    pub turn_id: Option<String>,
    pub settings: Option<SettingsReceipt>,
}
#[derive(Clone, Debug)]
pub struct BeginTaskInput {
    pub idempotency_key: String,
    pub payload_hash: String,
    pub project_id: String,
    pub title: String,
    pub runtime_mode: RuntimeMode,
}
#[derive(Default, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<OperationStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub t3_sequence: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_run_status: Option<TerminalRunStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_failure: Option<Value>,
}
#[derive(Default, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<TaskStage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_operation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_operation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_attribution: Option<BaselineAttribution>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<Option<String>>,
}
#[derive(Debug)]
pub struct Begun<T> {
    pub record: T,
    pub reused: bool,
}
#[derive(Debug)]
pub struct TerminalFailure {
    pub thread_id: String,
    pub turn_id: String,
    pub failure: Value,
    pub operation_id: Option<String>,
    pub order: Option<FailureEvidenceOrder>,
}
#[derive(Debug)]
pub struct TerminalRecovery {
    pub thread_id: String,
    pub turn_id: String,
    pub state: RecoveryState,
    pub order: Option<FailureEvidenceOrder>,
}
#[derive(Clone, Default)]
struct JournalState {
    initialized: bool,
    entries: IndexMap<String, OperationRecord>,
    tasks: IndexMap<String, TaskRecord>,
    thread_failures: IndexMap<String, Value>,
}
pub struct OperationJournal {
    file_path: PathBuf,
    audit_log: Option<Arc<AuditLog>>,
    state: Mutex<JournalState>,
}
impl OperationJournal {
    pub fn new(file_path: impl Into<PathBuf>, audit_log: Option<Arc<AuditLog>>) -> Self {
        Self {
            file_path: file_path.into(),
            audit_log,
            state: Mutex::new(JournalState::default()),
        }
    }
    pub fn init(&self) -> Result<()> {
        drop(self.state()?);
        Ok(())
    }
    fn state(&self) -> Result<MutexGuard<'_, JournalState>> {
        let mut state = self.state.lock().unwrap();
        if !state.initialized {
            self.init_state(&mut state)?;
        }
        Ok(state)
    }
    fn parent(&self) -> &Path {
        self.file_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
    }
    fn init_state(&self, state: &mut JournalState) -> Result<()> {
        fs::create_dir_all(self.parent())?;
        let mut loaded = JournalState::default();
        match fs::read_to_string(&self.file_path) {
            Ok(raw) => {
                let mut root: Value =
                    serde_json::from_str(&raw).map_err(|error| invalid(&error.to_string()))?;
                normalize_json(&mut root);
                if !root.is_object() || root.get("operations").is_none() {
                    return Err(invalid("Operation journal has an invalid shape."));
                }
                let operations = root["operations"]
                    .as_array()
                    .ok_or_else(|| invalid("Operation journal operations must be an array."))?;
                for entry in operations {
                    let record = OperationRecord::try_from(entry.clone())?;
                    loaded
                        .entries
                        .insert(text(entry, "idempotencyKey").into(), record);
                }
                if let Some(tasks) = root.get("tasks") {
                    for task in tasks
                        .as_array()
                        .ok_or_else(|| invalid("Operation journal tasks must be an array."))?
                    {
                        let record = TaskRecord::try_from(task.clone())?;
                        loaded
                            .tasks
                            .insert(text(task, "idempotencyKey").into(), record);
                    }
                }
                if let Some(failures) = root.get("threadFailures") {
                    for row in failures.as_array().ok_or_else(|| {
                        invalid("Operation journal thread failures must be an array.")
                    })? {
                        if !valid_thread_failure(row) {
                            return Err(invalid(
                                "Operation journal contains an invalid thread failure.",
                            ));
                        }
                        loaded.thread_failures.insert(
                            failure_key(text(row, "threadId"), text(row, "turnId")),
                            row.clone(),
                        );
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        let now = timestamp();
        let mut recovered = 0;
        for entry in loaded.entries.values_mut() {
            if entry["status"] == "prepared" {
                entry.0["status"] = json!("uncertain");
                entry.0["updatedAt"] = json!(now);
                entry.0["lastError"] =
                    json!("The gateway stopped before the T3 command outcome was recorded.");
                recovered += 1;
            }
        }
        loaded.initialized = true;
        if recovered > 0 {
            self.persist(&loaded)?;
        }
        *state = loaded;
        self.audit("journal.initialized", None, None, "completed", json!({
            "filePath":self.file_path, "operationCount":state.entries.len(), "taskCount":state.tasks.len(), "recoveredPreparedOperations":recovered
        }));
        Ok(())
    }
    pub fn begin(&self, input: BeginOperationInput) -> Result<Begun<OperationRecord>> {
        let mut locked = self.state()?;
        let mut state = locked.clone();
        let kind = serde_json::to_value(input.kind).unwrap();
        if let Some(existing) = state.entries.get(&input.idempotency_key) {
            if existing["payloadHash"] != input.payload_hash || existing["kind"] != kind {
                return Err(JournalError::IdempotencyConflict(format!(
                    "Idempotency key {} was already used for a different operation.",
                    input.idempotency_key
                )));
            }
            self.audit("operation.begin", Some(text(existing, "operationId")), kind.as_str(), "reused", json!({
                "operationId":existing["operationId"], "commandId":existing["commandId"], "idempotencyKey":input.idempotency_key,
                "payloadHash":input.payload_hash, "status":existing["status"]
            }));
            return Ok(Begun {
                record: existing.clone(),
                reused: true,
            });
        }
        let now = timestamp();
        let mut row = json!({"operationId":format!("op_{}", Uuid::new_v4()), "kind":kind, "idempotencyKey":input.idempotency_key,
            "payloadHash":input.payload_hash, "commandId":input.command_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
            "status":"prepared", "createdAt":now, "updatedAt":now});
        for (field, value) in [
            ("projectId", input.project_id),
            ("threadId", input.thread_id),
            ("runId", input.run_id),
            ("messageId", input.message_id),
            ("turnId", input.turn_id),
        ] {
            if let Some(value) = value {
                row[field] = json!(value);
            }
        }
        if let Some(settings) = input.settings {
            row["settings"] = serde_json::to_value(settings).unwrap();
        }
        let record = OperationRecord(row);
        state
            .entries
            .insert(input.idempotency_key.clone(), record.clone());
        self.persist(&state)?;
        *locked = state;
        let mut details = json!({"operationId":record["operationId"], "commandId":record["commandId"], "idempotencyKey":input.idempotency_key, "payloadHash":input.payload_hash});
        for field in ["projectId", "threadId", "settings"] {
            copy_field(&mut details, &record, field);
        }
        self.audit(
            "operation.begin",
            Some(text(&record, "operationId")),
            kind.as_str(),
            "prepared",
            details,
        );
        Ok(Begun {
            record,
            reused: false,
        })
    }
    pub fn begin_task(&self, input: BeginTaskInput) -> Result<Begun<TaskRecord>> {
        let mut locked = self.state()?;
        let mut state = locked.clone();
        if let Some(existing) = state.tasks.get(&input.idempotency_key) {
            if existing["payloadHash"] != input.payload_hash {
                return Err(JournalError::IdempotencyConflict(format!(
                    "Idempotency key {} was already used for a different task start.",
                    input.idempotency_key
                )));
            }
            self.audit("task.begin", Some(text(existing, "taskRef")), Some("task.start"), "reused", json!({"taskRef":existing["taskRef"], "idempotencyKey":input.idempotency_key, "payloadHash":input.payload_hash, "stage":existing["stage"]}));
            return Ok(Begun {
                record: existing.clone(),
                reused: true,
            });
        }
        let now = timestamp();
        let record = TaskRecord(
            json!({"taskRef":format!("task_{}",Uuid::new_v4()), "idempotencyKey":input.idempotency_key,
            "payloadHash":input.payload_hash, "projectId":input.project_id, "title":input.title, "runtimeMode":input.runtime_mode,
            "threadIdempotencyKey":format!("{}:thread",input.idempotency_key), "runIdempotencyKey":format!("{}:turn",input.idempotency_key),
            "stage":"prepared", "createdAt":now, "updatedAt":now}),
        );
        state
            .tasks
            .insert(input.idempotency_key.clone(), record.clone());
        self.persist(&state)?;
        *locked = state;
        self.audit("task.begin", Some(text(&record, "taskRef")), Some("task.start"), "prepared", json!({"taskRef":record["taskRef"], "idempotencyKey":input.idempotency_key,
            "payloadHash":input.payload_hash, "projectId":input.project_id, "runtimeMode":input.runtime_mode, "stage":record["stage"]}));
        Ok(Begun {
            record,
            reused: false,
        })
    }
    pub fn update(&self, operation_id: &str, patch: OperationPatch) -> Result<OperationRecord> {
        let mut locked = self.state()?;
        let mut state = locked.clone();
        let entry = state
            .entries
            .values_mut()
            .find(|entry| entry["operationId"] == operation_id)
            .ok_or_else(|| {
                JournalError::NotFound(format!("Operation {operation_id} was not found."))
            })?;
        let patch = serde_json::to_value(patch).unwrap();
        let mut updated = entry.0.clone();
        apply_patch(&mut updated, &patch);
        let record = OperationRecord::try_from(updated)?;
        *entry = record.clone();
        self.persist(&state)?;
        *locked = state;
        self.audit(
            "operation.update",
            Some(operation_id),
            record["kind"].as_str(),
            text(&record, "status"),
            AuditValue::object([
                ("operationId", operation_id.into()),
                ("status", (&record["status"]).into()),
                ("patch", summarize_for_audit(&patch)),
            ]),
        );
        Ok(record)
    }
    pub fn update_task(&self, task_ref: &str, patch: TaskPatch) -> Result<TaskRecord> {
        let mut locked = self.state()?;
        let mut state = locked.clone();
        let entry = state
            .tasks
            .values_mut()
            .find(|entry| entry["taskRef"] == task_ref)
            .ok_or_else(|| JournalError::NotFound(format!("Task {task_ref} was not found.")))?;
        let patch = serde_json::to_value(patch).unwrap();
        apply_patch(&mut entry.0, &patch);
        let record = entry.clone();
        self.persist(&state)?;
        *locked = state;
        self.audit(
            "task.update",
            Some(task_ref),
            Some("task.start"),
            "completed",
            AuditValue::object([
                ("taskRef", task_ref.into()),
                ("stage", (&record["stage"]).into()),
                ("patch", summarize_for_audit(&patch)),
            ]),
        );
        Ok(record)
    }
    pub fn get_by_operation_id(&self, id: &str) -> Result<Option<OperationRecord>> {
        Ok(self
            .state()?
            .entries
            .values()
            .find(|row| row["operationId"] == id)
            .cloned())
    }
    pub fn get_by_idempotency_key(&self, key: &str) -> Result<Option<OperationRecord>> {
        Ok(self.state()?.entries.get(key).cloned())
    }
    pub fn get_by_run_id(&self, id: &str) -> Result<Option<OperationRecord>> {
        Ok(self
            .state()?
            .entries
            .values()
            .find(|row| row["runId"] == id)
            .cloned())
    }
    pub fn get_task_by_ref(&self, id: &str) -> Result<Option<TaskRecord>> {
        Ok(self
            .state()?
            .tasks
            .values()
            .find(|row| row["taskRef"] == id)
            .cloned())
    }
    pub fn list_tasks(&self) -> Result<Vec<TaskRecord>> {
        Ok(self.state()?.tasks.values().cloned().collect())
    }
    pub fn refresh_task(&self, task_ref: &str, persist: bool) -> Result<TaskRecord> {
        let mut locked = self.state()?;
        let stored = locked
            .tasks
            .values()
            .find(|task| task["taskRef"] == task_ref)
            .ok_or_else(|| JournalError::NotFound(format!("Task {task_ref} was not found.")))?;
        let thread = locked.entries.get(text(stored, "threadIdempotencyKey"));
        let run = locked.entries.get(text(stored, "runIdempotencyKey"));
        let stage = match run.map(|operation| text(operation, "status")) {
            Some("accepted") => "run_accepted",
            Some("prepared" | "uncertain") => "dispatch_uncertain",
            Some("rejected") => "dispatch_rejected",
            _ => match thread.map(|operation| text(operation, "status")) {
                Some("accepted") => "thread_created",
                Some("prepared" | "uncertain") => "thread_create_uncertain",
                Some("rejected") => "rejected",
                _ => text(stored, "stage"),
            },
        };
        let last_error = if matches!(stage, "run_accepted" | "thread_created") {
            None
        } else {
            stored["lastError"]
                .as_str()
                .or_else(|| run.and_then(|row| row["lastError"].as_str()))
                .or_else(|| thread.and_then(|row| row["lastError"].as_str()))
        };
        let mut projected = stored.0.clone();
        projected["stage"] = json!(stage);
        for (field, operation, source) in [
            ("threadId", thread, "threadId"),
            ("runId", run, "runId"),
            ("messageId", run, "messageId"),
            ("threadOperationId", thread, "operationId"),
            ("runOperationId", run, "operationId"),
        ] {
            if projected[field].is_null()
                && let Some(value) = operation.and_then(|row| row.get(source))
            {
                projected[field] = value.clone();
            }
        }
        if projected.get("lastError").is_some() || last_error.is_some() {
            projected["lastError"] = json!(last_error);
        }
        if !persist || projected == stored.0 {
            return TaskRecord::try_from(projected);
        }
        projected["updatedAt"] = json!(timestamp());
        let record = TaskRecord::try_from(projected)?;
        let mut candidate = locked.clone();
        candidate
            .tasks
            .insert(text(&record, "idempotencyKey").into(), record.clone());
        self.persist(&candidate)?;
        *locked = candidate;
        self.audit(
            "task.update",
            Some(task_ref),
            Some("task.start"),
            "completed",
            json!({
                "taskRef": task_ref, "stage": record["stage"], "source": "child_receipts",
            }),
        );
        Ok(record)
    }
    pub fn get_failure_by_turn_id(&self, thread: &str, turn: &str) -> Result<Option<Value>> {
        let state = self.state()?;
        if let Some(row) = state.thread_failures.get(&failure_key(thread, turn)) {
            return Ok(non_null(row.get("failure")).cloned());
        }
        Ok(state
            .entries
            .values()
            .find(|row| {
                row["kind"] == "thread.turn.start"
                    && row["threadId"] == thread
                    && row["turnId"] == turn
                    && row["terminalRunStatus"] == "failed"
                    && row["terminalFailure"]["turnId"] == turn
            })
            .and_then(|row| row.get("terminalFailure"))
            .cloned())
    }
    pub fn get_failure_evidence_order(
        &self,
        thread: &str,
        turn: &str,
    ) -> Result<Option<FailureEvidenceOrder>> {
        Ok(self
            .state()?
            .thread_failures
            .get(&failure_key(thread, turn))
            .and_then(|row| row.get("order"))
            .cloned()
            .map(FailureEvidenceOrder))
    }
    pub fn order_failure_evidence(
        &self,
        thread: &str,
        turn: Option<&str>,
        order: FailureEvidenceOrder,
    ) -> Result<FailureEvidenceOrder> {
        let state = self.state()?;
        let retained = turn
            .filter(|turn| !turn.is_empty())
            .and_then(|turn| state.thread_failures.get(&failure_key(thread, turn)))
            .and_then(|row| row.get("order"));
        Ok(FailureEvidenceOrder(order_with_era(order.0, retained)))
    }
    pub fn get_recovery_by_turn_id(
        &self,
        thread: &str,
        turn: &str,
    ) -> Result<Option<RecoveryState>> {
        Ok(self
            .state()?
            .thread_failures
            .get(&failure_key(thread, turn))
            .and_then(|row| match row["recoveredState"].as_str() {
                Some("completed") => Some(RecoveryState::Completed),
                Some("interrupted") => Some(RecoveryState::Interrupted),
                _ => None,
            }))
    }
    pub fn retain_terminal_failure(
        &self,
        thread: &str,
        turn: &str,
        candidate: Value,
        operation_id: Option<&str>,
        order: Option<FailureEvidenceOrder>,
    ) -> Result<Option<Value>> {
        self.retain_terminal_failures(&[TerminalFailure {
            thread_id: thread.into(),
            turn_id: turn.into(),
            failure: candidate,
            operation_id: operation_id.map(str::to_owned),
            order,
        }])
        .map(|mut results| results.pop().flatten())
    }
    pub fn retain_terminal_failures(
        &self,
        failures: &[TerminalFailure],
    ) -> Result<Vec<Option<Value>>> {
        let mut locked = self.state()?;
        let mut pending_failures = IndexMap::<String, Value>::new();
        let mut pending_operations = IndexMap::<String, OperationRecord>::new();
        let mut changed = false;
        let mut results = Vec::with_capacity(failures.len());
        for observation in failures {
            let thread = observation.thread_id.as_str();
            let turn = observation.turn_id.as_str();
            let candidate = &observation.failure;
            if !valid_failure(candidate) || candidate["turnId"] != turn {
                return Err(invalid("A terminal failure must match its turn."));
            }
            let key = failure_key(thread, turn);
            let operation = observation
                .operation_id
                .as_deref()
                .and_then(|id| {
                    locked
                        .entries
                        .iter()
                        .map(|(key, row)| pending_operations.get(key).unwrap_or(row))
                        .find(|row| row["operationId"] == id)
                })
                .cloned();
            if operation.as_ref().is_some_and(|row| {
                row["threadId"] != thread || row.get("turnId").is_some_and(|id| id != turn)
            }) {
                return Err(invalid(
                    "A terminal failure does not match the run operation.",
                ));
            }
            let retained = pending_failures
                .get(&key)
                .or_else(|| locked.thread_failures.get(&key));
            let retained_order = retained.and_then(|row| row.get("order"));
            let order = observation
                .order
                .as_ref()
                .map(|order| order_with_era(order.0.clone(), retained_order));
            let comparison = order
                .as_ref()
                .zip(retained_order)
                .map(|(a, b)| compare_order(a, b));
            if retained.is_some_and(|row| row.get("recoveredState").is_some())
                && (order.is_none() || comparison.is_some_and(|cmp| cmp <= Ordering::Equal))
            {
                results.push(None);
                continue;
            }
            let existing = retained
                .and_then(|row| non_null(row.get("failure")))
                .or_else(|| {
                    operation
                        .as_ref()
                        .and_then(|row| row.get("terminalFailure"))
                })
                .or_else(|| {
                    locked
                        .entries
                        .iter()
                        .map(|(key, row)| pending_operations.get(key).unwrap_or(row))
                        .find(|row| {
                            row["kind"] == "thread.turn.start"
                                && row["threadId"] == thread
                                && row["turnId"] == turn
                                && row["terminalFailure"]["turnId"] == turn
                        })
                        .and_then(|row| row.get("terminalFailure"))
                });
            let stale =
                retained_order.is_some() && (order.is_none() || comparison == Some(Ordering::Less));
            let same_reason = order
                .as_ref()
                .and_then(|o| o.get("failureIdentity"))
                .is_some_and(|id| {
                    Some(id) == retained_order.and_then(|o| o.get("failureIdentity"))
                });
            let newer_protocol = comparison == Some(Ordering::Greater)
                && order.as_ref().map(|o| &o["protocolVersion"])
                    != retained_order.map(|o| &o["protocolVersion"]);
            let source = text(candidate, "source");
            let later_v1 = comparison == Some(Ordering::Greater)
                && existing.is_some_and(|old| old["source"] == candidate["source"])
                && (matches!(source, "t3_activity" | "t3_message")
                    || (source == "t3_session"
                        && (order.as_ref().is_some_and(|o| o["scope"] == "full")
                            || !same_reason
                            || existing.and_then(|o| o.get("class")) != candidate.get("class"))));
            let v2 = order.as_ref().is_some_and(|o| o["protocolVersion"] == 2);
            let shell = order.as_ref().is_some_and(|o| o["scope"] == "shell");
            let matching_shell = shell
                && existing.is_some_and(|old| {
                    (v2 || (source == "t3_session" && old["source"] == "t3_session"))
                        && same_reason
                        && same_fields(old, candidate, &["message", "class"])
                });
            let unbound_shell = v2 && shell && source == "t3_turn" && existing.is_some();
            let mut merged = if let Some(old) = existing.filter(|_| stale || unbound_shell) {
                old.clone()
            } else if matching_shell {
                merge_metadata(existing.unwrap(), candidate, &["resetAt", "retryAfter"])
            } else if v2 || later_v1 || newer_protocol {
                if comparison == Some(Ordering::Equal)
                    && same_reason
                    && existing
                        .is_some_and(|old| same_fields(old, candidate, &["message", "class"]))
                {
                    merge_metadata(candidate, existing.unwrap(), &["resetAt", "retryAfter"])
                } else {
                    candidate.clone()
                }
            } else {
                merge_terminal_failure(existing, candidate, same_reason)
            };
            let mut candidate_order = order.clone();
            let chose_existing = (stale || unbound_shell) && existing.is_some()
                || (!matching_shell
                    && !(v2 || later_v1 || newer_protocol)
                    && terminal_prefers_existing(existing, candidate)
                    && !(same_reason
                        && existing.is_some_and(|old| {
                            same_fields(old, candidate, &["message", "class"])
                        })));
            if chose_existing && let Some(o) = &mut candidate_order {
                if let Some(id) = retained_order.and_then(|old| old.get("failureIdentity")) {
                    o["failureIdentity"] = id.clone();
                } else {
                    o.as_object_mut().unwrap().shift_remove("failureIdentity");
                }
            }
            if !stale
                && v2
                && shell
                && let Some(old) = existing
            {
                merged["provider"] = old["provider"].clone();
                merged["model"] = old["model"].clone();
            }
            let reason_is_candidate = !stale
                && (unbound_shell
                    || matching_shell
                    || same_fields(&merged, candidate, &["source", "message", "category"]));
            let mut next_order = if reason_is_candidate {
                candidate_order.or_else(|| retained_order.cloned())
            } else {
                retained_order.cloned()
            };
            let run_identity = order
                .as_ref()
                .and_then(|o| o.get("runIdentity"))
                .or_else(|| retained_order.and_then(|o| o.get("runIdentity")));
            if let Some(identity) = run_identity {
                if let Some(next) = &mut next_order {
                    next["runIdentity"] = identity.clone();
                }
                merged
                    .as_object_mut()
                    .unwrap()
                    .extend(identity.as_object().unwrap().clone());
            }
            if let Some(next) = &mut next_order
                && let Some(old) = retained_order
                && same_era(next, old)
            {
                let shell_admission = if matching_shell {
                    order
                        .as_ref()
                        .filter(|o| same_era(o, next))
                        .map_or(0.0, |o| number(o, "readStartedAt"))
                } else {
                    0.0
                };
                next["readStartedAt"] = json!(
                    number(next, "readStartedAt")
                        .max(number(old, "readStartedAt"))
                        .max(shell_admission)
                );
            }
            if next_order
                .as_ref()
                .is_none_or(|o| o["protocolVersion"] != 2)
                && merged["source"] != "t3_v2_turn_item"
                && let Some(model) = operation
                    .as_ref()
                    .and_then(|o| o.pointer("/settings/resolved/modelSelection"))
            {
                merged["model"] = json!(sanitize_identifier(text(model, "model")));
                let provider = model
                    .get("provider")
                    .or_else(|| model.get("instanceId"))
                    .or_else(|| non_null(merged.get("provider")))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let provider = sanitize_identifier(provider);
                merged["provider"] = if provider.is_empty() {
                    Value::Null
                } else {
                    json!(provider)
                };
            }
            let failure_changed = existing.is_none_or(|old| !same_json(old, &merged));
            let order_changed = !same_persisted_order(retained_order, next_order.as_ref());
            let operation_changed = operation.as_ref().is_some_and(|o| {
                o["terminalRunStatus"] != "failed"
                    || o["turnId"] != turn
                    || !same_json(&o["terminalFailure"], &merged)
            });
            let mut row = json!({"threadId":thread,"turnId":turn,"failure":merged});
            if let Some(order) = next_order {
                row["order"] = order;
            }
            pending_failures.insert(key, row);
            if failure_changed || order_changed || operation_changed {
                changed = true;
                if let Some(mut operation) = operation {
                    operation.0["turnId"] = json!(turn);
                    operation.0["terminalRunStatus"] = json!("failed");
                    operation.0["terminalFailure"] = merged.clone();
                    operation.0["updatedAt"] = json!(timestamp());
                    pending_operations.insert(text(&operation, "idempotencyKey").into(), operation);
                }
            }
            results.push(Some(merged));
        }
        if changed {
            let mut state = locked.clone();
            state.thread_failures.extend(pending_failures);
            state.entries.extend(pending_operations);
            self.persist(&state)?;
            *locked = state;
        } else {
            locked.thread_failures.extend(pending_failures);
        }
        Ok(results)
    }
    pub fn clear_terminal_failure(
        &self,
        thread: &str,
        turn: &str,
        recovery: RecoveryState,
        order: Option<FailureEvidenceOrder>,
    ) -> Result<()> {
        self.clear_terminal_failures(&[TerminalRecovery {
            thread_id: thread.into(),
            turn_id: turn.into(),
            state: recovery,
            order,
        }])
    }
    pub fn clear_terminal_failures(&self, recoveries: &[TerminalRecovery]) -> Result<()> {
        let mut locked = self.state()?;
        let mut pending = IndexMap::<String, Value>::new();
        let mut cleared = std::collections::HashSet::new();
        for recovery in recoveries {
            let key = failure_key(&recovery.thread_id, &recovery.turn_id);
            let retained = pending
                .get(&key)
                .or_else(|| locked.thread_failures.get(&key));
            let old_order = retained.and_then(|o| o.get("order"));
            let mut order = recovery
                .order
                .as_ref()
                .map(|o| order_with_era(o.0.clone(), old_order));
            if let Some(old) = old_order
                && order
                    .as_ref()
                    .is_none_or(|o| compare_order(o, old) == Ordering::Less)
            {
                continue;
            }
            if let Some(order) = &mut order
                && let Some(old) = old_order
                && same_era(order, old)
            {
                order["readStartedAt"] =
                    json!(number(order, "readStartedAt").max(number(old, "readStartedAt")));
            }
            let mut tombstone = json!({"threadId":recovery.thread_id,"turnId":recovery.turn_id,"failure":null,"recoveredState":recovery.state});
            if let Some(order) = &order {
                tombstone["order"] = order.clone();
            }
            let unchanged = retained.is_some_and(|o| {
                o["failure"].is_null()
                    && o["recoveredState"] == tombstone["recoveredState"]
                    && same_persisted_order(old_order, order.as_ref())
            });
            pending.insert(key.clone(), tombstone);
            if !unchanged {
                cleared.insert(key);
            }
        }
        if cleared.is_empty() {
            locked.thread_failures.extend(pending);
            return Ok(());
        }
        let mut state = locked.clone();
        state.thread_failures.extend(pending);
        for entry in state.entries.values_mut() {
            if entry["kind"] != "thread.turn.start"
                || !cleared.contains(&failure_key(text(entry, "threadId"), text(entry, "turnId")))
                || (entry.get("terminalRunStatus").is_none()
                    && entry.get("terminalFailure").is_none())
            {
                continue;
            }
            let row = entry.0.as_object_mut().unwrap();
            row.shift_remove("terminalRunStatus");
            row.shift_remove("terminalFailure");
            row.insert("updatedAt".into(), json!(timestamp()));
        }
        self.persist(&state)?;
        *locked = state;
        Ok(())
    }
    fn persist(&self, state: &JournalState) -> Result<()> {
        let name = self
            .file_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("journal");
        let temp = self
            .parent()
            .join(format!(".{name}.{}.tmp", Uuid::new_v4()));
        let mut payload = json!({"version":2,"operations":state.entries.values().collect::<Vec<_>>(),"tasks":state.tasks.values().collect::<Vec<_>>(),"threadFailures":state.thread_failures.values().collect::<Vec<_>>()});
        normalize_json(&mut payload);
        let mut bytes = serde_json::to_vec_pretty(&payload).unwrap();
        bytes.push(b'\n');
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        let written = file.write_all(&bytes);
        drop(file);
        let result = written.and_then(|()| fs::rename(&temp, &self.file_path));
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result.map_err(JournalError::Io)
    }
    fn audit(
        &self,
        event: &str,
        correlation: Option<&str>,
        operation: Option<&str>,
        outcome: &str,
        details: impl Into<AuditValue>,
    ) {
        if let Some(log) = &self.audit_log {
            let mut input = AuditEventInput::new(AuditSource::Journal, event);
            input.correlation_id = correlation.map(str::to_owned);
            input.operation = operation.map(str::to_owned);
            input.outcome = Some(outcome.into());
            input.details = Some(details.into());
            log.record(input);
        }
    }
}
fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}
fn text<'a>(value: &'a Value, field: &str) -> &'a str {
    value[field].as_str().unwrap_or("")
}
fn number(value: &Value, field: &str) -> f64 {
    value[field].as_f64().unwrap_or(0.0)
}
fn non_null(value: Option<&Value>) -> Option<&Value> {
    value.filter(|v| !v.is_null())
}
fn failure_key(thread: &str, turn: &str) -> String {
    format!("{thread}\0{turn}")
}
fn copy_field(target: &mut Value, source: &Value, field: &str) {
    if let Some(value) = source.get(field) {
        target[field] = value.clone();
    }
}
fn apply_patch(target: &mut Value, patch: &Value) {
    target
        .as_object_mut()
        .unwrap()
        .extend(patch.as_object().unwrap().clone());
    target["updatedAt"] = json!(timestamp());
}
fn same_fields(a: &Value, b: &Value, fields: &[&str]) -> bool {
    fields.iter().all(|field| a.get(field) == b.get(field))
}
fn same_json(a: &Value, b: &Value) -> bool {
    let mut a = a.clone();
    let mut b = b.clone();
    normalize_json(&mut a);
    normalize_json(&mut b);
    serde_json::to_vec(&a).unwrap() == serde_json::to_vec(&b).unwrap()
}
fn merge_metadata(preferred: &Value, other: &Value, fields: &[&str]) -> Value {
    let mut merged = preferred.clone();
    for field in fields {
        merged[*field] = non_null(preferred.get(*field))
            .or_else(|| other.get(*field))
            .cloned()
            .unwrap_or(Value::Null);
    }
    merged
}
fn source_priority(value: &Value) -> usize {
    [
        "t3_turn",
        "t3_session",
        "t3_activity",
        "t3_message",
        "t3_v2_turn_item",
    ]
    .iter()
    .position(|s| value["source"] == *s)
    .unwrap()
}
fn terminal_prefers_existing(existing: Option<&Value>, candidate: &Value) -> bool {
    existing.is_some_and(|old| {
        candidate["source"] != "t3_v2_turn_item"
            && !((old["source"] != "t3_v2_turn_item"
                && old["category"] == "unknown"
                && candidate["category"] != "unknown")
                || (!(candidate["category"] == "unknown" && old["category"] != "unknown")
                    && source_priority(candidate) > source_priority(old)))
    })
}
fn merge_terminal_failure(existing: Option<&Value>, candidate: &Value, same_reason: bool) -> Value {
    let Some(existing) = existing else {
        return candidate.clone();
    };
    if candidate["source"] == "t3_v2_turn_item" {
        return candidate.clone();
    }
    let (preferred, other) = if terminal_prefers_existing(Some(existing), candidate) {
        (existing, candidate)
    } else {
        (candidate, existing)
    };
    if !same_reason || !same_fields(preferred, other, &["message", "class"]) {
        return preferred.clone();
    }
    let mut merged = merge_metadata(
        preferred,
        other,
        &[
            "class",
            "retryable",
            "retry",
            "code",
            "provider",
            "resetAt",
            "retryAfter",
        ],
    );
    if preferred["category"] == "unknown" && preferred["source"] == other["source"] {
        merged["category"] = other["category"].clone();
    }
    merged
}
fn order_with_era(mut order: Value, existing: Option<&Value>) -> Value {
    if let Some(existing) = existing
        && order.get("readStartedAt").is_some()
    {
        let boundary = number(existing, "protocolStartedAt");
        let admission = number(&order, "readStartedAt");
        if admission >= boundary {
            let started = if order["protocolVersion"] == existing["protocolVersion"] {
                boundary
            } else if admission > number(existing, "readStartedAt") {
                admission
            } else {
                0.0
            };
            if started != 0.0 {
                order["protocolStartedAt"] = json!(started);
            }
        }
    }
    order
}
fn same_era(a: &Value, b: &Value) -> bool {
    a["protocolVersion"] == b["protocolVersion"]
        && number(a, "protocolStartedAt") == number(b, "protocolStartedAt")
}
fn compare_order(a: &Value, b: &Value) -> Ordering {
    if a.get("readStartedAt").is_some()
        && number(a, "readStartedAt") < number(b, "protocolStartedAt")
    {
        return Ordering::Less;
    }
    if b.get("readStartedAt").is_some()
        && number(b, "readStartedAt") < number(a, "protocolStartedAt")
    {
        return Ordering::Greater;
    }
    if a["protocolVersion"] != b["protocolVersion"] {
        return number(a, "readStartedAt").total_cmp(&number(b, "readStartedAt"));
    }
    number(a, "snapshotSequence")
        .total_cmp(&number(b, "snapshotSequence"))
        .then_with(|| match (a.get("updatedAt"), b.get("updatedAt")) {
            (Some(a), Some(b)) => {
                parse_timestamp(a.as_str().unwrap()).cmp(&parse_timestamp(b.as_str().unwrap()))
            }
            _ => Ordering::Equal,
        })
        .then_with(|| (a["scope"] == "full").cmp(&(b["scope"] == "full")))
        .then_with(|| match (a.get("item"), b.get("item")) {
            (Some(a), Some(b)) => parse_timestamp(text(a, "updatedAt"))
                .cmp(&parse_timestamp(text(b, "updatedAt")))
                .then_with(|| number(a, "ordinal").total_cmp(&number(b, "ordinal")))
                .then_with(|| {
                    text(a, "id")
                        .encode_utf16()
                        .cmp(text(b, "id").encode_utf16())
                }),
            _ => Ordering::Equal,
        })
}
fn same_persisted_order(a: Option<&Value>, b: Option<&Value>) -> bool {
    let strip = |value: Option<&Value>| {
        let mut v = value.cloned().unwrap_or_else(|| json!({}));
        v.as_object_mut().unwrap().shift_remove("readStartedAt");
        v
    };
    same_json(&strip(a), &strip(b))
}
fn sanitize_identifier(value: &str) -> String {
    let mut units = 0;
    value
        .chars()
        .map(|c| {
            if c <= '\u{1f}' || c == '\u{7f}' {
                ' '
            } else {
                c
            }
        })
        .take_while(|c| {
            units += c.len_utf16();
            units <= 200
        })
        .collect()
}
pub fn hash_payload(value: &Value) -> String {
    fn stable(value: &Value) -> String {
        match value {
            Value::Array(items) => format!(
                "[{}]",
                items.iter().map(stable).collect::<Vec<_>>().join(",")
            ),
            Value::Object(fields) => {
                let mut keys: Vec<_> = fields.keys().collect();
                keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
                format!(
                    "{{{}}}",
                    keys.iter()
                        .map(|key| format!(
                            "{}:{}",
                            serde_json::to_string(key).unwrap(),
                            stable(&fields[*key])
                        ))
                        .collect::<Vec<_>>()
                        .join(",")
                )
            }
            _ => {
                let mut normalized = value.clone();
                normalize_json(&mut normalized);
                normalized.to_string()
            }
        }
    }
    format!("{:x}", Sha256::digest(stable(value).as_bytes()))
}
fn strings(value: &Value, fields: &[&str]) -> bool {
    fields
        .iter()
        .all(|field| value.get(*field).is_some_and(Value::is_string))
}
fn one_of(value: &Value, field: &str, choices: &[&str]) -> bool {
    value[field].as_str().is_some_and(|v| choices.contains(&v))
}
fn nullable_string(value: Option<&Value>) -> bool {
    value.is_some_and(|v| v.is_null() || v.is_string())
}
fn optional(value: &Value, field: &str, validate: impl FnOnce(&Value) -> bool) -> bool {
    value.get(field).is_none_or(validate)
}
fn integer(value: &Value, min: f64) -> bool {
    value.as_f64().is_some_and(|n| {
        n.is_finite() && n >= min && n.fract() == 0.0 && n <= 9_007_199_254_740_991.0
    })
}
fn valid_retry(value: &Value) -> bool {
    value.is_object()
        && integer(&value["attempt"], 1.0)
        && ["maxAttempts", "retryDelayMs"].iter().all(|field| {
            value.get(*field).is_some_and(|v| {
                v.is_null() || integer(v, if *field == "maxAttempts" { 1.0 } else { 0.0 })
            })
        })
}
fn valid_failure(value: &Value) -> bool {
    value.is_object()
        && one_of(
            value,
            "category",
            &[
                "quota",
                "rate_limit",
                "auth_billing",
                "provider_internal",
                "provider_error",
                "unknown",
            ],
        )
        && optional(value, "class", |v| nullable_string(Some(v)))
        && optional(value, "retryable", |v| v.is_null() || v.is_boolean())
        && optional(value, "retry", |v| v.is_null() || valid_retry(v))
        && strings(value, &["message", "model"])
        && ["code", "provider", "turnId", "resetAt", "retryAfter"]
            .iter()
            .all(|f| nullable_string(value.get(*f)))
        && one_of(
            value,
            "source",
            &[
                "t3_session",
                "t3_turn",
                "t3_activity",
                "t3_message",
                "t3_v2_turn_item",
            ],
        )
}
fn valid_operation(value: &Value) -> bool {
    value.is_object()
        && strings(
            value,
            &[
                "operationId",
                "kind",
                "idempotencyKey",
                "payloadHash",
                "commandId",
                "createdAt",
                "updatedAt",
            ],
        )
        && one_of(
            value,
            "status",
            &["prepared", "accepted", "uncertain", "rejected"],
        )
        && optional(value, "settings", |v| {
            serde_json::from_value::<SettingsReceipt>(v.clone()).is_ok()
        })
        && if let Some(status) = value.get("terminalRunStatus") {
            status == "failed"
                && value["turnId"].is_string()
                && valid_failure(&value["terminalFailure"])
                && value["terminalFailure"]["turnId"] == value["turnId"]
        } else {
            value.get("terminalFailure").is_none()
        }
}
fn valid_task(value: &Value) -> bool {
    value.is_object()
        && strings(
            value,
            &[
                "taskRef",
                "idempotencyKey",
                "payloadHash",
                "projectId",
                "title",
                "threadIdempotencyKey",
                "runIdempotencyKey",
                "createdAt",
                "updatedAt",
            ],
        )
        && one_of(
            value,
            "runtimeMode",
            &[
                "approval-required",
                "auto-accept-edits",
                "auto",
                "full-access",
            ],
        )
        && [
            "threadId",
            "runId",
            "messageId",
            "threadOperationId",
            "runOperationId",
            "baselineRevision",
        ]
        .iter()
        .all(|field| optional(value, field, Value::is_string))
        && optional(value, "baselineAttribution", |_| {
            one_of(
                value,
                "baselineAttribution",
                &["clean", "dirty", "unavailable"],
            )
        })
        && optional(value, "lastError", |v| nullable_string(Some(v)))
        && one_of(
            value,
            "stage",
            &[
                "prepared",
                "thread_create_uncertain",
                "thread_created",
                "dispatch_rejected",
                "dispatch_uncertain",
                "run_accepted",
                "rejected",
            ],
        )
}
fn valid_order(value: &Value) -> bool {
    let date = |v: &Value| {
        v.as_str().is_some_and(|v| {
            v.as_bytes().get(10) == Some(&b'T')
                && (v.ends_with('Z') || v.rfind(['+', '-']).is_some_and(|i| i > 10))
                && chrono::DateTime::parse_from_rfc3339(v)
                    .is_ok_and(|date| date.timestamp_subsec_nanos() < 1_000_000_000)
        })
    };
    value.is_object()
        && (value["protocolVersion"] == 1 || value["protocolVersion"] == 2)
        && one_of(value, "scope", &["full", "shell"])
        && integer(&value["snapshotSequence"], 0.0)
        && ["readStartedAt", "protocolStartedAt"]
            .iter()
            .all(|field| optional(value, field, |v| integer(v, 0.0)))
        && optional(value, "failureIdentity", |v| {
            v.as_str().is_some_and(|s| {
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        })
        && optional(value, "updatedAt", date)
        && optional(value, "runIdentity", |v| {
            v.is_object() && nullable_string(v.get("provider")) && v["model"].is_string()
        })
        && optional(value, "item", |v| {
            v.is_object()
                && date(&v["updatedAt"])
                && integer(&v["ordinal"], 0.0)
                && v["id"].as_str().is_some_and(|s| !s.is_empty())
        })
}
fn valid_thread_failure(value: &Value) -> bool {
    value.is_object()
        && strings(value, &["threadId", "turnId"])
        && optional(value, "order", valid_order)
        && if value.get("failure") == Some(&Value::Null) {
            one_of(value, "recoveredState", &["completed", "interrupted"])
        } else {
            valid_failure(&value["failure"])
                && value["failure"]["turnId"] == value["turnId"]
                && value.get("recoveredState").is_none()
        }
}

#[cfg(test)]
#[path = "journal_tests.rs"]
mod tests;
