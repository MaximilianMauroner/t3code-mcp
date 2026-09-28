import { afterEach, describe, expect, it } from "vitest";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { FakeT3, assistantMessage } from "./support/fake-t3.js";
import { gatewayFixture, type GatewayFixture } from "./support/gateway-fixture.js";
import { makeGateway } from "../src/gateway.js";

const fakes: FakeT3[] = [];
const fixtures: GatewayFixture[] = [];

afterEach(async () => {
  await Promise.all(fixtures.splice(0).map((fixture) => fixture.cleanup()));
  await Promise.all(fakes.splice(0).map((fake) => fake.close()));
});

async function setup(previousResponse = true) {
  const fake = new FakeT3();
  fakes.push(fake);
  await fake.start();
  const fixture = await gatewayFixture(fake);
  fixtures.push(fixture);
  fake.addProject({ id: "failure-project" });
  const thread = fake.addThread({
    id: "failure-thread",
    projectId: "failure-project",
    messages: previousResponse ? [assistantMessage("previous success", "old-turn")] : [],
  });
  const run = await fixture.gateway.threadSend({
    threadId: thread.id,
    message: "new attempt",
    idempotencyKey: "failed-run",
  });
  return { fake, fixture, thread, run };
}

describe("structured provider failures", () => {
  it("reports a pre-response quota failure with no invented reset time", async () => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = {
      status: "error", activeTurnId: turnId, lastError: "Usage limit reached",
      failureCategory: "quota", failureCode: "usage_limit",
    };

    const observed = await fixture.gateway.runWait(run.runId, 0.1);
    expect(observed).toMatchObject({
      runStatus: "failed", latestResponse: null,
      failure: { category: "quota", resetAt: null, retryAfter: null, turnId },
    });
  });

  it("binds source-supplied failure metadata to the exact run and overview", async () => {
    const { fixture, thread, run } = await setup();
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error", completedAt: new Date().toISOString() };
    thread.session = {
      status: "error",
      activeTurnId: turnId,
      providerName: "Codex",
      providerInstanceId: "codex_openai",
      lastError: "Usage limit exhausted Bearer secret-token",
      failureCode: "usage_limit",
      failureCategory: "quota",
      resetAt: "2026-09-15T00:00:00.000Z",
      retryAfter: "PT6H",
    };

    const observed = await fixture.gateway.runGet(run.runId);
    expect(observed).toMatchObject({
      runStatus: "failed",
      latestResponse: null,
      failure: {
        category: "quota",
        code: "usage_limit",
        provider: "Codex",
        model: thread.modelSelection.model,
        turnId,
        resetAt: "2026-09-15T00:00:00.000Z",
        retryAfter: "PT6H",
        source: "t3_session",
      },
    });
    expect(observed.failure?.message).toContain("Bearer [REDACTED]");
    expect(observed.failure?.message).not.toContain("secret-token");

    const detail = await fixture.gateway.threadGet(thread.id);
    expect(detail.thread.failure).toMatchObject({ category: "quota", turnId });
    expect(detail.thread.latestResponse).toBeNull();
    const overview = await fixture.gateway.threadsOverview({ includeArchived: false, runningLimit: 5 });
    expect(overview.highlights[0]?.failure).toMatchObject({ category: "quota", turnId });
  });

  it("does not infer a category or reset time from a 429-looking message", async () => {
    const { fixture, thread, run } = await setup();
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error", completedAt: new Date().toISOString() };
    thread.session = {
      status: "error",
      activeTurnId: turnId,
      providerName: "Provider",
      lastError: "API Error: Request rejected (429) · Usage credits are required for this model.",
    };

    const observed = await fixture.gateway.runGet(run.runId);
    expect(observed.failure).toMatchObject({ category: "unknown", code: null, resetAt: null, retryAfter: null });
  });

  it.each([
    ["rate_limit", "rate_limited", "Too many requests"],
    ["auth_billing", "credits_required", "API Error: Request rejected (429) · Usage credits are required"],
    ["provider_internal", "engine_error", "Engine unavailable"],
  ] as const)("passes through an explicit %s category", async (category, code, message) => {
    const { fixture, thread, run } = await setup();
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: turnId, lastError: message, failureCategory: category, failureCode: code };

    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject({
      category, code, message, resetAt: null, retryAfter: null,
    });
  });

  it("keeps an in-turn partial response and the exact failure after session recovery and journal restart", async () => {
    const { fixture, thread, run } = await setup();
    const turnId = thread.latestTurn!.turnId;
    const partial = assistantMessage("partial current answer", turnId);
    thread.messages.push(partial);
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: turnId, lastError: "Usage limit reached", failureCategory: "quota" };
    const observed = await fixture.gateway.runGet(run.runId);
    expect(observed.latestResponse?.id).toBe(partial.id);
    expect(observed.failure?.message).toBe("Usage limit reached");

    thread.session = { status: "ready", activeTurnId: null, lastError: null };
    const restarted = makeGateway(fixture.config).gateway;
    expect((await restarted.runGet(run.runId)).failure).toMatchObject({ category: "quota", message: "Usage limit reached" });
    expect((await restarted.threadGet(thread.id)).thread.failure?.message).toBe("Usage limit reached");

    thread.latestTurn = { turnId: "later-turn", state: "running", requestedAt: new Date().toISOString() };
    expect((await restarted.runGet(run.runId)).runStatus).toBe("failed");
    expect((await restarted.runGet(run.runId)).failure?.turnId).toBe(turnId);
    expect((await restarted.threadGet(thread.id)).thread.failure).toBeNull();
  });

  it("reports a retained terminal failure while T3 is disconnected", async () => {
    const { fake, fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: turnId, lastError: "Provider stopped" };
    await fixture.gateway.runGet(run.runId);
    await fake.close();

    expect(await fixture.gateway.runGet(run.runId)).toMatchObject({
      runStatus: "failed", connectionStatus: "disconnected",
      failure: { message: "Provider stopped", turnId },
    });
  });

  it("retains a failure first observed through the thread and overview reads", async () => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: turnId, lastError: "First observed in threadGet" };

    expect((await fixture.gateway.threadGet(thread.id)).thread.failure?.message).toBe("First observed in threadGet");
    thread.session = { status: "ready", activeTurnId: null, lastError: null };
    const restarted = makeGateway(fixture.config).gateway;
    expect((await restarted.threadGet(thread.id)).thread.failure?.message).toBe("First observed in threadGet");
    expect((await restarted.runGet(run.runId)).failure?.message).toBe("First observed in threadGet");

    thread.session = { status: "error", activeTurnId: turnId, lastError: "Updated in overview" };
    await restarted.threadsOverview({ includeArchived: false, runningLimit: 5 });
    thread.session = { status: "ready", activeTurnId: null, lastError: null };
    expect((await restarted.threadGet(thread.id)).thread.failure?.message).toBe("Updated in overview");
  });

  it("keeps a precise failure when concurrent reads later see only a generic fallback", async () => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: turnId, lastError: "Precise provider reason" };
    const exact = structuredClone(await fixture.client.getThread(thread.id));
    thread.session = { status: "ready", activeTurnId: null, lastError: null };
    const generic = structuredClone(await fixture.client.getThread(thread.id));
    let releaseFirst!: () => void;
    let releaseSecond!: () => void;
    const firstGate = new Promise<void>((resolve) => { releaseFirst = resolve; });
    const secondGate = new Promise<void>((resolve) => { releaseSecond = resolve; });
    let calls = 0;
    fixture.client.getThread = async () => {
      calls += 1;
      if (calls === 1) {
        await firstGate;
        return exact;
      }
      if (calls === 2) {
        releaseFirst();
        await secondGate;
        return generic;
      }
      return generic;
    };

    const first = fixture.gateway.runGet(run.runId);
    const second = fixture.gateway.runGet(run.runId);
    expect((await first).failure?.message).toBe("Precise provider reason");
    releaseSecond();
    expect((await second).failure?.message).toBe("Precise provider reason");
    expect((await makeGateway(fixture.config).gateway.runGet(run.runId)).failure?.message)
      .toBe("Precise provider reason");
  });

  it("does not use an error from another active turn or leak credential fields", async () => {
    const { fixture, thread, run } = await setup();
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = {
      status: "error", activeTurnId: "other-turn", lastError: "Bearer secret-token",
      providerName: "password=secret-provider", failureCode: "api_key=secret-code",
      failureCategory: "quota", resetAt: "2026-09-15T00:00:00Z",
    };
    const unmatched = (await fixture.gateway.runGet(run.runId)).failure;
    expect(unmatched).toMatchObject({ category: "unknown", code: null, resetAt: null, turnId });
    expect(unmatched?.message).not.toContain("secret-token");

    thread.session.activeTurnId = turnId;
    const matched = (await fixture.gateway.runGet(run.runId)).failure;
    expect(JSON.stringify(matched)).not.toMatch(/secret-token|secret-provider|secret-code/);
    expect(matched?.provider).toBe("password=[REDACTED]");

    thread.session.lastError = '{"api_key":"private-value","password":"private-password","secret":"escaped\\\"value"}';
    const quoted = (await fixture.gateway.runGet(run.runId)).failure;
    expect(JSON.stringify(quoted)).not.toMatch(/private-value|private-password|escaped/);
    expect(await readFile(join(fixture.directory, "operations.json"), "utf8"))
      .not.toMatch(/private-value|private-password|escaped/);
  });

  it("uses a session error without an active turn only when its timestamp belongs to the failed turn", async () => {
    const { fixture, thread, run } = await setup();
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    const oldTime = new Date(Date.parse(thread.latestTurn.requestedAt) - 1000).toISOString();
    thread.session = { status: "error", activeTurnId: null, lastError: "old session error", updatedAt: oldTime };
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject({
      category: "unknown", source: "t3_turn",
    });
    thread.session.updatedAt = new Date(Date.parse(thread.latestTurn.requestedAt) + 1000).toISOString();
    thread.session.lastError = "current session error";
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject({
      message: "current session error", source: "t3_session",
    });
  });

  it("never attaches the current thread failure to an older run", async () => {
    const { fixture, thread, run } = await setup();
    const runTurnId = thread.latestTurn!.turnId;
    thread.latestTurn = {
      turnId: "later-turn",
      state: "error",
      requestedAt: new Date().toISOString(),
      completedAt: new Date().toISOString(),
    };
    thread.session = { status: "error", activeTurnId: "later-turn", lastError: "later failure" };
    thread.messages.push(assistantMessage("later response", "later-turn"));

    const observed = await fixture.gateway.runGet(run.runId);
    expect(observed.t3TurnId).toBe(runTurnId);
    expect(observed.failure).toBeNull();
    expect(observed.latestResponse).toBeNull();
  });
});
