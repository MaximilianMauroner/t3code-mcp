import { afterEach, describe, expect, it } from "vitest";
import { FakeT3, assistantMessage } from "./support/fake-t3.js";
import { gatewayFixture, type GatewayFixture } from "./support/gateway-fixture.js";

const fakes: FakeT3[] = [];
const fixtures: GatewayFixture[] = [];

afterEach(async () => {
  await Promise.all(fixtures.splice(0).map((fixture) => fixture.cleanup()));
  await Promise.all(fakes.splice(0).map((fake) => fake.close()));
});

async function setup() {
  const fake = new FakeT3();
  fakes.push(fake);
  await fake.start();
  const fixture = await gatewayFixture(fake);
  fixtures.push(fixture);
  fake.addProject({ id: "failure-project" });
  const thread = fake.addThread({
    id: "failure-thread",
    projectId: "failure-project",
    messages: [assistantMessage("previous success", "old-turn")],
  });
  const run = await fixture.gateway.threadSend({
    threadId: thread.id,
    message: "new attempt",
    idempotencyKey: "failed-run",
  });
  return { fake, fixture, thread, run };
}

describe("structured provider failures", () => {
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
