use super::*;
use crate::operations::journal::OperationRecord;
use serde::Deserialize;
use serde_json::{Map, Value, json};

mod result;
mod run;
mod thread;

pub(super) use run::{failure_order, shell_failure_order};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum WorkspaceMode {
    Local,
    Worktree,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskStartInput {
    project_id: String,
    title: String,
    instruction: String,
    runtime_mode: crate::settings::RuntimeMode,
    model_selection: Option<ModelSelection>,
    interaction_mode: Option<InteractionMode>,
    workspace_mode: Option<WorkspaceMode>,
    branch: Option<String>,
    worktree_path: Option<String>,
    start_from_origin: Option<bool>,
    idempotency_key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskGetInput {
    task_ref: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TasksListInput {
    project_id: Option<String>,
    query: Option<String>,
    cursor: Option<String>,
    #[serde(default = "super::default_limit")]
    limit: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskCancelInput {
    task_ref: String,
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResultGetInput {
    task_ref: String,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default = "default_result_bytes")]
    max_bytes: usize,
}

struct ThreadCreateInput {
    project_id: String,
    title: String,
    model_selection: Option<ModelSelection>,
    runtime_mode: crate::settings::RuntimeMode,
    interaction_mode: InteractionMode,
    branch: Option<String>,
    worktree_path: Option<String>,
    managed_worktree: Option<(String, bool)>,
    idempotency_key: String,
}

struct ThreadSendInput {
    thread_id: String,
    message: String,
    model_selection: Option<ModelSelection>,
    runtime_mode: Option<crate::settings::RuntimeMode>,
    interaction_mode: Option<InteractionMode>,
    title_seed: Option<String>,
    initial_settings: Option<crate::settings::SettingsReceipt>,
    idempotency_key: String,
}

impl T3Gateway {
    pub async fn task_start(&self, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
        let input: TaskStartInput = parse_args(args)?;
        self.require_operation_scope().await?;
        let workspace_mode = input.workspace_mode.unwrap_or(WorkspaceMode::Local);
        match workspace_mode {
            WorkspaceMode::Worktree if input.branch.as_deref().is_none_or(str::is_empty) => {
                return Err(ToolFailure::new(
                    "worktree_base_branch_required",
                    "workspaceMode=worktree requires a base branch, for example branch=main. The gateway generates the isolated task branch/path; omit worktreePath. Use workspaceMode=local to use the current checkout.",
                ));
            }
            WorkspaceMode::Worktree if input.worktree_path.is_some() => {
                return Err(ToolFailure::new(
                    "worktree_path_not_allowed",
                    "workspaceMode=worktree creates a gateway-managed path; worktreePath must be omitted.",
                ));
            }
            WorkspaceMode::Local if input.start_from_origin.is_some() => {
                return Err(ToolFailure::new(
                    "origin_selection_requires_worktree",
                    "startFromOrigin is only valid when workspaceMode=worktree.",
                ));
            }
            _ => {}
        }
        let mut payload = json!({
            "projectId": input.project_id,
            "title": input.title,
            "instruction": input.instruction,
            "runtimeMode": input.runtime_mode,
            "modelSelection": input.model_selection,
            "interactionMode": input.interaction_mode.unwrap_or(InteractionMode::Default),
            "branch": input.branch,
            "worktreePath": input.worktree_path,
        });
        if let Some(mode) = input.workspace_mode {
            payload["workspaceMode"] = json!(mode);
        }
        if let Some(start_from_origin) = input.start_from_origin {
            payload["startFromOrigin"] = json!(start_from_origin);
        }
        let task = self
            .journal
            .begin_task(BeginTaskInput {
                idempotency_key: input.idempotency_key.clone(),
                payload_hash: hash_payload(&payload),
                project_id: input.project_id.clone(),
                title: input.title.clone(),
                runtime_mode: input.runtime_mode,
            })
            .map_err(journal_failure)?
            .record;
        let thread_operation = self
            .journal
            .get_by_idempotency_key(task["threadIdempotencyKey"].as_str().unwrap_or_default())
            .map_err(journal_failure)?;
        let run_operation = self
            .journal
            .get_by_idempotency_key(task["runIdempotencyKey"].as_str().unwrap_or_default())
            .map_err(journal_failure)?;
        if thread_operation.is_none() && run_operation.is_some() {
            return self.task_detail(&task, true).await;
        }

        let result = async {
            let created = self
                .thread_create(ThreadCreateInput {
                    project_id: input.project_id.clone(),
                    title: input.title.clone(),
                    model_selection: input.model_selection.clone(),
                    runtime_mode: input.runtime_mode,
                    interaction_mode: input.interaction_mode.unwrap_or(InteractionMode::Default),
                    branch: if matches!(workspace_mode, WorkspaceMode::Worktree) {
                        None
                    } else {
                        input.branch.clone()
                    },
                    worktree_path: if matches!(workspace_mode, WorkspaceMode::Worktree) {
                        None
                    } else {
                        input.worktree_path.clone()
                    },
                    managed_worktree: if matches!(workspace_mode, WorkspaceMode::Worktree) {
                        Some((
                            input.branch.clone().unwrap_or_default(),
                            input.start_from_origin.unwrap_or(false),
                        ))
                    } else {
                        None
                    },
                    idempotency_key: task["threadIdempotencyKey"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                })
                .await?;
            let created_status = created["status"].as_str().unwrap_or_default();
            let task = self
                .journal
                .update_task(
                    task["taskRef"].as_str().unwrap_or_default(),
                    TaskPatch {
                        thread_id: created["threadId"].as_str().map(str::to_owned),
                        thread_operation_id: created["operationId"].as_str().map(str::to_owned),
                        stage: Some(match created_status {
                            "accepted" => TaskStage::ThreadCreated,
                            "uncertain" => TaskStage::ThreadCreateUncertain,
                            _ => TaskStage::Rejected,
                        }),
                        last_error: Some(if created_status == "accepted" {
                            None
                        } else {
                            created["reason"].as_str().map(str::to_owned)
                        }),
                        ..TaskPatch::default()
                    },
                )
                .map_err(journal_failure)?;
            if created_status != "accepted" {
                return self.task_detail(&task, false).await;
            }

            if task.get("baselineAttribution").is_none() {
                let baseline = self
                    .git_status(
                        &json!({
                            "projectId": input.project_id,
                            "threadId": created["threadId"],
                            "maxEntries": 1,
                        })
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                    )
                    .await;
                let patch = match baseline {
                    Ok(status) => TaskPatch {
                        baseline_attribution: Some(if status["clean"] == true {
                            crate::operations::journal::BaselineAttribution::Clean
                        } else {
                            crate::operations::journal::BaselineAttribution::Dirty
                        }),
                        baseline_revision: status["branch"]["headCommit"]
                            .as_str()
                            .map(str::to_owned),
                        ..TaskPatch::default()
                    },
                    Err(_) => TaskPatch {
                        baseline_attribution: Some(
                            crate::operations::journal::BaselineAttribution::Unavailable,
                        ),
                        ..TaskPatch::default()
                    },
                };
                self.journal
                    .update_task(task["taskRef"].as_str().unwrap_or_default(), patch)
                    .map_err(journal_failure)?;
            }

            let initial_settings = self
                .journal
                .get_by_operation_id(created["operationId"].as_str().unwrap_or_default())
                .map_err(journal_failure)?
                .and_then(|record| record.get("settings").cloned())
                .map(serde_json::from_value)
                .transpose()
                .map_err(|error| ToolFailure::new("journal_invalid", error.to_string()))?;
            let sent = self
                .thread_send(ThreadSendInput {
                    thread_id: created["threadId"].as_str().unwrap_or_default().to_owned(),
                    message: input.instruction.clone(),
                    model_selection: input.model_selection.clone(),
                    runtime_mode: Some(input.runtime_mode),
                    interaction_mode: input.interaction_mode,
                    title_seed: Some(input.title.clone()),
                    initial_settings,
                    idempotency_key: task["runIdempotencyKey"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                })
                .await?;
            let sent_status = sent["status"].as_str().unwrap_or_default();
            let updated = self
                .journal
                .update_task(
                    task["taskRef"].as_str().unwrap_or_default(),
                    TaskPatch {
                        run_id: sent["runId"].as_str().map(str::to_owned),
                        message_id: sent["messageId"].as_str().map(str::to_owned),
                        run_operation_id: sent["operationId"].as_str().map(str::to_owned),
                        stage: Some(match sent_status {
                            "accepted" => TaskStage::RunAccepted,
                            "uncertain" => TaskStage::DispatchUncertain,
                            _ => TaskStage::DispatchRejected,
                        }),
                        last_error: Some(if sent_status == "accepted" {
                            None
                        } else {
                            sent["reason"].as_str().map(str::to_owned)
                        }),
                        ..TaskPatch::default()
                    },
                )
                .map_err(journal_failure)?;
            self.task_detail(&updated, false).await
        }
        .await;
        match result {
            Ok(task) => Ok(task),
            Err(error)
                if matches!(
                    error.code.as_str(),
                    "project_not_found" | "model_selection_required" | "thread_busy"
                ) =>
            {
                let task = self
                    .journal
                    .update_task(
                        task["taskRef"].as_str().unwrap_or_default(),
                        TaskPatch {
                            stage: Some(TaskStage::Rejected),
                            last_error: Some(Some(error.message)),
                            ..TaskPatch::default()
                        },
                    )
                    .map_err(journal_failure)?;
                self.task_detail(&task, false).await
            }
            Err(error) => Err(error),
        }
    }

    pub(super) async fn task_get(&self, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
        let input: TaskGetInput = parse_args(args)?;
        let record = self
            .journal
            .get_task_by_ref(&input.task_ref)
            .map_err(journal_failure)?
            .ok_or_else(|| {
                ToolFailure::new(
                    "task_not_found",
                    format!(
                        "Task {} was not found in the gateway journal.",
                        input.task_ref
                    ),
                )
            })?;
        let environment_id = self.task_environment_id().await?;
        Ok(json!({
            "environmentId": environment_id,
            "observedAt": now_iso(),
            "task": self.task_detail(&record, true).await?,
        }))
    }

    pub(super) async fn tasks_list(&self, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
        let input: TasksListInput = parse_args(args)?;
        let mut records = self.journal.list_tasks().map_err(journal_failure)?;
        records.retain(|task| {
            input
                .project_id
                .as_deref()
                .is_none_or(|project| task["projectId"] == project)
                && task_matches_query(input.query.as_deref(), task)
        });
        records.sort_by(|left, right| {
            right["updatedAt"]
                .as_str()
                .unwrap_or_default()
                .cmp(left["updatedAt"].as_str().unwrap_or_default())
                .then_with(|| {
                    left["taskRef"]
                        .as_str()
                        .unwrap_or_default()
                        .cmp(right["taskRef"].as_str().unwrap_or_default())
                })
        });
        let values = records.iter().cloned().map(Value::from).collect::<Vec<_>>();
        let mut page = paginate_values(&values, input.cursor.as_deref(), input.limit)?;
        let page_records = page["items"]
            .as_array()
            .into_iter()
            .flatten()
            .cloned()
            .map(crate::operations::journal::TaskRecord::try_from)
            .collect::<Result<Vec<_>, _>>()
            .map_err(journal_failure)?;
        let environment_id = self.task_environment_id().await?;
        page["items"] = json!(
            futures_util::future::try_join_all(
                page_records
                    .iter()
                    .map(|record| self.task_summary(record, true)),
            )
            .await?
        );
        Ok(json!({
            "environmentId": environment_id,
            "observedAt": now_iso(),
            "page": page,
        }))
    }

    async fn task_environment_id(&self) -> Result<String, ToolFailure> {
        let descriptor = match self.client.get_descriptor(None).await {
            Ok(descriptor) => Some(descriptor),
            Err(_) => self.client.cached_descriptor(),
        };
        if let Some(descriptor) = descriptor {
            self.assert_environment(&descriptor)?;
            return Ok(descriptor.environment_id);
        }
        Ok(self
            .config
            .environment_id
            .clone()
            .unwrap_or_else(|| "unknown".into()))
    }

    pub(super) async fn task_cancel(
        &self,
        args: &Map<String, Value>,
    ) -> Result<Value, ToolFailure> {
        authorize_native_mutation(self.config.read_only, &self.native.credential_state())?;
        let input: TaskCancelInput = parse_args(args)?;
        let mut get_args = Map::new();
        get_args.insert("taskRef".into(), json!(input.task_ref));
        let detail = self.task_get(&get_args).await?;
        let task = &detail["task"];
        let stage = task["stage"].as_str().unwrap_or("prepared");
        let thread_id = task["threadId"].as_str();
        let t3_run_id = task["run"]["t3TurnId"].as_str();
        let (Some(thread_id), Some(t3_run_id)) = (thread_id, t3_run_id) else {
            if thread_id.is_none() || !matches!(stage, "run_accepted" | "dispatch_uncertain") {
                return Err(ToolFailure::new(
                    "task_not_started",
                    format!(
                        "Task {} is at stage {stage} and started no run. {}",
                        input.task_ref,
                        task["nextAction"].as_str().unwrap_or_default()
                    ),
                ));
            }
            return Err(ToolFailure::new(
                "task_run_unknown",
                format!(
                    "T3 has not reported the run for task {}. Inspect thread {} with t3_thread_read, then use t3_thread_interrupt.",
                    input.task_ref,
                    thread_id.unwrap_or_default()
                ),
            ));
        };
        let mut interrupt = Map::new();
        interrupt.insert("threadId".into(), json!(thread_id));
        interrupt.insert("runId".into(), json!(t3_run_id));
        interrupt.insert(
            "clientRequestId".into(),
            json!(format!("task-cancel:{}:{t3_run_id}", input.task_ref)),
        );
        if let Some(reason) = input.reason {
            interrupt.insert("reason".into(), json!(reason));
        }
        let outcome = self
            .native
            .call_tool(
                json!({"name":"t3_thread_interrupt","arguments":interrupt})
                    .as_object()
                    .unwrap()
                    .clone(),
                true,
                None,
            )
            .await
            .map_err(native_failure)
            .and_then(|result| native_result_value(result, "t3_thread_interrupt"))?;
        let status = outcome["status"]
            .as_str()
            .ok_or_else(|| native_unexpected_result("t3_thread_interrupt"))?;
        Ok(json!({
            "taskRef": input.task_ref,
            "threadId": thread_id,
            "t3RunId": t3_run_id,
            "status": status,
            "nextAction": "Confirm the stop with t3_thread_wait or t3_task_get. Interrupt acceptance alone does not prove the run stopped.",
        }))
    }

    async fn task_summary(
        &self,
        record: &crate::operations::journal::TaskRecord,
        should_reconcile: bool,
    ) -> Result<Value, ToolFailure> {
        let thread_key = record["threadIdempotencyKey"].as_str().unwrap_or_default();
        let run_key = record["runIdempotencyKey"].as_str().unwrap_or_default();
        let stored_thread = self
            .journal
            .get_by_idempotency_key(thread_key)
            .map_err(journal_failure)?;
        let stored_run = self
            .journal
            .get_by_idempotency_key(run_key)
            .map_err(journal_failure)?;
        match stored_thread {
            Some(operation)
                if should_reconcile
                    && matches!(operation["status"].as_str(), Some("prepared" | "uncertain")) =>
            {
                self.reconcile(&operation).await?;
            }
            _ => {}
        };
        match stored_run {
            Some(operation)
                if should_reconcile
                    && matches!(operation["status"].as_str(), Some("prepared" | "uncertain")) =>
            {
                self.reconcile(&operation).await?;
            }
            _ => {}
        };
        let current = self
            .journal
            .refresh_task(
                record["taskRef"].as_str().unwrap_or_default(),
                should_reconcile,
            )
            .map_err(journal_failure)?;
        let stage = current["stage"].as_str().unwrap_or("prepared");
        Ok(json!({
            "taskRef": current["taskRef"],
            "projectId": current["projectId"],
            "title": current["title"],
            "runtimeMode": current["runtimeMode"],
            "stage": stage,
            "threadId": current.get("threadId").cloned().unwrap_or(Value::Null),
            "runId": current.get("runId").cloned().unwrap_or(Value::Null),
            "messageId": current.get("messageId").cloned().unwrap_or(Value::Null),
            "threadOperationId": current.get("threadOperationId").cloned().unwrap_or(Value::Null),
            "runOperationId": current.get("runOperationId").cloned().unwrap_or(Value::Null),
            "baselineRevision": current.get("baselineRevision").cloned().unwrap_or(Value::Null),
            "baselineAttribution": current.get("baselineAttribution").and_then(Value::as_str).unwrap_or("unavailable"),
            "createdAt": current["createdAt"],
            "updatedAt": current["updatedAt"],
            "nextAction": task_next_action(stage, current["taskRef"].as_str().unwrap_or_default()),
            "lastError": current.get("lastError").cloned().unwrap_or(Value::Null),
        }))
    }

    async fn task_detail(
        &self,
        record: &crate::operations::journal::TaskRecord,
        should_reconcile: bool,
    ) -> Result<Value, ToolFailure> {
        let summary = self.task_summary(record, should_reconcile).await?;
        let thread = match summary["threadId"].as_str() {
            Some(thread_id) => match self.thread_get(thread_id).await {
                Ok(result) => result.get("thread").cloned().unwrap_or(Value::Null),
                Err(error) if error.code == "environment_mismatch" => return Err(error),
                Err(_) => Value::Null,
            },
            None => Value::Null,
        };
        let run = match summary["runId"].as_str() {
            Some(run_id) => match self.run_get(run_id).await {
                Ok(result) => result,
                Err(error) if error.code == "environment_mismatch" => return Err(error),
                Err(_) => Value::Null,
            },
            None => Value::Null,
        };
        let mut detail = summary.as_object().cloned().unwrap_or_default();
        detail.insert("thread".into(), thread);
        detail.insert("run".into(), run.clone());
        let settings = run
            .get("settings")
            .filter(|value| !value.is_null())
            .cloned()
            .or_else(|| {
                let id = summary["runOperationId"]
                    .as_str()
                    .or_else(|| summary["threadOperationId"].as_str())?;
                self.journal
                    .get_by_operation_id(id)
                    .ok()
                    .flatten()
                    .and_then(|operation| operation.get("settings").cloned())
                    .and_then(|value| {
                        serde_json::from_value::<crate::settings::SettingsReceipt>(value).ok()
                    })
                    .map(|settings| {
                        serde_json::to_value(crate::settings::observe_settings(&settings, None))
                            .unwrap_or(Value::Null)
                    })
            });
        if let Some(settings) = settings {
            detail.insert("settings".into(), settings);
        }
        Ok(Value::Object(detail))
    }

    async fn thread_create(&self, input: ThreadCreateInput) -> Result<Value, ToolFailure> {
        let payload = json!({
            "projectId": input.project_id,
            "title": input.title,
            "modelSelection": input.model_selection,
            "runtimeMode": input.runtime_mode,
            "interactionMode": input.interaction_mode,
            "branch": input.branch,
            "worktreePath": input.worktree_path,
            "managedWorktree": input.managed_worktree.as_ref().map(|(base_branch, start_from_origin)| json!({
                "baseBranch": base_branch,
                "startFromOrigin": start_from_origin,
            })),
        });
        if self.config.read_only {
            self.require_operation_scope().await?;
        }
        let payload_hash = hash_payload(&payload);
        if let Some(existing) = self
            .journal
            .get_by_idempotency_key(&input.idempotency_key)
            .map_err(journal_failure)?
        {
            let begun = self
                .journal
                .begin(BeginOperationInput {
                    kind: WritableOperationKind::ThreadCreate,
                    idempotency_key: input.idempotency_key,
                    payload_hash,
                    command_id: None,
                    project_id: existing
                        .get("projectId")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or(Some(input.project_id.clone())),
                    thread_id: existing
                        .get("threadId")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    run_id: None,
                    message_id: None,
                    turn_id: None,
                    settings: None,
                })
                .map_err(journal_failure)?;
            let reconciled = self.reconcile(&begun.record).await?;
            return self
                .thread_create_result(
                    &reconciled,
                    &input.project_id,
                    input.model_selection.as_ref(),
                    input.branch.as_deref(),
                    input.worktree_path.as_deref(),
                )
                .await;
        }
        self.require_operation_scope().await?;
        let shell = self
            .client
            .get_shell(None)
            .await
            .map_err(ToolFailure::from)?;
        let project = shell
            .projects
            .iter()
            .find(|project| project.id == input.project_id)
            .ok_or_else(|| {
                ToolFailure::new(
                    "project_not_found",
                    format!(
                        "Project {} was not found. Call t3_project_list to discover available project IDs.",
                        input.project_id
                    ),
                )
            })?;
        if let Some(path) = input.worktree_path.as_deref() {
            match std::fs::metadata(path) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => {
                    return Err(ToolFailure::new(
                        "workspace_not_directory",
                        format!("The requested thread workspace is not a directory: {path}"),
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(ToolFailure::new(
                        "workspace_missing",
                        format!(
                            "The requested thread workspace does not exist: {path}. Use t3_task_start with workspaceMode=worktree to have the gateway create an isolated worktree."
                        ),
                    ));
                }
                Err(_) => {
                    return Err(ToolFailure::new(
                        "workspace_unreadable",
                        format!("The requested thread workspace cannot be inspected: {path}"),
                    ));
                }
            }
        }
        let model_selection = input
            .model_selection
            .clone()
            .or_else(|| project.default_model_selection.clone())
            .ok_or_else(|| {
                ToolFailure::new(
                    "model_selection_required",
                    "The project has no default model. Call orchestrator_capabilities to discover available instanceId/model values, then supply modelSelection.",
                )
            })?;
        let descriptor = self
            .client
            .get_descriptor(None)
            .await
            .map_err(ToolFailure::from)?;
        self.assert_environment(&descriptor)?;
        let thread_id = uuid::Uuid::new_v4().to_string();
        let resolved_model = settings_model(&model_selection)?;
        let receipt = crate::settings::SettingsReceipt {
            requested: crate::settings::RequestedSettings {
                model_selection: input
                    .model_selection
                    .as_ref()
                    .map(settings_model)
                    .transpose()?
                    .as_ref()
                    .map(crate::settings::safe_model_selection),
                runtime_mode: Some(input.runtime_mode),
            },
            resolved: crate::settings::ResolvedSettings {
                model_selection: crate::settings::safe_model_selection(&resolved_model),
                runtime_mode: input.runtime_mode,
            },
            model_source: if input.model_selection.is_some() {
                crate::settings::ModelSource::Explicit
            } else {
                crate::settings::ModelSource::ProjectDefault
            },
            runtime_source: crate::settings::RuntimeSource::Explicit,
        };
        let begun = self
            .journal
            .begin(BeginOperationInput {
                kind: WritableOperationKind::ThreadCreate,
                idempotency_key: input.idempotency_key,
                payload_hash,
                command_id: None,
                project_id: Some(input.project_id.clone()),
                thread_id: Some(thread_id.clone()),
                run_id: None,
                message_id: None,
                turn_id: None,
                settings: Some(receipt),
            })
            .map_err(journal_failure)?;
        if begun.reused {
            let reconciled = self.reconcile(&begun.record).await?;
            return self
                .thread_create_result(
                    &reconciled,
                    &input.project_id,
                    Some(&model_selection),
                    input.branch.as_deref(),
                    input.worktree_path.as_deref(),
                )
                .await;
        }
        let mut branch = input.branch.clone();
        let mut worktree_path = input.worktree_path.clone();
        if let Some((base_branch, start_from_origin)) = &input.managed_worktree {
            let worktree = match self
                .git_worktrees
                .prepare(&ManagedWorktreeRequest {
                    project_id: input.project_id.clone(),
                    project_workspace_root: project.workspace_root.clone(),
                    base_branch: base_branch.clone(),
                    start_from_origin: *start_from_origin,
                    operation_key: begun.record["commandId"]
                        .as_str()
                        .unwrap_or_default()
                        .into(),
                })
                .await
            {
                Ok(worktree) => worktree,
                Err(error) => {
                    let rejected = self
                        .journal
                        .update(
                            begun.record["operationId"].as_str().unwrap_or_default(),
                            OperationPatch {
                                status: Some(OperationStatus::Rejected),
                                last_error: Some(error.to_string()),
                                ..OperationPatch::default()
                            },
                        )
                        .map_err(journal_failure)?;
                    return self
                        .thread_create_result(
                            &rejected,
                            &input.project_id,
                            Some(&model_selection),
                            branch.as_deref(),
                            worktree_path.as_deref(),
                        )
                        .await;
                }
            };
            branch = Some(worktree.branch);
            worktree_path = Some(worktree.path);
        }
        let command = T3Command::Create {
            command_id: begun.record["commandId"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            thread_id,
            project_id: input.project_id.clone(),
            title: input.title,
            model_selection: model_selection.clone(),
            runtime_mode: input.runtime_mode,
            interaction_mode: input.interaction_mode,
            branch: branch.clone(),
            worktree_path: worktree_path.clone(),
            created_at: now_iso(),
        };
        let result = self.dispatch_new(&begun.record, &command).await?;
        self.thread_create_result(
            &result,
            &input.project_id,
            Some(&model_selection),
            branch.as_deref(),
            worktree_path.as_deref(),
        )
        .await
    }

    async fn thread_create_result(
        &self,
        record: &OperationRecord,
        project_id: &str,
        model_selection: Option<&ModelSelection>,
        branch: Option<&str>,
        worktree_path: Option<&str>,
    ) -> Result<Value, ToolFailure> {
        let mut result = self.mutation_result(record).await?;
        result["projectId"] = json!(project_id);
        result["threadId"] = record.get("threadId").cloned().unwrap_or(json!("unknown"));
        result["modelSelection"] = model_selection
            .map(serde_json::to_value)
            .transpose()
            .map_err(|error| ToolFailure::new("gateway_error", error.to_string()))?
            .unwrap_or(Value::Null);
        result["workspace"] = json!({
            "branch": branch,
            "worktreePath": worktree_path,
        });
        if record["status"] == "accepted"
            && let Some(thread_id) = record["threadId"].as_str()
            && let Ok(snapshot) = self.client.get_thread(thread_id, None).await
        {
            result["projectId"] = json!(snapshot.thread.shell.project_id);
            result["threadId"] = json!(snapshot.thread.shell.id);
            result["modelSelection"] =
                serde_json::to_value(snapshot.thread.shell.model_selection).unwrap_or(Value::Null);
            result["workspace"] = json!({
                "branch": snapshot.thread.shell.extra.get("branch"),
                "worktreePath": snapshot.thread.shell.extra.get("worktreePath"),
            });
        }
        Ok(result)
    }

    async fn thread_send(&self, input: ThreadSendInput) -> Result<Value, ToolFailure> {
        let payload = json!({
            "threadId": input.thread_id,
            "message": input.message,
            "modelSelection": input.model_selection,
            "runtimeMode": input.runtime_mode,
            "interactionMode": input.interaction_mode,
            "titleSeed": input.title_seed,
        });
        if self.config.read_only {
            self.require_operation_scope().await?;
        }
        let payload_hash = hash_payload(&payload);
        if let Some(existing) = self
            .journal
            .get_by_idempotency_key(&input.idempotency_key)
            .map_err(journal_failure)?
        {
            let begun = self
                .journal
                .begin(BeginOperationInput {
                    kind: WritableOperationKind::ThreadTurnStart,
                    idempotency_key: input.idempotency_key,
                    payload_hash,
                    command_id: None,
                    project_id: existing
                        .get("projectId")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    thread_id: Some(input.thread_id),
                    run_id: Some(
                        existing
                            .get("runId")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("run_{}", uuid::Uuid::new_v4())),
                    ),
                    message_id: Some(
                        existing
                            .get("messageId")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                            .unwrap_or_else(|| {
                                format!("user:msg_{}", uuid::Uuid::new_v4().simple())
                            }),
                    ),
                    turn_id: None,
                    settings: None,
                })
                .map_err(journal_failure)?;
            let record = self.reconcile(&begun.record).await?;
            return self.thread_send_result(&record).await;
        }
        self.require_operation_scope().await?;
        let snapshot = self
            .client
            .get_thread(&input.thread_id, None)
            .await
            .map_err(ToolFailure::from)?;
        if let Some(concurrent) = self
            .journal
            .get_by_idempotency_key(&input.idempotency_key)
            .map_err(journal_failure)?
        {
            let begun = self
                .journal
                .begin(BeginOperationInput {
                    kind: WritableOperationKind::ThreadTurnStart,
                    idempotency_key: input.idempotency_key,
                    payload_hash,
                    command_id: None,
                    project_id: concurrent
                        .get("projectId")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or(Some(snapshot.thread.shell.project_id.clone())),
                    thread_id: Some(input.thread_id),
                    run_id: Some(
                        concurrent
                            .get("runId")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("run_{}", uuid::Uuid::new_v4())),
                    ),
                    message_id: Some(
                        concurrent
                            .get("messageId")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                            .unwrap_or_else(|| {
                                format!("user:msg_{}", uuid::Uuid::new_v4().simple())
                            }),
                    ),
                    turn_id: None,
                    settings: None,
                })
                .map_err(journal_failure)?;
            let record = self.reconcile(&begun.record).await?;
            return self.thread_send_result(&record).await;
        }
        if thread_is_busy(&snapshot.thread) {
            let observation = observe_thread(&snapshot.thread.shell, Utc::now().timestamp_millis());
            let active_turn = snapshot
                .thread
                .shell
                .latest_turn
                .as_ref()
                .map(|turn| turn.turn_id.as_str())
                .unwrap_or("unknown");
            let session_status = snapshot
                .thread
                .shell
                .session
                .as_ref()
                .map(|session| session.status.as_str())
                .unwrap_or("unknown");
            return Err(ToolFailure::new(
                "thread_busy",
                format!(
                    "Thread {} is busy (execution={}, turn={active_turn}, session={session_status}, quality={}). Valid next actions: wait with t3_thread_wait, steer or queue with t3_thread_send mode=steer or mode=queue, or interrupt the observed run {active_turn} with t3_thread_interrupt.",
                    input.thread_id,
                    activity_name(observation.execution),
                    observation_quality_name(observation.quality),
                ),
            ));
        }
        let run_id = format!("run_{}", uuid::Uuid::new_v4());
        let message_id = format!("user:msg_{}", uuid::Uuid::new_v4().simple());
        let selected_model = input
            .model_selection
            .as_ref()
            .unwrap_or(&snapshot.thread.shell.model_selection);
        let resolved_model = settings_model(selected_model)?;
        let receipt = input
            .initial_settings
            .unwrap_or_else(|| crate::settings::SettingsReceipt {
                requested: crate::settings::RequestedSettings {
                    model_selection: input
                        .model_selection
                        .as_ref()
                        .map(settings_model)
                        .transpose()
                        .ok()
                        .flatten()
                        .as_ref()
                        .map(crate::settings::safe_model_selection),
                    runtime_mode: input.runtime_mode,
                },
                resolved: crate::settings::ResolvedSettings {
                    model_selection: crate::settings::safe_model_selection(&resolved_model),
                    runtime_mode: input
                        .runtime_mode
                        .unwrap_or(snapshot.thread.shell.runtime_mode),
                },
                model_source: if input.model_selection.is_some() {
                    crate::settings::ModelSource::Explicit
                } else {
                    crate::settings::ModelSource::ThreadInherited
                },
                runtime_source: if input.runtime_mode.is_some() {
                    crate::settings::RuntimeSource::Explicit
                } else {
                    crate::settings::RuntimeSource::ThreadInherited
                },
            });
        let begun = self
            .journal
            .begin(BeginOperationInput {
                kind: WritableOperationKind::ThreadTurnStart,
                idempotency_key: input.idempotency_key,
                payload_hash,
                command_id: None,
                project_id: Some(snapshot.thread.shell.project_id.clone()),
                thread_id: Some(input.thread_id.clone()),
                run_id: Some(run_id),
                message_id: Some(message_id.clone()),
                turn_id: None,
                settings: Some(receipt),
            })
            .map_err(journal_failure)?;
        if begun.reused {
            let record = self.reconcile(&begun.record).await?;
            return self.thread_send_result(&record).await;
        }
        let command = T3Command::TurnStart {
            command_id: begun.record["commandId"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            thread_id: input.thread_id,
            message: UserMessage {
                message_id,
                role: UserRole::User,
                text: input.message,
                attachments: Vec::new(),
            },
            model_selection: input.model_selection,
            title_seed: input.title_seed,
            runtime_mode: input
                .runtime_mode
                .unwrap_or(snapshot.thread.shell.runtime_mode),
            interaction_mode: input
                .interaction_mode
                .or(snapshot.thread.shell.interaction_mode)
                .unwrap_or(InteractionMode::Default),
            created_at: now_iso(),
        };
        let result = self.dispatch_new(&begun.record, &command).await?;
        self.thread_send_result(&result).await
    }

    async fn thread_send_result(&self, record: &OperationRecord) -> Result<Value, ToolFailure> {
        let mut result = self.mutation_result(record).await?;
        for field in ["projectId", "threadId", "runId", "messageId"] {
            result[field] = record.get(field).cloned().unwrap_or(json!("unknown"));
        }
        result["t3TurnId"] = record.get("turnId").cloned().unwrap_or(Value::Null);
        result["providerTurnId"] = Value::Null;
        result["nextAction"] = json!("Use t3_task_get, or t3_thread_wait with the threadId.");
        let snapshot = if record.get("settings").is_some() && record["status"] == "accepted" {
            match record["threadId"].as_str() {
                Some(thread) => self.client.get_thread(thread, None).await.ok(),
                None => None,
            }
        } else {
            None
        };
        let observed = snapshot
            .as_ref()
            .filter(|snapshot| {
                snapshot
                    .thread
                    .messages
                    .iter()
                    .find(|message| Some(message.id.as_str()) == record["messageId"].as_str())
                    .and_then(|message| message.turn_id.as_deref())
                    .is_some_and(|turn| {
                        snapshot
                            .thread
                            .shell
                            .latest_turn
                            .as_ref()
                            .is_some_and(|latest| latest.turn_id == turn)
                    })
            })
            .map(|snapshot| &snapshot.thread.shell);
        run::add_settings(&mut result, record, observed)?;
        Ok(result)
    }
}

fn default_result_bytes() -> usize {
    100_000
}

fn settings_model(model: &ModelSelection) -> Result<crate::settings::ModelSelection, ToolFailure> {
    serde_json::from_value(
        serde_json::to_value(model)
            .map_err(|error| ToolFailure::new("invalid_arguments", error.to_string()))?,
    )
    .map_err(|error| ToolFailure::new("invalid_arguments", error.to_string()))
}

fn thread_is_busy(thread: &Thread) -> bool {
    thread
        .shell
        .latest_turn
        .as_ref()
        .is_some_and(|turn| turn.state == crate::t3::types::TurnState::Running)
        || thread
            .shell
            .session
            .as_ref()
            .is_some_and(|session| matches!(session.status.as_str(), "starting" | "running"))
}

fn task_next_action(stage: &str, task_ref: &str) -> String {
    match stage {
        "run_accepted" => format!("Use t3_task_get with taskRef {task_ref} to inspect current work."),
        "thread_created" => "Retry t3_task_start with the same idempotency key and identical original input to dispatch the initial instruction.".into(),
        "dispatch_rejected" => "The initial dispatch was definitively rejected. Inspect the thread, correct the cause, then send a corrected message with t3_thread_send or start a replacement task.".into(),
        "thread_create_uncertain" | "dispatch_uncertain" => format!("Use t3_task_get with taskRef {task_ref} to reconcile; do not start a replacement task."),
        "prepared" => "Retry t3_task_start with the same idempotency key and identical original input.".into(),
        "rejected" => "Correct the reported input or permission problem, then start a new task with a new idempotency key.".into(),
        _ => unreachable!("TaskRecord validation accepts only known stages"),
    }
}

fn task_matches_query(query: Option<&str>, task: &crate::operations::journal::TaskRecord) -> bool {
    let Some(query) = query.map(str::trim).filter(|query| !query.is_empty()) else {
        return true;
    };
    let query = query.to_lowercase();
    ["title", "taskRef", "threadId"].into_iter().any(|field| {
        task[field]
            .as_str()
            .is_some_and(|value| value.to_lowercase().contains(&query))
    })
}

fn observation_quality_name(quality: crate::t3::thread_state::ObservationQuality) -> &'static str {
    use crate::t3::thread_state::ObservationQuality;
    match quality {
        ObservationQuality::Fresh => "fresh",
        ObservationQuality::Stale => "stale",
        ObservationQuality::Incomplete => "incomplete",
        ObservationQuality::Inconsistent => "inconsistent",
    }
}
