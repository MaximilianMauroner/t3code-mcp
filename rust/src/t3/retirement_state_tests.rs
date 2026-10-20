use super::{shell, timestamp};
use crate::t3::thread_state::{
    ThreadActivity, ThreadStatus, is_in_open_queue, is_thread_working, observe_thread,
    thread_status, thread_status_reason,
};
use serde_json::json;

const NOW: &str = "2026-09-10T12:00:00.000Z";
const BEFORE: &str = "2026-09-10T11:00:00.000Z";
const FUTURE: &str = "2026-09-10T13:00:00.000Z";

#[test]
fn pin_keeps_settled_threads_open_but_respects_the_snooze_deadline() {
    let now = timestamp(Some(NOW)).unwrap();
    let mut thread = shell();
    thread.settled_override = Some("settled".into());
    thread.settled_at = Some(BEFORE.into());
    assert_eq!(thread_status(&thread, now), ThreadStatus::Settled);
    thread.pinned_at = Some(BEFORE.into());
    assert_eq!(thread_status(&thread, now), ThreadStatus::Open);
    assert!(is_in_open_queue(&thread, now));
    thread.snoozed_until = Some(NOW.into());
    for (observed_at, expected, queued) in [
        (now - 1, ThreadStatus::Snoozed, false),
        (now, ThreadStatus::Open, true),
        (now + 1, ThreadStatus::Open, true),
    ] {
        assert_eq!(thread_status(&thread, observed_at), expected);
        assert_eq!(observe_thread(&thread, observed_at).lifecycle, expected);
        assert_eq!(is_in_open_queue(&thread, observed_at), queued);
    }
    thread.pinned_at = None;
    assert_eq!(thread_status(&thread, now), ThreadStatus::Settled);
    assert!(!is_in_open_queue(&thread, now));
}

#[test]
fn invalid_or_absent_snooze_deadlines_do_not_hide_an_idle_thread() {
    let now = timestamp(Some(NOW)).unwrap();
    for deadline in [None, Some("bad-date"), Some(BEFORE), Some(NOW)] {
        let mut thread = shell();
        thread.snoozed_until = deadline.map(str::to_owned);
        assert_eq!(thread_status(&thread, now), ThreadStatus::Open, "{deadline:?}");
        assert!(is_in_open_queue(&thread, now), "{deadline:?}");
    }
    let mut thread = shell();
    thread.snoozed_until = Some(FUTURE.into());
    assert_eq!(thread_status(&thread, now), ThreadStatus::Snoozed);
    assert!(!is_in_open_queue(&thread, now));
}

#[test]
fn pending_actions_wake_pinned_settled_and_running_snoozed_threads() {
    let now = timestamp(Some(NOW)).unwrap();
    for (approval, input, activity) in [
        (true, false, ThreadActivity::AwaitingApproval),
        (false, true, ThreadActivity::AwaitingInput),
        (true, true, ThreadActivity::AwaitingApproval),
    ] {
        let mut thread = shell();
        thread.pinned_at = Some(BEFORE.into());
        thread.settled_override = Some("settled".into());
        thread.snoozed_at = Some(BEFORE.into());
        thread.snoozed_until = Some(FUTURE.into());
        thread.session = Some(serde_json::from_value(json!({"status":"running","updatedAt":NOW})).unwrap());
        assert_eq!(thread_status(&thread, now), ThreadStatus::Snoozed);
        thread.has_pending_approvals = Some(approval);
        thread.has_pending_user_input = Some(input);
        let observation = observe_thread(&thread, now);
        assert_eq!(observation.lifecycle, ThreadStatus::Open);
        assert_eq!(observation.execution, activity);
        assert!(!is_thread_working(&thread));
        assert!(is_in_open_queue(&thread, now));
        assert!(thread_status_reason(&thread, now).contains("pending approval or input"));
    }
}

#[test]
fn only_errors_newer_than_the_acknowledged_snooze_wake_early() {
    let now = timestamp(Some(NOW)).unwrap();
    for (updated_at, expected) in [
        (Some("2026-09-10T10:59:59.999Z"), ThreadStatus::Snoozed),
        (Some(BEFORE), ThreadStatus::Snoozed),
        (Some("2026-09-10T11:00:00.001Z"), ThreadStatus::Open),
        (None, ThreadStatus::Snoozed),
    ] {
        let mut thread = shell();
        thread.snoozed_at = Some(BEFORE.into());
        thread.snoozed_until = Some(FUTURE.into());
        thread.session = Some(serde_json::from_value(json!({"status":"error","updatedAt":updated_at})).unwrap());
        assert_eq!(thread_status(&thread, now), expected, "{updated_at:?}");
        assert_eq!(is_in_open_queue(&thread, now), expected == ThreadStatus::Open);
        if expected == ThreadStatus::Open {
            assert!(thread_status_reason(&thread, now).contains("new session error"));
        }
    }
    let mut thread = shell();
    thread.snoozed_until = Some(FUTURE.into());
    thread.session = Some(serde_json::from_value(json!({"status":"error","updatedAt":NOW})).unwrap());
    assert_eq!(thread_status(&thread, now), ThreadStatus::Open);
    assert!(is_in_open_queue(&thread, now));
}

#[test]
fn completion_wakes_only_after_snoozing_and_requires_a_completed_turn() {
    let now = timestamp(Some(NOW)).unwrap();
    for (state, completed_at, expected) in [
        ("completed", Some("2026-09-10T10:59:59.999Z"), ThreadStatus::Snoozed),
        ("completed", Some(BEFORE), ThreadStatus::Snoozed),
        ("completed", Some("2026-09-10T11:00:00.001Z"), ThreadStatus::Open),
        ("completed", None, ThreadStatus::Snoozed),
        ("running", Some(NOW), ThreadStatus::Snoozed),
        ("interrupted", Some(NOW), ThreadStatus::Snoozed),
        ("error", Some(NOW), ThreadStatus::Snoozed),
    ] {
        let mut thread = shell();
        thread.snoozed_at = Some(BEFORE.into());
        thread.snoozed_until = Some(FUTURE.into());
        thread.latest_turn = Some(serde_json::from_value(json!({"turnId":"finished","state":state,"requestedAt":BEFORE,"completedAt":completed_at})).unwrap());
        assert_eq!(thread_status(&thread, now), expected, "{state} {completed_at:?}");
        assert_eq!(is_in_open_queue(&thread, now), expected == ThreadStatus::Open);
        if expected == ThreadStatus::Open {
            assert!(thread_status_reason(&thread, now).contains("turn completed after snoozing"));
        }
    }
}

#[test]
fn starting_and_running_sessions_keep_their_snooze_until_the_deadline() {
    let now = timestamp(Some(NOW)).unwrap();
    for status in ["starting", "running"] {
        let mut thread = shell();
        thread.pinned_at = Some(BEFORE.into());
        thread.settled_override = Some("settled".into());
        thread.snoozed_until = Some(NOW.into());
        thread.session = Some(serde_json::from_value(json!({"status":status,"updatedAt":NOW})).unwrap());
        assert_eq!(thread_status(&thread, now - 1), ThreadStatus::Snoozed);
        assert!(!is_in_open_queue(&thread, now - 1));
        assert_eq!(thread_status(&thread, now), ThreadStatus::Open);
        assert!(is_thread_working(&thread));
        assert!(!is_in_open_queue(&thread, now));
    }
}

#[test]
fn archive_wins_over_all_early_wake_signals_and_pin() {
    let now = timestamp(Some(NOW)).unwrap();
    let mut thread = shell();
    thread.archived_at = Some(BEFORE.into());
    thread.pinned_at = Some(BEFORE.into());
    thread.snoozed_at = Some(BEFORE.into());
    thread.snoozed_until = Some(FUTURE.into());
    thread.has_pending_approvals = Some(true);
    thread.has_pending_user_input = Some(true);
    thread.session = Some(serde_json::from_value(json!({"status":"error","updatedAt":NOW})).unwrap());
    thread.latest_turn = Some(serde_json::from_value(json!({"turnId":"finished","state":"completed","requestedAt":BEFORE,"completedAt":NOW})).unwrap());
    assert_eq!(thread_status(&thread, now), ThreadStatus::Archived);
    assert_eq!(observe_thread(&thread, now).lifecycle, ThreadStatus::Archived);
    assert!(!is_in_open_queue(&thread, now));
    assert!(thread_status_reason(&thread, now).starts_with("archived"));
    thread.archived_at = None;
    assert_eq!(thread_status(&thread, now), ThreadStatus::Open);
    assert!(is_in_open_queue(&thread, now));
    assert!(thread_status_reason(&thread, now).contains("pending approval or input"));
    thread.has_pending_approvals = Some(false);
    thread.has_pending_user_input = Some(false);
    assert!(thread_status_reason(&thread, now).contains("new session error"));
    thread.session = None;
    assert!(thread_status_reason(&thread, now).contains("turn completed after snoozing"));
}
