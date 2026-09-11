import type { ThreadShell } from "./types.js";

export type ThreadStatus = "open" | "snoozed" | "settled" | "archived";

export type ThreadActivity =
  | "running"
  | "starting"
  | "awaiting_approval"
  | "awaiting_input"
  | "failed"
  | "idle";

// Mirrors the server-backed portion of T3's threadSettled.ts and Sidebar.tsx.
// UI-local auto-settle preferences and PR state are not available in this API.
export function threadStatus(thread: ThreadShell, now = Date.now()): ThreadStatus {
  if (thread.archivedAt) return "archived";
  const session = thread.session;
  const pending = thread.hasPendingApprovals || thread.hasPendingUserInput;
  const freshError = session?.status === "error" &&
    (thread.snoozedAt == null || Date.parse(session.updatedAt ?? "") > Date.parse(thread.snoozedAt));
  const completedAfterSnooze = thread.snoozedAt != null && thread.latestTurn?.state === "completed" &&
    Date.parse(thread.latestTurn.completedAt ?? "") > Date.parse(thread.snoozedAt);
  if (Date.parse(thread.snoozedUntil ?? "") > now && !pending && !freshError && !completedAfterSnooze) {
    return "snoozed";
  }
  if (thread.pinnedAt || pending || session?.status === "starting" || session?.status === "running") {
    return "open";
  }
  if (thread.settledOverride === "settled") {
    const adjudicated = Date.parse(thread.settledAt ?? "") >= Date.parse(thread.latestUserMessageAt ?? "");
    if (!hasQueuedTurnStart(thread, now) || adjudicated) return "settled";
  }
  return "open";
}

export function hasQueuedTurnStart(thread: ThreadShell, now = Date.now()): boolean {
  if (thread.session?.status === "error") return false;
  const messageAt = Date.parse(thread.latestUserMessageAt ?? "");
  if (!Number.isFinite(messageAt) || Math.abs(now - messageAt) > 120_000) return false;
  const turn = thread.latestTurn;
  return !turn || [turn.requestedAt, turn.startedAt, turn.completedAt].every(
    (time) => time == null || Date.parse(time) < messageAt,
  );
}

export function isThreadRunning(thread: ThreadShell): boolean {
  if (thread.session?.status === "starting" || thread.session?.status === "running") return true;
  return thread.latestTurn?.state === "running";
}

export function threadActivity(thread: ThreadShell): ThreadActivity {
  if (thread.hasPendingApprovals) return "awaiting_approval";
  if (thread.hasPendingUserInput) return "awaiting_input";
  if (thread.session?.status === "starting") return "starting";
  if (thread.session?.status === "running" || thread.latestTurn?.state === "running") return "running";
  if (thread.session?.status === "error" || thread.latestTurn?.state === "error") return "failed";
  return "idle";
}

export function hasConflictingSignals(thread: ThreadShell): boolean {
  const sessionStatus = thread.session?.status;
  const turnState = thread.latestTurn?.state;
  if ((sessionStatus === "starting" || sessionStatus === "running") && turnState !== undefined && turnState !== "running") {
    return true;
  }
  if (turnState === "running" && sessionStatus !== undefined && sessionStatus !== "starting" && sessionStatus !== "running") {
    return true;
  }
  return false;
}

export function threadStatusReason(thread: ThreadShell, now = Date.now()): string {
  if (thread.archivedAt) return `archived at ${thread.archivedAt}`;
  const pendingApprovals = thread.hasPendingApprovals ?? false;
  const pendingInput = thread.hasPendingUserInput ?? false;
  const sessionStatus = thread.session?.status;
  const snoozedUntil = thread.snoozedUntil ?? null;
  const snoozedInFuture = snoozedUntil !== null && Date.parse(snoozedUntil) > now;
  const snoozeWake = snoozedWakeReason(thread);
  const snoozeNote = snoozedInFuture && snoozeWake !== null ? ` (snoozed until ${snoozedUntil} but woken: ${snoozeWake})` : "";

  if (snoozedInFuture && snoozeWake === null) {
    return `snoozed until ${snoozedUntil}`;
  }
  if (pendingApprovals && pendingInput) return `open because approval and user input are pending${snoozeNote}`;
  if (pendingApprovals) return `open because approval is pending${snoozeNote}`;
  if (pendingInput) return `open because user input is pending${snoozeNote}`;
  if (sessionStatus === "starting" || sessionStatus === "running") {
    const turnState = thread.latestTurn?.state;
    if (turnState !== undefined && turnState !== "running") {
      return `open because session is ${sessionStatus} but latest turn is ${turnState}${snoozeNote}`;
    }
    return `open because session is ${sessionStatus}${snoozeNote}`;
  }
  if (thread.latestTurn?.state === "running") {
    return `open because latest turn is running${snoozeNote}`;
  }
  if (thread.pinnedAt) return `open because thread is pinned${snoozeNote}`;
  if (thread.settledOverride === "settled") {
    const adjudicated = Date.parse(thread.settledAt ?? "") >= Date.parse(thread.latestUserMessageAt ?? "");
    if (!hasQueuedTurnStart(thread, now) || adjudicated) {
      return `settled by explicit override at ${thread.settledAt ?? "unknown time"}`;
    }
    return `open because a new user message is waiting for a turn (settled override is blocked)${snoozeNote}`;
  }
  return `open because thread is idle without a settled override${snoozeNote}`;
}

function snoozedWakeReason(thread: ThreadShell): string | null {
  if (thread.hasPendingApprovals || thread.hasPendingUserInput) return "pending approval or input";
  const session = thread.session;
  if (session?.status === "error" &&
    (thread.snoozedAt == null || Date.parse(session.updatedAt ?? "") > Date.parse(thread.snoozedAt))) {
    return "new session error";
  }
  if (thread.snoozedAt != null && thread.latestTurn?.state === "completed" &&
    Date.parse(thread.latestTurn.completedAt ?? "") > Date.parse(thread.snoozedAt)) {
    return "turn completed after snoozing";
  }
  return null;
}
