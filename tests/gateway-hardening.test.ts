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
    expect(status.gatewayOperations).toContain("t3_task_start");
    expect((await fixture.gateway.threadsList({ includeArchived: false, limit: 10 })).environmentId).toBe(fake.environmentId);
  });

  it("does not silently operate on a different configured environment", async () => {
    const { fixture } = await setup({ environmentId: "actual-environment" });
    const mismatched = await gatewayFixture((fakes.at(-1) as FakeT3), { environmentId: "expected-environment" });
    fixtures.push(mismatched);

    const status = await mismatched.gateway.connectionStatus();

    expect(status.connectionStatus).toBe("disconnected");
    expect(status.environment?.environmentId).toBe("actual-environment");
    expect(status.error).toContain("does not match");
    await expect(mismatched.gateway.threadsList({ includeArchived: false, limit: 10 })).rejects.toMatchObject<Partial<GatewayError>>({
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

  it("paginates threads while filtering archived threads", async () => {
    const { fake, fixture } = await setup();
    const project = fake.addProject({ id: "project-a", title: "A" });
    fake.addThread({ id: "thread-a1", projectId: project.id });
    fake.addThread({ id: "thread-a2", projectId: project.id });
    fake.addThread({ id: "thread-archived", projectId: project.id, archivedAt: "2026-01-02T00:00:00.000Z" });

    const first = await fixture.gateway.threadsList({ projectId: project.id, includeArchived: false, limit: 1 });
    const second = await fixture.gateway.threadsList({
      projectId: project.id,
      includeArchived: false,
      limit: 1,
      cursor: first.page.nextCursor ?? undefined,
    });
    const archived = await fixture.gateway.threadsList({ projectId: project.id, includeArchived: true, limit: 10 });

    expect(first.page.items.map((item) => item.id)).toEqual(["thread-a1"]);
    expect(first.page).toMatchObject({ hasMore: true, total: 2 });
    expect(second.page.items.map((item) => item.id)).toEqual(["thread-a2"]);
    expect(second.page.hasMore).toBe(false);
    expect(archived.page.items).toHaveLength(3);
  });

});

describe("gateway mutation and recovery hardening", () => {
  it("creates threads with explicit defaults", async () => {
    const { fake, fixture } = await setup();
    const project = fake.addProject({
      id: "new-remote-project",
      defaultModelSelection: { instanceId: "codex_openai", model: "gpt-5.3-codex-spark" },
    });
    const threadResult = await fixture.gateway.threadCreate({
      projectId: project.id,
      title: "isolated investigation",
      runtimeMode: "approval-required",
      interactionMode: "plan",
      branch: "investigation-branch",
      idempotencyKey: "thread-create-1",
    });

    expect(threadResult).toMatchObject({
      status: "accepted",
      environmentId: fake.environmentId,
      projectId: project.id,
      threadId: expect.any(String),
      workspace: { branch: "investigation-branch", worktreePath: null },
    });
    expect(fake.dispatches.map(({ command }) => command.type)).toEqual(["thread.create"]);
    expect(fake.dispatches[0]?.command).toMatchObject({
      type: "thread.create",
      runtimeMode: "approval-required",
      interactionMode: "plan",
      modelSelection: { instanceId: "codex_openai", model: "gpt-5.3-codex-spark" },
    });
  });

  it("rejects a missing low-level thread workspace before persisting it", async () => {
    const { fake, fixture } = await setup();
    fake.addProject({ id: "missing-workspace-project" });

    await expect(fixture.gateway.threadCreate({
      projectId: "missing-workspace-project",
      title: "broken workspace",
      branch: "feature/missing",
      worktreePath: "/definitely/missing/t3-worktree",
      idempotencyKey: "missing-thread-workspace",
    })).rejects.toMatchObject({
      code: "workspace_missing",
      message: expect.stringContaining("workspaceMode=worktree"),
    });
    expect(fake.dispatches).toHaveLength(0);
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

    const next = await fixture.gateway.threadSend({
      threadId: thread.id, message: "Continue with approval", idempotencyKey: "approval-run-key",
    });
    thread.hasPendingApprovals = true;
    const approval = await fixture.gateway.runGet(next.runId);
    expect(approval.runStatus).toBe("awaiting_approval");
    thread.hasPendingApprovals = false;
    thread.hasPendingUserInput = true;
    expect((await fixture.gateway.runGet(next.runId)).runStatus).toBe("awaiting_input");
  });

  it("returns rejected for definitive T3 errors and uncertain for ambiguous failures", async () => {
    const rejected = await setup({ dispatchStatus: 422, dispatchErrorCode: "invalid", dispatchErrorMessage: "invalid command" });
    rejected.fake.addProject({ id: "project-rejected" });
    rejected.fake.addThread({ id: "thread-rejected", projectId: "project-rejected" });
    const rejectedResult = await rejected.fixture.gateway.threadSend({ threadId: "thread-rejected", message: "x", idempotencyKey: "send-rejected" });
    expect(rejectedResult).toMatchObject({ status: "rejected", reason: "invalid command" });

    const uncertain = await setup({ dispatchStatus: 503, dispatchErrorCode: "unavailable" });
    uncertain.fake.addProject({ id: "project-uncertain" });
    uncertain.fake.addThread({ id: "thread-uncertain", projectId: "project-uncertain" });
    const uncertainResult = await uncertain.fixture.gateway.threadSend({ threadId: "thread-uncertain", message: "x", idempotencyKey: "send-uncertain" });
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

  it("blocks every mutation at the gateway read-only boundary", async () => {
    const { fake, fixture } = await setup();
    fake.addProject({ id: "project-read-only" });
    fake.addThread({ id: "thread-read-only", projectId: "project-read-only" });
    const readOnly = await gatewayFixture(fake, { readOnly: true });
    fixtures.push(readOnly);

    const cases = [
      () => readOnly.gateway.taskStart({
        projectId: "project-read-only", title: "x", instruction: "x", runtimeMode: "full-access", idempotencyKey: "ro-task",
      }),
      () => readOnly.gateway.threadCreate({ projectId: "project-read-only", title: "x", idempotencyKey: "ro-thread" }),
      () => readOnly.gateway.threadSend({ threadId: "thread-read-only", message: "x", idempotencyKey: "ro-send" }),
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
