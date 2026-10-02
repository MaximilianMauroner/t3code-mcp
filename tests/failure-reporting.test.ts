import { afterEach, describe, expect, it } from "vitest";
import { readFile } from "node:fs/promises";
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
    thread.messages.push(assistantMessage('API Error: auth_unavailable: Basic private-basic refresh_token=private-refresh /home/private/project /mnt/customer/project /opt/service/secret C:\\customer\\secret \\\\server\\share\\secret \"C:\\Program Files\\customer\\secret\" 10.2.3.4:8317 paths: [/mnt/customer/project/secret.ts] failed {/opt/service/secret} /mnt/customer/name,with,commas.ts OPENAI_API_KEY=vendor-private ANTHROPIC_API_KEY: vendor-private MY_ACCESS_TOKEN=vendor-private {"MY_ACCESS_TOKEN":"vendor-private"}', turnId));
    const observed = await fixture.gateway.runGet(run.runId);
    expect(JSON.stringify(observed.failure)).not.toMatch(/private-basic|private-refresh|vendor-private|home\/private|customer|service\/secret|server|share|Program Files|10\.2\.3\.4/);
    expect(observed.latestResponse?.text).not.toMatch(/private-basic|private-refresh|vendor-private|home\/private|customer|service\/secret|server|share|Program Files|10\.2\.3\.4/);
    expect(await readFile(join(fixture.directory, "operations.json"), "utf8")).not.toMatch(/private-basic|private-refresh|vendor-private|home\/private|customer|service\/secret|server|share|Program Files|10\.2\.3\.4/);
  });

  it("prefers a specific V1 provider error over a generic activity class", async () => {
    const { fixture, thread, run } = await setup(false);
    const turnId = thread.latestTurn!.turnId;
    thread.latestTurn = { ...thread.latestTurn!, state: "error" };
    thread.activities = creditsRequired.activities.map((activity) => ({ ...activity, turnId, payload: { ...activity.payload, class: "usage_limit" } }));
    thread.messages.push(MessageSchema.parse({ ...creditsRequired.messages[0]!, turnId }));
    expect((await fixture.gateway.runGet(run.runId)).failure).toMatchObject({ category: "auth_billing", source: "t3_message" });
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

  it("updates full list and overview rows when a newer turn has replaced the shell failure", async () => {
    const { fixture, thread } = await setup(false);
    const oldTurn = thread.latestTurn!;
    thread.latestTurn = { ...oldTurn, state: "error" };
    thread.session = { status: "error", activeTurnId: oldTurn.turnId, lastError: "Older failure" };
    const oldShell = structuredClone(await fixture.client.getShell());
    thread.latestTurn = { turnId: "new-turn", state: "running", requestedAt: new Date(Date.parse(oldTurn.requestedAt) + 1000).toISOString() };
    thread.session = { status: "running", activeTurnId: "new-turn", lastError: null };
    fixture.client.getShell = async () => oldShell;
    const list = await fixture.gateway.threadsList({ includeArchived: false, detail: "full", limit: 5 });
    expect(list.page.items[0]).toMatchObject({ latestTurn: { turnId: "new-turn" }, activity: "running", failure: null });
    const overview = await fixture.gateway.threadsOverview({ includeArchived: false, runningLimit: 5 });
    expect(overview.highlights[0]).toMatchObject({ latestTurn: { turnId: "new-turn" }, activity: "running", failure: null });
    expect((await fixture.gateway.threadGet(thread.id)).thread).toMatchObject({ latestTurn: { turnId: "new-turn" }, failure: null });
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
    expect((await fixture.gateway.threadsList({ includeArchived: false, detail: "full", limit: 5 })).page.items[0]).toMatchObject({ latestTurn: { state: "completed" }, failure: null });
    expect((await fixture.gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0]).toMatchObject({ latestTurn: { state: "completed" }, failure: null });
    expect((await fixture.gateway.threadGet(thread.id)).thread).toMatchObject({ latestTurn: { state: "completed" }, failure: null });
    expect(await fixture.gateway.runGet(run.runId)).toMatchObject({ runStatus: "completed", failure: null });
    expect(await makeGateway(fixture.config).gateway.runGet(run.runId)).toMatchObject({ runStatus: "completed", failure: null });
    await fake.close();
    expect(await makeGateway(fixture.config).gateway.runGet(run.runId)).toMatchObject({ runStatus: "unknown", failure: null });
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
