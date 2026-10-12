import { afterEach, describe, expect, it } from "vitest";
import { FakeT3 } from "./support/fake-t3.js";
import { gatewayFixture, type GatewayFixture } from "./support/gateway-fixture.js";

const fakes: FakeT3[] = [];
const fixtures: GatewayFixture[] = [];
afterEach(async () => {
  await Promise.all(fixtures.splice(0).map((fixture) => fixture.cleanup()));
  await Promise.all(fakes.splice(0).map((fake) => fake.close()));
});
async function setup(options: ConstructorParameters<typeof FakeT3>[0] = {}) {
  const fake = new FakeT3(options);
  fakes.push(fake);
  await fake.start();
  const fixture = await gatewayFixture(fake);
  fixtures.push(fixture);
  return { fake, fixture, gateway: fixture.gateway };
}
describe("voice discovery and control", () => {
  it("combines project, text, and lifecycle filters before pagination", async () => {
    const { fake, gateway } = await setup();
    fake.addThread({ id: "open", projectId: "p", title: "Login fix" });
    fake.addThread({ id: "settled", projectId: "p", title: "Login fix", settledOverride: "settled" });
    fake.addThread({ id: "snoozed", projectId: "p", title: "Login fix", snoozedUntil: "2099-01-01T00:00:00Z" });
    fake.addThread({ id: "archived", projectId: "p", title: "Login fix", archivedAt: "2026-01-01T00:00:00Z" });
    fake.addThread({ id: "elsewhere", projectId: "q", title: "Login fix" });
    const input = { projectId: "p", query: " LOGIN ", limit: 1, includeArchived: false };
    for (const status of ["open", "snoozed", "settled", "archived"] as const) {
      const result = await gateway.threadsList({ ...input, status });
      expect(result.page).toMatchObject({ items: [{ id: status, status }], total: 1, hasMore: false });
    }
    expect((await gateway.threadsList({ ...input, status: "all", limit: 10 })).page.total).toBe(3);
    expect((await gateway.threadsList({ ...input, status: "all", includeArchived: true, limit: 10 })).page.total).toBe(4);
  });

  it("checks existing threads using the shell flags absent from full thread snapshots", async () => {
    const fakeThread = new FakeT3().addThread({ id: "external-thread", hasPendingApprovals: false });
    const { fake, gateway } = await setup({ responseOverrides: {
      "/api/orchestration/threads/external-thread": { snapshotSequence: 1, thread: fakeThread },
    } });
    fake.addThread({ id: "external-thread", hasPendingApprovals: true, hasActionableProposedPlan: true, backgroundLiveness: "monitoring" });
    expect((await gateway.threadGet("external-thread")).thread).toMatchObject({
      hasPendingApprovals: true, hasActionableProposedPlan: true, backgroundLiveness: "monitoring", status: "open",
    });
  });

  it("adds follow-up messages to an idle existing thread and rejects a second busy turn", async () => {
    const { fake, gateway } = await setup();
    const thread = fake.addThread({ id: "existing", messages: [{ id: "old", role: "assistant", text: "Previous result" }] });
    expect((await gateway.threadSend({ threadId: thread.id, message: "Now check the tests", idempotencyKey: "follow-up" })).status).toBe("accepted");
    expect(thread.messages.map((message) => message.text)).toEqual(["Previous result", "Now check the tests"]);
    await expect(gateway.threadSend({ threadId: thread.id, message: "More work", idempotencyKey: "busy" })).rejects.toMatchObject({ code: "thread_busy" });
  });

  it("exposes running state, activity, reason, and timestamps on summaries", async () => {
    const { fake, gateway } = await setup();
    fake.addThread({
      id: "odd",
      projectId: "p",
      title: "Photo journey",
      branch: "journey-timeline",
      session: { status: "running", updatedAt: "2026-09-11T06:53:00.000Z" },
      latestTurn: { turnId: "turn-odd", state: "completed", requestedAt: "2026-09-11T06:50:00.000Z", completedAt: "2026-09-11T06:52:00.000Z" },
      snoozedAt: "2026-09-10T12:00:00.000Z",
      latestUserMessageAt: "2026-09-11T06:50:00.000Z",
    });
    const result = await gateway.threadsList({ includeArchived: false, status: "all", limit: 10 });
    expect(result.page.items[0]).toMatchObject({
      id: "odd",
      status: "open",
      isRunning: true,
      activity: "running",
      hasConflictingSignals: true,
      sessionUpdatedAt: "2026-09-11T06:53:00.000Z",
      snoozedAt: "2026-09-10T12:00:00.000Z",
      latestUserMessageAt: "2026-09-11T06:50:00.000Z",
    });
    expect(result.page.items[0]?.statusReason).toContain("session is running");
  });

  it("filters running threads server-side by activity or session status", async () => {
    const { fake, gateway } = await setup();
    fake.addThread({ id: "running", projectId: "p", title: "work", session: { status: "running" } });
    fake.addThread({ id: "idle", projectId: "p", title: "work", session: { status: "stopped" } });
    fake.addThread({ id: "starting", projectId: "p", title: "work", session: { status: "starting" } });
    expect((await gateway.threadsList({ includeArchived: false, onlyRunning: true, limit: 10 })).page.items.map((item) => item.id).sort())
      .toEqual(["running", "starting"]);
    expect((await gateway.threadsList({ includeArchived: false, sessionStatus: "running", limit: 10 })).page.items.map((item) => item.id))
      .toEqual(["running"]);
    expect((await gateway.threadsList({ includeArchived: false, onlyRunning: true, sessionStatus: "starting", limit: 10 })).page.items.map((item) => item.id))
      .toEqual(["starting"]);
  });

  it("defaults discovery to open threads that are not working, and keeps explicit review", async () => {
    const { fake, gateway } = await setup();
    const now = new Date().toISOString();
    const running = { session: { status: "running", updatedAt: now } };
    fake.addThread({ id: "idle", projectId: "p" });
    fake.addThread({ id: "approval", projectId: "p", ...running, hasPendingApprovals: true });
    fake.addThread({ id: "question", projectId: "p", backgroundLiveness: "working", hasPendingUserInput: true });
    fake.addThread({ id: "failed", projectId: "p", backgroundLiveness: "working", session: { status: "error", updatedAt: now },
      latestTurn: { turnId: "failed-turn", state: "error", requestedAt: now } });
    fake.addThread({ id: "running", projectId: "p", ...running });
    fake.addThread({ id: "parent", projectId: "p", backgroundLiveness: "working" });
    fake.addThread({ id: "watched", projectId: "p", backgroundLiveness: "monitoring" });
    fake.addThread({ id: "child", projectId: "p", lineage: { parentThreadId: "parent", relationshipToParent: "subagent" } });
    fake.addThread({ id: "settled", projectId: "p", settledOverride: "settled" });
    const ids = (result: { page: { items: ReadonlyArray<{ id: string }> } }) => result.page.items.map((item) => item.id).sort();

    const queue = await gateway.threadsList({ includeArchived: false, limit: 20 });
    expect(queue.scope).toBe("open_queue");
    expect(ids(queue)).toEqual(["approval", "failed", "idle", "question"]);

    const all = await gateway.threadsList({ includeArchived: false, status: "all", limit: 20 });
    expect(all.scope).toBe("filtered");
    expect(ids(all)).toEqual(["approval", "child", "failed", "idle", "parent", "question", "running", "settled", "watched"]);
    expect(ids(await gateway.threadsList({ includeArchived: false, onlyRunning: true, limit: 20 }))).toEqual(["approval", "running"]);
    expect(ids(await gateway.threadsList({ includeArchived: false, status: "open", query: "child", limit: 20 }))).toEqual(["child"]);

    const hidden = await gateway.threadsList({ includeArchived: false, query: "parent", limit: 20 });
    expect(hidden.page.total).toBe(0);
    expect(hidden.resolutionHint).toContain("status=all");
    expect((await gateway.threadGet("parent")).thread).toMatchObject({ id: "parent", backgroundLiveness: "working" });

    const overview = await gateway.threadsOverview({ includeArchived: false, runningLimit: 0 });
    expect(overview).toMatchObject({ total: 9, runningCount: 2, running: [] });
    expect(overview.highlights.map((row) => row.id)).toEqual(["approval", "question", "failed", "idle"]);
    expect((await gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).running.map((row) => row.id).sort())
      .toEqual(["approval", "running"]);
  });

  it("queues a failed session with a stale running turn and ranks it with failures", async () => {
    const { fake, gateway } = await setup();
    const old = "2026-01-01T00:00:00Z";
    fake.addThread({ id: "idle", projectId: "p" });
    fake.addThread({ id: "stale", projectId: "p", createdAt: old, updatedAt: old, session: { status: "error", updatedAt: old },
      latestTurn: { turnId: "stale-turn", state: "running", requestedAt: old } });
    const queue = await gateway.threadsList({ includeArchived: false, limit: 20 });
    expect(queue.page.items.map((item) => item.id).sort()).toEqual(["idle", "stale"]);
    const overview = await gateway.threadsOverview({ includeArchived: false, runningLimit: 0 });
    expect(overview.highlights.map((row) => row.id)).toEqual(["stale", "idle"]);
  });

  it("summarizes lifecycle counts and running threads in one overview call", async () => {
    const { fake, gateway } = await setup();
    fake.addThread({ id: "run-1", projectId: "p", title: "work", session: { status: "running" } });
    fake.addThread({ id: "idle-1", projectId: "p", title: "work", session: { status: "stopped" } });
    fake.addThread({ id: "snoozed-1", projectId: "p", title: "work", snoozedUntil: "2099-01-01T00:00:00Z" });
    fake.addThread({ id: "settled-1", projectId: "p", title: "work", settledOverride: "settled" });
    const overview = await gateway.threadsOverview({ includeArchived: false, runningLimit: 10 });
    expect(overview.total).toBe(4);
    expect(overview.counts).toMatchObject({ open: 2, snoozed: 1, settled: 1, archived: 0 });
    expect(overview.runningCount).toBe(1);
    expect(overview.running.map((item) => item.id)).toEqual(["run-1"]);
    const scoped = await gateway.threadsOverview({ projectId: "missing", includeArchived: false, runningLimit: 10 });
    expect(scoped).toMatchObject({ total: 0, runningCount: 0 });
  });
});
