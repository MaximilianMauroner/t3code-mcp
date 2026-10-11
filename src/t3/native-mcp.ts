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
const WAIT_MARGIN_MS = 15_000;
// T3 waits up to 10 minutes when `timeoutMs` is omitted and up to 60 minutes
// when it is "Infinity". A shorter request timeout would end the wait early and
// look like a lost result.
const UNBOUNDED_WAIT_TIMEOUT_MS = 60 * 60_000 + WAIT_MARGIN_MS;
// Node fires a timer that is longer than 2^31-1 ms after 1 ms.
const MAX_TIMER_MS = 2_147_483_647;
// The first try, one retry, and one more try on a fresh connection when T3 forgot the session.
const MAX_ATTEMPTS = 3;

/**
 * An MCP client for T3's own `/mcp` endpoint. One connection is shared by every
 * gateway request. It is rebuilt when the stored credential changes, or when T3
 * forgets its session or refuses its token. A failed call does not close it, so
 * other calls in progress are not affected. A mutation is never sent twice
 * unless the upstream provably did not process it.
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

  /**
   * The native tools to advertise. Only a cold start waits for T3. Otherwise
   * the last known catalog is returned at once, even when it is stale or was
   * read with an earlier token, and a refresh runs in the background, so a slow
   * T3 does not delay the gateway's own tools. When the catalog was read with
   * an earlier token, the first call after that read ends starts a read with
   * the current token. Without a valid credential the last known tools stay
   * listed, so a call can explain how to renew it. A tool call still checks
   * the current credential.
   */
  async snapshot(): Promise<NativeSnapshot> {
    const state = await this.credentials.read();
    if (state.state !== "valid") return failedSnapshot(this.catalog?.snapshot, credentialError(state));
    const tokenId = tokenIdentity(state.credential.accessToken);
    const known = this.catalog;
    if (known?.tokenId === tokenId && known.expiresAt > Date.now()) return known.snapshot;
    const refreshed = this.refresh(state.credential, tokenId);
    return known?.snapshot ?? refreshed;
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
      let connection: Connection | null = null;
      try {
        connection = await this.connect(credential);
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
        const failure = classifyFailure(error, connection !== null);
        await this.auditLog.record({
          source: "t3",
          event: "upstream.response",
          correlationId,
          operation: `mcp:${name}`,
          outcome: "error",
          durationMs: Date.now() - startedAt,
          details: { attempt, failure, error: errorMessage(error) },
        });
        const dropped = await this.dropIfInvalid(connection, error);
        if (failure === "rejected") {
          throw new GatewayError("native_rejected", `Native T3 rejected ${name}: ${errorMessage(error)}`);
        }
        if (failure === "unauthorized") throw credentialRejectedError();
        const processed = failure === "unknown";
        // Here `dropped` means T3 forgot the session: nothing ran, and the next
        // attempt opens a fresh connection, so it may follow even the retry.
        const retry = attempt === 1 ? !processed || !mutating : dropped && attempt < MAX_ATTEMPTS;
        if (retry) continue;
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
        // A redirect could send the bytes past the origin check above.
        redirect: "error",
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
    if (this.connection !== null) await this.discard(this.connection);
  }

  private async requireCredential(): Promise<NativeCredential> {
    const state = await this.credentials.read();
    if (state.state !== "valid") throw credentialError(state);
    return state.credential;
  }

  /**
   * Starts a catalog read, or joins the one in progress, whatever its token.
   * A second read would discard the first read's connection and fail it. The
   * result is saved under the token it was read with.
   */
  private refresh(credential: NativeCredential, tokenId: string): Promise<NativeSnapshot> {
    // readCatalog never rejects, so an unawaited refresh cannot leave an unhandled rejection.
    this.refreshing ??= this.readCatalog(credential).then((snapshot) => {
      const ttl = snapshot.error === null ? CATALOG_TTL_MS : CATALOG_FAILURE_TTL_MS;
      this.catalog = { snapshot, tokenId, expiresAt: Date.now() + ttl };
      this.refreshing = null;
      return snapshot;
    });
    return this.refreshing;
  }

  /** Lists the native tools. Never rejects: a failure keeps the last known tools and sets `error`. */
  private async readCatalog(credential: NativeCredential): Promise<NativeSnapshot> {
    let connection: Connection | null = null;
    try {
      connection = await this.connect(credential);
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
      return {
        tools,
        missing: NATIVE_TOOL_NAMES.filter((name) => !offered.has(name)),
        fetchedAt: new Date().toISOString(),
        error: null,
      };
    } catch (error) {
      await this.dropIfInvalid(connection, error);
      const failure = isCredentialRejection(error)
        ? credentialRejectedError()
        : new GatewayError("native_unavailable", `Native T3 tool catalog is unavailable: ${errorMessage(error)}`);
      return failedSnapshot(this.catalog?.snapshot, failure);
    }
  }

  private async connect(credential: NativeCredential): Promise<Connection> {
    const tokenId = tokenIdentity(credential.accessToken);
    if (this.connection?.tokenId === tokenId) return this.connection;
    if (this.connection !== null) await this.discard(this.connection);
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

  /**
   * Closes the connection when T3 forgot its session (HTTP 404) or refuses its
   * token (401 or 403), and reports whether it did. Other failures leave it
   * open, because other calls can still be in progress on it.
   */
  private async dropIfInvalid(connection: Connection | null, error: unknown): Promise<boolean> {
    if (connection === null || !invalidatesConnection(error)) return false;
    await this.discard(connection);
    return true;
  }

  /**
   * Closes one connection, which fails every call still in progress on it. A
   * concurrent failure can name a connection that was already replaced; the
   * replacement stays open.
   */
  private async discard(connection: Connection): Promise<void> {
    if (this.connection === connection) this.connection = null;
    await connection.client.close().catch(() => undefined);
  }

  private timeoutFor(name: string, args: Record<string, unknown>): number {
    const budget = args.timeoutMs;
    if (typeof budget === "number" && Number.isFinite(budget) && budget > 0) {
      return Math.min(MAX_TIMER_MS, Math.max(this.requestTimeoutMs, budget + WAIT_MARGIN_MS));
    }
    return name === "t3_thread_wait" ? UNBOUNDED_WAIT_TIMEOUT_MS : this.requestTimeoutMs;
  }
}

/**
 * `rejected`: upstream answered with a protocol error, so nothing ran.
 * `unauthorized`: T3 refused the token (HTTP 401 or 403), so nothing ran.
 * `unprocessed`: the request provably never reached a tool (no connection, or
 * an unknown MCP session after a T3 restart), so it is safe to send again.
 * `unknown`: the request may have run.
 */
type Failure = "rejected" | "unauthorized" | "unprocessed" | "unknown";

function classifyFailure(error: unknown, sent: boolean): Failure {
  // T3 refuses a revoked token already at `initialize`, before the call is sent.
  if (isCredentialRejection(error)) return "unauthorized";
  if (!sent) return "unprocessed";
  if (error instanceof StreamableHTTPError) return error.code === 404 ? "unprocessed" : "unknown";
  if (error instanceof McpError) {
    return error.code === ErrorCode.RequestTimeout || error.code === ErrorCode.ConnectionClosed ? "unknown" : "rejected";
  }
  return isConnectionRefused(error) ? "unprocessed" : "unknown";
}

function isCredentialRejection(error: unknown): boolean {
  return error instanceof StreamableHTTPError && (error.code === 401 || error.code === 403);
}

/** True when T3 forgot the MCP session (404) or refuses its token, so the connection cannot be used again. */
function invalidatesConnection(error: unknown): boolean {
  return isCredentialRejection(error) || (error instanceof StreamableHTTPError && error.code === 404);
}

function isConnectionRefused(error: unknown): boolean {
  const cause = error instanceof Error ? error.cause : undefined;
  return typeof cause === "object" && cause !== null && "code" in cause && cause.code === "ECONNREFUSED";
}

function failedSnapshot(previous: NativeSnapshot | undefined, error: GatewayError): NativeSnapshot {
  return {
    tools: previous?.tools ?? [],
    missing: previous?.missing ?? [...NATIVE_TOOL_NAMES],
    fetchedAt: previous?.fetchedAt ?? null,
    error: { code: error.code, message: error.message },
  };
}

function credentialError(state: Exclude<NativeCredentialState, { readonly state: "valid" }>): GatewayError {
  return state.state === "missing"
    ? new GatewayError("native_credential_missing", state.reason)
    : new GatewayError(
        "native_credential_expired",
        `The native MCP credential expired at ${state.credential.expiresAt}. Run \`t3-code-mcp native-login\` with a new pairing code.`,
      );
}

function credentialRejectedError(): GatewayError {
  return new GatewayError(
    "native_credential_rejected",
    "Native T3 rejected the stored MCP credential. Run `t3-code-mcp native-login` with a new pairing code.",
  );
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
