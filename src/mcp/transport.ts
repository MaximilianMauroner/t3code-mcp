import { randomUUID, timingSafeEqual } from "node:crypto";
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { StreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/streamableHttp.js";
import type { GatewayConfig } from "../config.js";
import { T3Gateway } from "../gateway.js";
import { summarizeForAudit } from "../operations/audit-log.js";
import { createMcpServer } from "./server.js";

const MAX_BODY_BYTES = 2 * 1024 * 1024;

export async function runStdio(gateway: T3Gateway): Promise<void> {
  const correlationId = `stdio_${randomUUID()}`;
  const startedAt = Date.now();
  await gateway.audit.record({
    source: "transport",
    event: "stdio.session",
    correlationId,
    outcome: "started",
    details: { transport: "stdio" },
  });
  const server = createMcpServer(gateway);
  const transport = new StdioServerTransport();
  try {
    await server.connect(transport);
    const onmessage = transport.onmessage;
    transport.onmessage = (message) => {
      void gateway.audit.record({
        source: "transport",
        event: "stdio.message",
        correlationId,
        operation: "stdio",
        outcome: "received",
        details: mcpRequestDetails(message),
      });
      onmessage?.(message);
    };
    const onerror = transport.onerror;
    transport.onerror = (error) => {
      void gateway.audit.record({
        source: "transport",
        event: "stdio.error",
        correlationId,
        operation: "stdio",
        outcome: "error",
        details: { error: error.message },
      });
      onerror?.(error);
    };
    await new Promise<void>((resolve) => {
      const finish = () => resolve();
      process.once("SIGINT", finish);
      process.once("SIGTERM", finish);
    });
  } finally {
    await server.close().catch(() => undefined);
    await gateway.audit.record({
      source: "transport",
      event: "stdio.session",
      correlationId,
      outcome: "completed",
      durationMs: Date.now() - startedAt,
      details: { transport: "stdio" },
    });
  }
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
  const address = server.address();
  await gateway.audit.record({
    source: "system",
    event: "http.listen",
    operation: "http",
    outcome: "started",
    details: {
      host: config.host,
      port: address && typeof address !== "string" ? address.port : config.port,
    },
  });
  return server;
}

async function handleHttpRequest(
  request: IncomingMessage,
  response: ServerResponse,
  gateway: T3Gateway,
  config: GatewayConfig,
): Promise<void> {
  const correlationId = `http_${randomUUID()}`;
  const startedAt = Date.now();
  const requestPath = (request.url ?? "/").split("?", 1)[0] || "/";
  let rpcDetails: Record<string, unknown> | null = null;
  let authOutcome: "not_checked" | "accepted" | "rejected" | "not_configured" = "not_checked";
  await gateway.audit.record({
    source: "transport",
    event: "http.request",
    correlationId,
    operation: requestPath,
    outcome: "started",
    details: {
      method: request.method,
      path: requestPath,
      contentLength: request.headers["content-length"] ?? null,
      remoteAddress: request.socket.remoteAddress ?? null,
      userAgent: request.headers["user-agent"] ?? null,
    },
  });

  try {
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
      authOutcome = config.mcpBearerToken === null ? "not_configured" : "rejected";
      response.setHeader("www-authenticate", "Bearer");
      writeJson(response, config.mcpBearerToken === null ? 503 : 401, {
        error: config.mcpBearerToken === null ? "MCP_BEARER_TOKEN is not configured." : "Unauthorized.",
      });
      return;
    }

    authOutcome = "accepted";
    let body: unknown;
    try {
      body = request.method === "POST" ? await readJsonBody(request) : undefined;
    } catch (error) {
      writeJson(response, 400, { error: error instanceof Error ? error.message : "Invalid request body." });
      return;
    }
    rpcDetails = mcpRequestDetails(body);

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
  } catch (error) {
    if (!response.headersSent) {
      writeJson(response, 500, { error: error instanceof Error ? error.message : "MCP request failed." });
    } else {
      response.destroy(error instanceof Error ? error : undefined);
    }
  } finally {
    await gateway.audit.record({
      source: "transport",
      event: "http.response",
      correlationId,
      operation: requestPath,
      outcome: response.statusCode >= 400 ? "error" : "completed",
      durationMs: Date.now() - startedAt,
      details: {
        method: request.method,
        path: requestPath,
        status: response.statusCode,
        authOutcome,
        rpc: rpcDetails,
      },
    });
  }
}

function mcpRequestDetails(body: unknown): Record<string, unknown> {
  if (Array.isArray(body)) {
    return { batch: true, batchLength: body.length };
  }
  if (typeof body !== "object" || body === null) {
    return { batch: false, validRequestShape: false };
  }
  const request = body as Record<string, unknown>;
  const params = typeof request.params === "object" && request.params !== null
    ? request.params as Record<string, unknown>
    : null;
  const clientInfo = params && typeof params.clientInfo === "object" && params.clientInfo !== null
    ? summarizeForAudit(params.clientInfo)
    : null;
  return {
    batch: false,
    jsonRpcMethod: typeof request.method === "string" ? request.method : null,
    requestId: typeof request.id === "string" || typeof request.id === "number" ? request.id : null,
    toolName: params && typeof params.name === "string" ? params.name : null,
    arguments: params?.arguments === undefined ? null : summarizeForAudit(params.arguments),
    clientInfo,
  };
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
