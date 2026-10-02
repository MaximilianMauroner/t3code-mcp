import { createHash, randomUUID } from "node:crypto";
import { appendFile, chmod, mkdir, open } from "node:fs/promises";
import { dirname } from "node:path";
import { createInterface } from "node:readline";
import { AsyncLocalStorage } from "node:async_hooks";

const auditContext = new AsyncLocalStorage<{ correlationId: string; operation: string }>();

/** Async-local attribution keeps concurrent MCP calls from sharing a parent. */
export function withAuditContext<T>(correlationId: string, operation: string, action: () => T): T {
  return auditContext.run({ correlationId, operation }, action);
}

const AUDIT_VERSION = 1;
const MAX_STRING_LENGTH = 512;
const MAX_ARRAY_ITEMS = 25;
const MAX_OBJECT_KEYS = 50;
const MAX_DEPTH = 6;

export type AuditSource = "transport" | "mcp" | "t3" | "git" | "journal" | "system";

export interface AuditEvent {
  readonly version: typeof AUDIT_VERSION;
  readonly eventId: string;
  readonly timestamp: string;
  readonly processId: number;
  readonly source: AuditSource;
  readonly event: string;
  readonly correlationId?: string;
  readonly parentCorrelationId?: string;
  readonly parentOperation?: string;
  readonly operation?: string;
  readonly outcome?: string;
  readonly durationMs?: number;
  readonly details?: Record<string, unknown>;
}

export interface AuditEventInput {
  readonly source: AuditSource;
  readonly event: string;
  readonly correlationId?: string;
  readonly operation?: string;
  readonly outcome?: string;
  readonly durationMs?: number;
  readonly details?: unknown;
}

export interface AuditQueryInput {
  readonly since?: string;
  readonly until?: string;
  readonly source?: AuditSource;
  readonly event?: string;
  readonly operation?: string;
  readonly outcome?: string;
  readonly cursor?: string;
  readonly limit: number;
}

export interface AuditLogPage {
  readonly items: ReadonlyArray<AuditEvent>;
  readonly nextCursor: string | null;
  readonly hasMore: boolean;
  readonly total: number;
  readonly invalidLines: number;
}

/**
 * A local, append-only usage trail. The logger is deliberately best-effort:
 * an audit filesystem problem must not turn a healthy T3 connection into a
 * failed MCP operation. The last failure is retained for diagnostics.
 */
export class AuditLog {
  private initialized = false;
  private writing: Promise<void> = Promise.resolve();
  private lastError: string | null = null;

  constructor(readonly filePath: string) {}

  async record(input: AuditEventInput): Promise<void> {
    try {
      const parent = auditContext.getStore();
      const event: AuditEvent = {
        version: AUDIT_VERSION,
        eventId: `evt_${randomUUID()}`,
        timestamp: new Date().toISOString(),
        processId: process.pid,
        source: input.source,
        event: input.event,
        ...(input.correlationId === undefined ? {} : { correlationId: input.correlationId }),
        ...(parent === undefined || parent.correlationId === input.correlationId ? {} : {
          parentCorrelationId: parent.correlationId,
          parentOperation: parent.operation,
        }),
        ...(input.operation === undefined ? {} : { operation: input.operation }),
        ...(input.outcome === undefined ? {} : { outcome: input.outcome }),
        ...(input.durationMs === undefined ? {} : { durationMs: Math.max(0, Math.round(input.durationMs)) }),
        ...(input.details === undefined ? {} : { details: asAuditDetails(input.details) }),
      };
      const line = `${JSON.stringify(event)}\n`;
      await this.init();
      const write = this.writing.then(() => appendFile(this.filePath, line, { encoding: "utf8", mode: 0o600 }));
      this.writing = write.catch(() => undefined);
      await write;
    } catch (error) {
      this.rememberError(error);
    }
  }

  async query(input: AuditQueryInput): Promise<AuditLogPage> {
    await this.init();
    await this.writing;
    const since = parseFilterTime(input.since, "since");
    const until = parseFilterTime(input.until, "until");
    const offset = parseCursor(input.cursor);
    const items: AuditEvent[] = [];
    let total = 0;
    let invalidLines = 0;
    try {
      const scan = await scanAuditFile(this.filePath, (event) => {
        const timestamp = Date.parse(event.timestamp);
        if (since !== null && (!Number.isFinite(timestamp) || timestamp < since)) return;
        if (until !== null && (!Number.isFinite(timestamp) || timestamp > until)) return;
        if (input.source !== undefined && event.source !== input.source) return;
        if (input.event !== undefined && event.event !== input.event) return;
        if (input.operation !== undefined && event.operation !== input.operation) return;
        if (input.outcome !== undefined && event.outcome !== input.outcome) return;
        if (total >= offset && items.length < input.limit) items.push(event);
        total += 1;
      });
      invalidLines = scan.invalidLines;
    } catch (error) {
      this.rememberError(error);
      return { items: [], nextCursor: null, hasMore: false, total: 0, invalidLines: 0 };
    }
    const nextOffset = offset + items.length;
    return {
      items,
      nextCursor: nextOffset < total ? String(nextOffset) : null,
      hasMore: nextOffset < total,
      total,
      invalidLines,
    };
  }

  get diagnosticError(): string | null {
    return this.lastError;
  }

  private async init(): Promise<void> {
    if (this.initialized) return;
    try {
      await mkdir(dirname(this.filePath), { recursive: true, mode: 0o700 });
      try {
        await chmod(this.filePath, 0o600);
      } catch (error) {
        if (!isFileNotFound(error)) throw error;
      }
      this.initialized = true;
    } catch (error) {
      this.rememberError(error);
      throw error;
    }
  }

  private rememberError(error: unknown): void {
    this.lastError = sanitizeError(error instanceof Error ? error.message : String(error));
  }
}

/**
 * Preserve the shape needed for usage analysis while removing high-risk
 * content. This function is shared by transport, upstream, Git, and MCP
 * instrumentation so new event producers inherit the same redaction rules.
 */
export function summarizeForAudit(value: unknown): unknown {
  return summarizeValue(value, undefined, 0);
}

export function hashAuditContent(value: string): string {
  return createHash("sha256").update(value).digest("hex");
}

function summarizeValue(value: unknown, key: string | undefined, depth: number): unknown {
  if (shouldRedactValue(key, value)) return redactedValue(value);
  if (value === null || typeof value === "number" || typeof value === "boolean") {
    return value;
  }
  if (typeof value === "bigint") {
    return { type: "bigint", value: String(value) };
  }
  if (typeof value === "string") {
    if (isSensitiveKey(key)) return redactedString(value);
    const sanitized = sanitizeError(value);
    if (sanitized.length > MAX_STRING_LENGTH) {
      return {
        type: "string",
        length: value.length,
        sha256: hashAuditContent(value),
        preview: sanitized.slice(0, MAX_STRING_LENGTH),
        truncated: true,
      };
    }
    return sanitized;
  }
  if (typeof value === "undefined") return null;
  if (depth >= MAX_DEPTH) {
    return { type: Array.isArray(value) ? "array" : "object", truncated: true };
  }
  if (Array.isArray(value)) {
    const items = value.slice(0, MAX_ARRAY_ITEMS).map((item) => summarizeValue(item, key, depth + 1));
    return value.length <= MAX_ARRAY_ITEMS
      ? items
      : { items, length: value.length, truncated: true };
  }
  if (typeof value === "object") {
    const record = value as Record<string, unknown>;
    const keys = Object.keys(record).sort();
    const output: Record<string, unknown> = {};
    for (const childKey of keys.slice(0, MAX_OBJECT_KEYS)) {
      output[childKey] = summarizeValue(record[childKey], childKey, depth + 1);
    }
    if (keys.length > MAX_OBJECT_KEYS) {
      output._truncatedKeys = keys.length - MAX_OBJECT_KEYS;
    }
    return output;
  }
  return { type: typeof value };
}

function asAuditDetails(value: unknown): Record<string, unknown> {
  const summarized = summarizeForAudit(value);
  if (typeof summarized === "object" && summarized !== null && !Array.isArray(summarized)) {
    return summarized as Record<string, unknown>;
  }
  return { value: summarized };
}

function isSensitiveKey(key: string | undefined): boolean {
  if (key === undefined) return false;
  const normalized = key.replace(/[^A-Za-z0-9]/g, "").toLowerCase();
  const secretTerms = [
    "authorization",
    "accesstoken",
    "refreshtoken",
    "idtoken",
    "token",
    "secret",
    "password",
    "credential",
    "cookie",
    "apikey",
    "privatekey",
    "idempotencykey",
  ];
  if (secretTerms.some((term) => normalized.includes(term))) return true;
  return ["message", "instruction", "prompt", "text", "patch", "content", "answers", "answer", "body", "payload", "options"]
    .includes(normalized);
}

function redactedString(value: string): Record<string, unknown> {
  return {
    redacted: true,
    type: "string",
    length: value.length,
    sha256: hashAuditContent(value),
  };
}

function redactedValue(value: unknown): Record<string, unknown> {
  if (typeof value === "string") return redactedString(value);
  if (Array.isArray(value)) {
    return { redacted: true, type: "array", length: value.length };
  }
  if (typeof value === "object" && value !== null) {
    return { redacted: true, type: "object", keyCount: Object.keys(value).length };
  }
  return { redacted: true, type: typeof value };
}

function shouldRedactValue(key: string | undefined, value: unknown): boolean {
  if (!isSensitiveKey(key)) return false;
  const normalized = key?.replace(/[^A-Za-z0-9]/g, "").toLowerCase();
  if (normalized === undefined) return false;
  const isObject = typeof value === "object" && value !== null;
  const contentContainers = ["answers", "answer", "options", "payload", "content"];
  if (contentContainers.includes(normalized)) return true;
  if (normalized === "body") return !isObject;
  return !isObject;
}

function sanitizeError(value: string): string {
  return value
    .replace(/Bearer\s+[^\s,;]+/gi, "Bearer [REDACTED]")
    .replace(/\bsk-[A-Za-z0-9_-]{12,}\b/g, "[REDACTED]")
    .replace(/\b(api[_-]?key|access[_-]?token|authorization|password|secret)\s*[:=]\s*[^\s,;]+/gi, "$1=[REDACTED]");
}

function parseCursor(cursor: string | undefined): number {
  if (cursor === undefined || cursor === "") return 0;
  const value = Number(cursor);
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new Error("cursor must be a non-negative integer string.");
  }
  return value;
}

export function parseFilterTime(value: string | undefined, name: string): number | null {
  if (value === undefined || value.trim() === "") return null;
  const parsed = Date.parse(value);
  if (!Number.isFinite(parsed)) throw new Error(`${name} must be a valid ISO date.`);
  return parsed;
}

function isAuditEvent(value: unknown): value is AuditEvent {
  if (typeof value !== "object" || value === null) return false;
  const event = value as Record<string, unknown>;
  return (
    event.version === AUDIT_VERSION &&
    typeof event.eventId === "string" &&
    typeof event.timestamp === "string" &&
    typeof event.processId === "number" &&
    isAuditSource(event.source) &&
    typeof event.event === "string" &&
    optionalString(event.correlationId) &&
    optionalString(event.parentCorrelationId) &&
    optionalString(event.parentOperation) &&
    optionalString(event.operation) &&
    optionalString(event.outcome) &&
    optionalNumber(event.durationMs) &&
    optionalDetails(event.details)
  );
}

function isAuditSource(value: unknown): value is AuditSource {
  return value === "transport" || value === "mcp" || value === "t3" || value === "git" || value === "journal" || value === "system";
}

function optionalString(value: unknown): boolean {
  return value === undefined || typeof value === "string";
}

function optionalNumber(value: unknown): boolean {
  return value === undefined || (typeof value === "number" && Number.isFinite(value));
}

function optionalDetails(value: unknown): boolean {
  return value === undefined || (typeof value === "object" && value !== null && !Array.isArray(value));
}

function isFileNotFound(error: unknown): boolean {
  return typeof error === "object" && error !== null && "code" in error && error.code === "ENOENT";
}

/** Scan a fixed byte snapshot, retaining at most a stream buffer and one line.
 * Appends during a query are deferred to the next query; offset order is unchanged.
 */
export async function scanAuditFile(
  filePath: string,
  visit: (event: AuditEvent) => void,
): Promise<{ invalidLines: number; bytes: number; validLines: number }> {
  const file = await open(filePath, "r").catch((error: unknown) => {
    if (isFileNotFound(error)) return null;
    throw error;
  });
  if (file === null) return { invalidLines: 0, bytes: 0, validLines: 0 };
  let lines: ReturnType<typeof createInterface> | undefined;
  let stream: ReturnType<typeof file.createReadStream> | undefined;
  try {
    const bytes = (await file.stat()).size;
    if (bytes === 0) return { invalidLines: 0, bytes, validLines: 0 };
    stream = file.createReadStream({ encoding: "utf8", start: 0, end: bytes - 1, autoClose: false });
    lines = createInterface({ input: stream, crlfDelay: Infinity });
    let invalidLines = 0;
    let validLines = 0;
    for await (const line of lines) {
      if (line.trim() === "") continue;
      let parsed: unknown;
      try { parsed = JSON.parse(line); } catch { invalidLines += 1; continue; }
      if (!isAuditEvent(parsed)) { invalidLines += 1; continue; }
      validLines += 1;
      visit(parsed);
    }
    return { invalidLines, bytes, validLines };
  } finally {
    lines?.close();
    stream?.destroy();
    await file.close();
  }
}
