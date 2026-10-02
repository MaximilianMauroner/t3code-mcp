import { scanAuditFile, parseFilterTime, type AuditEvent } from "./audit-log.js";

interface Latency {
  count: number;
  totalMs: number;
  minMs: number;
  maxMs: number;
  buckets: number[];
}
const LATENCY_BOUNDS = [1, 5, 10, 50, 100, 500, 1_000, 5_000, 10_000, 30_000, 60_000, Infinity];
const MAX_TRACKED_THREADS = 10_000;
const MAX_OPERATIONS = 1_000;

/** Fixed-cardinality aggregation; no prompt text or per-call history is retained. */
export async function summarizeUsage(filePath: string, window: { since?: string; until?: string }) {
  const since = parseFilterTime(window.since, "since");
  const until = parseFilterTime(window.until, "until");
  if (since !== null && until !== null && since > until) throw new Error("since must be before or equal to until.");
  const tools = new Map<string, { calls: number; completed: number; errors: number; latency: Latency }>();
  const endpoints = new Map<string, { requests: number; errors: number; latency: Latency }>();
  const attributed = new Map<string, number>();
  const errors = new Map<string, number>();
  const previous = new Map<string, string>();
  let includedEvents = 0, toolCalls = 0, upstreamRequests = 0, attributedRequests = 0;
  let dispatchCalls = 0, omittedModelSelection = 0, waits = 0, unchangedActiveWaits = 0;
  let comparableThreadPairs = 0, unchangedThreadPairs = 0, evictedThreadStates = 0;
  let waitObservations = 0, measuredWaits = 0;
  let first: string | null = null, last: string | null = null;
  const scan = await scanAuditFile(filePath, (event) => {
    const timestamp = Date.parse(event.timestamp);
    if (since !== null && (!Number.isFinite(timestamp) || timestamp < since)) return;
    if (until !== null && (!Number.isFinite(timestamp) || timestamp > until)) return;
    includedEvents += 1;
    if (Number.isFinite(timestamp)) {
      if (first === null || timestamp < Date.parse(first)) first = event.timestamp;
      if (last === null || timestamp > Date.parse(last)) last = event.timestamp;
    }
    const details = object(event.details);
    if (event.source === "mcp") {
      const name = boundedKey(tools, event.operation ?? "unknown");
      const entry = tools.get(name) ?? { calls: 0, completed: 0, errors: 0, latency: newLatency() };
      if (event.event === "tool.call") {
        entry.calls += 1; toolCalls += 1;
        if (["t3_task_start", "t3_thread_create", "t3_thread_send"].includes(event.operation ?? "")) {
          dispatchCalls += 1;
          if (object(details.arguments).modelSelection == null) omittedModelSelection += 1;
        }
      }
      if (event.event === "tool.result") {
        if (event.outcome === "error") {
          entry.errors += 1;
          increment(errors, `${name}:${typeof details.errorCode === "string" ? details.errorCode : "unknown"}`);
        } else if (event.outcome === "completed") entry.completed += 1;
        addLatency(entry.latency, event.durationMs);
        const result = object(details.result);
        if (event.operation === "t3_run_wait" && event.outcome === "completed") {
          waits += 1;
          if (result.timedOut === true && ["running", "accepted"].includes(String(result.runStatus))) unchangedActiveWaits += 1;
          const monitoring = object(result.monitoring);
          if (typeof monitoring.observations === "number") { measuredWaits += 1; waitObservations += monitoring.observations; }
        }
        if (event.operation === "t3_thread_get" && event.outcome === "completed") {
          const thread = object(result.thread);
          if (typeof thread.id === "string") {
            const signature = threadSignature(thread);
            const before = previous.get(thread.id);
            if (before !== undefined) { comparableThreadPairs += 1; if (before === signature) unchangedThreadPairs += 1; }
            previous.delete(thread.id);
            previous.set(thread.id, signature);
            if (previous.size > MAX_TRACKED_THREADS) {
              previous.delete(previous.keys().next().value!); evictedThreadStates += 1;
            }
          }
        }
      }
      tools.set(name, entry);
    }
    if (event.source === "t3" && ["upstream.request", "upstream.response"].includes(event.event)) {
      const name = boundedKey(endpoints, normalizeEndpoint(event.operation));
      const entry = endpoints.get(name) ?? { requests: 0, errors: 0, latency: newLatency() };
      if (event.event === "upstream.request") {
        entry.requests += 1; upstreamRequests += 1;
        if (event.parentCorrelationId && event.parentOperation) {
          attributedRequests += 1; increment(attributed, event.parentOperation);
        }
      } else {
        if (event.outcome === "error") entry.errors += 1;
        addLatency(entry.latency, event.durationMs);
      }
      endpoints.set(name, entry);
    }
  });
  return {
    window: { since: window.since ?? null, until: window.until ?? null, firstEventAt: first, lastEventAt: last },
    scan: { ...scan, includedEvents },
    denominators: { toolCalls, dispatchCalls, upstreamRequests, waits, comparableThreadPairs },
    launches: { omittedModelSelection, omissionMeans: "May intentionally use a project default or thread inheritance; not proof of an incorrect model." },
    monitoring: {
      unchangedActiveWaits, measuredWaits, waitObservations, unchangedThreadPairs, evictedThreadStates,
      comparison: "Successive successful same-thread detail results in file order; may bridge failed reads. Thread states use bounded LRU retention.",
      signatureFields: ["activity", "latestTurn", "updatedAt", "latestResponse", "hasPendingApprovals", "hasPendingUserInput", "messageCount", "activityCount"],
      interpretation: "Unchanged selected state is not proof a read was unnecessary. Wait timeout does not cancel work.",
    },
    tools: Object.fromEntries([...tools].sort().map(([name, entry]) => [name, { ...entry, latency: latencySummary(entry.latency) }])),
    endpoints: Object.fromEntries([...endpoints].sort().map(([name, entry]) => [name, { ...entry, latency: latencySummary(entry.latency) }])),
    errorCodes: Object.fromEntries([...errors].sort()),
    attribution: { attributedRequests, unattributedRequests: upstreamRequests - attributedRequests, requestsByTool: Object.fromEntries([...attributed].sort()) },
    limits: {
      maxTrackedThreads: MAX_TRACKED_THREADS, maxOperationGroups: MAX_OPERATIONS,
      latency: "Fixed histogram upper bounds for median/p95; null upper bound means over 60 seconds. Mean/min/max are exact.",
      outcomes: "Tool completion/accepted dispatch is not independently verified task success. No token or financial savings are inferred.",
    },
  };
}

function object(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? value as Record<string, unknown> : {};
}
function boundedKey(map: Map<string, unknown>, key: string): string {
  return map.has(key) || map.size < MAX_OPERATIONS - 1 ? key : "other";
}
function increment(map: Map<string, number>, key: string): void {
  const name = boundedKey(map, key); map.set(name, (map.get(name) ?? 0) + 1);
}
function normalizeEndpoint(operation: string | undefined): string {
  return (operation ?? "unknown").replace(/(\/api\/orchestration\/threads\/)[^/?\s]+/, "$1:threadId");
}
function threadSignature(thread: Record<string, unknown>): string {
  return JSON.stringify([thread.activity, thread.latestTurn, thread.updatedAt, thread.latestResponse,
    thread.hasPendingApprovals, thread.hasPendingUserInput, thread.messageCount, thread.activityCount]);
}
function newLatency(): Latency {
  return { count: 0, totalMs: 0, minMs: Infinity, maxMs: 0, buckets: LATENCY_BOUNDS.map(() => 0) };
}
function addLatency(latency: Latency, duration: number | undefined): void {
  if (duration === undefined || !Number.isFinite(duration) || duration < 0) return;
  latency.count += 1; latency.totalMs += duration;
  latency.minMs = Math.min(latency.minMs, duration); latency.maxMs = Math.max(latency.maxMs, duration);
  const bucket = LATENCY_BOUNDS.findIndex((bound) => duration <= bound);
  latency.buckets[bucket]! += 1;
}
function latencySummary(latency: Latency) {
  function quantile(fraction: number): number | null {
    if (latency.count === 0) return null;
    let cumulative = 0;
    for (const [i, count] of latency.buckets.entries()) {
      cumulative += count;
      if (cumulative >= Math.ceil(latency.count * fraction)) return Number.isFinite(LATENCY_BOUNDS[i]) ? LATENCY_BOUNDS[i]! : null;
    }
    return null;
  }
  return { count: latency.count, meanMs: latency.count ? latency.totalMs / latency.count : null,
    minMs: latency.count ? latency.minMs : null, maxMs: latency.count ? latency.maxMs : null,
    medianUpperBoundMs: quantile(0.5), p95UpperBoundMs: quantile(0.95) };
}
