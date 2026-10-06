import { createHash } from "node:crypto";
import { createServer } from "node:http";
import { once } from "node:events";
import { mkdtemp, readFile, rm } from "node:fs/promises";
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
  let protocolVersion = 2;
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
      snapshot.projection.messages.push({ threadId: snapshot.projection.thread.id, id: message.messageId, runId: "run-v2", nodeId: null, role: "user", text: message.text, attachments: [], streaming: false, createdAt: now, updatedAt: now });
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
  return { ...made, config, snapshot, shell, commands, requests, baseUrl, setArchivedSequence: (sequence: number) => { archivedSequence = sequence; }, setProtocol: (version: number) => { protocolVersion = version; }, disconnect: () => { disconnect = true; }, hold: () => { hold = true; }, rejectRpc: () => { rejectRpc = true; } };
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

  it.each(["shell", "thread", "snooze"] as const)("recovers the first %s after an upgrade", async (operation) => {
    const { client, setProtocol, requests, commands } = await setup();
    setProtocol(1);
    await client.getDescriptor();
    setProtocol(2);
    if (operation === "shell") expect((await client.getShell()).threads).toHaveLength(2);
    if (operation === "thread") expect((await client.getThread("thread-1")).thread.id).toBe("thread-1");
    if (operation === "snooze") {
      await client.dispatch({ type: "thread.snooze", commandId: "snooze-1", threadId: "thread-1", snoozedUntil: "2026-10-04T08:47:00.000Z" });
      expect(commands).toHaveLength(1);
      expect(commands[0]?.type).toBe("thread.snooze");
      expect(requests.some((request) => request.path === "/api/orchestration/dispatch")).toBe(false);
      expect(requests.some((request) => request.path.startsWith("/ws?"))).toBe(true);
    }
    expect(requests.filter((request) => request.path === "/.well-known/t3/environment")).toHaveLength(2);
    const reads = requests.filter((request) => request.path.startsWith("/api/orchestration/"));
    if (operation !== "snooze") expect(reads.map((request) => request.protocol)).toEqual([undefined, "2"]);
  });

  it("detects upgrades and rollbacks on the same client without an explicit refresh", async () => {
    const { client, setProtocol, requests, commands } = await setup();
    setProtocol(1);
    await client.getDescriptor(); // The gateway's startup discovery.
    for (const version of [2, 1, 2] as const) {
      setProtocol(version);
      const start = requests.length;
      expect((await client.getShell()).projects[0]?.id).toBe("project-1");
      expect((await client.getThread("thread-1")).thread.id).toBe("thread-1");
      await client.dispatch({ type: "thread.archive", commandId: `archive-${commands.length}`, threadId: "thread-1" });
      const observed = requests.slice(start);
      expect(observed.some((request) => request.path === "/api/orchestration/dispatch")).toBe(version === 1);
      expect(observed.some((request) => request.path.startsWith("/ws?"))).toBe(version === 2);
      const reads = observed.filter((request) => request.path.startsWith("/api/orchestration/"));
      expect(reads[0]?.protocol).toBe(version === 2 ? undefined : "2");
      expect(reads.slice(1).every((request) => request.protocol === (version === 2 ? "2" : undefined))).toBe(true);
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
    snapshot.projection.messages.push({ threadId: snapshot.projection.thread.id, id: "answer-1", runId: "run-v2", nodeId: "node-root", role: "assistant", text: "Done", streaming: false, attachments: [], createdAt: now, updatedAt: now });
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

  it("keeps native item-node responses across run reads, summaries, history pages and text chunks", async () => {
    const { client, gateway, snapshot, shell } = await setup();
    const sent = await gateway.threadSend({ threadId: thread.id, message: "Check the result", idempotencyKey: "native-result" });
    const current = snapshot.projection.runs[0]!;
    current.ordinal = 2;
    current.status = "completed";
    current.completedAt = now;
    snapshot.projection.runs.unshift({ ...current, id: "previous-run", ordinal: 1, rootNodeId: "previous-root" });
    const text = "Native item result 🙂. ".repeat(50) + "Final result.";
    snapshot.projection.messages.push(
      { id: "previous-answer", threadId: thread.id, runId: "previous-run", nodeId: "previous-native-item", role: "assistant", text: "Previous result", streaming: false, attachments: [], createdAt: now, updatedAt: now },
      { id: "native-answer", threadId: thread.id, runId: current.id, nodeId: "provider-item-node", role: "assistant", text, streaming: false, attachments: [], createdAt: now, updatedAt: now },
      // Delegated subagents own another thread and have no parent projection run.
      { id: "child-answer", threadId: "delegated-child-thread", runId: null, nodeId: null, role: "assistant", text: "Child result must not replace the parent result", streaming: false, attachments: [], createdAt: now, updatedAt: now },
    );
    Object.assign(shell.threads[0]!, {
      latestRunId: current.id, activeRunId: null, status: "completed",
      latestRunRequestedAt: now, latestRunStartedAt: now, latestRunCompletedAt: now,
    });
    const observed = (await client.getThread(thread.id)).thread;
    expect(observed.messages.map((message) => message.id)).toEqual([sent.messageId, "previous-answer", "native-answer"]);
    expect(observed.latestTurn?.assistantMessageId).toBe("native-answer");
    expect(await gateway.runGet(sent.runId)).toMatchObject({
      runStatus: "completed", latestResponse: { id: "native-answer", turnId: current.id, text },
    });
    expect((await gateway.threadGet(thread.id)).thread.latestResponse).toMatchObject({ id: "native-answer", text });
    const overview = await gateway.threadsOverview({ includeArchived: false, runningLimit: 5 });
    expect(overview.highlights.find((row) => row.id === thread.id)?.latestResponseExcerpt).toContain(text.slice(0, 40));
    const summaries = await gateway.threadsList({ includeArchived: false, detail: "full", limit: 10 });
    expect(summaries.page.items.find((row) => row.id === thread.id)).toMatchObject({
      latestResponseExcerpt: expect.stringContaining(text.slice(0, 40)),
    });
    const first = await gateway.threadMessages(thread.id, { limit: 1, maxChars: 100 });
    const second = await gateway.threadMessages(thread.id, { cursor: first.page.nextCursor!, limit: 1, maxChars: 100 });
    const third = await gateway.threadMessages(thread.id, { cursor: second.page.nextCursor!, limit: 1, maxChars: 100 });
    expect([first, second, third].flatMap((result) => result.page.messages.map((message) => message.id)))
      .toEqual([sent.messageId, "previous-answer", "native-answer"]);
    expect(third.page.nextCursor).toBeNull();
    let offset = 0;
    let reconstructed = "";
    do {
      const chunk = await gateway.threadMessages(thread.id, { messageId: "native-answer", textOffset: offset, limit: 1, maxChars: 256 });
      const message = chunk.page.messages[0]!;
      expect(message.id).toBe("native-answer");
      expect(message.textRange).toMatchObject({ offset, totalChars: text.length });
      const end = message.textRange.nextOffset ?? text.length;
      expect(end).toBeGreaterThan(offset);
      reconstructed += message.text.slice(0, end - offset);
      offset = end;
    } while (offset < text.length);
    expect(reconstructed).toBe(text);
  });

  it("requires authoritative message thread ownership", async () => {
    const { snapshot } = await setup();
    const unownedMessage = { id: "unowned", runId: null, nodeId: null, role: "assistant", text: "Unknown owner", streaming: false, attachments: [], createdAt: now, updatedAt: now };
    expect(V2ThreadSchema.safeParse({
      ...snapshot, projection: { ...snapshot.projection, messages: [unownedMessage] },
    }).success).toBe(false);
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
    snapshot.projection.messages.push({ threadId: "delegated-child-thread", id: "child-answer", runId: null, nodeId: "child-node", role: "assistant", text: "Child done", streaming: false, attachments: [], createdAt: now, updatedAt: now });
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
    snapshot.projection.messages.push({ threadId: snapshot.projection.thread.id, id: "later-answer", role: "assistant", runId: "later-run", nodeId: failed.rootNodeId, text: "Later response", streaming: false, attachments: [], createdAt: now, updatedAt: now });
    expect(await gateway.runGet(sent.runId)).toMatchObject({ runStatus: "failed", latestResponse: null, failure: { category: "provider_error", class: "provider_error", code: "api_error_401", model: "claude-opus-5-5", provider: "claude-instance", resetAt: null, source: "t3_v2_turn_item" } });
    expect((await gateway.runWait(sent.runId, 0.1)).failure).toMatchObject({ model: "claude-opus-5-5", provider: "claude-instance" });
    expect((await gateway.threadGet("thread-1")).thread.failure).toBeNull();
  });

  it.each([
    ["list", "running"], ["list", "completed"],
    ["overview", "running"], ["overview", "completed"],
  ] as const)("keeps a historical V2 failure when %s enrichment sees a %s successor", async (reader, successorState) => {
    const { gateway, snapshot, shell, config } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "historical-shell-enrichment" });
    const run = snapshot.projection.runs[0]!;
    run.status = "failed";
    const retry = { attempt: 2, maxAttempts: 3, retryDelayMs: 500 };
    snapshot.projection.turnItems.push({ id: "historical-error", type: "error", status: "failed", ordinal: 1,
      runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now, failure: providerFailures.codexUsageLimit, retry });
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: null, status: "failed",
      lastError: providerFailures.codexUsageLimit.message, lastErrorClass: "usage_limit" });
    snapshot.snapshotSequence = 3;
    snapshot.projection.runs.push({ ...run, id: "successor", ordinal: 2, rootNodeId: "successor-root",
      status: successorState, modelSelection: { instanceId: "other-provider", model: "other-model" }, providerInstanceId: "other-provider" });
    snapshot.projection.messages.push({ threadId: snapshot.projection.thread.id, id: "successor-answer", role: "assistant", runId: "successor", nodeId: "successor-root",
      text: "Successor response", streaming: false, attachments: [], createdAt: now, updatedAt: now });
    const expected = { turnId: run.id, source: "t3_v2_turn_item", category: "quota", code: "usageLimitExceeded",
      provider: "codex_openai", model: selection.model, retry };
    const row = reader === "list" ? (await gateway.threadsList({ includeArchived: false, detail: "full", limit: 5 })).page.items[0]
      : (await gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0];
    expect(row).toMatchObject({ activity: "failed", observedTurnId: run.id, latestTurn: { turnId: run.id, state: "error" }, failure: expected });
    expect(row).toMatchObject({ latestResponseExcerpt: null });
    expect((await makeGateway(config).gateway.runGet(sent.runId)).failure).toMatchObject(expected);
  });

  it.each([
    ["list", "completed"], ["list", "interrupted"],
    ["overview", "completed"], ["overview", "interrupted"],
  ] as const)("clears a historical recovered V2 failure when %s enrichment sees its %s record", async (reader, recoveredState) => {
    const { gateway, snapshot, shell, config } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "historical-enrichment-recovery" });
    const run = snapshot.projection.runs[0]!;
    run.status = "failed";
    snapshot.projection.turnItems.push({ id: "historical-error", type: "error", status: "failed", ordinal: 1,
      runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now, failure: providerFailures.codexUsageLimit });
    expect((await gateway.runGet(sent.runId)).failure?.code).toBe("usageLimitExceeded");
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: null, status: "failed",
      lastError: providerFailures.codexUsageLimit.message, lastErrorClass: "usage_limit" });
    snapshot.snapshotSequence = 3;
    run.status = recoveredState;
    snapshot.projection.runs.push({ ...run, id: "successor", ordinal: 2, rootNodeId: "successor-root", status: "running" });
    const row = reader === "list" ? (await gateway.threadsList({ includeArchived: false, detail: "full", limit: 5 })).page.items[0]
      : (await gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0];
    expect(row?.failure).toBeNull();
    const restarted = makeGateway(config);
    expect(await restarted.journal.getRecoveryByTurnId("thread-1", run.id)).toBe(recoveredState);
    expect(await restarted.gateway.runGet(sent.runId)).toMatchObject({ runStatus: recoveredState, failure: null });
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

  it.each(["anthropic.claude-3-5-sonnet-20240620-v1:0", "vendor.token:0", "azure.openai.api-key:deployment"])("preserves structured V2 model ID %s through shell enrichment and restart", async (model) => {
    const { gateway, snapshot, shell, config } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "dotted-model" });
    const run = snapshot.projection.runs[0]!;
    Object.assign(run, { status: "failed", providerInstanceId: "bedrock.instance", modelSelection: { model }, completedAt: now });
    snapshot.projection.turnItems.push({ id: "dotted", type: "error", ordinal: 1, status: "failed", runId: run.id,
      nodeId: run.rootNodeId, title: null, updatedAt: now, failure: providerFailures.codexUsageLimit });
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: null, status: "failed",
      lastError: providerFailures.codexUsageLimit.message, lastErrorClass: "usage_limit" });
    const expected = { model, provider: "bedrock.instance" };
    expect((await gateway.runGet(sent.runId)).failure).toMatchObject(expected);
    expect((await gateway.threadGet("thread-1")).thread.failure).toMatchObject(expected);
    expect((await gateway.threadsList({ includeArchived: false, detail: "full", limit: 5 })).page.items[0]?.failure).toMatchObject(expected);
    expect((await makeGateway(config).journal.getFailureByTurnId("thread-1", run.id))).toMatchObject(expected);
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

  it.each(["list", "overview"] as const)("preserves full V2 metadata across newer matching %s shell polls", async (reader) => {
    const { gateway, snapshot, shell, config } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "matching-shell-metadata" });
    const run = snapshot.projection.runs[0]!;
    run.status = "failed";
    const failure = providerFailures.codexUsageLimit;
    const retry = { attempt: 2, maxAttempts: 3, retryDelayMs: 500 };
    snapshot.projection.turnItems.push({ id: "metadata", type: "error", status: "failed", ordinal: 1,
      runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now, failure, retry });
    expect((await gateway.runGet(sent.runId)).failure).toMatchObject({ code: failure.code, retry });
    shell.snapshotSequence = 100;
    const resetAt = "2026-10-04T00:00:00.000Z";
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: null, status: "failed",
      lastError: failure.message, lastErrorClass: failure.class, usageLimitResetAt: resetAt });
    const expected = { source: "t3_v2_turn_item", category: "quota", message: failure.message,
      code: failure.code, retryable: failure.retryable, retry, resetAt };
    const observed = reader === "list" ? (await gateway.threadsList({ includeArchived: false, limit: 5 })).page.items[0]?.failure
      : (await gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0]?.failure;
    expect(observed).toMatchObject(expected);
    snapshot.snapshotSequence = 50;
    snapshot.projection.turnItems[0]!.failure = { class: failure.class, code: "api_error_429", message: "Different older error", retryable: true };
    expect((await gateway.runGet(sent.runId)).failure).toMatchObject(expected);
    expect((await makeGateway(config).gateway.runGet(sent.runId)).failure).toMatchObject(expected);
  });

  it.each(["get", "list", "overview"] as const)("does not preserve full V2 metadata across a shell redaction collision in %s", async (reader) => {
    const { gateway, snapshot, shell, config } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "shell-redaction-collision" });
    const run = snapshot.projection.runs[0]!;
    run.status = "failed";
    const fullFailure = { ...providerFailures.codexUsageLimit, message: "Provider failed at https://one.example/private" };
    snapshot.projection.turnItems.push({ id: "collision", type: "error", status: "failed", ordinal: 1,
      runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now, failure: fullFailure,
      retry: { attempt: 2, maxAttempts: 3, retryDelayMs: 500 } });
    await gateway.runGet(sent.runId);
    shell.snapshotSequence = 3;
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: null, status: "failed",
      lastError: "Provider failed at https://two.example/private", lastErrorClass: fullFailure.class,
      usageLimitResetAt: null, updatedAt: "2026-10-02T20:00:01.000Z" });
    const observed = reader === "get" ? (await gateway.threadGet("thread-1")).thread.failure
      : reader === "list" ? (await gateway.threadsList({ includeArchived: false, limit: 5 })).page.items[0]?.failure
      : (await gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0]?.failure;
    expect(observed).toMatchObject({ source: "t3_session", message: "Provider failed at [REDACTED URL]",
      code: null, retry: null, resetAt: null });
    expect((await makeGateway(config).gateway.runGet(sent.runId)).failure).toMatchObject({
      source: "t3_session", message: "Provider failed at [REDACTED URL]", code: null, retry: null, resetAt: null,
    });
  });

  it("uses matching weaker shell admission to reject a delayed other-protocol reason without a rewrite", async () => {
    const { gateway, snapshot, journal, config } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "shell-admission" });
    const run = snapshot.projection.runs[0]!;
    run.status = "failed";
    snapshot.projection.turnItems.push({ id: "admission", type: "error", status: "failed", ordinal: 1,
      runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now, failure: providerFailures.codexUsageLimit });
    const fullFailure = (await gateway.runGet(sent.runId)).failure!;
    const failureIdentity = createHash("sha256").update(providerFailures.codexUsageLimit.message).digest("hex");
    const admission = Date.now() + 10_000;
    await journal.retainTerminalFailure("thread-1", run.id, fullFailure, undefined,
      { protocolVersion: 2, scope: "full", snapshotSequence: 10, readStartedAt: admission, failureIdentity });
    const path = join(config.dataDir, "operations.json");
    const before = await readFile(path, "utf8");
    expect(await journal.retainTerminalFailure("thread-1", run.id,
      { ...fullFailure, source: "t3_session", code: null, retry: null, retryable: null }, undefined,
      { protocolVersion: 2, scope: "shell", snapshotSequence: 10, readStartedAt: admission + 200, failureIdentity })).toEqual(fullFailure);
    expect(await journal.retainTerminalFailure("thread-1", run.id,
      { ...fullFailure, source: "t3_session", message: "Delayed old-protocol failure", code: null }, undefined,
      { protocolVersion: 1, scope: "full", snapshotSequence: 100, readStartedAt: admission + 100 })).toEqual(fullFailure);
    expect(await readFile(path, "utf8")).toBe(before);
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

  it.each(["reason", "reset"])("accepts same-sequence newer shell %s evidence", async (change) => {
    const { gateway, snapshot, shell, config } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "same-sequence-shell" });
    const run = snapshot.projection.runs[0]!;
    run.status = "failed";
    const failure = { ...providerFailures.codexUsageLimit, resetAt: null };
    snapshot.projection.turnItems.push({ id: "first", type: "error", status: "failed", ordinal: 1,
      runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now, failure });
    expect((await gateway.runGet(sent.runId)).failure?.resetAt).toBeNull();
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: null, status: "failed",
      lastError: change === "reason" ? "Changed bound reason" : failure.message,
      lastErrorClass: change === "reason" ? "provider_error" : failure.class,
      usageLimitResetAt: change === "reason" ? null : providerFailures.codexUsageLimit.resetAt,
      updatedAt: "2026-10-02T20:00:01.000Z" });
    const expected = change === "reason" ? { message: "Changed bound reason", category: "provider_error", code: null, resetAt: null }
      : { message: failure.message, code: failure.code, resetAt: providerFailures.codexUsageLimit.resetAt };
    expect((await gateway.threadGet("thread-1")).thread.failure).toMatchObject(expected);
    expect((await makeGateway(config).gateway.runGet(sent.runId)).failure).toMatchObject(expected);
  });

  it.each(["run", "thread", "overview"])("rejects a delayed failed %s read after ordered recovery and restart", async (reader) => {
    const { gateway, client, snapshot, shell, config } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "recovery-race" });
    const run = snapshot.projection.runs[0]!;
    run.status = "failed";
    snapshot.projection.turnItems.push({ id: "first", type: "error", status: "failed", ordinal: 1,
      runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now, failure: providerFailures.codexUsageLimit });
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: null, status: "failed",
      lastError: providerFailures.codexUsageLimit.message, lastErrorClass: "usage_limit" });
    const older = await client.getThread("thread-1");
    let release!: () => void;
    let entered!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    const started = new Promise<void>((resolve) => { entered = resolve; });
    const getThread = client.getThread.bind(client);
    let calls = 0;
    client.getThread = async (id) => { if (++calls === 1) { entered(); await gate; return older; } return getThread(id); };
    const delayed = reader === "run" ? gateway.runGet(sent.runId).then((result) => result.failure)
      : reader === "thread" ? gateway.threadGet("thread-1").then((result) => result.thread.failure)
      : gateway.threadsOverview({ includeArchived: false, runningLimit: 5 }).then((result) => result.highlights[0]?.failure);
    await started;
    run.status = "completed";
    run.completedAt = "2026-10-02T20:00:01.000Z";
    snapshot.snapshotSequence = 3;
    shell.snapshotSequence = 3;
    Object.assign(shell.threads[0]!, { status: "completed", lastError: null, lastErrorClass: null });
    expect(await gateway.runGet(sent.runId)).toMatchObject({ runStatus: "completed", failure: null });
    release();
    expect(await delayed).toBeNull();
    const restarted = makeGateway(config);
    restarted.client.getThread = async () => older;
    expect(await restarted.gateway.runGet(sent.runId)).toMatchObject({ runStatus: "completed", failure: null });
    expect((await restarted.gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0]?.failure).toBeNull();
  });

  it.each(["list", "overview"] as const)("does not persist an older failed full turn behind a running %s shell", async (reader) => {
    const { gateway, client, snapshot, shell, config } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "running-shell-order" });
    const run = snapshot.projection.runs[0]!;
    run.status = "failed";
    snapshot.projection.turnItems.push({ id: "stale-root", type: "error", status: "failed", ordinal: 1,
      runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now, failure: providerFailures.codexUsageLimit });
    const older = await client.getThread("thread-1");
    shell.snapshotSequence = 3;
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: run.id, status: "running", lastError: null, lastErrorClass: null });
    const getThread = client.getThread.bind(client);
    client.getThread = async () => older;
    const observed = reader === "list" ? (await gateway.threadsList({ includeArchived: false, detail: "full", limit: 5 })).page.items[0]
      : (await gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0];
    expect(observed).toMatchObject({ latestTurn: { turnId: run.id, state: "running" }, activity: "running", failure: null });
    expect(await makeGateway(config).journal.getFailureByTurnId("thread-1", run.id)).toBeNull();
    run.status = "running";
    snapshot.snapshotSequence = 3;
    client.getThread = getThread;
    expect(await gateway.runGet(sent.runId)).toMatchObject({ runStatus: "running", failure: null });
    expect(await makeGateway(config).gateway.runGet(sent.runId)).toMatchObject({ runStatus: "running", failure: null });
  });

  it.each(["completed", "interrupted"] as const)("uses a newer V2 shell error over stale full %s", async (state) => {
    const { gateway, snapshot, shell, config } = await setup();
    await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "shell-state-order" });
    const run = snapshot.projection.runs[0]!;
    run.status = state;
    shell.snapshotSequence = 3;
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: null, status: "failed",
      lastError: "Current shell error", lastErrorClass: "provider_error" });
    const expected = { turnId: run.id, source: "t3_session", message: "Current shell error", category: "provider_error" };
    expect((await gateway.threadGet("thread-1")).thread).toMatchObject({ latestTurn: { state: "error" }, activity: "failed", failure: expected });
    expect(await makeGateway(config).journal.getFailureByTurnId("thread-1", run.id)).toMatchObject(expected);
  });

  it.each([
    { model: selection.model, provider: "codex" },
    { model: selection.model, instanceId: "other-model-instance" },
    { model: selection.model },
  ])("uses the matching V2 shell provider instance with model selection %j", async (modelSelection) => {
    const { gateway, snapshot, shell, config } = await setup();
    await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "shell-provider-instance" });
    const run = snapshot.projection.runs[0]!;
    run.status = "failed";
    run.providerInstanceId = "runtime-provider";
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: null, status: "failed",
      providerInstanceId: "runtime-provider", modelSelection, lastError: "Runtime provider failed", lastErrorClass: "provider_error" });
    const expected = { source: "t3_session", turnId: run.id, provider: "runtime-provider", model: selection.model };
    expect((await gateway.threadsList({ includeArchived: false, detail: "summary", limit: 5 })).page.items[0]?.failure).toMatchObject(expected);
    expect(await makeGateway(config).journal.getFailureByTurnId("thread-1", run.id)).toMatchObject(expected);
  });

  it.each([
    ["list", "completed"], ["list", "interrupted"],
    ["overview", "completed"], ["overview", "interrupted"],
  ] as const)("returns a new full failure after %s accepts the shell's %s recovery", async (reader, state) => {
    const { gateway, snapshot, shell, config } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "first-enriched-failure" });
    const run = snapshot.projection.runs[0]!;
    run.status = "failed";
    snapshot.snapshotSequence = 3;
    snapshot.projection.turnItems.push({ id: "new-root", type: "error", status: "failed", ordinal: 1,
      runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now, failure: providerFailures.codexUsageLimit });
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: null, status: state, lastError: null, lastErrorClass: null });
    const observed = reader === "list" ? (await gateway.threadsList({ includeArchived: false, detail: "full", limit: 5 })).page.items[0]
      : (await gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0];
    expect(observed).toMatchObject({ latestTurn: { turnId: run.id, state }, activity: "idle", latestResponseExcerpt: null,
      failure: { source: "t3_v2_turn_item", turnId: run.id, code: "usageLimitExceeded" } });
    expect((await makeGateway(config).gateway.runGet(sent.runId)).failure).toMatchObject({ turnId: run.id, code: "usageLimitExceeded" });
  });

  it.each([
    ["summary", "completed"], ["summary", "interrupted"],
    ["full", "completed"], ["full", "interrupted"],
    ["overview", "completed"], ["overview", "interrupted"],
    ["thread", "completed"], ["thread", "interrupted"],
  ] as const)("retains a newer failure when %s rejects stale %s recovery", async (reader, state) => {
    const { gateway, client, snapshot, shell, config } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "summary-recovery-order" });
    const run = snapshot.projection.runs[0]!;
    run.status = state;
    snapshot.snapshotSequence = 3;
    snapshot.projection.messages.push({ threadId: snapshot.projection.thread.id, id: "stale-success", role: "assistant", runId: run.id, nodeId: run.rootNodeId,
      text: "Stale successful response", streaming: false, attachments: [], createdAt: now, updatedAt: now });
    const recovery = await client.getThread("thread-1");
    run.status = "failed";
    snapshot.snapshotSequence = 4;
    snapshot.projection.turnItems.push({ id: "latest-root", type: "error", status: "failed", ordinal: 1,
      runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now, failure: providerFailures.codexUsageLimit });
    expect((await gateway.runGet(sent.runId)).failure?.code).toBe("usageLimitExceeded");
    shell.snapshotSequence = 3;
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: null, status: state, lastError: null, lastErrorClass: null });
    client.getThread = async () => recovery;
    const observed = reader === "thread" ? (await gateway.threadGet("thread-1")).thread
      : reader === "overview" ? (await gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0]
      : (await gateway.threadsList({ includeArchived: false, detail: reader, limit: 5 })).page.items[0];
    expect(observed).toMatchObject({ latestTurn: { turnId: run.id, state }, activity: "idle",
      failure: { turnId: run.id, source: "t3_v2_turn_item", code: "usageLimitExceeded" } });
    if (reader === "thread") expect(observed).toMatchObject({ latestResponse: null });
    if (reader === "full" || reader === "overview") expect(observed).toMatchObject({ latestResponseExcerpt: null });
    const restarted = makeGateway(config);
    restarted.client.getThread = async () => recovery;
    expect(await restarted.gateway.runGet(sent.runId)).toMatchObject({ runStatus: "failed", failure: { code: "usageLimitExceeded" } });
  });

  it.each([
    ["shell", "running"], ["shell", "completed"], ["shell", "failed"],
    ["full", "running"], ["full", "completed"], ["full", "failed"],
  ] as const)("selects the newer %s V2 turn over fallback request times with a %s shell", async (newer, state) => {
    const { gateway, snapshot, shell } = await setup();
    await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "different-turn-order" });
    const run = snapshot.projection.runs[0]!;
    run.status = "failed";
    snapshot.projection.thread.createdAt = "2026-01-01T00:00:00.000Z";
    snapshot.projection.turnItems.push({ id: "old-error", type: "error", status: "failed", ordinal: 1,
      runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now, failure: providerFailures.codexUsageLimit });
    snapshot.projection.messages.push({ threadId: snapshot.projection.thread.id, id: "old-answer", role: "assistant", runId: run.id, nodeId: run.rootNodeId,
      text: "Old turn output", streaming: false, attachments: [], createdAt: now, updatedAt: now });
    snapshot.snapshotSequence = newer === "full" ? 4 : 3;
    shell.snapshotSequence = newer === "shell" ? 4 : 3;
    Object.assign(shell.threads[0]!, { createdAt: snapshot.projection.thread.createdAt, latestRunId: "successor",
      activeRunId: state === "running" ? "successor" : null, status: state,
      lastError: state === "failed" ? "Successor error" : null, lastErrorClass: state === "failed" ? "provider_error" : null });
    if (newer === "full") shell.threads[0]!.latestRunRequestedAt = "2026-10-02T21:00:00.000Z";
    const detail = (await gateway.threadGet("thread-1")).thread;
    if (newer === "shell") {
      expect(detail.latestTurn?.turnId).toBe("successor");
      expect(detail.latestResponse).toBeNull();
      if (state === "failed") expect(detail.failure).toMatchObject({ turnId: "successor", message: "Successor error" });
      else expect(detail.failure).toBeNull();
    } else {
      expect(detail).toMatchObject({ latestTurn: { turnId: run.id, state: "error" }, failure: { turnId: run.id, code: "usageLimitExceeded" } });
    }
  });

  it.each([
    ["list", "provider_error"], ["list", "usage_limit"],
    ["overview", "provider_error"], ["overview", "usage_limit"],
    ["thread", "provider_error"], ["thread", "usage_limit"],
  ] as const)("retains textless bound V2 %s shell metadata with class %s", async (reader, errorClass) => {
    const { gateway, snapshot, shell, config } = await setup();
    await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "textless-shell" });
    const run = snapshot.projection.runs[0]!;
    run.status = "failed";
    shell.snapshotSequence = 3;
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: null, status: "failed", lastError: null,
      lastErrorClass: errorClass, usageLimitResetAt: "2026-10-04T00:00:00Z" });
    const observed = reader === "thread" ? (await gateway.threadGet("thread-1")).thread
      : reader === "overview" ? (await gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0]
      : (await gateway.threadsList({ includeArchived: false, detail: "summary", limit: 5 })).page.items[0];
    const expected = { source: "t3_session", turnId: run.id, class: errorClass,
      category: errorClass === "provider_error" ? "provider_error" : "unknown", code: null,
      resetAt: "2026-10-04T00:00:00.000Z", message: "T3 reported that the provider turn failed without an error message." };
    expect(observed?.failure).toMatchObject(expected);
    expect(await makeGateway(config).journal.getFailureByTurnId("thread-1", run.id)).toMatchObject(expected);
  });

  it.each(["completed", "interrupted"] as const)("keeps a newer failure when a bounded wait poll sees stale %s", async (state) => {
    const { gateway, client, snapshot, config } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "wait-stale-recovery" });
    const run = snapshot.projection.runs[0]!;
    const running = await client.getThread("thread-1");
    run.status = state;
    const recovery = await client.getThread("thread-1");
    let release!: () => void;
    let entered!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    const started = new Promise<void>((resolve) => { entered = resolve; });
    const getThread = client.getThread.bind(client);
    let calls = 0;
    client.getThread = async (id) => {
      if (++calls === 1) return running;
      if (calls === 2) { entered(); await gate; return recovery; }
      return getThread(id);
    };
    const waiting = gateway.runWait(sent.runId, 2);
    await started;
    run.status = "failed";
    snapshot.snapshotSequence = 3;
    snapshot.projection.turnItems.push({ id: "new-failure", type: "error", status: "failed", ordinal: 1,
      runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now, failure: providerFailures.codexUsageLimit });
    expect((await gateway.runGet(sent.runId)).runStatus).toBe("failed");
    release();
    const expected = { runStatus: "failed", failure: { source: "t3_v2_turn_item", code: "usageLimitExceeded" } };
    expect(await waiting).toMatchObject(expected);
    const restarted = makeGateway(config);
    restarted.client.getThread = async () => recovery;
    expect(await restarted.gateway.runGet(sent.runId)).toMatchObject(expected);
  });

  it.each([
    ["list", "completed"], ["list", "interrupted"],
    ["overview", "completed"], ["overview", "interrupted"],
  ] as const)("keeps a newer shell failure through stale %s enrichment of %s", async (reader, state) => {
    const { gateway, client, snapshot, shell, config } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "stale-enrichment-recovery" });
    const run = snapshot.projection.runs[0]!;
    run.status = state;
    const recovery = await client.getThread("thread-1");
    shell.snapshotSequence = 3;
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: null, status: "failed",
      lastError: providerFailures.codexUsageLimit.message, lastErrorClass: "usage_limit" });
    const observed = reader === "list" ? (await gateway.threadsList({ includeArchived: false, detail: "full", limit: 5 })).page.items[0]
      : (await gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0];
    expect(observed).toMatchObject({ activity: "failed", latestTurn: { state: "error" },
      failure: { source: "t3_session", category: "unknown", message: providerFailures.codexUsageLimit.message } });
    const restarted = makeGateway(config);
    restarted.client.getThread = async () => recovery;
    expect((await restarted.gateway.runGet(sent.runId)).failure).toMatchObject({ source: "t3_session", category: "unknown" });
  });

  it.each(["list", "overview"] as const)("keeps a newer retained failure through %s enrichment newer than the shell but older than the journal", async (reader) => {
    const { gateway, client, snapshot, shell } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "journal-enrichment-order" });
    const run = snapshot.projection.runs[0]!;
    run.status = "completed";
    snapshot.snapshotSequence = 3;
    const recovery = await client.getThread("thread-1");
    run.status = "failed";
    snapshot.snapshotSequence = 4;
    snapshot.projection.turnItems.push({ id: "latest-root", type: "error", status: "failed", ordinal: 1,
      runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now, failure: providerFailures.codexUsageLimit });
    expect((await gateway.runGet(sent.runId)).failure?.code).toBe("usageLimitExceeded");
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: null, status: "failed",
      lastError: providerFailures.codexUsageLimit.message, lastErrorClass: "usage_limit" });
    client.getThread = async () => recovery;
    const observed = reader === "list" ? (await gateway.threadsList({ includeArchived: false, detail: "full", limit: 5 })).page.items[0]
      : (await gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0];
    expect(observed?.failure).toMatchObject({ source: "t3_v2_turn_item", code: "usageLimitExceeded", category: "quota" });
  });

  it.each([
    ["completed", "completed"], ["interrupted", "interrupted"],
    ["cancelled", "interrupted"], ["rolled_back", "interrupted"],
  ] as const)("clears a historical V2 failure corrected to %s", async (state, expectedState) => {
    const { gateway, client, snapshot, shell, config } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "historical-recovery" });
    const run = snapshot.projection.runs[0]!;
    run.status = "failed";
    snapshot.projection.turnItems.push({ id: "first", type: "error", status: "failed", ordinal: 1,
      runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now, failure: providerFailures.codexUsageLimit });
    const older = await client.getThread("thread-1");
    expect((await gateway.runGet(sent.runId)).failure?.category).toBe("quota");
    run.status = state;
    run.completedAt = "2026-10-02T20:00:01.000Z";
    snapshot.snapshotSequence = 3;
    snapshot.projection.runs.push({ ...run, id: "successor", rootNodeId: "successor-root", ordinal: 2, status: "running",
      requestedAt: "2026-10-02T20:00:02.000Z", startedAt: "2026-10-02T20:00:02.000Z", completedAt: null });
    Object.assign(shell.threads[0]!, { latestRunId: "successor", activeRunId: "successor", status: "running", lastError: null, lastErrorClass: null });
    expect(await gateway.runGet(sent.runId)).toMatchObject({ runStatus: expectedState, failure: null, t3TurnId: run.id });
    expect(await gateway.runWait(sent.runId, 0.1)).toMatchObject({ runStatus: expectedState, failure: null });
    const restarted = makeGateway(config);
    restarted.client.getThread = async () => older;
    expect(await restarted.gateway.runGet(sent.runId)).toMatchObject({ runStatus: expectedState, failure: null });
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

  it.each(["get", "list", "overview"] as const)("keeps a retained V2 root reason through an unbound %s shell error", async (reader) => {
    const { gateway, snapshot, shell, config } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "unbound-shell-retention" });
    const run = snapshot.projection.runs[0]!;
    run.status = "failed";
    const failure = providerFailures.codexUsageLimit;
    const retry = { attempt: 2, maxAttempts: 3, retryDelayMs: 500 };
    snapshot.projection.turnItems.push({ id: "root-reason", type: "error", status: "failed", ordinal: 1,
      runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now, failure, retry });
    const expected = { category: "quota", source: "t3_v2_turn_item", code: failure.code, retry,
      message: failure.message, resetAt: failure.resetAt };
    expect((await gateway.runGet(sent.runId)).failure).toMatchObject(expected);
    shell.snapshotSequence = 100;
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: null, status: "failed",
      lastError: "Unrelated session error", lastErrorClass: null, usageLimitResetAt: null });
    const observed = reader === "get" ? (await gateway.threadGet("thread-1")).thread.failure
      : reader === "list" ? (await gateway.threadsList({ includeArchived: false, detail: "summary", limit: 5 })).page.items[0]?.failure
      : (await gateway.threadsOverview({ includeArchived: false, runningLimit: 5 })).highlights[0]?.failure;
    expect(observed).toMatchObject(expected);
    snapshot.snapshotSequence = 50;
    snapshot.projection.turnItems = [];
    expect((await makeGateway(config).gateway.runGet(sent.runId)).failure).toMatchObject(expected);
    // A newer authoritative full absence can still invalidate the old root reason.
    snapshot.snapshotSequence = 101;
    expect((await gateway.runGet(sent.runId)).failure).toMatchObject({ source: "t3_turn", code: null, retry: null });
  });

  it("keeps available full V2 root evidence when the first shell reason is unbound", async () => {
    const { gateway, snapshot, shell } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "fresh-unbound-shell" });
    const run = snapshot.projection.runs[0]!;
    run.status = "failed";
    snapshot.projection.turnItems.push({ id: "root-reason", type: "error", status: "failed", ordinal: 1,
      runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now, failure: providerFailures.codexUsageLimit });
    shell.snapshotSequence = 100;
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: null, status: "failed",
      lastError: "Unrelated session error", lastErrorClass: null, usageLimitResetAt: null });
    expect((await gateway.threadGet("thread-1")).thread.failure).toMatchObject({ source: "t3_v2_turn_item", code: "usageLimitExceeded" });
    expect((await gateway.runGet(sent.runId)).failure).toMatchObject({ source: "t3_v2_turn_item", code: "usageLimitExceeded" });
  });

  it.each([["rate_limit_error", "provider_error", "rate_limit"], ["api_error_429", "usage_limit", "unknown"]])("uses explicit JSON subtype %s despite a generic V2 root code", async (type, errorClass, category) => {
    const { gateway, snapshot, shell, config } = await setup();
    const sent = await gateway.threadSend({ threadId: "thread-1", message: "Work", idempotencyKey: "generic-root-code" });
    const run = snapshot.projection.runs[0]!;
    run.status = "failed";
    snapshot.projection.turnItems.push({ id: "json-reason", type: "error", status: "failed", ordinal: 1,
      runId: run.id, nodeId: run.rootNodeId, title: null, updatedAt: now,
      failure: { class: errorClass, code: "api_error", message: JSON.stringify({ error: { type } }) } });
    Object.assign(shell.threads[0]!, { latestRunId: run.id, activeRunId: null, status: "failed", lastError: null, lastErrorClass: null });
    const expected = { source: "t3_v2_turn_item", category, code: type };
    expect((await gateway.runGet(sent.runId)).failure).toMatchObject(expected);
    expect((await gateway.threadGet("thread-1")).thread.failure).toMatchObject(expected);
    expect((await makeGateway(config).gateway.runGet(sent.runId)).failure).toMatchObject(expected);
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
    const { baseUrl, commands, setProtocol } = await setup();
    const client = new T3HttpClient(baseUrl, "test-token");
    await client.getDescriptor();
    setProtocol(3);
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
