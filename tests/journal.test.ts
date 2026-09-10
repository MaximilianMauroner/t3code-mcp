import { mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { IdempotencyConflictError, OperationJournal, hashPayload } from "../src/operations/journal.js";

const temporaryDirectories: string[] = [];

afterEach(async () => {
  await Promise.all(temporaryDirectories.splice(0).map((directory) => rm(directory, { recursive: true, force: true })));
});

describe("OperationJournal", () => {
  it("returns the same operation for an identical idempotency key", async () => {
    const directory = await mkdtemp(join(tmpdir(), "t3-code-mcp-journal-"));
    temporaryDirectories.push(directory);
    const journal = new OperationJournal(join(directory, "operations.json"));
    const input = {
      kind: "thread.turn.start" as const,
      idempotencyKey: "same-key",
      payloadHash: hashPayload({ threadId: "thread-1", message: "hello" }),
      threadId: "thread-1",
      runId: "run-1",
      messageId: "message-1",
    };

    const first = await journal.begin(input);
    const second = await journal.begin(input);

    expect(second.reused).toBe(true);
    expect(second.record.operationId).toBe(first.record.operationId);
    expect(second.record.commandId).toBe(first.record.commandId);
    await expect(
      journal.begin({ ...input, payloadHash: hashPayload({ threadId: "thread-1", message: "different" }) }),
    ).rejects.toBeInstanceOf(IdempotencyConflictError);
  });

  it("recovers an uncompleted prepared operation as uncertain", async () => {
    const directory = await mkdtemp(join(tmpdir(), "t3-code-mcp-journal-"));
    temporaryDirectories.push(directory);
    const path = join(directory, "operations.json");
    const first = new OperationJournal(path);
    await first.begin({
      kind: "project.create",
      idempotencyKey: "crash-key",
      payloadHash: hashPayload({ title: "demo" }),
      projectId: "project-1",
    });

    const restarted = new OperationJournal(path);
    const recovered = await restarted.getByIdempotencyKey("crash-key");

    expect(recovered?.status).toBe("uncertain");
    expect(recovered?.lastError).toContain("stopped");
  });

  it("coalesces concurrent begins for one idempotency key", async () => {
    const directory = await mkdtemp(join(tmpdir(), "t3-code-mcp-journal-"));
    temporaryDirectories.push(directory);
    const journal = new OperationJournal(join(directory, "operations.json"));
    const input = {
      kind: "thread.turn.start" as const,
      idempotencyKey: "concurrent-key",
      payloadHash: hashPayload({ threadId: "thread-1", message: "hello" }),
      threadId: "thread-1",
      runId: "run-1",
      messageId: "message-1",
    };

    const results = await Promise.all(Array.from({ length: 20 }, () => journal.begin(input)));
    const operationIds = new Set(results.map((result) => result.record.operationId));

    expect(operationIds).toHaveLength(1);
    expect(results.filter((result) => result.reused)).toHaveLength(19);
  });

  it("hashes object key order canonically but preserves array order", () => {
    expect(hashPayload({ a: 1, b: [2, 3] })).toBe(hashPayload({ b: [2, 3], a: 1 }));
    expect(hashPayload({ a: [1, 2] })).not.toBe(hashPayload({ a: [2, 1] }));
  });

  it("persists accepted and uncertain transitions with restrictive file permissions", async () => {
    const directory = await mkdtemp(join(tmpdir(), "t3-code-mcp-journal-"));
    temporaryDirectories.push(directory);
    const path = join(directory, "nested", "operations.json");
    const journal = new OperationJournal(path);
    const begun = await journal.begin({
      kind: "thread.archive",
      idempotencyKey: "persist-key",
      payloadHash: hashPayload({ threadId: "thread-1" }),
      threadId: "thread-1",
    });
    await journal.update(begun.record.operationId, { status: "accepted", t3Sequence: 42 });

    const restarted = new OperationJournal(path);
    const record = await restarted.getByIdempotencyKey("persist-key");
    const statMode = await stat(path);

    expect(record).toMatchObject({ status: "accepted", t3Sequence: 42 });
    expect(statMode.mode & 0o777).toBe(0o600);
    expect(JSON.parse(await readFile(path, "utf8"))).toMatchObject({ version: 1, operations: expect.any(Array) });
  });

  it("fails closed on malformed or structurally invalid journals", async () => {
    const directory = await mkdtemp(join(tmpdir(), "t3-code-mcp-journal-"));
    temporaryDirectories.push(directory);
    const path = join(directory, "operations.json");
    await writeFile(path, JSON.stringify({ operations: [{ status: "accepted" }] }));

    await expect(new OperationJournal(path).init()).rejects.toThrow("invalid operation");
  });

  it("rejects an idempotency key reused across operation kinds", async () => {
    const directory = await mkdtemp(join(tmpdir(), "t3-code-mcp-journal-"));
    temporaryDirectories.push(directory);
    const journal = new OperationJournal(join(directory, "operations.json"));
    await journal.begin({
      kind: "thread.archive",
      idempotencyKey: "cross-kind-key",
      payloadHash: hashPayload({ threadId: "thread-1" }),
      threadId: "thread-1",
    });

    await expect(
      journal.begin({
        kind: "thread.turn.start",
        idempotencyKey: "cross-kind-key",
        payloadHash: hashPayload({ threadId: "thread-1", message: "different operation" }),
        threadId: "thread-1",
      }),
    ).rejects.toBeInstanceOf(IdempotencyConflictError);
  });
});
