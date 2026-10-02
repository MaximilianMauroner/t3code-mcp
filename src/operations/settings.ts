import { z } from "zod";
import type { ModelSelection, Thread } from "../t3/types.js";

const runtime = z.enum(["approval-required", "auto-accept-edits", "auto", "full-access"]);
const safeModel = z.object({ model: z.string(), instanceId: z.string().optional(), provider: z.string().optional() }).strict();
export const settingsReceiptSchema = z.object({
  requested: z.object({ modelSelection: safeModel.nullable(), runtimeMode: runtime.nullable() }).strict(),
  resolved: z.object({ modelSelection: safeModel, runtimeMode: runtime }).strict(),
  modelSource: z.enum(["explicit", "project_default", "thread_inherited"]),
  runtimeSource: z.enum(["explicit", "thread_inherited"]),
}).strict();
export type SettingsReceipt = z.infer<typeof settingsReceiptSchema>;
export interface SettingsObservation extends SettingsReceipt {
  readonly effective: (SettingsReceipt["resolved"] & { readonly observedAt: string }) | null;
  readonly state: "observed" | "unresolved";
  readonly matchesResolved: boolean | null;
}

/** Only known identity fields are persisted. Provider options can contain credentials. */
export function safeModelSelection(selection: ModelSelection): SettingsReceipt["resolved"]["modelSelection"] {
  return { model: selection.model,
    ...(selection.instanceId === undefined ? {} : { instanceId: selection.instanceId }),
    ...(selection.provider === undefined ? {} : { provider: selection.provider }) };
}
export function observeSettings(receipt: SettingsReceipt, thread?: Thread): SettingsObservation {
  const effective = thread ? { modelSelection: safeModelSelection(thread.modelSelection), runtimeMode: thread.runtimeMode, observedAt: new Date().toISOString() } : null;
  return { ...receipt, effective, state: effective ? "observed" : "unresolved",
    matchesResolved: effective === null ? null : JSON.stringify(effective.modelSelection) === JSON.stringify(receipt.resolved.modelSelection) && effective.runtimeMode === receipt.resolved.runtimeMode };
}
