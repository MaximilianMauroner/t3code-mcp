import { createHash, randomUUID } from "node:crypto";
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/streamableHttp.js";
import { z } from "zod";

export interface NativeToolCall {
  readonly name: string;
  readonly arguments: Record<string, unknown>;
}

/**
 * A stand-in for T3's native MCP endpoint: OAuth pairing-code login, a stateful
 * Streamable HTTP MCP server behind a bearer check, and the signed upload route.
 */
export class FakeNativeMcp {
  readonly calls: NativeToolCall[] = [];
  readonly uploads: Array<{ readonly path: string; readonly contentType: string | undefined; readonly bytes: Buffer }> = [];
  readonly decisions: Array<{ readonly access: unknown; readonly code: unknown }> = [];
  /** The bearer token that /mcp accepts and a login issues. Rotating it simulates a new login. */
  token = "native-token-1";
  /** Earlier tokens that /mcp still accepts, as T3 keeps an old session after a new login. */
  acceptedTokens: ReadonlyArray<string> = [];
  /** The bearer token of each tools/call request, in order. */
  readonly toolCallTokens: string[] = [];
  pairingCode = "PAIR-1234";
  expiresInSeconds = 30 * 24 * 3600;
  /** Destroy the socket instead of answering the next tools/call; the tool still runs. */
  dropNextToolResponse = false;
  /** With `dropNextToolResponse`, also forget every MCP session, as a T3 restart does. */
  restartOnDrop = false;
  /** Leave every new /mcp request unanswered, as a hung T3 does. */
  stallRequests = false;
  /** End each t3_thread_wait after this many ms, as a run that ends does. When null, a wait uses its whole budget. */
  runEndsAfterMs: number | null = null;
  /** Hold each authorized tools/list request until `releaseToolLists` is called. */
  holdToolLists = false;
  /** Hold each call to this tool after it is recorded, until `releaseToolCalls` is called. */
  holdToolCalls: string | null = null;
  /** Tools that new sessions do not offer, as in an older T3 build. */
  omitTools: ReadonlyArray<string> = [];
  /** /mcp requests refused because they did not carry `token`. */
  unauthorizedRequests = 0;
  /** Fail the next upload with this HTTP status. */
  failNextUpload: number | null = null;
  /** Answer the next upload with a 307 redirect to this URL. */
  redirectNextUpload: string | null = null;
  private server: Server | null = null;
  private readonly sessions = new Map<string, StreamableHTTPServerTransport>();
  private readonly heldToolLists: Array<() => void> = [];
  private readonly heldToolCalls: Array<() => void> = [];
  private readonly authorizations = new Map<string, { readonly challenge: string; readonly clientId: string; readonly scope: string }>();

  get baseUrl(): string {
    const address = this.server?.address() as AddressInfo | null | undefined;
    if (!address) throw new Error("Fake native MCP is not listening.");
    return `http://127.0.0.1:${address.port}`;
  }

  get mcpUrl(): string {
    return `${this.baseUrl}/mcp`;
  }

  async start(): Promise<void> {
    this.server = createServer((request, response) => {
      void this.handle(request, response).catch((error: unknown) => {
        if (!response.headersSent) json(response, 500, { error: error instanceof Error ? error.message : String(error) });
      });
    });
    await new Promise<void>((resolve) => this.server?.listen(0, "127.0.0.1", resolve));
  }

  /** Forget every MCP session, as a T3 restart does. */
  dropSessions(): void {
    this.sessions.clear();
  }

  /** tools/list requests that `holdToolLists` holds. */
  get heldToolListCount(): number {
    return this.heldToolLists.length;
  }

  /** Answer every held tools/list request. */
  releaseToolLists(): void {
    for (const release of this.heldToolLists.splice(0)) release();
  }

  /** Tool calls that `holdToolCalls` holds. */
  get heldToolCallCount(): number {
    return this.heldToolCalls.length;
  }

  /** Let every held tool call finish. */
  releaseToolCalls(): void {
    for (const release of this.heldToolCalls.splice(0)) release();
  }

  async close(): Promise<void> {
    await Promise.all([...this.sessions.values()].map((transport) => transport.close()));
    this.server?.closeAllConnections();
    await new Promise<void>((resolve) => (this.server ? this.server.close(() => resolve()) : resolve()));
  }

  private async handle(request: IncomingMessage, response: ServerResponse): Promise<void> {
    const url = new URL(request.url ?? "/", this.baseUrl);
    const body = await readBody(request);
    if (url.pathname === "/.well-known/oauth-protected-resource/mcp") {
      json(response, 200, { resource: this.mcpUrl, authorization_servers: [this.baseUrl] });
      return;
    }
    if (url.pathname === "/.well-known/oauth-authorization-server") {
      json(response, 200, {
        issuer: this.baseUrl,
        authorization_endpoint: `${this.baseUrl}/oauth/mcp/authorize`,
        token_endpoint: `${this.baseUrl}/oauth/mcp/token`,
        registration_endpoint: `${this.baseUrl}/oauth/mcp/register`,
      });
      return;
    }
    if (url.pathname === "/oauth/mcp/register") {
      json(response, 201, { client_id: `client-${randomUUID()}`, ...JSON.parse(body.toString("utf8")) });
      return;
    }
    if (url.pathname === "/oauth/mcp/decision") {
      this.decide(JSON.parse(body.toString("utf8")), response);
      return;
    }
    if (url.pathname === "/oauth/mcp/token") {
      this.exchange(new URLSearchParams(body.toString("utf8")), response);
      return;
    }
    if (url.pathname.startsWith("/api/attachments/upload/")) {
      if (this.failNextUpload !== null) {
        const status = this.failNextUpload;
        this.failNextUpload = null;
        response.writeHead(status).end("upload refused");
        return;
      }
      if (this.redirectNextUpload !== null) {
        response.writeHead(307, { location: this.redirectNextUpload }).end();
        this.redirectNextUpload = null;
        return;
      }
      this.uploads.push({ path: url.pathname, contentType: request.headers["content-type"], bytes: body });
      response.writeHead(204).end();
      return;
    }
    if (url.pathname === "/mcp") {
      if (!this.stallRequests) await this.handleMcp(request, response, body);
      return;
    }
    json(response, 404, { error: "not_found" });
  }

  private decide(input: { authorization: Record<string, string>; decision: { _tag: string; access: unknown; code: unknown } }, response: ServerResponse): void {
    this.decisions.push({ access: input.decision.access, code: input.decision.code });
    const redirect = new URL(input.authorization.redirect_uri ?? "http://127.0.0.1/callback");
    if (input.decision._tag !== "pairing-code" || input.decision.code !== this.pairingCode) {
      redirect.searchParams.set("error", "access_denied");
    } else {
      const code = `code-${randomUUID()}`;
      this.authorizations.set(code, {
        challenge: input.authorization.code_challenge ?? "",
        clientId: input.authorization.client_id ?? "",
        scope: input.decision.access === "read-only" ? "orchestration:read" : "orchestration:read orchestration:operate",
      });
      redirect.searchParams.set("code", code);
    }
    redirect.searchParams.set("state", input.authorization.state ?? "");
    json(response, 200, { redirectTo: redirect.toString() });
  }

  private exchange(form: URLSearchParams, response: ServerResponse): void {
    const grant = this.authorizations.get(form.get("code") ?? "");
    const verifier = form.get("code_verifier") ?? "";
    if (grant === undefined || grant.clientId !== form.get("client_id") ||
      createHash("sha256").update(verifier).digest("base64url") !== grant.challenge) {
      json(response, 400, { error: "invalid_grant", error_description: "PKCE or code mismatch" });
      return;
    }
    this.authorizations.delete(form.get("code") ?? "");
    json(response, 200, { access_token: this.token, token_type: "Bearer", expires_in: this.expiresInSeconds, scope: grant.scope });
  }

  private async handleMcp(request: IncomingMessage, response: ServerResponse, body: Buffer): Promise<void> {
    const token = [this.token, ...this.acceptedTokens].find((accepted) => request.headers.authorization === `Bearer ${accepted}`);
    if (token === undefined) {
      this.unauthorizedRequests += 1;
      json(response, 401, { error: "invalid_mcp_credential" });
      return;
    }
    const parsed: unknown = body.length === 0 ? undefined : JSON.parse(body.toString("utf8"));
    if (isRequest(parsed, "tools/call")) this.toolCallTokens.push(token);
    const sessionId = request.headers["mcp-session-id"];
    let transport = typeof sessionId === "string" ? this.sessions.get(sessionId) : undefined;
    if (transport === undefined) {
      if (typeof sessionId === "string") {
        json(response, 404, { jsonrpc: "2.0", error: { code: -32001, message: "Session not found" }, id: null });
        return;
      }
      const created = new StreamableHTTPServerTransport({
        sessionIdGenerator: () => randomUUID(),
        enableJsonResponse: true,
        onsessioninitialized: (id) => {
          this.sessions.set(id, created);
        },
      });
      await this.mcpServer().connect(created);
      transport = created;
    }
    if (this.holdToolLists && isRequest(parsed, "tools/list")) {
      await new Promise<void>((resolve) => this.heldToolLists.push(resolve));
    }
    if (this.dropNextToolResponse && isRequest(parsed, "tools/call")) {
      const restart = this.restartOnDrop;
      this.dropNextToolResponse = false;
      this.restartOnDrop = false;
      // Let the tool run, then lose its answer.
      response.writeHead = () => response;
      response.end = () => {
        if (restart) this.dropSessions();
        request.socket.destroy();
        return response;
      };
    }
    await transport.handleRequest(request, response, parsed);
  }

  private mcpServer(): McpServer {
    const server = new McpServer({ name: "Fake T3 Code", version: "0.0.0-test" });
    const record = async (name: string, args: Record<string, unknown>): Promise<void> => {
      this.calls.push({ name, arguments: args });
      if (this.holdToolCalls === name) await new Promise<void>((resolve) => this.heldToolCalls.push(resolve));
    };
    const loose = (shape: z.ZodRawShape) => z.looseObject(shape);
    const register: McpServer["registerTool"] = (name, config, callback) => {
      const tool = server.registerTool(name, config, callback);
      if (this.omitTools.includes(name)) tool.remove();
      return tool;
    };
    register(
      "t3_thread_list",
      { description: "List threads in a project.", inputSchema: loose({ projectId: z.string(), limit: z.number().int().min(1).max(100).optional() }), annotations: { readOnlyHint: true } },
      async (args) => {
        await record("t3_thread_list", args);
        const value = { projectId: args.projectId, threads: [{ threadId: "thread-1", title: "Native thread" }] };
        return { content: [{ type: "text", text: JSON.stringify(value) }], structuredContent: value };
      },
    );
    register(
      "t3_thread_wait",
      { description: "Wait for a run to end.", inputSchema: loose({ threadId: z.string(), timeoutMs: z.number().optional() }), annotations: { readOnlyHint: true } },
      async (args) => {
        await record("t3_thread_wait", args);
        const endsAfterMs = this.runEndsAfterMs;
        await new Promise((resolve) => setTimeout(resolve, endsAfterMs ?? (typeof args.timeoutMs === "number" ? args.timeoutMs : 0)));
        const ended = endsAfterMs !== null;
        const value = { threadId: args.threadId, runId: "run-1", status: ended ? "completed" : "running", timedOut: !ended };
        return { content: [{ type: "text", text: JSON.stringify(value) }], structuredContent: value };
      },
    );
    register(
      "t3_thread_send",
      {
        description: "Send, queue, or steer a message.",
        inputSchema: loose({ threadId: z.string(), message: z.string(), mode: z.enum(["auto", "queue", "steer", "restart"]).nullable().optional() }),
        // Deliberately wrong, as some native annotations are: the gateway must not trust it.
        annotations: { readOnlyHint: true },
      },
      async (args) => {
        await record("t3_thread_send", args);
        const value = { threadId: args.threadId, runId: "run-1", status: args.mode === "steer" ? "running" : "queued" };
        return { content: [{ type: "text", text: JSON.stringify(value) }], structuredContent: value };
      },
    );
    register(
      "t3_thread_interrupt",
      { description: "Interrupt a run.", inputSchema: loose({ threadId: z.string(), runId: z.string().nullable().optional() }) },
      async (args) => {
        await record("t3_thread_interrupt", args);
        const value = { threadId: args.threadId, runId: args.runId ?? null, status: "interrupt_requested" };
        return { content: [{ type: "text", text: JSON.stringify(value) }], structuredContent: value };
      },
    );
    register(
      "t3_attachment_prepare_upload",
      { description: "Prepare an upload.", inputSchema: loose({ upload: z.looseObject({ type: z.string(), name: z.string(), mimeType: z.string(), sizeBytes: z.number() }) }) },
      async (args) => {
        await record("t3_attachment_prepare_upload", args);
        const value = { attachmentId: "attachment-1", relativeUrl: "/api/attachments/upload/signed-token", expiresAt: Date.now() + 60_000 };
        return { content: [{ type: "text", text: JSON.stringify(value) }], structuredContent: value };
      },
    );
    register(
      "t3_attachment_discard",
      { description: "Discard an upload.", inputSchema: loose({ attachmentId: z.string() }) },
      async (args) => {
        await record("t3_attachment_discard", args);
        return { content: [{ type: "text", text: "{}" }], structuredContent: {} };
      },
    );
    register(
      "delegate_task",
      { description: "Needs a calling thread.", inputSchema: loose({ task: z.string() }) },
      async (args) => {
        await record("delegate_task", args);
        return { content: [{ type: "text", text: "unexpected" }], isError: true };
      },
    );
    return server;
  }
}

function isRequest(message: unknown, method: string): boolean {
  return typeof message === "object" && message !== null && "method" in message && message.method === method;
}

async function readBody(request: IncomingMessage): Promise<Buffer> {
  const chunks: Buffer[] = [];
  for await (const chunk of request) chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
  return Buffer.concat(chunks);
}

function json(response: ServerResponse, status: number, value: unknown): void {
  response.writeHead(status, { "content-type": "application/json" });
  response.end(JSON.stringify(value));
}
