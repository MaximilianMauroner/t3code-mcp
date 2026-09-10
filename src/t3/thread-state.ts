import type { ThreadShell } from "./types.js";

export type ThreadStatus = "open" | "snoozed" | "settled" | "archived";

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
