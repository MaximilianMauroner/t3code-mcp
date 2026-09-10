import { createServer, type Server } from "node:http";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { type GatewayConfig, } from "../src/config.js";
import { GatewayError, T3Gateway } from "../src/gateway.js";
import { OperationJournal } from "../src/operations/journal.js";
import { T3HttpClient } from "../src/t3/http-client.js";

const temporaryDirectories: string[] = [];
const servers: Server[] = [];

afterEach(async () => {
  await Promise.all(
    servers.splice(0).map(
      (server) =>
        new Promise<void>((resolvePromise) => {
          server.close(() => resolvePromise());
        }),
    ),
  );
  await Promise.all(temporaryDirectories.splice(0).map((directory) => rm(directory, { recursive: true, force: true })));
});

describe("T3Gateway", () => {
  it("does not dispatch a second turn when the same idempotency key is retried", async () => {
    const fake = await fakeT3();
    const gateway = await makeGateway(fake.port, false);

    const first = await gateway.threadSend({
      threadId: "thread-1",
      message: "hello",
      idempotencyKey: "turn-1",
    });
    const second = await gateway.threadSend({
      threadId: "thread-1",
      message: "hello",
      idempotencyKey: "turn-1",
    });

    expect(first.status).toBe("accepted");
    expect(first.providerTurnId).toBeNull();
    expect(first.t3TurnId).toBeNull();
    expect(second.status).toBe("accepted");
    expect(second.runId).toBe(first.runId);
    expect(second.providerTurnId).toBeNull();
    expect(fake.dispatchCount).toBe(1);
    await expect(
      gateway.threadSend({ threadId: "thread-1", message: "different", idempotencyKey: "turn-1" }),
    ).rejects.toBeInstanceOf(Error);
    expect(fake.dispatchCount).toBe(1);
  });

  it("keeps a dispatch timeout uncertain and never replays it automatically", async () => {
    const fake = await fakeT3({ failDispatch: true });
    const gateway = await makeGateway(fake.port, false);

    const first = await gateway.threadSend({
      threadId: "thread-1",
      message: "hello",
      idempotencyKey: "uncertain-turn",
    });
    const second = await gateway.threadSend({
      threadId: "thread-1",
      message: "hello",
      idempotencyKey: "uncertain-turn",
    });

    expect(first.status).toBe("uncertain");
    expect(second.status).toBe("uncertain");
    expect(fake.dispatchCount).toBe(1);
  });

  it("rejects agent control before dispatch for a read-only gateway", async () => {
    const fake = await fakeT3();
    const gateway = await makeGateway(fake.port, true);

    await expect(
      gateway.threadSend({ threadId: "thread-1", message: "hello", idempotencyKey: "read-only" }),
    ).rejects.toMatchObject<Partial<GatewayError>>({ code: "gateway_read_only" });
    expect(fake.dispatchCount).toBe(0);
  });

  it("rejects agent control when the T3 credential lacks operate scope", async () => {
    const fake = await fakeT3({ operateScope: false });
    const gateway = await makeGateway(fake.port, false);

    await expect(
      gateway.threadSend({ threadId: "thread-1", message: "hello", idempotencyKey: "t3-read-only" }),
    ).rejects.toMatchObject<Partial<GatewayError>>({ code: "t3_scope_required" });
    expect(fake.dispatchCount).toBe(0);
  });
});

async function makeGateway(port: number, readOnly: boolean): Promise<T3Gateway> {
  const directory = await mkdtemp(join(tmpdir(), "t3-code-mcp-gateway-"));
  temporaryDirectories.push(directory);
  const config: GatewayConfig = {
    t3HttpBaseUrl: `http://127.0.0.1:${port}`,
    t3AccessToken: "test-token",
    mcpBearerToken: "gateway-token",
    readOnly,
    host: "127.0.0.1",
    port: 0,
    environmentId: null,
    environmentLabel: null,
    dataDir: directory,
    staleAfterMs: 30_000,
  };
  return new T3Gateway(
    new T3HttpClient(config.t3HttpBaseUrl, config.t3AccessToken),
    new OperationJournal(join(directory, "operations.json")),
    config,
  );
}

async function fakeT3(options: { readonly failDispatch?: boolean; readonly operateScope?: boolean } = {}): Promise<{
  readonly port: number;
  readonly dispatchCount: number;
}> {
  const project = {
    id: "project-1",
    title: "demo",
    workspaceRoot: "/remote/demo",
    defaultModelSelection: { instanceId: "codex", model: "gpt-5.6-sol" },
    createdAt: "2026-01-01T00:00:00.000Z",
    updatedAt: "2026-01-01T00:00:00.000Z",
  };
  const thread = {
    id: "thread-1",
    projectId: "project-1",
    title: "investigation",
    modelSelection: { instanceId: "codex", model: "gpt-5.6-sol" },
    runtimeMode: "full-access",
    interactionMode: "default",
    branch: null,
    worktreePath: null,
    latestTurn: null,
    archivedAt: null,
    createdAt: "2026-01-01T00:00:00.000Z",
    updatedAt: "2026-01-01T00:00:00.000Z",
    session: { status: "stopped" },
    hasPendingApprovals: false,
    hasPendingUserInput: false,
    messages: [],
    activities: [],
    checkpoints: [],
    proposedPlans: [],
  };
  let dispatchCount = 0;
  const server = createServer(async (request, response) => {
    response.setHeader("content-type", "application/json");
    if (request.url === "/.well-known/t3/environment") {
      response.end(JSON.stringify({
        environmentId: "environment-1",
        label: "test",
        platform: { os: "test", arch: "test", machine: "test" },
        serverVersion: "test-version",
        capabilities: {},
      }));
      return;
    }
    if (request.url === "/api/auth/session") {
      response.end(JSON.stringify({
        authenticated: true,
        scopes: options.operateScope === false ? ["orchestration:read"] : ["orchestration:read", "orchestration:operate"],
        sessionMethod: "bearer-access-token",
        expiresAt: "2026-12-31T00:00:00.000Z",
      }));
      return;
    }
    if (request.url === "/api/orchestration/shell") {
      response.end(JSON.stringify({ snapshotSequence: 1, projects: [project], threads: [thread], updatedAt: project.updatedAt }));
      return;
    }
    if (request.url === "/api/orchestration/threads/thread-1") {
      response.end(JSON.stringify({ snapshotSequence: 1, thread }));
      return;
    }
    if (request.url === "/api/orchestration/dispatch") {
      dispatchCount += 1;
      if (options.failDispatch) {
        response.statusCode = 503;
        response.end(JSON.stringify({ code: "temporarily_unavailable", message: "test outage" }));
        return;
      }
      response.end(JSON.stringify({ sequence: dispatchCount + 1 }));
      return;
    }
    response.statusCode = 404;
    response.end(JSON.stringify({ code: "not_found" }));
  });
  await new Promise<void>((resolvePromise) => server.listen(0, "127.0.0.1", resolvePromise));
  servers.push(server);
  const address = server.address();
  if (!address || typeof address === "string") {
    throw new Error("Fake T3 server did not expose a TCP port.");
  }
  return {
    port: address.port,
    get dispatchCount() {
      return dispatchCount;
    },
  };
}
