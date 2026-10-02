import { mkdtemp, writeFile, rm, appendFile } from "node:fs/promises";
import { appendFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { AuditLog, scanAuditFile, withAuditContext } from "../src/operations/audit-log.js";
import { summarizeUsage } from "../src/operations/usage-summary.js";

const directories: string[] = [];
afterEach(async () => { await Promise.all(directories.splice(0).map((directory) => rm(directory, { recursive: true, force: true }))); });
async function file() {
  const directory = await mkdtemp(join(tmpdir(), "t3-usage-summary-")); directories.push(directory);
  return join(directory, "audit.jsonl");
}
function event(index: number, override: Record<string, unknown> = {}) {
  return { version: 1, eventId: `event-${index}`, timestamp: `2026-09-30T12:00:${String(index).padStart(2, "0")}.000Z`, processId: 1,
    source: "mcp", event: "tool.call", operation: "t3_thread_send", ...override };
}

describe("streaming audit queries", () => {
  it("preserves filtered offsets, totals and invalid-line counts across pages", async () => {
    const path = await file();
    await writeFile(path, [event(0), event(1, { source: "git" }), event(2), {}, event(3), event(4)].map((item) => JSON.stringify(item)).join("\n") + "\nnot-json\n\n");
    const audit = new AuditLog(path);
    const first = await audit.query({ source: "mcp", since: "2026-09-30T12:00:02Z", limit: 1 });
    expect(first).toMatchObject({ total: 3, invalidLines: 2, nextCursor: "1", hasMore: true });
    const second = await audit.query({ source: "mcp", since: "2026-09-30T12:00:02Z", cursor: first.nextCursor!, limit: 2 });
    expect(second.items.map((item) => item.eventId)).toEqual(["event-3", "event-4"]);
    expect(second).toMatchObject({ nextCursor: null, hasMore: false, total: 3, invalidLines: 2 });
    await appendFile(path, JSON.stringify(event(5)) + "\n");
    const appended = await audit.query({ source: "mcp", since: "2026-09-30T12:00:02Z", cursor: "3", limit: 2 });
    expect(appended.items.map((item) => item.eventId)).toEqual(["event-5"]);
    await expect(audit.query({ since: "invalid", limit: 1 })).rejects.toThrow("since");
  });

  it("does not chase appends while scanning a fixed byte snapshot", async () => {
    const path = await file();
    await writeFile(path, JSON.stringify(event(0)) + "\n");
    const seen: string[] = [];
    const scan = await scanAuditFile(path, (item) => {
      seen.push(item.eventId);
      appendFileSync(path, JSON.stringify(event(1)) + "\n");
    });
    expect(seen).toEqual(["event-0"]); expect(scan.validLines).toBe(1);
    expect((await new AuditLog(path).query({ limit: 10 })).total).toBe(2);
  });

  it("keeps concurrent context attribution isolated for upstream, Git and journal events", async () => {
    const path = await file(); const audit = new AuditLog(path);
    await Promise.all(["A", "B"].map((id) => withAuditContext(`mcp-${id}`, `tool-${id}`, async () => {
      await new Promise((resolve) => setTimeout(resolve, id === "A" ? 5 : 0));
      for (const source of ["t3", "git", "journal"] as const) {
        await audit.record({ source, event: "child", correlationId: `${source}-${id}`, operation: id });
      }
    })));
    const page = await audit.query({ limit: 10 });
    expect(page.items).toHaveLength(6);
    for (const item of page.items) {
      expect(item.parentCorrelationId).toBe(`mcp-${item.operation}`);
      expect(item.parentOperation).toBe(`tool-${item.operation}`);
    }
    await audit.record({ source: "t3", event: "outside" });
    expect((await audit.query({ event: "outside", limit: 1 })).items[0]?.parentCorrelationId).toBeUndefined();
  });
});

describe("read-only usage summary", () => {
  it("counts launches, errors, observations and correlation with explicit denominators", async () => {
    const path = await file();
    const thread = { id: "thread", activity: "idle", latestTurn: { turnId: "turn" }, updatedAt: "unchanged" };
    const rows = [
      event(0, { details: { arguments: {} } }),
      event(1, { details: { arguments: { modelSelection: { model: "selected" } } } }),
      event(2, { event: "tool.result", outcome: "error", durationMs: 12, details: { errorCode: "thread_busy" } }),
      event(3, { operation: "t3_thread_get", event: "tool.result", outcome: "completed", details: { result: { thread } } }),
      event(4, { operation: "t3_thread_get", event: "tool.result", outcome: "completed", details: { result: { thread } } }),
      event(5, { operation: "t3_run_wait", event: "tool.result", outcome: "completed", durationMs: 30_002,
        details: { result: { timedOut: true, runStatus: "running", monitoring: { observations: 18 } } } }),
      event(6, { source: "t3", event: "upstream.request", operation: "GET /api/orchestration/threads/thread", parentCorrelationId: "mcp-call", parentOperation: "t3_run_wait" }),
      event(7, { source: "t3", event: "upstream.response", operation: "GET /api/orchestration/threads/thread", durationMs: 33, outcome: "completed" }),
      event(8, { source: "t3", event: "upstream.request", operation: "GET /.well-known/t3/environment" }),
    ];
    await writeFile(path, rows.map((item) => JSON.stringify(item)).join("\n") + "\ninvalid\n");
    const report = await summarizeUsage(path, { since: "2026-09-30T12:00:00Z", until: "2026-09-30T12:00:08Z" });
    expect(report.scan).toMatchObject({ includedEvents: 9, invalidLines: 1 });
    expect(report.denominators).toEqual({ toolCalls: 2, dispatchCalls: 2, upstreamRequests: 2, waits: 1, comparableThreadPairs: 1 });
    expect(report.launches.omittedModelSelection).toBe(1);
    expect(report.monitoring).toMatchObject({ unchangedActiveWaits: 1, measuredWaits: 1, waitObservations: 18, unchangedThreadPairs: 1 });
    expect(report.attribution).toEqual({ attributedRequests: 1, unattributedRequests: 1, requestsByTool: { t3_run_wait: 1 } });
    expect(report.errorCodes).toEqual({ "t3_thread_send:thread_busy": 1 });
    expect(report.endpoints["GET /api/orchestration/threads/:threadId"]?.latency).toMatchObject({ count: 1, meanMs: 33, p95UpperBoundMs: 50 });
    expect(JSON.stringify(report)).not.toContain('"id":"thread"');
  });

  it("handles absent logs and rejects inverted or malformed windows", async () => {
    const path = await file();
    expect((await summarizeUsage(path, {})).scan.validLines).toBe(0);
    await expect(summarizeUsage(path, { since: "bad" })).rejects.toThrow("since");
    await expect(summarizeUsage(path, { since: "2026-10-01", until: "2026-09-30" })).rejects.toThrow("before");
  });
});
