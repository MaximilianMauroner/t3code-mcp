import { readFile } from "node:fs/promises";
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

afterEach(async () => {
  await Promise.all(clients.splice(0).map((client) => client.close().catch(() => undefined)));
  await Promise.all(servers.splice(0).map((server) => server.close().catch(() => undefined)));
  await Promise.all(fixtures.splice(0).map((fixture) => fixture.cleanup()));
  await Promise.all(fakes.splice(0).map((fake) => fake.close()));
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
      "t3_audit_log",
      "t3_connection_status",
      "t3_git_compare",
      "t3_git_diff",
      "t3_git_status",
      "t3_pending_action_respond",
      "t3_pending_actions_list",
      "t3_project_create",
      "t3_projects_list",
      "t3_providers_list",
      "t3_result_get",
      "t3_run_get",
      "t3_run_interrupt",
      "t3_run_wait",
      "t3_task_get",
      "t3_task_start",
      "t3_tasks_list",
      "t3_thread_archive",
      "t3_thread_create",
      "t3_thread_get",
      "t3_thread_interrupt",
      "t3_thread_messages",
      "t3_thread_send",
      "t3_thread_settle",
      "t3_thread_snooze",
      "t3_thread_unsettle",
      "t3_thread_unsnooze",
      "t3_threads_list",
      "t3_threads_overview",
    ]);
    expect(names).not.toContain("t3_call_rpc");
    expect(names).not.toContain("t3_terminal_write");
  });

  it("returns structured content for successful reads", async () => {
    const { fake, client } = await connectedClient();
    fake.addProject({ id: "project-mcp", title: "MCP project" });

    const result = await client.callTool({ name: "t3_projects_list", arguments: { limit: 10 } });

    expect(result.isError).not.toBe(true);
    expect(result.structuredContent).toMatchObject({
      environmentId: fake.environmentId,
      page: { items: [{ id: "project-mcp", title: "MCP project" }] },
    });
    expect(result.content).toBeTruthy();
  });

  it("records tool usage and exposes the bounded audit trail", async () => {
    const { fake, client, fixture } = await connectedClient();
    fake.addProject({ id: "project-audit", title: "Audit project" });
    fake.addThread({ id: "thread-audit", projectId: "project-audit" });

    const result = await client.callTool({
      name: "t3_projects_list",
      arguments: { query: "Audit", limit: 10 },
    });
    expect(result.isError).not.toBe(true);

    const sent = await client.callTool({
      name: "t3_thread_send",
      arguments: {
        threadId: "thread-audit",
        message: "private audit prompt should not be persisted",
        idempotencyKey: "audit-send",
      },
    });
    expect(sent.isError).not.toBe(true);

    const raw = await readFile(`${fixture.config.dataDir}/audit.jsonl`, "utf8");
    expect(raw).toContain('"event":"tool.call"');
    expect(raw).toContain('"operation":"t3_projects_list"');
    expect(raw).toContain('"event":"upstream.request"');
    expect(raw).not.toContain(fake.accessToken);
    expect(raw).not.toContain("private audit prompt should not be persisted");

    const reviewed = await client.callTool({
      name: "t3_audit_log",
      arguments: { source: "mcp", operation: "t3_projects_list", limit: 10 },
    });
    expect(reviewed.isError).not.toBe(true);
    expect(reviewed.structuredContent).toMatchObject({
      page: { total: 2, items: expect.arrayContaining([
        expect.objectContaining({ event: "tool.call", operation: "t3_projects_list" }),
        expect.objectContaining({ event: "tool.result", operation: "t3_projects_list" }),
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
    fake.addProject({ id: "project-worktree-mcp", workspaceRoot: "/remote/worktree-project" });
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
      type: "thread.turn.start",
      bootstrap: {
        createThread: { projectId: "project-worktree-mcp" },
        prepareWorktree: {
          projectCwd: "/remote/worktree-project",
          baseBranch: "main",
          startFromOrigin: true,
        },
      },
    });
  });

  it("finds open threads and interrupts the observed external turn through MCP", async () => {
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
    const stopped = await client.callTool({ name: "t3_thread_interrupt", arguments: {
      threadId: "voice-thread", expectedTurnId: "voice-turn", idempotencyKey: "voice-stop",
    } });
    expect(stopped.isError).not.toBe(true);
    expect(stopped.structuredContent).toMatchObject({ status: "accepted" });
    expect(fake.dispatches).toHaveLength(1);
    const invalid = await client.callTool({ name: "t3_threads_list", arguments: { status: "running" } });
    expect(invalid.isError).toBe(true);
    const missingTurn = await client.callTool({ name: "t3_thread_interrupt", arguments: { threadId: "voice-thread", idempotencyKey: "missing-turn" } });
    expect(missingTurn.isError).toBe(true);
    expect(fake.dispatches).toHaveLength(1);
  });

  it("turns gateway failures into stable structured MCP errors", async () => {
    const { client } = await connectedClient();

    const result = await client.callTool({ name: "t3_thread_create", arguments: {
      projectId: "missing-project",
      title: "will fail",
      message: "Start work.",
      runtimeMode: "approval-required",
      idempotencyKey: "mcp-error-key",
    } });

    expect(result.isError).toBe(true);
    expect(result.structuredContent).toMatchObject({ error: { code: "project_not_found" } });
    expect(result.content).toBeTruthy();
  });

  it("creates a thread only together with its initial message", async () => {
    const { fake, client } = await connectedClient();
    fake.addProject({ id: "atomic-create-project", workspaceRoot: "/remote/atomic" });

    const missingMessage = await client.callTool({ name: "t3_thread_create", arguments: {
      projectId: "atomic-create-project",
      title: "No empty threads",
      runtimeMode: "approval-required",
      idempotencyKey: "atomic-missing-message",
    } });
    expect(missingMessage.isError).toBe(true);
    expect(fake.dispatches).toHaveLength(0);

    const created = await client.callTool({ name: "t3_thread_create", arguments: {
      projectId: "atomic-create-project",
      title: "Atomic worktree thread",
      message: "Inspect the project.",
      runtimeMode: "approval-required",
      workspaceMode: "worktree",
      branch: "main",
      startFromOrigin: true,
      idempotencyKey: "atomic-create",
    } });
    expect(created.isError).not.toBe(true);
    expect(created.structuredContent).toMatchObject({
      status: "accepted",
      projectId: "atomic-create-project",
      runId: expect.stringMatching(/^run_/),
      messageId: expect.stringMatching(/^user:msg_/),
    });
    expect(fake.dispatches).toHaveLength(1);
    expect(fake.dispatches[0]?.command).toMatchObject({
      type: "thread.turn.start",
      message: { text: "Inspect the project." },
      bootstrap: {
        createThread: { projectId: "atomic-create-project", title: "Atomic worktree thread" },
        prepareWorktree: { baseBranch: "main", startFromOrigin: true },
      },
    });
  });

  it("rejects invalid tool arguments at the protocol boundary", async () => {
    const { client } = await connectedClient();

    const missingKey = await client.callTool({ name: "t3_thread_send", arguments: {
      threadId: "thread-1",
      message: "missing idempotency key",
    } });

    const badLimit = await client.callTool({ name: "t3_projects_list", arguments: { limit: 0 } });

    expect(missingKey).toMatchObject({ isError: true });
    expect(badLimit).toMatchObject({ isError: true });
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

    const result = await client.callTool({ name: "t3_thread_send", arguments: {
      threadId: "thread-1",
      message: "must not dispatch",
      idempotencyKey: "read-only-mcp",
    } });

    expect(result.isError).toBe(true);
    expect(result.structuredContent).toMatchObject({ error: { code: "gateway_read_only" } });
    const interruption = await client.callTool({ name: "t3_thread_interrupt", arguments: {
      threadId: "thread-1", expectedTurnId: "turn-1", idempotencyKey: "read-only-stop",
    } });
    expect(interruption.structuredContent).toMatchObject({ error: { code: "gateway_read_only" } });
    expect(fake.dispatches).toHaveLength(0);
  });
});
