import { createHash } from "node:crypto";
import { z } from "zod";
import { sanitizeFailureText } from "./failure.js";

const nonempty = z.string().refine((value) => value.trim().length > 0, "Expected a nonempty string");
const count = z.number().int().nonnegative();
const dayPattern = /^[0-9]{4}-[0-9]{2}-[0-9]{2}$/;
const timestampPattern = /^([0-9]{4}-[0-9]{2}-[0-9]{2})[Tt]((?:[01][0-9]|2[0-3]):[0-5][0-9]:[0-5][0-9])(?:\.([0-9]+))?([Zz]|[+-](?:[01][0-9]|2[0-3]):[0-5][0-9])$/;

function dayNumber(value: string): number | undefined {
  if (!dayPattern.test(value)) return undefined;
  const milliseconds = Date.parse(`${value}T00:00:00Z`);
  if (!Number.isFinite(milliseconds) || new Date(milliseconds).toISOString().slice(0, 10) !== value) return undefined;
  return milliseconds / 86_400_000;
}

function instant(value: string): { seconds: number; fraction: string } | undefined {
  const match = timestampPattern.exec(value);
  if (!match || match[0] !== value || dayNumber(match[1]!) === undefined) return undefined;
  const seconds = Date.parse(`${match[1]}T${match[2]}${match[4]!.toUpperCase()}`) / 1_000;
  return Number.isFinite(seconds) ? { seconds, fraction: match[3] ?? "" } : undefined;
}

const day = z.string().refine((value) => dayNumber(value) !== undefined, "Expected a real calendar date in YYYY-MM-DD format");
const timestamp = z.string().refine((value) => instant(value) !== undefined, "Expected an RFC3339 timestamp with a timezone");

export const UsageHistoryInputSchema = z.object({
  sinceDay: day,
  untilDay: day,
  timeZone: z.string().trim().min(1),
  resolution: z.enum(["day", "hour"]).optional(),
  sinceTime: timestamp.optional(),
  untilTime: timestamp.optional(),
  awaitRefresh: z.boolean().optional(),
});
export type UsageHistoryInput = z.infer<typeof UsageHistoryInputSchema>;

const validatedInput = UsageHistoryInputSchema.superRefine((input, ctx) => {
  const since = dayNumber(input.sinceDay);
  const until = dayNumber(input.untilDay);
  if (since !== undefined && until !== undefined) {
    const limit = 366;
    if (since > until) {
      ctx.addIssue({ code: "custom", path: ["untilDay"], message: "untilDay must be on or after sinceDay" });
    } else if (until - since + 1 > limit) {
      ctx.addIssue({ code: "custom", path: ["untilDay"], message: `The ${input.resolution ?? "day"} window must include at most ${limit} days` });
    }
  }
  const start = input.sinceTime === undefined ? undefined : instant(input.sinceTime);
  const end = input.untilTime === undefined ? undefined : instant(input.untilTime);
  if (input.resolution === "hour") {
    const milliseconds = (value: { seconds: number; fraction: string }) => value.seconds * 1_000 + Number(value.fraction.slice(0, 3).padEnd(3, "0"));
    if (!start || !end) {
      ctx.addIssue({ code: "custom", message: "Hourly usage requires sinceTime and untilTime" });
    } else if (milliseconds(end) - milliseconds(start) <= 0 || milliseconds(end) - milliseconds(start) > 86_400_000) {
      ctx.addIssue({ code: "custom", message: "Hourly usage window must be greater than zero and at most 24 hours" });
    }
  }
  if (start && end) {
    const precision = Math.max(start.fraction.length, end.fraction.length);
    if (start.seconds > end.seconds || (start.seconds === end.seconds
      && start.fraction.padEnd(precision, "0") > end.fraction.padEnd(precision, "0"))) {
      ctx.addIssue({ code: "custom", path: ["untilTime"], message: "untilTime must be on or after sinceTime" });
    }
  }
});

export function validateUsageHistoryInput(input: unknown): UsageHistoryInput {
  return validatedInput.parse(input);
}

const provider = nonempty.describe("Usage provider kind. Known values are claude, codex, grok, cursor, opencode, antigravity, and pi. Future provider strings are retained.");
const totals = z.object({
  uncachedInputTokens: count,
  cachedInputTokens: count,
  cacheCreationTokens: count,
  outputTokens: count,
  reasoningTokens: count,
});
const categoryCost = z.object({
  input: z.number(),
  cacheRead: z.number(),
  cacheWrite: z.number(),
  output: z.number(),
});
const bucket = z.object({
  day,
  hourStart: nonempty.optional(),
  provider,
  model: nonempty,
  totals,
  costUsd: z.number(),
  cacheSavingsUsd: z.number(),
  categoryCostUsd: categoryCost.optional(),
  fastCostUsd: z.number().optional(),
  ultrafastCostUsd: z.number().optional(),
  speedPremiumUsd: z.number().optional(),
  costSource: nonempty.describe("Cost provenance, including providerReported, modelPriced, or unpriced. Future values are retained."),
  records: count,
  unpricedRecords: count,
  sessions: count,
});
const sourceFields = {
  status: nonempty.describe("Source status, including ok, missing, partial, or failed. Future values are retained."),
  scannedFiles: count,
  skippedFiles: count,
  malformedRecords: count,
  distinctSessions: count,
  message: nonempty.nullable(),
  action: nonempty.optional(),
  refreshing: z.literal(true).optional(),
};
const sourceOutput = z.object({
  ...sourceFields,
  sourceRef: z.string().length(16).regex(/^[a-f0-9]{16}$/),
  provider,
});
const source = z.object({
  ...sourceFields,
  message: sourceFields.message.transform((value) => value === null ? null : sanitizeFailureText(value, 2_000)),
  fingerprint: z.object({ hostId: nonempty, provider, resolvedHomePath: nonempty, volumeId: z.string() }),
}).transform(({ fingerprint, ...fields }) => ({
  ...fields,
  sourceRef: createHash("sha256").update(JSON.stringify([
    fingerprint.hostId, fingerprint.provider, fingerprint.resolvedHomePath, fingerprint.volumeId,
  ])).digest("hex").slice(0, 16),
  provider: fingerprint.provider,
}));
const summaryFields = {
  contractVersion: z.number(),
  readAt: z.string(),
  timeZone: nonempty,
  sinceDay: day,
  untilDay: day,
  buckets: z.array(bucket),
  pricing: z.object({
    status: z.enum(["fresh", "cached", "unavailable"]),
    source: nonempty,
    fetchedAt: z.string().nullable(),
    knownModels: count,
  }),
  scanDurationMs: count,
};

export const UsageSummarySchema = z.object({ ...summaryFields, sources: z.array(source) });
export type UsageSummary = z.infer<typeof UsageSummarySchema>;

export const UsageHistoryOutputSchema = z.object({
  environmentId: nonempty,
  observedAt: z.iso.datetime({ offset: true }),
  ...summaryFields,
  sources: z.array(sourceOutput),
});
