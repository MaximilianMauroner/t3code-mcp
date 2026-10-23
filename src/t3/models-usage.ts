import { z } from "zod";
import { createHash } from "node:crypto";
import { sanitizeFailureText } from "./failure.js";

const nonempty = z.string().trim().min(1);
const timestamp = z.iso.datetime({ offset: true });
const model = z.object({
  slug: nonempty,
  name: nonempty,
  isCustom: z.boolean(),
  isDefault: z.boolean().optional(),
  isLegacy: z.boolean().optional(),
  aliases: z.array(nonempty).optional(),
  subProvider: nonempty.optional(),
});
const window = z.object({
  id: nonempty,
  kind: z.enum(["session", "weekly", "monthly", "other"]),
  label: nonempty,
  usedPercent: z.number().min(0).max(100),
  resetsAt: timestamp.optional(),
  windowDurationMins: z.number().int().nonnegative().optional(),
});
const usageLimits = z.object({
  checkedAt: timestamp,
  windows: z.array(window),
  unavailable: z.object({ reason: z.enum(["unsupported", "probeFailed"]) }).optional(),
  resetCredits: z.object({
    availableCount: z.number().int().nonnegative(),
    nextExpiresAt: timestamp.optional(),
  }).optional(),
});
const providerIdentity = z.object({
  instanceId: nonempty,
  driver: nonempty,
  displayName: nonempty.optional(),
  enabled: z.boolean(),
  installed: z.boolean(),
  status: z.enum(["ready", "warning", "error", "disabled"]),
  checkedAt: timestamp,
});

export const ProvidersConfigSchema = z.object({
  providers: z.array(providerIdentity.extend({ models: z.array(model), usageLimits: usageLimits.optional(), auth: z.object({ label: nonempty.optional() }).optional() })),
});
export type ProvidersConfig = z.infer<typeof ProvidersConfigSchema>;

const usageOutput = usageLimits.omit({ windows: true }).extend({
  status: z.enum(["reported", "unsupported", "probeFailed", "empty"]),
  windows: z.array(window.extend({ remainingPercent: z.number().min(0).max(100) })),
});
const usage = z.union([
  z.object({ status: z.literal("missing"), windows: z.array(z.never()) }),
  usageOutput,
]);
export const UsageSourcesSchema = z.object({ sources: z.array(z.object({
  id: nonempty,
  kind: nonempty,
  label: nonempty,
  checkedAt: timestamp,
  error: nonempty.optional(),
  accounts: z.array(z.object({
    id: nonempty,
    driver: nonempty,
    plan: nonempty.optional(),
    usageLimits,
  })),
})) });
export const RefreshOutputSchema = z.union([
  z.object({ status: z.enum(["notRequested", "completed"]) }),
  z.object({ status: z.literal("failed"), error: z.string() }),
]);
export type RefreshOutput = z.infer<typeof RefreshOutputSchema>;
export const SourcesOutputSchema = z.union([
  z.object({ status: z.literal("reported"), items: z.array(z.object({
    sourceRef: nonempty,
    kind: nonempty,
    label: z.string(),
    checkedAt: timestamp,
    error: z.string().optional(),
    accounts: z.array(z.object({
      accountRef: nonempty,
      driver: nonempty,
      plan: z.string().optional(),
      usage,
    })),
  })) }),
  z.object({ status: z.literal("unavailable"), reason: z.enum(["timeout", "streamEnded", "disconnected"]) }),
]);
export type UsageSources = z.infer<typeof UsageSourcesSchema>["sources"];
export type SourcesOutput = z.infer<typeof SourcesOutputSchema>;
export interface UsageLimitsRead {
  providers: ProvidersConfig["providers"];
  sources: SourcesOutput;
  refresh: RefreshOutput;
}
export const ModelsUsageOutputSchema = z.object({
  environmentId: nonempty,
  observedAt: timestamp,
  providers: z.array(providerIdentity.extend({
    accountLabel: z.string().optional(),
    models: z.array(model),
    usage,
  })),
  refresh: RefreshOutputSchema,
  sources: SourcesOutputSchema,
});

function usageValue(limits?: z.infer<typeof usageLimits>): z.infer<typeof usage> {
  return limits === undefined
    ? { status: "missing", windows: [] }
    : {
      ...limits,
      status: limits.unavailable?.reason ?? (limits.windows.length === 0 ? "empty" : "reported"),
      windows: limits.windows.map((window) => ({ ...window, remainingPercent: 100 - window.usedPercent })),
    };
}

export function modelsUsage(providers: ProvidersConfig["providers"]): z.infer<typeof ModelsUsageOutputSchema>["providers"] {
  return providers.map(({ usageLimits, auth, ...provider }) => ({
    ...provider,
    ...(auth?.label === undefined ? {} : { accountLabel: sanitizeFailureText(auth.label, 2_000) }),
    usage: usageValue(usageLimits),
  }));
}

export function sourceUsage(sources: UsageSources): SourcesOutput {
  const ref = (value: string) => createHash("sha256").update(value).digest("hex").slice(0, 16);
  return { status: "reported", items: sources.map((source) => ({
    sourceRef: `src_${ref(source.id)}`,
    kind: source.kind,
    label: sanitizeFailureText(source.label, 2_000),
    checkedAt: source.checkedAt,
    ...(source.error === undefined ? {} : { error: sanitizeFailureText(source.error, 2_000) }),
    accounts: source.accounts.map((account) => ({
      accountRef: `acct_${ref(source.id + "\0" + account.id)}`,
      driver: account.driver,
      ...(account.plan === undefined ? {} : { plan: sanitizeFailureText(account.plan, 2_000) }),
      usage: usageValue(account.usageLimits),
    })),
  })) };
}

export function usageLimitsCollector() {
  let providers: ProvidersConfig["providers"] | undefined;
  let sources: UsageSources | undefined;
  return {
    chunk(values: unknown[]): Omit<UsageLimitsRead, "refresh"> | undefined {
      for (const value of values) {
        const event = z.object({ type: z.string(), version: z.literal(1) }).parse(value);
        if (event.type === "snapshot") {
          providers = z.object({ config: ProvidersConfigSchema }).parse(value).config.providers;
        } else if (event.type === "providerStatuses") {
          const updated = z.object({ payload: ProvidersConfigSchema }).parse(value).payload.providers;
          if (providers !== undefined) providers = updated;
        } else if (event.type === "usageLimitSourcesUpdated") {
          const updated = z.object({ payload: UsageSourcesSchema }).parse(value).payload.sources;
          sources ??= updated;
        }
      }
      return providers !== undefined && sources !== undefined ? { providers, sources: sourceUsage(sources) } : undefined;
    },
    interrupted(reason: "timeout" | "streamEnded" | "disconnected"): Omit<UsageLimitsRead, "refresh"> {
      if (providers === undefined) throw new Error(`T3 RPC subscribeServerConfig ${reason} without a provider snapshot.`);
      return { providers, sources: sources === undefined ? { status: "unavailable", reason } : sourceUsage(sources) };
    },
  };
}
