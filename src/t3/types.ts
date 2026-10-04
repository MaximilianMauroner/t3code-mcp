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

export const ProviderRetrySchema = z.object({
  attempt: z.number().int().positive(),
  maxAttempts: z.number().int().positive().nullable(),
  retryDelayMs: z.number().int().nonnegative().nullable(),
});

export const ProviderFailureSchema = z.object({
  class: z.string(),
  message: z.string(),
  code: z.string().nullable(),
  retryable: z.boolean().nullable().optional(),
  resetAt: z.string().nullable().optional(),
});

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

// Private observation provenance. Never included in MCP output schemas.
export const FailureEvidenceOrderSchema = z.object({
  protocolVersion: z.union([z.literal(1), z.literal(2)]),
  scope: z.enum(["full", "shell"]),
  snapshotSequence: z.number().int().nonnegative(),
  readStartedAt: z.number().int().nonnegative().optional(),
  protocolStartedAt: z.number().int().nonnegative().optional(),
  // Private digest of the unsanitized provider reason. It prevents a
  // redaction collision from making two V2 shell reasons look identical.
  failureIdentity: z.string().regex(/^[a-f0-9]{64}$/u).optional(),
  updatedAt: z.iso.datetime({ offset: true }).optional(),
  runIdentity: z.object({ provider: z.string().nullable(), model: z.string() }).optional(),
  item: z.object({
    updatedAt: z.iso.datetime({ offset: true }),
    ordinal: z.number().int().nonnegative(),
    id: NonEmptyString,
  }).optional(),
});
export type FailureEvidenceOrder = z.infer<typeof FailureEvidenceOrderSchema>;

export const ThreadShellSchema = z
  .object({
    evidenceOrder: FailureEvidenceOrderSchema.optional(),
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
      providerName: z.string().nullable().optional(),
      providerInstanceId: z.string().optional(),
      activeTurnId: z.string().nullable().optional(),
      lastError: z.string().nullable().optional(),
      lastErrorClass: z.string().nullable().optional(),
      failureCode: z.string().nullable().optional(),
      failureCategory: z.string().nullable().optional(),
      resetAt: z.string().nullable().optional(),
      retryAfter: z.union([z.string(), z.number()]).nullable().optional(),
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
  // Internal V2 normalization retains authoritative failures for historical runs.
  turnFailures: z.array(z.object({
    order: FailureEvidenceOrderSchema.optional(),
    turnId: NonEmptyString,
    provider: NonEmptyString,
    modelSelection: ModelSelectionSchema,
    failure: ProviderFailureSchema.nullable(),
    retry: ProviderRetrySchema.optional(),
  })).optional(),
  turnRecoveries: z.array(z.object({
    turnId: NonEmptyString,
    state: z.enum(["completed", "interrupted"]),
    order: FailureEvidenceOrderSchema,
  })).optional(),
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
    orchestrationProtocolVersion: z.number().int().positive().optional(),
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
