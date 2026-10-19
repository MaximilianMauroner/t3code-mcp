use super::{
    failure::{category_for_failure, sanitize_failure_identifier},
    iso, timestamp,
    types::*,
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RunStatus {
    Preparing,
    Queued,
    Starting,
    Running,
    Waiting,
    Completed,
    Interrupted,
    Failed,
    Cancelled,
    RolledBack,
    Idle,
}
impl RunStatus {
    fn active(self) -> bool {
        matches!(
            self,
            Self::Preparing | Self::Starting | Self::Running | Self::Waiting
        )
    }
    fn turn(self) -> TurnState {
        if self.active() || self == Self::Queued {
            TurnState::Running
        } else {
            match self {
                Self::Failed => TurnState::Error,
                Self::Completed => TurnState::Completed,
                _ => TurnState::Interrupted,
            }
        }
    }
    fn session(self) -> &'static str {
        match self {
            Self::Preparing | Self::Starting => "starting",
            Self::Running | Self::Waiting | Self::Queued => "running",
            Self::Failed => "error",
            _ => "ready",
        }
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShellThread {
    title: String,
    provider_instance_id: String,
    created_at: String,
    updated_at: String,
    latest_run_id: Option<String>,
    latest_run_requested_at: Option<String>,
    latest_run_started_at: Option<String>,
    latest_run_completed_at: Option<String>,
    active_run_id: Option<String>,
    activity_run_started_at: Option<String>,
    activity_run_status: Option<RunStatus>,
    status: RunStatus,
    last_error: Option<String>,
    last_error_class: Option<String>,
    usage_limit_reset_at: Option<String>,
    pending_runtime_request: Option<Pending>,
    #[serde(default)]
    pending_background_tasks: Vec<Value>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pending {
    #[serde(rename = "id")]
    _id: String,
    kind: String,
    #[serde(rename = "createdAt")]
    _created_at: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Run {
    id: String,
    ordinal: std::num::NonZeroU64,
    status: RunStatus,
    provider_instance_id: String,
    model_selection: ModelSelection,
    #[serde(rename = "userMessageId")]
    _user_message_id: String,
    root_node_id: Option<String>,
    requested_at: String,
    started_at: Option<String>,
    completed_at: Option<String>,
}
fn approval(kind: &str) -> bool {
    kind != "user_input" && kind != "auth_refresh"
}
fn order(sequence: u64, scope: EvidenceScope, updated: &str) -> FailureEvidenceOrder {
    FailureEvidenceOrder {
        protocol_version: 2,
        scope,
        snapshot_sequence: sequence,
        read_started_at: None,
        protocol_started_at: None,
        failure_identity: None,
        updated_at: timestamp(Some(updated)).map(iso),
        run_identity: None,
        item: None,
    }
}
pub fn normalize_shell_thread(
    value: Value,
    sequence: Option<u64>,
) -> Result<ThreadShell, serde_json::Error> {
    let t: ShellThread = serde_json::from_value(value.clone())?;
    let status = t.activity_run_status.unwrap_or(t.status);
    let run = t.active_run_id.as_ref().or(t.latest_run_id.as_ref());
    let active_latest = t.active_run_id.is_none() || t.active_run_id == t.latest_run_id;
    let bound = status == RunStatus::Failed && t.last_error_class.is_some();
    let mut normalized = value;
    normalized["title"] = if t.title.is_empty() {
        "Untitled".into()
    } else {
        t.title.into()
    };
    normalized["orchestrationProtocolVersion"] = 2.into();
    if let Some(sequence) = sequence {
        normalized["evidenceOrder"] =
            serde_json::to_value(order(sequence, EvidenceScope::Shell, &t.updated_at))?;
    }
    normalized["latestTurn"] = if let Some(run) = run {
        json!({"turnId":run,"state":status.turn(),
        "requestedAt":if active_latest {t.latest_run_requested_at.as_ref()}else{t.activity_run_started_at.as_ref()}.unwrap_or(&t.created_at),
        "startedAt":if active_latest {t.latest_run_started_at.as_ref()}else{t.activity_run_started_at.as_ref()},
        "completedAt":if t.active_run_id.is_some(){None}else{t.latest_run_completed_at.as_ref()}})
    } else {
        Value::Null
    };
    normalized["session"] = json!({"status":status.session(),"providerInstanceId":t.provider_instance_id,"activeTurnId":run,
        "lastError":if bound{t.last_error.as_ref()}else{None},"lastErrorClass":if bound{t.last_error_class.as_ref()}else{None},
        "failureCategory":if bound{category_for_failure(t.last_error_class.as_deref().filter(|v|*v!="usage_limit"),None,t.last_error.as_deref())}else{super::failure::FailureCategory::Unknown},
        "resetAt":if bound{t.usage_limit_reset_at.as_ref()}else{None},"updatedAt":t.updated_at});
    normalized["hasPendingApprovals"] = t
        .pending_runtime_request
        .as_ref()
        .is_some_and(|r| approval(&r.kind))
        .into();
    normalized["hasPendingUserInput"] = t
        .pending_runtime_request
        .as_ref()
        .is_some_and(|r| r.kind == "user_input")
        .into();
    let holding: Vec<_> = t
        .pending_background_tasks
        .iter()
        .filter_map(|v| v["kind"].as_str())
        .filter(|v| matches!(*v, "subagent" | "monitor" | "background_task"))
        .collect();
    normalized["backgroundLiveness"] = if holding.is_empty() {
        Value::Null
    } else {
        if holding.iter().all(|v| *v == "monitor") {
            "monitoring"
        } else {
            "working"
        }
        .into()
    };
    serde_json::from_value(normalized)
}
pub fn normalize_thread(snapshot: Value) -> Result<ThreadSnapshot, serde_json::Error> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Snapshot {
        snapshot_sequence: u64,
        projection: Projection,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Projection {
        thread: serde_json::Map<String, Value>,
        runs: Vec<Run>,
        messages: Vec<Value>,
        provider_sessions: Vec<Value>,
        runtime_requests: Vec<Value>,
        turn_items: Vec<Value>,
        plans: Vec<Value>,
        checkpoints: Vec<Value>,
        updated_at: String,
    }
    let s: Snapshot = serde_json::from_value(snapshot)?;
    if s.projection.messages.iter().any(|message| {
        message
            .get("threadId")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    }) {
        return Err(<serde_json::Error as serde::de::Error>::custom(
            "V2 messages require a threadId.",
        ));
    }
    let p = s.projection;
    let thread = Value::Object(p.thread);
    let mut runs = p.runs;
    runs.sort_by_key(|r| r.ordinal);
    let active = runs.iter().find(|r| r.status.active());
    let messages: Vec<_> = p
        .messages
        .iter()
        .filter(|m| m["threadId"] == thread["id"])
        .collect();
    let pending: Vec<_> = p
        .runtime_requests
        .iter()
        .filter(|r| r["status"] == "pending" && r["responseCapability"]["type"] != "not_resumable")
        .collect();
    let mut failures = Vec::new();
    for run in runs.iter().filter(|r| r.status == RunStatus::Failed) {
        let item = p
            .turn_items
            .iter()
            .filter(|i| {
                i["runId"] == run.id
                    && i["nodeId"].as_str() == run.root_node_id.as_deref()
                    && i["type"] == "error"
                    && i["status"] == "failed"
                    && i.get("failure").is_some()
            })
            .max_by(|a, b| {
                timestamp(a["updatedAt"].as_str())
                    .cmp(&timestamp(b["updatedAt"].as_str()))
                    .then(a["ordinal"].as_u64().cmp(&b["ordinal"].as_u64()))
                    .then(a["id"].as_str().cmp(&b["id"].as_str()))
            });
        let mut evidence = order(
            s.snapshot_sequence,
            EvidenceScope::Full,
            thread["updatedAt"].as_str().unwrap_or_default(),
        );
        evidence.run_identity = Some(RunIdentity {
            provider: Some(sanitize_failure_identifier(&run.provider_instance_id)),
            model: sanitize_failure_identifier(&run.model_selection.model),
        });
        evidence.item = item
            .map(|i| {
                serde_json::from_value(
                    json!({"updatedAt":i["updatedAt"],"ordinal":i["ordinal"],"id":i["id"]}),
                )
            })
            .transpose()?;
        failures.push(TurnFailure {
            order: Some(evidence),
            turn_id: run.id.clone(),
            provider: run.provider_instance_id.clone(),
            model_selection: run.model_selection.clone(),
            failure: item
                .map(|i| serde_json::from_value(i["failure"].clone()))
                .transpose()?,
            retry: item
                .and_then(|i| i.get("retry"))
                .map(|v| serde_json::from_value(v.clone()))
                .transpose()?,
        });
    }
    let executed = runs.iter().rev().find(|r| {
        r.status != RunStatus::Queued
            && !(r.status == RunStatus::Cancelled && r.started_at.is_none())
    });
    let executed_failure = executed
        .and_then(|r| failures.iter().find(|f| f.turn_id == r.id))
        .and_then(|f| f.failure.as_ref());
    let session_error = executed
        .and_then(|r| {
            p.provider_sessions
                .iter()
                .filter(|s| s["providerInstanceId"] == r.provider_instance_id)
                .max_by_key(|s| timestamp(s["updatedAt"].as_str()))
        })
        .and_then(|s| s["lastError"].as_str());
    let limited = executed.filter(|_| {
        executed_failure.is_some_and(|f| {
            f.class == "usage_limit" && session_error.is_none_or(|s| s == f.message)
        })
    });
    let latest = active
        .or(limited)
        .or_else(|| runs.iter().rev().find(|r| r.status != RunStatus::Queued))
        .or_else(|| runs.last());
    let failure = latest
        .and_then(|r| failures.iter().find(|f| f.turn_id == r.id))
        .and_then(|f| f.failure.as_ref());
    let mut value = thread.clone();
    let status = latest
        .map(|r| match r.status {
            RunStatus::Preparing => "preparing",
            RunStatus::Queued => "queued",
            RunStatus::Starting => "starting",
            RunStatus::Running => "running",
            RunStatus::Waiting => "waiting",
            RunStatus::Completed => "completed",
            RunStatus::Interrupted => "interrupted",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
            RunStatus::RolledBack => "rolled_back",
            RunStatus::Idle => "idle",
        })
        .unwrap_or("idle");
    for (key, v) in [
        ("latestRunId", json!(latest.map(|r| &r.id))),
        (
            "latestRunRequestedAt",
            json!(latest.map(|r| &r.requested_at)),
        ),
        (
            "latestRunStartedAt",
            json!(latest.and_then(|r| r.started_at.as_ref())),
        ),
        (
            "latestRunCompletedAt",
            json!(latest.and_then(|r| r.completed_at.as_ref())),
        ),
        ("activeRunId", json!(active.map(|r| &r.id))),
        ("status", json!(status)),
        ("pendingRuntimeRequest", json!(pending.first())),
        ("lastError", json!(failure.map(|f| &f.message))),
        ("lastErrorClass", json!(failure.map(|f| &f.class))),
        (
            "usageLimitResetAt",
            json!(failure.and_then(|f| f.reset_at.as_ref())),
        ),
        (
            "latestUserMessageAt",
            json!(
                messages
                    .iter()
                    .rev()
                    .find(|m| m["role"] == "user")
                    .and_then(|m| m.get("createdAt"))
            ),
        ),
        (
            "hasActionableProposedPlan",
            json!(p.plans.iter().any(|plan| plan["kind"] == "proposed_plan"
                && matches!(plan["status"].as_str(), Some("draft" | "active")))),
        ),
    ] {
        value[key] = v;
    }
    let mut shell = normalize_shell_thread(value, None)?;
    if let Some(turn) = &mut shell.latest_turn {
        turn.assistant_message_id = messages
            .iter()
            .rev()
            .find(|m| {
                m["runId"].as_str() == latest.map(|r| r.id.as_str()) && m["role"] == "assistant"
            })
            .and_then(|m| m["id"].as_str())
            .map(str::to_owned);
    }
    shell.has_pending_approvals = Some(
        pending
            .iter()
            .any(|r| r["kind"].as_str().is_some_and(approval)),
    );
    shell.has_pending_user_input = Some(pending.iter().any(|r| r["kind"] == "user_input"));
    if let Some(s) = &mut shell.session {
        s.failure_code = failure.and_then(|f| f.code.clone());
    }
    let mut activities:Vec<_>=p.turn_items.iter().filter(|i|i.get("requestId").is_none()).map(|i|json!({"kind":i["type"],"tone":if i["type"]=="error"{"error"}else{"info"},"summary":i["title"].as_str().or(i["type"].as_str()),"payload":i})).collect();
    for request in pending {
        let item = p
            .turn_items
            .iter()
            .find(|i| i["requestId"] == request["id"]);
        let mut payload = item.cloned().unwrap_or(json!({}));
        payload["requestId"] = request["id"].clone();
        payload["responseCapability"] = request["responseCapability"].clone();
        activities.push(json!({"kind":if request["kind"]=="user_input"{"user_input"}else{"approval"},"tone":if request["kind"]=="user_input"{"info"}else{"approval"},"summary":item.and_then(|i|i["prompt"].as_str().or(i["title"].as_str())).or(request["kind"].as_str()),"payload":payload}));
    }
    let messages = messages
        .into_iter()
        .map(|m| {
            let mut m = m.clone();
            m["turnId"] = m["runId"].clone();
            serde_json::from_value(m)
        })
        .collect::<Result<_, _>>()?;
    let turn_recoveries = runs
        .iter()
        .filter(|r| {
            matches!(
                r.status,
                RunStatus::Completed
                    | RunStatus::Interrupted
                    | RunStatus::Cancelled
                    | RunStatus::RolledBack
            )
        })
        .map(|r| TurnRecovery {
            turn_id: r.id.clone(),
            state: if r.status == RunStatus::Completed {
                TurnState::Completed
            } else {
                TurnState::Interrupted
            },
            order: order(
                s.snapshot_sequence,
                EvidenceScope::Full,
                thread["updatedAt"].as_str().unwrap_or_default(),
            ),
        })
        .collect();
    let proposed_plans = p
        .plans
        .into_iter()
        .filter(|p| p["kind"] == "proposed_plan")
        .map(|mut plan| {
            plan["turnId"] = plan["runId"].clone();
            if let Some(markdown) = plan.get("markdown").cloned() {
                plan["planMarkdown"] = markdown;
            }
            plan["implementedAt"] = if plan["status"] == "completed" {
                p.updated_at.clone().into()
            } else {
                Value::Null
            };
            plan
        })
        .collect();
    Ok(ThreadSnapshot {
        snapshot_sequence: s.snapshot_sequence,
        thread: Thread {
            shell,
            messages,
            activities,
            checkpoints: p.checkpoints,
            proposed_plans,
            turn_failures: Some(failures),
            turn_recoveries: Some(turn_recoveries),
        },
        extra: Default::default(),
    })
}
