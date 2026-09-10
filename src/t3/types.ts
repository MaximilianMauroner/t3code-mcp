import { z } from "zod";

const NonEmptyString = z.string().min(1);

export const ModelSelectionSchema = z
  .object({
    instanceId: NonEmptyString.optional(),
    provider: NonEmptyString.optional(),
    model: NonEmptyString,
    options: z.unknown().optional(),
  })
  .catchall(z.unknown());
export type ModelSelection = z.infer<typeof ModelSelectionSchema>;

export const LatestTurnSchema = z
  .object({
    turnId: NonEmptyString,
    state: z.enum(["running", "interrupted", "completed", "error"]),
    requestedAt: NonEmptyString,
    startedAt: NonEmptyString.nullable().optional(),
    completedAt: NonEmptyString.nullable().optional(),
    assistantMessageId: NonEmptyString.nullable().optional(),
  })
  .catchall(z.unknown());
export type LatestTurn = z.infer<typeof LatestTurnSchema>;

export const ProjectSchema = z
  .object({
    id: NonEmptyString,
    title: NonEmptyString,
    workspaceRoot: NonEmptyString,
    defaultModelSelection: ModelSelectionSchema.nullable().optional(),
    createdAt: NonEmptyString.optional(),
    updatedAt: NonEmptyString.optional(),
    deletedAt: NonEmptyString.nullable().optional(),
  })
  .catchall(z.unknown());
export type Project = z.infer<typeof ProjectSchema>;

export const ThreadShellSchema = z
  .object({
    id: NonEmptyString,
    projectId: NonEmptyString,
    title: NonEmptyString,
    modelSelection: ModelSelectionSchema,
    runtimeMode: z.enum(["approval-required", "auto-accept-edits", "auto", "full-access"]),
    interactionMode: z.enum(["default", "plan"]).optional(),
    branch: NonEmptyString.nullable().optional(),
    worktreePath: NonEmptyString.nullable().optional(),
    latestTurn: LatestTurnSchema.nullable().optional(),
    archivedAt: NonEmptyString.nullable().optional(),
    createdAt: NonEmptyString.optional(),
    updatedAt: NonEmptyString.optional(),
    session: z.object({
      status: z.string(),
      updatedAt: z.string().optional(),
    }).catchall(z.unknown()).nullable().optional(),
    settledOverride: z.enum(["settled", "active"]).nullable().optional(),
    settledAt: z.string().nullable().optional(),
    snoozedUntil: z.string().nullable().optional(),
    snoozedAt: z.string().nullable().optional(),
    pinnedAt: z.string().nullable().optional(),
    latestUserMessageAt: z.string().nullable().optional(),
    hasActionableProposedPlan: z.boolean().optional(),
    backgroundLiveness: z.enum(["working", "monitoring"]).nullable().optional(),
    hasPendingApprovals: z.boolean().optional(),
    hasPendingUserInput: z.boolean().optional(),
  })
  .catchall(z.unknown());
export type ThreadShell = z.infer<typeof ThreadShellSchema>;

export const MessageSchema = z
  .object({
    id: NonEmptyString,
    role: z.enum(["user", "assistant", "system"]),
    text: z.string(),
    turnId: NonEmptyString.nullable().optional(),
    streaming: z.boolean().optional(),
    createdAt: NonEmptyString.optional(),
    updatedAt: NonEmptyString.optional(),
    attachments: z.unknown().optional(),
  })
  .catchall(z.unknown());
export type Message = z.infer<typeof MessageSchema>;

export const ThreadSchema = ThreadShellSchema.extend({
  messages: z.array(MessageSchema).default([]),
  activities: z.array(z.unknown()).default([]),
  checkpoints: z.array(z.unknown()).default([]),
  proposedPlans: z.array(z.unknown()).default([]),
}).catchall(z.unknown());
export type Thread = z.infer<typeof ThreadSchema>;

export const ShellSnapshotSchema = z
  .object({
    snapshotSequence: z.number().int().nonnegative(),
    projects: z.array(ProjectSchema),
    threads: z.array(ThreadShellSchema),
    updatedAt: NonEmptyString,
  })
  .catchall(z.unknown());
export type ShellSnapshot = z.infer<typeof ShellSnapshotSchema>;

export const ThreadSnapshotSchema = z
  .object({
    snapshotSequence: z.number().int().nonnegative(),
    thread: ThreadSchema,
  })
  .catchall(z.unknown());
export type ThreadSnapshot = z.infer<typeof ThreadSnapshotSchema>;

export const DescriptorSchema = z
  .object({
    environmentId: NonEmptyString,
    label: z.string(),
    platform: z.unknown().optional(),
    serverVersion: NonEmptyString,
    capabilities: z.record(z.string(), z.unknown()).default({}),
  })
  .catchall(z.unknown());
export type Descriptor = z.infer<typeof DescriptorSchema>;

export const AuthSessionSchema = z
  .object({
    authenticated: z.boolean(),
    scopes: z.array(NonEmptyString).optional(),
    sessionMethod: NonEmptyString.optional(),
    expiresAt: NonEmptyString.optional(),
  })
  .catchall(z.unknown());
export type AuthSession = z.infer<typeof AuthSessionSchema>;

export const DispatchResultSchema = z
  .object({ sequence: z.number().int().nonnegative() })
  .catchall(z.unknown());
export type DispatchResult = z.infer<typeof DispatchResultSchema>;
