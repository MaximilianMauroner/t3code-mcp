import { z } from "zod";

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
  providers: z.array(providerIdentity.extend({ models: z.array(model), usageLimits: usageLimits.optional() })),
});
export type ProvidersConfig = z.infer<typeof ProvidersConfigSchema>;

const usageOutput = usageLimits.omit({ windows: true }).extend({
  status: z.enum(["reported", "unsupported", "probeFailed", "empty"]),
  windows: z.array(window.extend({ remainingPercent: z.number().min(0).max(100) })),
});
export const ModelsUsageOutputSchema = z.object({
  environmentId: nonempty,
  observedAt: timestamp,
  providers: z.array(providerIdentity.extend({
    models: z.array(model),
    usage: z.union([
      z.object({ status: z.literal("missing"), windows: z.array(z.never()) }),
      usageOutput,
    ]),
  })),
});

export function modelsUsage(providers: ProvidersConfig["providers"]): z.infer<typeof ModelsUsageOutputSchema>["providers"] {
  return providers.map(({ usageLimits, ...provider }) => ({
    ...provider,
    usage: usageLimits === undefined
      ? { status: "missing" as const, windows: [] }
      : {
        ...usageLimits,
        status: usageLimits.unavailable?.reason ?? (usageLimits.windows.length === 0 ? "empty" as const : "reported" as const),
        windows: usageLimits.windows.map((window) => ({ ...window, remainingPercent: 100 - window.usedPercent })),
      },
  }));
}
