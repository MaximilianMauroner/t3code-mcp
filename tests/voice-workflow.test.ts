import { afterEach, describe, expect, it } from "vitest";
import { makeGateway } from "../src/gateway.js";
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
const running = { turnId: "external-turn", state: "running" as const, requestedAt: "2026-09-10T12:00:00Z" };
const interrupt = { threadId: "external-thread", expectedTurnId: running.turnId, idempotencyKey: "voice-stop" };

describe("voice discovery and control", () => {
  it("filters projects by title, path, or ID before pagination", async () => {
    const { fake, gateway } = await setup();
    fake.addProject({ id: "other", title: "Other", workspaceRoot: "/remote/other" });
    fake.addProject({ id: "web-1", title: "Website", workspaceRoot: "/remote/site" });
    fake.addProject({ id: "web-2", title: "App", workspaceRoot: "/remote/web" });
    const first = await gateway.projectsList({ query: " WEB ", limit: 1 });
    const second = await gateway.projectsList({ query: "web", cursor: first.page.nextCursor!, limit: 1 });
    expect(first.page).toMatchObject({ items: [{ id: "web-1" }], total: 2, hasMore: true });
    expect(second.page).toMatchObject({ items: [{ id: "web-2" }], total: 2, hasMore: false });
    expect((await gateway.projectsList({ query: "/remote/site", limit: 10 })).page.items).toHaveLength(1);
  });

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

  it("interrupts external work once and reuses the result after restart", async () => {
    const { fake, gateway, fixture } = await setup();
    fake.addThread({ id: interrupt.threadId, latestTurn: running, session: { status: "running" } });
    const first = await gateway.threadInterrupt(interrupt);
    expect(first.status).toBe("accepted");
    expect(fake.dispatches[0]?.command).toMatchObject({ type: "thread.turn.interrupt", threadId: interrupt.threadId, turnId: running.turnId });
    const restarted = makeGateway(fixture.config).gateway;
    expect(await restarted.threadInterrupt(interrupt)).toEqual(first);
    expect(fake.dispatches).toHaveLength(1);
    await expect(restarted.threadInterrupt({ ...interrupt, expectedTurnId: "another-turn" })).rejects.toThrow("different operation");
  });

  it("deduplicates concurrent interrupt requests", async () => {
    const { fake, gateway } = await setup({ dispatchDelayMs: 30 });
    fake.addThread({ id: interrupt.threadId, latestTurn: running, session: { status: "running" } });
    await Promise.all([gateway.threadInterrupt(interrupt), gateway.threadInterrupt(interrupt)]);
    expect(fake.dispatches).toHaveLength(1);
  });

  it("rejects stale and finished turns without dispatch", async () => {
    const { fake, gateway } = await setup();
    const thread = fake.addThread({ id: interrupt.threadId, latestTurn: { ...running, turnId: "new-turn" } });
    await expect(gateway.threadInterrupt(interrupt)).rejects.toMatchObject({ code: "turn_changed" });
    thread.latestTurn = { ...running, state: "completed" };
    await expect(gateway.threadInterrupt(interrupt)).rejects.toMatchObject({ code: "thread_not_running" });
    expect(fake.dispatches).toHaveLength(0);
  });

  it("denies interruption for read-only, insufficient scope, and wrong environment", async () => {
    const { fake, fixture } = await setup({ scopes: ["orchestration:read"] });
    await expect(fixture.gateway.threadInterrupt(interrupt)).rejects.toMatchObject({ code: "t3_scope_required" });
    await expect(makeGateway({ ...fixture.config, readOnly: true }).gateway.threadInterrupt(interrupt)).rejects.toMatchObject({ code: "gateway_read_only" });
    const other = await setup();
    await expect(makeGateway({ ...other.fixture.config, environmentId: "wrong" }).gateway.threadInterrupt(interrupt)).rejects.toMatchObject({ code: "environment_mismatch" });
    expect(fake.dispatches).toHaveLength(0);
    expect(other.fake.dispatches).toHaveLength(0);
  });

  it("does not replay an uncertain interruption against later work", async () => {
    const { fake, gateway } = await setup({ dispatchStatus: 503, applyBeforeDispatchFailure: true });
    const thread = fake.addThread({ id: interrupt.threadId, latestTurn: running, session: { status: "running" } });
    expect((await gateway.threadInterrupt(interrupt)).status).toBe("uncertain");
    thread.latestTurn = { ...running, turnId: "later-turn" };
    thread.session = { status: "running" };
    expect((await gateway.threadInterrupt(interrupt)).status).toBe("uncertain");
    expect(fake.dispatches).toHaveLength(1);
    expect(thread.latestTurn.state).toBe("running");
  });

  it("adds follow-up messages to an idle existing thread and rejects a second busy turn", async () => {
    const { fake, gateway } = await setup();
    const thread = fake.addThread({ id: "existing", messages: [{ id: "old", role: "assistant", text: "Previous result" }] });
    expect((await gateway.threadSend({ threadId: thread.id, message: "Now check the tests", idempotencyKey: "follow-up" })).status).toBe("accepted");
    expect(thread.messages.map((message) => message.text)).toEqual(["Previous result", "Now check the tests"]);
    await expect(gateway.threadSend({ threadId: thread.id, message: "More work", idempotencyKey: "busy" })).rejects.toMatchObject({ code: "thread_busy" });
  });
});
