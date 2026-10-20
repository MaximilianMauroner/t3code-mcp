use super::{
    iso, timestamp,
    types::{InteractionMode, ThreadShell, TurnState},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThreadStatus {
    Open,
    Snoozed,
    Settled,
    Archived,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadActivity {
    Running,
    Starting,
    AwaitingApproval,
    AwaitingInput,
    Failed,
    Idle,
}
impl ThreadActivity {
    fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Starting => "starting",
            Self::AwaitingApproval => "awaiting_approval",
            Self::AwaitingInput => "awaiting_input",
            Self::Failed => "failed",
            Self::Idle => "idle",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ObservationQuality {
    Fresh,
    Stale,
    Incomplete,
    Inconsistent,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadObservation {
    pub lifecycle: ThreadStatus,
    pub execution: ThreadActivity,
    pub quality: ObservationQuality,
    pub warning: Option<String>,
    pub observed_turn_id: Option<String>,
    pub observed_at: String,
}
fn session(thread: &ThreadShell) -> Option<&str> {
    thread.session.as_ref().map(|s| s.status.as_str())
}
fn pending(t: &ThreadShell) -> bool {
    t.has_pending_approvals == Some(true) || t.has_pending_user_input == Some(true)
}
fn after(a: Option<&str>, b: Option<&str>) -> bool {
    matches!((timestamp(a),timestamp(b)),(Some(a),Some(b)) if a>b)
}
fn adjudicated(t: &ThreadShell) -> bool {
    matches!((timestamp(t.settled_at.as_deref()),timestamp(t.latest_user_message_at.as_deref())),(Some(a),Some(b)) if a>=b)
}
fn truthy(value: &Option<String>) -> bool {
    value.as_ref().is_some_and(|v| !v.is_empty())
}
fn wake(t: &ThreadShell) -> Option<&'static str> {
    if pending(t) {
        return Some("pending approval or input");
    }
    if session(t) == Some("error")
        && (t.snoozed_at.is_none()
            || after(
                t.session.as_ref().and_then(|s| s.updated_at.as_deref()),
                t.snoozed_at.as_deref(),
            ))
    {
        return Some("new session error");
    }
    if t.snoozed_at.is_some()
        && t.latest_turn.as_ref().is_some_and(|turn| {
            turn.state == TurnState::Completed
                && after(turn.completed_at.as_deref(), t.snoozed_at.as_deref())
        })
    {
        return Some("turn completed after snoozing");
    }
    None
}
pub fn thread_status(t: &ThreadShell, now: i64) -> ThreadStatus {
    if truthy(&t.archived_at) {
        return ThreadStatus::Archived;
    }
    if timestamp(t.snoozed_until.as_deref()).is_some_and(|at| at > now) && wake(t).is_none() {
        return ThreadStatus::Snoozed;
    }
    if truthy(&t.pinned_at) || pending(t) || matches!(session(t), Some("starting" | "running")) {
        return ThreadStatus::Open;
    }
    if t.settled_override.as_deref() == Some("settled")
        && (!has_queued_turn_start(t, now) || adjudicated(t))
    {
        return ThreadStatus::Settled;
    }
    ThreadStatus::Open
}
pub fn has_queued_turn_start(t: &ThreadShell, now: i64) -> bool {
    if session(t) == Some("error") {
        return false;
    }
    let Some(at) = timestamp(t.latest_user_message_at.as_deref()) else {
        return false;
    };
    if now.abs_diff(at) > 120_000 {
        return false;
    }
    t.latest_turn.as_ref().is_none_or(|turn| {
        [
            Some(turn.requested_at.as_str()),
            turn.started_at.as_deref(),
            turn.completed_at.as_deref(),
        ]
        .into_iter()
        .all(|v| v.is_none() || timestamp(v).is_some_and(|v| v < at))
    })
}
pub fn is_thread_running(t: &ThreadShell) -> bool {
    matches!(session(t), Some("starting" | "running"))
        || t.latest_turn
            .as_ref()
            .is_some_and(|t| t.state == TurnState::Running)
}
pub fn thread_activity(t: &ThreadShell) -> ThreadActivity {
    if t.has_pending_approvals == Some(true) {
        ThreadActivity::AwaitingApproval
    } else if t.has_pending_user_input == Some(true) {
        ThreadActivity::AwaitingInput
    } else if session(t) == Some("starting") {
        ThreadActivity::Starting
    } else if session(t) == Some("running")
        || t.latest_turn
            .as_ref()
            .is_some_and(|t| t.state == TurnState::Running)
    {
        ThreadActivity::Running
    } else if session(t) == Some("error")
        || t.latest_turn
            .as_ref()
            .is_some_and(|t| t.state == TurnState::Error)
    {
        ThreadActivity::Failed
    } else {
        ThreadActivity::Idle
    }
}
pub fn is_thread_working(t: &ThreadShell) -> bool {
    if pending(t) || session(t) == Some("error") {
        return false;
    }
    if is_thread_running(t) {
        return true;
    }
    t.background_liveness.is_some()
        && thread_activity(t) != ThreadActivity::Failed
        && !(t.interaction_mode == Some(InteractionMode::Plan)
            && t.has_actionable_proposed_plan == Some(true))
}
pub fn is_in_open_queue(t: &ThreadShell, now: i64) -> bool {
    thread_status(t, now) == ThreadStatus::Open
        && t.lineage
            .as_ref()
            .and_then(|l| l["relationshipToParent"].as_str())
            != Some("subagent")
        && !is_thread_working(t)
}
pub fn has_conflicting_signals(t: &ThreadShell) -> bool {
    let state = t.latest_turn.as_ref().map(|t| t.state);
    (matches!(session(t), Some("starting" | "running"))
        && state.is_some_and(|v| v != TurnState::Running))
        || (state == Some(TurnState::Running)
            && session(t).is_some_and(|s| s != "starting" && s != "running"))
}
pub fn thread_status_reason(t: &ThreadShell, now: i64) -> String {
    if let Some(at) = t.archived_at.as_ref().filter(|a| !a.is_empty()) {
        return format!("archived at {at}");
    }
    let future = timestamp(t.snoozed_until.as_deref()).is_some_and(|at| at > now);
    let wake = wake(t);
    let note = if let (true, Some(why)) = (future, wake) {
        format!(
            " (snoozed until {} but woken: {why})",
            t.snoozed_until.as_deref().unwrap_or_default()
        )
    } else {
        String::new()
    };
    if future && wake.is_none() {
        return format!(
            "snoozed until {}",
            t.snoozed_until.as_deref().unwrap_or_default()
        );
    }
    let reason = if t.has_pending_approvals == Some(true) && t.has_pending_user_input == Some(true)
    {
        "approval and user input are pending"
    } else if t.has_pending_approvals == Some(true) {
        "approval is pending"
    } else if t.has_pending_user_input == Some(true) {
        "user input is pending"
    } else if let Some(s @ ("starting" | "running")) = session(t) {
        if let Some(turn) = t
            .latest_turn
            .as_ref()
            .filter(|turn| turn.state != TurnState::Running)
        {
            return format!(
                "open because session is {s} but latest turn is {}{note}",
                match turn.state {
                    TurnState::Completed => "completed",
                    TurnState::Interrupted => "interrupted",
                    TurnState::Error => "error",
                    TurnState::Running => "running",
                }
            );
        }
        return format!("open because session is {s}{note}");
    } else if t
        .latest_turn
        .as_ref()
        .is_some_and(|turn| turn.state == TurnState::Running)
    {
        "latest turn is running"
    } else if truthy(&t.pinned_at) {
        "thread is pinned"
    } else if t.settled_override.as_deref() == Some("settled") {
        if !has_queued_turn_start(t, now) || adjudicated(t) {
            return format!(
                "settled by explicit override at {}",
                t.settled_at.as_deref().unwrap_or("unknown time")
            );
        }
        "a new user message is waiting for a turn (settled override is blocked)"
    } else {
        "thread is idle without a settled override"
    };
    format!("open because {reason}{note}")
}
pub fn observe_thread(t: &ThreadShell, now: i64) -> ThreadObservation {
    let execution = thread_activity(t);
    let (quality, warning) = if has_conflicting_signals(t) {
        (
            ObservationQuality::Inconsistent,
            Some(format!(
                "T3 signals disagree: {}. Treat status as uncertain and read the thread before acting.",
                thread_status_reason(t, now)
            )),
        )
    } else if t.session.is_none() && t.latest_turn.is_none() && !pending(t) {
        (ObservationQuality::Incomplete,Some("T3 omitted session and turn signals. Treat status as incomplete and read the thread before acting.".into()))
    } else if matches!(
        execution,
        ThreadActivity::Running | ThreadActivity::Starting
    ) {
        let active = timestamp(t.session.as_ref().and_then(|s| s.updated_at.as_deref()))
            .or_else(|| timestamp(t.latest_turn.as_ref().map(|v| v.requested_at.as_str())));
        match active {
            Some(at) if now - at > 300_000 => (
                ObservationQuality::Stale,
                Some(format!(
                    "Active {} signal is older than 5 minutes. It may be stale; read the thread before acting.",
                    execution.label()
                )),
            ),
            None => (
                ObservationQuality::Incomplete,
                Some(format!(
                    "Active {} signal has no timestamp. Treat status as incomplete and read the thread before acting.",
                    execution.label()
                )),
            ),
            _ => (ObservationQuality::Fresh, None),
        }
    } else {
        (ObservationQuality::Fresh, None)
    };
    ThreadObservation {
        lifecycle: thread_status(t, now),
        execution,
        quality,
        warning,
        observed_turn_id: t.latest_turn.as_ref().map(|v| v.turn_id.clone()),
        observed_at: iso(now),
    }
}
pub fn needs_attention_for(t: &ThreadShell, now: i64) -> bool {
    let observation = observe_thread(t, now);
    pending(t)
        || matches!(
            observation.quality,
            ObservationQuality::Inconsistent | ObservationQuality::Stale
        )
        || observation.execution == ThreadActivity::Failed
}
