use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{
    config::GatewayConfig,
    contract::{
        GATEWAY_TOOL_NAMES, GatewayTool, gateway_tools, is_native_mutating_tool,
        is_native_tool_name, native_tool_names, tool_schema_fingerprint,
    },
    git::{
        GitDiffMode, GitInspectionError, GitInspector, GitWorkspaceSelection, GitWorkspaceSource,
        GitWorktreeManager, ManagedWorktreeRequest,
    },
    mcp::{Invocation, ToolCall, ToolFailure, ToolHandler, ToolResult, ToolsListPage},
    native_credential::{NativeCredentialState, NativeCredentialStore},
    operations::{
        audit_log::{
            AuditEventInput, AuditLog, AuditQueryInput, AuditSource, AuditValue,
            summarize_for_audit, with_audit_context,
        },
        journal::{
            BeginOperationInput, BeginTaskInput, FailureEvidenceOrder, OperationJournal,
            OperationPatch, OperationStatus, RecoveryState, TaskPatch, TaskStage, TerminalRecovery,
            WritableOperationKind, hash_payload,
        },
    },
    t3::{
        NativeMcpClient, T3HttpClient,
        commands::{ApprovalDecision, T3Command, UserMessage, UserRole},
        failure::{failure_info, shell_failure_info},
        thread_state::{
            ThreadActivity, ThreadStatus, has_conflicting_signals, is_in_open_queue,
            is_thread_running, needs_attention_for, observe_thread, thread_activity, thread_status,
            thread_status_reason,
        },
        types::{
            Descriptor, InteractionMode, Message, MessageRole, ModelSelection, Thread, ThreadShell,
            TurnState,
        },
    },
};

mod task;

const MUTATING_OPERATIONS: &[&str] = &[
    "t3_pending_action_respond",
    "t3_attachment_upload",
    "t3_task_start",
    "t3_task_cancel",
];
const IMAGE_MIME_TYPES: &[&str] = &["image/gif", "image/jpeg", "image/png", "image/webp"];
const MAX_IMAGE_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;
const MAX_FILE_ATTACHMENT_BYTES: usize = 50 * 1024 * 1024;

pub struct T3Gateway {
    config: GatewayConfig,
    client: T3HttpClient,
    journal: OperationJournal,
    audit: Arc<AuditLog>,
    git: GitInspector,
    git_worktrees: GitWorktreeManager,
    native: NativeMcpClient,
    read_clock: AtomicU64,
}

struct SummaryFailureObservation {
    failure: Option<Value>,
    order: FailureEvidenceOrder,
}

impl T3Gateway {
    pub fn new(config: GatewayConfig) -> Self {
        let _ = gateway_tools();
        let audit = Arc::new(AuditLog::new(config.endpoint.data_dir.join("audit.jsonl")));
        let client = T3HttpClient::new(
            config.endpoint.t3_http_base_url.clone(),
            config.t3_access_token.clone(),
            Duration::from_secs(15),
            Some(Arc::clone(&audit)),
        );
        let journal = OperationJournal::new(
            config.endpoint.data_dir.join("operations.json"),
            Some(Arc::clone(&audit)),
        );
        let git =
            GitInspector::new().with_audit_log(crate::git::audit_log_sink(Arc::clone(&audit)));
        let git_worktrees = GitWorktreeManager::new(config.worktree_root.clone())
            .with_audit_log(crate::git::audit_log_sink(Arc::clone(&audit)));
        let native = NativeMcpClient::new(
            config.endpoint.t3_native_mcp_url.clone(),
            NativeCredentialStore::in_data_dir(&config.endpoint.data_dir),
            Some(Arc::clone(&audit)),
            Duration::from_secs(120),
        );
        Self {
            config,
            client,
            journal,
            audit,
            git,
            git_worktrees,
            native,
            read_clock: AtomicU64::new(0),
        }
    }

    pub fn config(&self) -> &GatewayConfig {
        &self.config
    }

    pub fn audit_log(&self) -> &AuditLog {
        &self.audit
    }

    fn read_started_at(&self) -> u64 {
        let wall_time = Utc::now().timestamp_millis().max(0) as u64;
        let mut previous = self.read_clock.load(Ordering::Relaxed);
        loop {
            let next = wall_time.max(previous.saturating_add(1));
            match self.read_clock.compare_exchange_weak(
                previous,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return next,
                Err(current) => previous = current,
            }
        }
    }

    pub async fn connection_status(&self) -> Value {
        let mut descriptor = None;
        let mut scopes = Vec::new();
        let mut session_expires_at = None;
        let mut failure = None;
        match self.client.get_descriptor(None).await {
            Ok(value) => {
                if let Some(expected) = &self.config.environment_id
                    && value.environment_id != *expected
                {
                    failure = Some(format!(
                        "Configured environment {expected} does not match the connected T3 environment."
                    ));
                } else {
                    descriptor = Some(value);
                }
            }
            Err(error) => failure = Some(error.to_string()),
        }
        if failure.is_none() {
            match self.client.get_session(None).await {
                Ok(session) => {
                    scopes = session.scopes.unwrap_or_default();
                    session_expires_at = session.expires_at;
                    if !session.authenticated {
                        failure = Some("The T3 session is not authenticated.".into());
                    }
                }
                Err(error) => failure = Some(error.to_string()),
            }
        }
        if failure.is_none()
            && let Err(error) = self.client.get_shell(None).await
        {
            failure = Some(error.to_string());
        }
        if descriptor.is_none() {
            descriptor = self.client.cached_descriptor();
        }

        let telemetry = self.client.telemetry();
        let last_observed_at = telemetry.last_snapshot_at.or(telemetry.last_successful_at);
        let observed_at = now_iso();
        let (native, native_rejected) = self.native_status().await;
        let has_operate =
            failure.is_none() && scopes.iter().any(|scope| scope == "orchestration:operate");
        let access = if self.config.read_only || !has_operate {
            "read-only"
        } else {
            "read-write"
        };
        let gateway_operations = GATEWAY_TOOL_NAMES
            .iter()
            .filter(|name| !self.config.read_only || !MUTATING_OPERATIONS.contains(name))
            .copied()
            .collect::<Vec<_>>();
        let (callable, disabled) = self.partition_operations(&native, native_rejected, has_operate);
        let environment = descriptor.as_ref().map(|value| {
            json!({
                "environmentId": value.environment_id,
                "label": value.label,
                "serverVersion": value.server_version,
            })
        }).or_else(|| self.config.environment_id.as_ref().map(|id| json!({
            "environmentId": id,
            "label": self.config.environment_label.as_deref().unwrap_or("configured T3 environment"),
            "serverVersion": "unknown",
        })));
        let mut result = json!({
            "environment": environment,
            "connectionStatus": if failure.is_some() {"disconnected"} else {"connected"},
            "stateFreshness": freshness(last_observed_at.as_deref(), self.config.stale_after_ms),
            "lastObservedAt": last_observed_at,
            "observedAt": observed_at,
            "gatewayVersion": crate::contract::GATEWAY_VERSION,
            "gatewayCommit": option_env!("T3_CODE_MCP_COMMIT").unwrap_or("unknown"),
            "toolSchemaFingerprint": tool_schema_fingerprint(),
            "effectiveAccessMode": access,
            "callableOperations": callable,
            "disabledOperations": disabled,
            "supportedCapabilities": descriptor.as_ref().map(|value| {
                let mut names = value.capabilities.keys().cloned().collect::<Vec<_>>();
                names.sort();
                names
            }).unwrap_or_default(),
            "permittedOperations": permitted_operations(&scopes),
            "t3Scopes": scopes,
            "upstreamScopes": scopes,
            "gatewayOperations": gateway_operations,
            "sessionExpiresAt": session_expires_at,
            "native": native,
        });
        if let Some(failure) = failure {
            result["error"] = Value::String(failure);
        }
        result
    }

    async fn native_status(&self) -> (Value, bool) {
        let state = self.native.credential_state();
        let snapshot = self.native.snapshot().await;
        let (credential, access, scopes, expires_at, error) = match state {
            NativeCredentialState::Missing { reason } => {
                ("missing", None, Vec::<String>::new(), None, Some(reason))
            }
            NativeCredentialState::Expired { credential } => (
                "expired",
                Some(credential.access),
                credential.scopes,
                Some(credential.expires_at),
                None,
            ),
            NativeCredentialState::Valid { credential } => (
                "valid",
                Some(credential.access),
                credential.scopes,
                Some(credential.expires_at),
                None,
            ),
        };
        let rejected = snapshot
            .error
            .as_ref()
            .is_some_and(|error| error.code == "native_credential_rejected");
        let status = json!({
            "url": self.config.endpoint.t3_native_mcp_url,
            "credential": credential,
            "access": access,
            "scopes": scopes,
            "expiresAt": expires_at,
            "catalogFetchedAt": snapshot.fetched_at,
            "availableTools": snapshot.tools.len(),
            "missingTools": snapshot.missing,
            "error": snapshot.error.map(|value| value.message).or(error),
        });
        (status, rejected)
    }

    fn partition_operations(
        &self,
        native: &Value,
        native_rejected: bool,
        has_operate: bool,
    ) -> (Vec<String>, Vec<Value>) {
        let native_credential = native["credential"] == "valid" && !native_rejected;
        let native_usable = native_credential && native["error"].is_null();
        let native_operate = native_credential
            && native["scopes"]
                .as_array()
                .is_some_and(|scopes| scopes.iter().any(|scope| scope == "orchestration:operate"));
        let mut callable = Vec::new();
        let mut disabled = Vec::new();
        for operation in GATEWAY_TOOL_NAMES
            .iter()
            .chain(native_tool_names().collect::<Vec<_>>().iter())
        {
            let blocker = if is_native_tool_name(operation) {
                (!native_credential || !native_tool_available(native, operation))
                    .then(|| native_block_reason(native))
            } else if let Some(required) = match *operation {
                "t3_attachment_upload" => Some("t3_attachment_prepare_upload"),
                "t3_task_cancel" => Some("t3_thread_interrupt"),
                _ => None,
            } {
                if !native_usable {
                    Some(native_block_reason(native))
                } else {
                    native["missingTools"]
                        .as_array()
                        .is_some_and(|missing| missing.iter().any(|name| name == required))
                        .then(|| {
                            format!("Native T3 does not offer {required}, which {operation} needs.")
                        })
                }
            } else {
                None
            };
            if let Some(reason) = blocker {
                disabled.push(json!({"operation":operation,"reasonCode":"native_unavailable","reason":reason}));
            } else if (MUTATING_OPERATIONS.contains(operation)
                || is_native_mutating_tool(operation))
                && self.config.read_only
            {
                disabled.push(json!({
                    "operation": operation,
                    "reasonCode": "gateway_read_only",
                    "reason": "Gateway is configured read-only (MCP_READ_ONLY=true). Restart with MCP_READ_ONLY=false to enable control.",
                }));
            } else if (MUTATING_OPERATIONS.contains(operation)
                || is_native_mutating_tool(operation))
                && if is_native_tool_name(operation)
                    || matches!(*operation, "t3_attachment_upload" | "t3_task_cancel")
                {
                    !native_operate
                } else {
                    !has_operate
                }
            {
                disabled.push(json!({
                    "operation": operation,
                    "reasonCode": "t3_scope_required",
                    "reason": if is_native_tool_name(operation) || matches!(*operation, "t3_attachment_upload" | "t3_task_cancel") {
                        "The native MCP credential lacks orchestration:operate. Run native-login with a control access level."
                    } else {
                        "Upstream T3 credential lacks orchestration:operate. Mutations will be rejected until the scope is granted."
                    },
                }));
            } else {
                callable.push((*operation).to_owned());
            }
        }
        (callable, disabled)
    }

    async fn route(
        &self,
        call: &ToolCall,
        invocation: Option<&Invocation>,
    ) -> Result<ToolResult, ToolFailure> {
        if is_native_tool_name(&call.name) {
            if is_native_mutating_tool(&call.name) {
                authorize_native_mutation(self.config.read_only, &self.native.credential_state())?;
            }
            let native = invocation
                .map(|invocation| invocation.peer().native(&self.native))
                .unwrap_or(&self.native);
            let result = native
                .call_tool(
                    call.params.clone(),
                    is_native_mutating_tool(&call.name),
                    invocation.cloned(),
                )
                .await
                .map_err(native_failure)?;
            return ToolResult::new(&result)
                .map_err(|error| ToolFailure::new("gateway_error", error.to_string()));
        }
        let tool = GatewayTool::parse(&call.name).ok_or_else(|| {
            ToolFailure::new(
                "gateway_error",
                format!("Unknown gateway tool {}.", call.name),
            )
        })?;
        let value = match tool {
            GatewayTool::ConnectionStatus => self.connection_status().await,
            GatewayTool::AuditLog => return self.audit_query(&call.arguments),
            GatewayTool::GitStatus => self.git_status(&call.arguments).await?,
            GatewayTool::GitDiff => self.git_diff(&call.arguments).await?,
            GatewayTool::GitCompare => self.git_compare(&call.arguments).await?,
            GatewayTool::ThreadsList => self.threads_list(&call.arguments).await?,
            GatewayTool::ThreadsOverview => self.threads_overview(&call.arguments).await?,
            GatewayTool::PendingActionsList => self.pending_actions_list(&call.arguments).await?,
            GatewayTool::PendingActionRespond => {
                self.pending_action_respond(&call.arguments).await?
            }
            GatewayTool::AttachmentUpload => self.attachment_upload(&call.arguments).await?,
            GatewayTool::TaskStart => self.task_start(&call.arguments).await?,
            GatewayTool::TaskGet => self.task_get(&call.arguments).await?,
            GatewayTool::TasksList => self.tasks_list(&call.arguments).await?,
            GatewayTool::TaskCancel => self.task_cancel(&call.arguments).await?,
            GatewayTool::ResultGet => self.result_get(&call.arguments).await?,
        };
        ToolResult::structured(&value)
            .map_err(|error| ToolFailure::new("gateway_error", error.to_string()))
    }

    fn audit_query(&self, args: &Map<String, Value>) -> Result<ToolResult, ToolFailure> {
        let input = AuditQueryInput {
            since: optional_string(args, "since")?,
            until: optional_string(args, "until")?,
            source: optional_string(args, "source")?
                .map(|value| {
                    serde_json::from_value(Value::String(value)).map_err(|error| {
                        ToolFailure::new("invalid_audit_filter", error.to_string())
                    })
                })
                .transpose()?,
            event: optional_string(args, "event")?,
            operation: optional_string(args, "operation")?,
            outcome: optional_string(args, "outcome")?,
            cursor: optional_string(args, "cursor")?,
            limit: args.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize,
        };
        let page = self
            .audit
            .query(&input)
            .map_err(|error| ToolFailure::new("invalid_audit_filter", error.to_string()))?;
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct AuditResult {
            environment_id: String,
            observed_at: String,
            audit_error: Option<String>,
            page: crate::operations::audit_log::AuditLogPage,
        }
        ToolResult::structured(&AuditResult {
            environment_id: self
                .client
                .cached_descriptor()
                .map(|value| value.environment_id)
                .or_else(|| self.config.environment_id.clone())
                .unwrap_or_else(|| "unknown".into()),
            observed_at: now_iso(),
            audit_error: self.audit.diagnostic_error(),
            page,
        })
        .map_err(|error| ToolFailure::new("gateway_error", error.to_string()))
    }

    async fn git_status(&self, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
        let selection = self.git_selection(args).await?;
        let limit = args
            .get("maxEntries")
            .and_then(Value::as_u64)
            .unwrap_or(100) as usize;
        let value = self
            .git
            .status(&selection, limit)
            .await
            .map_err(git_failure)?;
        serde_json::to_value(value)
            .map_err(|error| ToolFailure::new("gateway_error", error.to_string()))
    }

    async fn git_diff(&self, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
        let selection = self.git_selection(args).await?;
        let mode = required_string(args, "mode")?;
        let mode = match mode {
            "staged" => GitDiffMode::Staged,
            "unstaged" => GitDiffMode::Unstaged,
            _ => {
                return Err(ToolFailure::new(
                    "invalid_git_diff_mode",
                    "mode must be staged or unstaged.",
                ));
            }
        };
        let paths = string_array(args, "paths")?;
        let max_bytes = args
            .get("maxBytes")
            .and_then(Value::as_u64)
            .unwrap_or(100_000) as usize;
        let value = self
            .git
            .diff(&selection, mode, &paths, max_bytes)
            .await
            .map_err(git_failure)?;
        serde_json::to_value(value)
            .map_err(|error| ToolFailure::new("gateway_error", error.to_string()))
    }

    async fn git_compare(&self, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
        let selection = self.git_selection(args).await?;
        let base = required_string(args, "baseRevision")?;
        let head = required_string(args, "headRevision")?;
        let paths = string_array(args, "paths")?;
        let max_bytes = args
            .get("maxBytes")
            .and_then(Value::as_u64)
            .unwrap_or(100_000) as usize;
        let value = self
            .git
            .compare(&selection, base, head, &paths, max_bytes)
            .await
            .map_err(git_failure)?;
        serde_json::to_value(value)
            .map_err(|error| ToolFailure::new("gateway_error", error.to_string()))
    }

    async fn git_selection(
        &self,
        args: &Map<String, Value>,
    ) -> Result<GitWorkspaceSelection, ToolFailure> {
        let project_id = required_string(args, "projectId")?;
        let shell = self
            .client
            .get_shell(None)
            .await
            .map_err(ToolFailure::from)?;
        let project = shell.projects.iter().find(|project| project.id == project_id)
            .ok_or_else(|| ToolFailure::new("project_not_found", format!("Project {project_id} was not found. Call t3_project_list to discover available project IDs.")))?;
        let thread_id = optional_string(args, "threadId")?;
        let thread = thread_id.as_deref().and_then(|id| shell.threads.iter().find(|thread| thread.id == id))
            .ok_or_else(|| if thread_id.is_some() {
                ToolFailure::new("thread_not_found", format!("Thread {} was not found. Call t3_threads_list to discover available thread IDs.", thread_id.as_deref().unwrap_or_default()))
            } else {
                ToolFailure::new("thread_not_found", "Thread was not found.")
            });
        let thread = if thread_id.is_some() {
            Some(thread?)
        } else {
            None
        };
        if let Some(thread) = thread
            && thread.project_id != project.id
        {
            return Err(ToolFailure::new(
                "thread_project_mismatch",
                format!(
                    "Thread {} belongs to project {}, not selected project {}.",
                    thread.id, thread.project_id, project.id
                ),
            ));
        }
        let path = thread
            .and_then(|value| value.extra.get("worktreePath").and_then(Value::as_str))
            .unwrap_or(&project.workspace_root);
        if !Path::new(path).is_absolute() {
            return Err(ToolFailure::new(
                "workspace_path_not_absolute",
                format!(
                    "T3 returned a relative workspace path; refusing to resolve it against the gateway working directory: {path}"
                ),
            ));
        }
        let descriptor = self
            .client
            .get_descriptor(None)
            .await
            .map_err(ToolFailure::from)?;
        self.assert_environment(&descriptor)?;
        Ok(GitWorkspaceSelection {
            environment_id: descriptor.environment_id,
            project_id: project.id.clone(),
            project_title: project.title.clone(),
            project_workspace_root: project.workspace_root.clone(),
            thread_id: thread.map(|value| value.id.clone()),
            thread_branch: thread
                .and_then(|value| value.extra.get("branch").and_then(Value::as_str))
                .map(str::to_owned),
            thread_worktree_path: thread
                .and_then(|value| value.extra.get("worktreePath").and_then(Value::as_str))
                .map(str::to_owned),
            source: if thread.is_some_and(|value| {
                value
                    .extra
                    .get("worktreePath")
                    .is_some_and(Value::is_string)
            }) {
                GitWorkspaceSource::ThreadWorktree
            } else {
                GitWorkspaceSource::ProjectWorkspace
            },
            selected_path: path.to_owned(),
        })
    }

    async fn threads_list(&self, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
        let input: ThreadsListInput = parse_args(args)?;
        let read_started_at = self.read_started_at();
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
        self.observe_shell_recoveries(&shell.threads, shell.snapshot_sequence, read_started_at)?;
        let now = Utc::now().timestamp_millis();
        let projects = project_titles(&shell);
        let open_queue = input.status.is_none()
            && input.only_running.is_none()
            && input.session_status.is_none()
            && input.activity.is_none()
            && input.needs_attention.is_none()
            && !input.include_archived;
        let mut filtered = shell
            .threads
            .iter()
            .filter(|thread| {
                if input
                    .project_id
                    .as_deref()
                    .is_some_and(|id| id != thread.project_id)
                    || !matches_query(input.query.as_deref(), thread)
                {
                    return false;
                }
                if open_queue {
                    return is_in_open_queue(thread, now);
                }
                if input.only_running == Some(true) && !is_thread_running(thread) {
                    return false;
                }
                if input.session_status.as_deref().is_some_and(|status| {
                    thread
                        .session
                        .as_ref()
                        .map(|session| session.status.as_str())
                        != Some(status)
                }) {
                    return false;
                }
                if input
                    .activity
                    .is_some_and(|activity| thread_activity(thread) != activity)
                {
                    return false;
                }
                if input.needs_attention == Some(true) && !needs_attention_for(thread, now) {
                    return false;
                }
                if input.needs_attention == Some(false) && needs_attention_for(thread, now) {
                    return false;
                }
                if let Some(status) = input.status.as_deref()
                    && status != "all"
                {
                    return thread_status_name(thread_status(thread, now)) == status;
                }
                input.include_archived || thread.archived_at.as_deref().is_none_or(str::is_empty)
            })
            .collect::<Vec<_>>();
        sort_threads(&mut filtered, &input.sort, now);
        let failures = filtered
            .iter()
            .map(|thread| {
                self.observe_summary_failure(
                    thread,
                    None,
                    shell.snapshot_sequence,
                    read_started_at,
                    None,
                )
                .map(|observed| (thread.id.as_str(), observed))
            })
            .collect::<Result<HashMap<_, _>, _>>()?;
        let mut summaries = filtered
            .iter()
            .map(|thread| {
                let mut summary = thread_summary(
                    thread,
                    projects.get(&thread.project_id).map(String::as_str),
                    &descriptor.environment_id,
                    now,
                );
                summary["failure"] = failures[thread.id.as_str()]
                    .failure
                    .clone()
                    .unwrap_or(Value::Null);
                summary
            })
            .collect::<Vec<_>>();
        if input.detail == "full" {
            for (summary, thread) in summaries.iter_mut().zip(filtered.iter()) {
                *summary = self
                    .enrich_summary(summary.clone(), thread, &failures[thread.id.as_str()].order)
                    .await?;
            }
        }
        let page = paginate_values(&summaries, input.cursor.as_deref(), input.limit)?;
        Ok(json!({
            "environmentId": descriptor.environment_id,
            "observedAt": now_iso(),
            "scope": if open_queue {"open_queue"} else {"filtered"},
            "page": page,
            "resolutionHint": resolution_hint(filtered.len(), input.query.as_deref(), input.project_id.as_deref(), input.status.as_deref(), open_queue),
        }))
    }

    pub async fn threads_overview(&self, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
        let input: ThreadsOverviewInput = parse_args(args)?;
        let read_started_at = self.read_started_at();
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
        self.observe_shell_recoveries(&shell.threads, shell.snapshot_sequence, read_started_at)?;
        let observed_at = Utc::now();
        let now = observed_at.timestamp_millis();
        let projects = project_titles(&shell);
        let filtered = shell
            .threads
            .iter()
            .filter(|thread| {
                input
                    .project_id
                    .as_deref()
                    .is_none_or(|id| id == thread.project_id)
                    && matches_query(input.query.as_deref(), thread)
                    && (input.include_archived
                        || thread.archived_at.as_deref().is_none_or(str::is_empty))
            })
            .collect::<Vec<_>>();
        let failures = filtered
            .iter()
            .map(|thread| {
                self.observe_summary_failure(
                    thread,
                    None,
                    shell.snapshot_sequence,
                    read_started_at,
                    None,
                )
                .map(|observed| (thread.id.as_str(), observed))
            })
            .collect::<Result<HashMap<_, _>, _>>()?;
        let mut counts = json!({"open":0,"snoozed":0,"settled":0,"archived":0});
        let mut execution_counts = json!({"running":0,"starting":0,"awaiting_approval":0,"awaiting_input":0,"failed":0,"idle":0});
        let mut running = Vec::new();
        let mut queue = Vec::new();
        let mut needs_attention_count = 0;
        for thread in &filtered {
            let status = thread_status_name(thread_status(thread, now));
            counts[status] = json!(counts[status].as_u64().unwrap_or_default() + 1);
            let activity = activity_name(thread_activity(thread));
            execution_counts[activity] =
                json!(execution_counts[activity].as_u64().unwrap_or_default() + 1);
            if needs_attention_for(thread, now) {
                needs_attention_count += 1;
            }
            if is_thread_running(thread) && running.len() < input.running_limit {
                let mut summary = thread_summary(
                    thread,
                    projects.get(&thread.project_id).map(String::as_str),
                    &descriptor.environment_id,
                    now,
                );
                summary["failure"] = failures[thread.id.as_str()]
                    .failure
                    .clone()
                    .unwrap_or(Value::Null);
                running.push(summary);
            }
            if is_in_open_queue(thread, now) {
                queue.push(*thread);
            }
        }
        queue.sort_by(|left, right| {
            highlight_rank(left)
                .cmp(&highlight_rank(right))
                .then_with(|| thread_recency(right).cmp(&thread_recency(left)))
                .then_with(|| left.id.cmp(&right.id))
        });
        let mut highlights = Vec::new();
        for thread in queue.into_iter().take(5) {
            let mut summary = thread_summary(
                thread,
                projects.get(&thread.project_id).map(String::as_str),
                &descriptor.environment_id,
                now,
            );
            let observed = &failures[thread.id.as_str()];
            summary["failure"] = observed.failure.clone().unwrap_or(Value::Null);
            highlights.push(
                self.enrich_summary(summary, thread, &observed.order)
                    .await?,
            );
        }
        Ok(json!({
            "environmentId": descriptor.environment_id,
            "observedAt": observed_at.to_rfc3339_opts(SecondsFormat::Millis, true),
            "total": filtered.len(),
            "counts": counts,
            "executionCounts": execution_counts,
            "runningCount": filtered.iter().filter(|thread| is_thread_running(thread)).count(),
            "needsAttentionCount": needs_attention_count,
            "running": running,
            "highlights": highlights,
        }))
    }

    async fn pending_actions_list(&self, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
        let thread_id = required_string(args, "threadId")?;
        let snapshot = self
            .client
            .get_thread(thread_id, None)
            .await
            .map_err(ToolFailure::from)?;
        let descriptor = self
            .client
            .get_descriptor(None)
            .await
            .map_err(ToolFailure::from)?;
        self.assert_environment(&descriptor)?;
        let actions = extract_pending_actions(&snapshot.thread);
        Ok(json!({
            "environmentId": descriptor.environment_id,
            "threadId": thread_id,
            "hasPendingApprovals": snapshot.thread.shell.has_pending_approvals.unwrap_or(false),
            "hasPendingUserInput": snapshot.thread.shell.has_pending_user_input.unwrap_or(false),
            "detailsAvailable": !actions.is_empty(),
            "actions": actions,
        }))
    }

    async fn attachment_upload(&self, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
        authorize_native_mutation(self.config.read_only, &self.native.credential_state())?;
        let input: AttachmentUploadInput = parse_args(args)?;
        let bytes = decode_base64_strict(&input.data_base64)?;
        let kind = if IMAGE_MIME_TYPES.contains(&input.mime_type.as_str()) {
            "image"
        } else {
            "file"
        };
        let limit = if kind == "image" {
            MAX_IMAGE_ATTACHMENT_BYTES
        } else {
            MAX_FILE_ATTACHMENT_BYTES
        };
        if bytes.is_empty() || bytes.len() > limit {
            return Err(ToolFailure::new(
                "invalid_attachment",
                format!(
                    "A {kind} attachment must be 1 to {limit} bytes; received {}.",
                    bytes.len()
                ),
            ));
        }
        let upload = json!({
            "type": kind,
            "name": input.name,
            "mimeType": input.mime_type,
            "sizeBytes": bytes.len(),
        });
        let prepared = self
            .native
            .call_tool(
                json!({"name":"t3_attachment_prepare_upload","arguments":{"upload":upload}})
                    .as_object()
                    .unwrap()
                    .clone(),
                true,
                None,
            )
            .await
            .map_err(native_failure)?;
        let prepared = native_result_value(prepared, "t3_attachment_prepare_upload")?;
        let attachment_id = prepared["attachmentId"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| native_unexpected_result("t3_attachment_prepare_upload"))?;
        let relative_url = prepared["relativeUrl"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| native_unexpected_result("t3_attachment_prepare_upload"))?;
        if let Err(error) = self
            .native
            .upload_attachment(relative_url, bytes, &input.mime_type)
            .await
        {
            let _ = self
                .native
                .call_tool(
                    json!({"name":"t3_attachment_discard","arguments":{"attachmentId":attachment_id}})
                        .as_object()
                        .unwrap()
                        .clone(),
                    true,
                    None,
                )
                .await;
            return Err(native_failure(error));
        }
        Ok(json!({
            "attachment": {
                "type": kind,
                "id": attachment_id,
                "name": input.name,
                "mimeType": input.mime_type,
                "sizeBytes": upload["sizeBytes"],
            },
            "nextAction": "Pass attachment in the attachments array of t3_thread_send_attachments with the target threadId.",
        }))
    }

    async fn pending_action_respond(
        &self,
        args: &Map<String, Value>,
    ) -> Result<Value, ToolFailure> {
        if self.config.read_only {
            self.require_operation_scope().await?;
        }
        let input: PendingActionRespondInput = parse_args(args)?;
        let kind = match input.kind.as_str() {
            "approval" => WritableOperationKind::ThreadApprovalRespond,
            "user_input" => WritableOperationKind::ThreadUserInputRespond,
            _ => {
                return Err(ToolFailure::new(
                    "invalid_arguments",
                    "kind must be approval or user_input.",
                ));
            }
        };
        if input.kind == "approval" && input.decision.is_none() {
            return Err(ToolFailure::new(
                "decision_required",
                "Approval responses require a decision.",
            ));
        }
        if input.kind == "user_input" && input.answers.is_none() {
            return Err(ToolFailure::new(
                "answers_required",
                "User-input responses require answers.",
            ));
        }
        let payload = json!({
            "threadId": input.thread_id,
            "requestId": input.request_id,
            "kind": input.kind,
            "decision": input.decision,
            "answers": input.answers,
        });
        let payload_hash = hash_payload(&payload);
        let existing = self
            .journal
            .get_by_idempotency_key(&input.idempotency_key)
            .map_err(journal_failure)?;
        if let Some(existing) = existing {
            let begun = self
                .journal
                .begin(BeginOperationInput {
                    kind,
                    idempotency_key: input.idempotency_key,
                    payload_hash,
                    command_id: None,
                    project_id: existing
                        .get("projectId")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    thread_id: existing
                        .get("threadId")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or(Some(input.thread_id)),
                    run_id: None,
                    message_id: None,
                    turn_id: None,
                    settings: None,
                })
                .map_err(journal_failure)?;
            let record = self.reconcile(&begun.record).await?;
            return self.mutation_result(&record).await;
        }
        self.require_operation_scope().await?;
        let begun = self
            .journal
            .begin(BeginOperationInput {
                kind,
                idempotency_key: input.idempotency_key,
                payload_hash,
                command_id: None,
                project_id: None,
                thread_id: Some(input.thread_id.clone()),
                run_id: None,
                message_id: None,
                turn_id: None,
                settings: None,
            })
            .map_err(journal_failure)?;
        if begun.reused {
            let record = self.reconcile(&begun.record).await?;
            return self.mutation_result(&record).await;
        }
        let created_at = now_iso();
        let command = if input.kind == "approval" {
            T3Command::ApprovalResponse {
                command_id: begun.record["commandId"]
                    .as_str()
                    .unwrap_or_default()
                    .into(),
                thread_id: input.thread_id,
                request_id: input.request_id,
                decision: approval_decision(input.decision.as_deref().ok_or_else(|| {
                    ToolFailure::new(
                        "decision_required",
                        "Approval responses require a decision.",
                    )
                })?)?,
                created_at,
            }
        } else {
            T3Command::UserInputResponse {
                command_id: begun.record["commandId"]
                    .as_str()
                    .unwrap_or_default()
                    .into(),
                thread_id: input.thread_id,
                request_id: input.request_id,
                answers: input.answers.unwrap_or_default(),
                created_at,
            }
        };
        let record = self.dispatch_new(&begun.record, &command).await?;
        self.mutation_result(&record).await
    }

    async fn require_operation_scope(&self) -> Result<(), ToolFailure> {
        if self.config.read_only {
            return Err(ToolFailure::new(
                "gateway_read_only",
                "This gateway endpoint is configured as read-only.",
            ));
        }
        let session = self
            .client
            .get_session(None)
            .await
            .map_err(ToolFailure::from)?;
        if !session.authenticated
            || !session
                .scopes
                .unwrap_or_default()
                .iter()
                .any(|scope| scope == "orchestration:operate")
        {
            return Err(ToolFailure::new(
                "t3_scope_required",
                "The configured T3 credential does not grant orchestration:operate.",
            ));
        }
        Ok(())
    }

    async fn dispatch_new(
        &self,
        record: &crate::operations::journal::OperationRecord,
        command: &T3Command,
    ) -> Result<crate::operations::journal::OperationRecord, ToolFailure> {
        match self.client.dispatch(command, None).await {
            Ok(result) => self
                .journal
                .update(
                    record["operationId"].as_str().unwrap_or_default(),
                    OperationPatch {
                        status: Some(OperationStatus::Accepted),
                        t3_sequence: Some(result.sequence as f64),
                        ..OperationPatch::default()
                    },
                )
                .map_err(journal_failure),
            Err(error)
                if error
                    .status
                    .is_some_and(|status| (400..500).contains(&status)) =>
            {
                self.journal
                    .update(
                        record["operationId"].as_str().unwrap_or_default(),
                        OperationPatch {
                            status: Some(OperationStatus::Rejected),
                            last_error: Some(error.message),
                            ..OperationPatch::default()
                        },
                    )
                    .map_err(journal_failure)
            }
            Err(error) => {
                let operation_id = record["operationId"].as_str().unwrap_or_default();
                let message = format!(
                    "{} Reconcile with operation {operation_id} via t3_task_get; do not resubmit with a fresh idempotency key.",
                    error.message
                );
                self.journal
                    .update(
                        operation_id,
                        OperationPatch {
                            status: Some(OperationStatus::Uncertain),
                            last_error: Some(message),
                            ..OperationPatch::default()
                        },
                    )
                    .map_err(journal_failure)
            }
        }
    }

    async fn reconcile(
        &self,
        record: &crate::operations::journal::OperationRecord,
    ) -> Result<crate::operations::journal::OperationRecord, ToolFailure> {
        if record["status"] == "accepted" || record["status"] == "rejected" {
            return Ok(record.clone());
        }
        let Some(thread_id) = record["threadId"].as_str() else {
            return Ok(record.clone());
        };
        let Ok(snapshot) = self.client.get_thread(thread_id, None).await else {
            return Ok(record.clone());
        };
        if record["kind"] == "thread.create" {
            return self
                .journal
                .update(
                    record["operationId"].as_str().unwrap_or_default(),
                    OperationPatch {
                        status: Some(OperationStatus::Accepted),
                        ..OperationPatch::default()
                    },
                )
                .map_err(journal_failure);
        }
        if record["kind"] == "thread.turn.start"
            && let Some(message_id) = record["messageId"].as_str()
            && let Some(message) = snapshot
                .thread
                .messages
                .iter()
                .find(|message| message.id == message_id)
        {
            return self
                .journal
                .update(
                    record["operationId"].as_str().unwrap_or_default(),
                    OperationPatch {
                        status: Some(OperationStatus::Accepted),
                        turn_id: message.turn_id.clone(),
                        ..OperationPatch::default()
                    },
                )
                .map_err(journal_failure);
        }
        Ok(record.clone())
    }

    async fn mutation_result(
        &self,
        record: &crate::operations::journal::OperationRecord,
    ) -> Result<Value, ToolFailure> {
        let mut result = json!({
            "environmentId": self.known_environment_id().await,
            "operationId": record["operationId"],
            "commandId": record["commandId"],
            "status": record["status"],
        });
        if record["status"] == "accepted" {
            result["t3Sequence"] = record.get("t3Sequence").cloned().unwrap_or(Value::Null);
        } else {
            result["reason"] = record
                .get("lastError")
                .cloned()
                .unwrap_or_else(|| json!("T3 did not accept the command."));
        }
        Ok(result)
    }

    async fn known_environment_id(&self) -> String {
        match self.client.get_descriptor(None).await {
            Ok(descriptor) if self.assert_environment(&descriptor).is_ok() => {
                descriptor.environment_id
            }
            _ => self
                .client
                .cached_descriptor()
                .map(|value| value.environment_id)
                .or_else(|| self.config.environment_id.clone())
                .unwrap_or_else(|| "unknown".into()),
        }
    }

    fn observe_shell_recoveries(
        &self,
        threads: &[ThreadShell],
        sequence: u64,
        read_started_at: u64,
    ) -> Result<(), ToolFailure> {
        let recoveries = threads
            .iter()
            .filter_map(|thread| {
                let turn = thread.latest_turn.as_ref()?;
                let state = match turn.state {
                    TurnState::Completed => RecoveryState::Completed,
                    TurnState::Interrupted => RecoveryState::Interrupted,
                    _ => return None,
                };
                Some(
                    task::shell_failure_order(thread, sequence, read_started_at).map(|order| {
                        TerminalRecovery {
                            thread_id: thread.id.clone(),
                            turn_id: turn.turn_id.clone(),
                            state,
                            order: Some(order),
                        }
                    }),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.journal
            .clear_terminal_failures(&recoveries)
            .map_err(journal_failure)
    }

    fn observe_summary_failure(
        &self,
        selected: &ThreadShell,
        full: Option<&Thread>,
        sequence: u64,
        read_started_at: u64,
        shell_order: Option<&FailureEvidenceOrder>,
    ) -> Result<SummaryFailureObservation, ToolFailure> {
        let shell_failure = shell_failure_info(selected, None);
        let turn_id = selected
            .latest_turn
            .as_ref()
            .map(|turn| turn.turn_id.as_str())
            .or_else(|| {
                shell_failure
                    .as_ref()
                    .map(|failure| failure.turn_id.as_str())
            });
        let source = full.map_or(selected, |thread| &thread.shell);
        let failure = match full {
            Some(thread) => turn_id.and_then(|turn| failure_info(thread, Some(turn))),
            None => shell_failure.clone(),
        };
        let recovered = full.and_then(|thread| {
            thread
                .turn_recoveries
                .as_ref()?
                .iter()
                .find(|entry| Some(entry.turn_id.as_str()) == turn_id)
        });
        let recovery = recovered
            .map(|entry| entry.state)
            .or_else(|| {
                source
                    .latest_turn
                    .as_ref()
                    .filter(|turn| Some(turn.turn_id.as_str()) == turn_id)
                    .map(|turn| turn.state)
            })
            .and_then(|state| match state {
                TurnState::Completed => Some(RecoveryState::Completed),
                TurnState::Interrupted => Some(RecoveryState::Interrupted),
                _ => None,
            });
        let order = match full {
            Some(thread) => task::failure_order(thread, sequence, turn_id, read_started_at)?,
            None => task::shell_failure_order(selected, sequence, read_started_at)?,
        };
        let order = self
            .journal
            .order_failure_evidence(&selected.id, turn_id, order)
            .map_err(journal_failure)?;
        if let Some(turn_id) = turn_id {
            let stale_full = shell_order.is_some_and(|shell_order| {
                selected
                    .latest_turn
                    .as_ref()
                    .is_some_and(|turn| turn.state != TurnState::Error)
                    && order.compare(shell_order).is_lt()
            });
            if !stale_full {
                match (recovery, failure) {
                    (Some(recovery), _) if full.is_some() => self
                        .journal
                        .clear_terminal_failure(
                            &selected.id,
                            turn_id,
                            recovery,
                            Some(order.clone()),
                        )
                        .map_err(journal_failure)?,
                    (None, Some(failure)) => {
                        self.journal
                            .retain_terminal_failure(
                                &selected.id,
                                turn_id,
                                serde_json::to_value(failure).expect("serializable failure"),
                                None,
                                Some(order.clone()),
                            )
                            .map_err(journal_failure)?;
                    }
                    _ => {}
                }
            }
        }
        let failure = turn_id
            .map(|turn_id| self.journal.get_failure_by_turn_id(&selected.id, turn_id))
            .transpose()
            .map_err(journal_failure)?
            .flatten();
        Ok(SummaryFailureObservation { failure, order })
    }

    async fn enrich_summary(
        &self,
        mut summary: Value,
        shell: &ThreadShell,
        shell_order: &FailureEvidenceOrder,
    ) -> Result<Value, ToolFailure> {
        let read_started_at = self.read_started_at();
        let Ok(snapshot) = self.client.get_thread(&shell.id, None).await else {
            summary["latestResponseExcerpt"] = Value::Null;
            return Ok(summary);
        };
        let full_turn = snapshot
            .thread
            .shell
            .latest_turn
            .as_ref()
            .map(|turn| json!(turn.turn_id))
            .unwrap_or(Value::Null);
        let full_state = snapshot
            .thread
            .shell
            .latest_turn
            .as_ref()
            .map(|turn| serde_json::to_value(turn.state).unwrap_or(Value::Null))
            .unwrap_or(Value::Null);
        let same_turn = summary["observedTurnId"] == full_turn;
        let same_state = summary["latestTurn"]["state"] == full_state;
        if !same_turn {
            self.observe_summary_failure(
                &snapshot.thread.shell,
                Some(&snapshot.thread),
                snapshot.snapshot_sequence,
                read_started_at,
                None,
            )?;
        }
        let observed = self.observe_summary_failure(
            shell,
            Some(&snapshot.thread),
            snapshot.snapshot_sequence,
            read_started_at,
            Some(shell_order),
        )?;
        summary["failure"] = observed.failure.unwrap_or(Value::Null);
        let turn_id = summary["observedTurnId"].as_str();
        let latest = if same_turn
            && same_state
            && (summary["failure"].is_null() || summary["latestTurn"]["state"] == "error")
        {
            latest_assistant(
                &snapshot.thread.messages,
                turn_id,
                turn_id.is_none() && summary["failure"].is_null(),
            )
        } else {
            None
        };
        summary["latestResponseExcerpt"] = latest
            .map(|message| {
                let text = if message.text.starts_with("API Error:") {
                    crate::t3::failure::sanitize_failure_text(&message.text, 2000)
                } else {
                    message.text.clone()
                };
                Value::String(truncate_excerpt(&text, 200))
            })
            .unwrap_or(Value::Null);
        Ok(summary)
    }

    fn assert_environment(&self, descriptor: &Descriptor) -> Result<(), ToolFailure> {
        if self
            .config
            .environment_id
            .as_ref()
            .is_some_and(|id| id != &descriptor.environment_id)
        {
            return Err(ToolFailure::new(
                "environment_mismatch",
                format!(
                    "Configured environment {} does not match the connected T3 environment.",
                    self.config.environment_id.as_deref().unwrap_or_default()
                ),
            ));
        }
        Ok(())
    }
}

impl Drop for T3Gateway {
    fn drop(&mut self) {
        self.native.close();
    }
}

impl ToolHandler for T3Gateway {
    fn native_client(&self) -> Option<&NativeMcpClient> {
        Some(&self.native)
    }

    async fn tool_catalog(&self) -> Option<ToolsListPage> {
        let snapshot = self.native.snapshot().await;
        let mut tools = ToolsListPage::gateway().tools().to_vec();
        for tool in snapshot.tools {
            tools.push(serde_json::to_value(tool).ok()?);
        }
        ToolsListPage::parse(json!({"tools": tools})).ok()
    }

    async fn call_tool(
        &self,
        call: ToolCall,
        invocation: Option<Invocation>,
    ) -> Result<ToolResult, ToolFailure> {
        let name = call.name.clone();
        let correlation_id = format!("mcp_{}", uuid::Uuid::new_v4());
        with_audit_context(correlation_id.clone(), name.clone(), async move {
            let mut started = AuditEventInput::new(AuditSource::Mcp, "tool.call");
            started.correlation_id = Some(correlation_id.clone());
            started.operation = Some(name.clone());
            started.outcome = Some("started".into());
            started.details = Some(AuditValue::object([(
                "arguments",
                summarize_for_audit(Value::Object(call.arguments.clone())),
            )]));
            let native = is_native_tool_name(&name);
            if native {
                started
                    .details
                    .as_mut()
                    .expect("call details")
                    .as_object_mut()
                    .expect("details object")
                    .insert("native".into(), AuditValue::Bool(true));
            }
            self.audit.record(started);
            let then = std::time::Instant::now();
            match Box::pin(self.route(&call, invocation.as_ref())).await {
                Ok(result) => {
                    let audit_result: AuditValue = serde_json::from_str(result.as_raw().get())
                        .expect("serialized tool result");
                    let mut event = AuditEventInput::new(AuditSource::Mcp, "tool.result");
                    event.correlation_id = Some(correlation_id);
                    event.operation = Some(name);
                    event.outcome = Some(
                        if audit_result.get("isError") == Some(&AuditValue::Bool(true)) {
                            "error"
                        } else {
                            "completed"
                        }
                        .into(),
                    );
                    event.duration_ms = Some(then.elapsed().as_secs_f64() * 1000.0);
                    let summary = if native {
                        native_audit_summary(&call.name, &audit_result)
                    } else {
                        audit_result
                            .get("structuredContent")
                            .cloned()
                            .unwrap_or(AuditValue::Null)
                    };
                    event.details = Some(AuditValue::object([(
                        "result",
                        summarize_for_audit(summary),
                    )]));
                    self.audit.record(event);
                    Ok(result)
                }
                Err(error) => {
                    let mut event = AuditEventInput::new(AuditSource::Mcp, "tool.result");
                    event.correlation_id = Some(correlation_id);
                    event.operation = Some(name);
                    event.outcome = Some("error".into());
                    event.duration_ms = Some(then.elapsed().as_secs_f64() * 1000.0);
                    event.details =
                        Some(json!({"errorCode":error.code,"errorMessage":error.message}).into());
                    self.audit.record(event);
                    Err(error)
                }
            }
        })
        .await
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadsListInput {
    project_id: Option<String>,
    #[serde(default)]
    include_archived: bool,
    query: Option<String>,
    status: Option<String>,
    only_running: Option<bool>,
    session_status: Option<String>,
    activity: Option<ThreadActivity>,
    needs_attention: Option<bool>,
    #[serde(default = "default_sort")]
    sort: String,
    #[serde(default = "default_detail")]
    detail: String,
    cursor: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadsOverviewInput {
    project_id: Option<String>,
    #[serde(default)]
    include_archived: bool,
    query: Option<String>,
    #[serde(default)]
    running_limit: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingActionRespondInput {
    thread_id: String,
    request_id: String,
    kind: String,
    decision: Option<String>,
    answers: Option<Map<String, Value>>,
    idempotency_key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttachmentUploadInput {
    name: String,
    mime_type: String,
    data_base64: String,
}

fn default_sort() -> String {
    "recent".into()
}

fn default_detail() -> String {
    "summary".into()
}

fn default_limit() -> usize {
    50
}

fn parse_args<T: serde::de::DeserializeOwned>(args: &Map<String, Value>) -> Result<T, ToolFailure> {
    serde_json::from_value(Value::Object(args.clone()))
        .map_err(|error| ToolFailure::new("invalid_arguments", error.to_string()))
}

fn decode_base64_strict(value: &str) -> Result<Vec<u8>, ToolFailure> {
    let compact = value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    let unpadded = compact.trim_end_matches('=');
    let bytes = STANDARD_NO_PAD
        .decode(unpadded)
        .map_err(|_| invalid_base64())?;
    if STANDARD_NO_PAD.encode(&bytes) != unpadded {
        return Err(invalid_base64());
    }
    Ok(bytes)
}

fn invalid_base64() -> ToolFailure {
    ToolFailure::new(
        "invalid_attachment",
        "dataBase64 is not valid standard base64.",
    )
}

fn native_result_value(
    result: crate::t3::native_mcp::NativeCallResult,
    tool: &str,
) -> Result<Value, ToolFailure> {
    let text = result
        .content
        .iter()
        .filter(|item| item["type"] == "text")
        .filter_map(|item| item["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if result.is_error == Some(true) {
        return Err(ToolFailure::new(
            "native_rejected",
            format!(
                "{tool} failed: {}",
                String::from_utf16_lossy(&text.encode_utf16().take(500).collect::<Vec<_>>())
            ),
        ));
    }
    Ok(result
        .extra
        .get("structuredContent")
        .filter(|value| !value.is_null())
        .cloned()
        .or_else(|| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null))
}

fn native_failure(error: crate::t3::native_mcp::NativeError) -> ToolFailure {
    ToolFailure {
        code: error.code,
        message: error.message,
        protocol_error: error.protocol_error,
    }
}

fn native_unexpected_result(tool: &str) -> ToolFailure {
    ToolFailure::new(
        "native_unexpected_result",
        format!("{tool} returned a result this gateway does not recognize."),
    )
}

fn project_titles(
    shell: &crate::t3::types::ShellSnapshot,
) -> std::collections::HashMap<String, String> {
    shell
        .projects
        .iter()
        .map(|project| (project.id.clone(), project.title.clone()))
        .collect()
}

fn matches_query(query: Option<&str>, thread: &ThreadShell) -> bool {
    let Some(query) = query.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };
    let query = query.to_lowercase();
    [
        Some(thread.title.as_str()),
        Some(thread.id.as_str()),
        thread.extra.get("branch").and_then(Value::as_str),
    ]
    .into_iter()
    .flatten()
    .any(|value| value.to_lowercase().contains(&query))
}

fn sort_threads(threads: &mut [&ThreadShell], sort: &str, now: i64) {
    match sort {
        "title" => threads.sort_by(|a, b| a.title.cmp(&b.title).then_with(|| a.id.cmp(&b.id))),
        "status" => threads.sort_by(|a, b| {
            status_rank(thread_status(a, now))
                .cmp(&status_rank(thread_status(b, now)))
                .then_with(|| thread_recency(b).cmp(&thread_recency(a)))
                .then_with(|| a.id.cmp(&b.id))
        }),
        _ => threads.sort_by(|a, b| {
            thread_recency(b)
                .cmp(&thread_recency(a))
                .then_with(|| a.id.cmp(&b.id))
        }),
    }
}

fn status_rank(status: ThreadStatus) -> u8 {
    match status {
        ThreadStatus::Open => 0,
        ThreadStatus::Snoozed => 1,
        ThreadStatus::Settled => 2,
        ThreadStatus::Archived => 3,
    }
}

fn thread_status_name(status: ThreadStatus) -> &'static str {
    match status {
        ThreadStatus::Open => "open",
        ThreadStatus::Snoozed => "snoozed",
        ThreadStatus::Settled => "settled",
        ThreadStatus::Archived => "archived",
    }
}

fn activity_name(activity: ThreadActivity) -> &'static str {
    match activity {
        ThreadActivity::Running => "running",
        ThreadActivity::Starting => "starting",
        ThreadActivity::AwaitingApproval => "awaiting_approval",
        ThreadActivity::AwaitingInput => "awaiting_input",
        ThreadActivity::Failed => "failed",
        ThreadActivity::Idle => "idle",
    }
}

fn thread_recency(thread: &ThreadShell) -> i64 {
    [
        thread.latest_user_message_at.as_deref(),
        thread
            .session
            .as_ref()
            .and_then(|session| session.updated_at.as_deref()),
        thread.extra.get("updatedAt").and_then(Value::as_str),
        thread.extra.get("createdAt").and_then(Value::as_str),
    ]
    .into_iter()
    .flatten()
    .filter_map(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
    .map(|value| value.timestamp_millis())
    .max()
    .unwrap_or_default()
}

fn highlight_rank(thread: &ThreadShell) -> u8 {
    if thread.has_pending_approvals == Some(true) || thread.has_pending_user_input == Some(true) {
        0
    } else if thread
        .session
        .as_ref()
        .is_some_and(|session| session.status == "error")
        || thread_activity(thread) == ThreadActivity::Failed
    {
        1
    } else {
        2
    }
}

fn thread_summary(
    thread: &ThreadShell,
    project_title: Option<&str>,
    environment_id: &str,
    now: i64,
) -> Value {
    let observation = observe_thread(thread, now);
    let failure = (observation.execution == ThreadActivity::Failed)
        .then(|| shell_failure_info(thread, None))
        .flatten()
        .and_then(|value| serde_json::to_value(value).ok());
    json!({
        "id": thread.id,
        "projectId": thread.project_id,
        "projectTitle": project_title,
        "title": thread.title,
        "modelSelection": thread.model_selection,
        "runtimeMode": thread.runtime_mode,
        "interactionMode": thread.interaction_mode.unwrap_or(crate::t3::types::InteractionMode::Default),
        "branch": thread.extra.get("branch").cloned().unwrap_or(Value::Null),
        "worktreePath": thread.extra.get("worktreePath").cloned().unwrap_or(Value::Null),
        "latestTurn": thread.latest_turn,
        "sessionStatus": thread.session.as_ref().map(|session| session.status.clone()),
        "sessionUpdatedAt": thread.session.as_ref().and_then(|session| session.updated_at.clone()),
        "status": observation.lifecycle,
        "statusReason": thread_status_reason(thread, now),
        "activity": observation.execution,
        "isRunning": is_thread_running(thread),
        "hasConflictingSignals": has_conflicting_signals(thread),
        "quality": observation.quality,
        "warning": observation.warning,
        "observedTurnId": observation.observed_turn_id,
        "observedAt": observation.observed_at,
        "observedTarget": {"environmentId": environment_id, "threadId": thread.id, "turnId": observation.observed_turn_id, "observedAt": observation.observed_at},
        "settledOverride": thread.settled_override,
        "settledAt": thread.settled_at,
        "snoozedUntil": thread.snoozed_until,
        "snoozedAt": thread.snoozed_at,
        "latestUserMessageAt": thread.latest_user_message_at,
        "pinnedAt": thread.pinned_at,
        "hasActionableProposedPlan": thread.has_actionable_proposed_plan.unwrap_or(false),
        "backgroundLiveness": thread.background_liveness,
        "archivedAt": thread.archived_at,
        "createdAt": thread.extra.get("createdAt"),
        "updatedAt": thread.extra.get("updatedAt"),
        "hasPendingApprovals": thread.has_pending_approvals.unwrap_or(false),
        "hasPendingUserInput": thread.has_pending_user_input.unwrap_or(false),
        "failure": failure,
    })
}

fn latest_assistant<'a>(
    messages: &'a [Message],
    turn_id: Option<&str>,
    allow_unbound_fallback: bool,
) -> Option<&'a Message> {
    if turn_id.is_none() && !allow_unbound_fallback {
        return None;
    }
    messages.iter().rev().find(|message| {
        message.role == MessageRole::Assistant
            && turn_id.is_none_or(|turn| message.turn_id.as_deref() == Some(turn))
    })
}

fn extract_pending_actions(thread: &Thread) -> Vec<Value> {
    thread
        .activities
        .iter()
        .filter_map(|activity| {
            let record = activity.as_object()?;
            let payload = record.get("payload")?.as_object()?;
            let kind_text = record
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let kind = if record.get("tone").and_then(Value::as_str) == Some("approval")
                || kind_text.contains("approval")
            {
                "approval"
            } else if kind_text.contains("input") || kind_text.contains("question") {
                "user_input"
            } else {
                "unknown"
            };
            let request_id = payload.get("requestId").and_then(Value::as_str);
            if request_id.is_none() && kind == "unknown" {
                return None;
            }
            Some(json!({
                "kind": kind,
                "requestId": request_id,
                "summary": record.get("summary").and_then(Value::as_str).unwrap_or(kind_text),
                "payload": payload,
            }))
        })
        .collect()
}

fn truncate_excerpt(text: &str, max_units: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut units = 0;
    let mut end = normalized.len();
    for (index, character) in normalized.char_indices() {
        let width = character.len_utf16();
        if units + width > max_units {
            end = index;
            break;
        }
        units += width;
    }
    if end == normalized.len() {
        normalized
    } else {
        format!("{}…", &normalized[..end])
    }
}

fn paginate_values(
    values: &[Value],
    cursor: Option<&str>,
    limit: usize,
) -> Result<Value, ToolFailure> {
    let offset = match cursor {
        None | Some("") => 0,
        Some(value) => value.parse::<usize>().map_err(|_| {
            ToolFailure::new(
                "invalid_cursor",
                "cursor must be a non-negative integer string.",
            )
        })?,
    };
    let items = values
        .iter()
        .skip(offset)
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();
    let next = offset.saturating_add(items.len());
    Ok(json!({
        "items": items,
        "nextCursor": if next < values.len() { Some(next.to_string()) } else { None },
        "hasMore": next < values.len(),
        "total": values.len(),
    }))
}

fn resolution_hint(
    total: usize,
    query: Option<&str>,
    project_id: Option<&str>,
    status: Option<&str>,
    open_queue: bool,
) -> String {
    if total == 1 {
        return "One exact candidate. The client may select it and carry its observedTarget into control calls.".into();
    }
    if total > 1 {
        return "Multiple candidates. Present project, title, branch, and recent activity for clarification; never invent a newest choice for a mutation.".into();
    }
    let mut scope = Vec::new();
    if let Some(project_id) = project_id.filter(|value| !value.is_empty()) {
        scope.push(format!("project {project_id}"));
    }
    if let Some(query) = query.filter(|value| !value.is_empty()) {
        scope.push(format!(
            "query {}",
            serde_json::to_string(query).unwrap_or_default()
        ));
    }
    if let Some(status) = status.filter(|value| *value != "all") {
        scope.push(format!("status {status}"));
    }
    let where_clause = if scope.is_empty() {
        String::new()
    } else {
        format!(" for {}", scope.join(", "))
    };
    if open_queue {
        format!(
            "No open threads that are not working{where_clause}. The default open queue leaves out working, snoozed, settled, archived, and subagent threads; retry with status=all, onlyRunning=true, needsAttention=true, or includeArchived=true to review them. Zero results must not select a thread for control."
        )
    } else {
        format!(
            "No threads{where_clause}. Retry with a broader query, another project, or includeArchived=true. Zero results must not select a thread for control."
        )
    }
}

fn native_tool_available(native: &Value, name: &str) -> bool {
    native["availableTools"].as_u64().unwrap_or_default() > 0
        && !native["missingTools"]
            .as_array()
            .is_some_and(|missing| missing.iter().any(|item| item == name))
}

fn native_audit_summary(name: &str, result: &AuditValue) -> AuditValue {
    let structured = result
        .get("structuredContent")
        .and_then(AuditValue::as_object);
    let mut summary = AuditValue::object([
        (
            "isError",
            AuditValue::Bool(result.get("isError") == Some(&AuditValue::Bool(true))),
        ),
        (
            "keys",
            AuditValue::Array(
                structured
                    .map(|value| {
                        value
                            .keys()
                            .map(|key| AuditValue::from_utf16(key.as_units().to_vec()))
                            .collect()
                    })
                    .unwrap_or_default(),
            ),
        ),
    ]);
    if name == "t3_thread_wait"
        && let Some(value) = structured
    {
        if let Some(timed_out) = value
            .get(&crate::operations::audit_log::JsString::from("timedOut"))
            .filter(|value| matches!(value, AuditValue::Bool(_)))
        {
            summary
                .as_object_mut()
                .unwrap()
                .insert("timedOut".into(), timed_out.clone());
        }
        if let Some(status) = value
            .get(&crate::operations::audit_log::JsString::from("status"))
            .filter(|value| {
                matches!(
                    value.as_str(),
                    Some(
                        "idle"
                            | "preparing"
                            | "queued"
                            | "starting"
                            | "running"
                            | "waiting"
                            | "completed"
                            | "interrupted"
                            | "failed"
                            | "cancelled"
                            | "rolled_back"
                    )
                )
            })
        {
            summary
                .as_object_mut()
                .unwrap()
                .insert("status".into(), status.clone());
        }
    }
    summary
}

fn native_block_reason(native: &Value) -> String {
    native["error"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| "Native T3 does not offer this tool.".into())
}

fn freshness(last: Option<&str>, stale_after_ms: u64) -> &'static str {
    let Some(last) = last else {
        return "unknown";
    };
    let Some(at) = crate::operations::parse_timestamp(last) else {
        return "stale";
    };
    if Utc::now().timestamp_millis().saturating_sub(at) <= stale_after_ms as i64 {
        "fresh"
    } else {
        "stale"
    }
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn permitted_operations(scopes: &[String]) -> Vec<&'static str> {
    [
        "orchestration:read",
        "orchestration:operate",
        "terminal:operate",
        "review:write",
    ]
    .into_iter()
    .filter(|scope| scopes.iter().any(|value| value == scope))
    .collect()
}

fn required_string<'a>(args: &'a Map<String, Value>, name: &str) -> Result<&'a str, ToolFailure> {
    args.get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ToolFailure::new(
                "invalid_arguments",
                format!("{name} must be a non-empty string."),
            )
        })
}

fn optional_string(args: &Map<String, Value>, name: &str) -> Result<Option<String>, ToolFailure> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(ToolFailure::new(
            "invalid_arguments",
            format!("{name} must be a string."),
        )),
    }
}

fn string_array(args: &Map<String, Value>, name: &str) -> Result<Vec<String>, ToolFailure> {
    match args.get(name) {
        None => Ok(Vec::new()),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value.as_str().map(str::to_owned).ok_or_else(|| {
                    ToolFailure::new("invalid_arguments", format!("{name} must contain strings."))
                })
            })
            .collect(),
        Some(_) => Err(ToolFailure::new(
            "invalid_arguments",
            format!("{name} must be an array."),
        )),
    }
}

impl From<Box<crate::t3::http_client::ClientError>> for ToolFailure {
    fn from(error: Box<crate::t3::http_client::ClientError>) -> Self {
        Self::new(
            error.code.unwrap_or_else(|| "t3_unavailable".into()),
            error.message,
        )
    }
}

fn authorize_native_mutation(
    read_only: bool,
    state: &NativeCredentialState,
) -> Result<(), ToolFailure> {
    if read_only {
        return Err(ToolFailure::new(
            "gateway_read_only",
            "This gateway endpoint is configured as read-only.",
        ));
    }
    if let NativeCredentialState::Valid { credential } = state
        && !credential
            .scopes
            .iter()
            .any(|scope| scope == "orchestration:operate")
    {
        return Err(ToolFailure::new(
            "t3_scope_required",
            "The native MCP credential was approved read-only. Run `t3-code-mcp native-login --access full-access` with a new pairing code.",
        ));
    }
    Ok(())
}

fn git_failure(error: GitInspectionError) -> ToolFailure {
    ToolFailure::new(error.code.clone(), error.to_string())
}

fn journal_failure(error: crate::operations::journal::JournalError) -> ToolFailure {
    let code = match &error {
        crate::operations::journal::JournalError::IdempotencyConflict(_) => "idempotency_conflict",
        crate::operations::journal::JournalError::NotFound(_) => "journal_not_found",
        crate::operations::journal::JournalError::Invalid(_) => "journal_invalid",
        crate::operations::journal::JournalError::Io(_) => "journal_io_error",
    };
    ToolFailure::new(code, error.to_string())
}

fn approval_decision(value: &str) -> Result<ApprovalDecision, ToolFailure> {
    match value {
        "accept" => Ok(ApprovalDecision::Accept),
        "acceptForSession" => Ok(ApprovalDecision::AcceptForSession),
        "decline" => Ok(ApprovalDecision::Decline),
        "cancel" => Ok(ApprovalDecision::Cancel),
        _ => Err(ToolFailure::new(
            "invalid_arguments",
            "decision must be accept, acceptForSession, decline, or cancel.",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::authorize_native_mutation;
    use crate::native_credential::{NativeAccessLevel, NativeCredential, NativeCredentialState};

    fn credential(scopes: &[&str]) -> NativeCredentialState {
        NativeCredentialState::Valid {
            credential: NativeCredential {
                resource: "https://t3.example/mcp".into(),
                client_id: "client".into(),
                access_token: "token".into(),
                access: NativeAccessLevel::ReadOnly,
                scopes: scopes.iter().map(|scope| (*scope).into()).collect(),
                obtained_at: "2026-10-18T00:00:00Z".into(),
                expires_at: "2099-10-18T00:00:00Z".into(),
            },
        }
    }

    #[test]
    fn native_mutations_respect_gateway_read_only_mode() {
        assert!(authorize_native_mutation(false, &credential(&["orchestration:operate"])).is_ok());

        let failure = authorize_native_mutation(true, &credential(&["orchestration:operate"]))
            .expect_err("read-only mode blocks native writes");
        assert_eq!(failure.code, "gateway_read_only");
        assert_eq!(
            failure.message,
            "This gateway endpoint is configured as read-only."
        );
    }

    #[test]
    fn native_mutations_require_the_operate_scope_on_valid_credentials() {
        let failure = authorize_native_mutation(false, &credential(&["orchestration:read"]))
            .expect_err("read-only native credentials block writes");
        assert_eq!(failure.code, "t3_scope_required");
        assert!(
            failure
                .message
                .contains("native-login --access full-access")
        );

        assert!(authorize_native_mutation(false, &credential(&["orchestration:operate"])).is_ok());
    }
}
