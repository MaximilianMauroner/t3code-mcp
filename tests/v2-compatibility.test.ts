import { createServer } from "node:http";
import { once } from "node:events";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { WebSocketServer } from "ws";
import { afterEach, describe, expect, it } from "vitest";
import { z } from "zod";
import { makeGateway } from "../src/gateway.js";
import { T3HttpClient } from "../src/t3/http-client.js";
import { requestT3Rpc } from "../src/t3/rpc-client.js";
import { V2ShellSchema, V2ThreadSchema } from "../src/t3/v2.js";
import providerFailures from "./fixtures/failures/v2-provider-failures.json" with { type: "json" };

const cleanups: Array<() => Promise<void>> = [];
afterEach(async () => { for (const cleanup of cleanups.splice(0).reverse()) await cleanup(); });
const now = "2026-10-02T20:00:00.000Z";
const selection = { instanceId: "codex_openai", model: "gpt-6.1-sol" };
const project = { id: "project-1", title: "Project", workspaceRoot: "/remote/project", defaultModelSelection: selection };
const thread = {
  id: "thread-1", projectId: project.id, title: "Thread", providerInstanceId: "codex_openai",
  modelSelection: selection, runtimeMode: "full-access", interactionMode: "default",
  branch: null, worktreePath: null, createdBy: "user", creationSource: "web",
  createdAt: now, updatedAt: now, archivedAt: null, settledOverride: null, settledAt: null,
  latestUserMessageAt: null, hasActionableProposedPlan: false,
};

// Wire fields from the merged V2 contracts, without the V1 session/latestTurn.
async function setup() {
  const snapshot = V2ThreadSchema.parse({ snapshotSequence: 2, projection: {
    thread, runs: [], messages: [], providerSessions: [], runtimeRequests: [], turnItems: [], plans: [], checkpoints: [], updatedAt: now,
  } });
  const shell = V2ShellSchema.parse({ schemaVersion: 1, snapshotSequence: 2, projects: [project], archivedThreads: [], threads: [{
    ...thread, latestRunId: null, activeRunId: null, status: "idle", pendingRuntimeRequest: null,
  }] });
  const requests: Array<{ path: string; protocol?: string; authorization?: string }> = [];
  const commands: Array<Record<string, unknown>> = [];
  let disconnect = false;
  let hold = false;
  let rejectRpc = false;
  let protocolVersion: 1 | 2 = 2;
  let archivedSequence = 2;
  const server = createServer(async (request, response) => {
    const path = request.url ?? "/";
    requests.push({ path, protocol: request.headers["x-t3-orchestration-protocol"]?.toString(), authorization: request.headers.authorization });
    const send = (body: unknown, status = 200) => { response.writeHead(status, { "content-type": "application/json" }); response.end(JSON.stringify(body)); };
    if (path === "/.well-known/t3/environment") return send({ environmentId: "v2-env", label: "Test", serverVersion: `protocol-${protocolVersion}`, orchestrationProtocolVersion: protocolVersion });
    if (request.headers.authorization !== "Bearer test-token") return send({ code: "auth_invalid", reason: "missing_credentials", traceId: "trace-1" }, 401);
    if (path === "/api/auth/session") return send({ authenticated: true, scopes: ["orchestration:read", "orchestration:operate"] });
    if (protocolVersion === 1) {
      if (path === "/api/orchestration/shell") return send({ snapshotSequence: 2, projects: [project], threads: [thread], updatedAt: now });
      if (path === "/api/orchestration/threads/thread-1") return send({ snapshotSequence: 2, thread: { ...thread, messages: [] } });
      if (path === "/api/orchestration/dispatch") {
        const chunks: Buffer[] = [];
        for await (const chunk of request) chunks.push(Buffer.from(chunk));
        commands.push(z.record(z.string(), z.unknown()).parse(JSON.parse(Buffer.concat(chunks).toString())));
        return send({ sequence: 3 });
      }
    }
    if (path.startsWith("/api/orchestration/") && request.headers["x-t3-orchestration-protocol"] !== "2") return send({ code: "invalid_request", reason: "protocol" }, 400);
    if (path === "/api/orchestration/shell") return send(shell);
    if (path === `/api/orchestration/threads/${snapshot.projection.thread.id}`) return send(snapshot);
    if (path === "/api/projects/mutate") {
      const chunks: Buffer[] = [];
      for await (const chunk of request) chunks.push(Buffer.from(chunk));
      const command = z.object({ type: z.literal("project.create"), commandId: z.string(), projectId: z.string(), title: z.string(), workspaceRoot: z.string() }).parse(JSON.parse(Buffer.concat(chunks).toString()));
      commands.push(command);
      return send({ id: command.projectId });
    }
    send({ code: "not_found", reason: "thread_not_found", traceId: "trace-2" }, 404);
  });
  const sockets = new WebSocketServer({ noServer: true });
  server.on("upgrade", (request, socket, head) => {
    requests.push({ path: request.url ?? "", authorization: request.headers.authorization });
    if (request.url !== "/ws?orchestrationProtocol=2" || request.headers.authorization !== "Bearer test-token") { socket.destroy(); return; }
    sockets.handleUpgrade(request, socket, head, (ws) => sockets.emit("connection", ws));
  });
  sockets.on("connection", (ws) => ws.on("message", (data) => {
    const frame = z.object({ _tag: z.literal("Request"), id: z.string(), tag: z.string(), payload: z.record(z.string(), z.unknown()), headers: z.array(z.unknown()) }).parse(JSON.parse(data.toString()));
    if (frame.tag === "orchestration.getArchivedShellSnapshot") {
      ws.send(JSON.stringify({ _tag: "Exit", requestId: frame.id, exit: { _tag: "Success", value: { schemaVersion: 1, snapshotSequence: archivedSequence, projects: [project], threads: [{ ...shell.threads[0], id: "archived-1", archivedAt: now }] } } }));
      return;
    }
    const command = frame.payload;
    commands.push(command);
    if (disconnect) { ws.close(); return; }
    if (hold) return;
    if (rejectRpc) {
      ws.send(JSON.stringify({ _tag: "Exit", requestId: frame.id, exit: { _tag: "Failure", cause: [{ _tag: "Fail", error: { _tag: "OrchestrationV2DispatchCommandError", message: "Cannot archive a running thread" } }] } }));
      return;
    }
    if (command.type === "thread.create") {
      const created = z.object({ threadId: z.string(), title: z.string(), createdBy: z.literal("user"), creationSource: z.literal("mcp"), runtimeMode: z.enum(["approval-required", "auto-accept-edits", "auto", "full-access"]) }).parse(command);
      snapshot.projection.thread.id = created.threadId;
      snapshot.projection.thread.title = created.title;
      snapshot.projection.thread.runtimeMode = created.runtimeMode;
      shell.threads[0]!.id = created.threadId;
      shell.threads[0]!.title = created.title;
    }
    if (command.type === "thread.runtime-mode.set") {
      snapshot.projection.thread.runtimeMode = z.enum(["approval-required", "auto-accept-edits", "auto", "full-access"]).parse(command.runtimeMode);
    }
    if (command.type === "thread.interaction-mode.set") {
      snapshot.projection.thread.interactionMode = z.enum(["default", "plan"]).parse(command.interactionMode);
    }
    if (command.type === "message.dispatch") {
      const message = z.object({ messageId: z.string(), threadId: z.string(), text: z.string(), createdBy: z.literal("user"), creationSource: z.literal("mcp"), dispatchMode: z.object({ type: z.literal("start_immediately") }), attachments: z.array(z.unknown()) }).parse(command);
      snapshot.projection.runs.push({ id: "run-v2", ordinal: 1, providerInstanceId: "codex_openai", modelSelection: selection, status: "running", userMessageId: message.messageId, rootNodeId: "node-root", requestedAt: now, startedAt: now, completedAt: null });
      snapshot.projection.messages.push({ id: message.messageId, runId: "run-v2", nodeId: null, role: "user", text: message.text, attachments: [], streaming: false, createdAt: now, updatedAt: now });
    }
    if (command.type === "run.interrupt") snapshot.projection.runs[0]!.status = "interrupted";
    if (command.type === "runtime-request.respond") snapshot.projection.runtimeRequests.find((request) => request.id === command.requestId)!.status = "resolved";
    ws.send(JSON.stringify([{ _tag: "Exit", requestId: "unrelated", exit: { _tag: "Success", value: {} } }, { _tag: "Exit", requestId: frame.id, exit: { _tag: "Success", value: { sequence: 3 } } }]));
  }));
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("No server address");
  const baseUrl = `http://127.0.0.1:${address.port}`;
  cleanups.push(async () => { for (const ws of sockets.clients) ws.terminate(); sockets.close(); await new Promise<void>((resolve) => server.close(() => resolve())); });
  const directory = await mkdtemp(join(tmpdir(), "t3-v2-test-"));
  cleanups.push(() => rm(directory, { recursive: true, force: true }));
  const config = { t3HttpBaseUrl: baseUrl, t3AccessToken: "test-token", mcpBearerToken: "mcp-test", readOnly: false, host: "127.0.0.1", port: 0, environmentId: null, environmentLabel: null, dataDir: directory, worktreeRoot: null, staleAfterMs: 30_000 };
  const made = makeGateway(config);
  return { ...made, config, snapshot, shell, commands, requests, baseUrl, setArchivedSequence: (sequence: number) => { archivedSequence = sequence; }, setProtocol: (version: 1 | 2) => { protocolVersion = version; }, disconnect: () => { disconnect = true; }, hold: () => { hold = true; }, rejectRpc: () => { rejectRpc = true; } };
}

describe("merged orchestrator V2 boundary", () => {
  it("keeps each V2 shell row's source sequence when archived reads are ahead", async () => {
    const { client, setArchivedSequence } = await setup();
    setArchivedSequence(10);
    const shell = await client.getShell();
    expect(shell.snapshotSequence).toBe(10);
    expect(shell.threads.find((thread) => thread.id === "thread-1")?.evidenceOrder).toMatchObject({ scope: "shell", snapshotSequence: 2 });
    expect(shell.threads.find((thread) => thread.id === "archived-1")?.evidenceOrder).toMatchObject({ scope: "shell", snapshotSequence: 10 });
  });

  it("switches between V2 and V1 in both directions after descriptor refresh", async () => {
    const { client, setProtocol, requests, commands } = await setup();
    for (const version of [2, 1, 2] as const) {
      setProtocol(version);
      await client.getDescriptor();
      const start = requests.length;
      expect((await client.getShell()).projects[0]?.id).toBe("project-1");
      expect((await client.getThread("thread-1")).thread.id).toBe("thread-1");
      await client.dispatch({ type: "thread.archive", commandId: `archive-${commands.length}`, threadId: "thread-1" });
      const observed = requests.slice(start);
      expect(observed.some((request) => request.path === "/api/orchestration/dispatch")).toBe(version === 1);
      expect(observed.some((request) => request.path.startsWith("/ws?"))).toBe(version === 2);
      expect(observed.filter((request) => request.path.startsWith("/api/orchestration/")).every((request) => request.protocol === (version === 2 ? "2" : undefined))).toBe(true);
    }
    expect(commands).toHaveLength(3);
  });

  it("starts a recoverable composite task with V2 thread creation and message admission", async () => {
    const { gateway, commands } = await setup();
    const input = { projectId: "project-1", title: "V2 task", instruction: "Check this project", runtimeMode: "full-access" as const, workspaceMode: "local" as const, idempotencyKey: "task-v2" };
    const first = await gateway.taskStart(input);
    const second = await gateway.taskStart(input);
    expect(first).toMatchObject({ stage: "run_accepted" });
    expect(second.threadId).toBe(first.threadId);
    expect(commands.filter((command) => command.type === "thread.create")).toHaveLength(1);
    expect(commands.filter((command) => command.type === "message.dispatch")).toHaveLength(1);
  });

  it("reads active and archived threads with negotiated protocol and maps run responses", async () => {
    const { client, requests, snapshot } = await setup();
    expect((await client.getShell()).threads.map((thread) => thread.id)).toEqual(["thread-1", "archived-1"]);
    snapshot.projection.runs.push({ id: "run-v2", ordinal: 1, providerInstanceId: "codex_openai", modelSelection: selection, status: "completed", userMessageId: "user-1", rootNodeId: "node-root", requestedAt: now, startedAt: now, completedAt: now });
    snapshot.projection.messages.push({ id: "answer-1", runId: "run-v2", nodeId: "node-root", role: "assistant", text: "Done", streaming: false, attachments: [], createdAt: now, updatedAt: now });
    expect((await client.getThread("thread-1")).thread).toMatchObject({ latestTurn: { turnId: "run-v2", state: "completed", assistantMessageId: "answer-1" }, messages: [{ turnId: "run-v2", text: "Done" }] });
    expect(requests.filter((request) => request.path.startsWith("/api/orchestration/")).every((request) => request.protocol === "2")).toBe(true);
  });

  it("sends once, binds the run, and interrupts the V2 run ID", async () => {
    const { gateway, commands } = await setup();
    const input = { threadId: "thread-1", message: "Do the work", runtimeMode: "approval-required" as const, interactionMode: "plan" as const, idempotencyKey: "send-v2" };
    const sent = await gateway.threadSend(input);
    expect(sent.status).toBe("accepted");
    await gateway.threadSend(input);
    expect(commands.filter((command) => command.type === "message.dispatch")).toHaveLength(1);
    expect(await gateway.runGet(sent.runId)).toMatchObject({ runStatus: "running", settings: { matchesResolved: true, effective: { runtimeMode: "approval-required" } } });
    await gateway.runInterrupt({ runId: sent.runId, idempotencyKey: "stop-v2" });
    expect(commands.find((command) => command.type === "run.interrupt")).toMatchObject({ runId: "run-v2" });
    expect((await gateway.runGet(sent.runId)).runStatus).toBe("interrupted");
  });

  it("ignores queued successors, child responses, and non-actionable runtime requests", async () => {
    const { client, snapshot, shell } = await setup();
    Object.assign(shell.threads[0]!, {
      latestRunId: "queued", activeRunId: "active", status: "queued", activityRunStatus: "waiting",
      activityRunStartedAt: now, latestRunRequestedAt: "2026-10-02T21:00:00.000Z", latestRunCompletedAt: null,
    });
    expect((await client.getShell()).threads[0]!.latestTurn).toMatchObject({ turnId: "active", requestedAt: now, completedAt: null });
    snapshot.projection.runs.push(
      { id: "active", ordinal: 1, providerInstanceId: "codex_openai", modelSelection: selection, status: "waiting", userMessageId: "user-1", rootNodeId: "node-root", requestedAt: now, startedAt: now, completedAt: null },
      { id: "queued", ordinal: 2, providerInstanceId: "codex_openai", modelSelection: selection, status: "queued", userMessageId: "user-2", rootNodeId: null, requestedAt: now, startedAt: null, completedAt: null },
    );
    snapshot.projection.messages.push({ id: "child-answer", runId: "active", nodeId: "child-node", role: "assistant", text: "Child done", streaming: false, attachments: [], createdAt: now, updatedAt: now });
    snapshot.projection.runtimeRequests.push(
      { id: "dead", nodeId: "node-root", kind: "user_input", status: "pending", responseCapability: { type: "not_resumable", reason: "restart" }, createdAt: now },
      { id: "live", nodeId: "node-root", kind: "user_input", status: "pending", responseCapability: { type: "message" }, createdAt: now },
    );
    snapshot.projection.turnItems.push({ id: "question", ordinal: 0, type: "user_input_request", status: "waiting", requestId: "live", runId: "active", nodeId: "node-root", title: "Question", questions: [{ id: "q1", question: "Which?" }], updatedAt: now });
    const result = (await client.getThread("thread-1")).thread;
    expect(result.latestTurn?.turnId).toBe("active");
    expect(result.messages).toEqual([]);
    expect(result.hasPendingUserInput).toBe(true);
    expect(result.activities).toHaveLength(1);
    await client.dispatch({ type: "thread.user-input.respond", commandId: "answer-command", threadId: "thread-1", requestId: "live", answers: { q1: "first" }, createdAt: now });
    expect((await client.getThread("thread-1")).thread.hasPendingUserInput).toBe(false);
  });

  it("retains structured usage-limit evidence tied to the failed run", async () => {
    const { gateway, snapshot } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "limited" });
    snapshot.projection.runs[0]!.status = "failed";
    snapshot.projection.turnItems.push({ id: "failure", type: "error", ordinal: 1, status: "failed", runId: "run-v2", nodeId: "node-root", title: null, updatedAt: now, failure: { class: "usage_limit", code: "usage_limit", message: "Limit reached", resetAt: "2026-10-03T00:00:00.000Z" } });
    expect(await gateway.runGet(sent.runId)).toMatchObject({ runStatus: "failed", latestResponse: null, failure: { category: "quota", code: "usage_limit", resetAt: "2026-10-03T00:00:00.000Z", turnId: "run-v2" } });
  });

  it("reports V2 class and retryability and enriches a retained reset from the same root failure", async () => {
    const { gateway, snapshot, shell } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "late-reset" });
    const run = snapshot.projection.runs[0]!;
    run.status = "failed";
    run.completedAt = now;
    const failure = { ...providerFailures.codexUsageLimit, resetAt: null, retryable: false };
    snapshot.projection.turnItems.push({ id: "terminal-error", type: "error", status: "failed", ordinal: 1, runId: run.id, nodeId: run.rootNodeId, title: "Usage limit reached", updatedAt: now, failure, retry: { attempt: 2, maxAttempts: 3, retryDelayMs: 250 } });
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: null, status: "failed", lastError: failure.message, lastErrorClass: failure.class, usageLimitResetAt: null });
    // Observe a summary-only shell row first; a full read upgrades its provenance.
    expect((await gateway.threadsList({ includeArchived: false, limit: 5, detail: "summary" })).page.items[0]?.failure).toMatchObject({ category: "unknown", class: "usage_limit", source: "t3_session", resetAt: null });
    expect((await gateway.runWait(sent.runId, 0.1)).failure).toMatchObject({ category: "quota", class: "usage_limit", retryable: false, retry: { attempt: 2, maxAttempts: 3, retryDelayMs: 250 }, source: "t3_v2_turn_item", resetAt: null });
    snapshot.projection.turnItems[0]!.failure!.resetAt = providerFailures.codexUsageLimit.resetAt;
    expect((await gateway.runGet(sent.runId)).failure).toMatchObject({ resetAt: providerFailures.codexUsageLimit.resetAt, retryAfter: null });
    expect((await gateway.threadGet("thread-1")).thread.failure).toMatchObject({ source: "t3_v2_turn_item", code: "usageLimitExceeded" });
  });

  it("reads historical V2 failures after a later run and retains the failed run's model", async () => {
    const { gateway, snapshot } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "historical" });
    const failed = snapshot.projection.runs[0]!;
    failed.status = "failed";
    failed.modelSelection = { instanceId: "claude-instance", model: "claude-opus-5-5" };
    failed.providerInstanceId = "claude-instance";
    snapshot.projection.turnItems.push({ id: "auth-error", type: "error", ordinal: 1, status: "failed", runId: failed.id, nodeId: failed.rootNodeId, title: "Provider error", updatedAt: now, failure: providerFailures.claudeAuthentication });
    snapshot.projection.runs.push({ ...failed, id: "later-run", ordinal: 2, status: "running", userMessageId: "later-user", modelSelection: selection, providerInstanceId: "codex_openai" });
    snapshot.projection.messages.push({ id: "later-answer", role: "assistant", runId: "later-run", nodeId: failed.rootNodeId, text: "Later response", streaming: false, attachments: [], createdAt: now, updatedAt: now });
    expect(await gateway.runGet(sent.runId)).toMatchObject({ runStatus: "failed", latestResponse: null, failure: { category: "provider_error", class: "provider_error", code: "api_error_401", model: "claude-opus-5-5", provider: "claude-instance", resetAt: null, source: "t3_v2_turn_item" } });
    expect((await gateway.runWait(sent.runId, 0.1)).failure).toMatchObject({ model: "claude-opus-5-5", provider: "claude-instance" });
    expect((await gateway.threadGet("thread-1")).thread.failure).toBeNull();
  });

  it("ignores V2 child failures and recovered retry items, even in a failed run", async () => {
    const { gateway, snapshot } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "root-only" });
    const run = snapshot.projection.runs[0]!;
    run.status = "failed";
    snapshot.projection.turnItems.push(
      { id: "child", type: "error", status: "failed", ordinal: 4, runId: run.id, nodeId: "child-node", title: null, updatedAt: now, failure: providerFailures.codexUsageLimit },
      { id: "recovered", type: "error", status: "completed", ordinal: 3, runId: run.id, nodeId: run.rootNodeId, title: "Provider recovered", updatedAt: now, failure: providerFailures.codexUsageLimit },
    );
    expect((await gateway.runGet(sent.runId)).failure).toMatchObject({ category: "unknown", source: "t3_turn", resetAt: null });
    snapshot.projection.turnItems.push({ id: "terminal", type: "error", status: "failed", ordinal: 5, runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now, failure: { class: "provider_error", code: null, message: "API Error: 429" } });
    expect((await gateway.runGet(sent.runId)).failure).toMatchObject({ category: "provider_error", class: "provider_error", source: "t3_v2_turn_item", resetAt: null });
  });

  it.each(["transport_error", "permission_error", "validation_error", "future_error"])("retains the V2 %s class without guessing a category", async (errorClass) => {
    const { gateway, snapshot } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "other-class" });
    snapshot.projection.runs[0]!.status = "failed";
    snapshot.projection.turnItems.push({ id: "terminal", type: "error", ordinal: 1, status: "failed", runId: "run-v2", nodeId: "node-root", title: null, updatedAt: now, failure: { class: errorClass, code: null, message: "Provider stopped", retryable: true, resetAt: "not-a-time" } });
    expect((await gateway.runGet(sent.runId)).failure).toMatchObject({ category: "unknown", class: errorClass, retryable: true, resetAt: null, retryAfter: null });
  });

  it.each([
    ["rateLimitExceeded", "Provider stopped this request.", "rate_limit"],
    ["api_error_429", "Provider stopped this request.", "unknown"],
    [null, "API Error: Request rejected (429) · Usage credits are required for this model.", "auth_billing"],
  ] as const)("uses explicit V2 provider evidence instead of its broad limit class: %s", async (code, message, category) => {
    const { gateway, snapshot, shell } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "specific-type" });
    snapshot.projection.runs[0]!.status = "failed";
    snapshot.projection.turnItems.push({ id: "terminal", type: "error", ordinal: 1, status: "failed", runId: "run-v2", nodeId: "node-root", title: null, updatedAt: now, failure: { class: "usage_limit", code, message } });
    Object.assign(shell.threads[0]!, { latestRunId: "run-v2", activeRunId: null, status: "failed", lastError: message, lastErrorClass: "usage_limit" });
    await gateway.threadsList({ includeArchived: false, detail: "summary", limit: 5 });
    expect((await gateway.runGet(sent.runId)).failure).toMatchObject({ category, class: "usage_limit", resetAt: null });
  });

  it("keeps a usage-limit-blocked run visible after an unstarted successor is cancelled", async () => {
    const { gateway, client, snapshot, shell } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "cancelled-queue" });
    const failed = snapshot.projection.runs[0]!;
    failed.status = "failed";
    failed.completedAt = now;
    snapshot.projection.turnItems.push({ id: "limited", type: "error", ordinal: 1, status: "failed", runId: failed.id, nodeId: failed.rootNodeId, title: null, updatedAt: now, failure: providerFailures.codexUsageLimit });
    snapshot.projection.runs.push({ ...failed, id: "cancelled-successor", ordinal: 2, status: "cancelled", userMessageId: "later-user", startedAt: null });
    Object.assign(shell.threads[0]!, { latestRunId: failed.id, activeRunId: null, status: "failed", lastError: providerFailures.codexUsageLimit.message, lastErrorClass: "usage_limit" });
    expect((await gateway.threadGet("thread-1")).thread).toMatchObject({ latestTurn: { turnId: failed.id, state: "error" }, failure: { class: "usage_limit", source: "t3_v2_turn_item" } });
    expect((await gateway.runGet(sent.runId)).failure?.turnId).toBe(failed.id);
    snapshot.projection.providerSessions.push({ providerInstanceId: "codex_openai", lastError: "Distinct session failure", updatedAt: now });
    const { thread: normalized } = await client.getThread("thread-1");
    expect(normalized.latestTurn?.turnId).toBe("cancelled-successor");
  });

  it("rejects V2 error items without the required status instead of guessing terminal state", async () => {
    const { snapshot } = await setup();
    const payload = { ...snapshot, projection: { ...snapshot.projection, turnItems: [{ id: "missing-status", type: "error", ordinal: 1, runId: "run-v2", nodeId: "node-root", title: null, updatedAt: now, failure: providerFailures.codexUsageLimit }] } };
    expect(V2ThreadSchema.safeParse(payload).success).toBe(false);
  });

  it("keeps a limited executed run using its own provider session", async () => {
    const { gateway, client, snapshot, shell } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "provider-session" });
    const failed = snapshot.projection.runs[0]!;
    Object.assign(failed, { status: "failed", providerInstanceId: "claude-instance", modelSelection: { instanceId: "claude-instance", model: "claude-opus-5-5" }, completedAt: now });
    snapshot.projection.turnItems.push({ id: "limited", type: "error", ordinal: 1, status: "failed", runId: failed.id, nodeId: failed.rootNodeId, title: null, updatedAt: now, failure: providerFailures.codexUsageLimit });
    snapshot.projection.runs.push({ ...failed, id: "cancelled-successor", ordinal: 2, status: "cancelled", userMessageId: "later-user", startedAt: null });
    snapshot.projection.providerSessions.push(
      { providerInstanceId: "codex_openai", lastError: "Unrelated default-provider error", updatedAt: now },
      { providerInstanceId: "claude-instance", lastError: providerFailures.codexUsageLimit.message, updatedAt: now },
    );
    Object.assign(shell.threads[0]!, { latestRunId: failed.id, activeRunId: null, status: "failed", lastError: providerFailures.codexUsageLimit.message, lastErrorClass: "usage_limit" });
    expect((await client.getThread("thread-1")).thread.latestTurn?.turnId).toBe(failed.id);
    expect((await gateway.threadGet("thread-1")).thread).toMatchObject({ latestTurn: { turnId: failed.id, state: "error" }, failure: { provider: "claude-instance", source: "t3_v2_turn_item" } });
    expect((await gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0]?.failure?.turnId).toBe(failed.id);
    expect((await gateway.runGet(sent.runId)).failure?.turnId).toBe(failed.id);
    snapshot.projection.providerSessions[1]!.lastError = "Distinct error on executed provider";
    expect((await client.getThread("thread-1")).thread).toMatchObject({ latestTurn: { turnId: "cancelled-successor" } });
  });

  it("replaces retained root evidence when a later authoritative error item is selected", async () => {
    const { gateway, snapshot, shell } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "newer-error" });
    const run = snapshot.projection.runs[0]!;
    run.status = "failed";
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: null, status: "failed", lastError: providerFailures.codexUsageLimit.message, lastErrorClass: "usage_limit" });
    snapshot.projection.turnItems.push({ id: "first", type: "error", status: "failed", ordinal: 1, runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now, failure: providerFailures.codexUsageLimit });
    expect((await gateway.runGet(sent.runId)).failure).toMatchObject({ category: "quota", resetAt: providerFailures.codexUsageLimit.resetAt });
    snapshot.projection.turnItems.push({ id: "second", type: "error", status: "failed", ordinal: 2, runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now, failure: { class: "usage_limit", code: "api_error_429", message: "API Error: 429", retryable: true }, retry: { attempt: 3, maxAttempts: 3, retryDelayMs: null } });
    expect((await gateway.runGet(sent.runId)).failure).toMatchObject({ category: "unknown", code: "api_error_429", message: "API Error: 429", resetAt: null, retryable: true, retry: { attempt: 3 } });
    expect((await gateway.threadGet("thread-1")).thread.failure).toMatchObject({ category: "unknown", code: "api_error_429", resetAt: null });
    expect((await gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0]?.failure).toMatchObject({ category: "unknown", code: "api_error_429", resetAt: null });
  });

  it("enriches a root failure with a later matching shell reset", async () => {
    const { gateway, snapshot, shell, journal } = await setup();
    await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "shell-reset" });
    const run = snapshot.projection.runs[0]!;
    run.status = "failed";
    const failure = { ...providerFailures.codexUsageLimit, resetAt: null };
    snapshot.projection.turnItems.push({ id: "terminal", type: "error", status: "failed", ordinal: 1, runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now, failure });
    shell.snapshotSequence = 3;
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: null, status: "failed", lastError: failure.message, lastErrorClass: failure.class, usageLimitResetAt: providerFailures.codexUsageLimit.resetAt, updatedAt: "2026-10-02T20:00:01.000Z" });
    expect((await gateway.threadGet("thread-1")).thread.failure).toMatchObject({ source: "t3_v2_turn_item", code: failure.code, resetAt: providerFailures.codexUsageLimit.resetAt });
    expect(await journal.getFailureByTurnId("thread-1", run.id)).toMatchObject({ resetAt: providerFailures.codexUsageLimit.resetAt });
  });

  it("replaces an older root with a newer bound shell reason without mixing failure metadata", async () => {
    const { gateway, client, snapshot, shell, config } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "changed-shell" });
    const run = snapshot.projection.runs[0]!;
    run.status = "failed";
    snapshot.projection.turnItems.push({ id: "first", type: "error", status: "failed", ordinal: 1,
      runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now, failure: providerFailures.codexUsageLimit });
    const older = await client.getThread("thread-1");
    expect((await gateway.runGet(sent.runId)).failure?.category).toBe("quota");
    shell.snapshotSequence = 3;
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: null, status: "failed",
      lastError: "New provider failure", lastErrorClass: "provider_error", usageLimitResetAt: null,
      modelSelection: { instanceId: "changed-provider", model: "changed-model" }, updatedAt: "2026-10-02T20:00:01.000Z" });
    const expected = { category: "provider_error", class: "provider_error", message: "New provider failure",
      code: null, resetAt: null, retry: null, source: "t3_session", provider: run.providerInstanceId, model: run.modelSelection.model };
    expect((await gateway.threadGet("thread-1")).thread.failure).toMatchObject(expected);
    expect((await gateway.threadsList({ includeArchived: false, limit: 5 })).page.items[0]?.failure).toMatchObject(expected);
    expect((await gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0]?.failure).toMatchObject(expected);
    const restarted = makeGateway(config);
    restarted.client.getThread = async () => older;
    expect((await restarted.gateway.runGet(sent.runId)).failure).toMatchObject(expected);
  });

  it.each(["list", "overview"])("uses immutable run identity on a fresh %s with a newer shell failure", async (reader) => {
    const { gateway, client, snapshot, shell, config } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "fresh-shell-identity" });
    const run = snapshot.projection.runs[0]!;
    run.status = "failed";
    snapshot.projection.turnItems.push({ id: "first", type: "error", status: "failed", ordinal: 1,
      runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now, failure: providerFailures.codexUsageLimit });
    const older = await client.getThread("thread-1");
    shell.snapshotSequence = 3;
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: null, status: "failed",
      lastError: "New provider failure", lastErrorClass: "provider_error", usageLimitResetAt: null,
      modelSelection: { instanceId: "changed-provider", model: "changed-model" }, updatedAt: "2026-10-02T20:00:01.000Z" });
    const expected = { category: "provider_error", message: "New provider failure", code: null, resetAt: null,
      provider: run.providerInstanceId, model: run.modelSelection.model };
    const failure = reader === "list"
      ? (await gateway.threadsList({ includeArchived: false, detail: "full", limit: 5 })).page.items[0]?.failure
      : (await gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0]?.failure;
    expect(failure).toMatchObject(expected);
    expect(failure).not.toHaveProperty("runIdentity");
    expect((await gateway.threadGet("thread-1")).thread.failure).toMatchObject(expected);
    const restarted = makeGateway(config);
    restarted.client.getThread = async () => older;
    expect((await restarted.gateway.runGet(sent.runId)).failure).toMatchObject(expected);
    expect((await restarted.gateway.threadsList({ includeArchived: false, limit: 5 })).page.items[0]?.failure).toMatchObject(expected);
  });

  it("invalidates a retained root when a newer full snapshot marks that error recovered", async () => {
    const { gateway, client, snapshot, shell, config } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "recovered-item" });
    const run = snapshot.projection.runs[0]!;
    run.status = "failed";
    run.modelSelection = { instanceId: "claude-instance", model: "claude-opus-5-5" };
    run.providerInstanceId = "claude-instance";
    snapshot.projection.turnItems.push({ id: "first", type: "error", status: "failed", ordinal: 1,
      runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now, failure: providerFailures.codexUsageLimit });
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: null, status: "failed", lastError: providerFailures.codexUsageLimit.message, lastErrorClass: "usage_limit" });
    const older = await client.getThread("thread-1");
    expect((await gateway.runGet(sent.runId)).failure?.category).toBe("quota");
    snapshot.snapshotSequence = 3;
    snapshot.projection.turnItems[0]!.status = "completed";
    snapshot.projection.thread.modelSelection = { instanceId: "changed-provider", model: "changed-model" };
    const expected = { category: "unknown", class: null, code: null, resetAt: null, retry: null,
      source: "t3_turn", provider: run.providerInstanceId, model: run.modelSelection.model };
    expect(await gateway.runGet(sent.runId)).toMatchObject({ runStatus: "failed", failure: expected });
    expect((await gateway.runWait(sent.runId, 0.1)).failure).toMatchObject(expected);
    expect((await gateway.threadGet("thread-1")).thread.failure).toMatchObject(expected);
    expect((await gateway.threadsList({ includeArchived: false, detail: "full", limit: 5 })).page.items[0]?.failure).toMatchObject(expected);
    const restarted = makeGateway(config);
    restarted.client.getThread = async () => older;
    expect((await restarted.gateway.runGet(sent.runId)).failure).toMatchObject(expected);
  });

  it.each(["run", "thread", "overview"])("rejects a delayed older V2 %s read, including after restart", async (reader) => {
    const { gateway, client, snapshot, shell, config } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "concurrent-root" });
    const run = snapshot.projection.runs[0]!;
    run.status = "failed";
    snapshot.projection.turnItems.push({ id: "first", type: "error", status: "failed", ordinal: 1, runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now, failure: providerFailures.codexUsageLimit });
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: null, status: "failed", lastError: providerFailures.codexUsageLimit.message, lastErrorClass: "usage_limit" });
    const older = await client.getThread("thread-1");
    snapshot.snapshotSequence = 3;
    snapshot.projection.turnItems.push({ id: "second", type: "error", status: "failed", ordinal: 2, runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: "2026-10-02T20:00:01.000Z", failure: { class: "usage_limit", code: "api_error_429", message: "API Error: 429" }, retry: { attempt: 3, maxAttempts: 3, retryDelayMs: null } });
    const newer = await client.getThread("thread-1");
    let releaseOlder!: () => void;
    let signalEntered!: () => void;
    const gate = new Promise<void>((resolve) => { releaseOlder = resolve; });
    const entered = new Promise<void>((resolve) => { signalEntered = resolve; });
    let calls = 0;
    client.getThread = async () => {
      if (++calls === 1) { signalEntered(); await gate; return older; }
      return newer;
    };
    const delayed = reader === "run" ? gateway.runGet(sent.runId).then((result) => result.failure)
      : reader === "thread" ? gateway.threadGet("thread-1").then((result) => result.thread.failure)
      : gateway.threadsOverview({ includeArchived: false, runningLimit: 5 }).then((result) => result.highlights[0]?.failure);
    await entered;
    const expected = { category: "unknown", code: "api_error_429", resetAt: null, retry: { attempt: 3 } };
    expect((await gateway.runGet(sent.runId)).failure).toMatchObject(expected);
    releaseOlder();
    expect(await delayed).toMatchObject(expected);
    const restarted = makeGateway(config);
    restarted.client.getThread = async () => older;
    const retained = await restarted.gateway.runGet(sent.runId);
    expect(retained.failure).toMatchObject(expected);
    expect(retained).not.toHaveProperty("failureOrder");
    expect(retained.failure).not.toHaveProperty("order");
  });

  it("uses exact shell credit text and leaves a broad shell limit unknown", async () => {
    const { gateway, shell } = await setup();
    Object.assign(shell.threads[0]!, { latestRunId: "failed-run", activeRunId: null, status: "failed", lastError: "API Error: Request rejected (429) · Usage credits are required for this model.", lastErrorClass: "usage_limit" });
    const credits = await gateway.threadsList({ includeArchived: false, detail: "summary", limit: 5 });
    expect(credits.page.items[0]?.failure).toMatchObject({ category: "auth_billing", class: "usage_limit" });
    shell.threads[0]!.latestRunId = "another-failed-run";
    shell.threads[0]!.lastError = "Provider stopped this request.";
    const broad = await gateway.threadsList({ includeArchived: false, detail: "summary", limit: 5 });
    expect(broad.page.items[0]?.failure).toMatchObject({ category: "unknown", class: "usage_limit" });
  });

  it("uses precise same-turn shell evidence when the full failure is generic", async () => {
    const { gateway, snapshot, shell } = await setup();
    snapshot.projection.runs.push({ id: "failed-run", ordinal: 1, providerInstanceId: "codex_openai", modelSelection: selection, status: "failed", userMessageId: "user-1", rootNodeId: "root", requestedAt: now, startedAt: now, completedAt: now });
    shell.snapshotSequence = 3;
    Object.assign(shell.threads[0]!, { latestRunId: "failed-run", activeRunId: null, status: "failed", lastError: "Codex usage limit reached. Send the message again once the limit resets.", lastErrorClass: "usage_limit", usageLimitResetAt: providerFailures.codexUsageLimit.resetAt });
    expect((await gateway.threadGet("thread-1")).thread.failure).toMatchObject({ category: "quota", source: "t3_session", resetAt: providerFailures.codexUsageLimit.resetAt });
  });

  it.each([
    ["API Error: rate_limit_error: Too many requests", "rate_limit"],
    ["API Error: 503 auth_unavailable: No available credentials", "auth_billing"],
    ["API Error: 429", "unknown"],
  ])("preserves explicit provider types in shell-only text: %s", async (message, category) => {
    const { gateway, shell } = await setup();
    Object.assign(shell.threads[0]!, { latestRunId: "failed-run", activeRunId: null, status: "failed", lastError: message, lastErrorClass: "usage_limit" });
    const rows = await gateway.threadsList({ includeArchived: false, detail: "summary", limit: 5 });
    expect(rows.page.items[0]?.failure).toMatchObject({ category, class: "usage_limit", source: "t3_session", resetAt: null });
  });

  it("does not bind a distinct V2 session error to the shell's latest run", async () => {
    const { gateway, shell } = await setup();
    Object.assign(shell.threads[0]!, { latestRunId: "old-failed-run", activeRunId: null, status: "failed", lastError: "New unbound session error", lastErrorClass: null, usageLimitResetAt: null });
    const summary = (await gateway.threadsList({ includeArchived: false, detail: "summary", limit: 5 })).page.items[0];
    expect(summary?.failure).toMatchObject({ category: "unknown", source: "t3_turn", resetAt: null });
    expect(summary?.failure?.message).not.toContain("unbound session");
  });

  it("keeps a disconnected mutation uncertain and never replays it", async () => {
    const { gateway, disconnect, commands } = await setup();
    disconnect();
    const input = { threadId: "thread-1", message: "Work", idempotencyKey: "disconnect" };
    expect((await gateway.threadSend(input)).status).toBe("uncertain");
    expect((await gateway.threadSend(input)).status).toBe("uncertain");
    expect(commands).toHaveLength(1);
  });

  it("rejects unknown orchestration versions before sending a command", async () => {
    const { baseUrl, commands } = await setup();
    const client = new T3HttpClient(baseUrl, "test-token");
    const descriptor = await client.getDescriptor();
    descriptor.orchestrationProtocolVersion = 3;
    await expect(client.dispatch({ type: "thread.archive", commandId: "archive", threadId: "thread-1" })).rejects.toThrow("Unsupported T3 orchestration protocol 3");
    expect(commands).toEqual([]);
  });

  it("bounds an unacknowledged WebSocket RPC with cancellation", async () => {
    const { baseUrl } = await setup();
    const signal = AbortSignal.abort(new Error("cancelled"));
    await expect(requestT3Rpc(baseUrl, "test-token", 50, "orchestration.dispatchCommand", {}, z.unknown(), signal)).rejects.toThrow("cancelled");
  });

  it("times out a connected socket and surfaces tagged RPC failures", async () => {
    const fixture = await setup();
    fixture.rejectRpc();
    await expect(fixture.client.dispatch({ type: "thread.archive", threadId: "thread-1", commandId: "archive" })).rejects.toThrow("Cannot archive a running thread");
    fixture.hold();
    await expect(requestT3Rpc(fixture.baseUrl, "test-token", 50, "orchestration.dispatchCommand", { type: "thread.archive" }, z.unknown())).rejects.toMatchObject({ name: "TimeoutError" });
  });
});
