use super::*;
use crate::{
    operations::journal::{FailureEvidenceOrder, RecoveryState},
    t3::{
        failure::sanitize_failure_text,
        http_client::ClientError,
        types::{LatestTurn, TurnState},
    },
};

struct RunHandle<'a> {
    operation_id: &'a str,
    thread_id: &'a str,
    run_id: &'a str,
}

impl<'a> RunHandle<'a> {
    fn from_record(record: &'a OperationRecord) -> Result<Self, ToolFailure> {
        match (record["threadId"].as_str(), record["runId"].as_str()) {
            (Some(thread_id), Some(run_id)) if !thread_id.is_empty() && !run_id.is_empty() => {
                Ok(Self {
                    operation_id: record["operationId"].as_str().unwrap_or_default(),
                    thread_id,
                    run_id,
                })
            }
            _ => Err(ToolFailure::new(
                "run_invalid",
                "The journal entry does not contain a run handle.",
            )),
        }
    }
}

impl T3Gateway {
    pub(super) async fn run_get(&self, run_id: &str) -> Result<Value, ToolFailure> {
        let mut operation = self
            .journal
            .get_by_run_id(run_id)
            .map_err(journal_failure)?;
        let normalized = run_id.strip_prefix("run_").unwrap_or(run_id);
        for candidate in [
            run_id.to_owned(),
            normalized.to_owned(),
            format!("run_{normalized}"),
        ] {
            if operation.is_none() {
                operation = self
                    .journal
                    .get_by_operation_id(&candidate)
                    .map_err(journal_failure)?;
            }
        }
        let record = operation
            .filter(|record| record["kind"] == "thread.turn.start")
            .ok_or_else(|| {
                ToolFailure::new(
                    "run_not_found",
                    format!("Run {run_id} was not found in the gateway journal."),
                )
            })?;
        self.observe_run(&record).await
    }

    async fn observe_run(&self, record: &OperationRecord) -> Result<Value, ToolFailure> {
        let handle = RunHandle::from_record(record)?;
        let read_started_at = self.read_started_at();
        let snapshot = match self.client.get_thread(handle.thread_id, None).await {
            Ok(snapshot) => snapshot,
            Err(error) => return self.unavailable_run(record, &handle, &error).await,
        };
        let thread = &snapshot.thread;
        let shell = &thread.shell;
        let observed_at = now_iso();
        let observation = observe_thread(shell, Utc::now().timestamp_millis());
        let message = record["messageId"]
            .as_str()
            .and_then(|id| thread.messages.iter().find(|message| message.id == id));
        let turn_id = record["turnId"]
            .as_str()
            .or_else(|| message.and_then(|message| message.turn_id.as_deref()));
        if let Some(turn_id) = turn_id
            && record["turnId"].as_str() != Some(turn_id)
        {
            self.journal
                .update(
                    handle.operation_id,
                    OperationPatch {
                        turn_id: Some(turn_id.to_owned()),
                        ..OperationPatch::default()
                    },
                )
                .map_err(journal_failure)?;
        }
        let matching_turn = shell
            .latest_turn
            .as_ref()
            .filter(|turn| Some(turn.turn_id.as_str()) == turn_id);
        let order = failure_order(thread, snapshot.snapshot_sequence, turn_id, read_started_at)?;
        let recovery = thread
            .turn_recoveries
            .as_ref()
            .and_then(|recoveries| {
                recoveries
                    .iter()
                    .find(|entry| Some(entry.turn_id.as_str()) == turn_id)
            })
            .map(|entry| entry.state)
            .or_else(|| matching_turn.map(|turn| turn.state))
            .and_then(|state| match state {
                TurnState::Completed => Some(RecoveryState::Completed),
                TurnState::Interrupted => Some(RecoveryState::Interrupted),
                _ => None,
            });
        if let (Some(turn_id), Some(recovery)) = (turn_id, recovery) {
            self.journal
                .clear_terminal_failure(&shell.id, turn_id, recovery, Some(order.clone()))
                .map_err(journal_failure)?;
        }
        let observed_failure = turn_id.and_then(|turn_id| failure_info(thread, Some(turn_id)));
        let mut failure = if let Some(turn_id) = turn_id {
            let prior = self
                .journal
                .get_failure_by_turn_id(&shell.id, turn_id)
                .map_err(journal_failure)?;
            if recovery.is_some() {
                prior
            } else if let Some(candidate) = observed_failure
                .as_ref()
                .map(|value| serde_json::to_value(value).expect("serializable failure"))
                .or(prior)
            {
                self.journal
                    .retain_terminal_failure(
                        &shell.id,
                        turn_id,
                        candidate,
                        Some(handle.operation_id),
                        Some(order),
                    )
                    .map_err(journal_failure)?
            } else if self
                .journal
                .get_recovery_by_turn_id(&shell.id, turn_id)
                .map_err(journal_failure)?
                .is_some()
            {
                None
            } else {
                record
                    .get("terminalFailure")
                    .cloned()
                    .filter(|value| !value.is_null())
            }
        } else {
            record
                .get("terminalFailure")
                .cloned()
                .filter(|value| !value.is_null())
        };
        let retained_recovery = turn_id
            .map(|turn_id| self.journal.get_recovery_by_turn_id(&shell.id, turn_id))
            .transpose()
            .map_err(journal_failure)?
            .flatten();
        let observed_status = if record["status"] == "rejected"
            || (shell.latest_turn.is_none() && observed_failure.is_some())
        {
            "failed"
        } else if record["status"] == "uncertain"
            && message.is_none()
            && shell.latest_turn.is_none()
        {
            "unknown"
        } else if let Some(turn) = matching_turn {
            turn_status(turn, shell)
        } else if message.is_some_and(|message| message.role == MessageRole::Assistant) {
            "completed"
        } else {
            "accepted"
        };
        let run_status = if failure.is_some()
            || (recovery.is_none()
                && retained_recovery.is_none()
                && record["terminalRunStatus"] == "failed")
        {
            "failed"
        } else {
            match recovery.or(retained_recovery) {
                Some(RecoveryState::Completed) => "completed",
                Some(RecoveryState::Interrupted) => "interrupted",
                None => observed_status,
            }
        };
        if run_status != "failed" {
            failure = None;
        }
        let descriptor = match self.client.get_descriptor(None).await {
            Ok(descriptor) => descriptor,
            Err(error) => return self.unavailable_run(record, &handle, &error).await,
        };
        self.assert_environment(&descriptor)?;
        let mut run = json!({
            "environmentId": descriptor.environment_id,
            "operationId": handle.operation_id,
            "projectId": shell.project_id,
            "threadId": shell.id,
            "runId": handle.run_id,
            "t3TurnId": turn_id,
            "runStatus": run_status,
            "providerTurnId": null,
            "connectionStatus": "connected",
            "stateFreshness": "fresh",
            "lastObservedAt": observed_at,
            "observedAt": observed_at,
            "threadQuality": observation.quality,
            "threadWarning": observation.warning,
            "latestResponse": response_value(&thread.messages, turn_id),
            "failure": failure,
            "pendingActions": {
                "approvals": shell.has_pending_approvals.unwrap_or(false),
                "userInput": shell.has_pending_user_input.unwrap_or(false),
            },
        });
        add_settings(&mut run, record, matching_turn.map(|_| shell))?;
        Ok(run)
    }

    async fn unavailable_run(
        &self,
        record: &OperationRecord,
        handle: &RunHandle<'_>,
        error: &ClientError,
    ) -> Result<Value, ToolFailure> {
        let telemetry = self.client.telemetry();
        let mut environment_id = self
            .client
            .cached_descriptor()
            .map(|descriptor| descriptor.environment_id)
            .or_else(|| self.config.environment_id.clone())
            .unwrap_or_else(|| "unknown".into());
        let encoded = url::form_urlencoded::byte_serialize(handle.thread_id.as_bytes())
            .collect::<String>()
            .replace('+', "%20");
        let target_read = error.status == Some(404)
            && error.method.as_deref() == Some("GET")
            && error.path.as_deref()
                == Some(format!("/api/orchestration/threads/{encoded}").as_str());
        let explicit_missing = target_read
            && (error.reason.as_deref() == Some("thread_not_found")
                || error.code.as_deref() == Some("thread_not_found"));
        let legacy_missing =
            target_read && error.code.as_deref() == Some("not_found") && error.reason.is_none();
        let mut connected = false;
        if let Ok(descriptor) = self.client.get_descriptor(None).await {
            self.assert_environment(&descriptor)?;
            environment_id = descriptor.environment_id;
            connected = explicit_missing
                || (legacy_missing
                    && self.client.get_shell(None).await.is_ok_and(|shell| {
                        !shell
                            .threads
                            .iter()
                            .any(|thread| thread.id == handle.thread_id)
                    }));
        }
        let latest = self
            .journal
            .get_by_operation_id(handle.operation_id)
            .map_err(journal_failure)?
            .unwrap_or_else(|| record.clone());
        let retained = match (latest["threadId"].as_str(), latest["turnId"].as_str()) {
            (Some(thread), Some(turn)) => self
                .journal
                .get_failure_by_turn_id(thread, turn)
                .map_err(journal_failure)?,
            _ => None,
        };
        let failure = retained.or_else(|| {
            latest
                .get("terminalFailure")
                .cloned()
                .filter(|value| !value.is_null())
        });
        let mut run = json!({
            "environmentId": environment_id,
            "operationId": handle.operation_id,
            "projectId": record.get("projectId"),
            "threadId": record.get("threadId"),
            "runId": handle.run_id,
            "t3TurnId": latest.get("turnId"),
            "runStatus": if latest["terminalRunStatus"] == "failed" || failure.is_some() { "failed" } else { "unknown" },
            "providerTurnId": null,
            "connectionStatus": if connected { "connected" } else { "disconnected" },
            "stateFreshness": if connected { "unknown" } else { freshness(telemetry.last_snapshot_at.as_deref(), self.config.stale_after_ms) },
            "lastObservedAt": telemetry.last_snapshot_at,
            "observedAt": now_iso(),
            "threadQuality": null,
            "threadWarning": null,
            "latestResponse": null,
            "failure": failure,
            "pendingActions": { "approvals": false, "userInput": false },
            "error": sanitize_failure_text(&error.message, 2000),
        });
        add_settings(&mut run, record, None)?;
        Ok(run)
    }
}

fn turn_status(turn: &LatestTurn, thread: &ThreadShell) -> &'static str {
    if thread.has_pending_approvals == Some(true) {
        "awaiting_approval"
    } else if thread.has_pending_user_input == Some(true) {
        "awaiting_input"
    } else {
        match turn.state {
            TurnState::Running => "running",
            TurnState::Completed => "completed",
            TurnState::Interrupted => "interrupted",
            TurnState::Error => "failed",
        }
    }
}

fn response_value(messages: &[Message], turn_id: Option<&str>) -> Value {
    latest_assistant(messages, turn_id, false)
        .map(|message| {
            let mut message = message.clone();
            if message.text.starts_with("API Error:") {
                message.text = sanitize_failure_text(&message.text, 2000);
            }
            serde_json::to_value(message).expect("serializable message")
        })
        .unwrap_or(Value::Null)
}

pub(super) fn add_settings(
    run: &mut Value,
    record: &OperationRecord,
    thread: Option<&ThreadShell>,
) -> Result<(), ToolFailure> {
    if let Some(receipt) = record.get("settings") {
        let receipt = serde_json::from_value(receipt.clone())
            .map_err(|error| ToolFailure::new("journal_invalid", error.to_string()))?;
        let thread = thread
            .map(|shell| {
                Ok::<_, ToolFailure>(crate::settings::ThreadSettings {
                    model_selection: settings_model(&shell.model_selection)?,
                    runtime_mode: shell.runtime_mode,
                })
            })
            .transpose()?;
        run["settings"] =
            serde_json::to_value(crate::settings::observe_settings(&receipt, thread.as_ref()))
                .expect("serializable settings");
    }
    Ok(())
}

pub(in crate::gateway) fn failure_order(
    thread: &Thread,
    sequence: u64,
    turn_id: Option<&str>,
    read_started_at: u64,
) -> Result<FailureEvidenceOrder, ToolFailure> {
    let persisted = thread
        .turn_failures
        .as_ref()
        .and_then(|failures| {
            failures
                .iter()
                .find(|entry| Some(entry.turn_id.as_str()) == turn_id)
        })
        .and_then(|entry| entry.order.as_ref());
    let recovered = thread
        .turn_recoveries
        .as_ref()
        .and_then(|recoveries| {
            recoveries
                .iter()
                .find(|entry| Some(entry.turn_id.as_str()) == turn_id)
        })
        .map(|entry| &entry.order);
    let mut order = persisted
        .or(recovered)
        .map(|order| serde_json::to_value(order).expect("serializable order"))
        .unwrap_or_else(|| base_failure_order(&thread.shell, "full", sequence));
    order["readStartedAt"] = json!(read_started_at);
    if let Some(failure) = turn_id.and_then(|turn| failure_info(thread, Some(turn))) {
        order["failureIdentity"] = json!(failure.identity);
    } else if let Some(object) = order.as_object_mut() {
        object.remove("failureIdentity");
    }
    FailureEvidenceOrder::try_from(order).map_err(journal_failure)
}

pub(in crate::gateway) fn shell_failure_order(
    shell: &ThreadShell,
    sequence: u64,
    read_started_at: u64,
) -> Result<FailureEvidenceOrder, ToolFailure> {
    let mut order = shell
        .evidence_order
        .as_ref()
        .map(|order| serde_json::to_value(order).expect("serializable order"))
        .unwrap_or_else(|| base_failure_order(shell, "shell", sequence));
    order["readStartedAt"] = json!(read_started_at);
    if let Some(failure) = shell_failure_info(shell, None) {
        order["failureIdentity"] = json!(failure.identity);
    } else if let Some(object) = order.as_object_mut() {
        object.remove("failureIdentity");
    }
    FailureEvidenceOrder::try_from(order).map_err(journal_failure)
}

fn base_failure_order(shell: &ThreadShell, scope: &str, sequence: u64) -> Value {
    let mut order = json!({
        "protocolVersion": if shell.extra.get("orchestrationProtocolVersion") == Some(&json!(2)) { 2 } else { 1 },
        "scope": scope,
        "snapshotSequence": sequence,
    });
    if let Some(updated_at) = shell
        .extra
        .get("updatedAt")
        .and_then(Value::as_str)
        .and_then(crate::operations::parse_timestamp)
    {
        order["updatedAt"] = json!(
            chrono::DateTime::from_timestamp_millis(updated_at)
                .expect("valid timestamp")
                .to_rfc3339_opts(SecondsFormat::Millis, true)
        );
    }
    order
}
