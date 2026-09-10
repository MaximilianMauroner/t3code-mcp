import { createHash, randomUUID } from "node:crypto";
import { mkdir, readFile, rename, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";

export type OperationStatus = "prepared" | "accepted" | "uncertain" | "rejected";

export type OperationKind =
  | "project.create"
  | "thread.create"
  | "thread.turn.start"
  | "thread.turn.interrupt"
  | "thread.approval.respond"
  | "thread.user-input.respond"
  | "thread.archive";

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
  readonly t3Sequence?: number;
  readonly lastError?: string;
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
}

export class IdempotencyConflictError extends Error {
  override readonly name = "IdempotencyConflictError";
}

export class OperationJournal {
  private readonly entries = new Map<string, OperationRecord>();
  private initialized = false;
  private writing: Promise<void> = Promise.resolve();

  constructor(private readonly filePath: string) {}

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
    } catch (error) {
      if (!isFileNotFound(error)) {
        throw error;
      }
    }

    const now = new Date().toISOString();
    let recovered = false;
    for (const [key, entry] of this.entries) {
      if (entry.status === "prepared") {
        this.entries.set(key, {
          ...entry,
          status: "uncertain",
          updatedAt: now,
          lastError: "The gateway stopped before the T3 command outcome was recorded.",
        });
        recovered = true;
      }
    }
    this.initialized = true;
    if (recovered) {
      await this.persist();
    }
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
    };
    this.entries.set(input.idempotencyKey, record);
    await this.persist();
    return { record, reused: false };
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
          version: 1,
          operations: [...this.entries.values()],
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
