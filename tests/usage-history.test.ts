import { describe, expect, it } from "vitest";
import { z } from "zod";
import {
  UsageHistoryInputSchema,
  UsageHistoryOutputSchema,
  UsageSummarySchema,
  validateUsageHistoryInput,
} from "../src/t3/usage-history.js";

const input = { sinceDay: "2026-10-01", untilDay: "2026-10-07", timeZone: "UTC" };
const readAt = "2026-10-23T12:00:00Z";

function summary() {
  return {
    contractVersion: 6, readAt, timeZone: "UTC", sinceDay: "2026-10-01", untilDay: "2026-10-07",
    buckets: [{
      day: "2026-10-01", hourStart: "2026-10-01T01:00:00Z", provider: "codex", model: "vendor/model-v1",
      sourcePath: "/home/max/.codex/sessions/private.jsonl",
      totals: { uncachedInputTokens: 11, cachedInputTokens: 12, cacheCreationTokens: 13, outputTokens: 14, reasoningTokens: 5, secret: "hidden-total" },
      costUsd: 0.125, cacheSavingsUsd: 0.025,
      categoryCostUsd: { input: 0.01, cacheRead: 0.02, cacheWrite: 0.03, output: 0.065, secret: "hidden-category" },
      fastCostUsd: 0.02, ultrafastCostUsd: 0.03, speedPremiumUsd: 0.04,
      costSource: "providerReported", records: 3, unpricedRecords: 1, sessions: 2, secret: "hidden-bucket",
    }],
    sources: [{
      fingerprint: { hostId: "host-a", provider: "codex", resolvedHomePath: "/home/max/.codex", volumeId: "7:42", secret: "hidden-fingerprint" },
      status: "partial", scannedFiles: 8, skippedFiles: 2, malformedRecords: 1, distinctSessions: 2,
      message: "Cannot read /home/max/.codex/private.jsonl token=private-token at https://private.example/log",
      action: "enableCursorKeychain", refreshing: true, secret: "hidden-source",
    }],
    pricing: { status: "cached", source: "model catalog", fetchedAt: null, knownModels: 100, secret: "hidden-pricing" },
    scanDurationMs: 17, secret: "hidden-root",
  };
}

describe("usage history input", () => {
  it("keeps the request options and trims the timezone", () => {
    expect(validateUsageHistoryInput({ ...input, timeZone: " UTC ", resolution: "day", awaitRefresh: false, extra: true })).toEqual({
      ...input, resolution: "day", awaitRefresh: false,
    });
  });

  it.each(["2026-02-29", "2026-02-30", "1900-02-29", "2026-04-31", "2026-00-01", "2026-13-01", "2026-10-00", "2026-1-01", " 2026-10-01", "not-a-day"])("rejects invalid calendar date %s", (sinceDay) => {
    expect(() => validateUsageHistoryInput({ ...input, sinceDay })).toThrow();
    expect(() => validateUsageHistoryInput({ ...input, untilDay: sinceDay })).toThrow();
  });

  it.each(["0000-02-29", "2000-02-29", "2024-02-29", "0099-12-31"])("accepts real calendar date %s", (value) => {
    expect(validateUsageHistoryInput({ ...input, sinceDay: value, untilDay: value }).sinceDay).toBe(value);
  });

  it("accepts inclusive day limits across leap years", () => {
    expect(validateUsageHistoryInput({ ...input, sinceDay: "2024-01-01", untilDay: "2024-12-31" })).toBeDefined();
    expect(validateUsageHistoryInput({ ...input, sinceDay: "2026-01-01", untilDay: "2027-01-01" })).toBeDefined();
  });

  it.each([
    { sinceDay: "2026-10-08" },
    { sinceDay: "2024-01-01", untilDay: "2025-01-01" },
    { sinceDay: "2026-01-01", untilDay: "2027-01-02", resolution: "day" },
    { untilDay: "2026-10-08", resolution: "hour" },
  ])("rejects reversed or oversized windows %j", (changes) => {
    expect(() => validateUsageHistoryInput({ ...input, ...changes })).toThrow();
  });

  it("accepts hourly intervals up to 24 hours with explicit time bounds", () => {
    for (const times of [
      { sinceTime: "2026-10-01T00:00:00Z", untilTime: "2026-10-02T00:00:00Z" },
      { sinceTime: "2026-10-01T02:00:00+02:00", untilTime: "2026-10-01T01:00:00Z" },
    ]) expect(validateUsageHistoryInput({ ...input, resolution: "hour", ...times })).toMatchObject(times);
  });

  it.each([
    {},
    { sinceTime: "2026-10-01T00:00:00Z" },
    { untilTime: "2026-10-02T00:00:00Z" },
    { sinceTime: "2026-10-01T00:00:00Z", untilTime: "2026-10-01T00:00:00Z" },
    { sinceTime: "2026-10-01T00:00:00.0000000001Z", untilTime: "2026-10-01T00:00:00.0000000002Z" },
    { sinceTime: "2026-10-01T00:00:00Z", untilTime: "2026-10-02T00:00:00.001Z" },
    { sinceTime: "2026-10-02T00:00:00Z", untilTime: "2026-10-01T00:00:00Z" },
  ])("rejects hourly bounds that T3 cannot read %j", (times) => {
    expect(() => validateUsageHistoryInput({ ...input, resolution: "hour", ...times })).toThrow();
  });

  it("compares instants by offset and arbitrary fractional precision", () => {
    const times = { sinceTime: "2026-10-01T02:00:00+02:00", untilTime: "2026-10-01T00:00:00Z" };
    expect(validateUsageHistoryInput({ ...input, ...times })).toMatchObject(times);
    expect(() => validateUsageHistoryInput({ ...input, sinceTime: "2026-10-01T00:00:00.0000000002Z", untilTime: "2026-10-01T00:00:00.0000000001Z" })).toThrow(/untilTime/);
    expect(validateUsageHistoryInput({ ...input, sinceTime: "2026-10-01t00:00:00.10z", untilTime: "2026-10-01T00:00:00.1Z" })).toBeDefined();
    expect(() => validateUsageHistoryInput({ ...input, sinceTime: "2026-10-01T00:00:01Z", untilTime: "2026-10-01T00:00:00Z" })).toThrow(/untilTime/);
  });

  it("keeps a single time bound and defers requested timezone boundaries upstream", () => {
    expect(validateUsageHistoryInput({
      sinceDay: "2026-10-23", untilDay: "2026-10-23", timeZone: "America/Los_Angeles", sinceTime: "2026-10-24T06:00:00Z",
    }).sinceTime).toBe("2026-10-24T06:00:00Z");
    expect(validateUsageHistoryInput({ ...input, untilTime: "2026-10-07T23:00:00-07:00" })).toBeDefined();
  });

  it.each(["2026-02-30T00:00:00Z", "2026-10-01T00:00:00", "2026-10-01", "2026-10-01T24:00:00Z", "2026-10-01T01:60:00Z", "2026-10-01T01:00:00+24:00", "2026-10-01T00:00:00Z\n"])("rejects invalid timestamp %s", (sinceTime) => {
    expect(() => validateUsageHistoryInput({ ...input, sinceTime })).toThrow();
    expect(() => validateUsageHistoryInput({ ...input, untilTime: sinceTime })).toThrow();
  });

  it.each([null, [], {}, { ...input, timeZone: " " }, { ...input, timeZone: null }, { ...input, resolution: "week" }, { ...input, sinceTime: null }, { ...input, awaitRefresh: "true" }])("rejects invalid request %j", (value) => {
    expect(() => validateUsageHistoryInput(value)).toThrow();
  });
});

describe("usage history summary", () => {
  it("preserves reported metrics and strips private fields at every object boundary", () => {
    const parsed = UsageSummarySchema.parse(summary());
    expect(parsed.buckets).toEqual([{
      day: "2026-10-01", hourStart: "2026-10-01T01:00:00Z", provider: "codex", model: "vendor/model-v1",
      totals: { uncachedInputTokens: 11, cachedInputTokens: 12, cacheCreationTokens: 13, outputTokens: 14, reasoningTokens: 5 },
      costUsd: 0.125, cacheSavingsUsd: 0.025,
      categoryCostUsd: { input: 0.01, cacheRead: 0.02, cacheWrite: 0.03, output: 0.065 },
      fastCostUsd: 0.02, ultrafastCostUsd: 0.03, speedPremiumUsd: 0.04,
      costSource: "providerReported", records: 3, unpricedRecords: 1, sessions: 2,
    }]);
    expect(parsed.sources).toEqual([{
      sourceRef: "56295e100793dd16", provider: "codex", status: "partial", scannedFiles: 8, skippedFiles: 2,
      malformedRecords: 1, distinctSessions: 2, action: "enableCursorKeychain", refreshing: true,
      message: "Cannot read [REDACTED PATH] token=[REDACTED] at [REDACTED URL]",
    }]);
    expect(parsed).toMatchObject({ contractVersion: 6, readAt, timeZone: "UTC", sinceDay: input.sinceDay, untilDay: input.untilDay,
      pricing: { status: "cached", source: "model catalog", fetchedAt: null, knownModels: 100 }, scanDurationMs: 17 });
    const output = UsageHistoryOutputSchema.parse({ environmentId: "environment-a", observedAt: readAt, ...parsed });
    expect(output).toEqual({ environmentId: "environment-a", observedAt: readAt, ...parsed });
    for (const privateValue of ["hidden-", "/home/max", "volumeId", "fingerprint", "host-a", "sourcePath", "private-token", "private.example"]) {
      expect(JSON.stringify(output)).not.toContain(privateValue);
    }
    expect(output).not.toHaveProperty("remainingPercent");
    expect(output.buckets[0]!.totals).not.toHaveProperty("totalTokens");
  });

  it("retains known and future providers and keeps source identities distinct", () => {
    for (const provider of ["claude", "codex", "grok", "cursor", "opencode", "antigravity", "pi", "future-provider"]) {
      const payload = summary();
      payload.buckets[0]!.provider = provider;
      payload.sources[0]!.fingerprint.provider = provider;
      const parsed = UsageSummarySchema.parse(payload);
      expect(parsed.buckets[0]!.provider).toBe(provider);
      expect(parsed.sources[0]!.provider).toBe(provider);
    }
    const original = UsageSummarySchema.parse(summary()).sources[0]!.sourceRef;
    for (const key of ["hostId", "provider", "resolvedHomePath", "volumeId"] as const) {
      const payload = summary();
      payload.sources[0]!.fingerprint[key] += "-other";
      expect(UsageSummarySchema.parse(payload).sources[0]!.sourceRef).not.toBe(original);
    }
    expect(UsageSummarySchema.parse(summary()).sources[0]!.sourceRef).toBe(original);
  });

  it("retains future history element tags without discarding their data", () => {
    const payload = summary();
    payload.buckets[0]!.costSource = "future-cost";
    payload.sources[0]!.status = "future-status";
    payload.sources[0]!.action = "future-action";
    expect(UsageSummarySchema.parse(payload)).toMatchObject({
      buckets: [{ costSource: "future-cost" }], sources: [{ status: "future-status", action: "future-action" }],
    });
  });

  it("keeps empty histories, null messages, optional fields and pricing status", () => {
    const payload = summary();
    expect(UsageSummarySchema.parse({ ...payload, buckets: [], sources: [], pricing: { ...payload.pricing, status: "unavailable" } })).toMatchObject({ buckets: [], sources: [], pricing: { status: "unavailable" } });
    const { hourStart, fastCostUsd, ultrafastCostUsd, speedPremiumUsd, categoryCostUsd, ...minimalBucket } = payload.buckets[0]!;
    const { action, refreshing, ...minimalSource } = payload.sources[0]!;
    const parsed = UsageSummarySchema.parse({ ...payload, buckets: [{ ...minimalBucket, costSource: "unpriced" }], sources: [{ ...minimalSource, message: null }] });
    expect(parsed.sources[0]!.message).toBeNull();
    expect(parsed.sources[0]).not.toHaveProperty("action");
    expect(parsed.sources[0]).not.toHaveProperty("refreshing");
    expect(parsed.buckets[0]).not.toHaveProperty("hourStart");
    expect(parsed.buckets[0]!.costSource).toBe("unpriced");
  });

  const invalid: Array<{ path: Array<string | number>; value: unknown }> = [
    { path: [], value: [] },
    { path: ["buckets"], value: {} },
    { path: ["buckets", 0], value: [] },
    { path: ["buckets", 0, "totals"], value: [1, 2, 3, 4, 5] },
    { path: ["buckets", 0, "categoryCostUsd"], value: [1, 2, 3, 4] },
    { path: ["buckets", 0, "totals", "outputTokens"], value: -1 },
    { path: ["buckets", 0, "records"], value: 0.5 },
    { path: ["buckets", 0, "sessions"], value: Number.MAX_SAFE_INTEGER + 1 },
    { path: ["buckets", 0, "model"], value: " " },
    { path: ["buckets", 0, "provider"], value: null },
    { path: ["buckets", 0, "costUsd"], value: Infinity },
    { path: ["buckets", 0, "costSource"], value: 1 },
    { path: ["buckets", 0, "fastCostUsd"], value: null },
    { path: ["sources"], value: null },
    { path: ["sources", 0], value: [] },
    { path: ["sources", 0, "fingerprint"], value: ["host-a", "codex", "/home/max/.codex", "7:42"] },
    { path: ["sources", 0, "fingerprint", "volumeId"], value: null },
    { path: ["sources", 0, "status"], value: "" },
    { path: ["sources", 0, "refreshing"], value: false },
    { path: ["sources", 0, "action"], value: false },
    { path: ["pricing"], value: ["cached", "catalog", null, 100] },
    { path: ["pricing", "knownModels"], value: "100" },
    { path: ["pricing", "fetchedAt"], value: false },
    { path: ["scanDurationMs"], value: -1 },
  ];
  it.each(invalid)("rejects invalid summary field $path", ({ path, value }) => {
    if (path.length === 0) {
      expect(UsageSummarySchema.safeParse(value).success).toBe(false);
      return;
    }
    const payload = summary();
    let target: unknown = payload;
    for (const key of path.slice(0, -1)) target = (target as Record<string | number, unknown>)[key];
    (target as Record<string | number, unknown>)[path.at(-1)!] = value;
    expect(UsageSummarySchema.safeParse(payload).success).toBe(false);
  });

  it("requires all upstream summary fields", () => {
    for (const key of Object.keys(summary())) {
      if (key === "secret") continue;
      const payload: Record<string, unknown> = summary();
      delete payload[key];
      expect(UsageSummarySchema.safeParse(payload).success).toBe(false);
    }
  });

  it("publishes object schemas for MCP input and safe output", () => {
    expect(z.toJSONSchema(UsageHistoryInputSchema).type).toBe("object");
    expect(z.toJSONSchema(UsageHistoryOutputSchema).type).toBe("object");
    expect(JSON.stringify(z.toJSONSchema(UsageHistoryOutputSchema))).not.toContain("fingerprint");
  });
});
