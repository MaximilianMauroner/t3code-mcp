import { z } from "zod";
import { ProjectSchema, ThreadSchema, ThreadShellSchema, type ThreadShell } from "./types.js";

const Id = z.string().min(1);
const Time = z.string().min(1);
const RunStatus = z.enum(["preparing", "queued", "starting", "running", "waiting", "completed", "interrupted", "failed", "cancelled", "rolled_back"]);
const ActiveStatuses = new Set(["preparing", "starting", "running", "waiting"]);
const Failure = z.object({ class: z.string(), message: z.string(), code: z.string().nullable(), resetAt: Time.nullable().optional() });
const PendingRequest = z.object({ id: Id, kind: z.string(), createdAt: Time });
const AppThread = ThreadShellSchema.omit({ latestTurn: true, session: true }).extend({
  providerInstanceId: Id,
  // T3's creation command requires a title, but shell titles can be empty.
  title: z.string(),
  createdAt: Time,
  updatedAt: Time,
}).passthrough();
const ShellThread = AppThread.extend({
  latestRunId: Id.nullable(),
  latestRunRequestedAt: Time.nullable().optional(),
  latestRunStartedAt: Time.nullable().optional(),
  latestRunCompletedAt: Time.nullable().optional(),
  activeRunId: Id.nullable(),
  activityRunStartedAt: Time.nullable().optional(),
  activityRunStatus: RunStatus.nullable().optional(),
  status: z.union([RunStatus, z.literal("idle")]),
  lastError: z.string().nullable().optional(),
  lastErrorClass: z.string().nullable().optional(),
  usageLimitResetAt: Time.nullable().optional(),
  pendingRuntimeRequest: PendingRequest.nullable(),
  pendingBackgroundTasks: z.array(z.object({ kind: z.string().optional() }).passthrough()).optional(),
});
export const V2ShellSchema = z.object({
  schemaVersion: z.number().int().positive(),
  snapshotSequence: z.number().int().nonnegative(),
  projects: z.array(ProjectSchema),
  threads: z.array(ShellThread),
  archivedThreads: z.array(ShellThread),
});
export const V2ArchivedShellSchema = V2ShellSchema.omit({ projects: true, archivedThreads: true });
const Run = z.object({
  id: Id, ordinal: z.number().int().positive(), status: RunStatus,
  userMessageId: Id, rootNodeId: Id.nullable(),
  requestedAt: Time, startedAt: Time.nullable(), completedAt: Time.nullable(),
});
const Message = z.object({
  id: Id, runId: Id.nullable(), nodeId: Id.nullable(), role: z.enum(["user", "assistant", "system"]),
  text: z.string(), streaming: z.boolean(), createdAt: Time, updatedAt: Time,
  attachments: z.array(z.unknown()),
}).passthrough();
const TurnItem = z.object({
  id: Id, type: z.string(), runId: Id.nullable(), nodeId: Id.nullable(), updatedAt: Time,
  requestId: Id.optional(), questions: z.array(z.unknown()).optional(), prompt: z.string().optional(),
  failure: Failure.optional(), title: z.string().nullable(),
}).passthrough();
const RuntimeRequest = PendingRequest.extend({
  nodeId: Id,
  status: z.enum(["pending", "resolved", "expired", "cancelled"]),
  responseCapability: z.object({ type: z.enum(["live", "message", "not_resumable"]) }).passthrough(),
});
export const V2ThreadSchema = z.object({
  snapshotSequence: z.number().int().nonnegative(),
  projection: z.object({
    thread: AppThread, runs: z.array(Run), messages: z.array(Message),
    runtimeRequests: z.array(RuntimeRequest), turnItems: z.array(TurnItem),
    plans: z.array(z.object({ id: Id, runId: Id.nullable(), kind: z.string(), status: z.string(), markdown: z.string().optional() }).passthrough()),
    checkpoints: z.array(z.unknown()), updatedAt: Time,
  }).passthrough(),
});

function failureCategory(value: string | null | undefined): string {
  if (value === "usage_limit") return "quota";
  if (value === "provider_error") return "provider_internal";
  return "unknown";
}

function turnState(status: string): "running" | "completed" | "interrupted" | "error" {
  if (ActiveStatuses.has(status) || status === "queued") return "running";
  if (status === "failed") return "error";
  if (status === "completed") return "completed";
  return "interrupted";
}

function sessionStatus(status: string): string {
  if (status === "preparing" || status === "starting") return "starting";
  if (status === "running" || status === "waiting" || status === "queued") return "running";
  if (status === "failed") return "error";
  return "ready";
}

export function normalizeV2ShellThread(thread: z.infer<typeof ShellThread>): ThreadShell {
  const request = thread.pendingRuntimeRequest;
  const runId = thread.activeRunId ?? thread.latestRunId;
  const status = thread.activityRunStatus ?? thread.status;
  const activeIsLatest = thread.activeRunId === null || thread.activeRunId === thread.latestRunId;
  return ThreadShellSchema.parse({
    ...thread,
    title: thread.title || "Untitled",
    orchestrationProtocolVersion: 2,
    latestTurn: runId ? {
      turnId: runId, state: turnState(status),
      requestedAt: (activeIsLatest ? thread.latestRunRequestedAt : thread.activityRunStartedAt) ?? thread.createdAt,
      startedAt: (activeIsLatest ? thread.latestRunStartedAt : thread.activityRunStartedAt) ?? null,
      completedAt: thread.activeRunId ? null : thread.latestRunCompletedAt ?? null,
    } : null,
    session: {
      status: sessionStatus(status), providerInstanceId: thread.providerInstanceId,
      activeTurnId: runId, lastError: thread.lastError ?? null,
      failureCategory: failureCategory(thread.lastErrorClass),
      resetAt: thread.usageLimitResetAt ?? null, updatedAt: thread.updatedAt,
    },
    hasPendingApprovals: request !== null && request.kind !== "user_input",
    hasPendingUserInput: request?.kind === "user_input",
    backgroundLiveness: thread.pendingBackgroundTasks?.length
      ? thread.pendingBackgroundTasks.every((task) => task.kind === "monitor") ? "monitoring" : "working"
      : null,
  });
}

export function normalizeV2Thread(snapshot: z.infer<typeof V2ThreadSchema>) {
  const projection = snapshot.projection;
  // Queue entries can have larger ordinals than the run that is executing.
  const runs = [...projection.runs].sort((a, b) => a.ordinal - b.ordinal);
  const active = runs.find((run) => ActiveStatuses.has(run.status));
  const latest = active ?? runs.filter((run) => run.status !== "queued").at(-1) ?? runs.at(-1);
  const rootNodes = new Set(runs.map((run) => run.rootNodeId));
  const messages = projection.messages.filter((message) => message.nodeId === null || rootNodes.has(message.nodeId));
  const pending = projection.runtimeRequests.filter((request) => request.status === "pending" && request.responseCapability.type !== "not_resumable");
  const failure = projection.turnItems.filter((item) => item.runId === latest?.id && item.type === "error" && item.failure).at(-1)?.failure;
  const shell = normalizeV2ShellThread({
    ...projection.thread,
    latestRunId: latest?.id ?? null,
    latestRunRequestedAt: latest?.requestedAt ?? null,
    latestRunStartedAt: latest?.startedAt ?? null,
    latestRunCompletedAt: latest?.completedAt ?? null,
    activeRunId: active?.id ?? null,
    status: latest?.status ?? "idle",
    pendingRuntimeRequest: pending[0] ?? null,
    lastError: failure?.message ?? null,
    lastErrorClass: failure?.class ?? null,
    usageLimitResetAt: failure?.resetAt ?? null,
    latestUserMessageAt: messages.filter((message) => message.role === "user").at(-1)?.createdAt ?? null,
    hasActionableProposedPlan: projection.plans.some((plan) => plan.kind === "proposed_plan" && (plan.status === "draft" || plan.status === "active")),
  });
  return {
    snapshotSequence: snapshot.snapshotSequence,
    thread: ThreadSchema.parse({
      ...shell,
      latestTurn: shell.latestTurn ? {
        ...shell.latestTurn,
        assistantMessageId: messages.filter((message) => message.runId === latest?.id && message.role === "assistant").at(-1)?.id ?? null,
      } : null,
      hasPendingApprovals: pending.some((request) => request.kind !== "user_input"),
      hasPendingUserInput: pending.some((request) => request.kind === "user_input"),
      session: { ...shell.session, failureCode: failure?.code ?? null },
      messages: messages.map((message) => ({ ...message, turnId: message.runId })),
      // Expose only actionable requests, rather than historical activity guesses.
      activities: [...projection.turnItems.filter((item) => item.requestId === undefined).map((item) => ({
        kind: item.type, tone: item.type === "error" ? "error" : "info",
        summary: item.title ?? item.type, payload: item,
      })), ...pending.map((request) => {
        const item = projection.turnItems.find((item) => item.requestId === request.id);
        return {
          kind: request.kind === "user_input" ? "user_input" : "approval",
          tone: request.kind === "user_input" ? "info" : "approval",
          summary: item?.prompt ?? item?.title ?? request.kind,
          payload: { ...item, requestId: request.id, responseCapability: request.responseCapability },
        };
      })],
      checkpoints: projection.checkpoints,
      proposedPlans: projection.plans.filter((plan) => plan.kind === "proposed_plan").map((plan) => ({
        ...plan, turnId: plan.runId, planMarkdown: plan.markdown,
        implementedAt: plan.status === "completed" ? projection.updatedAt : null,
      })),
    }),
  };
}
