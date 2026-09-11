import { describe, expect, it } from "vitest";
import {
  hasConflictingSignals,
  isThreadRunning,
  threadActivity,
  threadStatus,
  threadStatusReason,
} from "../src/t3/thread-state.js";
import { FakeT3 } from "./support/fake-t3.js";
import type { Thread } from "../src/t3/types.js";

const NOW = Date.parse("2026-09-10T12:00:00Z");
const before = "2026-09-10T11:00:00Z";
const future = "2026-09-10T13:00:00Z";
const completed = { turnId: "turn-1", state: "completed" as const, requestedAt: before, completedAt: before };

function status(overrides: Partial<Thread> = {}) {
  return threadStatus(new FakeT3().addThread(overrides), NOW);
}

function shell(overrides: Partial<Thread> = {}) {
  return new FakeT3().addThread(overrides);
}

describe("server-backed thread organization", () => {
  it("keeps completed and old threads open without an explicit settled override", () => {
    expect(status()).toBe("open");
    expect(status({ latestTurn: completed })).toBe("open");
    expect(status({ settledOverride: "active", settledAt: before })).toBe("open");
    expect(status({ settledOverride: "settled", settledAt: before })).toBe("settled");
  });

  it("gives archive, snooze, and pin their T3 sidebar precedence", () => {
    expect(status({ archivedAt: before, snoozedUntil: future })).toBe("archived");
    expect(status({ snoozedUntil: future, pinnedAt: before, settledOverride: "settled" })).toBe("snoozed");
    expect(status({ pinnedAt: before, settledOverride: "settled" })).toBe("open");
  });

  it("wakes snoozed threads at the exact deadline and ignores malformed dates", () => {
    for (const snoozedUntil of [before, new Date(NOW).toISOString(), "bad-date"]) {
      expect(status({ snoozedUntil })).toBe("open");
    }
    expect(status({ snoozedUntil: new Date(NOW + 1).toISOString() })).toBe("snoozed");
  });

  it.each(["hasPendingApprovals", "hasPendingUserInput"] as const)("keeps %s visible", (flag) => {
    expect(status({ [flag]: true, snoozedUntil: future, settledOverride: "settled" })).toBe("open");
  });

  it("lets a running thread stay snoozed but never settled", () => {
    expect(status({ session: { status: "running" }, snoozedUntil: future })).toBe("snoozed");
    expect(status({ session: { status: "running" }, settledOverride: "settled" })).toBe("open");
    expect(status({ session: { status: "starting" }, settledOverride: "settled" })).toBe("open");
  });

  it("wakes for new errors or completion, while preserving acknowledged old errors", () => {
    const snooze = { snoozedUntil: future, snoozedAt: before };
    expect(status({ ...snooze, session: { status: "error", updatedAt: new Date(NOW).toISOString() } })).toBe("open");
    expect(status({ ...snooze, session: { status: "error", updatedAt: before } })).toBe("snoozed");
    expect(status({ ...snooze, latestTurn: { ...completed, completedAt: new Date(NOW).toISOString() } })).toBe("open");
    expect(status({ ...snooze, latestTurn: completed })).toBe("snoozed");
  });

  it("keeps unadopted messages open until the grace window passes or T3 settles them", () => {
    const latestUserMessageAt = new Date(NOW - 60_000).toISOString();
    expect(status({ settledOverride: "settled", latestUserMessageAt })).toBe("open");
    expect(status({ settledOverride: "settled", latestUserMessageAt, settledAt: new Date(NOW).toISOString() })).toBe("settled");
    expect(status({ settledOverride: "settled", latestUserMessageAt: before })).toBe("settled");
  });
});

describe("thread activity and signal clarity", () => {
  it("derives running state from session or turn, not lifecycle status", () => {
    expect(isThreadRunning(shell({ session: { status: "running" } }))).toBe(true);
    expect(isThreadRunning(shell({ session: { status: "starting" } }))).toBe(true);
    expect(isThreadRunning(shell({
      session: { status: "stopped" },
      latestTurn: { turnId: "t", state: "running", requestedAt: before },
    }))).toBe(true);
    expect(isThreadRunning(shell())).toBe(false);
    expect(isThreadRunning(shell({ latestTurn: completed }))).toBe(false);
  });

  it("maps pending, starting, running, failed, and idle activity", () => {
    expect(threadActivity(shell({ hasPendingApprovals: true }))).toBe("awaiting_approval");
    expect(threadActivity(shell({ hasPendingUserInput: true }))).toBe("awaiting_input");
    expect(threadActivity(shell({ session: { status: "starting" } }))).toBe("starting");
    expect(threadActivity(shell({ session: { status: "running" } }))).toBe("running");
    expect(threadActivity(shell({ session: { status: "error" } }))).toBe("failed");
    expect(threadActivity(shell())).toBe("idle");
  });

  it("flags completed-turn plus running-session as conflicting", () => {
    expect(hasConflictingSignals(shell({
      session: { status: "running" },
      latestTurn: { ...completed },
    }))).toBe(true);
    expect(hasConflictingSignals(shell({
      session: { status: "stopped" },
      latestTurn: { turnId: "t", state: "running", requestedAt: before },
    }))).toBe(true);
    expect(hasConflictingSignals(shell({ session: { status: "running" } }))).toBe(false);
    expect(hasConflictingSignals(shell())).toBe(false);
  });

  it("explains why a thread is open, snoozed, settled, or archived", () => {
    expect(threadStatusReason(shell({ archivedAt: before }), NOW)).toContain("archived");
    expect(threadStatusReason(shell({ snoozedUntil: future }), NOW)).toContain("snoozed until");
    expect(threadStatusReason(shell({ session: { status: "running" } }), NOW)).toContain("session is running");
    expect(threadStatusReason(shell({
      session: { status: "running" },
      latestTurn: { ...completed },
    }), NOW)).toContain("latest turn is completed");
    expect(threadStatusReason(shell({ hasPendingApprovals: true }), NOW)).toContain("approval is pending");
    expect(threadStatusReason(shell({ settledOverride: "settled", settledAt: before }), NOW)).toContain("settled by explicit override");
    expect(threadStatusReason(shell(), NOW)).toContain("idle without a settled override");
  });

  it("notes when a future snooze is woken early", () => {
    const reason = threadStatusReason(shell({
      snoozedUntil: future,
      snoozedAt: before,
      hasPendingApprovals: true,
    }), NOW);
    expect(reason).toContain("open because approval is pending");
    expect(reason).toContain("woken");
  });
});
