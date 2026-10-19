use super::*;
use crate::operations::journal::{FailureEvidenceOrder, RecoveryState};
use crate::t3::{failure::sanitize_failure_identifier, types::TurnState};

struct ThreadObservation {
    source: ThreadShell,
    failure: Option<Value>,
    order: FailureEvidenceOrder,
}

impl T3Gateway {
    pub(super) async fn thread_get(&self, thread_id: &str) -> Result<Value, ToolFailure> {
        let full_started_at = self.read_started_at();
        let snapshot = self
            .client
            .get_thread(thread_id, None)
            .await
            .map_err(ToolFailure::from)?;
        let shell_started_at = self.read_started_at();
        let shell = self
            .client
            .get_shell(None)
            .await
            .map_err(ToolFailure::from)?;
        let descriptor = self
            .client
            .get_descriptor(None)
            .await
            .map_err(ToolFailure::from)?;
        self.assert_environment(&descriptor)?;
        let thread = &snapshot.thread;
        let summary = shell.threads.iter().find(|thread| thread.id == thread_id);
        let project_title = shell
            .projects
            .iter()
            .find(|project| project.id == thread.shell.project_id)
            .map(|project| project.title.as_str());
        let full_order = self
            .journal
            .order_failure_evidence(
                thread_id,
                turn_id(&thread.shell),
                run::failure_order(
                    thread,
                    snapshot.snapshot_sequence,
                    turn_id(&thread.shell),
                    full_started_at,
                )?,
            )
            .map_err(journal_failure)?;
        let shell_order = summary
            .map(|summary| {
                self.journal
                    .order_failure_evidence(
                        thread_id,
                        turn_id(summary),
                        run::shell_failure_order(
                            summary,
                            shell.snapshot_sequence,
                            shell_started_at,
                        )?,
                    )
                    .map_err(journal_failure)
            })
            .transpose()?;
        let observed = select_observation(thread, summary, full_order, shell_order)?;
        let source = &observed.source;
        let turn = source.latest_turn.as_ref();
        let recovery = turn.and_then(|turn| match turn.state {
            TurnState::Completed => Some(RecoveryState::Completed),
            TurnState::Interrupted => Some(RecoveryState::Interrupted),
            _ => None,
        });
        let mut failure = observed.failure;
        if let Some(turn) = turn {
            if let Some(recovery) = recovery {
                self.journal
                    .clear_terminal_failure(
                        thread_id,
                        &turn.turn_id,
                        recovery,
                        Some(observed.order.clone()),
                    )
                    .map_err(journal_failure)?;
                failure = self
                    .journal
                    .get_failure_by_turn_id(thread_id, &turn.turn_id)
                    .map_err(journal_failure)?;
            } else {
                let prior = self
                    .journal
                    .get_failure_by_turn_id(thread_id, &turn.turn_id)
                    .map_err(journal_failure)?;
                if let Some(candidate) = failure.or(prior) {
                    failure = self
                        .journal
                        .retain_terminal_failure(
                            thread_id,
                            &turn.turn_id,
                            candidate,
                            None,
                            Some(observed.order.clone()),
                        )
                        .map_err(journal_failure)?;
                } else {
                    failure = None;
                }
            }
        } else if let Some(candidate) = failure.take() {
            let failed_turn = candidate["turnId"].as_str().map(str::to_owned);
            if let Some(failed_turn) = failed_turn {
                failure = self
                    .journal
                    .retain_terminal_failure(
                        thread_id,
                        &failed_turn,
                        candidate,
                        None,
                        Some(observed.order),
                    )
                    .map_err(journal_failure)?;
            }
        }
        if failure.is_none()
            && thread.shell.latest_turn.is_none()
            && let Some(turn) = thread
                .messages
                .iter()
                .rev()
                .find(|message| message.role == MessageRole::User)
                .and_then(|message| message.turn_id.as_deref())
        {
            failure = self
                .journal
                .get_failure_by_turn_id(thread_id, turn)
                .map_err(journal_failure)?;
        }
        let mut detail = thread_summary(
            source,
            project_title,
            &descriptor.environment_id,
            Utc::now().timestamp_millis(),
        );
        detail["failure"] = failure.clone().unwrap_or(Value::Null);
        let response_turn = turn_id(source).or_else(|| {
            failure
                .as_ref()
                .and_then(|failure| failure["turnId"].as_str())
        });
        let allow_unbound = source.latest_turn.is_none()
            && thread
                .shell
                .session
                .as_ref()
                .is_none_or(|session| session.status != "error");
        detail["latestResponse"] = if recovery.is_some() && failure.is_some() {
            Value::Null
        } else {
            latest_assistant(&thread.messages, response_turn, allow_unbound)
                .map(|message| {
                    let mut message = message.clone();
                    if message.text.starts_with("API Error:") {
                        message.text =
                            crate::t3::failure::sanitize_failure_text(&message.text, 2000);
                    }
                    serde_json::to_value(message).expect("serializable message")
                })
                .unwrap_or(Value::Null)
        };
        detail["messageCount"] = json!(thread.messages.len());
        detail["activityCount"] = json!(thread.activities.len());
        detail["checkpointCount"] = json!(thread.checkpoints.len());
        detail["proposedPlanCount"] = json!(thread.proposed_plans.len());
        Ok(json!({"environmentId":descriptor.environment_id,"thread":detail}))
    }
}

fn turn_id(thread: &ThreadShell) -> Option<&str> {
    thread
        .latest_turn
        .as_ref()
        .map(|turn| turn.turn_id.as_str())
}

fn select_observation(
    thread: &Thread,
    summary: Option<&ThreadShell>,
    full_order: FailureEvidenceOrder,
    shell_order: Option<FailureEvidenceOrder>,
) -> Result<ThreadObservation, ToolFailure> {
    let same_turn = turn_id(&thread.shell) == summary.and_then(turn_id);
    let different_eras = shell_order.as_ref().is_some_and(|shell_order| {
        full_order["protocolVersion"] != shell_order["protocolVersion"]
            || full_order["protocolStartedAt"].as_u64().unwrap_or(0)
                != shell_order["protocolStartedAt"].as_u64().unwrap_or(0)
    });
    let newer_full_turn = thread
        .shell
        .latest_turn
        .as_ref()
        .zip(summary.and_then(|summary| summary.latest_turn.as_ref()))
        .is_some_and(|(full, shell)| {
            !same_turn
                && if let Some(shell_order) = shell_order.as_ref().filter(|order| {
                    different_eras
                        || (full_order["protocolVersion"] == 2 && order["protocolVersion"] == 2)
                }) {
                    full_order.compare(shell_order).is_gt()
                } else {
                    crate::operations::parse_timestamp(&full.requested_at)
                        .zip(crate::operations::parse_timestamp(&shell.requested_at))
                        .is_some_and(|(full, shell)| full > shell)
                }
        });
    let newer_full_state = same_turn
        && summary.is_some_and(|summary| {
            if let Some(shell_order) = shell_order.as_ref().filter(|order| {
                different_eras
                    || full_order["snapshotSequence"] != order["snapshotSequence"]
                    || (full_order.get("updatedAt").is_some()
                        && order.get("updatedAt").is_some()
                        && full_order["updatedAt"] != order["updatedAt"])
            }) {
                full_order.compare(shell_order).is_gt()
            } else {
                timestamp_field(&thread.shell, "updatedAt")
                    .zip(timestamp_field(summary, "updatedAt"))
                    .is_some_and(|(full, shell)| full > shell)
                    || (thread.shell.latest_turn.as_ref().is_some_and(|turn| {
                        matches!(turn.state, TurnState::Completed | TurnState::Interrupted)
                    }) && summary
                        .latest_turn
                        .as_ref()
                        .is_some_and(|turn| turn.state == TurnState::Error))
            }
        });
    let from_full = newer_full_turn || newer_full_state || summary.is_none();
    let mut source = if from_full {
        thread.shell.clone()
    } else {
        summary.expect("shell source exists").clone()
    };
    if from_full {
        source.latest_user_message_at = source
            .latest_user_message_at
            .or_else(|| summary.and_then(|summary| summary.latest_user_message_at.clone()));
        source.has_pending_approvals = source
            .has_pending_approvals
            .or_else(|| summary.and_then(|summary| summary.has_pending_approvals));
        source.has_pending_user_input = source
            .has_pending_user_input
            .or_else(|| summary.and_then(|summary| summary.has_pending_user_input));
    }
    let full_failure = source
        .latest_turn
        .as_ref()
        .filter(|turn| {
            turn.state == TurnState::Error && Some(turn.turn_id.as_str()) == turn_id(&thread.shell)
        })
        .and_then(|_| failure_info(thread, None))
        .map(|failure| serde_json::to_value(failure).expect("serializable failure"));
    let shell_failure = shell_failure_info(&source, None)
        .map(|failure| serde_json::to_value(failure).expect("serializable failure"));
    let mut failure = full_failure.clone().or_else(|| shell_failure.clone());
    let mut order = if full_failure.is_some() || from_full {
        full_order.clone()
    } else {
        shell_order.clone().unwrap_or_else(|| full_order.clone())
    };
    let newer_shell = !from_full
        && shell_order.as_ref().is_some_and(|shell_order| {
            (shell_order["protocolVersion"] == 2
                || different_eras
                || (full_failure
                    .as_ref()
                    .is_some_and(|failure| failure["source"] == "t3_session")
                    && shell_failure
                        .as_ref()
                        .is_some_and(|failure| failure["source"] == "t3_session")))
                && shell_order.compare(&full_order).is_gt()
        });
    if newer_shell && let Some(shell_failure) = shell_failure {
        let shell_order = shell_order.expect("newer shell has an order");
        let run = thread.turn_failures.as_ref().and_then(|failures| {
            failures
                .iter()
                .find(|failure| Some(failure.turn_id.as_str()) == shell_failure["turnId"].as_str())
        });
        let keep_full = shell_order["protocolVersion"] == 2
            && shell_failure["source"] == "t3_turn"
            && full_failure.is_some();
        failure = if keep_full {
            full_failure
        } else if let Some(mut full) = full_failure.filter(|full| {
            full_order.get("failureIdentity").is_some()
                && full_order["failureIdentity"] == shell_order["failureIdentity"]
                && full["message"] == shell_failure["message"]
                && full["class"] == shell_failure["class"]
        }) {
            for field in ["resetAt", "retryAfter"] {
                if !shell_failure[field].is_null() {
                    full[field] = shell_failure[field].clone();
                }
            }
            Some(full)
        } else {
            let mut failure = shell_failure;
            if let Some(run) = run {
                failure["provider"] = json!(sanitize_failure_identifier(&run.provider));
                failure["model"] = json!(sanitize_failure_identifier(&run.model_selection.model));
            }
            Some(failure)
        };
        let mut next: Value = shell_order.into();
        if keep_full {
            if let Some(identity) = full_order.get("failureIdentity") {
                next["failureIdentity"] = identity.clone();
            } else {
                next.as_object_mut().unwrap().remove("failureIdentity");
            }
        }
        if let Some(identity) = run
            .and_then(|run| run.order.as_ref())
            .and_then(|order| order.run_identity.as_ref())
        {
            next["runIdentity"] = serde_json::to_value(identity).expect("serializable identity");
        }
        order = FailureEvidenceOrder::try_from(next).map_err(journal_failure)?;
    } else if full_failure
        .as_ref()
        .is_some_and(|failure| failure["source"] == "t3_turn")
        && full_order["protocolVersion"] != 2
    {
        if shell_failure.is_some() {
            order = shell_order.unwrap_or(full_order);
        }
        failure = shell_failure.or(full_failure);
    }
    Ok(ThreadObservation {
        source,
        failure,
        order,
    })
}

fn timestamp_field(thread: &ThreadShell, field: &str) -> Option<i64> {
    thread
        .extra
        .get(field)
        .and_then(Value::as_str)
        .and_then(crate::operations::parse_timestamp)
}
