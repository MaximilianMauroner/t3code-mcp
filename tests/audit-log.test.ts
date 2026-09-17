import { mkdtemp, readFile, stat, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { OperationJournal, hashPayload } from "../src/operations/journal.js";
import { AuditLog, summarizeForAudit } from "../src/operations/audit-log.js";

const directories: string[] = [];

afterEach(async () => {
  await Promise.all(directories.splice(0).map((directory) => rm(directory, { recursive: true, force: true })));
});

describe("AuditLog", () => {
  it("persists structured events with restrictive permissions and redacts content", async () => {
    const directory = await makeDirectory();
    const path = join(directory, "audit.jsonl");
    const audit = new AuditLog(path);

    await audit.record({
      source: "mcp",
      event: "tool.call",
      operation: "t3_thread_send",
      details: {
        arguments: {
          threadId: "thread-1",
          message: "private prompt that must not be persisted",
          authorization: "Bearer gateway-secret",
        },
      },
    });
    await audit.record({
      source: "mcp",
      event: "tool.result",
      operation: "t3_thread_send",
      outcome: "completed",
      durationMs: 12,
      details: { result: { status: "accepted" } },
    });

    const raw = await readFile(path, "utf8");
    expect(raw).not.toContain("private prompt that must not be persisted");
    expect(raw).not.toContain("gateway-secret");
    expect(raw).toContain("thread-1");
    expect((await stat(path)).mode & 0o777).toBe(0o600);

    const page = await audit.query({ source: "mcp", operation: "t3_thread_send", limit: 1 });
    expect(page).toMatchObject({ total: 2, hasMore: true, invalidLines: 0 });
    expect(page.items[0]).toMatchObject({ event: "tool.call", operation: "t3_thread_send" });
    expect(page.nextCursor).toBe("1");
  });

  it("keeps valid events queryable when a log has a malformed line", async () => {
    const directory = await makeDirectory();
    const path = join(directory, "audit.jsonl");
    await writeFile(path, "not-json\n", { mode: 0o600 });
    const audit = new AuditLog(path);
    await audit.record({ source: "system", event: "startup", outcome: "completed" });

    const page = await audit.query({ limit: 50 });
    expect(page.invalidLines).toBe(1);
    expect(page.items).toHaveLength(1);
    expect(page.items[0]?.event).toBe("startup");
  });

  it("captures durable journal transitions without persisting the key", async () => {
    const directory = await makeDirectory();
    const audit = new AuditLog(join(directory, "audit.jsonl"));
    const journal = new OperationJournal(join(directory, "operations.json"), audit);
    const begun = await journal.begin({
      kind: "thread.archive",
      idempotencyKey: "journal-key",
      payloadHash: hashPayload({ threadId: "thread-1" }),
      threadId: "thread-1",
    });
    await journal.update(begun.record.operationId, { status: "accepted", t3Sequence: 4 });

    const raw = await readFile(audit.filePath, "utf8");
    expect(raw).not.toContain("journal-key");
    const page = await audit.query({ source: "journal", operation: "thread.archive", limit: 50 });
    expect(page.items.map((event) => event.event)).toEqual(["operation.begin", "operation.update"]);
  });

  it("retains useful metadata for long and sensitive values", () => {
    const summary = summarizeForAudit({
      instruction: "do not expose this instruction",
      text: "assistant output",
      answers: { question: "private answer" },
      query: "keep this filter",
      longValue: "x".repeat(600),
    }) as Record<string, any>;

    expect(summary.instruction).toMatchObject({ redacted: true, length: 30 });
    expect(summary.text).toMatchObject({ redacted: true, length: 16 });
    expect(summary.answers).toMatchObject({ redacted: true, type: "object" });
    expect(summary.query).toBe("keep this filter");
    expect(summary.longValue).toMatchObject({ truncated: true, length: 600 });
  });
});

async function makeDirectory(): Promise<string> {
  const directory = await mkdtemp(join(tmpdir(), "t3-code-mcp-audit-"));
  directories.push(directory);
  return directory;
}
