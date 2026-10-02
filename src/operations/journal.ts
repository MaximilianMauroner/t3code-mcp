import { createHash, randomUUID } from "node:crypto";
import { mkdir, readFile, rename, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { sanitizeFailureText } from "../t3/failure.js";
import { ProviderRetrySchema } from "../t3/types.js";
import { summarizeForAudit, type AuditLog } from "./audit-log.js";
import { settingsReceiptSchema, type SettingsReceipt } from "./settings.js";
import type { FailureInfo } from "../gateway.js";

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
  readonly settings?: SettingsReceipt;
  readonly t3Sequence?: number;
  readonly lastError?: string;
  readonly terminalRunStatus?: "failed";
  readonly terminalFailure?: FailureInfo;
}

interface ThreadFailureRecord {
  readonly threadId: string;
  readonly turnId: string;
  readonly failure: FailureInfo;
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
  readonly settings?: SettingsReceipt;
}

export class IdempotencyConflictError extends Error {
  override readonly name = "IdempotencyConflictError";
}

export class OperationJournal {
  private readonly entries = new Map<string, OperationRecord>();
  private readonly tasks = new Map<string, TaskRecord>();
  private readonly threadFailures = new Map<string, ThreadFailureRecord>();
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
      const threadFailures = (parsed as { threadFailures?: unknown }).threadFailures;
      if (threadFailures !== undefined && !Array.isArray(threadFailures)) {
        throw new Error("Operation journal thread failures must be an array.");
      }
      for (const item of threadFailures ?? []) {
        if (!isThreadFailureRecord(item)) {
          throw new Error("Operation journal contains an invalid thread failure.");
        }
        this.threadFailures.set(`${item.threadId}\u0000${item.turnId}`, item);
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
      ...(input.settings === undefined ? {} : { settings: settingsReceiptSchema.parse(input.settings) }),
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
        ...(record.settings === undefined ? {} : { settings: record.settings }),
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
    patch: Partial<Pick<OperationRecord, "status" | "t3Sequence" | "turnId" | "lastError" | "terminalRunStatus" | "terminalFailure">>,
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

  async getFailureByTurnId(threadId: string, turnId: string): Promise<FailureInfo | null> {
    await this.init();
    const retained = this.threadFailures.get(`${threadId}\u0000${turnId}`);
    if (retained) return retained.failure;
    return [...this.entries.values()].find((entry) =>
      entry.kind === "thread.turn.start" && entry.threadId === threadId &&
      entry.turnId === turnId && entry.terminalRunStatus === "failed" &&
      entry.terminalFailure?.turnId === turnId
    )?.terminalFailure ?? null;
  }

  async retainTerminalFailure(
    threadId: string,
    turnId: string,
    candidate: FailureInfo,
    operationId?: string,
  ): Promise<FailureInfo> {
    await this.init();
    if (candidate.turnId !== turnId) {
      throw new Error("A terminal failure must match its turn.");
    }
    const key = `${threadId}\u0000${turnId}`;
    const operation = operationId === undefined
      ? null
      : [...this.entries.values()].find((entry) => entry.operationId === operationId) ?? null;
    if (operation !== null && (operation.threadId !== threadId ||
      (operation.turnId !== undefined && operation.turnId !== turnId))) {
      throw new Error("A terminal failure does not match the run operation.");
    }
    const legacyFailure = [...this.entries.values()].find((entry) =>
      entry.kind === "thread.turn.start" && entry.threadId === threadId &&
      entry.turnId === turnId && entry.terminalFailure?.turnId === turnId
    )?.terminalFailure ?? null;
    const existing = this.threadFailures.get(key)?.failure ?? operation?.terminalFailure ?? legacyFailure;
    const merged = mergeTerminalFailure(existing, candidate);
    const admittedModel = operation?.settings?.resolved.modelSelection;
    // A V1 snapshot carries mutable thread settings. The run receipt records
    // the settings admitted for this operation; V2 root items use run metadata.
    const failure = admittedModel && merged.source !== "t3_v2_turn_item" ? {
      ...merged,
      model: sanitizeFailureText(admittedModel.model, 200),
      provider: sanitizeFailureText(admittedModel.provider ?? admittedModel.instanceId ?? merged.provider ?? "", 200) || null,
    } : merged;
    const failureChanged = JSON.stringify(existing) !== JSON.stringify(failure);
    const operationChanged = operation !== null && (operation.terminalRunStatus !== "failed" ||
      operation.turnId !== turnId || JSON.stringify(operation.terminalFailure) !== JSON.stringify(failure));
    if (!failureChanged && !operationChanged) return failure;

    // Update both maps before awaiting persistence so concurrent reads cannot
    // replace a precise session failure with a later generic turn fallback.
    this.threadFailures.set(key, { threadId, turnId, failure });
    if (operation !== null) {
      this.entries.set(operation.idempotencyKey, {
        ...operation,
        turnId,
        terminalRunStatus: "failed",
        terminalFailure: failure,
        updatedAt: new Date().toISOString(),
      });
    }
    await this.persist();
    return failure;
  }

  private async persist(): Promise<void> {
    const run = this.writing.then(async () => {
      const tempPath = join(dirname(this.filePath), `.${this.filePath.split("/").at(-1) ?? "journal"}.${randomUUID()}.tmp`);
      const payload = JSON.stringify(
        {
          version: 2,
          operations: [...this.entries.values()],
          tasks: [...this.tasks.values()],
          threadFailures: [...this.threadFailures.values()],
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
    typeof entry.updatedAt === "string" &&
    (entry.settings === undefined || settingsReceiptSchema.safeParse(entry.settings).success) &&
    (entry.terminalRunStatus === undefined
      ? entry.terminalFailure === undefined
      : entry.terminalRunStatus === "failed" && typeof entry.turnId === "string" &&
        isFailureInfo(entry.terminalFailure) && entry.terminalFailure.turnId === entry.turnId)
  );
}

function isThreadFailureRecord(value: unknown): value is ThreadFailureRecord {
  if (typeof value !== "object" || value === null) return false;
  const item = value as Record<string, unknown>;
  return typeof item.threadId === "string" && typeof item.turnId === "string" &&
    isFailureInfo(item.failure) && item.failure.turnId === item.turnId;
}

function isFailureInfo(value: unknown): value is FailureInfo {
  if (typeof value !== "object" || value === null) return false;
  const failure = value as Record<string, unknown>;
  const nullableString = (field: unknown) => field === null || typeof field === "string";
  return ["quota", "rate_limit", "auth_billing", "provider_internal", "provider_error", "unknown"].includes(String(failure.category)) &&
    (failure.class === undefined || nullableString(failure.class)) &&
    (failure.retryable === undefined || failure.retryable === null || typeof failure.retryable === "boolean") &&
    (failure.retry === undefined || failure.retry === null || ProviderRetrySchema.safeParse(failure.retry).success) &&
    nullableString(failure.code) && typeof failure.message === "string" &&
    nullableString(failure.provider) && typeof failure.model === "string" &&
    nullableString(failure.turnId) && nullableString(failure.resetAt) &&
    nullableString(failure.retryAfter) &&
    ["t3_session", "t3_turn", "t3_activity", "t3_message", "t3_v2_turn_item"].includes(String(failure.source));
}

const failureSourcePriority = {
  t3_turn: 0, t3_session: 1, t3_activity: 2, t3_message: 3, t3_v2_turn_item: 4,
};

function mergeTerminalFailure(existing: FailureInfo | null | undefined, candidate: FailureInfo): FailureInfo {
  if (!existing) return candidate;
  const preferred = failureSourcePriority[candidate.source] > failureSourcePriority[existing.source] ? candidate : existing;
  const other = preferred === candidate ? existing : candidate;
  return {
    ...preferred,
    category: preferred.category === "unknown" && preferred.source === other.source ? other.category : preferred.category,
    class: preferred.class ?? other.class ?? null,
    retryable: preferred.retryable ?? other.retryable ?? null,
    retry: preferred.retry ?? other.retry ?? null,
    code: preferred.code ?? other.code,
    provider: preferred.provider ?? other.provider,
    resetAt: preferred.resetAt ?? other.resetAt,
    retryAfter: preferred.retryAfter ?? other.retryAfter,
  };
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
