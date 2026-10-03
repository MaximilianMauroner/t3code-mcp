import { afterEach, describe, expect, it } from "vitest";
import { readFile } from "node:fs/promises";
import { watch } from "node:fs";
import { join } from "node:path";
import { FakeT3, assistantMessage } from "./support/fake-t3.js";
import { gatewayFixture, type GatewayFixture } from "./support/gateway-fixture.js";
import { makeGateway } from "../src/gateway.js";
import { MessageSchema, ThreadSchema } from "../src/t3/types.js";
import claudeRateLimit from "./fixtures/failures/v1-claude-rate-limit.json" with { type: "json" };
import codexUsageLimit from "./fixtures/failures/v1-codex-usage-limit.json" with { type: "json" };
import creditsRequired from "./fixtures/failures/v1-credits-required.json" with { type: "json" };
import authUnavailable from "./fixtures/failures/v1-auth-unavailable.json" with { type: "json" };
import unboundStartError from "./fixtures/failures/v1-unbound-start-error.json" with { type: "json" };

const fakes: FakeT3[] = [];
const fixtures: GatewayFixture[] = [];

afterEach(async () => {
  await Promise.all(fixtures.splice(0).map((fixture) => fixture.cleanup()));
  await Promise.all(fakes.splice(0).map((fake) => fake.close()));
});

async function setup(previousResponse = true, model?: string) {
  const fake = new FakeT3();
  fakes.push(fake);
  await fake.start();
  const fixture = await gatewayFixture(fake);
  fixtures.push(fixture);
  fake.addProject({ id: "failure-project" });
  const thread = fake.addThread({
    id: "failure-thread",
    projectId: "failure-project",
    ...(model ? { modelSelection: { model, provider: "bedrock" } } : {}),
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
  it.each(["list", "overview"])("records recovery outside %s filters before disconnection", async (reader) => {
    const { fixture, fake, thread, run } = await setup(false);
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: thread.latestTurn.turnId, lastError: "Old failure" };
    expect((await fixture.gateway.runGet(run.runId)).failure).not.toBeNull();
    thread.latestTurn = { ...thread.latestTurn!, state: "completed" };
    thread.session = { status: "ready", lastError: null };
    const recoveredShell = structuredClone(await fixture.client.getShell());
    recoveredShell.snapshotSequence += 1;
    fixture.client.getShell = async () => recoveredShell;
    if (reader === "list") {
      expect((await fixture.gateway.threadsList({ includeArchived: false, activity: "failed", limit: 5 })).page.items).toHaveLength(0);
    } else {
      expect((await fixture.gateway.threadsOverview({ includeArchived: false, query: "no match", runningLimit: 5 })).total).toBe(0);
    }
    await fake.close();
    expect((await makeGateway(fixture.config).gateway.runGet(run.runId)).failure).toBeNull();
  });

  it.each([[1, 2], [2, 1]] as const)("rejects a delayed protocol %s failure after protocol %s recovery", async (oldProtocol, newProtocol) => {
    const { fixture, thread, run } = await setup(false);
    await fixture.gateway.runGet(run.runId);
    const failed = structuredClone(await fixture.client.getThread(thread.id));
    failed.snapshotSequence = 100;
    failed.thread.orchestrationProtocolVersion = oldProtocol;
    failed.thread.latestTurn = { ...failed.thread.latestTurn!, state: "error" };
    failed.thread.session = { status: "error", activeTurnId: failed.thread.latestTurn.turnId, lastError: "Old protocol failure" };
    const recovered = structuredClone(failed);
    recovered.snapshotSequence = 1;
    recovered.thread.orchestrationProtocolVersion = newProtocol;
    recovered.thread.latestTurn = { ...recovered.thread.latestTurn!, state: "completed" };
    recovered.thread.session = { status: "ready", activeTurnId: null, lastError: null };
    let release!: () => void;
    let entered!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    const started = new Promise<void>((resolve) => { entered = resolve; });
    let calls = 0;
    fixture.client.getThread = async () => { if (++calls === 1) { entered(); await gate; return failed; } return recovered; };
    const delayed = fixture.gateway.runGet(run.runId);
    await started;
    expect(await fixture.gateway.runGet(run.runId)).toMatchObject({ runStatus: "completed", failure: null });
    release();
    expect(await delayed).toMatchObject({ runStatus: "completed", failure: null });
    const restarted = makeGateway(fixture.config);
    // Repeat the prior candidate's recorded order through the journal, as a delayed in-flight observer would.
    expect(await restarted.journal.retainTerminalFailure(thread.id, failed.thread.latestTurn!.turnId,
      { category: "unknown", code: null, message: "Old protocol failure", class: null, retry: null, retryable: null,
        turnId: failed.thread.latestTurn!.turnId, model: thread.modelSelection.model, provider: null,
        resetAt: null, retryAfter: null, source: "t3_session" }, undefined,
      { protocolVersion: oldProtocol, scope: "full", snapshotSequence: 100, readStartedAt: 1 })).toBeNull();
  });

  it.each([[1, 2], [2, 1]] as const)("uses later protocol %s shell evidence after a protocol %s full read", async (shellProtocol, fullProtocol) => {
    const { fixture, thread, run } = await setup(false);
    await fixture.gateway.runGet(run.runId);
    const full = structuredClone(await fixture.client.getThread(thread.id));
    full.snapshotSequence = 100;
    full.thread.orchestrationProtocolVersion = fullProtocol;
    full.thread.latestTurn = { ...full.thread.latestTurn!, state: "error" };
    full.thread.session = { status: "error", activeTurnId: full.thread.latestTurn.turnId, lastError: "Old full failure" };
    const shell = structuredClone(await fixture.client.getShell());
    shell.snapshotSequence = 1;
    const row = shell.threads[0]!;
    row.orchestrationProtocolVersion = shellProtocol;
    row.latestTurn = full.thread.latestTurn;
    row.session = { status: "error", activeTurnId: row.latestTurn.turnId, lastError: "New shell failure", lastErrorClass: "provider_error" };
    if (fullProtocol === 2) {
      full.thread.turnFailures = [{ turnId: full.thread.latestTurn.turnId, provider: "old-provider", modelSelection: { model: "old-model" },
        failure: { class: "provider_error", code: "old-code", message: "Old full failure", retryable: false },
        order: { protocolVersion: 2, scope: "full", snapshotSequence: 100 } }];
    }
    fixture.client.getThread = async () => full;
    fixture.client.getShell = async () => shell;
    expect((await fixture.gateway.runGet(run.runId)).failure?.message).toBe("Old full failure");
    expect((await fixture.gateway.threadGet(thread.id)).thread.failure?.message).toBe("New shell failure");
    expect((await makeGateway(fixture.config).journal.getFailureByTurnId(thread.id, row.latestTurn.turnId))?.message).toBe("New shell failure");
  });

  it.each([[1, 2], [2, 1]] as const)("rejects a first protocol %s read after switching to %s and back", async (firstProtocol, middleProtocol) => {
    const { fixture, thread, run } = await setup(false);
    await fixture.gateway.runGet(run.runId);
    const failed = structuredClone(await fixture.client.getThread(thread.id));
    failed.snapshotSequence = 100;
    failed.thread.orchestrationProtocolVersion = firstProtocol;
    failed.thread.latestTurn = { ...failed.thread.latestTurn!, state: "error" };
    failed.thread.session = { status: "error", activeTurnId: failed.thread.latestTurn.turnId, lastError: "Old era failure" };
    const recovered = structuredClone(failed);
    recovered.snapshotSequence = 1;
    recovered.thread.orchestrationProtocolVersion = middleProtocol;
    recovered.thread.latestTurn = { ...recovered.thread.latestTurn!, state: "completed" };
    recovered.thread.session = { status: "ready", activeTurnId: null, lastError: null };
    const final = structuredClone(recovered);
    final.thread.orchestrationProtocolVersion = firstProtocol;
    let release!: () => void;
    let entered!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    const started = new Promise<void>((resolve) => { entered = resolve; });
    let calls = 0;
    fixture.client.getThread = async () => {
      if (++calls === 1) { entered(); await gate; return failed; }
      return calls === 2 ? recovered : final;
    };
    const delayed = fixture.gateway.runGet(run.runId);
    await started;
    expect((await fixture.gateway.runGet(run.runId)).failure).toBeNull();
    expect((await fixture.gateway.runGet(run.runId)).failure).toBeNull();
    release();
    expect(await delayed).toMatchObject({ runStatus: "completed", failure: null });
    const restarted = makeGateway(fixture.config);
    expect(await restarted.journal.retainTerminalFailure(thread.id, failed.thread.latestTurn!.turnId,
      { category: "unknown", code: null, message: "Old era failure", class: null, retry: null, retryable: null,
        turnId: failed.thread.latestTurn!.turnId, model: thread.modelSelection.model, provider: null,
        resetAt: null, retryAfter: null, source: "t3_session" }, undefined,
      { protocolVersion: firstProtocol, scope: "full", snapshotSequence: 100, readStartedAt: 1 })).toBeNull();
  });

  it.each(["anthropic.claude-3-5-sonnet-20240620-v1:0", "vendor.token:0", "azure.openai.api-key:deployment"])("preserves structured V1 model ID %s", async (model) => {
    const { fixture, thread, run } = await setup(false, model);
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: thread.latestTurn.turnId, lastError: "API Error: auth_unavailable: db.internal" };
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject({ model, provider: "bedrock", message: "API Error: auth_unavailable: [REDACTED HOST]" });
    expect((await fixture.gateway.threadGet(thread.id)).thread.failure?.model).toBe(model);
    expect((await makeGateway(fixture.config).journal.getFailureByTurnId(thread.id, thread.latestTurn.turnId))?.model).toBe(model);
  });

  it.each(["list", "overview"] as const)("does not rewrite unchanged failed and recovered rows on a %s poll", async (reader) => {
    const { fixture, fake, thread } = await setup(false);
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: thread.latestTurn.turnId, lastError: "Unchanged failure" };
    for (let index = 0; index < 12; index += 1) {
      fake.addThread({ id: `unchanged-failed-${index}`, projectId: thread.projectId,
        latestTurn: thread.latestTurn, session: thread.session });
      fake.addThread({ id: `unchanged-recovered-${index}`, projectId: thread.projectId,
        latestTurn: { ...thread.latestTurn!, turnId: `recovered-${index}`, state: "completed" }, session: { status: "ready" } });
    }
    const poll = () => reader === "list" ? fixture.gateway.threadsList({ includeArchived: false, limit: 50 })
      : fixture.gateway.threadsOverview({ includeArchived: false, runningLimit: 5 });
    await poll();
    const path = join(fixture.directory, "operations.json");
    const original = await readFile(path, "utf8");
    let writes = 0;
    const watcher = watch(fixture.directory, (event, filename) => {
      if (event === "rename" && filename === "operations.json") writes += 1;
    });
    try {
      await poll();
      await poll();
      await new Promise((resolve) => setTimeout(resolve, 20));
      expect(writes).toBe(0);
      expect(await readFile(path, "utf8")).toBe(original);
    } finally { watcher.close(); }
  });

  it.each([[1, 2], [2, 1]] as const)("selects the current shell after a delayed protocol %s completion and protocol %s failure", async (firstProtocol, middleProtocol) => {
    const { fixture, thread, run } = await setup(false);
    await fixture.gateway.runGet(run.runId);
    const oldCompletion = structuredClone(await fixture.client.getThread(thread.id));
    oldCompletion.snapshotSequence = 100;
    oldCompletion.thread.orchestrationProtocolVersion = firstProtocol;
    oldCompletion.thread.latestTurn = { ...oldCompletion.thread.latestTurn!, state: "completed" };
    oldCompletion.thread.session = { status: "ready", activeTurnId: null, lastError: null };
    const current = structuredClone(oldCompletion);
    current.snapshotSequence = 1;
    current.thread.orchestrationProtocolVersion = middleProtocol;
    current.thread.latestTurn = { ...current.thread.latestTurn!, state: "error" };
    current.thread.session = { status: "error", activeTurnId: current.thread.latestTurn.turnId,
      lastError: "Current failure", lastErrorClass: "provider_error" };
    const shell = structuredClone(await fixture.client.getShell());
    shell.snapshotSequence = 1;
    Object.assign(shell.threads[0]!, { orchestrationProtocolVersion: firstProtocol,
      latestTurn: current.thread.latestTurn, session: current.thread.session });
    let release!: () => void;
    let entered!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    const started = new Promise<void>((resolve) => { entered = resolve; });
    let calls = 0;
    fixture.client.getThread = async () => {
      if (++calls === 1) { entered(); await gate; return oldCompletion; }
      return current;
    };
    fixture.client.getShell = async () => shell;
    const delayed = fixture.gateway.threadGet(thread.id);
    await started;
    expect((await fixture.gateway.runGet(run.runId)).failure?.message).toBe("Current failure");
    release();
    expect((await delayed).thread).toMatchObject({ activity: "failed", latestTurn: { state: "error" },
      failure: { message: "Current failure" } });
    expect((await makeGateway(fixture.config).journal.getFailureByTurnId(thread.id, current.thread.latestTurn.turnId))?.message).toBe("Current failure");
  });

  it.each([{ type: "rate_limit_error", code: null }, { type: null, code: "rate_limit_error" },
    { type: "api_error", code: "rate_limit_error" }])("uses explicit JSON subtype with nullable companion field: %j", async (error) => {
    const { fixture, thread, run } = await setup(false);
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: thread.latestTurn.turnId, lastError: JSON.stringify({ error }) };
    const expected = { category: "rate_limit", code: "rate_limit_error" };
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject(expected);
    expect((await fixture.gateway.threadGet(thread.id)).thread.failure).toMatchObject(expected);
    expect((await makeGateway(fixture.config).journal.getFailureByTurnId(thread.id, thread.latestTurn.turnId))).toMatchObject(expected);
  });

  it("uses an explicit JSON subtype despite a generic V1 activity code", async () => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.activities.push({ kind: "runtime.error", turnId, payload: {
      class: "provider_error", code: "api_error", message: JSON.stringify({ error: { type: "rate_limit_error" } }),
    } });
    const expected = { category: "rate_limit", code: "rate_limit_error", source: "t3_activity" };
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject(expected);
    expect((await fixture.gateway.threadGet(thread.id)).thread.failure).toMatchObject(expected);
    expect((await makeGateway(fixture.config).journal.getFailureByTurnId(thread.id, turnId))).toMatchObject(expected);
  });

  it.each(["other-turn", null])("does not fail an unprojected historical run from session turn %s", async (activeTurnId) => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = null;
    thread.session = { status: "error", activeTurnId, lastError: "Unrelated session error" };
    expect(await fixture.gateway.runGet(run.runId)).toMatchObject({ t3TurnId: turnId, runStatus: "accepted", failure: null });
    expect((await makeGateway(fixture.config).gateway.runGet(run.runId)).failure).toBeNull();
    expect(await fixture.journal.getFailureByTurnId(thread.id, turnId)).toBeNull();
  });

  it("retains an explicitly bound activity without the turn projection despite another session error", async () => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = null;
    thread.session = { status: "error", activeTurnId: "other-turn", lastError: "Unrelated session error" };
    thread.activities.push({ kind: "runtime.error", turnId, payload: { code: "rate_limit_error", message: "Bound provider failure" } });
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject({
      turnId, category: "rate_limit", source: "t3_activity", message: "Bound provider failure",
    });
  });

  it("does not relabel a retained V1 reason with a colliding incoming raw identity", async () => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    const getThread = fixture.client.getThread.bind(fixture.client);
    let sequence = 100;
    fixture.client.getThread = async (id) => ({ ...await getThread(id), snapshotSequence: sequence++ });
    thread.session = { status: "error", activeTurnId: turnId, lastErrorClass: "provider_error",
      lastError: "Provider failed at https://one.example/private", failureCode: "first_error", resetAt: "2026-10-04T00:00:00Z" };
    expect((await fixture.gateway.runGet(run.runId)).failure?.code).toBe("first_error");
    thread.session.lastError = "Provider failed at https://two.example/private";
    thread.session.failureCode = null;
    thread.session.resetAt = null;
    await fixture.gateway.runGet(run.runId);
    thread.activities.push({ kind: "runtime.error", turnId,
      payload: { class: "provider_error", message: thread.session.lastError } });
    const expected = { source: "t3_activity", code: null, resetAt: null, message: "Provider failed at [REDACTED URL]" };
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject(expected);
    expect((await makeGateway(fixture.config).journal.getFailureByTurnId(thread.id, turnId))).toMatchObject(expected);
  });

  it.each(["summary", "full", "overview"] as const)("batches recovery persistence for a %s read", async (reader) => {
    const { fixture, fake, thread } = await setup(false);
    for (let index = 0; index < 24; index += 1) {
      fake.addThread({ id: `finished-${index}`, projectId: thread.projectId,
        latestTurn: { turnId: `finished-turn-${index}`, state: "completed", requestedAt: "2026-01-01T00:00:00.000Z",
          completedAt: "2026-01-01T00:01:00.000Z" } });
    }
    let writes = 0;
    const watcher = watch(fixture.directory, (event, filename) => {
      if (event === "rename" && filename === "operations.json") writes += 1;
    });
    try {
      if (reader === "overview") await fixture.gateway.threadsOverview({ includeArchived: false, runningLimit: 5 });
      else await fixture.gateway.threadsList({ includeArchived: false, detail: reader, limit: 50 });
      await new Promise((resolve) => setTimeout(resolve, 20));
      expect(writes).toBeGreaterThan(0);
      expect(writes).toBeLessThanOrEqual(2);
      const restarted = makeGateway(fixture.config);
      expect(await restarted.journal.getRecoveryByTurnId("finished-0", "finished-turn-0")).toBe("completed");
    } finally {
      watcher.close();
    }
  });

  it("replaces superseded V1 reason metadata without inheriting an ambiguous code", async () => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: turnId, lastError: "API Error: 429", failureCode: "api_error_429",
      resetAt: "2026-10-04T00:00:00.000Z", retryAfter: "300" };
    expect((await fixture.gateway.threadsList({ includeArchived: false, limit: 5 })).page.items[0]?.failure)
      .toMatchObject({ category: "unknown", code: "api_error_429" });
    thread.activities.push({ kind: "runtime.error", turnId,
      payload: { message: "Codex usage limit reached. Send the message again once the limit resets." } });
    const expected = { category: "quota", source: "t3_activity", code: null, resetAt: null, retryAfter: null, retry: null };
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject(expected);
    expect((await makeGateway(fixture.config).gateway.runGet(run.runId)).failure).toMatchObject(expected);
  });

  it("redacts bare DNS and IPv6 endpoints from failure, response, and journal", async () => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "ready" };
    thread.messages.push(assistantMessage("API Error: auth_unavailable: db.internal:5432 [fd00::1]:8080 fd00::2 [fe80::3%eth0]:8000 at 13:55:45", turnId));
    const result = await fixture.gateway.runGet(run.runId);
    for (const text of [JSON.stringify(result.failure), result.latestResponse!.text,
      await readFile(join(fixture.directory, "operations.json"), "utf8")]) {
      expect(text).not.toMatch(/db\.internal|fd00|fe80|eth0/);
      expect(text).toContain("13:55:45");
    }
  });

  it.each(["runtime.error", "provider.turn.start.failed"])("retains structured %s metadata without text", async (kind) => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "ready", activeTurnId: null, lastError: null };
    thread.activities.push({ kind, turnId, payload: { code: "rate_limit_error", retryable: true,
      retry: { attempt: 2, maxAttempts: 3, retryDelayMs: 500 } } });
    const expected = { source: "t3_activity", category: "rate_limit", code: "rate_limit_error",
      retryable: true, retry: { attempt: 2, maxAttempts: 3, retryDelayMs: 500 },
      message: "T3 reported that the provider turn failed without an error message." };
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject(expected);
    expect((await fixture.gateway.threadGet(thread.id)).thread.failure).toMatchObject(expected);
    expect((await fixture.gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0]?.failure).toMatchObject(expected);
  });

  it.each([
    ["rate limit", claudeRateLimit, "rate_limit", "rate_limit_error", "t3_message"],
    ["Codex usage limit", codexUsageLimit, "quota", null, "t3_activity"],
    ["credit refusal", creditsRequired, "auth_billing", null, "t3_message"],
  ] as const)("reads a real V1 %s payload through run, thread, and overview status", async (_label, payload, category, code, source) => {
    const { fixture, thread, run } = await setup();
    const turnId = thread.latestTurn!.turnId;
    const captured = ThreadSchema.parse({ ...thread, ...payload });
    thread.latestTurn = { ...captured.latestTurn!, turnId };
    thread.session = captured.session;
    thread.activities = payload.activities.map((value) => ({ ...value, turnId }));
    thread.messages.push(...captured.messages.map((message) => ({ ...message, turnId })));
    const expected = { category, code, source, turnId, class: null, retryable: null, resetAt: null, retryAfter: null };
    expect((await fixture.gateway.runWait(run.runId, 0.1)).failure).toMatchObject(expected);
    expect((await fixture.gateway.threadGet(thread.id)).thread.failure).toMatchObject(expected);
    expect((await fixture.gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0]?.failure).toMatchObject(expected);
  });

  it("reads a turn-bound auth_unavailable type and ignores an earlier turn's provider error", async () => {
    const { fixture, thread, run } = await setup();
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "ready", activeTurnId: null, lastError: null };
    thread.messages.push(MessageSchema.parse({ ...authUnavailable, turnId: "old-turn" }));
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject({ category: "unknown", source: "t3_turn" });
    thread.messages.push(MessageSchema.parse({ ...authUnavailable, turnId }));
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject({ category: "auth_billing", code: "auth_unavailable", source: "t3_message" });
  });

  it("rejects real unbound start errors and a session timestamp after the failed turn", async () => {
    const { fixture, thread, run } = await setup();
    const turnId = thread.latestTurn!.turnId;
    const captured = ThreadSchema.parse({ ...thread, ...unboundStartError });
    thread.latestTurn = { ...captured.latestTurn!, turnId };
    thread.session = captured.session;
    thread.activities = captured.activities;
    const observed = await fixture.gateway.runGet(run.runId);
    expect(observed.failure).toMatchObject({ category: "unknown", source: "t3_turn", turnId });
    expect(observed.failure?.message).not.toContain("ProviderUnsupportedError");
    expect(observed.latestResponse).toBeNull();
  });

  it.each(["exceeded retry limit, last status: 429 Too Many Requests", "API Error: Request rejected (429)"])("leaves a bare 429 unknown: %s", async (message) => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: turnId, lastError: message };
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject({ category: "unknown", code: null, resetAt: null });
  });

  it("accepts only exact known T3 usage-limit sentences", async () => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: turnId, lastError: "Maybe Claude usage limit reached. Send the message again once the limit resets." };
    expect((await fixture.gateway.runGet(run.runId)).failure?.category).toBe("unknown");
    thread.session.lastError = "Claude usage limit reached. Send the message again once the limit resets.";
    expect((await fixture.gateway.runGet(run.runId)).failure?.category).toBe("quota");
  });

  it("reports a V1 activity first seen through overview and retains it after session recovery", async () => {
    const { fixture, thread, run } = await setup();
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "stopped", activeTurnId: null, lastError: null };
    thread.activities = codexUsageLimit.activities.map((activity) => ({ ...activity, turnId }));
    const overview = await fixture.gateway.threadsOverview({ includeArchived: false, runningLimit: 5 });
    expect(overview.highlights[0]).toMatchObject({ failure: { category: "quota", source: "t3_activity" }, latestResponseExcerpt: null });
    thread.activities = [];
    expect((await makeGateway(fixture.config).gateway.runGet(run.runId)).failure).toMatchObject({ category: "quota", source: "t3_activity" });
  });

  it("keeps a later shell turn separate from a failed full snapshot", async () => {
    const { fixture, thread } = await setup();
    const oldTurn = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.activities = codexUsageLimit.activities.map((activity) => ({ ...activity, turnId: oldTurn }));
    thread.messages.push(assistantMessage("older failed output", oldTurn));
    const newer = structuredClone(await fixture.client.getShell());
    newer.threads[0]!.latestTurn = { turnId: "newer-turn", state: "running", requestedAt: new Date().toISOString() };
    newer.threads[0]!.session = { status: "running", activeTurnId: "newer-turn", lastError: null };
    fixture.client.getShell = async () => newer;
    const detail = (await fixture.gateway.threadGet(thread.id)).thread;
    expect(detail.latestTurn?.turnId).toBe("newer-turn");
    expect(detail.failure).toBeNull();
    expect(detail.latestResponse).toBeNull();
  });

  it("redacts provider-error responses, credential forms, and private paths before retention", async () => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "ready" };
    thread.messages.push(assistantMessage('API Error: auth_unavailable: Basic private-basic refresh_token=private-refresh /home/private/project /mnt/customer/project /opt/service/secret C:\\customer\\secret \\\\server\\share\\secret \"C:\\Program Files\\customer\\secret\" 10.2.3.4:8317 paths: [/mnt/customer/project/secret.ts] failed {/opt/service/secret} /mnt/customer/name,with,commas.ts OPENAI_API_KEY="vendor-private quoted-private" ANTHROPIC_API_KEY: vendor-private MY_ACCESS_TOKEN=vendor-private {"MY_ACCESS_TOKEN":"vendor-private","AWS_SECRET_ACCESS_KEY":"composite-private"} AWS_SECRET_ACCESS_KEY=composite-private', turnId));
    const observed = await fixture.gateway.runGet(run.runId);
    expect(JSON.stringify(observed.failure)).not.toMatch(/private-basic|private-refresh|vendor-private|quoted-private|composite-private|home\/private|customer|service\/secret|server|share|Program Files|10\.2\.3\.4/);
    expect(observed.latestResponse?.text).not.toMatch(/private-basic|private-refresh|vendor-private|quoted-private|composite-private|home\/private|customer|service\/secret|server|share|Program Files|10\.2\.3\.4/);
    expect(await readFile(join(fixture.directory, "operations.json"), "utf8")).not.toMatch(/private-basic|private-refresh|vendor-private|quoted-private|composite-private|home\/private|customer|service\/secret|server|share|Program Files|10\.2\.3\.4/);
  });

  it("prefers a specific V1 provider error over a generic activity class", async () => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.activities = creditsRequired.activities.map((activity) => ({ ...activity, turnId, payload: { ...activity.payload, class: "usage_limit" } }));
    thread.messages.push(MessageSchema.parse({ ...creditsRequired.messages[0]!, turnId }));
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject({ category: "auth_billing", source: "t3_message" });
  });

  it.each(["code", "type"] as const)("keeps a matching activity %s when the V1 credit refusal has no parsed subtype", async (field) => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    const message = creditsRequired.messages[0]!.text;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.activities.push({ kind: "runtime.error", turnId, payload: { message, [field]: "credits_required" } });
    thread.messages.push(assistantMessage(message, turnId));
    const expected = { source: "t3_message", category: "auth_billing", code: "credits_required", turnId };
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject(expected);
    expect((await fixture.gateway.threadGet(thread.id)).thread.failure).toMatchObject(expected);
    expect((await makeGateway(fixture.config).journal.getFailureByTurnId(thread.id, turnId))).toMatchObject(expected);
  });

  it.each(["message", "detail"] as const)("keeps matching activity %s metadata on a specific V1 provider message", async (field) => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    const message = claudeRateLimit.messages[0]!.text;
    const retry = { attempt: 2, maxAttempts: 3, retryDelayMs: 500 };
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.activities.push({ kind: "runtime.error", turnId, payload: {
      [field]: message, class: "usage_limit", code: "api_error", retryable: true, retry,
      resetAt: "2026-10-04T00:00:00Z", retryAfter: 120,
    } });
    thread.messages.push(assistantMessage(message, turnId));
    const expected = { source: "t3_message", category: "rate_limit", code: "rate_limit_error", class: "usage_limit",
      retryable: true, retry, resetAt: "2026-10-04T00:00:00.000Z", retryAfter: "120" };
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject(expected);
    expect((await fixture.gateway.threadGet(thread.id)).thread.failure).toMatchObject(expected);
    expect((await makeGateway(fixture.config).journal.getFailureByTurnId(thread.id, turnId))).toMatchObject(expected);
  });

  it.each(["raw_reason", "turn"] as const)("does not mix V1 activity metadata when the %s differs", async (difference) => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    const message = "API Error: rate_limit_error: Failed at https://one.example/private";
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.activities.push({ kind: "runtime.error", turnId: difference === "turn" ? "other-turn" : turnId,
      payload: { message: difference === "raw_reason" ? message.replace("one.example", "two.example") : message,
        class: "usage_limit", retryable: true, retry: { attempt: 2, maxAttempts: 3, retryDelayMs: 500 }, resetAt: "2026-10-04T00:00:00Z" } });
    thread.messages.push(assistantMessage(message, turnId));
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject({ source: "t3_message", category: "rate_limit",
      code: "rate_limit_error", class: null, retryable: null, retry: null, resetAt: null });
  });

  it("uses admitted V1 run settings after the thread model changes, including on reconnect", async () => {
    const { fixture, fake, thread, run } = await setup(false);
    const admittedModel = thread.modelSelection.model;
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: turnId, lastError: "Provider stopped" };
    thread.modelSelection = { instanceId: "different-provider", model: "different-model" };
    // A thread-only read cannot know the admitted V1 model before run binding.
    await fixture.gateway.threadGet(thread.id);
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject({ model: admittedModel, provider: "codex_openai" });
    await fake.close();
    expect((await makeGateway(fixture.config).gateway.runGet(run.runId)).failure).toMatchObject({ model: admittedModel, provider: "codex_openai" });
  });

  it.each([
    ["list", "running"], ["list", "completed"], ["list", "interrupted"], ["list", "error"],
    ["overview", "running"], ["overview", "completed"], ["overview", "interrupted"], ["overview", "error"],
  ] as const)("keeps a V1 shell failure when %s enrichment sees a %s successor", async (reader, state) => {
    const { fixture, fake, thread, run } = await setup(false);
    await fixture.gateway.runGet(run.runId);
    const oldTurn = thread.latestTurn!;
    thread.latestTurn = { ...oldTurn, state: "error" };
    thread.session = { status: "error", activeTurnId: oldTurn.turnId, lastError: "Older failure" };
    const oldShell = structuredClone(await fixture.client.getShell());
    thread.latestTurn = { turnId: "new-turn", state, requestedAt: new Date(Date.parse(oldTurn.requestedAt) + 1000).toISOString() };
    thread.session = { status: state === "error" ? "error" : "ready", activeTurnId: "new-turn",
      lastError: state === "error" ? "Successor failure" : null };
    thread.messages.push(assistantMessage("Successor response", "new-turn"));
    fixture.client.getShell = async () => oldShell;
    const failure = { message: "Older failure", turnId: oldTurn.turnId, source: "t3_session" };
    if (reader === "list") {
      const list = await fixture.gateway.threadsList({ includeArchived: false, detail: "full", activity: "failed", needsAttention: true, limit: 5 });
      expect(list.page.items[0]).toMatchObject({ latestTurn: { turnId: oldTurn.turnId }, activity: "failed", failure, latestResponseExcerpt: null });
    } else {
      const overview = await fixture.gateway.threadsOverview({ includeArchived: false, runningLimit: 5 });
      expect(overview.highlights[0]).toMatchObject({ latestTurn: { turnId: oldTurn.turnId }, activity: "failed", failure, latestResponseExcerpt: null });
      expect(overview.executionCounts.failed).toBe(1);
      expect(overview.runningCount).toBe(0);
    }
    const detail = (await fixture.gateway.threadGet(thread.id)).thread;
    expect(detail.latestTurn?.turnId).toBe("new-turn");
    if (state === "error") expect(detail.failure).toMatchObject({ message: "Successor failure", turnId: "new-turn" });
    else expect(detail.failure).toBeNull();
    await fake.close();
    expect((await makeGateway(fixture.config).gateway.runGet(run.runId)).failure).toMatchObject(failure);
  });

  it.each(["completed", "interrupted"] as const)("uses a newer V1 shell error over stale full %s", async (state) => {
    const { fixture, thread } = await setup(false);
    thread.latestTurn = { ...thread.latestTurn!, state };
    thread.session = { status: "ready", lastError: null };
    const full = structuredClone(await fixture.client.getThread(thread.id));
    const shell = structuredClone(await fixture.client.getShell());
    shell.snapshotSequence = full.snapshotSequence + 1;
    const row = shell.threads[0]!;
    row.latestTurn = { ...thread.latestTurn, state: "error" };
    row.session = { status: "error", activeTurnId: row.latestTurn.turnId, lastError: "Current V1 shell error" };
    fixture.client.getThread = async () => full;
    fixture.client.getShell = async () => shell;
    expect((await fixture.gateway.threadGet(thread.id)).thread).toMatchObject({ latestTurn: { state: "error" }, activity: "failed",
      failure: { turnId: row.latestTurn.turnId, source: "t3_session", message: "Current V1 shell error" } });
  });

  it.each([
    [false, "changed"], [true, "changed"],
    [false, "matching"], [true, "matching"],
    [false, "matching reset"], [true, "matching reset"],
    [false, "redaction collision"], [true, "redaction collision"],
    [false, "known to unknown"], [true, "known to unknown"],
  ] as const)("orders newer V1 shell session evidence (retained=%s, reason=%s)", async (retained, reason) => {
    const { fixture, fake, thread, run } = await setup(false);
    await fixture.gateway.runGet(run.runId);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    const oldMessage = reason === "redaction collision" ? "Failure at https://one.example/private" : "Old session failure";
    const matching = reason === "matching" || reason === "matching reset";
    const newMessage = matching ? oldMessage
      : reason === "redaction collision" ? "Failure at https://two.example/private" : "New session failure";
    thread.session = { status: "error", activeTurnId: turnId, lastError: oldMessage,
      failureCode: reason === "known to unknown" ? "usage_limit_error" : "old_code",
      ...(reason === "known to unknown" ? { failureCategory: "quota" } : {}),
      resetAt: "2026-10-04T00:00:00Z", retryAfter: "300" };
    const full = structuredClone(await fixture.client.getThread(thread.id));
    full.snapshotSequence = 100;
    const shell = structuredClone(await fixture.client.getShell());
    shell.snapshotSequence = 101;
    shell.threads[0]!.session = { status: "error", activeTurnId: turnId, lastError: newMessage,
      ...(reason === "matching reset" ? { resetAt: "2026-10-05T00:00:00Z", retryAfter: "600" } : {}) };
    fixture.client.getThread = async () => full;
    if (retained) expect((await fixture.gateway.runGet(run.runId)).failure?.code).toBe(thread.session.failureCode);
    fixture.client.getShell = async () => shell;
    const expected = { source: "t3_session", turnId, category: "unknown",
      message: reason === "redaction collision" ? "Failure at [REDACTED URL]" : newMessage,
      code: matching ? "old_code" : null,
      resetAt: reason === "matching reset" ? "2026-10-05T00:00:00.000Z" : matching ? "2026-10-04T00:00:00.000Z" : null,
      retryAfter: reason === "matching reset" ? "600" : matching ? "300" : null };
    expect((await fixture.gateway.threadGet(thread.id)).thread.failure).toMatchObject(expected);
    const restarted = makeGateway(fixture.config);
    expect(await restarted.journal.getFailureByTurnId(thread.id, turnId)).toMatchObject(expected);
    expect(await restarted.journal.getFailureEvidenceOrder(thread.id, turnId)).toMatchObject({ snapshotSequence: 101 });
    await fake.close();
    expect((await restarted.gateway.runGet(run.runId)).failure).toMatchObject(expected);
  });

  it.each(["t3_activity", "t3_message"] as const)("keeps richer V1 full %s evidence over a newer shell session", async (source) => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: turnId, lastError: "Old session failure" };
    const message = source === "t3_message" ? "API Error: auth_unavailable: No credentials" : "Older activity refusal";
    if (source === "t3_message") thread.messages.push(assistantMessage(message, turnId));
    else thread.activities.push({ kind: "runtime.error", turnId, payload: { message } });
    const full = structuredClone(await fixture.client.getThread(thread.id));
    full.snapshotSequence = 100;
    const shell = structuredClone(await fixture.client.getShell());
    shell.snapshotSequence = 101;
    shell.threads[0]!.session = { status: "error", activeTurnId: turnId, lastError: "New shell session failure" };
    fixture.client.getThread = async () => full;
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject({ source, message });
    fixture.client.getShell = async () => shell;
    expect((await fixture.gateway.threadGet(thread.id)).thread.failure).toMatchObject({ source, message });
    expect(await makeGateway(fixture.config).journal.getFailureByTurnId(thread.id, turnId)).toMatchObject({ source, message });
  });

  it("replaces ordered V1 full session reasons without retaining obsolete metadata", async () => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: turnId, lastError: "Old full session failure",
      failureCode: "old_code", resetAt: "2026-10-04T00:00:00Z" };
    const getThread = fixture.client.getThread.bind(fixture.client);
    let sequence = 100;
    fixture.client.getThread = async (id) => ({ ...await getThread(id), snapshotSequence: sequence++ });
    expect((await fixture.gateway.runGet(run.runId)).failure?.code).toBe("old_code");
    thread.session = { status: "error", activeTurnId: turnId, lastError: "New full session failure" };
    const expected = { source: "t3_session", message: "New full session failure", code: null, resetAt: null };
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject(expected);
    expect(await makeGateway(fixture.config).journal.getFailureByTurnId(thread.id, turnId)).toMatchObject(expected);
  });

  it.each([99, 100])("keeps V1 full session evidence when shell sequence %s is not newer", async (sequence) => {
    const { fixture, thread } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: turnId, lastError: "Current full session failure" };
    const full = structuredClone(await fixture.client.getThread(thread.id));
    full.snapshotSequence = 100;
    const shell = structuredClone(await fixture.client.getShell());
    shell.snapshotSequence = sequence;
    shell.threads[0]!.session = { status: "error", activeTurnId: turnId, lastError: "Stale shell session failure" };
    fixture.client.getThread = async () => full;
    fixture.client.getShell = async () => shell;
    expect((await fixture.gateway.threadGet(thread.id)).thread.failure).toMatchObject({ message: "Current full session failure" });
    expect(await makeGateway(fixture.config).journal.getFailureByTurnId(thread.id, turnId)).toMatchObject({ message: "Current full session failure" });
  });

  it.each([false, true])("keeps bound V1 full session evidence over a newer generic shell (retained=%s)", async (retained) => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: turnId, lastError: "Bound full session failure" };
    const full = structuredClone(await fixture.client.getThread(thread.id));
    full.snapshotSequence = 100;
    const shell = structuredClone(await fixture.client.getShell());
    shell.snapshotSequence = 101;
    shell.threads[0]!.session = { status: "ready", activeTurnId: null, lastError: null };
    fixture.client.getThread = async () => full;
    if (retained) await fixture.gateway.runGet(run.runId);
    fixture.client.getShell = async () => shell;
    const expected = { source: "t3_session", message: "Bound full session failure" };
    expect((await fixture.gateway.threadGet(thread.id)).thread.failure).toMatchObject(expected);
    expect(await makeGateway(fixture.config).journal.getFailureByTurnId(thread.id, turnId)).toMatchObject(expected);
  });

  it.each(["vendor.example", "/namespace/error", "vendor:api_error", "vendor.token:0"])("preserves structured failure namespace %s and redacts message text", async (namespace) => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.activities.push({ kind: "runtime.error", turnId, payload: { class: namespace, code: namespace,
      message: "Failed at https://private.example/path access_token=private-secret" } });
    const expected = { class: namespace, code: namespace, category: "unknown", turnId };
    const observed = await fixture.gateway.runGet(run.runId);
    expect(observed.failure).toMatchObject(expected);
    expect(observed.failure?.message).not.toMatch(/private.example|private-secret/);
    expect((await fixture.gateway.threadGet(thread.id)).thread.failure).toMatchObject(expected);
    expect(await makeGateway(fixture.config).journal.getFailureByTurnId(thread.id, turnId)).toMatchObject(expected);
  });

  it.each(["api_key=private-code", "api_key:private-code"])("redacts structured credential assignments %s", async (credential) => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.activities.push({ kind: "runtime.error", turnId, payload: { class: credential, code: credential, message: "Provider failed" } });
    expect(JSON.stringify((await fixture.gateway.runGet(run.runId)).failure)).not.toContain("private-code");
    expect(JSON.stringify(await makeGateway(fixture.config).journal.getFailureByTurnId(thread.id, turnId))).not.toContain("private-code");
  });

  it("keeps overview recovery order when full enrichment fails for a V1 session-only error", async () => {
    const { fixture, thread, run } = await setup(false);
    await fixture.gateway.runGet(run.runId);
    const turn = thread.latestTurn!;
    thread.latestTurn = null;
    thread.session = { status: "error", activeTurnId: turn.turnId, lastError: "Old session refusal" };
    const older = structuredClone(await fixture.client.getThread(thread.id));
    fixture.client.getThread = async () => { throw new Error("Full read unavailable"); };
    expect((await fixture.gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0]?.failure)
      .toMatchObject({ source: "t3_session", message: "Old session refusal" });
    thread.latestTurn = { ...turn, state: "completed", completedAt: new Date().toISOString() };
    thread.session = { status: "ready", activeTurnId: null, lastError: null };
    const recoveredShell = structuredClone(await fixture.client.getShell());
    recoveredShell.snapshotSequence = older.snapshotSequence + 1;
    fixture.client.getShell = async () => recoveredShell;
    expect((await fixture.gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0]?.failure).toBeNull();
    const restarted = makeGateway(fixture.config);
    restarted.client.getThread = async () => older;
    expect(await restarted.gateway.runGet(run.runId)).toMatchObject({ runStatus: "completed", failure: null });
  });

  it("clears an earlier same-turn shell failure after the full snapshot has recovered", async () => {
    const { fixture, thread, run, fake } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: turnId, lastError: "Older failure" };
    const failedShell = structuredClone(await fixture.client.getShell());
    await fixture.gateway.threadsList({ includeArchived: false, detail: "summary", limit: 5 });
    thread.latestTurn = { ...thread.latestTurn!, state: "completed", completedAt: new Date().toISOString() };
    thread.session = { status: "ready", activeTurnId: null, lastError: null };
    thread.updatedAt = new Date().toISOString();
    fixture.client.getShell = async () => failedShell;
    expect((await fixture.gateway.threadsList({ includeArchived: false, detail: "full", limit: 5 })).page.items[0]).toMatchObject({ latestTurn: { state: "error" }, failure: null });
    expect((await fixture.gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0]).toMatchObject({ latestTurn: { state: "error" }, failure: null });
    expect((await fixture.gateway.threadGet(thread.id)).thread).toMatchObject({ latestTurn: { state: "completed" }, failure: null });
    expect(await fixture.gateway.runGet(run.runId)).toMatchObject({ runStatus: "completed", failure: null });
    expect(await makeGateway(fixture.config).gateway.runGet(run.runId)).toMatchObject({ runStatus: "completed", failure: null });
    await fake.close();
    expect(await makeGateway(fixture.config).gateway.runGet(run.runId)).toMatchObject({ runStatus: "unknown", failure: null });
  });

  it("preserves omitted shell metadata when a full read advances the turn", async () => {
    const { fixture, thread } = await setup(false);
    const latestUserMessageAt = new Date().toISOString();
    thread.latestUserMessageAt = latestUserMessageAt;
    thread.hasPendingApprovals = true;
    thread.hasPendingUserInput = true;
    const shell = structuredClone(await fixture.client.getShell());
    thread.latestTurn = { turnId: "new-turn", state: "running", requestedAt: new Date(Date.now() + 1000).toISOString() };
    const full = structuredClone(await fixture.client.getThread(thread.id));
    delete full.thread.latestUserMessageAt;
    delete full.thread.hasPendingApprovals;
    delete full.thread.hasPendingUserInput;
    fixture.client.getShell = async () => shell;
    fixture.client.getThread = async () => full;
    const expected = { latestTurn: { turnId: "new-turn" }, latestUserMessageAt, hasPendingApprovals: true, hasPendingUserInput: true };
    const shellExpected = { ...expected, latestTurn: { turnId: shell.threads[0]!.latestTurn!.turnId } };
    expect((await fixture.gateway.threadsList({ includeArchived: false, detail: "full", limit: 5 })).page.items[0]).toMatchObject(shellExpected);
    expect((await fixture.gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0]).toMatchObject(shellExpected);
    expect((await fixture.gateway.threadGet(thread.id)).thread).toMatchObject(expected);
  });

  it.each(["completed", "interrupted"] as const)("clears a full failure when the later shell is %s", async (state) => {
    const { fixture, thread, run, fake } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: turnId, lastError: "Earlier failure" };
    const failed = structuredClone(await fixture.client.getThread(thread.id));
    await fixture.gateway.runGet(run.runId);
    thread.latestTurn = { ...thread.latestTurn!, state, completedAt: new Date().toISOString() };
    thread.session = { status: "ready", activeTurnId: null, lastError: null };
    thread.updatedAt = new Date(Date.now() + 1000).toISOString();
    fixture.client.getThread = async () => failed;
    expect((await fixture.gateway.threadGet(thread.id)).thread).toMatchObject({ latestTurn: { state }, failure: null });
    await fake.close();
    expect(await makeGateway(fixture.config).gateway.runGet(run.runId)).toMatchObject({ runStatus: "unknown", failure: null });
  });

  it("clears a retained run failure when a direct full read proves completion", async () => {
    const { fixture, thread, run, fake } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: turnId, lastError: "Earlier run failure" };
    await fixture.gateway.runGet(run.runId);
    thread.latestTurn = { ...thread.latestTurn!, state: "completed", completedAt: new Date().toISOString() };
    thread.session = { status: "ready", activeTurnId: null, lastError: null };
    expect(await fixture.gateway.runGet(run.runId)).toMatchObject({ runStatus: "completed", failure: null });
    await fake.close();
    expect(await makeGateway(fixture.config).gateway.runGet(run.runId)).toMatchObject({ runStatus: "unknown", failure: null });
  });

  it("does not bind an unbounded session timestamp to an old failed turn", async () => {
    const { fixture, thread, run } = await setup(false);
    thread.latestTurn = { ...thread.latestTurn!, state: "error", completedAt: null };
    thread.session = { status: "error", activeTurnId: null, lastError: "Later unbound start failure", updatedAt: new Date(Date.now() + 1000).toISOString() };
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject({ source: "t3_turn", category: "unknown" });
    expect((await fixture.gateway.threadGet(thread.id)).thread.failure?.message).not.toContain("Later unbound");
  });

  it("keeps an explicit provider code when a session category is unknown", async () => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: turnId, lastError: "Provider refused", failureCategory: "unknown", failureCode: "rate_limit_error" };
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject({ category: "rate_limit", code: "rate_limit_error" });
    expect((await fixture.gateway.threadGet(thread.id)).thread.failure?.category).toBe("rate_limit");
    expect((await fixture.gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0]?.failure?.category).toBe("rate_limit");
  });

  it("upgrades an unknown assistant refusal with later explicit activity evidence", async () => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "ready" };
    thread.messages.push(assistantMessage("API Error: 429", turnId));
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject({ category: "unknown", source: "t3_message" });
    thread.activities.push({ kind: "runtime.error", turnId, payload: { message: "Codex usage limit reached. Send the message again once the limit resets." } });
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject({ category: "quota", source: "t3_activity" });
    thread.activities = [];
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject({ category: "quota", source: "t3_activity" });
    expect((await makeGateway(fixture.config).gateway.runGet(run.runId)).failure).toMatchObject({ category: "quota", source: "t3_activity" });
  });

  it("keeps a newer same-turn shell failure when the earlier full read was still running", async () => {
    const { fixture, thread } = await setup(false);
    const running = structuredClone(await fixture.client.getThread(thread.id));
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: turnId, lastError: "Codex usage limit reached. Send the message again once the limit resets." };
    fixture.client.getThread = async () => running;
    expect((await fixture.gateway.threadGet(thread.id)).thread).toMatchObject({ latestTurn: { state: "error" }, activity: "failed", failure: { category: "quota", source: "t3_session" } });
  });

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
        provider: "codex_openai",
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

  it("classifies the exact credit refusal without inferring a reset time", async () => {
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
    expect(observed.failure).toMatchObject({ category: "auth_billing", code: null, resetAt: null, retryAfter: null });
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
    expect((await restarted.threadGet(thread.id)).thread.failure?.message).toBe("First observed in threadGet");
  });

  it.each(["activity", "message"] as const)("replaces a retained V1 %s with later evidence from the same source", async (source) => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: turnId, lastError: null };
    if (source === "activity") thread.activities.push({ kind: "runtime.error", turnId,
      payload: { message: "Codex usage limit reached. Send the message again once the limit resets." } });
    else thread.messages.push(assistantMessage("API Error: rate_limit_error: first refusal", turnId));
    expect((await fixture.gateway.runGet(run.runId)).failure?.category).toBe(source === "activity" ? "quota" : "rate_limit");
    const older = structuredClone(await fixture.client.getThread(thread.id));
    if (source === "activity") thread.activities.push({ kind: "runtime.error", turnId,
      payload: { message: "Credentials unavailable", type: "auth_unavailable" } });
    else thread.messages.push(assistantMessage("API Error: auth_unavailable: credentials unavailable", turnId));
    const newer = structuredClone(await fixture.client.getThread(thread.id));
    newer.snapshotSequence = older.snapshotSequence + 1;
    fixture.client.getThread = async () => newer;
    const expected = { category: "auth_billing", code: "auth_unavailable", resetAt: null };
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject(expected);
    expect((await fixture.gateway.threadGet(thread.id)).thread.failure).toMatchObject(expected);
    expect((await fixture.gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0]?.failure).toMatchObject(expected);
    const restarted = makeGateway(fixture.config);
    restarted.client.getThread = async () => older;
    expect((await restarted.gateway.runGet(run.runId)).failure).toMatchObject(expected);
  });

  it("keeps retained failure metadata when a later session observation is less specific", async () => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = {
      status: "error", activeTurnId: turnId, lastError: "Usage exhausted",
      failureCategory: "quota", failureCode: "usage_limit", resetAt: "2026-09-30T00:00:00Z",
    };
    await fixture.gateway.runGet(run.runId);
    thread.session = { status: "error", activeTurnId: turnId, lastError: "Later generic error" };
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject({
      category: "quota", code: "usage_limit", message: "Usage exhausted",
      resetAt: "2026-09-30T00:00:00.000Z",
    });
  });

  it("binds a session-only failure to its active turn", async () => {
    const { fixture, thread, run } = await setup();
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = null;
    thread.session = { status: "error", activeTurnId: turnId, lastError: "Session failed before turn projection" };
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject({
      turnId, source: "t3_session", message: "Session failed before turn projection",
    });
    expect((await fixture.gateway.threadGet(thread.id)).thread.failure?.turnId).toBe(turnId);
    thread.session = { status: "ready", activeTurnId: null, lastError: null };
    const restarted = makeGateway(fixture.config).gateway;
    const detail = (await restarted.threadGet(thread.id)).thread;
    expect(detail.failure?.message).toBe("Session failed before turn projection");
    expect(detail.latestResponse).toBeNull();
    expect((await restarted.runGet(run.runId)).failure?.turnId).toBe(turnId);
  });

  it("returns a failure retained by threadGet after T3 disconnects", async () => {
    const { fake, fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    await fixture.gateway.runGet(run.runId);
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: turnId, lastError: "Thread-only observation" };
    await fixture.gateway.threadGet(thread.id);
    await fake.close();
    expect((await makeGateway(fixture.config).gateway.runGet(run.runId))).toMatchObject({
      runStatus: "failed", connectionStatus: "disconnected",
      failure: { message: "Thread-only observation", turnId },
    });
  });

  it("returns a failure persisted before a descriptor request fails", async () => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.session = { status: "error", activeTurnId: turnId, lastError: "Provider refused" };
    fixture.client.getDescriptor = async () => { throw new Error("descriptor unavailable"); };
    expect(await fixture.gateway.runGet(run.runId)).toMatchObject({
      runStatus: "failed", connectionStatus: "disconnected",
      failure: { message: "Provider refused", turnId },
    });
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
    expect(matched?.provider).toBe("codex_openai");

    thread.session.lastError = '{"api_key":"private-value","password":"private-password","secret":"escaped\\\"value"}';
    const quoted = (await fixture.gateway.runGet(run.runId)).failure;
    expect(JSON.stringify(quoted)).not.toMatch(/private-value|private-password|escaped/);
    expect(await readFile(join(fixture.directory, "operations.json"), "utf8"))
      .not.toMatch(/private-value|private-password|escaped/);
  });

  it("uses a session error without an active turn only when its timestamp belongs to the failed turn", async () => {
    const { fixture, thread, run } = await setup();
    thread.latestTurn = { ...thread.latestTurn!, state: "error", completedAt: new Date(Date.parse(thread.latestTurn!.requestedAt) + 2000).toISOString() };
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
