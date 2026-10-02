import { mkdtemp, rm, stat } from "node:fs/promises";
import { createWriteStream } from "node:fs";
import { once } from "node:events";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";

if (process.argv[2] === "--child") {
  const { AuditLog } = await import("../dist/operations/audit-log.js");
  const { summarizeUsage } = await import("../dist/operations/usage-summary.js");
  const started = performance.now();
  const page = await new AuditLog(process.argv[3]).query({ source: "mcp", cursor: "65535", limit: 1 });
  const summary = await summarizeUsage(process.argv[3], {});
  if (page.items.length !== 1 || page.total !== 131_072 || summary.denominators.toolCalls !== page.total) throw new Error("Benchmark counts differ from fixture.");
  console.log(JSON.stringify({ bytes: (await stat(process.argv[3])).size, validEvents: page.total,
    retainedPageEvents: page.items.length, elapsedMs: Math.round(performance.now() - started),
    heapLimitMB: 32, peakRssKB: process.resourceUsage().maxRSS }));
} else {
  const directory = await mkdtemp(join(tmpdir(), "t3-audit-benchmark-"));
  try {
    const path = join(directory, "audit.jsonl");
    const row = JSON.stringify({ version: 1, eventId: "benchmark", timestamp: "2026-09-30T00:00:00Z", processId: 1,
      source: "mcp", event: "tool.call", operation: "t3_thread_get", details: { padding: "x".repeat(400) } }) + "\n";
    const block = row.repeat(1_024);
    const output = createWriteStream(path, { mode: 0o600 });
    for (let i = 0; i < 128; i += 1) if (!output.write(block)) await once(output, "drain");
    output.end(); await once(output, "finish");
    const result = await promisify(execFile)(process.execPath,
      ["--max-old-space-size=32", fileURLToPath(import.meta.url), "--child", path], { timeout: 60_000 });
    console.log(result.stdout.trim());
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}
