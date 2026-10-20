use super::*;

fn observation(thread: &str, message: &str, sequence: u64, admission: u64) -> TerminalFailure {
    TerminalFailure {
        thread_id: thread.into(),
        turn_id: "turn-1".into(),
        failure: failure("t3_session", "provider_error", message),
        operation_id: None,
        order: Some(identified(order(2, "shell", sequence, admission), message)),
    }
}

#[test]
fn rejected_recovered_failure_does_not_skip_later_batch_rows() {
    let file = Fixture::new();
    let journal = file.journal();
    journal
        .clear_terminal_failure(
            "recovered",
            "turn-1",
            RecoveryState::Completed,
            Some(order(2, "shell", 20, 100)),
        )
        .unwrap();
    let results = journal
        .retain_terminal_failures(&[
            observation("recovered", "stale", 20, 200),
            observation("new", "accepted", 20, 200),
            observation("recovered", "newer", 21, 300),
        ])
        .unwrap();
    assert_eq!(results[0], None);
    assert_eq!(results[1].as_ref().unwrap()["message"], "accepted");
    assert_eq!(results[2].as_ref().unwrap()["message"], "newer");
    let reopened = file.journal();
    assert!(
        reopened
            .get_recovery_by_turn_id("recovered", "turn-1")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        reopened.get_failure_by_turn_id("new", "turn-1").unwrap(),
        results[1]
    );
    assert_eq!(
        reopened
            .get_failure_by_turn_id("recovered", "turn-1")
            .unwrap(),
        results[2]
    );
}

#[test]
fn failure_batch_preserves_duplicate_evidence_and_linked_receipt_order() {
    let file = Fixture::new();
    let mut operation = loaded_operation();
    operation.as_object_mut().unwrap().shift_remove("turnId");
    file.write(json!({"version": 2, "operations": [operation]}));
    let journal = file.journal();
    let mut precise = observation("thread-1", "quota", 20, 100);
    precise.failure = failure("t3_v2_turn_item", "quota", "quota");
    precise.failure["code"] = json!("limit");
    precise.failure["retryable"] = json!(true);
    precise.failure["retryAfter"] = json!("42");
    precise.order = Some(identified(order(2, "full", 20, 100), "quota"));
    precise.operation_id = Some("op-1".into());
    let mut shell = observation("thread-1", "quota", 21, 200);
    shell.failure["category"] = json!("quota");
    shell.failure["model"] = json!("shell-model");
    shell.operation_id = Some("op-1".into());
    let stale = observation("thread-1", "stale", 19, 300);
    let mut protocol = observation("thread-1", "v1", 1, 400);
    protocol.order = Some(identified(order(1, "full", 1, 400), "v1"));
    protocol.operation_id = Some("op-1".into());
    let delayed = observation("thread-1", "delayed v2", 1000, 350);
    let observations = [precise, shell, stale, protocol, delayed];
    let results = journal.retain_terminal_failures(&observations).unwrap();
    for result in &results[..3] {
        let result = result.as_ref().unwrap();
        assert_eq!(result["source"], "t3_v2_turn_item");
        assert_eq!(result["code"], "limit");
        assert_eq!(result["retryable"], true);
        assert_eq!(result["retryAfter"], "42");
        assert_eq!(result["model"], "model-1");
    }
    for result in &results[3..] {
        assert_eq!(result.as_ref().unwrap()["message"], "v1");
    }
    let receipt = journal.get_by_operation_id("op-1").unwrap().unwrap();
    assert_eq!(receipt["turnId"], "turn-1");
    assert_eq!(receipt["terminalRunStatus"], "failed");
    assert_eq!(receipt["terminalFailure"]["message"], "v1");
    let retained = journal
        .get_failure_evidence_order("thread-1", "turn-1")
        .unwrap()
        .unwrap();
    assert_eq!(retained["protocolVersion"], 1);
    assert_eq!(retained["readStartedAt"].as_f64(), Some(400.0));
    assert_eq!(retained["failureIdentity"], hash_payload(&json!("v1")));
    let reopened = file.journal();
    assert_eq!(
        reopened
            .get_failure_by_turn_id("thread-1", "turn-1")
            .unwrap(),
        results[4]
    );
    assert_eq!(reopened.get_by_operation_id("op-1").unwrap(), Some(receipt));
}

#[test]
fn invalid_failure_batch_discards_admission_rows_and_linked_receipt_updates() {
    for fault in ["candidate", "staged-operation"] {
        let file = Fixture::new();
        let mut operation = loaded_operation();
        operation.as_object_mut().unwrap().shift_remove("turnId");
        file.write(json!({"version": 2, "operations": [operation]}));
        let journal = file.journal();
        journal
            .retain_terminal_failures(&[observation("admission", "same", 10, 100)])
            .unwrap();
        let before = fs::read(&file.path).unwrap();
        let original_receipt = journal.get_by_operation_id("op-1").unwrap();
        let mut linked = observation("thread-1", "linked", 20, 200);
        linked.operation_id = Some("op-1".into());
        let mut invalid = observation("thread-1", "invalid", 21, 300);
        if fault == "candidate" {
            invalid.failure["turnId"] = json!("wrong-turn");
        } else {
            invalid.turn_id = "turn-2".into();
            invalid.failure["turnId"] = json!("turn-2");
            invalid.operation_id = Some("op-1".into());
        }
        let mut batch = vec![observation("admission", "same", 10, 500), linked, invalid];
        assert!(matches!(
            journal.retain_terminal_failures(&batch),
            Err(JournalError::Invalid(_))
        ));
        assert_eq!(fs::read(&file.path).unwrap(), before);
        assert_eq!(
            journal.get_by_operation_id("op-1").unwrap(),
            original_receipt
        );
        assert!(
            journal
                .get_failure_by_turn_id("thread-1", "turn-1")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            journal
                .get_failure_evidence_order("admission", "turn-1")
                .unwrap()
                .unwrap()["readStartedAt"],
            100
        );
        journal.begin(input("unrelated")).unwrap();
        let reopened = file.journal();
        assert_eq!(
            reopened.get_by_operation_id("op-1").unwrap(),
            original_receipt
        );
        assert!(
            reopened
                .get_failure_by_turn_id("thread-1", "turn-1")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            reopened
                .get_failure_evidence_order("admission", "turn-1")
                .unwrap()
                .unwrap()["readStartedAt"],
            100
        );
        batch.pop();
        journal.retain_terminal_failures(&batch).unwrap();
        let reopened = file.journal();
        assert_eq!(
            reopened.get_by_operation_id("op-1").unwrap().unwrap()["terminalFailure"]["message"],
            "linked"
        );
        assert_eq!(
            reopened
                .get_failure_evidence_order("admission", "turn-1")
                .unwrap()
                .unwrap()["readStartedAt"],
            500
        );
        assert!(
            reopened
                .get_by_idempotency_key("unrelated")
                .unwrap()
                .is_some()
        );
    }
}

#[test]
fn failure_batch_serializes_with_an_unrelated_writer_and_reopens() {
    let file = Fixture::new();
    let journal = Arc::new(file.journal());
    let operation = journal.begin(input("unrelated")).unwrap().record;
    let operation_id = text(&operation, "operationId").to_owned();
    let barrier = Arc::new(Barrier::new(3));
    let batch = {
        let journal = Arc::clone(&journal);
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            journal
                .retain_terminal_failures(&[
                    observation("a", "first", 20, 100),
                    observation("b", "second", 20, 100),
                    observation("a", "latest", 21, 200),
                ])
                .unwrap();
        })
    };
    let writer = {
        let journal = Arc::clone(&journal);
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            journal
                .update(
                    &operation_id,
                    OperationPatch {
                        t3_sequence: Some(77.0),
                        ..Default::default()
                    },
                )
                .unwrap();
        })
    };
    barrier.wait();
    batch.join().unwrap();
    writer.join().unwrap();
    let reopened = file.journal();
    assert_eq!(
        reopened
            .get_by_idempotency_key("unrelated")
            .unwrap()
            .unwrap()["t3Sequence"],
        77
    );
    assert_eq!(
        reopened
            .get_failure_by_turn_id("a", "turn-1")
            .unwrap()
            .unwrap()["message"],
        "latest"
    );
    assert_eq!(
        reopened
            .get_failure_by_turn_id("b", "turn-1")
            .unwrap()
            .unwrap()["message"],
        "second"
    );
}
