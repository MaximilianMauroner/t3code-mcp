import { mkdir, mkdtemp, readFile, rm, stat } from "node:fs/promises";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { GatewayConfig } from "../src/config.js";
import { GATEWAY_TOOL_NAMES, NATIVE_TOOL_NAMES } from "../src/contract.js";
import { makeGateway, type T3Gateway } from "../src/gateway.js";
import { createMcpServer } from "../src/mcp/server.js";
import { listenHttp } from "../src/mcp/transport.js";
import { loginNative, parseNativeLoginArgs } from "../src/native-login.js";
import { AuditLog } from "../src/operations/audit-log.js";
import { NativeCredentialStore, type NativeCredential } from "../src/t3/native-credential.js";
import { NativeMcpClient } from "../src/t3/native-mcp.js";
import { FakeNativeMcp, type FakeNativeMcpOptions } from "./support/fake-native-mcp.js";
import { FakeT3 } from "./support/fake-t3.js";
import { gatewayFixture } from "./support/gateway-fixture.js";

const cleanups: Array<() => Promise<void>> = [];

afterEach(async () => {
  for (const cleanup of cleanups.splice(0).reverse()) await cleanup();
});

async function startNative(options?: FakeNativeMcpOptions): Promise<FakeNativeMcp> {
  const native = new FakeNativeMcp(options);
  await native.start();
  cleanups.push(() => native.close());
  return native;
}

async function nativeGateway(native: FakeNativeMcp, overrides: Partial<GatewayConfig> = {}): Promise<{
  readonly gateway: T3Gateway;
  readonly store: NativeCredentialStore;
  readonly directory: string;
  readonly config: GatewayConfig;
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
  return { gateway: makeGateway(config).gateway, store: NativeCredentialStore.inDataDir(directory), directory, config };
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

/** Serves the gateway over HTTP, as `serve` does, and returns its base URL. */
async function listen(gateway: T3Gateway, config: GatewayConfig): Promise<string> {
  const server = await listenHttp(gateway, config);
  cleanups.push(() => new Promise<void>((resolve) => server.close(() => resolve())));
  return `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
}

async function postMcp(baseUrl: string, bearer: string, method: string, params: unknown): Promise<{ readonly status: number; readonly body: unknown }> {
  const response = await fetch(`${baseUrl}/mcp`, {
    method: "POST",
    headers: { authorization: `Bearer ${bearer}`, accept: "application/json, text/event-stream", "content-type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
  });
  return { status: response.status, body: await response.json() };
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
  it("refreshes every catalog page without retaining output-schema validators", async () => {
    const native = await startNative({ pagedOutputSchemaTools: 75 });
    const { store, directory } = await nativeGateway(native);
    await login(native, store);
    const client = new NativeMcpClient(native.mcpUrl, store, new AuditLog(join(directory, "audit.jsonl")));
    cleanups.push(() => client.close());

    const initial = await client.snapshot();
    const sdkClient = (client as unknown as { readonly connection: { readonly client: {
      readonly _jsonSchemaValidator: { readonly _ajv: { readonly _cache: Map<unknown, unknown> } };
    } } }).connection.client;
    const initialCacheSize = sdkClient._jsonSchemaValidator._ajv._cache.size;

    expect(initial.tools.map((tool) => tool.name)).toEqual(["t3_thread_list", "t3_thread_send"]);
    expect(native.toolListCursors).toEqual([undefined, ""]);

    vi.useFakeTimers({ toFake: ["Date"] });
    cleanups.push(async () => {
      vi.useRealTimers();
    });
    for (let refresh = 0; refresh < 10; refresh += 1) {
      const previousFetchedAt = (await client.snapshot()).fetchedAt;
      vi.setSystemTime(Date.now() + 61_000);
      await client.snapshot();
      await vi.waitFor(async () => expect((await client.snapshot()).fetchedAt).not.toBe(previousFetchedAt));

      expect(native.toolListCursors.slice(-2)).toEqual([undefined, ""]);
      expect(sdkClient._jsonSchemaValidator._ajv._cache.size).toBe(initialCacheSize);
    }

    const forwarded = await client.callTool("t3_thread_list", { projectId: "project-1" }, false);
    expect(forwarded.structuredContent).toEqual({ projectId: "project-1", threads: [{ threadId: "thread-1", title: "Native thread" }] });
  });

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

  it("forwards calls and returns the upstream result unchanged, with a redacted audit trail that marks native calls", async () => {
    const native = await startNative();
    const { gateway, store, directory } = await nativeGateway(native);
    await login(native, store);
    const client = await connect(gateway);

    const result = await client.callTool({ name: "t3_thread_send", arguments: { threadId: "thread-1", message: "secret steering text", mode: "steer" } });
    await client.callTool({ name: "t3_audit_log", arguments: { limit: 1 } });

    expect(result.isError).toBeFalsy();
    expect(result.structuredContent).toEqual({ threadId: "thread-1", runId: "run-1", status: "running" });
    expect(native.calls).toEqual([{ name: "t3_thread_send", arguments: { threadId: "thread-1", message: "secret steering text", mode: "steer" } }]);
    const audit = await readFile(join(directory, "audit.jsonl"), "utf8");
    expect(audit).toContain('"operation":"mcp:t3_thread_send"');
    expect(audit).not.toContain("secret steering text");
    expect(audit).not.toContain(native.token);
    const events = audit.trim().split("\n").map((line): unknown => JSON.parse(line));
    expect(events).toContainEqual(expect.objectContaining({
      event: "tool.call",
      operation: "t3_thread_send",
      details: { arguments: expect.objectContaining({ threadId: "thread-1", mode: "steer" }), native: true },
    }));
    expect(events).toContainEqual(expect.objectContaining({ event: "tool.call", operation: "t3_audit_log", details: { arguments: { limit: 1 } } }));
  });

  it("keeps only timedOut and status from a wait result in the audit trail", async () => {
    const native = await startNative();
    const { gateway, store, directory } = await nativeGateway(native);
    await login(native, store);

    await (await connect(gateway)).callTool({ name: "t3_thread_wait", arguments: { threadId: "thread-1", timeoutMs: 1 } });

    const events = (await readFile(join(directory, "audit.jsonl"), "utf8")).trim().split("\n").map((line): unknown => JSON.parse(line));
    expect(events).toContainEqual(expect.objectContaining({
      source: "mcp",
      event: "tool.result",
      operation: "t3_thread_wait",
      details: { result: { isError: false, keys: ["threadId", "runId", "status", "timedOut"], status: "running", timedOut: true } },
    }));
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

  /** A new login while T3 keeps the earlier session, as T3 does until that session expires or is revoked. */
  async function renewKeepingOldSession(native: FakeNativeMcp, store: NativeCredentialStore): Promise<void> {
    native.acceptedTokens = [native.token];
    native.token = "native-token-2";
    await login(native, store);
  }

  it("lets a wait in progress finish on the old connection after a renewal, and then closes it", async () => {
    const native = await startNative();
    const { gateway, store } = await nativeGateway(native);
    await login(native, store);
    const closes = vi.spyOn(Client.prototype, "close");
    cleanups.push(async () => {
      closes.mockRestore();
    });

    native.holdToolCalls = "t3_thread_wait";
    // Settled at once, so a failure shows in an assertion and not as an unhandled rejection.
    const wait = gateway.callNative("t3_thread_wait", { threadId: "thread-1" }).catch((error: unknown) => error);
    await vi.waitFor(() => expect(native.heldToolCallCount).toBe(1));
    await renewKeepingOldSession(native, store);
    const list = await gateway.callNative("t3_thread_list", { projectId: "project-1" });
    const closesWhileWaiting = closes.mock.calls.length;
    native.holdToolCalls = null;
    native.releaseToolCalls();

    expect(await wait).toEqual(expect.objectContaining({ structuredContent: { threadId: "thread-1", runId: "run-1", status: "running", timedOut: true } }));
    expect(list.isError).toBeFalsy();
    expect(native.calls.map((call) => call.name)).toEqual(["t3_thread_wait", "t3_thread_list"]);
    expect(native.toolCallTokens).toEqual(["native-token-1", "native-token-2"]);
    expect(closesWhileWaiting).toBe(0);
    expect(closes).toHaveBeenCalledTimes(1);
  });

  it("returns the result of a send in progress after a renewal instead of an unknown outcome", async () => {
    const native = await startNative();
    const { gateway, store } = await nativeGateway(native);
    await login(native, store);

    native.holdToolCalls = "t3_thread_send";
    const send = gateway.callNative("t3_thread_send", { threadId: "thread-1", message: "go" }).catch((error: unknown) => error);
    await vi.waitFor(() => expect(native.heldToolCallCount).toBe(1));
    await renewKeepingOldSession(native, store);
    await gateway.callNative("t3_thread_list", { projectId: "project-1" });
    native.holdToolCalls = null;
    native.releaseToolCalls();

    expect(await send).toEqual(expect.objectContaining({ structuredContent: { threadId: "thread-1", runId: "run-1", status: "queued" } }));
    expect(native.calls.filter((call) => call.name === "t3_thread_send")).toHaveLength(1);
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

  it("reports an unreadable credential file and keeps the gateway's own tools working", async () => {
    const native = await startNative();
    const { gateway, store, config } = await nativeGateway(native, { mcpBearerToken: "gateway-bearer" });
    await login(native, store);
    await gateway.nativeSnapshot();
    const baseUrl = await listen(gateway, config);

    // A directory at the credential path fails the read with EISDIR, also for root.
    await rm(store.filePath);
    await mkdir(store.filePath);
    const unreadable = expect.stringContaining("credential file cannot be read (EISDIR)");

    await expect(store.read()).resolves.toEqual({ state: "missing", reason: unreadable });
    await expect(gateway.nativeSnapshot()).resolves.toMatchObject({ error: { code: "native_credential_missing", message: unreadable } });
    const audit = await postMcp(baseUrl, "gateway-bearer", "tools/call", { name: "t3_audit_log", arguments: { limit: 1 } });
    const list = await postMcp(baseUrl, "gateway-bearer", "tools/call", { name: "t3_thread_list", arguments: { projectId: "project-1" } });
    expect(audit).toMatchObject({ status: 200, body: { result: { structuredContent: { page: expect.any(Object) } } } });
    expect(list).toMatchObject({
      status: 200,
      body: { result: { isError: true, structuredContent: { error: { code: "native_credential_missing", message: unreadable } } } },
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

  // The 20 s test timeout lets a slow answer fail on the elapsed time, not on the test timeout.
  it("keeps gateway tools fast and native tools listed after a renewal while T3 does not answer", async () => {
    const native = await startNative();
    const { gateway, store, config } = await nativeGateway(native, { mcpBearerToken: "gateway-bearer" });
    await login(native, store);
    await gateway.nativeSnapshot();
    const baseUrl = await listen(gateway, config);

    native.token = "native-token-2";
    await login(native, store);
    native.stallRequests = true;
    const started = performance.now();
    const audit = await postMcp(baseUrl, "gateway-bearer", "tools/call", { name: "t3_audit_log", arguments: { limit: 1 } });
    const elapsed = performance.now() - started;
    const listed = await postMcp(baseUrl, "gateway-bearer", "tools/list", {});

    expect(elapsed).toBeLessThan(1000);
    expect(audit).toMatchObject({ status: 200, body: { result: { structuredContent: { page: expect.any(Object) } } } });
    expect(listed).toMatchObject({ status: 200, body: { result: { tools: expect.arrayContaining([expect.objectContaining({ name: "t3_thread_send" })]) } } });
  }, 20_000);

  // A stdio session reads the tool list once, so this first answer lasts for the whole session.
  it("gives the first caller the native tools when a renewal lands during the first catalog read", async () => {
    const native = await startNative();
    const { store, directory } = await nativeGateway(native);
    let current = await login(native, store);
    // The credential source has no file I/O, so the second call below reaches the catalog read in microtasks.
    const client = new NativeMcpClient(native.mcpUrl, { read: async () => ({ state: "valid", credential: current }) }, new AuditLog(join(directory, "audit.jsonl")));
    cleanups.push(() => client.close());

    native.holdToolLists = true;
    const first = client.snapshot();
    await vi.waitFor(() => expect(native.heldToolListCount).toBe(1));
    native.token = "native-token-2";
    current = await login(native, store);
    const second = client.snapshot();
    // setImmediate runs after every pending microtask, so the second call has now reached the catalog read.
    await new Promise((resolve) => setImmediate(resolve));
    native.releaseToolLists();

    for (const pending of [first, second]) {
      const snapshot = await pending;
      expect(snapshot.error).toBeNull();
      expect(snapshot.tools.map((tool) => tool.name)).toContain("t3_thread_send");
    }
    // The catalog was read with the earlier token, so the next call reads it again with the new one.
    await client.snapshot();
    await vi.waitFor(() => expect(native.heldToolListCount).toBe(1));
    native.releaseToolLists();
  });

  it("keeps the last known tools when the catalog refresh for a renewed credential fails", async () => {
    const native = await startNative();
    const { gateway, store } = await nativeGateway(native);
    await login(native, store);
    const before = await gateway.nativeSnapshot();

    native.token = "native-token-2";
    await login(native, store);
    // T3 stops, so the refresh for the new token fails at once.
    await native.close();
    await vi.waitFor(async () => expect((await gateway.nativeSnapshot())?.error?.code).toBe("native_unavailable"));

    expect((await gateway.nativeSnapshot())?.tools).toEqual(before?.tools);
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

    expect((await wait).structuredContent).toEqual({ threadId: "thread-1", runId: "run-1", status: "running", timedOut: true });
    expect(native.calls.map((call) => call.name)).toEqual(["t3_thread_wait"]);
  });

  it("keeps a wait with a very large finite budget open until T3 answers", async () => {
    const native = await startNative();
    const { gateway, store } = await nativeGateway(native);
    await login(native, store);
    native.runEndsAfterMs = 200;

    // The budget plus the margin is above the largest timer that Node supports.
    const result = await gateway.callNative("t3_thread_wait", { threadId: "thread-1", timeoutMs: 2_200_000_000 });

    expect(result.structuredContent).toEqual({ threadId: "thread-1", runId: "run-1", status: "completed", timedOut: false });
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

  it("reports every native tool as unavailable after T3 refuses a credential that has not expired", async () => {
    const native = await startNative();
    const { gateway, store } = await nativeGateway(native);
    await login(native, store);
    await gateway.nativeSnapshot();

    // As after `t3 auth session revoke`. The catalog refresh after its TTL meets the refusal.
    native.token = "native-token-revoked";
    vi.useFakeTimers({ toFake: ["Date"] });
    cleanups.push(async () => {
      vi.useRealTimers();
    });
    vi.setSystemTime(Date.now() + 61_000);
    await vi.waitFor(async () => expect((await gateway.nativeSnapshot())?.error?.code).toBe("native_credential_rejected"));
    const status = await gateway.connectionStatus();

    expect(status.native.credential).toBe("valid");
    for (const operation of [...NATIVE_TOOL_NAMES, "t3_attachment_upload", "t3_task_cancel"]) {
      expect(status.callableOperations).not.toContain(operation);
      expect(status.disabledOperations).toContainEqual({
        operation,
        reasonCode: "native_unavailable",
        reason: expect.stringContaining("rejected the stored MCP credential"),
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

  it.each([
    ["t3_task_cancel", "t3_thread_interrupt"],
    ["t3_attachment_upload", "t3_attachment_prepare_upload"],
  ])("reports %s as unavailable when T3 does not offer %s", async (operation, needed) => {
    const native = await startNative();
    native.omitTools = [needed];
    const { gateway, store } = await nativeGateway(native);
    await login(native, store);

    const status = await gateway.connectionStatus();

    expect(status.callableOperations).not.toContain(operation);
    expect(status.disabledOperations).toContainEqual({ operation, reasonCode: "native_unavailable", reason: expect.stringContaining(needed) });
    expect(status.native.missingTools).toContain(needed);
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
