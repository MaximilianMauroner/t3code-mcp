import { afterEach, describe, expect, it, vi } from "vitest";
import { readFile } from "node:fs/promises";
import { FakeT3 } from "./support/fake-t3.js";
import { gatewayFixture, type GatewayFixture } from "./support/gateway-fixture.js";
import { makeGateway } from "../src/gateway.js";

const fixtures: GatewayFixture[] = [];
const fakes: FakeT3[] = [];
afterEach(async () => {
  vi.restoreAllMocks();
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
  await state.fixture.gateway.runGet(run.runId); // settle durable turn attribution before stubbing reads
  const snapshot = await state.fixture.client.getThread("thread");
  const descriptor = await state.fixture.client.getDescriptor();
  const observations = vi.spyOn(state.fixture.client, "getThread").mockImplementation(async () => snapshot);
  const descriptors = vi.spyOn(state.fixture.client, "getDescriptor").mockResolvedValue(descriptor);
  return { ...state, thread: snapshot.thread, run, snapshot, descriptor, observations, descriptors };
}

describe("run observation honesty", () => {
  it("reports disconnection when the thread read fails", async () => {
    const { fixture, run, observations } = await running();
    observations.mockRejectedValue(new Error("offline"));
    const result = await fixture.gateway.runGet(run.runId);
    expect(result).toMatchObject({ connectionStatus: "disconnected", runStatus: "unknown" });
    expect(observations).toHaveBeenCalledTimes(1);
  });

  it("reports unknown when descriptor revalidation is unavailable", async () => {
    const { fixture, run, descriptors, thread } = await running();
    descriptors.mockRejectedValue(new Error("descriptor offline"));
    thread.latestTurn = { ...thread.latestTurn!, state: "completed" };
    const result = await fixture.gateway.runGet(run.runId);
    expect(result).toMatchObject({ connectionStatus: "disconnected", runStatus: "unknown", stateFreshness: "unknown", settings: { state: "unresolved" } });
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
