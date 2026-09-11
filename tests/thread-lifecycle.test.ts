import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { afterEach, describe, expect, it } from "vitest";
import { createMcpServer } from "../src/mcp/server.js";
import { defaultSnoozePreset, resolveSnoozePresets } from "../src/t3/snooze.js";
import { gatewayFixture, type GatewayFixture } from "./support/gateway-fixture.js";
import { FakeT3 } from "./support/fake-t3.js";

const fakes: FakeT3[] = [];
const fixtures: GatewayFixture[] = [];
const clients: Client[] = [];
const servers: Array<{ close: () => Promise<void> }> = [];

afterEach(async () => {
  await Promise.all(clients.splice(0).map((client) => client.close().catch(() => undefined)));
  await Promise.all(servers.splice(0).map((server) => server.close().catch(() => undefined)));
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

function localAt(hour: number, minute = 0): Date {
  const date = new Date();
  date.setHours(hour, minute, 0, 0);
  return date;
}

describe("snooze presets", () => {
  it("offers this evening in the morning and defaults to it", () => {
    const now = localAt(10);
    const presets = resolveSnoozePresets(now);
    expect(presets.map((preset) => preset.id)).toContain("evening");
    expect(defaultSnoozePreset(now).id).toBe("evening");
  });

  it("drops this evening close to 18:00 and defaults to tomorrow", () => {
    const now = localAt(17, 30);
    const presets = resolveSnoozePresets(now);
    expect(presets.map((preset) => preset.id)).not.toContain("evening");
    expect(defaultSnoozePreset(now).id).toBe("tomorrow");
    expect(presets.map((preset) => preset.id)).toEqual(
      expect.arrayContaining(["hour", "three-hours", "tomorrow"]),
    );
  });
});

describe("thread snooze", () => {
  it("snoozes with the evening/tomorrow default and reports the wake time", async () => {
    const { fake, gateway } = await setup();
    const thread = fake.addThread({ id: "snooze-me" });
    const result = await gateway.threadSnooze({ threadId: thread.id, idempotencyKey: "snooze-1" });

    expect(result.status).toBe("accepted");
    expect(Date.parse(result.snoozedUntil)).toBeGreaterThan(Date.now());
    expect(result.wakeDescription).toBeTruthy();
    expect(thread.snoozedUntil).toBe(result.snoozedUntil);
    expect((await gateway.threadGet(thread.id)).thread.status).toBe("snoozed");
  });

  it("supports every preset and explicit wake times", async () => {
    const { fake, gateway } = await setup();
    for (const preset of ["hour", "three-hours", "tomorrow", "next-week"] as const) {
      const thread = fake.addThread({ id: `snooze-${preset}` });
      const result = await gateway.threadSnooze({ threadId: thread.id, preset, idempotencyKey: `snooze-${preset}` });
      expect(result).toMatchObject({ status: "accepted", preset });
    }
    const custom = fake.addThread({ id: "snooze-custom" });
    const until = new Date(Date.now() + 48 * 3600_000).toISOString();
    const customResult = await gateway.threadSnooze({
      threadId: custom.id,
      snoozedUntil: until,
      idempotencyKey: "snooze-custom",
    });
    expect(customResult).toMatchObject({ status: "accepted", preset: "custom", snoozedUntil: until });
  });

  it("rejects past times, ambiguous input, and blocked threads without dispatch", async () => {
    const { fake, gateway } = await setup();
    const thread = fake.addThread({ id: "blocked" });
    await expect(
      gateway.threadSnooze({ threadId: thread.id, snoozedUntil: new Date(Date.now() - 1000).toISOString(), idempotencyKey: "past" }),
    ).rejects.toMatchObject({ code: "invalid_snooze_time" });
    await expect(
      gateway.threadSnooze({ threadId: thread.id, preset: "hour", snoozedUntil: new Date(Date.now() + 3600_000).toISOString(), idempotencyKey: "both" }),
    ).rejects.toMatchObject({ code: "invalid_snooze_input" });

    thread.hasPendingApprovals = true;
    await expect(gateway.threadSnooze({ threadId: thread.id, idempotencyKey: "pending" })).rejects.toMatchObject({
      code: "snooze_not_allowed",
    });
    expect(fake.dispatches).toHaveLength(0);
  });

  it("snoozes running threads with an honesty note and wakes on unsnooze", async () => {
    const { fake, gateway } = await setup();
    const thread = fake.addThread({
      id: "running-snooze",
      session: { status: "running", updatedAt: new Date().toISOString() },
      latestTurn: { turnId: "turn-1", state: "running", requestedAt: new Date().toISOString() },
    });
    const snoozed = await gateway.threadSnooze({ threadId: thread.id, preset: "hour", idempotencyKey: "run-snooze" });
    expect(snoozed.status).toBe("accepted");
    expect(snoozed.note).toContain("keeps running");

    const repeat = await gateway.threadSnooze({ threadId: thread.id, preset: "hour", idempotencyKey: "run-snooze" });
    expect(repeat.operationId).toBe(snoozed.operationId);
    expect(fake.dispatches.filter(({ command }) => command.type === "thread.snooze")).toHaveLength(1);

    const woken = await gateway.threadUnsnooze({ threadId: thread.id, idempotencyKey: "wake-1" });
    expect(woken.status).toBe("accepted");
    expect(thread.snoozedUntil).toBeNull();
    expect((await gateway.threadGet(thread.id)).thread.status).toBe("open");
  });

  it("reconciles an uncertain snooze that committed remotely", async () => {
    const { fake, gateway } = await setup({
      dispatchStatus: 503,
      dispatchErrorMessage: "response lost after commit",
      applyBeforeDispatchFailure: true,
    });
    const thread = fake.addThread({ id: "uncertain-snooze" });
    const first = await gateway.threadSnooze({ threadId: thread.id, preset: "hour", idempotencyKey: "uncertain" });
    expect(first.status).toBe("uncertain");
    const second = await gateway.threadSnooze({ threadId: thread.id, preset: "hour", idempotencyKey: "uncertain" });
    expect(second.status).toBe("accepted");
    expect(fake.dispatches.filter(({ command }) => command.type === "thread.snooze")).toHaveLength(1);
  });
});

describe("thread settle", () => {
  it("settles an idle thread and reopens it", async () => {
    const { fake, gateway } = await setup();
    const thread = fake.addThread({ id: "settle-me" });
    const settled = await gateway.threadSettle({ threadId: thread.id, idempotencyKey: "settle-1" });

    expect(settled.status).toBe("accepted");
    expect(settled.settledOverride).toBe("settled");
    expect(settled.lifecycle).toBe("settled");
    expect((await gateway.threadGet(thread.id)).thread.status).toBe("settled");

    const reopened = await gateway.threadUnsettle({ threadId: thread.id, idempotencyKey: "reopen-1" });
    expect(reopened.status).toBe("accepted");
    expect(thread.settledOverride).toBe("active");
    expect((await gateway.threadGet(thread.id)).thread.status).toBe("open");
  });

  it("blocks settle for running work and pending approvals without dispatch", async () => {
    const { fake, gateway } = await setup();
    const running = fake.addThread({
      id: "running-settle",
      session: { status: "running", updatedAt: new Date().toISOString() },
      latestTurn: { turnId: "turn-1", state: "running", requestedAt: new Date().toISOString() },
    });
    await expect(gateway.threadSettle({ threadId: running.id, idempotencyKey: "blocked-run" })).rejects.toMatchObject({
      code: "settle_blocked",
    });

    const approval = fake.addThread({ id: "approval-settle", hasPendingApprovals: true });
    await expect(gateway.threadSettle({ threadId: approval.id, idempotencyKey: "blocked-approval" })).rejects.toMatchObject({
      code: "settle_blocked",
    });
    expect(fake.dispatches).toHaveLength(0);
  });
});

describe("lifecycle tools through MCP", () => {
  it("snoozes and settles with structured results", async () => {
    const fake = new FakeT3();
    fakes.push(fake);
    await fake.start();
    const fixture = await gatewayFixture(fake);
    fixtures.push(fixture);
    fake.addThread({ id: "voice-thread", title: "Login fix" });
    const server = createMcpServer(fixture.gateway);
    const client = new Client({ name: "lifecycle", version: "1.0.0" }, { capabilities: {} });
    const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
    await server.connect(serverTransport);
    await client.connect(clientTransport);
    clients.push(client);
    servers.push(server);

    const snoozed = await client.callTool({
      name: "t3_thread_snooze",
      arguments: { threadId: "voice-thread", idempotencyKey: "voice-snooze" },
    });
    expect(snoozed.isError).not.toBe(true);
    expect(snoozed.structuredContent).toMatchObject({ status: "accepted", threadId: "voice-thread" });

    const settled = await client.callTool({
      name: "t3_thread_settle",
      arguments: { threadId: "voice-thread", idempotencyKey: "voice-settle" },
    });
    expect(settled.isError).not.toBe(true);
    expect(settled.structuredContent).toMatchObject({ status: "accepted", lifecycle: "settled" });

    const badSnooze = await client.callTool({
      name: "t3_thread_snooze",
      arguments: { threadId: "voice-thread", preset: "hour", snoozedUntil: new Date(Date.now() + 3600_000).toISOString(), idempotencyKey: "bad" },
    });
    expect(badSnooze.isError).toBe(true);
  });
});
