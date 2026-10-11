import { mkdtemp, readFile, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { GatewayConfig } from "../src/config.js";
import { GATEWAY_TOOL_NAMES } from "../src/contract.js";
import { makeGateway, type T3Gateway } from "../src/gateway.js";
import { createMcpServer } from "../src/mcp/server.js";
import { loginNative, parseNativeLoginArgs } from "../src/native-login.js";
import { AuditLog } from "../src/operations/audit-log.js";
import { NativeCredentialStore, type NativeCredential } from "../src/t3/native-credential.js";
import { NativeMcpClient } from "../src/t3/native-mcp.js";
import { FakeNativeMcp } from "./support/fake-native-mcp.js";
import { FakeT3 } from "./support/fake-t3.js";
import { gatewayFixture } from "./support/gateway-fixture.js";

const cleanups: Array<() => Promise<void>> = [];

afterEach(async () => {
  for (const cleanup of cleanups.splice(0).reverse()) await cleanup();
});

async function startNative(): Promise<FakeNativeMcp> {
  const native = new FakeNativeMcp();
  await native.start();
  cleanups.push(() => native.close());
  return native;
}

async function nativeGateway(native: FakeNativeMcp, overrides: Partial<GatewayConfig> = {}): Promise<{
  readonly gateway: T3Gateway;
  readonly store: NativeCredentialStore;
  readonly directory: string;
}> {
  const directory = await mkdtemp(join(tmpdir(), "t3-code-mcp-native-test-"));
  cleanups.push(() => rm(directory, { recursive: true, force: true }));
  const config: GatewayConfig = {
    // The native proxy never calls T3's HTTP API; an unreachable address proves it.
    t3HttpBaseUrl: "http://127.0.0.1:9",
    t3NativeMcpUrl: native.mcpUrl,
    t3AccessToken: "t3-access-token",
    mcpBearerToken: null,
    readOnly: false,
    host: "127.0.0.1",
    port: 0,
    maxBodyBytes: 2 * 1024 * 1024,
    environmentId: null,
    environmentLabel: null,
    dataDir: directory,
    worktreeRoot: null,
    staleAfterMs: 30_000,
    ...overrides,
  };
  return { gateway: makeGateway(config).gateway, store: NativeCredentialStore.inDataDir(directory), directory };
}

async function login(native: FakeNativeMcp, store: NativeCredentialStore, access: NativeCredential["access"] = "full-access"): Promise<NativeCredential> {
  const credential = await loginNative({ resource: native.mcpUrl, access, pairingCode: native.pairingCode });
  await store.write(credential);
  return credential;
}

async function connect(gateway: T3Gateway): Promise<Client> {
  const server = createMcpServer(gateway, await gateway.nativeSnapshot());
  const client = new Client({ name: "native-proxy-test", version: "1.0.0" });
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await server.connect(serverTransport);
  await client.connect(clientTransport);
  cleanups.push(async () => {
    await client.close();
    await server.close();
  });
  return client;
}

function errorCode(result: Awaited<ReturnType<Client["callTool"]>>): unknown {
  const structured = result.structuredContent as { error?: { code?: unknown } } | undefined;
  return structured?.error?.code;
}

describe("native T3 login", () => {
  it("obtains a credential with a pairing code and PKCE, and stores it privately", async () => {
    const native = await startNative();
    const { store } = await nativeGateway(native);
    const before = Date.now();
    const credential = await login(native, store);

    expect(native.decisions).toEqual([{ access: "full-access", code: native.pairingCode }]);
    expect(credential).toMatchObject({ resource: native.mcpUrl, accessToken: native.token, access: "full-access" });
    expect(credential.scopes).toEqual(["orchestration:read", "orchestration:operate"]);
    expect(Date.parse(credential.expiresAt) - before).toBeGreaterThanOrEqual(native.expiresInSeconds * 1000 - 1000);
    expect((await stat(store.filePath)).mode & 0o777).toBe(0o600);
    expect(await store.read()).toEqual({ state: "valid", credential });
  });

  it("reports a denied pairing code instead of storing anything", async () => {
    const native = await startNative();
    await expect(loginNative({ resource: native.mcpUrl, access: "auto", pairingCode: "WRONG" })).rejects.toThrow(/denied the login: access_denied/);
  });

  it("refuses a pairing code passed as an argument", () => {
    expect(() => parseNativeLoginArgs(["--code", "PAIR-1234"])).toThrow(/process lists/);
    expect(() => parseNativeLoginArgs(["--code=PAIR-1234"])).toThrow(/process lists/);
    expect(parseNativeLoginArgs(["--access", "auto"])).toEqual({ access: "auto" });
    expect(() => parseNativeLoginArgs(["--access", "root"])).toThrow(/--access must be one of/);
  });
});

describe("native T3 tool proxy", () => {
  it("advertises allowlisted native tools beside gateway tools and hides thread-bound ones", async () => {
    const native = await startNative();
    const { gateway, store } = await nativeGateway(native);
    await login(native, store);
    const tools = (await (await connect(gateway)).listTools()).tools;
    const names = tools.map((tool) => tool.name);

    expect(names).toEqual(expect.arrayContaining([...GATEWAY_TOOL_NAMES, "t3_thread_list", "t3_thread_send", "t3_thread_interrupt", "t3_attachment_discard"]));
    // Excluded: needs a calling T3 thread, or is used only inside t3_attachment_upload.
    expect(names).not.toContain("delegate_task");
    expect(names).not.toContain("t3_attachment_prepare_upload");
    const send = tools.find((tool) => tool.name === "t3_thread_send");
    expect(send?.annotations?.readOnlyHint).toBe(false);
    expect(send?.inputSchema.properties).toHaveProperty("mode");
    expect(send?.inputSchema.required).toEqual(["threadId", "message"]);
  });

  it("forwards calls and returns the upstream result unchanged, with a redacted audit trail", async () => {
    const native = await startNative();
    const { gateway, store, directory } = await nativeGateway(native);
    await login(native, store);
    const client = await connect(gateway);

    const result = await client.callTool({ name: "t3_thread_send", arguments: { threadId: "thread-1", message: "secret steering text", mode: "steer" } });

    expect(result.isError).toBeFalsy();
    expect(result.structuredContent).toEqual({ threadId: "thread-1", runId: "run-1", status: "running" });
    expect(native.calls).toEqual([{ name: "t3_thread_send", arguments: { threadId: "thread-1", message: "secret steering text", mode: "steer" } }]);
    const audit = await readFile(join(directory, "audit.jsonl"), "utf8");
    expect(audit).toContain('"operation":"mcp:t3_thread_send"');
    expect(audit).not.toContain("secret steering text");
    expect(audit).not.toContain(native.token);
  });

  it("enforces gateway read-only mode before anything reaches T3", async () => {
    const native = await startNative();
    const { gateway, store } = await nativeGateway(native, { readOnly: true });
    await login(native, store);
    const client = await connect(gateway);

    const send = await client.callTool({ name: "t3_thread_send", arguments: { threadId: "thread-1", message: "go" } });
    const list = await client.callTool({ name: "t3_thread_list", arguments: { projectId: "project-1" } });

    expect(errorCode(send)).toBe("gateway_read_only");
    expect(list.isError).toBeFalsy();
    expect(native.calls.map((call) => call.name)).toEqual(["t3_thread_list"]);
  });

  it("rejects mutations when the native login was approved read-only", async () => {
    const native = await startNative();
    const { gateway, store } = await nativeGateway(native);
    await login(native, store, "read-only");

    await expect(gateway.callNative("t3_thread_send", { threadId: "thread-1", message: "go" })).rejects.toMatchObject({ code: "t3_scope_required" });
    expect(native.calls).toEqual([]);
  });

  it("uses a renewed credential without a restart", async () => {
    const native = await startNative();
    const { gateway, store } = await nativeGateway(native);
    await login(native, store);
    await gateway.callNative("t3_thread_list", { projectId: "project-1" });

    native.token = "native-token-2";
    await login(native, store);
    const result = await gateway.callNative("t3_thread_list", { projectId: "project-1" });

    expect(result.isError).toBeFalsy();
    expect(native.calls).toHaveLength(2);
  });

  it("explains how to renew a missing or expired credential without calling T3", async () => {
    const native = await startNative();
    const { gateway, store } = await nativeGateway(native);

    await expect(gateway.callNative("t3_thread_list", { projectId: "p" })).rejects.toMatchObject({ code: "native_credential_missing" });
    const credential = await login(native, store);
    await store.write({ ...credential, expiresAt: new Date(Date.now() - 1000).toISOString() });
    await expect(gateway.callNative("t3_thread_list", { projectId: "p" })).rejects.toMatchObject({
      code: "native_credential_expired",
      message: expect.stringContaining("native-login"),
    });
    expect(native.calls).toEqual([]);
  });

  it("keeps native tools listed after the credential expires, so a call explains how to renew it", async () => {
    const native = await startNative();
    const { gateway, store } = await nativeGateway(native);
    const credential = await login(native, store);
    await gateway.nativeSnapshot();
    await store.write({ ...credential, expiresAt: new Date(Date.now() - 1000).toISOString() });

    const result = await (await connect(gateway)).callTool({ name: "t3_thread_send", arguments: { threadId: "thread-1", message: "go" } });

    expect(errorCode(result)).toBe("native_credential_expired");
    expect(native.calls).toEqual([]);
  });

  it("reports a token that T3 refuses at connect as a rejected credential, without a retry", async () => {
    const native = await startNative();
    const { gateway, store } = await nativeGateway(native);
    await login(native, store);
    native.token = "native-token-revoked";

    await expect(gateway.callNative("t3_thread_list", { projectId: "project-1" })).rejects.toMatchObject({
      code: "native_credential_rejected",
      message: expect.stringContaining("native-login"),
    });
    expect(native.unauthorizedRequests).toBe(1);
    expect((await gateway.nativeSnapshot())?.error?.code).toBe("native_credential_rejected");
    expect(native.calls).toEqual([]);
  });

  it("serves a stale tool catalog at once while T3 does not answer", async () => {
    const native = await startNative();
    const { gateway, store } = await nativeGateway(native);
    await login(native, store);
    const fresh = await gateway.nativeSnapshot();

    native.stallRequests = true;
    vi.useFakeTimers({ toFake: ["Date"] });
    cleanups.push(async () => {
      vi.useRealTimers();
    });
    vi.setSystemTime(Date.now() + 61_000);
    const started = performance.now();
    const stale = await gateway.nativeSnapshot();

    expect(performance.now() - started).toBeLessThan(1000);
    expect(stale?.tools.length).toBeGreaterThan(0);
    expect(stale?.tools).toEqual(fresh?.tools);
  });

  it("keeps the shared session open when one call times out, so a concurrent call runs once", async () => {
    const native = await startNative();
    const { store, directory } = await nativeGateway(native);
    await login(native, store);
    // A 100 ms request timeout. A wait gets its own budget plus a margin instead.
    const client = new NativeMcpClient(native.mcpUrl, store, new AuditLog(join(directory, "audit.jsonl")), 100);
    cleanups.push(() => client.close());

    const wait = client.callTool("t3_thread_wait", { threadId: "thread-1", timeoutMs: 500 }, false);
    await vi.waitFor(() => expect(native.calls).toHaveLength(1));
    native.stallRequests = true;
    await expect(client.callTool("t3_thread_list", { projectId: "project-1" }, false)).rejects.toMatchObject({ code: "native_unavailable" });

    expect((await wait).structuredContent).toEqual({ threadId: "thread-1", timedOut: true });
    expect(native.calls.map((call) => call.name)).toEqual(["t3_thread_wait"]);
  });

  it("resends once after T3 forgets the MCP session, because nothing ran", async () => {
    const native = await startNative();
    const { gateway, store } = await nativeGateway(native);
    await login(native, store);
    await gateway.callNative("t3_thread_list", { projectId: "project-1" });

    native.dropSessions();
    const result = await gateway.callNative("t3_thread_send", { threadId: "thread-1", message: "go" });

    expect(result.isError).toBeFalsy();
    expect(native.calls.filter((call) => call.name === "t3_thread_send")).toHaveLength(1);
  });

  it("never replays a mutation whose result was lost, but retries a lost read once", async () => {
    const native = await startNative();
    const { gateway, store } = await nativeGateway(native);
    await login(native, store);

    native.dropNextToolResponse = true;
    await expect(gateway.callNative("t3_thread_send", { threadId: "thread-1", message: "go" })).rejects.toMatchObject({ code: "native_outcome_unknown" });
    expect(native.calls.filter((call) => call.name === "t3_thread_send")).toHaveLength(1);

    native.dropNextToolResponse = true;
    const read = await gateway.callNative("t3_thread_list", { projectId: "project-1" });
    expect(read.isError).toBeFalsy();
    expect(native.calls.filter((call) => call.name === "t3_thread_list")).toHaveLength(2);
  });

  it("retries a read on a fresh connection when T3 restarts after losing its response", async () => {
    const native = await startNative();
    const { gateway, store } = await nativeGateway(native);
    await login(native, store);

    native.dropNextToolResponse = true;
    native.restartOnDrop = true;
    const read = await gateway.callNative("t3_thread_list", { projectId: "project-1" });

    expect(read.isError).toBeFalsy();
    // The first attempt ran, the retry met the forgotten session, and the third attempt ran.
    expect(native.calls.filter((call) => call.name === "t3_thread_list")).toHaveLength(2);
  });

  it("never replays a mutation whose response T3 lost in a restart", async () => {
    const native = await startNative();
    const { gateway, store } = await nativeGateway(native);
    await login(native, store);

    native.dropNextToolResponse = true;
    native.restartOnDrop = true;
    await expect(gateway.callNative("t3_thread_send", { threadId: "thread-1", message: "go" })).rejects.toMatchObject({ code: "native_outcome_unknown" });

    expect(native.calls.filter((call) => call.name === "t3_thread_send")).toHaveLength(1);
    expect((await gateway.callNative("t3_thread_list", { projectId: "project-1" })).isError).toBeFalsy();
  });

  it("reports every native tool as unavailable after the credential expires", async () => {
    const native = await startNative();
    const { gateway, store } = await nativeGateway(native);
    const credential = await login(native, store);
    await gateway.nativeSnapshot();
    await store.write({ ...credential, expiresAt: new Date(Date.now() - 1000).toISOString() });

    const status = await gateway.connectionStatus();

    for (const operation of ["t3_thread_list", "t3_thread_send"]) {
      expect(status.callableOperations).not.toContain(operation);
      expect(status.disabledOperations).toContainEqual({
        operation,
        reasonCode: "native_unavailable",
        reason: expect.stringContaining("native-login"),
      });
    }
  });
});

describe("native-backed gateway tools", () => {
  it("uploads base64 bytes through the signed native URL", async () => {
    const native = await startNative();
    const { gateway, store } = await nativeGateway(native);
    await login(native, store);
    const bytes = Buffer.from("png-bytes");

    const result = await gateway.attachmentUpload({ name: "shot.png", mimeType: "image/png", dataBase64: bytes.toString("base64") });

    expect(result.attachment).toEqual({ type: "image", id: "attachment-1", name: "shot.png", mimeType: "image/png", sizeBytes: bytes.byteLength });
    expect(native.calls[0]).toEqual({
      name: "t3_attachment_prepare_upload",
      arguments: { upload: { type: "image", name: "shot.png", mimeType: "image/png", sizeBytes: bytes.byteLength } },
    });
    expect(native.uploads).toEqual([{ path: "/api/attachments/upload/signed-token", contentType: "image/png", bytes }]);
  });

  it("discards a prepared upload when the byte upload fails, and rejects malformed base64", async () => {
    const native = await startNative();
    const { gateway, store } = await nativeGateway(native);
    await login(native, store);

    native.failNextUpload = 400;
    await expect(gateway.attachmentUpload({ name: "notes.txt", mimeType: "text/plain", dataBase64: Buffer.from("hi").toString("base64") }))
      .rejects.toMatchObject({ code: "native_upload_failed" });
    expect(native.calls.map((call) => call.name)).toEqual(["t3_attachment_prepare_upload", "t3_attachment_discard"]);

    await expect(gateway.attachmentUpload({ name: "x.txt", mimeType: "text/plain", dataBase64: "not base64!" }))
      .rejects.toMatchObject({ code: "invalid_attachment" });
    expect(native.calls).toHaveLength(2);
  });

  it("does not follow a redirect on the byte upload", async () => {
    const native = await startNative();
    const elsewhere = await startNative();
    const { gateway, store } = await nativeGateway(native);
    await login(native, store);

    native.redirectNextUpload = `${elsewhere.baseUrl}/api/attachments/upload/elsewhere`;
    await expect(gateway.attachmentUpload({ name: "notes.txt", mimeType: "text/plain", dataBase64: Buffer.from("hi").toString("base64") }))
      .rejects.toMatchObject({ code: "native_upload_failed" });
    expect(elsewhere.uploads).toEqual([]);
  });

  async function taskGateway(native: FakeNativeMcp, options: ConstructorParameters<typeof FakeT3>[0] = {}) {
    const fake = new FakeT3(options);
    await fake.start();
    cleanups.push(() => fake.close());
    fake.addProject({ id: "project-task", title: "Task project" });
    const fixture = await gatewayFixture(fake, { t3NativeMcpUrl: native.mcpUrl });
    cleanups.push(fixture.cleanup);
    await login(native, NativeCredentialStore.inDataDir(fixture.directory));
    return fixture.gateway;
  }

  const taskInput = {
    projectId: "project-task",
    title: "Cancellable assignment",
    instruction: "Inspect the project and report.",
    runtimeMode: "approval-required" as const,
    idempotencyKey: "cancel-task-key",
  };

  it("cancels a task by interrupting its own T3 run with a stable request ID", async () => {
    const native = await startNative();
    const gateway = await taskGateway(native);
    const started = await gateway.taskStart(taskInput);
    const turnId = (await gateway.taskGet(started.taskRef)).task.run?.t3TurnId;
    expect(turnId).toEqual(expect.any(String));

    const cancelled = await gateway.taskCancel({ taskRef: started.taskRef, reason: "Superseded" });

    expect(cancelled).toMatchObject({ taskRef: started.taskRef, threadId: started.threadId, t3RunId: turnId, status: "interrupt_requested" });
    expect(native.calls).toEqual([{
      name: "t3_thread_interrupt",
      arguments: { threadId: started.threadId, runId: turnId, reason: "Superseded", clientRequestId: `task-cancel:${started.taskRef}:${turnId}` },
    }]);
  });

  it("refuses to cancel a task that started no run", async () => {
    const native = await startNative();
    // thread.create fails, so the first message is never sent.
    const gateway = await taskGateway(native, { dispatchStatus: 503, dispatchErrorMessage: "gateway unavailable" });
    const started = await gateway.taskStart(taskInput);

    await expect(gateway.taskCancel({ taskRef: started.taskRef })).rejects.toMatchObject({
      code: "task_not_started",
      message: expect.stringContaining("started no run"),
    });
    expect(native.calls).toEqual([]);
  });
});
