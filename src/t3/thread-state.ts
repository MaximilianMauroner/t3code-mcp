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

export type ObservationQuality = "fresh" | "stale" | "incomplete" | "inconsistent";

export interface ThreadObservation {
  readonly lifecycle: ThreadStatus;
  readonly execution: ThreadActivity;
  readonly quality: ObservationQuality;
  readonly warning: string | null;
  readonly observedTurnId: string | null;
  readonly observedAt: string;
}

// Stale when a running/starting signal has not been refreshed recently.
// Idle threads with old timestamps are not stale; only active signals age out.
const STALE_ACTIVE_AFTER_MS = 5 * 60_000;

export function observeThread(thread: ThreadShell, now = Date.now()): ThreadObservation {
  const lifecycle = threadStatus(thread, now);
  const execution = threadActivity(thread);
  const observedTurnId = thread.latestTurn?.turnId ?? null;
  const observedAt = new Date(now).toISOString();
  const conflicting = hasConflictingSignals(thread);
  if (conflicting) {
    const reason = threadStatusReason(thread, now);
    return {
      lifecycle,
      execution,
      quality: "inconsistent",
      warning: `T3 signals disagree: ${reason}. Treat status as uncertain and read the thread before acting.`,
      observedTurnId,
      observedAt,
    };
  }
  const sessionStatus = thread.session?.status ?? null;
  const hasSession = sessionStatus !== null;
  const hasTurn = thread.latestTurn != null;
  const hasPending = thread.hasPendingApprovals === true || thread.hasPendingUserInput === true;
  if (!hasSession && !hasTurn && !hasPending) {
    return {
      lifecycle,
      execution,
      quality: "incomplete",
      warning: "T3 omitted session and turn signals. Treat status as incomplete and read the thread before acting.",
      observedTurnId,
      observedAt,
    };
  }
  if (execution === "running" || execution === "starting") {
    const updatedAt = Date.parse(thread.session?.updatedAt ?? "");
    const requestedAt = Date.parse(thread.latestTurn?.requestedAt ?? "");
    const lastActive = Number.isFinite(updatedAt)
      ? updatedAt
      : Number.isFinite(requestedAt)
        ? requestedAt
        : Number.NaN;
    if (Number.isFinite(lastActive) && now - lastActive > STALE_ACTIVE_AFTER_MS) {
      return {
        lifecycle,
        execution,
        quality: "stale",
        warning: `Active ${execution} signal is older than 5 minutes. It may be stale; read the thread before acting.`,
        observedTurnId,
        observedAt,
      };
    }
    if (!Number.isFinite(lastActive)) {
      return {
        lifecycle,
        execution,
        quality: "incomplete",
        warning: `Active ${execution} signal has no timestamp. Treat status as incomplete and read the thread before acting.`,
        observedTurnId,
        observedAt,
      };
    }
  }
  return { lifecycle, execution, quality: "fresh", warning: null, observedTurnId, observedAt };
}

export function needsAttentionFor(thread: ThreadShell, now = Date.now()): boolean {
  if (thread.hasPendingApprovals === true || thread.hasPendingUserInput === true) return true;
  const observation = observeThread(thread, now);
  if (observation.quality === "inconsistent" || observation.quality === "stale") return true;
  if (observation.execution === "failed") return true;
  return false;
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
