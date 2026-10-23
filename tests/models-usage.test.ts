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

async function connected(config: unknown, options: { mode?: "auth" | "rpc" | "disconnect" | "malformed"; environmentId?: string; readOnly?: boolean } = {}) {
  const fake = new FakeT3({ scopes: ["orchestration:read"] });
  await fake.start();
  cleanup.push(() => fake.close());
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
      connection.on("message", (data) => {
        const call = JSON.parse(data.toString());
        calls.push(call);
        if (options.mode === "disconnect") { connection.close(); return; }
        if (options.mode === "malformed") { connection.send("not-json"); return; }
        const exit = options.mode === "rpc"
          ? { _tag: "Failure", cause: [{ _tag: "Fail", error: { message: "requires orchestration:read" } }] }
          : { _tag: "Success", value: config };
        connection.send(JSON.stringify({ _tag: "Exit", requestId: call.id, exit }));
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
  return { client, paths, calls, fixture, call: (args = {}) => client.callTool({ name: "t3_models_usage", arguments: args }) };
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
    expect(calls).toEqual([expect.objectContaining({ tag: "server.getConfig", payload: {} })]);
    expect(paths).toEqual(["/.well-known/t3/environment", "/ws?orchestrationProtocol=2"]);
    const audit = await readFile(`${fixture.directory}/audit.jsonl`, "utf8");
    expect(audit).toContain('"operation":"server.getConfig"');
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
});
