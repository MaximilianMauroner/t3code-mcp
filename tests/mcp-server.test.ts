import { execFile } from "node:child_process";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { afterEach, describe, expect, it } from "vitest";
import { createMcpServer } from "../src/mcp/server.js";
import { gatewayFixture, type GatewayFixture } from "./support/gateway-fixture.js";
import { FakeT3 } from "./support/fake-t3.js";

const fakes: FakeT3[] = [];
const fixtures: GatewayFixture[] = [];
const clients: Client[] = [];
const servers: Array<{ close: () => Promise<void> }> = [];
const directories: string[] = [];
const execute = promisify(execFile);

afterEach(async () => {
  await Promise.all(clients.splice(0).map((client) => client.close().catch(() => undefined)));
  await Promise.all(servers.splice(0).map((server) => server.close().catch(() => undefined)));
  await Promise.all(fixtures.splice(0).map((fixture) => fixture.cleanup()));
  await Promise.all(fakes.splice(0).map((fake) => fake.close()));
  await Promise.all(directories.splice(0).map((directory) => rm(directory, { recursive: true, force: true })));
});

async function connectedClient(): Promise<{ readonly fake: FakeT3; readonly client: Client; readonly fixture: GatewayFixture }> {
  const fake = new FakeT3();
  fakes.push(fake);
  await fake.start();
  const fixture = await gatewayFixture(fake);
  fixtures.push(fixture);
  const server = createMcpServer(fixture.gateway);
  const client = new Client({ name: "mcp-test-client", version: "1.0.0" }, { capabilities: {} });
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await server.connect(serverTransport);
  await client.connect(clientTransport);
  clients.push(client);
  servers.push(server);
  return { fake, client, fixture };
}

describe("MCP tool contract", () => {
  it("publishes only the task-specific gateway tools", async () => {
    const { client } = await connectedClient();
    const result = await client.listTools();
    const names = result.tools.map((tool) => tool.name).sort();

    expect(names).toEqual([
      "t3_attachment_upload",
      "t3_audit_log",
      "t3_connection_status",
      "t3_git_compare",
      "t3_git_diff",
      "t3_git_status",
      "t3_pending_action_respond",
      "t3_pending_actions_list",
      "t3_result_get",
      "t3_task_cancel",
      "t3_task_get",
      "t3_task_start",
      "t3_tasks_list",
      "t3_threads_list",
      "t3_threads_overview",
    ]);
    expect(names).not.toContain("t3_call_rpc");
    expect(names).not.toContain("t3_terminal_write");
  });

  it("returns structured content for successful reads", async () => {
    const { fake, client } = await connectedClient();
    fake.addThread({ id: "thread-mcp", title: "MCP thread" });

    const result = await client.callTool({ name: "t3_threads_list", arguments: { limit: 10 } });

    expect(result.isError).not.toBe(true);
    expect(result.structuredContent).toMatchObject({
      environmentId: fake.environmentId,
      page: { items: [{ id: "thread-mcp", title: "MCP thread" }] },
    });
    expect(result.content).toBeTruthy();
  });

  it("records tool usage and exposes the bounded audit trail", async () => {
    const { fake, client, fixture } = await connectedClient();
    fake.addProject({ id: "project-audit", title: "Audit project" });
    fake.addThread({ id: "thread-audit", projectId: "project-audit", title: "Audit thread" });

    const result = await client.callTool({
      name: "t3_threads_list",
      arguments: { query: "Audit", limit: 10 },
    });
    expect(result.isError).not.toBe(true);

    const sent = await client.callTool({
      name: "t3_task_start",
      arguments: {
        projectId: "project-audit",
        title: "Audit task",
        instruction: "private audit prompt should not be persisted",
        runtimeMode: "approval-required",
        idempotencyKey: "audit-send",
      },
    });
    expect(sent.isError).not.toBe(true);

    const raw = await readFile(`${fixture.config.dataDir}/audit.jsonl`, "utf8");
    expect(raw).toContain('"event":"tool.call"');
    expect(raw).toContain('"operation":"t3_threads_list"');
    expect(raw).toContain('"event":"upstream.request"');
    expect(raw).not.toContain(fake.accessToken);
    expect(raw).not.toContain("private audit prompt should not be persisted");

    const reviewed = await client.callTool({
      name: "t3_audit_log",
      arguments: { source: "mcp", operation: "t3_threads_list", limit: 10 },
    });
    expect(reviewed.isError).not.toBe(true);
    expect(reviewed.structuredContent).toMatchObject({
      page: { total: 2, items: expect.arrayContaining([
        expect.objectContaining({ event: "tool.call", operation: "t3_threads_list" }),
        expect.objectContaining({ event: "tool.result", operation: "t3_threads_list" }),
      ]) },
    });
  });

  it("starts and recovers a composite task through MCP", async () => {
    const { fake, client } = await connectedClient();
    fake.addProject({ id: "project-task-mcp", title: "Task MCP project" });
    const started = await client.callTool({
      name: "t3_task_start",
      arguments: {
        projectId: "project-task-mcp",
        title: "MCP task",
        instruction: "Report status only.",
        runtimeMode: "approval-required",
        idempotencyKey: "mcp-task-key",
      },
    });
    expect(started.isError).not.toBe(true);
    expect(started.structuredContent).toMatchObject({ stage: "run_accepted", title: "MCP task" });
    const taskRef = (started.structuredContent as { taskRef: string }).taskRef;
    const recovered = await client.callTool({ name: "t3_task_get", arguments: { taskRef } });
    expect(recovered.isError).not.toBe(true);
    expect(recovered.structuredContent).toMatchObject({ task: { taskRef, run: { runStatus: "running" } } });
    const result = await client.callTool({ name: "t3_result_get", arguments: { taskRef } });
    expect(result.isError).not.toBe(true);
    expect(result.structuredContent).toMatchObject({
      task: { taskRef },
      evidence: { taskStateSource: "t3_observed", git: null },
      limitations: [expect.stringContaining("baseline")],
    });
  });

  it("accepts worktree mode with explicit origin selection through MCP", async () => {
    const { fake, client } = await connectedClient();
    const repository = await createRepositoryWithOrigin();
    fake.addProject({ id: "project-worktree-mcp", workspaceRoot: repository });
    const started = await client.callTool({
      name: "t3_task_start",
      arguments: {
        projectId: "project-worktree-mcp",
        title: "Origin worktree task",
        instruction: "Inspect the origin version.",
        runtimeMode: "approval-required",
        workspaceMode: "worktree",
        branch: "main",
        startFromOrigin: true,
        idempotencyKey: "mcp-worktree-task-key",
      },
    });

    expect(started.isError).not.toBe(true);
    expect(fake.dispatches[0]?.command).toMatchObject({
      type: "thread.create",
      projectId: "project-worktree-mcp",
      branch: expect.stringMatching(/^t3code\/mcp-[0-9a-f]+$/),
      worktreePath: expect.stringContaining(".t3-code-mcp-worktrees"),
    });
    expect(fake.dispatches[1]?.command).toMatchObject({
      type: "thread.turn.start",
    });
  });

  it("finds open threads and summarizes them through MCP", async () => {
    const { fake, client } = await connectedClient();
    fake.addThread({ id: "voice-thread", projectId: "voice-project", title: "Login fix",
      latestTurn: { turnId: "voice-turn", state: "running", requestedAt: "2026-09-10T12:00:00Z" },
      session: { status: "running" },
    });
    fake.addThread({ id: "settled-thread", title: "Login fix", settledOverride: "settled" });
    const found = await client.callTool({ name: "t3_threads_list", arguments: { query: "LOGIN", status: "open", projectId: "voice-project" } });
    expect(found.isError).not.toBe(true);
    expect(found.structuredContent).toMatchObject({ page: { total: 1, items: [{ id: "voice-thread", status: "open" }] } });
    const runningOnly = await client.callTool({ name: "t3_threads_list", arguments: { onlyRunning: true } });
    expect(runningOnly.isError).not.toBe(true);
    expect(runningOnly.structuredContent).toMatchObject({ page: { total: 1, items: [{ id: "voice-thread", isRunning: true }] } });
    const overview = await client.callTool({ name: "t3_threads_overview", arguments: {} });
    expect(overview.isError).not.toBe(true);
    expect(overview.structuredContent).toMatchObject({
      total: 2,
      counts: { open: 1, settled: 1 },
      runningCount: 1,
    });
    const invalid = await client.callTool({ name: "t3_threads_list", arguments: { status: "running" } });
    expect(invalid.isError).toBe(true);
  });

  it("turns gateway failures into stable structured MCP errors", async () => {
    const { client } = await connectedClient();

    const result = await client.callTool({ name: "t3_task_get", arguments: { taskRef: "task_missing" } });

    expect(result.isError).toBe(true);
    expect(result.structuredContent).toMatchObject({ error: { code: "task_not_found" } });
    expect(result.content).toBeTruthy();
  });

  it("rejects invalid tool arguments at the protocol boundary", async () => {
    const { fake, client } = await connectedClient();
    fake.addProject({ id: "project-boundary" });

    const missingKey = await client.callTool({ name: "t3_pending_action_respond", arguments: {
      threadId: "thread-1",
      requestId: "request-1",
      kind: "approval",
      decision: "accept",
    } });
    // A task never creates an empty thread: the initial instruction is required.
    const missingInstruction = await client.callTool({ name: "t3_task_start", arguments: {
      projectId: "project-boundary",
      title: "No empty threads",
      runtimeMode: "approval-required",
      idempotencyKey: "missing-instruction",
    } });

    const badLimit = await client.callTool({ name: "t3_threads_list", arguments: { limit: 0 } });

    expect(missingKey).toMatchObject({ isError: true });
    expect(missingInstruction).toMatchObject({ isError: true });
    expect(badLimit).toMatchObject({ isError: true });
    expect(fake.dispatches).toHaveLength(0);
  });

  it("enforces the read-only boundary through MCP, not only direct calls", async () => {
    const fake = new FakeT3();
    fakes.push(fake);
    await fake.start();
    const fixture = await gatewayFixture(fake, { readOnly: true });
    fixtures.push(fixture);
    const server = createMcpServer(fixture.gateway);
    const client = new Client({ name: "read-only-test-client", version: "1.0.0" }, { capabilities: {} });
    const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
    await server.connect(serverTransport);
    await client.connect(clientTransport);
    clients.push(client);
    servers.push(server);

    fake.addProject({ id: "project-read-only" });
    const result = await client.callTool({ name: "t3_task_start", arguments: {
      projectId: "project-read-only",
      title: "Read-only task",
      instruction: "must not dispatch",
      runtimeMode: "approval-required",
      idempotencyKey: "read-only-mcp",
    } });

    expect(result.isError).toBe(true);
    expect(result.structuredContent).toMatchObject({ error: { code: "gateway_read_only" } });
    const response = await client.callTool({ name: "t3_pending_action_respond", arguments: {
      threadId: "thread-1", requestId: "request-1", kind: "approval", decision: "decline", idempotencyKey: "read-only-response",
    } });
    expect(response.structuredContent).toMatchObject({ error: { code: "gateway_read_only" } });
    expect(fake.dispatches).toHaveLength(0);
  });
});

async function createRepositoryWithOrigin(): Promise<string> {
  const parent = await mkdtemp(join(tmpdir(), "t3-mcp-worktree-repository-"));
  directories.push(parent);
  const origin = join(parent, "origin.git");
  const repository = join(parent, "workspace");
  await execute("git", ["init", "--bare", origin]);
  await execute("git", ["clone", origin, repository]);
  await execute("git", ["config", "user.name", "T3 Test"], { cwd: repository });
  await execute("git", ["config", "user.email", "t3@example.test"], { cwd: repository });
  await execute("git", ["checkout", "-b", "main"], { cwd: repository });
  await writeFile(join(repository, "file.txt"), "initial\n");
  await execute("git", ["add", "."], { cwd: repository });
  await execute("git", ["commit", "-m", "initial"], { cwd: repository });
  await execute("git", ["push", "-u", "origin", "main"], { cwd: repository });
  return repository;
}

describe("MCP monitoring and launch guidance", () => {
  it("tracks a launch through pending input and completion without duplicate dispatch", async () => {
    const { fake, client } = await connectedClient();
    fake.addProject({ id: "journey", workspaceRoot: "/missing-journey-workspace" });
    const args = { projectId: "journey", title: "Repair", instruction: "Repair the owned defect", runtimeMode: "full-access", idempotencyKey: "journey-launch" };
    const started = await client.callTool({ name: "t3_task_start", arguments: args });
    expect(started.isError).not.toBe(true);
    const task = started.structuredContent as { taskRef: string; threadId: string; settings: { modelSource: string } };
    expect(task.settings.modelSource).toBe("project_default");
    const thread = fake.thread(task.threadId);
    thread.hasPendingUserInput = true;
    const pending = await client.callTool({ name: "t3_task_get", arguments: { taskRef: task.taskRef } });
    expect(pending.structuredContent).toMatchObject({ task: { run: { pendingActions: { userInput: true } } } });
    thread.hasPendingUserInput = false;
    thread.latestTurn = { ...thread.latestTurn!, state: "completed" };
    thread.session = { status: "stopped" };
    const done = await client.callTool({ name: "t3_task_get", arguments: { taskRef: task.taskRef } });
    expect(done.structuredContent).toMatchObject({ task: { run: { runStatus: "completed" } } });
    await client.callTool({ name: "t3_task_start", arguments: args });
    expect(fake.dispatches.map((dispatch) => dispatch.command.type)).toEqual(["thread.create", "thread.turn.start"]);
  });

  it("attributes overlapping MCP requests to the correct caller", async () => {
    const { fake, client, fixture } = await connectedClient();
    fake.addProject({ id: "correlation" });
    fake.addThread({ id: "correlated-thread", projectId: "correlation" });
    await Promise.all([
      client.callTool({ name: "t3_pending_actions_list", arguments: { threadId: "correlated-thread" } }),
      client.callTool({ name: "t3_threads_list", arguments: { limit: 10 } }),
    ]);
    const rows = (await readFile(`${fixture.directory}/audit.jsonl`, "utf8")).trim().split("\n").map((line) => JSON.parse(line));
    const parents = new Map(rows.filter((row) => row.event === "tool.call").map((row) => [row.correlationId, row.operation]));
    const requests = rows.filter((row) => row.event === "upstream.request");
    expect(requests.length).toBeGreaterThan(2);
    for (const row of requests) expect(row.parentOperation).toBe(parents.get(row.parentCorrelationId));
    expect(requests.some((row) => row.parentOperation === "t3_pending_actions_list")).toBe(true);
    expect(requests.some((row) => row.parentOperation === "t3_threads_list")).toBe(true);
  });
});
