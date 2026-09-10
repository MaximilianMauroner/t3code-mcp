import { timingSafeEqual } from "node:crypto";
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { StreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/streamableHttp.js";
import type { GatewayConfig } from "../config.js";
import { T3Gateway } from "../gateway.js";
import { createMcpServer } from "./server.js";

const MAX_BODY_BYTES = 2 * 1024 * 1024;

export async function runStdio(gateway: T3Gateway): Promise<void> {
  const server = createMcpServer(gateway);
  const transport = new StdioServerTransport();
  await server.connect(transport);
  await new Promise<void>((resolve) => {
    const finish = () => resolve();
    process.once("SIGINT", finish);
    process.once("SIGTERM", finish);
  });
  await server.close();
}

export async function listenHttp(gateway: T3Gateway, config: GatewayConfig): Promise<Server> {
  const server = createServer(async (request, response) => {
    await handleHttpRequest(request, response, gateway, config);
  });
  await new Promise<void>((resolve, reject) => {
    const onError = (error: Error) => reject(error);
    server.once("error", onError);
    server.listen(config.port, config.host, () => {
      server.off("error", onError);
      resolve();
    });
  });
  return server;
}

async function handleHttpRequest(
  request: IncomingMessage,
  response: ServerResponse,
  gateway: T3Gateway,
  config: GatewayConfig,
): Promise<void> {
  const url = new URL(request.url ?? "/", `http://${request.headers.host ?? "localhost"}`);
  if (url.pathname === "/healthz") {
    writeJson(response, 200, { ok: true, service: "t3-code-mcp" });
    return;
  }
  if (url.pathname !== "/mcp") {
    writeJson(response, 404, { error: "not_found" });
    return;
  }
  if (!authorized(request, config.mcpBearerToken)) {
    response.setHeader("www-authenticate", "Bearer");
    writeJson(response, config.mcpBearerToken === null ? 503 : 401, {
      error: config.mcpBearerToken === null ? "MCP_BEARER_TOKEN is not configured." : "Unauthorized.",
    });
    return;
  }

  let body: unknown;
  try {
    body = request.method === "POST" ? await readJsonBody(request) : undefined;
  } catch (error) {
    writeJson(response, 400, { error: error instanceof Error ? error.message : "Invalid request body." });
    return;
  }

  const server = createMcpServer(gateway);
  // The gateway creates a fresh server for each HTTP request. Keep the
  // transport stateless so a client is not handed a session ID that cannot be
  // resolved by the next request. Durable run identity lives in the journal.
  const transport = new StreamableHTTPServerTransport({
    enableJsonResponse: true,
    sessionIdGenerator: undefined,
  });
  try {
    await server.connect(transport);
    await transport.handleRequest(request, response, body);
  } catch (error) {
    if (!response.headersSent) {
      writeJson(response, 500, { error: error instanceof Error ? error.message : "MCP request failed." });
    } else {
      response.destroy(error instanceof Error ? error : undefined);
    }
  } finally {
    await server.close().catch(() => undefined);
  }
}

function authorized(request: IncomingMessage, expected: string | null): boolean {
  if (expected === null) {
    return false;
  }
  const header = request.headers.authorization;
  if (typeof header !== "string" || !header.startsWith("Bearer ")) {
    return false;
  }
  const supplied = Buffer.from(header.slice("Bearer ".length));
  const configured = Buffer.from(expected);
  return supplied.length === configured.length && timingSafeEqual(supplied, configured);
}

async function readJsonBody(request: IncomingMessage): Promise<unknown> {
  const chunks: Buffer[] = [];
  let size = 0;
  for await (const chunk of request) {
    const buffer = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    size += buffer.length;
    if (size > MAX_BODY_BYTES) {
      throw new Error("Request body is too large.");
    }
    chunks.push(buffer);
  }
  if (chunks.length === 0) {
    return undefined;
  }
  return JSON.parse(Buffer.concat(chunks).toString("utf8"));
}

function writeJson(response: ServerResponse, status: number, value: unknown): void {
  response.statusCode = status;
  response.setHeader("content-type", "application/json; charset=utf-8");
  response.end(JSON.stringify(value));
}
