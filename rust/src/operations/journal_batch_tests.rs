use super::*;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

const THREADS: &[&str] = &["a", "b", "stale", "admission", "new"];

fn recovery(thread: &str, sequence: u64, admission: u64, state: RecoveryState) -> TerminalRecovery {
    TerminalRecovery {
        thread_id: thread.into(),
        turn_id: "turn-1".into(),
        state,
        order: Some(order(2, "shell", sequence, admission)),
    }
}

fn recoveries() -> Vec<TerminalRecovery> {
    vec![
        recovery("admission", 10, 300, RecoveryState::Completed),
        recovery("new", 10, 300, RecoveryState::Completed),
        recovery("a", 21, 300, RecoveryState::Completed),
        recovery("a", 19, 400, RecoveryState::Completed),
        recovery("a", 22, 500, RecoveryState::Interrupted),
        recovery("b", 21, 300, RecoveryState::Interrupted),
        recovery("stale", 19, 300, RecoveryState::Completed),
        recovery("new", 10, 400, RecoveryState::Interrupted),
    ]
}

fn failures() -> Vec<TerminalFailure> {
    [
        ("a", "a", 20, 300, "original"),
        ("new", "missing", 10, 300, "new failure"),
        ("b", "b", 21, 300, "changed failure"),
        ("b", "b", 19, 400, "stale failure"),
        ("b", "b", 22, 500, "latest failure"),
        ("b", "b", 22, 600, "latest failure"),
    ]
    .into_iter()
    .map(
        |(thread, operation, sequence, admission, message)| TerminalFailure {
            thread_id: thread.into(),
            turn_id: "turn-1".into(),
            failure: failure("t3_session", "quota", message),
            operation_id: Some(operation.into()),
            order: Some(order(2, "shell", sequence, admission)),
        },
    )
    .collect()
}

fn populated(file: &Fixture) -> OperationJournal {
    let mut operations = Vec::new();
    for id in ["a", "a-second", "b", "stale", "unrelated"] {
        let mut row = loaded_operation();
        row["operationId"] = json!(id);
        row["idempotencyKey"] = json!(id);
        row["threadId"] = json!(if id == "a-second" { "a" } else { id });
        operations.push(row);
    }
    file.write(json!({"version": 2, "operations": operations}));
    let journal = file.journal();
    journal.begin_task(task_input("task")).unwrap();
    for id in ["a", "a-second", "b", "stale"] {
        let thread = if id == "a-second" { "a" } else { id };
        journal
            .retain_terminal_failure(
                thread,
                "turn-1",
                failure("t3_session", "quota", "original"),
                Some(id),
                Some(order(2, "shell", 20, 100)),
            )
            .unwrap();
    }
    journal
        .clear_terminal_failures(&[recovery("admission", 10, 100, RecoveryState::Completed)])
        .unwrap();
    journal
}

fn observable(journal: &OperationJournal) -> Value {
    json!({
        "rows": THREADS.iter().map(|thread| json!({
            "failure": journal.get_failure_by_turn_id(thread, "turn-1").unwrap(),
            "recovery": journal.get_recovery_by_turn_id(thread, "turn-1").unwrap(),
            "order": journal.get_failure_evidence_order(thread, "turn-1").unwrap().map(|order| {
                let mut value = serde_json::to_value(order).unwrap();
                if let Some(admission) = value["readStartedAt"].as_f64() {
                    value["readStartedAt"] = json!(admission);
                }
                value
            }),
        })).collect::<Vec<_>>(),
        "operations": (["a", "a-second", "b", "stale", "unrelated"].iter().map(|id|
            journal.get_by_operation_id(id).unwrap()
        ).collect::<Vec<_>>()),
        "tasks": journal.list_tasks().unwrap(),
    })
}

enum StorageFault {
    Create(PathBuf),
    Write(libc::rlimit, libc::sighandler_t),
    Rename(WriteBlock),
}

impl StorageFault {
    fn new(file: &Fixture, kind: &str) -> Self {
        match kind {
            "create" => {
                fs::set_permissions(&file.directory, fs::Permissions::from_mode(0o500)).unwrap();
                Self::Create(file.directory.clone())
            }
            "write" => unsafe {
                let mut original = std::mem::zeroed();
                assert_eq!(libc::getrlimit(libc::RLIMIT_FSIZE, &mut original), 0);
                let signal = libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
                let limit = libc::rlimit {
                    rlim_cur: 1024,
                    rlim_max: original.rlim_max,
                };
                assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
                Self::Write(original, signal)
            },
            "rename" => Self::Rename(file.block_writes()),
            _ => panic!("unknown fault"),
        }
    }
}

impl Drop for StorageFault {
    fn drop(&mut self) {
        match self {
            Self::Create(path) => {
                fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap()
            }
            Self::Write(limit, signal) => unsafe {
                assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, limit), 0);
                libc::signal(libc::SIGXFSZ, *signal);
            },
            Self::Rename(block) => assert!(block.path.is_dir()),
        }
    }
}

#[test]
fn journal_transactions_survive_create_write_and_rename_errors() {
    let Ok(kind) = std::env::var("T3_JOURNAL_TEST_FAULT") else {
        for kind in ["create", "write", "rename"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "operations::journal::tests::batch::journal_transactions_survive_create_write_and_rename_errors", "--nocapture"])
                .env("T3_JOURNAL_TEST_FAULT", kind)
                .output().unwrap();
            assert!(
                output.status.success(),
                "{kind}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
        }
        return;
    };
    for batch in ["singleton", "recovery", "failure"] {
        let file = Fixture::new();
        let journal = populated(&file);
        let before = observable(&journal);
        let bytes = fs::read(&file.path).unwrap();
        let attempt = || {
            if batch == "recovery" {
                journal.clear_terminal_failures(&recoveries())
            } else if batch == "failure" {
                journal.retain_terminal_failures(&failures()).map(|_| ())
            } else {
                journal
                    .retain_terminal_failure(
                        "a",
                        "turn-1",
                        failure("t3_session", "provider_error", "new reason"),
                        Some("a"),
                        Some(order(2, "shell", 21, 300)),
                    )
                    .map(|_| ())
            }
        };
        let fault = StorageFault::new(&file, &kind);
        let error = attempt().unwrap_err();
        assert!(matches!(error, JournalError::Io(_)), "{error}");
        assert_eq!(observable(&journal), before);
        let durable = if kind == "rename" {
            file.directory.join("operations.saved")
        } else {
            file.path.clone()
        };
        assert_eq!(fs::read(durable).unwrap(), bytes);
        drop(fault);
        assert_eq!(fs::read(&file.path).unwrap(), bytes);
        assert!(!fs::read_dir(&file.directory).unwrap().any(|entry| {
            entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "tmp")
        }));
        journal.begin(input("unrelated-success")).unwrap();
        assert_eq!(
            observable(&file.journal()),
            before,
            "later write flushed an abandoned transaction"
        );
        attempt().unwrap();
        let reopened = file.journal();
        assert_eq!(observable(&reopened), observable(&journal));
        if batch == "recovery" {
            for id in ["a", "a-second", "b"] {
                let receipt = reopened.get_by_operation_id(id).unwrap().unwrap();
                assert!(receipt.get("terminalRunStatus").is_none());
                assert!(receipt.get("terminalFailure").is_none());
            }
            assert!(
                reopened
                    .get_failure_by_turn_id("stale", "turn-1")
                    .unwrap()
                    .is_some()
            );
            for id in ["a", "b", "new"] {
                assert!(matches!(
                    reopened.get_recovery_by_turn_id(id, "turn-1").unwrap(),
                    Some(RecoveryState::Interrupted)
                ));
            }
            assert_eq!(
                reopened
                    .get_failure_evidence_order("a", "turn-1")
                    .unwrap()
                    .unwrap()["snapshotSequence"],
                22
            );
            assert_eq!(
                reopened
                    .get_failure_evidence_order("admission", "turn-1")
                    .unwrap()
                    .unwrap()["readStartedAt"],
                300
            );
        } else if batch == "failure" {
            assert_eq!(
                reopened
                    .get_failure_by_turn_id("new", "turn-1")
                    .unwrap()
                    .unwrap()["message"],
                "new failure"
            );
            assert_eq!(
                reopened.get_by_operation_id("b").unwrap().unwrap()["terminalFailure"]["message"],
                "latest failure"
            );
            for (thread, sequence, admission) in [("a", 20, 300), ("b", 22, 600)] {
                let order = reopened
                    .get_failure_evidence_order(thread, "turn-1")
                    .unwrap()
                    .unwrap();
                assert_eq!(order["snapshotSequence"], sequence);
                assert_eq!(order["readStartedAt"], admission);
            }
        } else {
            assert_eq!(
                reopened.get_by_operation_id("a").unwrap().unwrap()["terminalFailure"]["message"],
                "new reason"
            );
        }
    }
}

#[test]
fn unchanged_observations_succeed_without_storage_and_advance_admission() {
    let file = Fixture::new();
    let journal = populated(&file);
    let bytes = fs::read(&file.path).unwrap();
    let fault = StorageFault::new(&file, "create");
    journal
        .clear_terminal_failures(&[recovery("admission", 10, 300, RecoveryState::Completed)])
        .unwrap();
    journal
        .retain_terminal_failure(
            "a",
            "turn-1",
            failure("t3_session", "quota", "original"),
            Some("a"),
            Some(order(2, "shell", 20, 300)),
        )
        .unwrap();
    let unchanged = ["a", "b"]
        .into_iter()
        .map(|thread| TerminalFailure {
            thread_id: thread.into(),
            turn_id: "turn-1".into(),
            failure: failure("t3_session", "quota", "original"),
            operation_id: Some(thread.into()),
            order: Some(order(2, "shell", 20, 400)),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        journal.retain_terminal_failures(&unchanged).unwrap().len(),
        2
    );
    assert!(journal.retain_terminal_failures(&[]).unwrap().is_empty());
    for id in ["a", "b", "admission"] {
        assert_eq!(
            journal
                .get_failure_evidence_order(id, "turn-1")
                .unwrap()
                .unwrap()["readStartedAt"]
                .as_f64(),
            Some(if id == "admission" { 300.0 } else { 400.0 })
        );
        journal
            .retain_terminal_failure(
                id,
                "turn-1",
                failure("t3_session", "quota", "delayed protocol"),
                None,
                Some(order(1, "full", 1000, 200)),
            )
            .unwrap();
        assert_eq!(
            journal
                .get_failure_evidence_order(id, "turn-1")
                .unwrap()
                .unwrap()["protocolVersion"],
            2
        );
    }
    assert!(
        journal
            .clear_terminal_failures(&[recovery("admission", 11, 400, RecoveryState::Completed)])
            .is_err()
    );
    assert_eq!(
        journal
            .get_failure_evidence_order("admission", "turn-1")
            .unwrap()
            .unwrap()["readStartedAt"]
            .as_f64(),
        Some(300.0)
    );
    assert_eq!(fs::read(&file.path).unwrap(), bytes);
    drop(fault);
}

#[test]
fn batch_commit_and_failure_serialize_with_readers_and_an_unrelated_writer() {
    for fail in [false, true] {
        let file = Fixture::new();
        let journal = Arc::new(populated(&file));
        let fault = fail.then(|| file.block_writes());
        let barrier = Arc::new(Barrier::new(3));
        let done = Arc::new(AtomicBool::new(false));
        let reader = {
            let journal = Arc::clone(&journal);
            let done = Arc::clone(&done);
            let path = if fail {
                file.directory.join("operations.saved")
            } else {
                file.path.clone()
            };
            std::thread::spawn(move || {
                let mut reads = 0;
                while !done.load(AtomicOrdering::SeqCst) || reads == 0 {
                    let bytes = fs::read(&path).unwrap();
                    let value: Value = serde_json::from_slice(&bytes).unwrap();
                    let related = value["operations"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|row| ["a", "a-second", "b"].contains(&text(row, "operationId")))
                        .collect::<Vec<_>>();
                    let cleared = related
                        .iter()
                        .filter(|row| row.get("terminalFailure").is_none())
                        .count();
                    assert!(cleared == 0 || (!fail && cleared == 3));
                    for id in ["a", "b"] {
                        let row = journal.get_by_operation_id(id).unwrap().unwrap();
                        assert_eq!(
                            row.get("terminalFailure").is_some(),
                            row.get("terminalRunStatus").is_some()
                        );
                    }
                    reads += 1;
                }
                reads
            })
        };
        let batch = {
            let journal = Arc::clone(&journal);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                journal.clear_terminal_failures(&recoveries()).is_ok()
            })
        };
        let writer = {
            let journal = Arc::clone(&journal);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                journal
                    .update(
                        "unrelated",
                        OperationPatch {
                            t3_sequence: Some(77.0),
                            ..Default::default()
                        },
                    )
                    .is_ok()
            })
        };
        barrier.wait();
        assert_eq!(batch.join().unwrap(), !fail);
        assert_eq!(writer.join().unwrap(), !fail);
        done.store(true, AtomicOrdering::SeqCst);
        assert!(reader.join().unwrap() > 0);
        drop(fault);
        if fail {
            journal
                .update(
                    "unrelated",
                    OperationPatch {
                        t3_sequence: Some(77.0),
                        ..Default::default()
                    },
                )
                .unwrap();
            let reopened = file.journal();
            assert!(
                reopened
                    .get_failure_by_turn_id("a", "turn-1")
                    .unwrap()
                    .is_some()
            );
            assert_eq!(
                reopened
                    .get_failure_evidence_order("admission", "turn-1")
                    .unwrap()
                    .unwrap()["readStartedAt"],
                100
            );
            assert!(
                reopened
                    .get_failure_evidence_order("new", "turn-1")
                    .unwrap()
                    .is_none()
            );
            journal.clear_terminal_failures(&recoveries()).unwrap();
        }
        let reopened = file.journal();
        assert_eq!(
            reopened.get_by_operation_id("unrelated").unwrap().unwrap()["t3Sequence"],
            77
        );
        assert!(
            reopened
                .get_failure_by_turn_id("a", "turn-1")
                .unwrap()
                .is_none()
        );
    }
}
