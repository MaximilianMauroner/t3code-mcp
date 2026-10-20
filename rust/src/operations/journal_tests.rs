use super::*;
use std::sync::Barrier;

#[cfg(unix)]
#[path = "journal_batch_tests.rs"]
mod batch;

struct Fixture {
    directory: PathBuf,
    path: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("t3-journal-{}", Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("operations.json");
        Self { directory, path }
    }
    fn journal(&self) -> OperationJournal {
        OperationJournal::new(&self.path, None)
    }
    fn write(&self, value: Value) {
        fs::write(&self.path, value.to_string()).unwrap();
    }
    fn read(&self) -> Value {
        serde_json::from_slice(&fs::read(&self.path).unwrap()).unwrap()
    }
    fn block_writes(&self) -> WriteBlock {
        let backup = self.directory.join("operations.saved");
        let saved = self.path.exists();
        if saved {
            fs::rename(&self.path, &backup).unwrap();
        }
        fs::create_dir(&self.path).unwrap();
        WriteBlock {
            path: self.path.clone(),
            backup: saved.then_some(backup),
        }
    }
}

struct WriteBlock {
    path: PathBuf,
    backup: Option<PathBuf>,
}

impl Drop for WriteBlock {
    fn drop(&mut self) {
        fs::remove_dir(&self.path).unwrap();
        if let Some(backup) = &self.backup {
            fs::rename(backup, &self.path).unwrap();
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.directory).unwrap();
    }
}
fn input(key: &str) -> BeginOperationInput {
    BeginOperationInput {
        kind: WritableOperationKind::ThreadTurnStart,
        idempotency_key: key.into(),
        payload_hash: hash_payload(&json!({"threadId":"thread-1","message":"hello"})),
        command_id: None,
        project_id: None,
        thread_id: Some("thread-1".into()),
        run_id: Some("run-1".into()),
        message_id: Some("message-1".into()),
        turn_id: Some("turn-1".into()),
        settings: None,
    }
}
fn task_input(key: &str) -> BeginTaskInput {
    BeginTaskInput {
        idempotency_key: key.into(),
        payload_hash: hash_payload(&json!({"instruction":"not persisted"})),
        project_id: "project-1".into(),
        title: "Legacy upgrade".into(),
        runtime_mode: RuntimeMode::ApprovalRequired,
    }
}
fn failure(source: &str, category: &str, message: &str) -> Value {
    json!({"category":category,"class":null,"code":null,"message":message,"provider":"provider-1","model":"model-1","turnId":"turn-1","resetAt":null,"retryAfter":null,"retryable":null,"retry":null,"source":source})
}
fn order(protocol: u8, scope: &str, sequence: u64, admission: u64) -> FailureEvidenceOrder {
    FailureEvidenceOrder::try_from(json!({"protocolVersion":protocol,"scope":scope,"snapshotSequence":sequence,"readStartedAt":admission})).unwrap()
}
fn identified(mut order: FailureEvidenceOrder, identity: &str) -> FailureEvidenceOrder {
    order.0["failureIdentity"] = json!(hash_payload(&json!(identity)));
    order
}
fn retain(
    journal: &OperationJournal,
    candidate: Value,
    order: Option<FailureEvidenceOrder>,
) -> Option<Value> {
    journal
        .retain_terminal_failure("thread-1", "turn-1", candidate, None, order)
        .unwrap()
}
fn loaded_operation() -> Value {
    json!({"operationId":"op-1","kind":"thread.turn.start","idempotencyKey":"key-1","payloadHash":"hash","commandId":"command-1","status":"accepted","createdAt":"2026-01-01T00:00:00.000Z","updatedAt":"2026-01-01T00:00:00.000Z","threadId":"thread-1","turnId":"turn-1"})
}

#[test]
fn failed_begins_do_not_reserve_keys_or_authorize_dispatch() {
    let file = Fixture::new();
    let journal = file.journal();
    journal.init().unwrap();
    let blocked = file.block_writes();
    assert!(journal.begin(input("failed-operation")).is_err());
    assert!(journal.begin_task(task_input("failed-task")).is_err());
    assert!(
        journal
            .get_by_idempotency_key("failed-operation")
            .unwrap()
            .is_none()
    );
    assert!(journal.list_tasks().unwrap().is_empty());
    assert!(fs::read_dir(&file.directory).unwrap().all(|entry| {
        entry
            .unwrap()
            .path()
            .extension()
            .is_none_or(|extension| extension != "tmp")
    }));
    drop(blocked);
    assert!(!journal.begin(input("failed-operation")).unwrap().reused);
    assert!(
        !journal
            .begin_task(task_input("failed-task"))
            .unwrap()
            .reused
    );
}

#[test]
fn failed_updates_leave_durable_and_in_memory_receipts_unchanged() {
    let file = Fixture::new();
    let journal = file.journal();
    let operation = journal.begin(input("operation")).unwrap().record;
    let task = journal.begin_task(task_input("task")).unwrap().record;
    let before = file.read();
    let blocked = file.block_writes();
    assert!(
        journal
            .update(
                text(&operation, "operationId"),
                OperationPatch {
                    status: Some(OperationStatus::Accepted),
                    ..OperationPatch::default()
                }
            )
            .is_err()
    );
    assert!(
        journal
            .update_task(
                text(&task, "taskRef"),
                TaskPatch {
                    stage: Some(TaskStage::RunAccepted),
                    ..TaskPatch::default()
                }
            )
            .is_err()
    );
    assert_eq!(
        journal
            .get_by_idempotency_key("operation")
            .unwrap()
            .unwrap(),
        operation
    );
    assert_eq!(
        journal
            .get_task_by_ref(text(&task, "taskRef"))
            .unwrap()
            .unwrap(),
        task
    );
    drop(blocked);
    assert_eq!(file.read(), before);
}

#[test]
fn failed_failure_and_recovery_writes_do_not_change_the_reported_outcome() {
    let file = Fixture::new();
    let journal = file.journal();
    journal.begin(input("operation")).unwrap();
    let candidate = failure("t3_session", "quota", "usage limit");
    let blocked = file.block_writes();
    assert!(
        journal
            .retain_terminal_failure(
                "thread-1",
                "turn-1",
                candidate.clone(),
                None,
                Some(order(1, "full", 1, 1))
            )
            .is_err()
    );
    assert!(
        journal
            .get_failure_by_turn_id("thread-1", "turn-1")
            .unwrap()
            .is_none()
    );
    drop(blocked);
    let retained = retain(&journal, candidate, Some(order(1, "full", 1, 1))).unwrap();
    let blocked = file.block_writes();
    assert!(
        journal
            .clear_terminal_failure(
                "thread-1",
                "turn-1",
                RecoveryState::Completed,
                Some(order(1, "full", 2, 2))
            )
            .is_err()
    );
    assert_eq!(
        journal
            .get_failure_by_turn_id("thread-1", "turn-1")
            .unwrap(),
        Some(retained)
    );
    assert!(
        journal
            .get_recovery_by_turn_id("thread-1", "turn-1")
            .unwrap()
            .is_none()
    );
    drop(blocked);
}

#[test]
fn task_projection_uses_current_child_receipts_and_does_not_rewrite_unchanged_reads() {
    let file = Fixture::new();
    let journal = file.journal();
    let task = journal.begin_task(task_input("task")).unwrap().record;
    let run = journal.begin(input("task:turn")).unwrap().record;
    let snapshot_only = journal.refresh_task(text(&task, "taskRef"), false).unwrap();
    assert_eq!(snapshot_only["stage"], "dispatch_uncertain");
    assert_eq!(snapshot_only["runId"], "run-1");
    assert_eq!(
        journal
            .get_task_by_ref(text(&task, "taskRef"))
            .unwrap()
            .unwrap(),
        task
    );
    journal
        .update(
            text(&run, "operationId"),
            OperationPatch {
                status: Some(OperationStatus::Accepted),
                ..OperationPatch::default()
            },
        )
        .unwrap();
    let projected = journal.refresh_task(text(&task, "taskRef"), true).unwrap();
    assert_eq!(projected["stage"], "run_accepted");
    assert_eq!(projected["runOperationId"], run["operationId"]);
    let bytes = fs::read(&file.path).unwrap();
    let repeated = journal.refresh_task(text(&task, "taskRef"), true).unwrap();
    assert_eq!(repeated, projected);
    assert_eq!(fs::read(&file.path).unwrap(), bytes);
    let reloaded = file
        .journal()
        .get_task_by_ref(text(&task, "taskRef"))
        .unwrap()
        .unwrap();
    assert_eq!(reloaded, projected);
}

#[test]
fn identical_keys_reuse_command_and_conflicting_payload_or_kind_fails() {
    let file = Fixture::new();
    let journal = file.journal();
    let first = journal.begin(input("same-key")).unwrap();
    let second = journal.begin(input("same-key")).unwrap();
    assert!(!first.reused);
    assert!(second.reused);
    assert_eq!(first.record, second.record);
    let mut changed = input("same-key");
    changed.payload_hash = "different".into();
    assert!(matches!(
        journal.begin(changed),
        Err(JournalError::IdempotencyConflict(_))
    ));
    let mut changed = input("same-key");
    changed.kind = WritableOperationKind::ThreadCreate;
    assert!(matches!(
        journal.begin(changed),
        Err(JournalError::IdempotencyConflict(_))
    ));
    assert_eq!(file.read()["operations"].as_array().unwrap().len(), 1);
    assert!(text(&first.record, "operationId").starts_with("op_"));
}
#[test]
fn concurrent_begins_coalesce_and_every_return_has_a_persisted_record() {
    let file = Fixture::new();
    let journal = Arc::new(file.journal());
    let barrier = Arc::new(Barrier::new(20));
    let threads: Vec<_> = (0..20)
        .map(|_| {
            let journal = journal.clone();
            let barrier = barrier.clone();
            let path = file.path.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let result = journal.begin(input("concurrent-key")).unwrap();
                let disk: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
                assert_eq!(
                    disk["operations"][0]["operationId"],
                    result.record["operationId"]
                );
                result
            })
        })
        .collect();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.reused).count(), 19);
    assert!(
        results
            .iter()
            .all(|r| r.record["operationId"] == results[0].record["operationId"])
    );
}
#[test]
fn restart_recovers_only_prepared_operations_and_persists_recovery() {
    let file = Fixture::new();
    let journal = file.journal();
    let prepared = journal.begin(input("crash-key")).unwrap().record;
    for (key, status) in [
        ("accepted", OperationStatus::Accepted),
        ("uncertain", OperationStatus::Uncertain),
        ("rejected", OperationStatus::Rejected),
    ] {
        let row = journal.begin(input(key)).unwrap().record;
        journal
            .update(
                text(&row, "operationId"),
                OperationPatch {
                    status: Some(status),
                    ..Default::default()
                },
            )
            .unwrap();
    }
    let recovered = file
        .journal()
        .get_by_idempotency_key("crash-key")
        .unwrap()
        .unwrap();
    assert_eq!(recovered["status"], "uncertain");
    assert!(text(&recovered, "lastError").contains("stopped"));
    assert_eq!(recovered["commandId"], prepared["commandId"]);
    for key in ["accepted", "uncertain", "rejected"] {
        assert_eq!(
            file.journal().get_by_idempotency_key(key).unwrap().unwrap()["status"],
            key
        );
    }
    let bytes = fs::read(&file.path).unwrap();
    file.journal().init().unwrap();
    assert_eq!(bytes, fs::read(&file.path).unwrap());
}
#[test]
fn accepted_transitions_have_node_field_order_and_private_atomic_files() {
    let file = Fixture::new();
    let path = file.directory.join("nested/operations.json");
    let journal = OperationJournal::new(&path, None);
    let row = journal.begin(input("persist-key")).unwrap().record;
    journal
        .update(
            text(&row, "operationId"),
            OperationPatch {
                status: Some(OperationStatus::Accepted),
                t3_sequence: Some(42.0),
                ..Default::default()
            },
        )
        .unwrap();
    let loaded = OperationJournal::new(&path, None)
        .get_by_run_id("run-1")
        .unwrap()
        .unwrap();
    assert_eq!(loaded["status"], "accepted");
    assert_eq!(loaded["t3Sequence"], 42);
    assert_eq!(
        loaded
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec![
            "operationId",
            "kind",
            "idempotencyKey",
            "payloadHash",
            "commandId",
            "status",
            "createdAt",
            "updatedAt",
            "threadId",
            "runId",
            "messageId",
            "turnId",
            "t3Sequence"
        ]
    );
    let bytes = fs::read(&path).unwrap();
    assert_eq!(bytes.last(), Some(&b'\n'));
    assert!(
        String::from_utf8(bytes)
            .unwrap()
            .starts_with("{\n  \"version\": 2,\n  \"operations\": [")
    );
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
#[test]
fn failed_rename_keeps_the_old_file_and_the_write_queue_can_continue() {
    let file = Fixture::new();
    let journal = file.journal();
    let row = journal.begin(input("atomic-key")).unwrap().record;
    let saved = file.directory.join("saved.json");
    fs::rename(&file.path, &saved).unwrap();
    fs::create_dir(&file.path).unwrap();
    assert!(matches!(
        journal.update(
            text(&row, "operationId"),
            OperationPatch {
                status: Some(OperationStatus::Accepted),
                ..Default::default()
            }
        ),
        Err(JournalError::Io(_))
    ));
    let old: Value = serde_json::from_slice(&fs::read(&saved).unwrap()).unwrap();
    assert_eq!(old["operations"][0]["status"], "prepared");
    fs::remove_dir(&file.path).unwrap();
    fs::rename(saved, &file.path).unwrap();
    journal
        .update(
            text(&row, "operationId"),
            OperationPatch {
                status: Some(OperationStatus::Uncertain),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        file.journal()
            .get_by_idempotency_key("atomic-key")
            .unwrap()
            .unwrap()["status"],
        "uncertain"
    );
}
#[test]
fn readers_never_observe_partial_journals_during_replacements() {
    let file = Fixture::new();
    let journal = Arc::new(file.journal());
    let row = journal.begin(input("atomic-key")).unwrap().record;
    let id = text(&row, "operationId").to_owned();
    let writer = {
        let journal = journal.clone();
        std::thread::spawn(move || {
            for n in 0..80 {
                journal
                    .update(
                        &id,
                        OperationPatch {
                            t3_sequence: Some(n as f64),
                            ..Default::default()
                        },
                    )
                    .unwrap();
            }
        })
    };
    for _ in 0..160 {
        let value: Value = serde_json::from_slice(&fs::read(&file.path).unwrap()).unwrap();
        assert_eq!(value["operations"].as_array().unwrap().len(), 1);
    }
    writer.join().unwrap();
    assert_eq!(file.read()["operations"][0]["t3Sequence"], 79);
}
#[test]
fn canonical_payload_hash_matches_node_including_utf16_and_numbers() {
    assert_eq!(
        hash_payload(&json!({"a":1,"b":[2,3]})),
        hash_payload(&json!({"b":[2,3],"a":1}))
    );
    assert_ne!(
        hash_payload(&json!({"a":[1,2]})),
        hash_payload(&json!({"a":[2,1]}))
    );
    assert_eq!(
        hash_payload(&json!({"a":1,"b":[2,3]})),
        "efbd0040190fb0871831e606c581f8a66db79d8e2bb836745a70051306956070"
    );
    assert_eq!(hash_payload(&json!(1.0)), hash_payload(&json!(1)));
    assert_eq!(hash_payload(&json!(-0.0)), hash_payload(&json!(0)));
    let expected = "{\"10\":1,\"2\":2,\"𐀀\":3,\"\":4}";
    assert_eq!(
        hash_payload(&json!({"":4,"𐀀":3,"2":2,"10":1})),
        format!("{:x}", Sha256::digest(expected.as_bytes()))
    );
}
#[test]
fn version_one_and_unknown_legacy_kinds_preserve_fields_and_reserve_keys() {
    let file = Fixture::new();
    let mut legacy = loaded_operation();
    legacy["kind"] = json!("thread.snooze");
    legacy["snoozedUntil"] = json!("2026-01-02T08:00:00.000Z");
    legacy["futureField"] = json!({"extra":true});
    file.write(json!({"version":1,"operations":[legacy.clone()]}));
    let journal = file.journal();
    assert_eq!(
        journal.get_by_idempotency_key("key-1").unwrap().unwrap().0,
        legacy
    );
    assert!(matches!(
        journal.begin(input("key-1")),
        Err(JournalError::IdempotencyConflict(_))
    ));
    let task = journal.begin_task(task_input("legacy-upgrade")).unwrap();
    assert!(text(&task.record, "taskRef").starts_with("task_"));
    assert_eq!(file.read()["version"], 2);
    assert_eq!(file.read()["operations"][0], legacy);
    assert_eq!(file.read()["tasks"][0]["title"], "Legacy upgrade");
    assert_eq!(file.read()["threadFailures"], json!([]));
}
#[test]
fn tasks_reuse_keys_and_all_stages_settings_and_null_errors_survive_restart() {
    let file = Fixture::new();
    let journal = file.journal();
    let task = journal.begin_task(task_input("task-key")).unwrap().record;
    let task_ref = text(&task, "taskRef");
    assert!(journal.begin_task(task_input("task-key")).unwrap().reused);
    let mut changed = task_input("task-key");
    changed.payload_hash = "changed".into();
    assert!(matches!(
        journal.begin_task(changed),
        Err(JournalError::IdempotencyConflict(_))
    ));
    assert_eq!(task["threadIdempotencyKey"], "task-key:thread");
    assert_eq!(task["runIdempotencyKey"], "task-key:turn");
    for (stage, expected) in [
        (TaskStage::ThreadCreateUncertain, "thread_create_uncertain"),
        (TaskStage::ThreadCreated, "thread_created"),
        (TaskStage::DispatchRejected, "dispatch_rejected"),
        (TaskStage::DispatchUncertain, "dispatch_uncertain"),
        (TaskStage::RunAccepted, "run_accepted"),
        (TaskStage::Rejected, "rejected"),
        (TaskStage::Prepared, "prepared"),
    ] {
        journal
            .update_task(
                task_ref,
                TaskPatch {
                    stage: Some(stage),
                    thread_id: Some("thread-1".into()),
                    run_id: Some("run-1".into()),
                    message_id: Some("message-1".into()),
                    thread_operation_id: Some("op-thread".into()),
                    run_operation_id: Some("op-run".into()),
                    baseline_revision: Some("base".into()),
                    baseline_attribution: Some(BaselineAttribution::Dirty),
                    last_error: Some(None),
                },
            )
            .unwrap();
        let recovered = file.journal().get_task_by_ref(task_ref).unwrap().unwrap();
        assert_eq!(recovered["stage"], expected);
        assert_eq!(recovered["lastError"], Value::Null);
        assert_eq!(recovered["baselineAttribution"], "dirty");
        assert_eq!(recovered["runId"], "run-1");
    }
    assert_eq!(journal.list_tasks().unwrap().len(), 1);
    assert!(journal.get_task_by_ref("missing").unwrap().is_none());
    assert!(matches!(
        journal.update_task("missing", TaskPatch::default()),
        Err(JournalError::NotFound(_))
    ));
    assert!(matches!(
        journal.update("missing", OperationPatch::default()),
        Err(JournalError::NotFound(_))
    ));
    assert!(
        !fs::read_to_string(&file.path)
            .unwrap()
            .contains("not persisted")
    );
}
#[test]
fn malformed_files_fail_closed_without_replacing_bytes() {
    let file = Fixture::new();
    for raw in [
        "{",
        "null",
        "[]",
        "{}",
        "{\"operations\":{}}",
        "{\"operations\":[],\"tasks\":null}",
        "{\"operations\":[],\"threadFailures\":{}}",
    ] {
        fs::write(&file.path, raw).unwrap();
        assert!(file.journal().init().is_err(), "{raw}");
        assert_eq!(fs::read_to_string(&file.path).unwrap(), raw);
    }
    let mut bad_operations = vec![json!({"status":"accepted"})];
    for patch in [
        json!({"terminalRunStatus":"completed"}),
        json!({"terminalRunStatus":"failed","terminalFailure":{"message":"missing fields"}}),
        json!({"terminalFailure":failure("t3_turn","unknown","failed")}),
        json!({"terminalRunStatus":"failed","turnId":"other","terminalFailure":failure("t3_turn","unknown","failed")}),
        json!({"settings":{}}),
    ] {
        let mut row = loaded_operation();
        row.as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        bad_operations.push(row);
    }
    for row in bad_operations {
        file.write(json!({"operations":[row]}));
        assert!(
            file.journal()
                .init()
                .unwrap_err()
                .to_string()
                .contains("invalid operation")
        );
    }
    let task = file.directory.join("task.json");
    let journal = OperationJournal::new(&task, None);
    let task = journal.begin_task(task_input("bad")).unwrap().record.0;
    for patch in [
        json!({"threadId":42}),
        json!({"threadId":null}),
        json!({"lastError":42}),
        json!({"stage":"done"}),
        json!({"runtimeMode":"unsafe"}),
        json!({"baselineAttribution":null}),
    ] {
        let mut task = task.clone();
        task.as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        file.write(json!({"operations":[],"tasks":[task]}));
        assert!(
            file.journal()
                .init()
                .unwrap_err()
                .to_string()
                .contains("invalid task")
        );
    }
}
#[test]
fn failure_and_order_validation_rejects_malformed_nested_rows() {
    let file = Fixture::new();
    let good = failure("t3_session", "quota", "limit");
    let mut rows = vec![
        json!({"threadId":"thread-1","turnId":"turn-1","failure":{"message":"missing fields"}}),
        json!({"threadId":"thread-1","turnId":"turn-1","failure":null}),
        json!({"threadId":"thread-1","turnId":"other","failure":good}),
    ];
    for patch in [
        json!({"retry":{"attempt":0,"maxAttempts":null,"retryDelayMs":null}}),
        json!({"retry":{"attempt":1}}),
        json!({"retryable":0}),
        json!({"code":false}),
        json!({"source":"session"}),
        json!({"class":0}),
    ] {
        let mut failure = good.clone();
        failure
            .as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        rows.push(json!({"threadId":"thread-1","turnId":"turn-1","failure":failure}));
    }
    for bad in [
        json!({"protocolVersion":3,"scope":"full","snapshotSequence":1}),
        json!({"protocolVersion":2,"scope":"full","snapshotSequence":-1}),
        json!({"protocolVersion":2,"scope":"full","snapshotSequence":1,"readStartedAt":null}),
        json!({"protocolVersion":2,"scope":"full","snapshotSequence":1,"updatedAt":"yesterday"}),
        json!({"protocolVersion":2,"scope":"full","snapshotSequence":1,"failureIdentity":"not-a-hash"}),
        json!({"protocolVersion":2,"scope":"full","snapshotSequence":1,"runIdentity":{"provider":null}}),
    ] {
        rows.push(json!({"threadId":"thread-1","turnId":"turn-1","failure":good,"order":bad}));
    }
    for row in rows {
        file.write(json!({"operations":[],"threadFailures":[row]}));
        assert!(
            file.journal()
                .init()
                .unwrap_err()
                .to_string()
                .contains("invalid thread failure")
        );
    }
}
#[test]
fn duplicate_rows_keep_last_values_at_first_insertion_position_and_extra_fields() {
    let file = Fixture::new();
    let first = loaded_operation();
    let mut middle = first.clone();
    middle["idempotencyKey"] = json!("middle");
    let mut last = first.clone();
    last["operationId"] = json!("op-last");
    last["projectId"] = json!(42);
    last["extra"] = json!(true);
    file.write(json!({"operations":[first,middle,last.clone()]}));
    let journal = file.journal();
    journal.begin_task(task_input("flush")).unwrap();
    assert_eq!(file.read()["operations"][0], last);
    assert_eq!(file.read()["operations"].as_array().unwrap().len(), 2);
}
#[test]
fn retained_and_legacy_failures_survive_restart_and_attach_to_operations() {
    let file = Fixture::new();
    let journal = file.journal();
    let op = journal.begin(input("failure-key")).unwrap().record;
    let op_id = text(&op, "operationId");
    let failed = failure("t3_session", "quota", "limit");
    assert_eq!(
        journal
            .retain_terminal_failure("thread-1", "turn-1", failed.clone(), Some(op_id), None)
            .unwrap(),
        Some(failed.clone())
    );
    let restarted = file.journal();
    assert_eq!(
        restarted
            .get_failure_by_turn_id("thread-1", "turn-1")
            .unwrap(),
        Some(failed.clone())
    );
    let op = restarted.get_by_operation_id(op_id).unwrap().unwrap();
    assert_eq!(op["terminalRunStatus"], "failed");
    assert_eq!(op["terminalFailure"], failed);
    file.write(json!({"operations":[op]}));
    assert_eq!(
        file.journal()
            .get_failure_by_turn_id("thread-1", "turn-1")
            .unwrap(),
        Some(failed)
    );
    assert!(
        journal
            .retain_terminal_failure(
                "thread-1",
                "other",
                failure("t3_turn", "unknown", "x"),
                None,
                None
            )
            .is_err()
    );
    assert!(
        journal
            .retain_terminal_failure(
                "other",
                "turn-1",
                failure("t3_turn", "unknown", "x"),
                Some(op_id),
                None
            )
            .is_err()
    );
}
#[test]
fn stronger_known_reasons_win_over_generic_and_weaker_sources() {
    let file = Fixture::new();
    let journal = file.journal();
    retain(&journal, failure("t3_turn", "unknown", "fallback"), None);
    for source in ["t3_session", "t3_activity", "t3_message", "t3_v2_turn_item"] {
        let result = retain(&journal, failure(source, "quota", source), None).unwrap();
        assert_eq!(result["source"], source);
    }
    let v2 = retain(&journal, failure("t3_turn", "unknown", "generic"), None).unwrap();
    assert_eq!(v2["source"], "t3_v2_turn_item");
    assert_eq!(
        file.journal()
            .get_failure_by_turn_id("thread-1", "turn-1")
            .unwrap(),
        Some(v2)
    );
}
#[test]
fn matching_shell_keeps_precise_fields_and_advances_admission_without_write() {
    let file = Fixture::new();
    let journal = file.journal();
    let mut full = failure("t3_v2_turn_item", "quota", "limit");
    full["code"] = json!("usage_limit");
    full["retry"] = json!({"attempt":2,"maxAttempts":3,"retryDelayMs":500});
    retain(
        &journal,
        full.clone(),
        Some(identified(order(2, "full", 10, 100), "limit")),
    );
    let before = fs::read(&file.path).unwrap();
    assert_eq!(
        retain(
            &journal,
            failure("t3_session", "quota", "limit"),
            Some(identified(order(2, "shell", 10, 300), "limit"))
        ),
        Some(full.clone())
    );
    assert_eq!(
        retain(
            &journal,
            failure("t3_session", "quota", "old protocol"),
            Some(order(1, "full", 100, 200))
        ),
        Some(full)
    );
    assert_eq!(fs::read(&file.path).unwrap(), before);
    assert_eq!(
        journal
            .get_failure_evidence_order("thread-1", "turn-1")
            .unwrap()
            .unwrap()["readStartedAt"],
        300.0
    );
}
#[test]
fn changed_shell_and_redaction_collision_replace_reason_without_mixing_metadata() {
    let file = Fixture::new();
    let journal = file.journal();
    let mut full = failure("t3_v2_turn_item", "quota", "redacted");
    full["retryAfter"] = json!("100");
    full["code"] = json!("usage_limit");
    retain(
        &journal,
        full,
        Some(identified(order(2, "full", 1, 1), "raw one")),
    );
    let mut shell = failure("t3_session", "quota", "redacted");
    shell["provider"] = json!("changed");
    shell["model"] = json!("changed");
    let expected = failure("t3_session", "quota", "redacted");
    assert_eq!(
        retain(
            &journal,
            shell,
            Some(identified(order(2, "shell", 2, 2), "raw two"))
        ),
        Some(expected.clone())
    );
    assert_eq!(
        file.journal()
            .get_failure_by_turn_id("thread-1", "turn-1")
            .unwrap(),
        Some(expected)
    );
}
#[test]
fn unbound_shell_cannot_replace_a_reason_and_full_read_supplies_run_identity() {
    let file = Fixture::new();
    let journal = file.journal();
    retain(
        &journal,
        failure("t3_session", "quota", "new reason"),
        Some(order(2, "shell", 20, 20)),
    );
    let mut full_order = order(2, "full", 10, 10);
    full_order.0["runIdentity"] = json!({"provider":"run-provider","model":"run-model"});
    let result = retain(
        &journal,
        failure("t3_v2_turn_item", "quota", "old full"),
        Some(full_order),
    )
    .unwrap();
    assert_eq!(result["message"], "new reason");
    assert_eq!(result["model"], "run-model");
    let result = retain(
        &journal,
        failure("t3_turn", "unknown", "unbound"),
        Some(order(2, "shell", 30, 30)),
    )
    .unwrap();
    assert_eq!(result["message"], "new reason");
    assert_eq!(result["provider"], "run-provider");
    assert_eq!(
        file.journal()
            .get_failure_evidence_order("thread-1", "turn-1")
            .unwrap()
            .unwrap()["runIdentity"]["model"],
        "run-model"
    );
}
#[test]
fn recovery_tombstones_clear_operations_and_reject_stale_or_unordered_failures() {
    let file = Fixture::new();
    let journal = file.journal();
    let op = journal.begin(input("recover-key")).unwrap().record;
    journal
        .retain_terminal_failure(
            "thread-1",
            "turn-1",
            failure("t3_session", "quota", "limit"),
            Some(text(&op, "operationId")),
            Some(order(1, "full", 10, 10)),
        )
        .unwrap();
    journal
        .clear_terminal_failure(
            "thread-1",
            "turn-1",
            RecoveryState::Completed,
            Some(order(1, "full", 9, 11)),
        )
        .unwrap();
    assert!(
        journal
            .get_failure_by_turn_id("thread-1", "turn-1")
            .unwrap()
            .is_some()
    );
    journal
        .clear_terminal_failure(
            "thread-1",
            "turn-1",
            RecoveryState::Completed,
            Some(order(1, "full", 11, 12)),
        )
        .unwrap();
    let restarted = file.journal();
    restarted.init().unwrap();
    let bytes = fs::read(&file.path).unwrap();
    assert!(
        restarted
            .get_failure_by_turn_id("thread-1", "turn-1")
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        restarted
            .get_recovery_by_turn_id("thread-1", "turn-1")
            .unwrap(),
        Some(RecoveryState::Completed)
    ));
    let recovered = restarted.get_by_run_id("run-1").unwrap().unwrap();
    assert!(recovered.get("terminalFailure").is_none());
    assert!(recovered.get("terminalRunStatus").is_none());
    assert_eq!(
        retain(&restarted, failure("t3_session", "quota", "stale"), None),
        None
    );
    assert_eq!(
        retain(
            &restarted,
            failure("t3_session", "quota", "stale"),
            Some(order(1, "full", 11, 13))
        ),
        None
    );
    restarted
        .clear_terminal_failure(
            "thread-1",
            "turn-1",
            RecoveryState::Completed,
            Some(order(1, "full", 11, 14)),
        )
        .unwrap();
    assert_eq!(bytes, fs::read(&file.path).unwrap());
    assert!(
        retain(
            &restarted,
            failure("t3_session", "quota", "new"),
            Some(order(1, "full", 12, 15))
        )
        .is_some()
    );
}
#[test]
fn protocol_transitions_reject_delayed_reads_even_after_switching_back_and_restart() {
    for (first, middle) in [(1, 2), (2, 1)] {
        let file = Fixture::new();
        let journal = file.journal();
        retain(
            &journal,
            failure("t3_session", "quota", "first"),
            Some(order(first, "full", 100, 10)),
        );
        journal
            .clear_terminal_failure(
                "thread-1",
                "turn-1",
                RecoveryState::Interrupted,
                Some(order(middle, "shell", 1, 20)),
            )
            .unwrap();
        journal
            .clear_terminal_failure(
                "thread-1",
                "turn-1",
                RecoveryState::Interrupted,
                Some(order(first, "shell", 1, 30)),
            )
            .unwrap();
        let restarted = file.journal();
        assert_eq!(
            retain(
                &restarted,
                failure("t3_session", "quota", "delayed"),
                Some(order(first, "full", 200, 11))
            ),
            None
        );
        assert_eq!(
            restarted
                .get_failure_evidence_order("thread-1", "turn-1")
                .unwrap()
                .unwrap()["protocolStartedAt"],
            30
        );
        assert!(matches!(
            restarted
                .get_recovery_by_turn_id("thread-1", "turn-1")
                .unwrap(),
            Some(RecoveryState::Interrupted)
        ));
    }
}
#[test]
fn equal_snapshot_order_uses_timestamp_scope_item_time_ordinal_and_id() {
    let file = Fixture::new();
    let journal = file.journal();
    let mut a = order(2, "shell", 10, 10);
    a.0["updatedAt"] = json!("2026-10-02T20:00:00Z");
    retain(
        &journal,
        failure("t3_session", "quota", "shell"),
        Some(a.clone()),
    );
    let mut b = a.clone();
    b.0["scope"] = json!("full");
    b.0["item"] = json!({"updatedAt":"2026-10-02T20:00:00Z","ordinal":1,"id":"a"});
    retain(
        &journal,
        failure("t3_v2_turn_item", "quota", "full"),
        Some(b.clone()),
    );
    let mut c = b.clone();
    c.0["item"]["id"] = json!("b");
    assert_eq!(
        retain(
            &journal,
            failure("t3_v2_turn_item", "quota", "id wins"),
            Some(c.clone())
        )
        .unwrap()["message"],
        "id wins"
    );
    assert_eq!(
        retain(
            &journal,
            failure("t3_v2_turn_item", "quota", "stale id"),
            Some(b)
        )
        .unwrap()["message"],
        "id wins"
    );
    c.0["item"]["ordinal"] = json!(2);
    retain(
        &journal,
        failure("t3_v2_turn_item", "quota", "ordinal wins"),
        Some(c.clone()),
    );
    c.0["item"]["updatedAt"] = json!("2026-10-02T20:00:01Z");
    retain(
        &journal,
        failure("t3_v2_turn_item", "quota", "item time wins"),
        Some(c.clone()),
    );
    c.0["updatedAt"] = json!("2026-10-02T20:00:02Z");
    assert_eq!(
        retain(
            &journal,
            failure("t3_v2_turn_item", "quota", "time wins"),
            Some(c)
        )
        .unwrap()["message"],
        "time wins"
    );
}
#[test]
fn settings_receipt_is_safe_and_v1_failure_uses_admitted_settings() {
    let file = Fixture::new();
    let journal = file.journal();
    let mut op_input = input("settings");
    let settings: SettingsReceipt = serde_json::from_value(json!({"requested":{"modelSelection":null,"runtimeMode":null},"resolved":{"modelSelection":{"model":"vendor.token:0\nmodel","instanceId":"admitted-provider"},"runtimeMode":"auto"},"modelSource":"project_default","runtimeSource":"explicit"})).unwrap();
    op_input.settings = Some(settings.clone());
    let op = journal.begin(op_input).unwrap().record;
    let result = journal
        .retain_terminal_failure(
            "thread-1",
            "turn-1",
            failure("t3_session", "quota", "limit"),
            Some(text(&op, "operationId")),
            Some(order(1, "full", 1, 1)),
        )
        .unwrap()
        .unwrap();
    assert_eq!(result["model"], "vendor.token:0 model");
    assert_eq!(result["provider"], "admitted-provider");
    assert_eq!(
        file.journal().get_by_run_id("run-1").unwrap().unwrap()["settings"],
        serde_json::to_value(settings).unwrap()
    );
    let v2 = failure("t3_v2_turn_item", "quota", "v2");
    assert_eq!(
        journal
            .retain_terminal_failure(
                "thread-1",
                "turn-1",
                v2.clone(),
                Some(text(&op, "operationId")),
                Some(order(2, "full", 1, 2))
            )
            .unwrap(),
        Some(v2)
    );
}
#[test]
fn audit_events_match_node_names_outcomes_correlations_and_recovery_counts() {
    let file = Fixture::new();
    let audit = Arc::new(AuditLog::new(file.directory.join("audit.jsonl")));
    let journal = OperationJournal::new(&file.path, Some(audit.clone()));
    let op = journal.begin(input("audited")).unwrap().record;
    journal.begin(input("audited")).unwrap();
    journal
        .update(
            text(&op, "operationId"),
            OperationPatch {
                last_error: Some("api_key=private-value".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let task = journal
        .begin_task(task_input("audited-task"))
        .unwrap()
        .record;
    journal.begin_task(task_input("audited-task")).unwrap();
    journal
        .update_task(
            text(&task, "taskRef"),
            TaskPatch {
                stage: Some(TaskStage::ThreadCreated),
                ..Default::default()
            },
        )
        .unwrap();
    OperationJournal::new(&file.path, Some(audit.clone()))
        .init()
        .unwrap();
    assert!(audit.diagnostic_error().is_none());
    let rows: Vec<Value> = fs::read_to_string(&audit.file_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        rows.iter().map(|r| text(r, "event")).collect::<Vec<_>>(),
        vec![
            "journal.initialized",
            "operation.begin",
            "operation.begin",
            "operation.update",
            "task.begin",
            "task.begin",
            "task.update",
            "journal.initialized"
        ]
    );
    assert_eq!(rows[1]["outcome"], "prepared");
    assert_eq!(rows[2]["outcome"], "reused");
    assert_eq!(rows[1]["correlationId"], op["operationId"]);
    assert_eq!(rows[4]["operation"], "task.start");
    assert_eq!(rows[6]["correlationId"], task["taskRef"]);
    assert_eq!(rows[7]["details"]["recoveredPreparedOperations"], 1);
    assert_eq!(rows[7]["details"]["taskCount"], 1);
    assert!(
        !fs::read_to_string(&audit.file_path)
            .unwrap()
            .contains("private-value")
    );
}

#[test]
fn captured_node_oracle_matches_failure_traces_and_persisted_field_order() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/node-journal-trace.json"
    ))
    .unwrap();
    let file = Fixture::new();
    file.write(fixture["initial"].clone());
    let actions = fixture["actions"].as_array().unwrap();
    assert_eq!(actions.len(), 216);
    let mut expected = fixture["observations"].clone();
    let mut journal = file.journal();
    let mut trace = Vec::new();
    for action in actions {
        let result = match text(action, "type") {
            "retain" => journal
                .retain_terminal_failure(
                    "thread-1",
                    "turn-1",
                    action["candidate"].clone(),
                    action["operationId"].as_str(),
                    Some(FailureEvidenceOrder::try_from(action["order"].clone()).unwrap()),
                )
                .unwrap(),
            "clear" => {
                journal
                    .clear_terminal_failure(
                        "thread-1",
                        "turn-1",
                        if action["state"] == "completed" {
                            RecoveryState::Completed
                        } else {
                            RecoveryState::Interrupted
                        },
                        Some(FailureEvidenceOrder::try_from(action["order"].clone()).unwrap()),
                    )
                    .unwrap();
                None
            }
            _ => {
                journal = file.journal();
                journal.init().unwrap();
                None
            }
        };
        trace.push(json!({"result":result,"failure":journal.get_failure_by_turn_id("thread-1","turn-1").unwrap(),"order":journal.get_failure_evidence_order("thread-1","turn-1").unwrap(),"recovery":journal.get_recovery_by_turn_id("thread-1","turn-1").unwrap(),"persisted":file.read()}));
    }
    fn scrub(value: &mut Value) {
        match value {
            Value::Array(items) => items.iter_mut().for_each(scrub),
            Value::Object(fields) => {
                for (key, value) in fields {
                    if key == "createdAt" || key == "updatedAt" {
                        *value = json!("TIME");
                    } else {
                        scrub(value);
                    }
                }
            }
            _ => (),
        }
    }
    let mut actual = Value::Array(trace);
    scrub(&mut actual);
    scrub(&mut expected);
    normalize_json(&mut actual);
    normalize_json(&mut expected);
    assert_eq!(
        actual.as_array().unwrap().len(),
        expected.as_array().unwrap().len()
    );
    for (index, (a, b)) in actual
        .as_array()
        .unwrap()
        .iter()
        .zip(expected.as_array().unwrap())
        .enumerate()
    {
        assert_eq!(
            a.to_string(),
            b.to_string(),
            "Node parity diverged at observation {index}"
        );
    }
}

#[test]
fn task_workflow_matches_the_captured_node_journal_including_field_order() {
    let expected = super::super::test_support::baseline()["files"]["operations"].clone();
    let file = Fixture::new();
    let journal = file.journal();
    let task = journal
        .begin_task(BeginTaskInput {
            idempotency_key: "baseline-task".into(),
            payload_hash: "hash".into(),
            project_id: "project-baseline".into(),
            title: "Baseline task".into(),
            runtime_mode: RuntimeMode::ApprovalRequired,
        })
        .unwrap()
        .record;
    let task_ref = text(&task, "taskRef");
    let settings: SettingsReceipt =
        serde_json::from_value(expected["operations"][0]["settings"].clone()).unwrap();
    let mut create = input("baseline-task:thread");
    create.kind = WritableOperationKind::ThreadCreate;
    create.project_id = Some("project-baseline".into());
    create.run_id = None;
    create.message_id = None;
    create.turn_id = None;
    create.settings = Some(settings.clone());
    let created = journal.begin(create).unwrap().record;
    let create_id = text(&created, "operationId");
    journal
        .update(
            create_id,
            OperationPatch {
                status: Some(OperationStatus::Accepted),
                t3_sequence: Some(2.0),
                ..Default::default()
            },
        )
        .unwrap();
    journal
        .update_task(
            task_ref,
            TaskPatch {
                thread_id: Some("thread-1".into()),
                thread_operation_id: Some(create_id.into()),
                stage: Some(TaskStage::ThreadCreated),
                last_error: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
    journal
        .update_task(
            task_ref,
            TaskPatch {
                baseline_attribution: Some(BaselineAttribution::Unavailable),
                ..Default::default()
            },
        )
        .unwrap();
    let mut send = input("baseline-task:turn");
    send.project_id = Some("project-baseline".into());
    send.turn_id = None;
    send.settings = Some(settings);
    let sent = journal.begin(send).unwrap().record;
    let run_id = text(&sent, "operationId");
    journal
        .update(
            run_id,
            OperationPatch {
                status: Some(OperationStatus::Accepted),
                t3_sequence: Some(3.0),
                ..Default::default()
            },
        )
        .unwrap();
    journal
        .update(
            run_id,
            OperationPatch {
                turn_id: Some("turn-1".into()),
                ..Default::default()
            },
        )
        .unwrap();
    journal
        .update_task(
            task_ref,
            TaskPatch {
                run_id: Some("run-1".into()),
                message_id: Some("message-1".into()),
                run_operation_id: Some(run_id.into()),
                stage: Some(TaskStage::RunAccepted),
                last_error: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
    fn scrub(value: &mut Value) {
        match value {
            Value::Array(items) => items.iter_mut().for_each(scrub),
            Value::Object(fields) => {
                for (key, value) in fields {
                    if let Some(replacement) = match key.as_str() {
                        "operationId" | "threadOperationId" | "runOperationId" => Some("op_<UUID>"),
                        "commandId" | "threadId" => Some("<UUID>"),
                        "taskRef" => Some("task_<UUID>"),
                        "runId" => Some("run_<UUID>"),
                        "messageId" => Some("user:msg_<ID>"),
                        "turnId" => Some("turn-user:msg_<ID>"),
                        "createdAt" | "updatedAt" => Some("<ISO_TIME>"),
                        "payloadHash" => Some("<HASH>"),
                        _ => None,
                    } {
                        *value = json!(replacement);
                    } else {
                        scrub(value);
                    }
                }
            }
            _ => (),
        }
    }
    let mut actual = file.read();
    scrub(&mut actual);
    assert_eq!(actual.to_string(), expected.to_string());
    journal
        .update_task(
            task_ref,
            TaskPatch {
                baseline_attribution: Some(BaselineAttribution::Clean),
                baseline_revision: Some("base".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let actual = file.read();
    let keys: Vec<_> = actual["tasks"][0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert!(
        keys.iter()
            .position(|k| *k == "baselineAttribution")
            .unwrap()
            < keys.iter().position(|k| *k == "baselineRevision").unwrap()
    );
}

#[test]
fn split_patch_previews_do_not_interrupt_persistence_or_poison_the_journal() {
    let file = Fixture::new();
    let audit = Arc::new(AuditLog::new(file.directory.join("audit.jsonl")));
    let journal = OperationJournal::new(&file.path, Some(audit.clone()));
    let op = journal.begin(input("split-operation")).unwrap().record;
    let task = journal.begin_task(task_input("split-task")).unwrap().record;
    let error = format!("{}😀", "a".repeat(511));
    let operation_id = text(&op, "operationId");
    let task_ref = text(&task, "taskRef");
    assert_eq!(
        journal
            .update(
                operation_id,
                OperationPatch {
                    status: Some(OperationStatus::Rejected),
                    last_error: Some(error.clone()),
                    ..Default::default()
                }
            )
            .unwrap()["lastError"],
        error
    );
    assert_eq!(
        journal
            .update_task(
                task_ref,
                TaskPatch {
                    stage: Some(TaskStage::Rejected),
                    last_error: Some(Some(error.clone())),
                    ..Default::default()
                }
            )
            .unwrap()["lastError"],
        error
    );
    assert_eq!(
        journal.get_by_operation_id(operation_id).unwrap().unwrap()["lastError"],
        error
    );
    assert_eq!(
        journal.get_task_by_ref(task_ref).unwrap().unwrap()["lastError"],
        error
    );
    let reopened = file.journal();
    assert_eq!(
        reopened.get_by_operation_id(operation_id).unwrap().unwrap()["lastError"],
        error
    );
    assert_eq!(
        reopened.get_task_by_ref(task_ref).unwrap().unwrap()["lastError"],
        error
    );
    journal
        .update(
            operation_id,
            OperationPatch {
                last_error: Some("later".into()),
                ..Default::default()
            },
        )
        .unwrap();
    journal
        .update_task(
            task_ref,
            TaskPatch {
                last_error: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
    let page = audit
        .query(&crate::operations::audit_log::AuditQueryInput::default())
        .unwrap();
    assert_eq!(page.invalid_lines, 0);
    assert!(page.items.iter().any(|row| row.event == "operation.update"));
    assert!(page.items.iter().any(|row| row.event == "task.update"));
    assert!(audit.diagnostic_error().is_none());
}
