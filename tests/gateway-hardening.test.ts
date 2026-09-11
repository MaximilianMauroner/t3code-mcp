import { afterEach, describe, expect, it } from "vitest";
import { readFile } from "node:fs/promises";
import { GatewayError } from "../src/gateway.js";
import { gatewayFixture, type GatewayFixture } from "./support/gateway-fixture.js";
import { assistantMessage, FakeT3 } from "./support/fake-t3.js";

const fakes: FakeT3[] = [];
const fixtures: GatewayFixture[] = [];

afterEach(async () => {
  await Promise.all(fixtures.splice(0).map((fixture) => fixture.cleanup()));
  await Promise.all(fakes.splice(0).map((fake) => fake.close()));
});

async function setup(options: ConstructorParameters<typeof FakeT3>[0] = {}): Promise<{
  readonly fake: FakeT3;
  readonly fixture: GatewayFixture;
}> {
  const fake = new FakeT3(options);
  fakes.push(fake);
  await fake.start();
  const fixture = await gatewayFixture(fake);
  fixtures.push(fixture);
  return { fake, fixture };
}

describe("gateway read model and connection hardening", () => {
  it("reports environment identity, capabilities, scopes, and operation boundary", async () => {
    const { fake, fixture } = await setup({
      environmentId: "env-remote",
      environmentLabel: "coding-remote",
      serverVersion: "0.0.41-test",
      descriptorCapabilities: { b: true, a: true },
      scopes: ["orchestration:operate", "orchestration:read", "irrelevant:scope"],
    });
    const status = await fixture.gateway.connectionStatus();

    expect(status).toMatchObject({
      environment: { environmentId: "env-remote", label: "coding-remote", serverVersion: "0.0.41-test" },
      connectionStatus: "connected",
      stateFreshness: "fresh",
      t3Scopes: ["orchestration:operate", "orchestration:read", "irrelevant:scope"],
      supportedCapabilities: ["a", "b"],
    });
    expect(status.permittedOperations).toEqual(["orchestration:read", "orchestration:operate"]);
    expect(status.gatewayOperations).toContain("t3_thread_send");
    expect((await fixture.gateway.projectsList({ limit: 10 })).environmentId).toBe(fake.environmentId);
  });

  it("does not silently operate on a different configured environment", async () => {
    const { fixture } = await setup({ environmentId: "actual-environment" });
    const mismatched = await gatewayFixture((fakes.at(-1) as FakeT3), { environmentId: "expected-environment" });
    fixtures.push(mismatched);

    const status = await mismatched.gateway.connectionStatus();

    expect(status.connectionStatus).toBe("disconnected");
    expect(status.environment?.environmentId).toBe("actual-environment");
    expect(status.error).toContain("does not match");
    await expect(mismatched.gateway.projectsList({ limit: 10 })).rejects.toMatchObject<Partial<GatewayError>>({
      code: "environment_mismatch",
    });
    expect(fixture.config.environmentId).toBeNull();
  });

  it("reports stale cached state after the remote becomes unreachable", async () => {
    const { fake } = await setup();
    const fixture = await gatewayFixture(fake, { staleAfterMs: 1 });
    fixtures.push(fixture);
    await fixture.gateway.connectionStatus();
    await new Promise<void>((resolve) => setTimeout(resolve, 10));
    await fake.close();

    const status = await fixture.gateway.connectionStatus();

    expect(status.connectionStatus).toBe("disconnected");
    expect(status.stateFreshness).toBe("stale");
    expect(status.environment?.environmentId).toBe(fake.environmentId);
    expect(status.lastObservedAt).not.toBeNull();
  });

  it("paginates projects and threads while filtering archived threads", async () => {
    const { fake, fixture } = await setup();
    const project = fake.addProject({ id: "project-a", title: "A" });
    fake.addProject({ id: "project-b", title: "B" });
    fake.addProject({ id: "project-c", title: "C" });
    fake.addThread({ id: "thread-a1", projectId: project.id });
    fake.addThread({ id: "thread-a2", projectId: project.id });
    fake.addThread({ id: "thread-archived", projectId: project.id, archivedAt: "2026-01-02T00:00:00.000Z" });

    const projectsFirst = await fixture.gateway.projectsList({ limit: 2 });
    const projectsSecond = await fixture.gateway.projectsList({ limit: 2, cursor: projectsFirst.page.nextCursor ?? undefined });
    const threads = await fixture.gateway.threadsList({ projectId: project.id, includeArchived: false, limit: 10 });
    const archived = await fixture.gateway.threadsList({ projectId: project.id, includeArchived: true, limit: 10 });

    expect(projectsFirst.page.items.map((item) => item.id)).toEqual(["project-a", "project-b"]);
    expect(projectsFirst.page.hasMore).toBe(true);
    expect(projectsSecond.page.items.map((item) => item.id)).toEqual(["project-c"]);
    expect(threads.page.items.map((item) => item.id)).toEqual(["thread-a1", "thread-a2"]);
    expect(archived.page.items).toHaveLength(3);
  });

  it("bounds message text and exposes truncation instead of hiding it", async () => {
    const { fake, fixture } = await setup();
    const project = fake.addProject({ id: "project-messages" });
    const messages = Array.from({ length: 4 }, (_, index) => ({
      id: `message-${index + 1}`,
      role: index % 2 === 0 ? ("user" as const) : ("assistant" as const),
      text: index === 1 ? "0123456789" : `message-${index + 1}`,
      turnId: null,
      streaming: false,
      createdAt: "2026-01-01T00:00:00.000Z",
      updatedAt: "2026-01-01T00:00:00.000Z",
    }));
    fake.addThread({ id: "thread-messages", projectId: project.id, messages });

    const first = await fixture.gateway.threadMessages("thread-messages", { limit: 2, maxChars: 5 });
    const second = await fixture.gateway.threadMessages("thread-messages", {
      limit: 2,
      cursor: first.page.nextCursor ?? undefined,
      maxChars: 5,
    });

    expect(first.page.messages).toHaveLength(2);
    expect(first.page.messages[1]?.text).toBe("01234\n[truncated]");
    expect(first.page.truncated).toBe(true);
    expect(first.page).not.toHaveProperty("items");
    expect(JSON.stringify(first)).not.toContain("0123456789");
    expect(first.page.hasMore).toBe(true);
    expect(second.page.messages.map((message) => message.id)).toEqual(["message-3", "message-4"]);
  });
});

describe("gateway mutation and recovery hardening", () => {
  it("registers projects and creates threads with explicit defaults", async () => {
    const { fake, fixture } = await setup();
    const projectResult = await fixture.gateway.projectCreate({
      title: "new remote project",
      workspaceRoot: "/remote/new-project",
      createWorkspaceRootIfMissing: false,
      defaultModelSelection: { instanceId: "codex_openai", model: "gpt-5.3-codex-spark" },
      idempotencyKey: "project-create-1",
    });
    const threadResult = await fixture.gateway.threadCreate({
      projectId: projectResult.projectId,
      title: "isolated investigation",
      runtimeMode: "approval-required",
      interactionMode: "plan",
      branch: "investigation-branch",
      worktreePath: "/remote/new-project-worktree",
      idempotencyKey: "thread-create-1",
    });

    expect(projectResult).toMatchObject({
      status: "accepted",
      environmentId: fake.environmentId,
      projectId: expect.any(String),
      workspaceRoot: "/remote/new-project",
    });
    expect(threadResult).toMatchObject({
      status: "accepted",
      projectId: projectResult.projectId,
      threadId: expect.any(String),
      workspace: { branch: "investigation-branch", worktreePath: "/remote/new-project-worktree" },
    });
    expect(fake.dispatches.map(({ command }) => command.type)).toEqual(["project.create", "thread.create"]);
    expect(fake.dispatches[0]?.command).toMatchObject({
      type: "project.create",
      workspaceRoot: "/remote/new-project",
      createWorkspaceRootIfMissing: false,
    });
    expect(fake.dispatches[1]?.command).toMatchObject({
      type: "thread.create",
      runtimeMode: "approval-required",
      interactionMode: "plan",
      modelSelection: { instanceId: "codex_openai", model: "gpt-5.3-codex-spark" },
    });
  });

  it("rejects thread creation without a discovered/default model", async () => {
    const { fake, fixture } = await setup();
    fake.addProject({ id: "no-model", defaultModelSelection: null });

    await expect(
      fixture.gateway.threadCreate({ projectId: "no-model", title: "needs model", idempotencyKey: "no-model-key" }),
    ).rejects.toMatchObject<Partial<GatewayError>>({ code: "model_selection_required" });
    expect(fake.dispatches).toHaveLength(0);
  });

  it("rejects unknown projects and busy threads before creating commands", async () => {
    const { fake, fixture } = await setup();
    fake.addProject({ id: "project-1" });
    fake.addThread({
      id: "busy-thread",
      projectId: "project-1",
      latestTurn: {
        turnId: "turn-running",
        state: "running",
        requestedAt: "2026-01-01T00:00:00.000Z",
        startedAt: "2026-01-01T00:00:00.000Z",
        completedAt: null,
        assistantMessageId: null,
      },
      session: { status: "running" },
    });

    await expect(
      fixture.gateway.threadCreate({ projectId: "missing", title: "x", idempotencyKey: "missing-project" }),
    ).rejects.toMatchObject<Partial<GatewayError>>({ code: "project_not_found" });
    await expect(
      fixture.gateway.threadSend({ threadId: "busy-thread", message: "second turn", idempotencyKey: "busy-turn" }),
    ).rejects.toMatchObject<Partial<GatewayError>>({ code: "thread_busy" });
    expect(fake.dispatches).toHaveLength(0);
  });

  it("accepts a turn, observes running/completed/approval states, and preserves provider-ID honesty", async () => {
    const { fake, fixture } = await setup();
    fake.addProject({ id: "project-run" });
    const thread = fake.addThread({ id: "thread-run", projectId: "project-run" });

    const accepted = await fixture.gateway.threadSend({
      threadId: thread.id,
      message: "Investigate failing tests",
      modelSelection: { instanceId: "codex_openai", model: "gpt-5.3-codex-spark" },
      runtimeMode: "auto",
      idempotencyKey: "run-key",
    });
    const running = await fixture.gateway.runGet(accepted.runId);
    const turnId = running.t3TurnId;
    expect(accepted).toMatchObject({ status: "accepted", providerTurnId: null, t3TurnId: null });
    expect(running).toMatchObject({ runStatus: "running", providerTurnId: null, connectionStatus: "connected" });
    expect(turnId).toBeTruthy();

    thread.latestTurn = {
      ...thread.latestTurn!,
      state: "completed",
      completedAt: "2026-01-01T00:01:00.000Z",
      assistantMessageId: "assistant-run",
    };
    thread.session = { status: "stopped" };
    thread.messages.push(assistantMessage("I found the failing assertion.", turnId!, "assistant-run"));
    const completed = await fixture.gateway.runGet(accepted.runId);
    expect(completed).toMatchObject({
      runStatus: "completed",
      t3TurnId: turnId,
      latestResponse: { id: "assistant-run", role: "assistant" },
    });

    thread.hasPendingApprovals = true;
    thread.latestTurn = { ...thread.latestTurn!, state: "running" };
    const approval = await fixture.gateway.runGet(accepted.runId);
    expect(approval.runStatus).toBe("awaiting_approval");
    thread.hasPendingApprovals = false;
    thread.hasPendingUserInput = true;
    expect((await fixture.gateway.runGet(accepted.runId)).runStatus).toBe("awaiting_input");
  });

  it("waits for a state change and distinguishes timeout from failure", async () => {
    const { fake, fixture } = await setup();
    fake.addProject({ id: "project-wait" });
    const thread = fake.addThread({ id: "thread-wait", projectId: "project-wait" });
    const accepted = await fixture.gateway.threadSend({ threadId: thread.id, message: "wait", idempotencyKey: "wait-key" });

    const timedOut = await fixture.gateway.runWait(accepted.runId, 0.02);
    expect(timedOut.runStatus).toBe("running");
    expect(timedOut.timedOut).toBe(true);

    const change = setTimeout(() => {
      thread.latestTurn = { ...thread.latestTurn!, state: "completed", completedAt: "2026-01-01T00:01:00.000Z" };
      thread.session = { status: "stopped" };
    }, 60);
    const changed = await fixture.gateway.runWait(accepted.runId, 0.5);
    clearTimeout(change);
    expect(changed.runStatus).toBe("completed");
    expect(changed.timedOut).toBeUndefined();
  });

  it("interrupts an active run and retries the same interrupt without a second dispatch", async () => {
    const { fake, fixture } = await setup();
    fake.addProject({ id: "project-interrupt" });
    const thread = fake.addThread({ id: "thread-interrupt", projectId: "project-interrupt" });
    const run = await fixture.gateway.threadSend({ threadId: thread.id, message: "long task", idempotencyKey: "long-task" });

    const first = await fixture.gateway.runInterrupt({ runId: run.runId, idempotencyKey: "interrupt-key" });
    const second = await fixture.gateway.runInterrupt({ runId: run.runId, idempotencyKey: "interrupt-key" });

    expect(first.status).toBe("accepted");
    expect(second).toMatchObject({ status: "accepted", operationId: first.operationId, commandId: first.commandId });
    expect(fake.dispatches.filter(({ command }) => command.type === "thread.turn.interrupt")).toHaveLength(1);
    expect((await fixture.gateway.runGet(run.runId)).runStatus).toBe("interrupted");
  });

  it("returns an accepted interrupt receipt without replay when T3 disconnects", async () => {
    const { fake, fixture } = await setup();
    fake.addProject({ id: "project-interrupt-offline" });
    const thread = fake.addThread({ id: "thread-interrupt-offline", projectId: "project-interrupt-offline" });
    const run = await fixture.gateway.threadSend({ threadId: thread.id, message: "interrupt me", idempotencyKey: "offline-run" });
    const first = await fixture.gateway.runInterrupt({ runId: run.runId, idempotencyKey: "offline-interrupt" });
    await fake.close();
    const restarted = await gatewayFixture(fake, { dataDir: fixture.directory, t3HttpBaseUrl: fixture.config.t3HttpBaseUrl });
    fixtures.push(restarted);

    const second = await restarted.gateway.runInterrupt({ runId: run.runId, idempotencyKey: "offline-interrupt" });

    expect(second).toMatchObject({ status: "accepted", operationId: first.operationId, commandId: first.commandId });
    expect(fake.dispatches.filter(({ command }) => command.type === "thread.turn.interrupt")).toHaveLength(1);
  });

  it("returns rejected for definitive T3 errors and uncertain for ambiguous failures", async () => {
    const rejected = await setup({ dispatchStatus: 422, dispatchErrorCode: "invalid", dispatchErrorMessage: "invalid command" });
    rejected.fake.addProject({ id: "project-rejected" });
    rejected.fake.addThread({ id: "thread-rejected", projectId: "project-rejected" });
    const rejectedResult = await rejected.fixture.gateway.threadArchive({ threadId: "thread-rejected", idempotencyKey: "archive-rejected" });
    expect(rejectedResult).toMatchObject({ status: "rejected", reason: "invalid command" });

    const uncertain = await setup({ dispatchStatus: 503, dispatchErrorCode: "unavailable" });
    uncertain.fake.addProject({ id: "project-uncertain" });
    uncertain.fake.addThread({ id: "thread-uncertain", projectId: "project-uncertain" });
    const uncertainResult = await uncertain.fixture.gateway.threadArchive({ threadId: "thread-uncertain", idempotencyKey: "archive-uncertain" });
    expect(uncertainResult.status).toBe("uncertain");
    expect(uncertain.fake.dispatches).toHaveLength(0);
  });

  it("reconciles an uncertain turn that actually committed remotely", async () => {
    const { fake, fixture } = await setup({
      dispatchStatus: 503,
      dispatchErrorMessage: "response lost after commit",
      applyBeforeDispatchFailure: true,
    });
    fake.addProject({ id: "project-reconcile" });
    fake.addThread({ id: "thread-reconcile", projectId: "project-reconcile" });

    const first = await fixture.gateway.threadSend({
      threadId: "thread-reconcile",
      message: "commit but lose response",
      idempotencyKey: "reconcile-key",
    });
    const second = await fixture.gateway.threadSend({
      threadId: "thread-reconcile",
      message: "commit but lose response",
      idempotencyKey: "reconcile-key",
    });

    expect(first.status).toBe("uncertain");
    expect(second).toMatchObject({ status: "accepted", runId: first.runId, operationId: first.operationId });
    expect(fake.dispatches.filter(({ command }) => command.type === "thread.turn.start")).toHaveLength(1);
  });

  it("returns the durable uncertain handle on a retry while T3 is unavailable", async () => {
    const { fake, fixture } = await setup({ dispatchStatus: 503, dispatchErrorMessage: "remote offline" });
    fake.addProject({ id: "project-offline-retry" });
    fake.addThread({ id: "thread-offline-retry", projectId: "project-offline-retry" });
    const first = await fixture.gateway.threadSend({
      threadId: "thread-offline-retry",
      message: "retry after disconnect",
      idempotencyKey: "offline-retry-key",
    });
    await fake.close();
    const restarted = await gatewayFixture(fake, { dataDir: fixture.directory, t3HttpBaseUrl: fixture.config.t3HttpBaseUrl });
    fixtures.push(restarted);

    const second = await restarted.gateway.threadSend({
      threadId: "thread-offline-retry",
      message: "retry after disconnect",
      idempotencyKey: "offline-retry-key",
    });

    expect(second).toMatchObject({ status: "uncertain", runId: first.runId, operationId: first.operationId });
    expect(fake.dispatches).toHaveLength(0);
  });

  it("extracts pending approvals/questions and validates response shape", async () => {
    const { fake, fixture } = await setup();
    fake.addProject({ id: "project-actions" });
    fake.addThread({
      id: "thread-actions",
      projectId: "project-actions",
      hasPendingApprovals: true,
      hasPendingUserInput: true,
      activities: [
        { tone: "approval", kind: "tool.approval", summary: "Allow test command", payload: { requestId: "approval-1", command: "pnpm test" } },
        { kind: "user.question", summary: "Which branch?", payload: { requestId: "question-1", field: "branch" } },
        { kind: "noise", payload: { irrelevant: true } },
      ],
    });

    const pending = await fixture.gateway.pendingActionsList("thread-actions");
    expect(pending).toMatchObject({ hasPendingApprovals: true, hasPendingUserInput: true, detailsAvailable: true });
    expect(pending.actions.map((action) => [action.kind, action.requestId])).toEqual([
      ["approval", "approval-1"],
      ["user_input", "question-1"],
    ]);
    await expect(
      fixture.gateway.pendingActionRespond({ threadId: "thread-actions", requestId: "approval-1", kind: "approval", idempotencyKey: "missing-decision" }),
    ).rejects.toMatchObject<Partial<GatewayError>>({ code: "decision_required" });
    await expect(
      fixture.gateway.pendingActionRespond({ threadId: "thread-actions", requestId: "question-1", kind: "user_input", idempotencyKey: "missing-answers" }),
    ).rejects.toMatchObject<Partial<GatewayError>>({ code: "answers_required" });

    const approval = await fixture.gateway.pendingActionRespond({
      threadId: "thread-actions",
      requestId: "approval-1",
      kind: "approval",
      decision: "accept",
      idempotencyKey: "approval-response",
    });
    const question = await fixture.gateway.pendingActionRespond({
      threadId: "thread-actions",
      requestId: "question-1",
      kind: "user_input",
      answers: { branch: "main" },
      idempotencyKey: "question-response",
    });
    expect(approval.status).toBe("accepted");
    expect(question.status).toBe("accepted");
    expect(fake.dispatches.map(({ command }) => command.type)).toEqual([
      "thread.approval.respond",
      "thread.user-input.respond",
    ]);
  });

  it("archives without deleting and excludes archived threads by default", async () => {
    const { fake, fixture } = await setup();
    fake.addProject({ id: "project-archive" });
    fake.addThread({ id: "thread-archive", projectId: "project-archive" });

    const archived = await fixture.gateway.threadArchive({ threadId: "thread-archive", idempotencyKey: "archive-key" });
    const visible = await fixture.gateway.threadsList({ includeArchived: false, limit: 10 });
    const all = await fixture.gateway.threadsList({ includeArchived: true, limit: 10 });

    expect(archived.status).toBe("accepted");
    expect(visible.page.items.some((thread) => thread.id === "thread-archive")).toBe(false);
    expect(all.page.items.some((thread) => thread.id === "thread-archive")).toBe(true);
    expect(fake.threads.some((thread) => thread.id === "thread-archive")).toBe(true);
  });

  it("blocks every mutation at the gateway read-only boundary", async () => {
    const { fake, fixture } = await setup();
    fake.addProject({ id: "project-read-only" });
    fake.addThread({ id: "thread-read-only", projectId: "project-read-only" });
    const readOnly = await gatewayFixture(fake, { readOnly: true });
    fixtures.push(readOnly);

    const cases = [
      () => readOnly.gateway.projectCreate({ title: "x", workspaceRoot: "/x", idempotencyKey: "ro-project" }),
      () => readOnly.gateway.threadCreate({ projectId: "project-read-only", title: "x", idempotencyKey: "ro-thread" }),
      () => readOnly.gateway.threadSend({ threadId: "thread-read-only", message: "x", idempotencyKey: "ro-send" }),
      () => readOnly.gateway.threadArchive({ threadId: "thread-read-only", idempotencyKey: "ro-archive" }),
      () => readOnly.gateway.threadSnooze({ threadId: "thread-read-only", idempotencyKey: "ro-snooze" }),
      () => readOnly.gateway.threadUnsnooze({ threadId: "thread-read-only", idempotencyKey: "ro-unsnooze" }),
      () => readOnly.gateway.threadSettle({ threadId: "thread-read-only", idempotencyKey: "ro-settle" }),
      () => readOnly.gateway.threadUnsettle({ threadId: "thread-read-only", idempotencyKey: "ro-unsettle" }),
      () => readOnly.gateway.pendingActionRespond({ threadId: "thread-read-only", requestId: "a", kind: "approval", decision: "decline", idempotencyKey: "ro-action" }),
    ];
    for (const operation of cases) {
      await expect(operation()).rejects.toMatchObject<Partial<GatewayError>>({ code: "gateway_read_only" });
    }
    expect(fake.dispatches).toHaveLength(0);
  });
});

describe("run identity and journal restart behavior", () => {
  it("does not duplicate an accepted turn across a second gateway instance", async () => {
    const { fake, fixture } = await setup();
    fake.addProject({ id: "project-restart" });
    fake.addThread({ id: "thread-restart", projectId: "project-restart" });
    const first = await fixture.gateway.threadSend({ threadId: "thread-restart", message: "once", idempotencyKey: "restart-key" });
    const second = await gatewayFixture(fake, { dataDir: fixture.directory, t3HttpBaseUrl: fixture.config.t3HttpBaseUrl });
    fixtures.push(second);

    const retried = await second.gateway.threadSend({ threadId: "thread-restart", message: "once", idempotencyKey: "restart-key" });

    expect(retried.runId).toBe(first.runId);
    expect(retried.operationId).toBe(first.operationId);
    expect(fake.dispatches.filter(({ command }) => command.type === "thread.turn.start")).toHaveLength(1);
  });

  it("coalesces concurrent MCP submissions with one idempotency key", async () => {
    const { fake, fixture } = await setup({ dispatchDelayMs: 20 });
    fake.addProject({ id: "project-concurrent" });
    fake.addThread({ id: "thread-concurrent", projectId: "project-concurrent" });
    const input = { threadId: "thread-concurrent", message: "submit once", idempotencyKey: "concurrent-turn" };

    const results = await Promise.all(Array.from({ length: 10 }, () => fixture.gateway.threadSend(input)));

    expect(new Set(results.map((result) => result.runId))).toHaveLength(1);
    expect(new Set(results.map((result) => result.operationId))).toHaveLength(1);
    expect(fake.dispatches.filter(({ command }) => command.type === "thread.turn.start")).toHaveLength(1);
  });

  it("keeps prompts and access tokens out of the operation journal", async () => {
    const { fake, fixture } = await setup();
    fake.addProject({ id: "project-secret" });
    fake.addThread({ id: "thread-secret", projectId: "project-secret" });
    await fixture.gateway.threadSend({
      threadId: "thread-secret",
      message: "do not persist this prompt: secret-prompt-value",
      idempotencyKey: "secret-key",
    });

    const journal = await readFile(`${fixture.directory}/operations.json`, "utf8");
    expect(journal).not.toContain("secret-prompt-value");
    expect(journal).not.toContain(fake.accessToken);
  });

  it("reports an uncertain run honestly after restart when T3 cannot be reconciled", async () => {
    const { fake, fixture } = await setup({ dispatchStatus: 503, dispatchErrorMessage: "network outage" });
    fake.addProject({ id: "project-unknown" });
    fake.addThread({ id: "thread-unknown", projectId: "project-unknown" });
    const uncertain = await fixture.gateway.threadSend({ threadId: "thread-unknown", message: "unknown", idempotencyKey: "unknown-key" });
    expect(uncertain.status).toBe("uncertain");
    await fake.close();
    const second = await gatewayFixture(fake, { dataDir: fixture.directory, t3HttpBaseUrl: fixture.config.t3HttpBaseUrl });
    fixtures.push(second);

    const observed = await second.gateway.runGet(uncertain.runId);

    expect(observed).toMatchObject({ runStatus: "unknown", connectionStatus: "disconnected" });
    expect(observed.error).toBeTruthy();
  });
});
