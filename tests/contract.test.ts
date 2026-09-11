import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { afterEach, describe, expect, it } from "vitest";
import { TOOL_NAMES, toolSchemaFingerprint, GATEWAY_VERSION } from "../src/contract.js";
import { createMcpServer } from "../src/mcp/server.js";
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

describe("slice 1: dependable check-ins", () => {
  it("identifies build, policy, and upstream scopes", async () => {
    const { gateway } = await setup();
    const status = await gateway.connectionStatus();
    expect(status.gatewayVersion).toBe(GATEWAY_VERSION);
    expect(status.gatewayCommit).toBeTruthy();
    expect(status.toolSchemaFingerprint).toBe(toolSchemaFingerprint());
    expect(status.observedAt).toBeTruthy();
    expect(status.effectiveAccessMode).toBe("read-write");
    expect(status.callableOperations).toContain("t3_thread_send");
    expect(status.disabledOperations).toEqual([]);
    expect(status.upstreamScopes).toEqual(status.t3Scopes);
    expect(status.upstreamScopes).toContain("orchestration:operate");
  });

  it("disables mutations with stable reasons when read-only or missing operate scope", async () => {
    const { fixture, fake } = await setup();
    const readOnly = await gatewayFixture(fake, { readOnly: true });
    fixtures.push(readOnly);
    const ro = await readOnly.gateway.connectionStatus();
    expect(ro.effectiveAccessMode).toBe("read-only");
    expect(ro.disabledOperations.map((d) => d.reasonCode)).toContain("gateway_read_only");

    const limited = new FakeT3({ scopes: ["orchestration:read"] });
    fakes.push(limited);
    await limited.start();
    const limitedFixture = await gatewayFixture(limited);
    fixtures.push(limitedFixture);
    const degraded = await limitedFixture.gateway.connectionStatus();
    expect(degraded.effectiveAccessMode).toBe("read-only");
    expect(degraded.disabledOperations.map((d) => d.reasonCode)).toContain("t3_scope_required");
    void fixture;
  });

  it("shares one observation contract across overview, detail, and run", async () => {
    const { fake, gateway } = await setup();
    fake.addThread({
      id: "odd",
      projectId: "p",
      title: "Photo journey",
      session: { status: "running", updatedAt: "2026-09-11T06:53:00.000Z" },
      latestTurn: { turnId: "turn-odd", state: "completed", requestedAt: "2026-09-11T06:50:00.000Z", completedAt: "2026-09-11T06:52:00.000Z" },
    });
    const overview = await gateway.threadsOverview({ includeArchived: false, runningLimit: 10 });
    const listed = await gateway.threadsList({ includeArchived: false, limit: 10 });
    const detail = await gateway.threadGet("odd");
    expect(overview.highlights[0]?.quality).toBe("inconsistent");
    expect(overview.highlights[0]?.warning).toContain("disagree");
    expect(listed.page.items[0]).toMatchObject({ quality: "inconsistent", hasConflictingSignals: true });
    expect(detail.thread).toMatchObject({ quality: "inconsistent", hasConflictingSignals: true });
    expect(detail.thread.warning).toContain("disagree");
  });

  it("bounds overview to five highlights with short excerpts and shared time", async () => {
    const { fake, gateway } = await setup();
    for (let i = 0; i < 7; i++) {
      fake.addThread({
        id: `run-${i}`,
        projectId: "p",
        title: `work ${i}`,
        session: { status: "running", updatedAt: new Date().toISOString() },
        latestTurn: { turnId: `turn-${i}`, state: "running", requestedAt: new Date().toISOString() },
      });
    }
    const thread = fake.thread("run-0");
    thread.messages.push({
      id: "assistant-long",
      role: "assistant",
      text: "x".repeat(1000),
      turnId: "turn-0",
      streaming: false,
      createdAt: new Date().toISOString(),
      updatedAt: new Date().toISOString(),
    });
    const overview = await gateway.threadsOverview({ includeArchived: false, runningLimit: 50 });
    expect(overview.executionCounts.running).toBe(7);
    expect(overview.needsAttentionCount).toBeGreaterThanOrEqual(0);
    expect(overview.highlights.length).toBeLessThanOrEqual(5);
    for (const h of overview.highlights) {
      expect((h.latestResponseExcerpt ?? "").length).toBeLessThanOrEqual(201);
      expect(h.projectId).toBeTruthy();
      expect(h.observedAt).toBe(overview.observedAt);
    }
    expect(overview.running.length).toBeLessThanOrEqual(50);
  });
});

describe("slice 2: deterministic resolution", () => {
  it("filters by attention, sorts deterministically, and hints resolution", async () => {
    const { fake, gateway } = await setup();
    fake.addThread({ id: "b-title", projectId: "p", title: "Beta" });
    fake.addThread({ id: "a-title", projectId: "p", title: "Alpha" });
    fake.addThread({ id: "attention", projectId: "p", title: "Alpha", hasPendingApprovals: true });
    const byTitle = await gateway.threadsList({ includeArchived: false, limit: 10, sort: "title" });
    expect(byTitle.page.items.map((t) => t.id)).toEqual(["a-title", "attention", "b-title"]);
    const attention = await gateway.threadsList({ includeArchived: false, limit: 10, needsAttention: true });
    expect(attention.page.items.map((t) => t.id)).toEqual(["attention"]);
    expect(attention.resolutionHint).toContain("One exact candidate");
    const none = await gateway.threadsList({ includeArchived: false, limit: 10, query: "missing-xyz" });
    expect(none.resolutionHint).toContain("No threads");
    expect(none.resolutionHint).toContain("must not select");
    const multi = await gateway.threadsList({ includeArchived: false, limit: 10, query: "Alpha" });
    expect(multi.resolutionHint).toContain("Multiple candidates");
    const full = await gateway.threadsList({ includeArchived: false, limit: 10, detail: "full", query: "Beta" });
    expect(full.page.items[0]).toMatchObject({ id: "b-title" });
  });
});

describe("slices 3-4: safe follow-up and honest interruption", () => {
  it("discovers provider options read-only", async () => {
    const { fake, gateway } = await setup();
    fake.addProject({ id: "p1", defaultModelSelection: { instanceId: "codex_openai", model: "gpt-5.3-codex-spark" } });
    const providers = await gateway.providersList();
    expect(providers.defaultsByProject["p1"]).toMatchObject({ model: "gpt-5.3-codex-spark" });
    expect(providers.options[0]?.label).toContain("gpt-5.3-codex-spark");
  });

  it("rejects busy threads with the active turn and next actions", async () => {
    const { fake, gateway } = await setup();
    fake.addThread({
      id: "busy",
      latestTurn: { turnId: "turn-busy", state: "running", requestedAt: new Date().toISOString() },
      session: { status: "running", updatedAt: new Date().toISOString() },
    });
    await expect(gateway.threadSend({ threadId: "busy", message: "x", idempotencyKey: "k1" })).rejects.toMatchObject({
      code: "thread_busy",
    });
    try {
      await gateway.threadSend({ threadId: "busy", message: "x", idempotencyKey: "k2" });
    } catch (error) {
      expect(String((error as Error).message)).toContain("turn-busy");
      expect(String((error as Error).message)).toContain("t3_thread_interrupt");
    }
  });

  it("returns durable uncertain handles with reconcile guidance", async () => {
    const { fake, gateway } = await setup({ dispatchStatus: 503, dispatchErrorMessage: "outage" });
    fake.addProject({ id: "p" });
    fake.addThread({ id: "t", projectId: "p" });
    const first = await gateway.threadSend({ threadId: "t", message: "hi", idempotencyKey: "u1" });
    expect(first.status).toBe("uncertain");
    if (first.status === "uncertain") {
      expect(first.reason).toContain("do not resubmit");
      expect(first.reason).toContain(first.operationId);
    }
  });

  it("verifies interruption instead of claiming success", async () => {
    const { fake, gateway } = await setup();
    fake.addThread({
      id: "stop",
      latestTurn: { turnId: "turn-stop", state: "running", requestedAt: new Date().toISOString() },
      session: { status: "running", updatedAt: new Date().toISOString() },
    });
    const result = await gateway.threadInterrupt({ threadId: "stop", expectedTurnId: "turn-stop", idempotencyKey: "s1" });
    expect(result.status).toBe("accepted");
    expect(result.verification?.observed).toBe("interrupted");
  });
});

describe("transport contract", () => {
  it("lists tools, proves fingerprint, and bounds overview through MCP", async () => {
    const fake = new FakeT3();
    fakes.push(fake);
    await fake.start();
    const fixture = await gatewayFixture(fake);
    fixtures.push(fixture);
    fake.addThread({
      id: "odd",
      title: "contradiction",
      session: { status: "running", updatedAt: new Date().toISOString() },
      latestTurn: { turnId: "t-odd", state: "completed", requestedAt: new Date().toISOString(), completedAt: new Date().toISOString() },
    });
    const server = createMcpServer(fixture.gateway);
    const client = new Client({ name: "contract", version: "1.0.0" }, { capabilities: {} });
    const [ct, st] = InMemoryTransport.createLinkedPair();
    await server.connect(st);
    await client.connect(ct);
    clients.push(client);
    servers.push(server);

    const tools = await client.listTools();
    expect(tools.tools.map((t) => t.name).sort()).toEqual([...TOOL_NAMES].sort());
    const status = await client.callTool({ name: "t3_connection_status", arguments: {} });
    expect(status.structuredContent).toMatchObject({
      gatewayVersion: GATEWAY_VERSION,
      toolSchemaFingerprint: toolSchemaFingerprint(),
    });
    const overview = await client.callTool({ name: "t3_threads_overview", arguments: {} });
    const ov = overview.structuredContent as unknown as {
      highlights: Array<{ latestResponseExcerpt: string | null; quality: string; warning: string | null }>;
    };
    expect(ov.highlights.length).toBeLessThanOrEqual(5);
    expect(ov.highlights[0]?.quality).toBe("inconsistent");
    expect(ov.highlights[0]?.warning).toBeTruthy();
  });
});
