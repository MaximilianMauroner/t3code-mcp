import type { AddressInfo } from "node:net";
import type { Server } from "node:http";
import { afterEach, describe, expect, it } from "vitest";
import { listenHttp } from "../src/mcp/transport.js";
import { gatewayFixture, type GatewayFixture } from "./support/gateway-fixture.js";
import { FakeT3 } from "./support/fake-t3.js";

const fakes: FakeT3[] = [];
const fixtures: GatewayFixture[] = [];
const servers: Server[] = [];

afterEach(async () => {
  await Promise.all(
    servers.splice(0).map(
      (server) =>
        new Promise<void>((resolve) => {
          if (!server.listening) {
            resolve();
            return;
          }
          server.close(() => resolve());
        }),
    ),
  );
  await Promise.all(fixtures.splice(0).map((fixture) => fixture.cleanup()));
  await Promise.all(fakes.splice(0).map((fake) => fake.close()));
});

async function httpFixture(options: { readonly bearer?: string | null } = {}): Promise<{
  readonly fake: FakeT3;
  readonly fixture: GatewayFixture;
  readonly baseUrl: string;
}> {
  const fake = new FakeT3();
  fakes.push(fake);
  await fake.start();
  const fixture = await gatewayFixture(fake, { mcpBearerToken: options.bearer === undefined ? "gateway-test-token" : options.bearer });
  fixtures.push(fixture);
  const server = await listenHttp(fixture.gateway, fixture.config);
  servers.push(server);
  const address = server.address();
  if (!address || typeof address === "string") {
    throw new Error("MCP test server did not expose a TCP port.");
  }
  return { fake, fixture, baseUrl: `http://127.0.0.1:${(address as AddressInfo).port}` };
}

async function request(baseUrl: string, path: string, init: RequestInit = {}): Promise<Response> {
  return fetch(`${baseUrl}${path}`, init);
}

function mcpRequest(id: number, method: string, params: unknown): RequestInit {
  return {
    method: "POST",
    headers: {
      authorization: "Bearer gateway-test-token",
      accept: "application/json, text/event-stream",
      "content-type": "application/json",
    },
    body: JSON.stringify({ jsonrpc: "2.0", id, method, params }),
  };
}

describe("Streamable HTTP gateway boundary", () => {
  it("keeps health and routing behavior separate from MCP authentication", async () => {
    const { baseUrl } = await httpFixture();

    const health = await request(baseUrl, "/healthz");
    const missing = await request(baseUrl, "/not-found");
    const unauthorized = await request(baseUrl, "/mcp", {
      ...mcpRequest(1, "initialize", {}),
      headers: { authorization: "Bearer wrong-token", accept: "application/json, text/event-stream", "content-type": "application/json" },
    });

    expect(health.status).toBe(200);
    expect(await health.json()).toEqual({ ok: true, service: "t3-code-mcp" });
    expect(missing.status).toBe(404);
    expect(unauthorized.status).toBe(401);
    expect(unauthorized.headers.get("www-authenticate")).toBe("Bearer");
    expect(await unauthorized.json()).toEqual({ error: "Unauthorized." });
  });

  it("supports independent stateless MCP clients and never returns T3 credentials", async () => {
    const { fake, baseUrl } = await httpFixture();
    fake.addProject({ id: "project-http", title: "HTTP project" });
    const initializeParams = {
      protocolVersion: "2025-06-18",
      capabilities: {},
      clientInfo: { name: "http-test", version: "1.0.0" },
    };

    const [firstInit, secondInit] = await Promise.all([
      request(baseUrl, "/mcp", mcpRequest(1, "initialize", initializeParams)),
      request(baseUrl, "/mcp", mcpRequest(2, "initialize", initializeParams)),
    ]);
    const firstInitBody = await firstInit.json() as Record<string, any>;
    const secondInitBody = await secondInit.json() as Record<string, any>;
    const tools = await request(baseUrl, "/mcp", mcpRequest(3, "tools/list", {}));
    const call = await request(baseUrl, "/mcp", mcpRequest(4, "tools/call", {
      name: "t3_projects_list",
      arguments: { limit: 10 },
    }));
    const callText = JSON.stringify(await call.json());

    expect(firstInit.status).toBe(200);
    expect(secondInit.status).toBe(200);
    expect(firstInitBody.result?.serverInfo?.name).toBe("t3-code-mcp");
    expect(secondInitBody.result?.serverInfo?.name).toBe("t3-code-mcp");
    expect(tools.status).toBe(200);
    expect(call.status).toBe(200);
    expect(callText).toContain("project-http");
    expect(callText).not.toContain(fake.accessToken);
    expect(firstInit.headers.get("mcp-session-id")).toBeNull();
    expect(secondInit.headers.get("mcp-session-id")).toBeNull();
  });

  it("returns protocol errors for malformed JSON and oversized bodies", async () => {
    const { baseUrl } = await httpFixture();
    const malformed = await request(baseUrl, "/mcp", {
      method: "POST",
      headers: { authorization: "Bearer gateway-test-token", "content-type": "application/json" },
      body: "{not-json",
    });
    const oversized = await request(baseUrl, "/mcp", {
      method: "POST",
      headers: { authorization: "Bearer gateway-test-token", "content-type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize", params: { padding: "x".repeat(2 * 1024 * 1024) } }),
    });

    expect(malformed.status).toBe(400);
    expect(await malformed.json()).toMatchObject({ error: expect.stringContaining("JSON") });
    expect(oversized.status).toBe(400);
    expect(await oversized.json()).toMatchObject({ error: "Request body is too large." });
  });

  it("fails closed when the gateway bearer token is not configured", async () => {
    const { baseUrl } = await httpFixture({ bearer: null });

    const response = await request(baseUrl, "/mcp", mcpRequest(1, "initialize", {}));

    expect(response.status).toBe(503);
    expect(await response.json()).toEqual({ error: "MCP_BEARER_TOKEN is not configured." });
  });
});
