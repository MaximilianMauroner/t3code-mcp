import { describe, expect, it } from "vitest";
import { threadStatus } from "../src/t3/thread-state.js";
import { FakeT3 } from "./support/fake-t3.js";
import type { Thread } from "../src/t3/types.js";

const NOW = Date.parse("2026-09-10T12:00:00Z");
const before = "2026-09-10T11:00:00Z";
const future = "2026-09-10T13:00:00Z";
const completed = { turnId: "turn-1", state: "completed" as const, requestedAt: before, completedAt: before };

function status(overrides: Partial<Thread> = {}) {
  return threadStatus(new FakeT3().addThread(overrides), NOW);
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
