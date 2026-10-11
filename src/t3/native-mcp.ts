import { randomUUID } from "node:crypto";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport, StreamableHTTPError } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { CallToolResultSchema, ErrorCode, McpError, type CallToolResult, type Tool } from "@modelcontextprotocol/sdk/types.js";
import { GATEWAY_VERSION, NATIVE_TOOL_NAMES, isNativeToolName, type NativeToolName } from "../contract.js";
import { GatewayError } from "../errors.js";
import { summarizeForAudit, type AuditLog } from "../operations/audit-log.js";
import { tokenIdentity, type NativeCredential, type NativeCredentialState, type NativeCredentialStore } from "./native-credential.js";

export interface NativeTool {
  readonly name: NativeToolName;
  readonly title?: string;
  readonly description?: string;
  readonly inputSchema: Tool["inputSchema"];
  readonly annotations?: Tool["annotations"];
}

export interface NativeSnapshot {
  /** Allowlisted tools the upstream currently offers. */
  readonly tools: ReadonlyArray<NativeTool>;
  /** Allowlisted tools the upstream did not offer at the last successful read. */
  readonly missing: ReadonlyArray<NativeToolName>;
  readonly fetchedAt: string | null;
  readonly error: { readonly code: string; readonly message: string } | null;
}

type CredentialSource = Pick<NativeCredentialStore, "read">;

interface Connection {
  readonly client: Client;
  readonly tokenId: string;
}

const CATALOG_TTL_MS = 60_000;
const CATALOG_FAILURE_TTL_MS = 10_000;
const CATALOG_TIMEOUT_MS = 10_000;
const REQUEST_TIMEOUT_MS = 120_000;
// A wait without a finite budget would otherwise hit the request timeout and
// look like a lost result.
const UNBOUNDED_WAIT_TIMEOUT_MS = 15 * 60_000;
const WAIT_MARGIN_MS = 15_000;

/**
 * An MCP client for T3's own `/mcp` endpoint. One connection is shared by every
 * gateway request and is rebuilt when the stored credential changes. A
 * mutation is never sent twice unless the upstream provably did not process it.
 */
export class NativeMcpClient {
  private connection: Connection | null = null;
  private connecting: Promise<Connection> | null = null;
  private catalog: { readonly snapshot: NativeSnapshot; readonly tokenId: string; readonly expiresAt: number } | null = null;
  private refreshing: Promise<NativeSnapshot> | null = null;

  constructor(
    readonly url: string,
    private readonly credentials: CredentialSource,
    private readonly auditLog: AuditLog,
    private readonly requestTimeoutMs = REQUEST_TIMEOUT_MS,
  ) {}

  credentialState(): Promise<NativeCredentialState> {
    return this.credentials.read();
  }

  async snapshot(): Promise<NativeSnapshot> {
    const state = await this.credentials.read();
    if (state.state !== "valid") {
      const error = credentialError(state);
      return { tools: [], missing: [...NATIVE_TOOL_NAMES], fetchedAt: null, error: { code: error.code, message: error.message } };
    }
    const tokenId = tokenIdentity(state.credential.accessToken);
    if (this.catalog !== null && this.catalog.tokenId === tokenId && this.catalog.expiresAt > Date.now()) {
      return this.catalog.snapshot;
    }
    this.refreshing ??= this.refreshCatalog(state.credential).finally(() => {
      this.refreshing = null;
    });
    return this.refreshing;
  }

  async callTool(name: string, args: Record<string, unknown>, mutating: boolean): Promise<CallToolResult> {
    const credential = await this.requireCredential();
    for (let attempt = 1; ; attempt += 1) {
      const correlationId = `native_${randomUUID()}`;
      const startedAt = Date.now();
      await this.auditLog.record({
        source: "t3",
        event: "upstream.request",
        correlationId,
        operation: `mcp:${name}`,
        outcome: "started",
        details: { attempt, arguments: summarizeForAudit(args) },
      });
      let sent = false;
      try {
        const connection = await this.connect(credential);
        sent = true;
        const result = await connection.client.request(
          { method: "tools/call", params: { name, arguments: args } },
          CallToolResultSchema,
          { timeout: this.timeoutFor(name, args) },
        );
        await this.auditLog.record({
          source: "t3",
          event: "upstream.response",
          correlationId,
          operation: `mcp:${name}`,
          outcome: result.isError ? "error" : "completed",
          durationMs: Date.now() - startedAt,
          details: { attempt, isError: result.isError === true },
        });
        return result;
      } catch (error) {
        const failure = classifyFailure(error, sent);
        await this.auditLog.record({
          source: "t3",
          event: "upstream.response",
          correlationId,
          operation: `mcp:${name}`,
          outcome: "error",
          durationMs: Date.now() - startedAt,
          details: { attempt, failure, error: errorMessage(error) },
        });
        if (failure === "rejected") {
          throw new GatewayError("native_rejected", `Native T3 rejected ${name}: ${errorMessage(error)}`);
        }
        await this.reset();
        if (failure === "unauthorized") {
          throw new GatewayError(
            "native_credential_rejected",
            "Native T3 rejected the stored MCP credential. Run `t3-code-mcp native-login` with a new pairing code.",
          );
        }
        const processed = failure === "unknown";
        if (attempt === 1 && (!processed || !mutating)) continue;
        if (processed && mutating) {
          throw new GatewayError(
            "native_outcome_unknown",
            `${name} was sent to native T3, but its result was lost (${errorMessage(error)}). ` +
              "Check the target with t3_thread_read before retrying. Tools with clientRequestId are safe to retry with the same value.",
          );
        }
        throw new GatewayError("native_unavailable", `Native T3 MCP is unavailable: ${errorMessage(error)}`);
      }
    }
  }

  /** POST attachment bytes to a signed upload URL that native T3 issued. */
  async uploadAttachment(relativeUrl: string, bytes: Uint8Array<ArrayBuffer>, mimeType: string): Promise<void> {
    const origin = new URL(this.url).origin;
    const target = new URL(relativeUrl, origin);
    if (target.origin !== origin) {
      throw new GatewayError("native_upload_rejected", "Native T3 returned an upload URL outside its own origin.");
    }
    const correlationId = `native_${randomUUID()}`;
    const startedAt = Date.now();
    await this.auditLog.record({
      source: "t3",
      event: "upstream.request",
      correlationId,
      operation: "attachment.upload",
      outcome: "started",
      details: { sizeBytes: bytes.byteLength, mimeType },
    });
    let status: number | null = null;
    try {
      const response = await fetch(target, {
        method: "POST",
        headers: { "content-type": mimeType, "content-length": String(bytes.byteLength) },
        body: bytes,
        signal: AbortSignal.timeout(this.requestTimeoutMs),
      });
      status = response.status;
      if (!response.ok) {
        throw new GatewayError("native_upload_failed", `Native T3 refused the upload (HTTP ${response.status}): ${(await response.text()).slice(0, 200)}`);
      }
    } catch (error) {
      throw error instanceof GatewayError ? error : new GatewayError("native_upload_failed", `Attachment upload failed: ${errorMessage(error)}`);
    } finally {
      await this.auditLog.record({
        source: "t3",
        event: "upstream.response",
        correlationId,
        operation: "attachment.upload",
        outcome: status !== null && status < 300 ? "completed" : "error",
        durationMs: Date.now() - startedAt,
        details: { status },
      });
    }
  }

  async close(): Promise<void> {
    await this.reset();
  }

  private async requireCredential(): Promise<NativeCredential> {
    const state = await this.credentials.read();
    if (state.state !== "valid") throw credentialError(state);
    return state.credential;
  }

  private async refreshCatalog(credential: NativeCredential): Promise<NativeSnapshot> {
    const tokenId = tokenIdentity(credential.accessToken);
    try {
      const connection = await this.connect(credential);
      const listed: Tool[] = [];
      let cursor: string | undefined;
      do {
        const page = await connection.client.listTools(cursor === undefined ? {} : { cursor }, { timeout: CATALOG_TIMEOUT_MS });
        listed.push(...page.tools);
        cursor = page.nextCursor;
      } while (cursor !== undefined);
      const tools = listed.flatMap((tool): NativeTool[] => isNativeToolName(tool.name)
        ? [{
            name: tool.name,
            ...(tool.title === undefined ? {} : { title: tool.title }),
            ...(tool.description === undefined ? {} : { description: tool.description }),
            inputSchema: tool.inputSchema,
            ...(tool.annotations === undefined ? {} : { annotations: tool.annotations }),
          }]
        : []);
      const offered = new Set<string>(tools.map((tool) => tool.name));
      const snapshot: NativeSnapshot = {
        tools,
        missing: NATIVE_TOOL_NAMES.filter((name) => !offered.has(name)),
        fetchedAt: new Date().toISOString(),
        error: null,
      };
      this.catalog = { snapshot, tokenId, expiresAt: Date.now() + CATALOG_TTL_MS };
      return snapshot;
    } catch (error) {
      await this.reset();
      const previous = this.catalog?.tokenId === tokenId ? this.catalog.snapshot : null;
      const snapshot: NativeSnapshot = {
        tools: previous?.tools ?? [],
        missing: previous?.missing ?? [...NATIVE_TOOL_NAMES],
        fetchedAt: previous?.fetchedAt ?? null,
        error: { code: "native_unavailable", message: `Native T3 tool catalog is unavailable: ${errorMessage(error)}` },
      };
      this.catalog = { snapshot, tokenId, expiresAt: Date.now() + CATALOG_FAILURE_TTL_MS };
      return snapshot;
    }
  }

  private async connect(credential: NativeCredential): Promise<Connection> {
    const tokenId = tokenIdentity(credential.accessToken);
    if (this.connection?.tokenId === tokenId) return this.connection;
    if (this.connection !== null) await this.reset();
    this.connecting ??= this.open(credential.accessToken, tokenId).finally(() => {
      this.connecting = null;
    });
    const connection = await this.connecting;
    if (connection.tokenId !== tokenId) return this.connect(credential);
    return connection;
  }

  private async open(token: string, tokenId: string): Promise<Connection> {
    const transport = new StreamableHTTPClientTransport(new URL(this.url), {
      requestInit: { headers: { authorization: `Bearer ${token}` } },
    });
    const client = new Client({ name: "t3-code-mcp", version: GATEWAY_VERSION });
    await client.connect(transport, { timeout: CATALOG_TIMEOUT_MS });
    this.connection = { client, tokenId };
    return this.connection;
  }

  private async reset(): Promise<void> {
    const connection = this.connection;
    this.connection = null;
    await connection?.client.close().catch(() => undefined);
  }

  private timeoutFor(name: string, args: Record<string, unknown>): number {
    const budget = args.timeoutMs;
    if (typeof budget === "number" && Number.isFinite(budget) && budget > 0) {
      return Math.max(this.requestTimeoutMs, budget + WAIT_MARGIN_MS);
    }
    return name === "t3_thread_wait" ? UNBOUNDED_WAIT_TIMEOUT_MS : this.requestTimeoutMs;
  }
}

/**
 * `rejected`: upstream answered with a protocol error, so nothing ran.
 * `unprocessed`: the request provably never reached a tool (no connection, or
 * an unknown MCP session after a T3 restart), so it is safe to send again.
 * `unknown`: the request may have run.
 */
type Failure = "rejected" | "unauthorized" | "unprocessed" | "unknown";

function classifyFailure(error: unknown, sent: boolean): Failure {
  if (!sent) return "unprocessed";
  if (error instanceof StreamableHTTPError) {
    if (error.code === 401 || error.code === 403) return "unauthorized";
    if (error.code === 404) return "unprocessed";
    return "unknown";
  }
  if (error instanceof McpError) {
    return error.code === ErrorCode.RequestTimeout || error.code === ErrorCode.ConnectionClosed ? "unknown" : "rejected";
  }
  return isConnectionRefused(error) ? "unprocessed" : "unknown";
}

function isConnectionRefused(error: unknown): boolean {
  const cause = error instanceof Error ? error.cause : undefined;
  return typeof cause === "object" && cause !== null && "code" in cause && cause.code === "ECONNREFUSED";
}

function credentialError(state: Exclude<NativeCredentialState, { readonly state: "valid" }>): GatewayError {
  return state.state === "missing"
    ? new GatewayError("native_credential_missing", state.reason)
    : new GatewayError(
        "native_credential_expired",
        `The native MCP credential expired at ${state.credential.expiresAt}. Run \`t3-code-mcp native-login\` with a new pairing code.`,
      );
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
