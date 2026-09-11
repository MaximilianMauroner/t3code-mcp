import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { once } from "node:events";
import type { AddressInfo } from "node:net";
import type { T3Command } from "../../src/t3/commands.js";
import type { Message, Project, Thread } from "../../src/t3/types.js";

export interface FakeT3Options {
  readonly accessToken?: string;
  readonly environmentId?: string;
  readonly environmentLabel?: string;
  readonly serverVersion?: string;
  readonly scopes?: readonly string[];
  readonly descriptorCapabilities?: Record<string, unknown>;
  readonly dispatchStatus?: number;
  readonly dispatchErrorCode?: string;
  readonly dispatchErrorMessage?: string;
  readonly applyBeforeDispatchFailure?: boolean;
  readonly dispatchDelayMs?: number;
  readonly responseOverrides?: Record<string, unknown>;
  readonly invalidResponsePaths?: readonly string[];
}

export interface FakeRequest {
  readonly method: string;
  readonly path: string;
  readonly authorization: string | undefined;
  readonly body: unknown;
}

export interface FakeDispatch {
  readonly command: T3Command;
  readonly request: FakeRequest;
}

const DEFAULT_MODEL = { instanceId: "codex_openai", model: "gpt-5.3-codex-spark" };

export class FakeT3 {
  readonly accessToken: string;
  readonly environmentId: string;
  readonly environmentLabel: string;
  readonly serverVersion: string;
  readonly scopes: readonly string[];
  readonly descriptorCapabilities: Record<string, unknown>;
  readonly projects: Project[] = [];
  readonly threads: Thread[] = [];
  readonly requests: FakeRequest[] = [];
  readonly dispatches: FakeDispatch[] = [];

  private readonly server: Server;
  private readonly dispatchStatus: number;
  private readonly dispatchErrorCode: string;
  private readonly dispatchErrorMessage: string;
  private readonly applyBeforeDispatchFailure: boolean;
  private readonly dispatchDelayMs: number;
  private readonly responseOverrides: Record<string, unknown>;
  private readonly invalidResponsePaths: ReadonlySet<string>;
  private sequence = 1;

  constructor(options: FakeT3Options = {}) {
    this.accessToken = options.accessToken ?? "t3-test-token";
    this.environmentId = options.environmentId ?? "environment-test";
    this.environmentLabel = options.environmentLabel ?? "fake-coding";
    this.serverVersion = options.serverVersion ?? "fake-t3-1.0.0";
    this.scopes = options.scopes ?? ["orchestration:read", "orchestration:operate", "terminal:operate"];
    this.descriptorCapabilities = options.descriptorCapabilities ?? {
      orchestration: true,
      terminals: false,
    };
    this.dispatchStatus = options.dispatchStatus ?? 200;
    this.dispatchErrorCode = options.dispatchErrorCode ?? "dispatch_failed";
    this.dispatchErrorMessage = options.dispatchErrorMessage ?? "fake dispatch failed";
    this.applyBeforeDispatchFailure = options.applyBeforeDispatchFailure ?? false;
    this.dispatchDelayMs = options.dispatchDelayMs ?? 0;
    this.responseOverrides = options.responseOverrides ?? {};
    this.invalidResponsePaths = new Set(options.invalidResponsePaths ?? []);
    this.server = createServer((request, response) => {
      void this.handle(request, response).catch((error: unknown) => {
        if (!response.headersSent) {
          writeJson(response, 500, { code: "fake_failure", message: error instanceof Error ? error.message : String(error) });
        } else {
          response.destroy(error instanceof Error ? error : undefined);
        }
      });
    });
  }

  get baseUrl(): string {
    const address = this.server.address();
    if (!address || typeof address === "string") {
      throw new Error("Fake T3 server is not listening.");
    }
    return `http://127.0.0.1:${(address as AddressInfo).port}`;
  }

  async start(): Promise<void> {
    this.server.listen(0, "127.0.0.1");
    await once(this.server, "listening");
  }

  async close(): Promise<void> {
    if (!this.server.listening) {
      return;
    }
    this.server.close();
    await once(this.server, "close");
  }

  addProject(overrides: Partial<Project> = {}): Project {
    const project: Project = {
      id: overrides.id ?? `project-${this.projects.length + 1}`,
      title: overrides.title ?? `Project ${this.projects.length + 1}`,
      workspaceRoot: overrides.workspaceRoot ?? `/remote/project-${this.projects.length + 1}`,
      defaultModelSelection: overrides.defaultModelSelection === undefined ? DEFAULT_MODEL : overrides.defaultModelSelection,
      createdAt: overrides.createdAt ?? "2026-01-01T00:00:00.000Z",
      updatedAt: overrides.updatedAt ?? "2026-01-01T00:00:00.000Z",
      ...overrides,
    };
    this.projects.push(project);
    return project;
  }

  addThread(overrides: Partial<Thread> = {}): Thread {
    const projectId = overrides.projectId ?? this.projects[0]?.id ?? "project-1";
    const now = "2026-01-01T00:00:00.000Z";
    const thread: Thread = {
      id: overrides.id ?? `thread-${this.threads.length + 1}`,
      projectId,
      title: overrides.title ?? `Thread ${this.threads.length + 1}`,
      modelSelection: overrides.modelSelection ?? DEFAULT_MODEL,
      runtimeMode: overrides.runtimeMode ?? "full-access",
      interactionMode: overrides.interactionMode ?? "default",
      branch: overrides.branch ?? null,
      worktreePath: overrides.worktreePath ?? null,
      latestTurn: overrides.latestTurn ?? null,
      archivedAt: overrides.archivedAt ?? null,
      createdAt: overrides.createdAt ?? now,
      updatedAt: overrides.updatedAt ?? now,
      session: overrides.session ?? { status: "stopped" },
      hasPendingApprovals: overrides.hasPendingApprovals ?? false,
      hasPendingUserInput: overrides.hasPendingUserInput ?? false,
      messages: overrides.messages ?? [],
      activities: overrides.activities ?? [],
      checkpoints: overrides.checkpoints ?? [],
      proposedPlans: overrides.proposedPlans ?? [],
      ...overrides,
    };
    this.threads.push(thread);
    return thread;
  }

  thread(id: string): Thread {
    const thread = this.threads.find((candidate) => candidate.id === id);
    if (!thread) {
      throw new Error(`Fake thread ${id} was not found.`);
    }
    return thread;
  }

  private async handle(request: IncomingMessage, response: ServerResponse): Promise<void> {
    const url = new URL(request.url ?? "/", `http://${request.headers.host ?? "localhost"}`);
    const path = url.pathname;
    const body = request.method === "POST" ? await readBody(request) : undefined;
    const fakeRequest: FakeRequest = {
      method: request.method ?? "GET",
      path,
      authorization: request.headers.authorization,
      body,
    };
    this.requests.push(fakeRequest);

    if (path !== "/.well-known/t3/environment" && request.headers.authorization !== `Bearer ${this.accessToken}`) {
      writeJson(response, 401, { code: "unauthorized", message: "fake auth required" });
      return;
    }

    if (this.invalidResponsePaths.has(path)) {
      writeJson(response, 200, { invalid: true });
      return;
    }
    if (path in this.responseOverrides) {
      writeJson(response, 200, this.responseOverrides[path]);
      return;
    }

    switch (path) {
      case "/.well-known/t3/environment":
        writeJson(response, 200, {
          environmentId: this.environmentId,
          label: this.environmentLabel,
          platform: { os: "fake", arch: "fake", machine: "fake" },
          serverVersion: this.serverVersion,
          capabilities: this.descriptorCapabilities,
        });
        return;
      case "/api/auth/session":
        writeJson(response, 200, {
          authenticated: true,
          scopes: this.scopes,
          sessionMethod: "test-bearer",
          expiresAt: "2026-12-31T00:00:00.000Z",
        });
        return;
      case "/api/orchestration/shell":
        writeJson(response, 200, {
          snapshotSequence: this.sequence,
          projects: this.projects,
          threads: this.threads.map(toShellThread),
          updatedAt: new Date().toISOString(),
        });
        return;
      case "/api/orchestration/dispatch":
        await this.handleDispatch(response, fakeRequest, body);
        return;
      default:
        if (path.startsWith("/api/orchestration/threads/")) {
          const threadId = decodeURIComponent(path.slice("/api/orchestration/threads/".length));
          const thread = this.threads.find((candidate) => candidate.id === threadId);
          if (!thread) {
            writeJson(response, 404, { code: "not_found", message: "fake thread not found" });
            return;
          }
          writeJson(response, 200, { snapshotSequence: this.sequence, thread });
          return;
        }
        writeJson(response, 404, { code: "not_found", message: "fake route not found" });
    }
  }

  private async handleDispatch(response: ServerResponse, request: FakeRequest, body: unknown): Promise<void> {
    if (this.dispatchDelayMs > 0) {
      await new Promise<void>((resolve) => setTimeout(resolve, this.dispatchDelayMs));
    }
    if (!isCommand(body)) {
      writeJson(response, 400, { code: "invalid_command", message: "fake command must be an object" });
      return;
    }
    if (this.dispatchStatus !== 200) {
      if (this.applyBeforeDispatchFailure) {
        this.dispatches.push({ command: body, request });
        this.applyCommand(body);
        this.sequence += 1;
      }
      writeJson(response, this.dispatchStatus, { code: this.dispatchErrorCode, message: this.dispatchErrorMessage });
      return;
    }
    const dispatch = { command: body, request };
    this.dispatches.push(dispatch);
    this.applyCommand(body);
    this.sequence += 1;
    writeJson(response, 200, { sequence: this.sequence });
  }

  private applyCommand(command: T3Command): void {
    const now = new Date().toISOString();
    switch (command.type) {
      case "project.create":
        this.projects.push({
          id: command.projectId,
          title: command.title,
          workspaceRoot: command.workspaceRoot,
          defaultModelSelection: command.defaultModelSelection ?? null,
          createdAt: now,
          updatedAt: now,
        });
        return;
      case "thread.create":
        this.addThread({
          id: command.threadId,
          projectId: command.projectId,
          title: command.title,
          modelSelection: command.modelSelection,
          runtimeMode: command.runtimeMode,
          interactionMode: command.interactionMode,
          branch: command.branch,
          worktreePath: command.worktreePath,
        });
        return;
      case "thread.turn.start": {
        const thread = this.thread(command.threadId);
        const turnId = `turn-${command.message.messageId}`;
        thread.messages.push({
          id: command.message.messageId,
          role: "user",
          text: command.message.text,
          turnId,
          streaming: false,
          createdAt: now,
          updatedAt: now,
        });
        thread.latestTurn = {
          turnId,
          state: "running",
          requestedAt: now,
          startedAt: now,
          completedAt: null,
          assistantMessageId: null,
        };
        thread.session = { status: "running" };
        thread.updatedAt = now;
        return;
      }
      case "thread.turn.interrupt": {
        const thread = this.thread(command.threadId);
        if (thread.latestTurn) {
          thread.latestTurn = { ...thread.latestTurn, state: "interrupted", completedAt: now };
        }
        thread.session = { status: "stopped" };
        thread.updatedAt = now;
        return;
      }
      case "thread.approval.respond": {
        const thread = this.thread(command.threadId);
        thread.hasPendingApprovals = false;
        thread.updatedAt = now;
        return;
      }
      case "thread.user-input.respond": {
        const thread = this.thread(command.threadId);
        thread.hasPendingUserInput = false;
        thread.updatedAt = now;
        return;
      }
      case "thread.archive": {
        const thread = this.thread(command.threadId);
        thread.archivedAt = now;
        thread.updatedAt = now;
        return;
      }
      case "thread.snooze": {
        const thread = this.thread(command.threadId);
        thread.snoozedUntil = command.snoozedUntil;
        thread.snoozedAt = now;
        thread.updatedAt = now;
        return;
      }
      case "thread.unsnooze": {
        const thread = this.thread(command.threadId);
        thread.snoozedUntil = null;
        thread.snoozedAt = null;
        thread.updatedAt = now;
        return;
      }
      case "thread.settle": {
        const thread = this.thread(command.threadId);
        thread.settledOverride = "settled";
        thread.settledAt = now;
        thread.snoozedUntil = null;
        thread.snoozedAt = null;
        thread.pinnedAt = null;
        thread.updatedAt = now;
        return;
      }
      case "thread.unsettle": {
        const thread = this.thread(command.threadId);
        thread.settledOverride = "active";
        thread.updatedAt = now;
        return;
      }
    }
  }
}

export function assistantMessage(text: string, turnId: string, id = `assistant-${turnId}`): Message {
  return {
    id,
    role: "assistant",
    text,
    turnId,
    streaming: false,
    createdAt: "2026-01-01T00:00:00.000Z",
    updatedAt: "2026-01-01T00:00:00.000Z",
  };
}

function toShellThread(thread: Thread): Thread {
  return {
    ...thread,
    messages: [],
    activities: [],
    checkpoints: [],
    proposedPlans: [],
  };
}

function isCommand(value: unknown): value is T3Command {
  return typeof value === "object" && value !== null && "type" in value && typeof value.type === "string";
}

async function readBody(request: IncomingMessage): Promise<unknown> {
  const chunks: Buffer[] = [];
  for await (const chunk of request) {
    chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
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
