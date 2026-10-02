import { randomUUID } from "node:crypto";
import {
  AuthSessionSchema,
  DescriptorSchema,
  DispatchResultSchema,
  ShellSnapshotSchema,
  ThreadSnapshotSchema,
  type AuthSession,
  type Descriptor,
  type DispatchResult,
  type ShellSnapshot,
  type ThreadSnapshot,
} from "./types.js";
import type { T3Command } from "./commands.js";
import { summarizeForAudit, type AuditLog } from "../operations/audit-log.js";
import { z } from "zod";
import { requestT3Rpc } from "./rpc-client.js";
import { V2ArchivedShellSchema, V2ShellSchema, V2ThreadSchema, normalizeV2ShellThread, normalizeV2Thread } from "./v2.js";

export class T3HttpError extends Error {
  override readonly name = "T3HttpError";

  constructor(
    readonly status: number,
    readonly method: string,
    readonly path: string,
    readonly code: string | null,
    message: string,
    readonly reason: string | null = null,
    readonly requiredScope: string | null = null,
    readonly traceId: string | null = null,
  ) {
    super(message);
  }
}

function errorField(body: unknown, field: string): string | null {
  if (typeof body !== "object" || body === null) return null;
  const value = (body as Record<string, unknown>)[field];
  return typeof value === "string" && value.length > 0 ? value : null;
}

export interface T3ConnectionTelemetry {
  readonly lastSuccessfulAt: string | null;
  readonly lastSnapshotAt: string | null;
  readonly lastError: string | null;
}

function errorCode(body: unknown): string | null {
  if (typeof body !== "object" || body === null || !("code" in body)) {
    return null;
  }
  const code = (body as { code?: unknown }).code;
  return typeof code === "string" ? code : null;
}

function errorMessage(body: unknown, status: number, method: string, path: string): string {
  if (typeof body === "object" && body !== null && "message" in body) {
    const message = (body as { message?: unknown }).message;
    if (typeof message === "string" && message.length > 0) {
      return message;
    }
  }
  // Effect tagged errors serialize their fields, but not their message getter.
  const reason = errorField(body, "reason");
  const requiredScope = errorField(body, "requiredScope");
  const detail = reason ?? (requiredScope ? `requires ${requiredScope}` : errorCode(body));
  return `T3 ${method} ${path} failed with HTTP ${status}${detail ? ` (${detail})` : ""}.`;
}

function joinUrl(baseUrl: string, path: string): string {
  return `${baseUrl.replace(/\/$/, "")}/${path.replace(/^\//, "")}`;
}

export class T3HttpClient {
  private lastSuccessfulAt: number | null = null;
  private lastSnapshotAt: number | null = null;
  private lastError: string | null = null;
  private cachedDescriptor: Descriptor | null = null;

  constructor(
    private readonly baseUrl: string,
    private readonly accessToken: string,
    private readonly requestTimeoutMs = 15_000,
    private readonly auditLog?: AuditLog,
  ) {}

  telemetry(): T3ConnectionTelemetry {
    return {
      lastSuccessfulAt: this.lastSuccessfulAt === null ? null : new Date(this.lastSuccessfulAt).toISOString(),
      lastSnapshotAt: this.lastSnapshotAt === null ? null : new Date(this.lastSnapshotAt).toISOString(),
      lastError: this.lastError,
    };
  }

  getCachedDescriptor(): Descriptor | null {
    return this.cachedDescriptor;
  }

  async getDescriptor(signal?: AbortSignal): Promise<Descriptor> {
    const descriptor = await this.request(
      "GET",
      "/.well-known/t3/environment",
      undefined,
      DescriptorSchema,
      signal,
      false,
    );
    this.cachedDescriptor = descriptor;
    return descriptor;
  }

  async getSession(signal?: AbortSignal): Promise<AuthSession> {
    return this.request("GET", "/api/auth/session", undefined, AuthSessionSchema, signal, true);
  }

  async getShell(signal?: AbortSignal): Promise<ShellSnapshot> {
    if (await this.protocolVersion(signal) === 2) {
      const snapshot = await this.request("GET", "/api/orchestration/shell", undefined, V2ShellSchema, signal, true);
      const archived = await this.rpc("orchestration.getArchivedShellSnapshot", {}, V2ArchivedShellSchema, signal);
      const threads = [...snapshot.threads, ...snapshot.archivedThreads, ...archived.threads];
      const unique = new Map(threads.map((thread) => [thread.id, thread]));
      this.lastSnapshotAt = Date.now();
      return {
        snapshotSequence: Math.max(snapshot.snapshotSequence, archived.snapshotSequence),
        projects: snapshot.projects,
        threads: [...unique.values()].map(normalizeV2ShellThread),
        updatedAt: new Date(this.lastSnapshotAt).toISOString(),
      };
    }
    const snapshot = await this.request(
      "GET",
      "/api/orchestration/shell",
      undefined,
      ShellSnapshotSchema,
      signal,
      true,
    );
    this.lastSnapshotAt = Date.now();
    return snapshot;
  }

  async getThread(threadId: string, signal?: AbortSignal): Promise<ThreadSnapshot> {
    if (await this.protocolVersion(signal) === 2) {
      const snapshot = await this.request("GET", `/api/orchestration/threads/${encodeURIComponent(threadId)}`, undefined, V2ThreadSchema, signal, true);
      return normalizeV2Thread(snapshot);
    }
    return this.request(
      "GET",
      `/api/orchestration/threads/${encodeURIComponent(threadId)}`,
      undefined,
      ThreadSnapshotSchema,
      signal,
      true,
    );
  }

  async dispatch(command: T3Command, signal?: AbortSignal): Promise<DispatchResult> {
    if (await this.protocolVersion(signal) === 2) return this.dispatchV2(command, signal);
    return this.request(
      "POST",
      "/api/orchestration/dispatch",
      command,
      DispatchResultSchema,
      signal,
      true,
    );
  }

  private async protocolVersion(signal?: AbortSignal): Promise<1 | 2> {
    const descriptor = this.cachedDescriptor ?? await this.getDescriptor(signal);
    const version = descriptor.orchestrationProtocolVersion ?? 1;
    if (version !== 1 && version !== 2) throw new Error(`Unsupported T3 orchestration protocol ${String(version)}.`);
    return version;
  }

  private async rpc<T>(method: string, payload: unknown, schema: z.ZodType<T>, signal?: AbortSignal): Promise<T> {
    const correlationId = `t3_${randomUUID()}`;
    const startedAt = Date.now();
    await this.auditLog?.record({ source: "t3", event: "upstream.request", correlationId, operation: method, outcome: "started", details: { body: summarizeForAudit(payload), transport: "websocket" } });
    try {
      const result = await requestT3Rpc(this.baseUrl, this.accessToken, this.requestTimeoutMs, method, payload, schema, signal);
      this.lastSuccessfulAt = Date.now();
      this.lastError = null;
      await this.auditLog?.record({ source: "t3", event: "upstream.response", correlationId, operation: method, outcome: "completed", durationMs: Date.now() - startedAt });
      return result;
    } catch (error) {
      this.lastError = error instanceof Error ? error.message : String(error);
      await this.auditLog?.record({ source: "t3", event: "upstream.response", correlationId, operation: method, outcome: "error", durationMs: Date.now() - startedAt, details: { error: this.lastError } });
      throw error;
    }
  }

  private async dispatchV2(command: T3Command, signal?: AbortSignal): Promise<DispatchResult> {
    const dispatch = (payload: unknown) => this.rpc("orchestration.dispatchCommand", payload, DispatchResultSchema, signal);
    switch (command.type) {
      case "project.create": {
        await this.request("POST", "/api/projects/mutate", command, z.object({ id: z.string() }).passthrough(), signal, true);
        // Project mutation is a separate store in V2 and has no event sequence.
        return { sequence: (await this.getShell(signal)).snapshotSequence };
      }
      case "thread.create":
        return dispatch({ ...command, createdBy: "user", creationSource: "mcp" });
      case "thread.turn.start": {
        if (command.bootstrap) throw new T3HttpError(400, "POST", "/ws", "unsupported_bootstrap", "V2 turn dispatch requires an existing thread and workspace.");
        // V2 persists these settings on the thread before admitting a message.
        await dispatch({ type: "thread.runtime-mode.set", commandId: `${command.commandId}:runtime`, threadId: command.threadId, runtimeMode: command.runtimeMode });
        await dispatch({ type: "thread.interaction-mode.set", commandId: `${command.commandId}:interaction`, threadId: command.threadId, interactionMode: command.interactionMode });
        return dispatch({
          type: "message.dispatch", commandId: command.commandId, threadId: command.threadId,
          messageId: command.message.messageId, text: command.message.text, attachments: command.message.attachments,
          dispatchMode: { type: "start_immediately" }, createdBy: "user", creationSource: "mcp",
          ...(command.modelSelection ? { modelSelection: command.modelSelection } : {}),
          ...(command.titleSeed ? { titleSeed: command.titleSeed } : {}),
        });
      }
      case "thread.turn.interrupt": {
        const runId = command.turnId ?? (await this.getThread(command.threadId, signal)).thread.latestTurn?.turnId;
        if (!runId) throw new T3HttpError(400, "POST", "/ws", "run_not_found", "T3 has no run to interrupt.");
        return dispatch({ type: "run.interrupt", commandId: command.commandId, threadId: command.threadId, runId });
      }
      case "thread.approval.respond":
        return dispatch({ type: "runtime-request.respond", commandId: command.commandId, threadId: command.threadId, requestId: command.requestId, decision: command.decision });
      case "thread.user-input.respond":
        return dispatch({ type: "runtime-request.respond", commandId: command.commandId, threadId: command.threadId, requestId: command.requestId, answers: command.answers });
      default:
        return dispatch(command);
    }
  }

  private async request<T>(
    method: "GET" | "POST",
    path: string,
    body: unknown,
    schema: z.ZodType<T>,
    signal: AbortSignal | undefined,
    authenticated: boolean,
  ): Promise<T> {
    const requestUrl = joinUrl(this.baseUrl, path);
    const timeoutSignal = AbortSignal.timeout(this.requestTimeoutMs);
    const requestSignal = signal ? AbortSignal.any([signal, timeoutSignal]) : timeoutSignal;
    const correlationId = `t3_${randomUUID()}`;
    const startedAt = Date.now();
    await this.auditLog?.record({
      source: "t3",
      event: "upstream.request",
      correlationId,
      operation: `${method} ${path}`,
      outcome: "started",
      details: {
        method,
        path,
        authenticated,
        body: body === undefined ? null : summarizeForAudit(body),
      },
    });
    try {
      const response = await fetch(requestUrl, {
        method,
        signal: requestSignal,
        headers: {
          accept: "application/json",
          ...(authenticated && path.startsWith("/api/orchestration/") && this.cachedDescriptor?.orchestrationProtocolVersion === 2 ? { "x-t3-orchestration-protocol": "2" } : {}),
          ...(authenticated ? { authorization: `Bearer ${this.accessToken}` } : {}),
          ...(body === undefined ? {} : { "content-type": "application/json" }),
        },
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      });

      const responseBody: unknown = await response.json().catch(() => null);
      if (!response.ok) {
        throw new T3HttpError(
          response.status,
          method,
          path,
          errorCode(responseBody),
          errorMessage(responseBody, response.status, method, path),
          errorField(responseBody, "reason"),
          errorField(responseBody, "requiredScope"),
          errorField(responseBody, "traceId"),
        );
      }

      const parsed = schema.safeParse(responseBody);
      if (!parsed.success) {
        throw new Error(`T3 ${method} ${path} returned an invalid response.`);
      }
      this.lastSuccessfulAt = Date.now();
      this.lastError = null;
      await this.auditLog?.record({
        source: "t3",
        event: "upstream.response",
        correlationId,
        operation: `${method} ${path}`,
        outcome: "completed",
        durationMs: Date.now() - startedAt,
        details: { status: response.status },
      });
      return parsed.data;
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      this.lastError = message;
      await this.auditLog?.record({
        source: "t3",
        event: "upstream.response",
        correlationId,
        operation: `${method} ${path}`,
        outcome: "error",
        durationMs: Date.now() - startedAt,
        details: {
          status: error instanceof T3HttpError ? error.status : null,
          code: error instanceof T3HttpError ? error.code : null,
          error: message,
        },
      });
      throw error;
    }
  }
}
