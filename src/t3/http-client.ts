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
import { z } from "zod";

export class T3HttpError extends Error {
  override readonly name = "T3HttpError";

  constructor(
    readonly status: number,
    readonly method: string,
    readonly path: string,
    readonly code: string | null,
    message: string,
  ) {
    super(message);
  }
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
  return `T3 ${method} ${path} failed with HTTP ${status}.`;
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
    return this.request(
      "POST",
      "/api/orchestration/dispatch",
      command,
      DispatchResultSchema,
      signal,
      true,
    );
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
    try {
      const response = await fetch(requestUrl, {
        method,
        signal: requestSignal,
        headers: {
          accept: "application/json",
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
        );
      }

      const parsed = schema.safeParse(responseBody);
      if (!parsed.success) {
        throw new Error(`T3 ${method} ${path} returned an invalid response.`);
      }
      this.lastSuccessfulAt = Date.now();
      this.lastError = null;
      return parsed.data;
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      this.lastError = message;
      throw error;
    }
  }
}
