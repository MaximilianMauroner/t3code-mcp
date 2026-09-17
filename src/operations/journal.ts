import { createHash, randomUUID } from "node:crypto";
import { mkdir, readFile, rename, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { summarizeForAudit, type AuditLog } from "./audit-log.js";

export type OperationStatus = "prepared" | "accepted" | "uncertain" | "rejected";

export type TaskStage =
  | "prepared"
  | "thread_create_uncertain"
  | "thread_created"
  | "dispatch_rejected"
  | "dispatch_uncertain"
  | "run_accepted"
  | "rejected";

export type OperationKind =
  | "project.create"
  | "thread.create"
  | "thread.turn.start"
  | "thread.turn.interrupt"
  | "thread.approval.respond"
  | "thread.user-input.respond"
  | "thread.archive"
  | "thread.snooze"
  | "thread.unsnooze"
  | "thread.settle"
  | "thread.unsettle";

export interface OperationRecord {
  readonly operationId: string;
  readonly kind: OperationKind;
  readonly idempotencyKey: string;
  readonly payloadHash: string;
  readonly commandId: string;
  readonly status: OperationStatus;
  readonly createdAt: string;
  readonly updatedAt: string;
  readonly projectId?: string;
  readonly threadId?: string;
  readonly runId?: string;
  readonly messageId?: string;
  readonly turnId?: string;
  readonly snoozedUntil?: string;
  readonly t3Sequence?: number;
  readonly lastError?: string;
}

export interface TaskRecord {
  readonly taskRef: string;
  readonly idempotencyKey: string;
  readonly payloadHash: string;
  readonly projectId: string;
  readonly title: string;
  readonly runtimeMode: "approval-required" | "auto-accept-edits" | "auto" | "full-access";
  readonly threadId?: string;
  readonly runId?: string;
  readonly messageId?: string;
  readonly threadOperationId?: string;
  readonly runOperationId?: string;
  readonly threadIdempotencyKey: string;
  readonly runIdempotencyKey: string;
  readonly stage: TaskStage;
  readonly baselineRevision?: string;
  readonly baselineAttribution?: "clean" | "dirty" | "unavailable";
  readonly createdAt: string;
  readonly updatedAt: string;
  readonly lastError?: string | null;
}

export interface BeginTaskInput {
  readonly idempotencyKey: string;
  readonly payloadHash: string;
  readonly projectId: string;
  readonly title: string;
  readonly runtimeMode: TaskRecord["runtimeMode"];
}

export interface BeginOperationInput {
  readonly kind: OperationKind;
  readonly idempotencyKey: string;
  readonly payloadHash: string;
  readonly commandId?: string;
  readonly projectId?: string;
  readonly threadId?: string;
  readonly runId?: string;
  readonly messageId?: string;
  readonly turnId?: string;
  readonly snoozedUntil?: string;
}

export class IdempotencyConflictError extends Error {
  override readonly name = "IdempotencyConflictError";
}

export class OperationJournal {
  private readonly entries = new Map<string, OperationRecord>();
  private readonly tasks = new Map<string, TaskRecord>();
  private initialized = false;
  private writing: Promise<void> = Promise.resolve();

  constructor(
    private readonly filePath: string,
    private readonly auditLog?: AuditLog,
  ) {}

  async init(): Promise<void> {
    if (this.initialized) {
      return;
    }
    await mkdir(dirname(this.filePath), { recursive: true });
    try {
      const raw = await readFile(this.filePath, "utf8");
      const parsed: unknown = JSON.parse(raw);
      if (typeof parsed !== "object" || parsed === null || !("operations" in parsed)) {
        throw new Error("Operation journal has an invalid shape.");
      }
      const operations = (parsed as { operations?: unknown }).operations;
      if (!Array.isArray(operations)) {
        throw new Error("Operation journal operations must be an array.");
      }
      for (const entry of operations) {
        if (!isOperationRecord(entry)) {
          throw new Error("Operation journal contains an invalid operation.");
        }
        this.entries.set(entry.idempotencyKey, entry);
      }
      const tasks = (parsed as { tasks?: unknown }).tasks;
      if (tasks !== undefined && !Array.isArray(tasks)) {
        throw new Error("Operation journal tasks must be an array.");
      }
      for (const task of tasks ?? []) {
        if (!isTaskRecord(task)) {
          throw new Error("Operation journal contains an invalid task.");
        }
        this.tasks.set(task.idempotencyKey, task);
      }
    } catch (error) {
      if (!isFileNotFound(error)) {
        throw error;
      }
    }

    const now = new Date().toISOString();
    let recoveredCount = 0;
    for (const [key, entry] of this.entries) {
      if (entry.status === "prepared") {
        this.entries.set(key, {
          ...entry,
          status: "uncertain",
          updatedAt: now,
          lastError: "The gateway stopped before the T3 command outcome was recorded.",
        });
        recoveredCount += 1;
      }
    }
    this.initialized = true;
    if (recoveredCount > 0) {
      await this.persist();
    }
    await this.auditLog?.record({
      source: "journal",
      event: "journal.initialized",
      outcome: "completed",
      details: {
        filePath: this.filePath,
        operationCount: this.entries.size,
        taskCount: this.tasks.size,
        recoveredPreparedOperations: recoveredCount,
      },
    });
  }

  async begin(input: BeginOperationInput): Promise<{ readonly record: OperationRecord; readonly reused: boolean }> {
    await this.init();
    const existing = this.entries.get(input.idempotencyKey);
    if (existing) {
      if (existing.payloadHash !== input.payloadHash || existing.kind !== input.kind) {
        throw new IdempotencyConflictError(
          `Idempotency key ${input.idempotencyKey} was already used for a different operation.`,
        );
      }
      await this.auditLog?.record({
        source: "journal",
        event: "operation.begin",
        correlationId: existing.operationId,
        operation: input.kind,
        outcome: "reused",
        details: {
          operationId: existing.operationId,
          commandId: existing.commandId,
          idempotencyKey: input.idempotencyKey,
          payloadHash: input.payloadHash,
          status: existing.status,
        },
      });
      return { record: existing, reused: true };
    }

    const now = new Date().toISOString();
    const record: OperationRecord = {
      operationId: `op_${randomUUID()}`,
      kind: input.kind,
      idempotencyKey: input.idempotencyKey,
      payloadHash: input.payloadHash,
      commandId: input.commandId ?? randomUUID(),
      status: "prepared",
      createdAt: now,
      updatedAt: now,
      ...(input.projectId === undefined ? {} : { projectId: input.projectId }),
      ...(input.threadId === undefined ? {} : { threadId: input.threadId }),
      ...(input.runId === undefined ? {} : { runId: input.runId }),
      ...(input.messageId === undefined ? {} : { messageId: input.messageId }),
      ...(input.turnId === undefined ? {} : { turnId: input.turnId }),
      ...(input.snoozedUntil === undefined ? {} : { snoozedUntil: input.snoozedUntil }),
    };
    this.entries.set(input.idempotencyKey, record);
    await this.persist();
    await this.auditLog?.record({
      source: "journal",
      event: "operation.begin",
      correlationId: record.operationId,
      operation: input.kind,
      outcome: "prepared",
      details: {
        operationId: record.operationId,
        commandId: record.commandId,
        idempotencyKey: input.idempotencyKey,
        payloadHash: input.payloadHash,
        projectId: input.projectId,
        threadId: input.threadId,
      },
    });
    return { record, reused: false };
  }

  async beginTask(input: BeginTaskInput): Promise<{ readonly record: TaskRecord; readonly reused: boolean }> {
    await this.init();
    const existing = this.tasks.get(input.idempotencyKey);
    if (existing) {
      if (existing.payloadHash !== input.payloadHash) {
        throw new IdempotencyConflictError(
          `Idempotency key ${input.idempotencyKey} was already used for a different task start.`,
        );
      }
      await this.auditLog?.record({
        source: "journal",
        event: "task.begin",
        correlationId: existing.taskRef,
        operation: "task.start",
        outcome: "reused",
        details: {
          taskRef: existing.taskRef,
          idempotencyKey: input.idempotencyKey,
          payloadHash: input.payloadHash,
          stage: existing.stage,
        },
      });
      return { record: existing, reused: true };
    }
    const now = new Date().toISOString();
    const record: TaskRecord = {
      taskRef: `task_${randomUUID()}`,
      idempotencyKey: input.idempotencyKey,
      payloadHash: input.payloadHash,
      projectId: input.projectId,
      title: input.title,
      runtimeMode: input.runtimeMode,
      threadIdempotencyKey: `${input.idempotencyKey}:thread`,
      runIdempotencyKey: `${input.idempotencyKey}:turn`,
      stage: "prepared",
      createdAt: now,
      updatedAt: now,
    };
    this.tasks.set(record.idempotencyKey, record);
    await this.persist();
    await this.auditLog?.record({
      source: "journal",
      event: "task.begin",
      correlationId: record.taskRef,
      operation: "task.start",
      outcome: "prepared",
      details: {
        taskRef: record.taskRef,
        idempotencyKey: input.idempotencyKey,
        payloadHash: input.payloadHash,
        projectId: input.projectId,
        runtimeMode: input.runtimeMode,
        stage: record.stage,
      },
    });
    return { record, reused: false };
  }

  async updateTask(
    taskRef: string,
    patch: Partial<Pick<TaskRecord,
      "stage" | "threadId" | "runId" | "messageId" | "threadOperationId" | "runOperationId" |
      "baselineRevision" | "baselineAttribution" | "lastError"
    >>,
  ): Promise<TaskRecord> {
    await this.init();
    const entry = [...this.tasks.values()].find((candidate) => candidate.taskRef === taskRef);
    if (!entry) throw new Error(`Task ${taskRef} was not found.`);
    const updated: TaskRecord = { ...entry, ...patch, updatedAt: new Date().toISOString() };
    this.tasks.set(updated.idempotencyKey, updated);
    await this.persist();
    await this.auditLog?.record({
      source: "journal",
      event: "task.update",
      correlationId: updated.taskRef,
      operation: "task.start",
      outcome: "completed",
      details: {
        taskRef: updated.taskRef,
        stage: updated.stage,
        patch: summarizeForAudit(patch),
      },
    });
    return updated;
  }

  async getTaskByRef(taskRef: string): Promise<TaskRecord | null> {
    await this.init();
    return [...this.tasks.values()].find((task) => task.taskRef === taskRef) ?? null;
  }

  async listTasks(): Promise<ReadonlyArray<TaskRecord>> {
    await this.init();
    return [...this.tasks.values()];
  }

  async update(
    operationId: string,
    patch: Partial<Pick<OperationRecord, "status" | "t3Sequence" | "turnId" | "lastError">>,
  ): Promise<OperationRecord> {
    await this.init();
    const entry = [...this.entries.values()].find((candidate) => candidate.operationId === operationId);
    if (!entry) {
      throw new Error(`Operation ${operationId} was not found.`);
    }
    const updated: OperationRecord = {
      ...entry,
      ...patch,
      updatedAt: new Date().toISOString(),
    };
    this.entries.set(updated.idempotencyKey, updated);
    await this.persist();
    await this.auditLog?.record({
      source: "journal",
      event: "operation.update",
      correlationId: updated.operationId,
      operation: updated.kind,
      outcome: updated.status,
      details: {
        operationId: updated.operationId,
        status: updated.status,
        patch: summarizeForAudit(patch),
      },
    });
    return updated;
  }

  async getByOperationId(operationId: string): Promise<OperationRecord | null> {
    await this.init();
    return [...this.entries.values()].find((entry) => entry.operationId === operationId) ?? null;
  }

  async getByIdempotencyKey(idempotencyKey: string): Promise<OperationRecord | null> {
    await this.init();
    return this.entries.get(idempotencyKey) ?? null;
  }

  async getByRunId(runId: string): Promise<OperationRecord | null> {
    await this.init();
    return [...this.entries.values()].find((entry) => entry.runId === runId) ?? null;
  }

  private async persist(): Promise<void> {
    const run = this.writing.then(async () => {
      const tempPath = join(dirname(this.filePath), `.${this.filePath.split("/").at(-1) ?? "journal"}.${randomUUID()}.tmp`);
      const payload = JSON.stringify(
        {
          version: 2,
          operations: [...this.entries.values()],
          tasks: [...this.tasks.values()],
        },
        null,
        2,
      );
      await writeFile(tempPath, `${payload}\n`, { encoding: "utf8", mode: 0o600 });
      await rename(tempPath, this.filePath);
    });
    this.writing = run.catch(() => undefined);
    await run;
  }
}

export function hashPayload(value: unknown): string {
  return createHash("sha256").update(stableStringify(value)).digest("hex");
}

function stableStringify(value: unknown): string {
  if (value === null || typeof value !== "object") {
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) {
    return `[${value.map((item) => stableStringify(item)).join(",")}]`;
  }
  const record = value as Record<string, unknown>;
  return `{${Object.keys(record)
    .sort()
    .map((key) => `${JSON.stringify(key)}:${stableStringify(record[key])}`)
    .join(",")}}`;
}

function isFileNotFound(error: unknown): boolean {
  return typeof error === "object" && error !== null && "code" in error && error.code === "ENOENT";
}

function isOperationRecord(value: unknown): value is OperationRecord {
  if (typeof value !== "object" || value === null) {
    return false;
  }
  const entry = value as Record<string, unknown>;
  return (
    typeof entry.operationId === "string" &&
    typeof entry.kind === "string" &&
    typeof entry.idempotencyKey === "string" &&
    typeof entry.payloadHash === "string" &&
    typeof entry.commandId === "string" &&
    ["prepared", "accepted", "uncertain", "rejected"].includes(String(entry.status)) &&
    typeof entry.createdAt === "string" &&
    typeof entry.updatedAt === "string"
  );
}

function isTaskRecord(value: unknown): value is TaskRecord {
  if (typeof value !== "object" || value === null) return false;
  const task = value as Record<string, unknown>;
  return (
    typeof task.taskRef === "string" &&
    typeof task.idempotencyKey === "string" &&
    typeof task.payloadHash === "string" &&
    typeof task.projectId === "string" &&
    typeof task.title === "string" &&
    ["approval-required", "auto-accept-edits", "auto", "full-access"].includes(String(task.runtimeMode)) &&
    typeof task.threadIdempotencyKey === "string" &&
    typeof task.runIdempotencyKey === "string" &&
    optionalString(task.threadId) &&
    optionalString(task.runId) &&
    optionalString(task.messageId) &&
    optionalString(task.threadOperationId) &&
    optionalString(task.runOperationId) &&
    optionalString(task.baselineRevision) &&
    (task.baselineAttribution === undefined || ["clean", "dirty", "unavailable"].includes(String(task.baselineAttribution))) &&
    (task.lastError === undefined || task.lastError === null || typeof task.lastError === "string") &&
    ["prepared", "thread_create_uncertain", "thread_created", "dispatch_rejected", "dispatch_uncertain", "run_accepted", "rejected"].includes(String(task.stage)) &&
    typeof task.createdAt === "string" &&
    typeof task.updatedAt === "string"
  );
}

function optionalString(value: unknown): boolean {
  return value === undefined || typeof value === "string";
}
