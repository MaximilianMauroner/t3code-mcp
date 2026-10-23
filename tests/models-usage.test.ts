import { ModelsUsageOutputSchema } from "../src/t3/models-usage.js";
import { createServer } from "node:http";
import { once } from "node:events";
import type { AddressInfo } from "node:net";
import { readFile } from "node:fs/promises";
import { WebSocketServer } from "ws";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { afterEach, describe, expect, it } from "vitest";
import { createMcpServer } from "../src/mcp/server.js";
import { gatewayFixture } from "./support/gateway-fixture.js";
import { T3HttpClient } from "../src/t3/http-client.js";
import { FakeT3 } from "./support/fake-t3.js";

const checkedAt = "2026-10-22T10:00:00Z";
const model = { slug: "default-model", name: "Default model", isCustom: false, isDefault: true, capabilities: { secret: "hidden-capability" } };
const window = { id: "weekly-opus", kind: "weekly", label: "Weekly Opus", usedPercent: 25 };
const provider = (instanceId: string, usageLimits?: unknown) => ({
  instanceId, driver: "codex", displayName: "Shared display name", enabled: true, installed: true,
  status: "ready", checkedAt, models: [model], ...(usageLimits === undefined ? {} : { usageLimits }),
});
const cleanup: Array<() => Promise<unknown>> = [];
afterEach(async () => { for (const close of cleanup.splice(0).reverse()) await close(); });

async function connected(config: unknown, options: { mode?: "auth" | "rpc" | "disconnect" | "malformed"; environmentId?: string; readOnly?: boolean; sources?: unknown; events?: unknown[]; followingEvents?: unknown[]; afterSnapshot?: "end" | "disconnect" | "timeout" | "rpc" | "defect" | "malformed"; refreshError?: string; refreshStall?: boolean } = {}) {
  const fake = new FakeT3({ scopes: ["orchestration:read"] });
  await fake.start();
  cleanup.push(() => fake.close());
  let snapshotReceivedResolve!: () => void;
  const snapshotReceived = new Promise<void>((resolve) => { snapshotReceivedResolve = resolve; });
  const paths: string[] = [];
  const calls: unknown[] = [];
  const server = createServer(async (request, response) => {
    paths.push(request.url!);
    const upstream = await fetch(`${fake.baseUrl}${request.url}`);
    response.writeHead(upstream.status, { "content-type": "application/json" });
    response.end(await upstream.text());
  });
  const ws = new WebSocketServer({ noServer: true });
  server.on("upgrade", (request, socket, head) => {
    paths.push(request.url!);
    expect(request.headers.authorization).toBe(`Bearer ${fake.accessToken}`);
    if (options.mode === "auth") {
      socket.end("HTTP/1.1 403 Forbidden\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
      return;
    }
    ws.handleUpgrade(request, socket, head, (connection) => {
      let requestId: string;
      let acknowledged = false;
      connection.on("message", (data) => {
        const call = JSON.parse(data.toString());
        if (call._tag === "Ack") {
          expect(call.requestId).toBe(requestId);
          if (acknowledged) return;
          acknowledged = true;
          snapshotReceivedResolve();
          const mode = options.afterSnapshot;
          if (mode === "disconnect") { connection.close(); return; }
          if (mode === "timeout") return;
          if (mode === "malformed") { connection.send("not-json"); return; }
          if (mode === "defect") { connection.send(JSON.stringify({ _tag: "Defect" })); return; }
          if (mode === "end" || mode === "rpc") {
            connection.send(JSON.stringify({ _tag: "Exit", requestId, exit: mode === "rpc"
              ? { _tag: "Failure", cause: [{ _tag: "Fail", error: { message: "source read failed" } }] }
              : { _tag: "Success", value: null } }));
            return;
          }
          connection.send(JSON.stringify({ _tag: "Chunk", requestId, values: options.followingEvents ?? [{ version: 1, type: "usageLimitSourcesUpdated", payload: { sources: options.sources ?? [] } }] }));
          return;
        }
        calls.push(call);
        requestId = call.id;
        if (options.mode === "disconnect") { connection.close(); return; }
        if (options.mode === "malformed") { connection.send("not-json"); return; }
        if (call.tag === "server.refreshProviders" && options.refreshStall) return;
        if (options.mode === "rpc" || call.tag === "server.refreshProviders") {
          const error = options.mode === "rpc" ? "requires orchestration:read" : options.refreshError;
          connection.send(JSON.stringify({ _tag: "Exit", requestId, exit: error
            ? { _tag: "Failure", cause: [{ _tag: "Fail", error: { message: error } }] }
            : { _tag: "Success", value: null } }));
          return;
        }
        expect(call.tag).toBe("subscribeServerConfig");
        expect(call.payload).toEqual({ usageLimitSources: true });
        connection.send(JSON.stringify({ _tag: "Chunk", requestId, values: options.events ?? [{ version: 1, type: "snapshot", config }] }));
      });
    });
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  cleanup.push(async () => {
    for (const connection of ws.clients) connection.terminate();
    await new Promise<void>((resolve) => ws.close(() => resolve()));
    server.close();
    await once(server, "close");
  });
  const fixture = await gatewayFixture(fake, {
    t3HttpBaseUrl: `http://127.0.0.1:${(server.address() as AddressInfo).port}`,
    readOnly: options.readOnly ?? false, environmentId: options.environmentId ?? null,
  });
  cleanup.push(() => fixture.cleanup());
  const mcp = createMcpServer(fixture.gateway);
  const client = new Client({ name: "models-usage-test", version: "1" });
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await mcp.connect(serverTransport);
  await client.connect(clientTransport);
  cleanup.push(() => mcp.close(), () => client.close());
  const rpcClient = new T3HttpClient(`http://127.0.0.1:${(server.address() as AddressInfo).port}`, fake.accessToken, 200);
  return { client, rpcClient, snapshotReceived, paths, calls, fixture, call: (args = {}) => client.callTool({ name: "t3_models_usage", arguments: args }) };
}

describe("t3_models_usage through MCP and authenticated RPC", () => {
  it("publishes a read-only tool and returns complete catalogs with shared provider windows and safe fields", async () => {
    const snapshot = {
      checkedAt, windows: [
        { ...window, usedPercent: 0, resetsAt: "2026-10-23T10:00:00Z", windowDurationMins: 10080 },
        { ...window, id: "session", kind: "session", usedPercent: 100 },
      ], credentialFingerprint: "secret-fingerprint",
      resetCredits: { availableCount: 2, nextExpiresAt: checkedAt, nextCreditId: "secret-credit" },
      externalUsage: { label: "Billing", url: "https://secret.example/token" },
    };
    const first = { ...provider("first", snapshot), models: [model, { slug: "custom", name: "Custom", isCustom: true, isLegacy: true, aliases: ["alias"], subProvider: "custom-provider", unknown: "hidden-model" }], auth: { token: "secret-auth" }, config: { secret: "secret-config" } };
    const { client, call, calls, fixture, paths } = await connected({ providers: [first, provider("second")], settings: { secret: "secret-settings" } }, { readOnly: true });
    const tool = (await client.listTools()).tools.find((tool) => tool.name === "t3_models_usage");
    expect(tool?.annotations).toMatchObject({ readOnlyHint: true, destructiveHint: false });
    expect(tool?.description).toContain("Do not sum");
    expect(tool?.inputSchema.properties).toHaveProperty("providerInstanceId");
    const result = await call();
    expect(result.isError).not.toBe(true);
    expect(result.structuredContent).toMatchObject({ environmentId: "environment-test", observedAt: expect.any(String), providers: [
      { instanceId: "first", models: [{ slug: "default-model", isDefault: true }, { slug: "custom", isCustom: true, isLegacy: true, aliases: ["alias"], subProvider: "custom-provider" }], usage: { status: "reported", checkedAt, windows: [{ remainingPercent: 100, resetsAt: "2026-10-23T10:00:00Z", windowDurationMins: 10080 }, { remainingPercent: 0 }], resetCredits: { availableCount: 2, nextExpiresAt: checkedAt } } },
      { instanceId: "second", driver: "codex", usage: { status: "missing", windows: [] } },
    ] });
    const output = result.structuredContent as { providers: Array<{ models: Array<Record<string, unknown>>; usage: { windows: Array<Record<string, unknown>> } }> };
    expect(output.providers[0]!.models.every((model) => !("usage" in model) && !("windows" in model))).toBe(true);
    expect(output.providers[0]!.usage.windows[1]).not.toHaveProperty("resetsAt");
    for (const secret of ["secret-", "hidden-", "capabilities", "credentialFingerprint", "nextCreditId", "externalUsage", "auth", "settings", "config"]) {
      expect(JSON.stringify(result)).not.toContain(secret);
    }
    expect(calls).toEqual([expect.objectContaining({ tag: "subscribeServerConfig", payload: { usageLimitSources: true } })]);
    expect(paths).toEqual(["/.well-known/t3/environment", "/ws?orchestrationProtocol=2"]);
    const audit = await readFile(`${fixture.directory}/audit.jsonl`, "utf8");
    expect(audit).toContain('"operation":"subscribeServerConfig"');
    expect(audit).not.toContain("secret-");
  });

  it("keeps absent snapshots, unavailable reasons, retained windows and empty windows distinct", async () => {
    const { call } = await connected({ providers: [
      provider("missing"), provider("unsupported", { checkedAt, windows: [], unavailable: { reason: "unsupported" } }),
      provider("failed", { checkedAt, windows: [window], unavailable: { reason: "probeFailed", message: "private failure" } }),
      provider("empty", { checkedAt, windows: [] }),
    ] });
    const result = await call();
    expect(result.isError).not.toBe(true);
    expect(result.structuredContent).toMatchObject({ providers: [
      { usage: { status: "missing", windows: [] } },
      { usage: { status: "unsupported", unavailable: { reason: "unsupported" }, windows: [] } },
      { usage: { status: "probeFailed", unavailable: { reason: "probeFailed" }, checkedAt, windows: [{ ...window, remainingPercent: 75 }] } },
      { usage: { status: "empty", checkedAt, windows: [] } },
    ] });
    const missing = (result.structuredContent as { providers: Array<{ usage: object }> }).providers[0]!.usage;
    expect(missing).not.toHaveProperty("checkedAt");
  });

  it("filters exact instance IDs, rejects unknown instances and empty arguments, and reads each call anew", async () => {
    const providers = [provider("first"), provider("second")];
    const { call, calls } = await connected({ providers });
    expect((await call({ providerInstanceId: "second" })).structuredContent).toMatchObject({ providers: [{ instanceId: "second" }] });
    providers[0]!.displayName = "Updated name";
    const selected = await call({ providerInstanceId: "first" });
    expect(selected.structuredContent).toMatchObject({ providers: [{ instanceId: "first", displayName: "Updated name" }] });
    expect((selected.structuredContent as { providers: unknown[] }).providers).toHaveLength(1);
    expect((await call({ providerInstanceId: "codex" })).structuredContent).toMatchObject({ error: { code: "provider_instance_not_found" } });
    expect((await call({ providerInstanceId: " " })).isError).toBe(true);
    expect(calls).toHaveLength(3);
  });

  it("checks environment identity before reading config", async () => {
    const { call, paths, calls } = await connected({ providers: [] }, { environmentId: "another-environment" });
    expect((await call()).structuredContent).toMatchObject({ error: { code: "environment_mismatch" } });
    expect(paths).toEqual(["/.well-known/t3/environment"]);
    expect(calls).toEqual([]);
  });

  it.each([
    { name: "missing catalog", config: {} },
    { name: "malformed catalog", config: { providers: [{}] } },
    { name: "out of range quota", config: { providers: [provider("bad", { checkedAt, windows: [{ ...window, usedPercent: 101 }] })] } },
    { name: "invalid quota time", config: { providers: [provider("bad", { checkedAt: "yesterday", windows: [] })] } },
    { name: "missing source time", config: { providers: [provider("bad", { windows: [] })] } },
    { name: "positional auth", config: { providers: [{ ...provider("bad"), auth: ["private-label"] }] } },
  ])("returns schema errors for $name", async ({ config }) => {
    const { call } = await connected(config);
    expect(await call()).toMatchObject({ isError: true, structuredContent: { error: { code: "gateway_error", message: expect.stringContaining("invalid response") } } });
  });

  it.each(["auth", "rpc", "disconnect", "malformed"] as const)("keeps %s failures as errors", async (mode) => {
    const { call } = await connected({ providers: [] }, { mode });
    const result = await call();
    expect(result).toMatchObject({ isError: true, structuredContent: { error: { code: "gateway_error" } } });
    expect(result.structuredContent).not.toHaveProperty("providers");
    const message = mode === "auth" ? "HTTP 403" : mode === "rpc" ? "orchestration:read" : mode === "disconnect" ? "disconnected" : "invalid response";
    expect(result.structuredContent).toMatchObject({ error: { message: expect.stringContaining(message) } });
  });

  it("allows a valid empty catalog", async () => {
    const { call } = await connected({ providers: [] });
    expect(await call()).toMatchObject({ structuredContent: { providers: [] } });
  });
  it("returns source-only accounts, safe labels and exact native filtering without inferred allowances", async () => {
    const limits = { checkedAt, windows: [{ ...window, id: "primary", kind: "other", label: "Primary", usedPercent: 18, windowDurationMins: 10080 }], unavailable: { reason: "probeFailed", message: "private-probe" }, credentialFingerprint: "private-fingerprint", resetCredits: { availableCount: 1, nextCreditId: "private-credit" } };
    const source = { id: "proxy/private", kind: "future-source", label: "CLIProxy", checkedAt, error: "token=private-token https://private.example/secret", accounts: [{ id: "private@example.com", driver: "future-driver", email: "private@example.com", workspaceId: "private-workspace", plan: "Pro", usageLimits: limits }] };
    const native = { ...provider("native", limits), auth: { label: "ChatGPT Pro 20x Subscription", token: "private-auth" }, plan: "private-native-plan" };
    const { call, calls } = await connected({ providers: [native, provider("other")] }, { sources: [source, { ...source, id: "other-source" }] });
    const result = await call({ providerInstanceId: " native " });
    expect(result.isError).not.toBe(true);
    expect(result.structuredContent).toMatchObject({
      refresh: { status: "notRequested" },
      providers: [{ instanceId: "native", accountLabel: "ChatGPT Pro 20x Subscription", usage: { status: "probeFailed", windows: [{ kind: "other", remainingPercent: 82, windowDurationMins: 10080 }] } }],
      sources: { status: "reported", items: [{ sourceRef: "src_558de6b776f54b2e", kind: "future-source", label: "CLIProxy", checkedAt, accounts: [{ accountRef: "acct_0ed7811796e95495", driver: "future-driver", plan: "Pro", usage: { status: "probeFailed", windows: [{ kind: "other", remainingPercent: 82 }] } }] }, expect.anything()] },
    });
    expect((result.structuredContent as { providers: unknown[] }).providers).toHaveLength(1);
    for (const privateText of ["proxy/private", "private@", "private-", "workspaceId", "credentialFingerprint", "nextCreditId", '"auth"', '"plan":"private-native-plan"']) expect(JSON.stringify(result)).not.toContain(privateText);
    expect((result.structuredContent as { sources: { items: unknown[] } }).sources.items).toHaveLength(2);
    expect(result.structuredContent).toMatchObject({ sources: { items: [expect.anything(), { sourceRef: "src_e264ed6acf0e847e", accounts: [{ accountRef: "acct_732c733411f2e4be" }] }] } });
    const again = await call();
    expect(ModelsUsageOutputSchema.parse(again.structuredContent).sources).toEqual(ModelsUsageOutputSchema.parse(result.structuredContent).sources);
    expect(calls).toHaveLength(2);
    const onlySources = await connected({ providers: [] }, { sources: [source] });
    expect((await onlySources.call()).structuredContent).toMatchObject({ providers: [], sources: { status: "reported", items: [{ accounts: [{ plan: "Pro" }] }] } });
  });

  it("accepts sources first and processes all values and provider status updates before completion", async () => {
    const { call } = await connected({}, { events: [
      { version: 1, type: "usageLimitSourcesUpdated", payload: { sources: [] } },
    ], followingEvents: [
      { version: 1, type: "snapshot", config: { providers: [provider("before")] } },
      { version: 1, type: "providerStatuses", payload: { providers: [provider("after", { checkedAt, windows: [window] })] } },
    ] });
    expect((await call()).structuredContent).toMatchObject({ providers: [{ instanceId: "after", usage: { windows: [{ remainingPercent: 75 }] } }], sources: { status: "reported", items: [] } });
  });

  it.each(["end", "disconnect"] as const)("keeps missing sources distinct after %s", async (afterSnapshot) => {
    const { call } = await connected({ providers: [] }, { afterSnapshot });
    expect((await call()).structuredContent).toMatchObject({ providers: [], sources: { status: "unavailable", reason: afterSnapshot === "end" ? "streamEnded" : "disconnected" } });
    const empty = await connected({ providers: [] });
    expect((await empty.call()).structuredContent).toMatchObject({ sources: { status: "reported", items: [] } });
  });

  it("returns partial quotas on deadline and propagates explicit cancellation", async () => {
    const { rpcClient } = await connected({ providers: [provider("native")] }, { afterSnapshot: "timeout" });
    expect(await rpcClient.getUsageLimits()).toMatchObject({ providers: [{ instanceId: "native" }], sources: { status: "unavailable", reason: "timeout" } });
    const absent = await connected({}, { events: [], afterSnapshot: "timeout" });
    await expect(absent.rpcClient.getUsageLimits()).rejects.toThrow("without a provider snapshot");
    const other = await connected({ providers: [provider("native")] }, { afterSnapshot: "timeout" });
    const abort = new AbortController();
    const cancelled = other.rpcClient.getUsageLimits({}, abort.signal);
    await other.snapshotReceived;
    abort.abort(new Error("explicit cancellation"));
    await expect(cancelled).rejects.toThrow("explicit cancellation");
  });

  it.each(["rpc", "defect", "malformed"] as const)("does not hide %s errors after a provider snapshot", async (afterSnapshot) => {
    const { call } = await connected({ providers: [] }, { afterSnapshot });
    expect(await call()).toMatchObject({ isError: true, structuredContent: { error: { code: "gateway_error" } } });
  });

  it("refreshes once and keeps sanitized refresh failure beside a valid snapshot", async () => {
    for (const refreshError of [undefined, "token=private-token https://private.example/secret"]) {
      const { call, calls, fixture } = await connected({ providers: [] }, { refreshError });
      const result = await call({ refresh: true });
      expect(result.isError).not.toBe(true);
      expect(result.structuredContent).toMatchObject({ refresh: { status: refreshError ? "failed" : "completed" }, providers: [], sources: { status: "reported", items: [] } });
      expect(calls).toEqual([
        expect.objectContaining({ tag: "server.refreshProviders", payload: {} }),
        expect.objectContaining({ tag: "subscribeServerConfig", payload: { usageLimitSources: true } }),
      ]);
      if (refreshError) {
        expect(result.structuredContent).toMatchObject({ refresh: { error: expect.stringContaining("[REDACTED]") } });
        expect(JSON.stringify(result)).not.toContain("private-");
      }
      expect(await readFile(`${fixture.directory}/audit.jsonl`, "utf8")).toContain('"operation":"server.refreshProviders"');
    }
  });

  it.each([
    null, [null], [[]], [{ id: "proxy", kind: "cliproxy", label: "Proxy", checkedAt, accounts: [null] }],
    [{ id: "proxy", kind: "cliproxy", label: "Proxy", checkedAt, accounts: [["account", "codex"]] }],
    [{ id: "proxy", kind: "cliproxy", label: "Proxy", checkedAt, accounts: [{ id: "account", driver: "codex", usageLimits: [checkedAt, []] }] }],
  ].map((sources) => ({ sources })))("rejects malformed external source boundaries %#", async ({ sources }) => {
    const { call } = await connected({ providers: [] }, { events: [
      { version: 1, type: "snapshot", config: { providers: [] } },
      { version: 1, type: "usageLimitSourcesUpdated", payload: { sources } },
    ] });
    expect(await call()).toMatchObject({ isError: true, structuredContent: { error: { message: expect.stringContaining("invalid response") } } });
  });

  it("sanitizes permitted labels and plans without exposing private text", async () => {
    const privateText = "token=private-value https://private.example/secret";
    const { call } = await connected({ providers: [{ ...provider("native"), auth: { label: privateText } }] }, { sources: [{
      id: "source", kind: "cliproxy", label: privateText, checkedAt,
      accounts: [{ id: "account", driver: "codex", plan: privateText, usageLimits: { checkedAt, windows: [] } }],
    }] });
    const result = await call();
    expect(result.isError).not.toBe(true);
    expect(result.structuredContent).toMatchObject({
      providers: [{ accountLabel: expect.stringContaining("[REDACTED]") }],
      sources: { items: [{ label: expect.stringContaining("[REDACTED]"), accounts: [{ plan: expect.stringContaining("[REDACTED]") }] }] },
    });
    expect(JSON.stringify(result)).not.toContain("private-");
  });

});

it("returns cached snapshots after a refresh timeout", async () => {
  const { rpcClient, calls } = await connected({ providers: [provider("cached")] }, { refreshStall: true });
  const result = await rpcClient.getUsageLimits({ refresh: true });
  expect(result).toMatchObject({ refresh: { status: "failed" }, providers: [{ instanceId: "cached" }], sources: { status: "reported", items: [] } });
  expect(calls).toEqual([
    expect.objectContaining({ tag: "server.refreshProviders" }),
    expect.objectContaining({ tag: "subscribeServerConfig" }),
  ]);
});
