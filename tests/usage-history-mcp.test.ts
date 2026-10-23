import { createServer } from "node:http";
import { once } from "node:events";
import type { AddressInfo } from "node:net";
import { WebSocketServer } from "ws";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { afterEach, describe, expect, it } from "vitest";
import { createMcpServer } from "../src/mcp/server.js";
import { gatewayFixture } from "./support/gateway-fixture.js";
import { FakeT3 } from "./support/fake-t3.js";

const cleanup: Array<() => Promise<unknown>> = [];
afterEach(async () => { for (const close of cleanup.splice(0).reverse()) await close(); });
const input = { sinceDay: "2026-10-23", untilDay: "2026-10-23", timeZone: "UTC" };
const summary = {
  contractVersion: 6, readAt: "2026-10-23T13:00:00Z", ...input,
  buckets: [{ day: input.sinceDay, provider: "codex", model: "gpt-6.1-sol", sourcePath: "/private/transcript",
    totals: { uncachedInputTokens: 10, cachedInputTokens: 20, cacheCreationTokens: 0, outputTokens: 30, reasoningTokens: 15 },
    costUsd: 0.5, cacheSavingsUsd: 0.1, costSource: "modelPriced", records: 2, unpricedRecords: 0, sessions: 1 }],
  sources: [{ fingerprint: { hostId: "private-host", provider: "codex", resolvedHomePath: "/private/codex", volumeId: "private-volume" },
    status: "ok", scannedFiles: 1, skippedFiles: 0, malformedRecords: 0, distinctSessions: 1, message: null, refreshing: true }],
  pricing: { status: "cached", source: "provider pricing", fetchedAt: null, knownModels: 1 }, scanDurationMs: 5,
};

async function connected(options: { denied?: boolean; environmentId?: string } = {}) {
  const fake = new FakeT3({ scopes: ["orchestration:read", "diagnostics:read"] });
  await fake.start();
  cleanup.push(() => fake.close());
  const calls: Array<{ tag: string; payload: unknown }> = [];
  const server = createServer(async (request, response) => {
    const upstream = await fetch(`${fake.baseUrl}${request.url}`);
    response.writeHead(upstream.status, { "content-type": "application/json" });
    response.end(await upstream.text());
  });
  const ws = new WebSocketServer({ noServer: true });
  server.on("upgrade", (request, socket, head) => {
    expect(request.headers.authorization).toBe(`Bearer ${fake.accessToken}`);
    ws.handleUpgrade(request, socket, head, connection => {
      connection.on("message", data => {
        const call = JSON.parse(data.toString());
        calls.push(call);
        const exit = options.denied
          ? { _tag: "Failure", cause: [{ _tag: "Fail", error: { _tag: "EnvironmentAuthorizationError", message: "The authenticated token is missing required scope: diagnostics:read.", requiredScope: "orchestration:read", requiredPermission: "diagnostics:read" } }] }
          : { _tag: "Success", value: summary };
        connection.send(JSON.stringify({ _tag: "Exit", requestId: call.id, exit }));
      });
    });
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  cleanup.push(async () => {
    for (const connection of ws.clients) connection.terminate();
    await new Promise<void>(resolve => ws.close(() => resolve()));
    server.close();
    await once(server, "close");
  });
  const fixture = await gatewayFixture(fake, {
    t3HttpBaseUrl: `http://127.0.0.1:${(server.address() as AddressInfo).port}`,
    environmentId: options.environmentId ?? null, readOnly: true,
  });
  cleanup.push(() => fixture.cleanup());
  const mcp = createMcpServer(fixture.gateway);
  const client = new Client({ name: "usage-history-boundary-test", version: "1" });
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await mcp.connect(serverTransport);
  await client.connect(clientTransport);
  cleanup.push(() => mcp.close(), () => client.close());
  return { client, calls, call: (args: unknown = input) => client.callTool({ name: "t3_usage_history", arguments: args as Record<string, unknown> }) };
}

describe("Usage tab history through public MCP and authenticated RPC", () => {
  it("advertises in read-only mode and preserves the Usage tab's token and cost data", async () => {
    const { client, calls, call } = await connected();
    const tool = (await client.listTools()).tools.find(tool => tool.name === "t3_usage_history");
    expect(tool?.annotations?.readOnlyHint).toBe(true);
    const result = await call();
    expect(result.isError).not.toBe(true);
    expect(result.structuredContent).toMatchObject({
      environmentId: "environment-test", contractVersion: 6, ...input,
      buckets: [{ totals: summary.buckets[0]!.totals, costUsd: 0.5, sessions: 1 }],
      sources: [{ provider: "codex", distinctSessions: 1, refreshing: true }],
    });
    expect(calls).toEqual([expect.objectContaining({ tag: "server.getUsageSummary", payload: input })]);
    const text = JSON.stringify(result);
    for (const privateField of ["/private/", "private-host", "private-volume", "sourcePath", "resolvedHomePath", "volumeId"]) expect(text).not.toContain(privateField);
  });

  it("keeps diagnostics permission failures distinct from successful empty history", async () => {
    const { call } = await connected({ denied: true });
    const result = await call();
    expect(result.isError).toBe(true);
    expect(JSON.stringify(result)).toContain("diagnostics:read");
    expect(result.structuredContent).not.toHaveProperty("buckets");
  });

  it("rejects an invalid or oversized time window before it reaches T3", async () => {
    const { calls, call } = await connected();
    for (const args of [
      { ...input, sinceDay: "2026-02-30" },
      { ...input, sinceDay: "2026-10-24" },
      { ...input, sinceDay: "2024-01-01" },
    ]) {
      const result = await call(args);
      expect(result.isError).toBe(true);
    }
    expect(calls).toEqual([]);
  });

  it("checks the environment before reading history", async () => {
    const { calls, call } = await connected({ environmentId: "another-environment" });
    const result = await call();
    expect(result.isError).toBe(true);
    expect(JSON.stringify(result)).toContain("environment_mismatch");
    expect(calls).toEqual([]);
  });
});
