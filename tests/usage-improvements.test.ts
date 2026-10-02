import { afterEach, describe, expect, it, vi } from "vitest";
import { readFile } from "node:fs/promises";
import { FakeT3 } from "./support/fake-t3.js";
import { gatewayFixture, type GatewayFixture } from "./support/gateway-fixture.js";
import { makeGateway } from "../src/gateway.js";

const fixtures: GatewayFixture[] = [];
const fakes: FakeT3[] = [];
afterEach(async () => {
  vi.useRealTimers(); vi.restoreAllMocks();
  await Promise.all(fixtures.splice(0).map((fixture) => fixture.cleanup()));
  await Promise.all(fakes.splice(0).map((fake) => fake.close()));
});
async function setup(options: ConstructorParameters<typeof FakeT3>[0] = {}) {
  const fake = new FakeT3(options); fakes.push(fake); await fake.start();
  fake.addProject({ id: "project", workspaceRoot: "/missing-test-workspace" });
  const thread = fake.addThread({ id: "thread", projectId: "project" });
  const fixture = await gatewayFixture(fake); fixtures.push(fixture);
  return { fake, fixture, thread };
}
async function running() {
  const state = await setup();
  const run = await state.fixture.gateway.threadSend({ threadId: "thread", message: "work", idempotencyKey: "run" });
  await state.fixture.gateway.runGet(run.runId); // settle durable turn attribution before fake time
  const snapshot = await state.fixture.client.getThread("thread");
  const descriptor = await state.fixture.client.getDescriptor();
  const observations = vi.spyOn(state.fixture.client, "getThread").mockImplementation(async () => snapshot);
  const descriptors = vi.spyOn(state.fixture.client, "getDescriptor").mockResolvedValue(descriptor);
  return { ...state, thread: snapshot.thread, run, snapshot, descriptor, observations, descriptors };
}

describe("bounded run monitoring", () => {
  it("halves unchanged-run observations, bounds descriptors and never redispatches", async () => {
    const { fixture, run, fake, observations, descriptors } = await running();
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    const pending = fixture.gateway.runWait(run.runId, 30);
    await vi.advanceTimersByTimeAsync(30_000);
    const result = await pending;
    // Old 500 ms loop: initial read + 60 polls, each also fetching identity.
    expect(observations.mock.calls.length).toBeLessThanOrEqual(30);
    expect(descriptors).toHaveBeenCalledTimes(2);
    expect(result).toMatchObject({ runStatus: "running", timedOut: true, monitoring: { observations: 18, requestedTimeoutSeconds: 30 } });
    expect(fake.dispatches).toHaveLength(1);
  });

  it.each(["completed", "approval", "input"])("detects %s within the healthy latency guardrail", async (change) => {
    const { fixture, run, thread, observations } = await running();
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    const pending = fixture.gateway.runWait(run.runId, 30);
    await vi.advanceTimersByTimeAsync(4_000);
    if (change === "completed") thread.latestTurn = { ...thread.latestTurn!, state: "completed" };
    if (change === "approval") thread.hasPendingApprovals = true;
    if (change === "input") thread.hasPendingUserInput = true;
    await vi.advanceTimersByTimeAsync(2_000);
    const result = await pending;
    expect(result.monitoring!.elapsedMs).toBeLessThanOrEqual(6_000);
    expect(result.timedOut).toBeUndefined();
    if (change === "completed") expect(result.runStatus).toBe("completed");
    else expect(result.pendingActions[change === "approval" ? "approvals" : "userInput"]).toBe(true);
    expect(observations.mock.calls.length).toBeLessThan(10);
  });

  it("returns immediately for already pending approval", async () => {
    const { fixture, run, thread, observations } = await running();
    thread.hasPendingApprovals = true;
    const result = await fixture.gateway.runWait(run.runId, 30);
    expect(result.pendingActions.approvals).toBe(true);
    expect(observations).toHaveBeenCalledTimes(1);
    expect(result.timedOut).toBeUndefined();
  });

  it("stops at disconnection instead of consuming the wait interval", async () => {
    const { fixture, run, observations, descriptors } = await running();
    observations.mockRejectedValue(new Error("offline"));
    const result = await fixture.gateway.runWait(run.runId, 30);
    expect(result.connectionStatus).toBe("disconnected");
    expect(result.runStatus).toBe("unknown");
    expect(observations).toHaveBeenCalledTimes(1);
    expect(descriptors).toHaveBeenCalledTimes(1);
    expect(result.monitoring!.elapsedMs).toBeLessThan(2_500);
  });

  it("reports unknown when descriptor revalidation is unavailable", async () => {
    const { fixture, run, descriptor, descriptors, thread } = await running();
    descriptors.mockResolvedValueOnce(descriptor).mockRejectedValue(new Error("descriptor offline"));
    thread.latestTurn = { ...thread.latestTurn!, state: "completed" };
    const result = await fixture.gateway.runWait(run.runId, 30);
    expect(result).toMatchObject({ connectionStatus: "disconnected", runStatus: "unknown", stateFreshness: "unknown", settings: { state: "unresolved" } });
  });

  it("does not retain a failure from an environment that changes during the wait", async () => {
    const { fixture, run, descriptor, descriptors, thread } = await running();
    descriptors.mockResolvedValueOnce(descriptor).mockResolvedValue({ ...descriptor, environmentId: "different" });
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    const pending = fixture.gateway.runWait(run.runId, 30);
    const rejected = expect(pending).rejects.toMatchObject({ code: "environment_mismatch" });
    await vi.advanceTimersByTimeAsync(100);
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", lastError: "foreign failure" };
    await vi.advanceTimersByTimeAsync(1_000);
    await rejected;
    expect(await fixture.journal.getFailureByTurnId("thread", thread.latestTurn.turnId)).toBeNull();
  });

  it("rejects an environment change during a wait even without a configured pin", async () => {
    const { fixture, run, descriptor, descriptors, thread } = await running();
    descriptors.mockResolvedValueOnce(descriptor).mockResolvedValue({ ...descriptor, environmentId: "different" });
    thread.latestTurn = { ...thread.latestTurn!, state: "completed" };
    await expect(fixture.gateway.runWait(run.runId, 30)).rejects.toMatchObject({ code: "environment_mismatch" });
  });
});

describe("requested and observed settings", () => {
  it("reports explicit selection, strips secret options, and preserves provenance after restart", async () => {
    const { fixture, fake } = await setup();
    const run = await fixture.gateway.threadSend({ threadId: "thread", message: "work", idempotencyKey: "explicit",
      modelSelection: { instanceId: "codex", model: "requested", options: { secret: "must-not-persist", reasoningEffort: "high" } }, runtimeMode: "approval-required" });
    expect(run.settings).toMatchObject({ modelSource: "explicit", runtimeSource: "explicit", state: "observed", matchesResolved: true,
      effective: { modelSelection: { instanceId: "codex", model: "requested" }, runtimeMode: "approval-required" } });
    const journal = await readFile(`${fixture.directory}/operations.json`, "utf8");
    expect(journal).not.toContain("must-not-persist"); expect(journal).not.toContain("reasoningEffort");
    const restarted = makeGateway(fixture.config).gateway;
    const retried = await restarted.threadSend({ threadId: "thread", message: "work", idempotencyKey: "explicit",
      modelSelection: { instanceId: "codex", model: "requested", options: { secret: "must-not-persist", reasoningEffort: "high" } }, runtimeMode: "approval-required" });
    expect(retried.settings).toEqual(run.settings && { ...run.settings, effective: { ...run.settings.effective!, observedAt: retried.settings!.effective!.observedAt } });
    expect(fake.dispatches).toHaveLength(1);
  });

  it("retains project-default provenance for a task's initial turn", async () => {
    const { fixture } = await setup();
    const task = await fixture.gateway.taskStart({ projectId: "project", title: "task", instruction: "work", runtimeMode: "full-access", idempotencyKey: "task" });
    expect(task.settings).toMatchObject({ modelSource: "project_default", runtimeSource: "explicit", requested: { modelSelection: null }, state: "observed", matchesResolved: true });
  });

  it("reports follow-up inheritance and does not attribute newer-turn settings to an older run", async () => {
    const { fixture, thread } = await setup();
    const run = await fixture.gateway.threadSend({ threadId: "thread", message: "work", idempotencyKey: "inherited" });
    expect(run.settings).toMatchObject({ modelSource: "thread_inherited", runtimeSource: "thread_inherited", state: "observed" });
    thread.latestTurn = { ...thread.latestTurn!, turnId: "newer-turn" };
    thread.modelSelection = { model: "newer-model", instanceId: "codex" };
    const old = await fixture.gateway.runGet(run.runId);
    expect(old.settings).toMatchObject({ state: "unresolved", effective: null, matchesResolved: null });
  });

  it("marks failed/uncertain dispatch settings unresolved rather than claiming a provider selection", async () => {
    const { fixture } = await setup({ dispatchStatus: 503 });
    const run = await fixture.gateway.threadSend({ threadId: "thread", message: "work", idempotencyKey: "uncertain", modelSelection: { model: "requested" } });
    expect(run.status).toBe("uncertain");
    expect(run.settings).toMatchObject({ state: "unresolved", effective: null, modelSource: "explicit" });
  });

  it("does not call a rejected selection effective", async () => {
    const { fixture } = await setup({ dispatchStatus: 409, dispatchErrorCode: "provider_rejected" });
    const run = await fixture.gateway.threadSend({ threadId: "thread", message: "work", idempotencyKey: "rejected", modelSelection: { model: "requested" } });
    expect(run.status).toBe("rejected");
    expect(run.settings).toMatchObject({ state: "unresolved", effective: null, modelSource: "explicit" });
  });

  it("surfaces a settings mismatch without widening permissions or guessing effort", async () => {
    const { fixture, thread } = await setup();
    const run = await fixture.gateway.threadSend({ threadId: "thread", message: "work", idempotencyKey: "mismatch", runtimeMode: "approval-required" });
    thread.runtimeMode = "full-access";
    const observed = await fixture.gateway.runGet(run.runId);
    expect(observed.settings).toMatchObject({ matchesResolved: false, effective: { runtimeMode: "full-access" }, requested: { runtimeMode: "approval-required" } });
  });
});
