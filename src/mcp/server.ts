import { randomUUID } from "node:crypto";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import type { CallToolResult } from "@modelcontextprotocol/sdk/types.js";
import { z } from "zod";
import { GATEWAY_VERSION } from "../contract.js";
import { summarizeForAudit, type AuditLog } from "../operations/audit-log.js";
import {
  GatewayError,
  T3Gateway,
  type PendingActionRespondInput,
  type GitDiffInput,
  type GitCompareInput,
  type ProjectCreateInput,
  type TaskStartInput,
  type ThreadSendInput,
  type ThreadStartInput,
} from "../gateway.js";

const modelSelection = z
  .object({
    model: z.string().min(1),
    instanceId: z.string().min(1).optional(),
    provider: z.string().min(1).optional(),
    options: z.unknown().optional(),
  })
  .passthrough();

const idempotencyKey = z.string().trim().min(1).max(200);
const query = z.string().trim().min(1).max(200).optional();
const cursor = z.string().regex(/^\d+$/).optional();
const limit = z.number().int().min(1).max(100).default(50);
const auditFilter = z.string().trim().min(1).max(200).optional();
const auditSource = z.enum(["transport", "mcp", "t3", "git", "journal", "system"]);

const modelSelectionOutput = z
  .object({
    model: z.string(),
    instanceId: z.string().optional(),
    provider: z.string().optional(),
    options: z.unknown().optional(),
  })
  .passthrough();

const environmentIdentityOutput = z
  .object({
    environmentId: z.string(),
    label: z.string(),
    serverVersion: z.string(),
  })
  .passthrough();

const latestTurnOutput = z
  .object({
    turnId: z.string(),
    state: z.enum(["running", "interrupted", "completed", "error"]),
    requestedAt: z.string(),
    startedAt: z.string().nullable().optional(),
    completedAt: z.string().nullable().optional(),
    assistantMessageId: z.string().nullable().optional(),
  })
  .passthrough();

const messageOutput = z
  .object({
    id: z.string(),
    role: z.enum(["user", "assistant", "system"]),
    text: z.string(),
    turnId: z.string().nullable().optional(),
    streaming: z.boolean().optional(),
    createdAt: z.string().optional(),
    updatedAt: z.string().optional(),
    attachments: z.unknown().optional(),
  })
  .passthrough();

const projectSummaryOutput = z
  .object({
    id: z.string(),
    title: z.string(),
    workspaceRoot: z.string(),
    defaultModelSelection: modelSelectionOutput.nullable(),
    createdAt: z.string().nullable(),
    updatedAt: z.string().nullable(),
  })
  .passthrough();

const observedTargetOutput = z
  .object({
    environmentId: z.string(),
    threadId: z.string(),
    turnId: z.string().nullable(),
    observedAt: z.string(),
  })
  .passthrough();

const failureOutput = z.object({
  category: z.enum(["quota", "rate_limit", "auth_billing", "provider_internal", "unknown"]),
  code: z.string().nullable(),
  message: z.string(),
  provider: z.string().nullable(),
  model: z.string(),
  turnId: z.string().nullable(),
  resetAt: z.string().nullable(),
  retryAfter: z.string().nullable(),
  source: z.literal("t3_session"),
}).passthrough();

const threadSummaryOutput = z
  .object({
    id: z.string(),
    projectId: z.string(),
    projectTitle: z.string().nullable(),
    title: z.string(),
    modelSelection: modelSelectionOutput,
    runtimeMode: z.enum(["approval-required", "auto-accept-edits", "auto", "full-access"]),
    interactionMode: z.enum(["default", "plan"]),
    branch: z.string().nullable(),
    worktreePath: z.string().nullable(),
    latestTurn: latestTurnOutput.nullable(),
    sessionStatus: z.string().nullable(),
    sessionUpdatedAt: z.string().nullable(),
    status: z.enum(["open", "snoozed", "settled", "archived"]),
    statusReason: z.string(),
    activity: z.enum(["running", "starting", "awaiting_approval", "awaiting_input", "failed", "idle"]),
    isRunning: z.boolean(),
    hasConflictingSignals: z.boolean(),
    quality: z.enum(["fresh", "stale", "incomplete", "inconsistent"]),
    warning: z.string().nullable(),
    observedTurnId: z.string().nullable(),
    observedAt: z.string(),
    observedTarget: observedTargetOutput,
    settledOverride: z.enum(["settled", "active"]).nullable(),
    settledAt: z.string().nullable(),
    snoozedUntil: z.string().nullable(),
    snoozedAt: z.string().nullable(),
    latestUserMessageAt: z.string().nullable(),
    pinnedAt: z.string().nullable(),
    hasActionableProposedPlan: z.boolean(),
    backgroundLiveness: z.enum(["working", "monitoring"]).nullable(),
    archivedAt: z.string().nullable(),
    createdAt: z.string().nullable(),
    updatedAt: z.string().nullable(),
    hasPendingApprovals: z.boolean(),
    hasPendingUserInput: z.boolean(),
    failure: failureOutput.nullable(),
  })
  .passthrough();

const overviewHighlightOutput = threadSummaryOutput.extend({
  latestResponseExcerpt: z.string().nullable(),
});

const threadDetailOutput = threadSummaryOutput.extend({
  latestResponse: messageOutput.nullable(),
  messageCount: z.number(),
  activityCount: z.number(),
  checkpointCount: z.number(),
  proposedPlanCount: z.number(),
});

const pendingActionOutput = z
  .object({
    kind: z.enum(["approval", "user_input", "unknown"]),
    requestId: z.string().nullable(),
    summary: z.string(),
    payload: z.unknown(),
  })
  .passthrough();

const mutationResultShape = {
  environmentId: z.string(),
  operationId: z.string(),
  commandId: z.string(),
  status: z.enum(["accepted", "uncertain", "rejected"]),
  t3Sequence: z.number().nullable().optional(),
  reason: z.string().optional(),
};

const disabledOperationOutput = z
  .object({
    operation: z.string(),
    reasonCode: z.enum(["gateway_read_only", "t3_scope_required"]),
    reason: z.string(),
  })
  .passthrough();

const connectionStatusOutputSchema = {
  environment: environmentIdentityOutput.nullable(),
  connectionStatus: z.enum(["connected", "disconnected"]),
  stateFreshness: z.enum(["fresh", "stale", "unknown"]),
  lastObservedAt: z.string().nullable(),
  observedAt: z.string(),
  gatewayVersion: z.string(),
  gatewayCommit: z.string(),
  toolSchemaFingerprint: z.string(),
  effectiveAccessMode: z.enum(["read-only", "read-write"]),
  callableOperations: z.array(z.string()),
  disabledOperations: z.array(disabledOperationOutput),
  supportedCapabilities: z.array(z.string()),
  permittedOperations: z.array(z.string()),
  t3Scopes: z.array(z.string()),
  upstreamScopes: z.array(z.string()),
  gatewayOperations: z.array(z.string()),
  sessionExpiresAt: z.string().nullable(),
  error: z.string().optional(),
};

const auditEventOutput = z
  .object({
    version: z.literal(1),
    eventId: z.string(),
    timestamp: z.string(),
    processId: z.number().int(),
    source: auditSource,
    event: z.string(),
    correlationId: z.string().optional(),
    operation: z.string().optional(),
    outcome: z.string().optional(),
    durationMs: z.number().int().nonnegative().optional(),
    details: z.record(z.string(), z.unknown()).optional(),
  })
  .passthrough();

const auditLogOutputSchema = {
  environmentId: z.string(),
  observedAt: z.string(),
  auditError: z.string().nullable(),
  page: z
    .object({
      items: z.array(auditEventOutput),
      nextCursor: z.string().nullable(),
      hasMore: z.boolean(),
      total: z.number().int().nonnegative(),
      invalidLines: z.number().int().nonnegative(),
    })
    .passthrough(),
};

const projectsListOutputSchema = {
  environmentId: z.string(),
  page: z
    .object({
      items: z.array(projectSummaryOutput),
      nextCursor: z.string().nullable(),
      hasMore: z.boolean(),
      total: z.number(),
    })
    .passthrough(),
};

const gitWorkspaceOutput = z.object({
  environmentId: z.string(),
  projectId: z.string(),
  projectTitle: z.string(),
  projectWorkspaceRoot: z.string(),
  threadId: z.string().nullable(),
  threadBranch: z.string().nullable(),
  threadWorktreePath: z.string().nullable(),
  source: z.enum(["project_workspace", "thread_worktree"]),
  selectedPath: z.string(),
  resolvedPath: z.string(),
  repositoryRoot: z.string(),
  gitDirectory: z.string(),
  gitCommonDirectory: z.string(),
});

const gitChangeOutput = z.object({
  path: z.string(),
  originalPath: z.string().optional(),
  kind: z.enum(["ordinary", "rename_or_copy"]),
  indexStatus: z.string(),
  worktreeStatus: z.string(),
  score: z.string().optional(),
  submodule: z.string(),
});

const gitCategoryOutput = <T extends z.ZodType>(item: T) => z.object({
  count: z.number().int().nonnegative(),
  items: z.array(item),
  truncated: z.boolean(),
});

const gitStatusOutputSchema = {
  environmentId: z.string(),
  observedAt: z.string(),
  workspace: gitWorkspaceOutput,
  branch: z.object({
    name: z.string().nullable(),
    detached: z.boolean(),
    unborn: z.boolean(),
    headCommit: z.string().nullable(),
    upstream: z.string().nullable(),
    ahead: z.number().int().nonnegative().nullable(),
    behind: z.number().int().nonnegative().nullable(),
  }),
  staged: gitCategoryOutput(gitChangeOutput),
  unstaged: gitCategoryOutput(gitChangeOutput),
  untracked: gitCategoryOutput(z.object({ path: z.string() })),
  conflicts: gitCategoryOutput(z.object({
    path: z.string(),
    indexStatus: z.string(),
    worktreeStatus: z.string(),
    submodule: z.string(),
  })),
  clean: z.boolean(),
  truncation: z.object({
    truncated: z.boolean(),
    itemLimitPerCategory: z.number().int().positive(),
    omittedItems: z.object({
      staged: z.number().int().nonnegative(),
      unstaged: z.number().int().nonnegative(),
      untracked: z.number().int().nonnegative(),
      conflicts: z.number().int().nonnegative(),
    }),
  }),
};

const gitDiffOutputSchema = {
  environmentId: z.string(),
  observedAt: z.string(),
  workspace: gitWorkspaceOutput,
  mode: z.enum(["unstaged", "staged"]),
  comparison: z.object({
    base: z.enum(["index", "HEAD"]),
    target: z.enum(["working_tree", "index"]),
  }),
  paths: z.array(z.string()),
  patch: z.string(),
  truncation: z.object({
    truncated: z.boolean(),
    reason: z.literal("byte_limit").nullable(),
    maxBytes: z.number().int().positive(),
    capturedBytes: z.number().int().nonnegative(),
    totalBytes: z.number().int().nonnegative(),
    omittedBytes: z.number().int().nonnegative(),
  }),
};

const gitCompareOutputSchema = {
  environmentId: z.string(),
  observedAt: z.string(),
  workspace: gitWorkspaceOutput,
  comparison: z.object({
    requestedBase: z.string(),
    requestedHead: z.string(),
    baseCommit: z.string(),
    headCommit: z.string(),
  }),
  paths: z.array(z.string()),
  patch: z.string(),
  attribution: z.object({
    quality: z.enum(["clean_baseline", "working_tree_dirty"]),
    detail: z.string(),
  }),
  truncation: z.object({
    truncated: z.boolean(),
    reason: z.enum(["byte_limit"]).nullable(),
    maxBytes: z.number(),
    capturedBytes: z.number(),
    totalBytes: z.number(),
    omittedBytes: z.number(),
  }),
};

const projectCreateOutputSchema = {
  ...mutationResultShape,
  projectId: z.string(),
  workspaceRoot: z.string(),
};

const threadsListOutputSchema = {
  environmentId: z.string(),
  observedAt: z.string(),
  page: z
    .object({
      items: z.array(threadSummaryOutput),
      nextCursor: z.string().nullable(),
      hasMore: z.boolean(),
      total: z.number(),
    })
    .passthrough(),
  resolutionHint: z.string().nullable(),
};

const threadsOverviewOutputSchema = {
  environmentId: z.string(),
  observedAt: z.string(),
  total: z.number(),
  counts: z
    .object({
      open: z.number(),
      snoozed: z.number(),
      settled: z.number(),
      archived: z.number(),
    })
    .passthrough(),
  executionCounts: z
    .object({
      running: z.number(),
      starting: z.number(),
      awaiting_approval: z.number(),
      awaiting_input: z.number(),
      failed: z.number(),
      idle: z.number(),
    })
    .passthrough(),
  runningCount: z.number(),
  needsAttentionCount: z.number(),
  running: z.array(threadSummaryOutput),
  highlights: z.array(overviewHighlightOutput),
};

const threadGetOutputSchema = {
  environmentId: z.string(),
  thread: threadDetailOutput,
};

const threadMessagesOutputSchema = {
  environmentId: z.string(),
  page: z
    .object({
      threadId: z.string(),
      messages: z.array(messageOutput),
      nextCursor: z.string().nullable(),
      hasMore: z.boolean(),
      total: z.number(),
      truncated: z.boolean(),
    })
    .passthrough(),
};

const threadSendOutputSchema = {
  ...mutationResultShape,
  projectId: z.string(),
  threadId: z.string(),
  runId: z.string(),
  messageId: z.string(),
  t3TurnId: z.string().nullable(),
  providerTurnId: z.string().nullable(),
  nextAction: z.string(),
};

const runResultOutputSchema = {
  environmentId: z.string(),
  operationId: z.string(),
  projectId: z.string().nullable(),
  threadId: z.string().nullable(),
  runId: z.string(),
  t3TurnId: z.string().nullable(),
  runStatus: z.enum([
    "accepted",
    "running",
    "completed",
    "awaiting_approval",
    "awaiting_input",
    "failed",
    "interrupted",
    "unknown",
  ]),
  providerTurnId: z.string().nullable(),
  connectionStatus: z.enum(["connected", "disconnected"]),
  stateFreshness: z.enum(["fresh", "stale", "unknown"]),
  lastObservedAt: z.string().nullable(),
  observedAt: z.string(),
  threadQuality: z.enum(["fresh", "stale", "incomplete", "inconsistent"]).nullable(),
  threadWarning: z.string().nullable(),
  latestResponse: messageOutput.nullable(),
  pendingActions: z
    .object({
      approvals: z.boolean(),
      userInput: z.boolean(),
    })
    .passthrough(),
  timedOut: z.boolean().optional(),
  error: z.string().optional(),
  failure: failureOutput.nullable(),
};

const taskStageOutput = z.enum([
  "prepared",
  "thread_create_uncertain",
  "thread_created",
  "dispatch_rejected",
  "dispatch_uncertain",
  "run_accepted",
  "rejected",
]);

const taskSummaryOutput = z.object({
  taskRef: z.string(),
  projectId: z.string(),
  title: z.string(),
  runtimeMode: z.enum(["approval-required", "auto-accept-edits", "auto", "full-access"]),
  stage: taskStageOutput,
  threadId: z.string().nullable(),
  runId: z.string().nullable(),
  messageId: z.string().nullable(),
  threadOperationId: z.string().nullable(),
  runOperationId: z.string().nullable(),
  baselineRevision: z.string().nullable(),
  baselineAttribution: z.enum(["clean", "dirty", "unavailable"]),
  createdAt: z.string(),
  updatedAt: z.string(),
  nextAction: z.string(),
  lastError: z.string().nullable(),
}).passthrough();

const taskDetailOutput = taskSummaryOutput.extend({
  thread: threadDetailOutput.nullable(),
  run: z.object(runResultOutputSchema).passthrough().nullable(),
});

const taskGetOutputSchema = {
  environmentId: z.string(),
  observedAt: z.string(),
  task: taskDetailOutput,
};

const tasksListOutputSchema = {
  environmentId: z.string(),
  observedAt: z.string(),
  page: z.object({
    items: z.array(taskSummaryOutput),
    nextCursor: z.string().nullable(),
    hasMore: z.boolean(),
    total: z.number(),
  }).passthrough(),
};

const resultPackageOutputSchema = {
  environmentId: z.string(),
  observedAt: z.string(),
  task: taskDetailOutput,
  evidence: z.object({
    taskStateSource: z.enum(["t3_observed", "gateway_journal"]),
    git: z.object({
      source: z.literal("git_observed"),
      baselineRevision: z.string(),
      status: z.object(gitStatusOutputSchema).passthrough(),
      committed: z.object(gitCompareOutputSchema).passthrough(),
      staged: z.object(gitDiffOutputSchema).passthrough(),
      unstaged: z.object(gitDiffOutputSchema).passthrough(),
    }).passthrough().nullable(),
  }).passthrough(),
  limitations: z.array(z.string()),
};

const threadInterruptOutputSchema = {
  ...mutationResultShape,
  threadId: z.string(),
  expectedTurnId: z.string(),
  verification: z
    .object({
      observed: z.enum(["interrupted", "still_running", "target_changed", "not_running", "inconsistent", "unknown"]),
      observedTurnId: z.string().nullable(),
      observedAt: z.string(),
      detail: z.string(),
    })
    .nullable(),
};

const providersListOutputSchema = {
  environmentId: z.string(),
  observedAt: z.string(),
  defaultsByProject: z.record(z.string(), modelSelectionOutput.nullable()),
  options: z
    .array(
      z
        .object({
          instanceId: z.string().nullable(),
          provider: z.string().nullable(),
          model: z.string(),
          label: z.string(),
          projectsWithDefault: z.array(z.string()),
          threadsUsing: z.number(),
        })
        .passthrough(),
    ),
};

const pendingActionsListOutputSchema = {
  environmentId: z.string(),
  threadId: z.string(),
  hasPendingApprovals: z.boolean(),
  hasPendingUserInput: z.boolean(),
  actions: z.array(pendingActionOutput),
  detailsAvailable: z.boolean(),
};

const threadSnoozeOutputSchema = {
  ...mutationResultShape,
  threadId: z.string(),
  snoozedUntil: z.string(),
  preset: z.enum(["hour", "three-hours", "evening", "tomorrow", "next-week", "custom"]),
  wakeDescription: z.string(),
  note: z.string().optional(),
};

const threadSettleOutputSchema = {
  ...mutationResultShape,
  threadId: z.string(),
  settledOverride: z.enum(["settled", "active"]).nullable(),
  lifecycle: z.enum(["open", "snoozed", "settled", "archived"]).nullable(),
  note: z.string().optional(),
};

export function createMcpServer(gateway: T3Gateway): McpServer {
  const auditLog = gateway.audit;
  const server = new McpServer(
    { name: "t3-code-mcp", version: GATEWAY_VERSION },
    {
      instructions:
        "This gateway controls one configured remote T3 Code environment. T3 remains authoritative for projects, threads, messages, runs, and workspaces. Mutation tools return after T3 accepts command intent; poll a returned runId with t3_run_get or t3_run_wait. " +
        "Prefer t3_task_start for a new assignment so creation and initial dispatch are recoverable; use t3_tasks_list/t3_task_get after reconnect and t3_result_get for task-bound review evidence. " +
        "Name-based resolution: zero results need a broader retry, one exact candidate may be selected, multiple candidates need clarification with project/title/branch/activity. " +
        "Interruptions are non-atomic: T3 interrupts by provider session, so always carry observedTarget (environmentId/threadId/turnId/observedAt) and verify with t3_thread_get. " +
        "Usage is recorded in a local redacted audit trail; use t3_audit_log to inspect tool calls, upstream requests, Git commands, outcomes, and timings.",
    },
  );

  server.registerTool(
    "t3_connection_status",
    {
      title: "T3 connection status",
      description: "Inspect gateway build (version/commit/fingerprint), effective access mode with callable/disabled operations and reasons, upstream T3 scopes, and freshness. toolSchemaFingerprint changes when the tool set or output shapes change, which makes cached tool definitions stale.",
      inputSchema: {},
      outputSchema: connectionStatusOutputSchema,
      annotations: { readOnlyHint: true, openWorldHint: false },
    },
    async () => runTool(auditLog, "t3_connection_status", {}, () => gateway.connectionStatus()),
  );

  server.registerTool(
    "t3_audit_log",
    {
      title: "Review gateway usage audit log",
      description:
        "Read a bounded, filtered page of the gateway's local usage audit trail. It includes MCP calls, transport requests, upstream T3 requests, Git commands, and durable operation transitions with timings and outcomes. Prompts, message text, patches, answers, and credentials are represented by redacted metadata rather than their values.",
      inputSchema: {
        since: auditFilter,
        until: auditFilter,
        source: auditSource.optional(),
        event: auditFilter,
        operation: auditFilter,
        outcome: auditFilter,
        cursor,
        limit,
      },
      outputSchema: auditLogOutputSchema,
      annotations: { readOnlyHint: true, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_audit_log", args, () => gateway.auditQuery(args)),
  );

  server.registerTool(
    "t3_projects_list",
    {
      title: "List T3 projects",
      description: "Find projects by case-insensitive title, workspace path, or ID substring. Filtering happens before pagination.",
      inputSchema: { query, cursor, limit },
      outputSchema: projectsListOutputSchema,
      annotations: { readOnlyHint: true, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_projects_list", args, () => gateway.projectsList(args)),
  );

  server.registerTool(
    "t3_project_create",
    {
      title: "Register a T3 project",
      description:
        "Register a remote workspace as a T3 project. Set createWorkspaceRootIfMissing to request creation of a missing directory. This does not clone a repository. Requires orchestration control scope and an idempotencyKey.",
      inputSchema: {
        title: z.string().trim().min(1).max(200),
        workspaceRoot: z.string().trim().min(1).max(4096),
        createWorkspaceRootIfMissing: z.boolean().optional(),
        defaultModelSelection: modelSelection.nullable().optional(),
        idempotencyKey,
      },
      outputSchema: projectCreateOutputSchema,
      annotations: { readOnlyHint: false, destructiveHint: false, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_project_create", args, () => gateway.projectCreate(args satisfies ProjectCreateInput)),
  );

  server.registerTool(
    "t3_git_status",
    {
      title: "Inspect T3 workspace Git status",
      description:
        "Read structured Git branch/worktree identity and staged, unstaged, untracked, and conflict summaries from a T3-selected project workspace or thread worktree. projectId is always required; threadId selects its recorded worktree when present. Output item lists are bounded and report exact omitted counts.",
      inputSchema: {
        projectId: z.string().trim().min(1),
        threadId: z.string().trim().min(1).optional(),
        maxEntries: z.number().int().min(1).max(500).default(100),
      },
      outputSchema: gitStatusOutputSchema,
      annotations: { readOnlyHint: true, destructiveHint: false, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_git_status", args, () => gateway.gitStatus(args)),
  );

  server.registerTool(
    "t3_git_diff",
    {
      title: "Inspect T3 workspace Git diff",
      description:
        "Read a bounded patch from a T3-selected project workspace or thread worktree. mode=unstaged compares working tree to index; mode=staged compares index to HEAD. Optional paths are literal workspace-relative filters. Truncation metadata always reports captured, total, and omitted bytes.",
      inputSchema: {
        projectId: z.string().trim().min(1),
        threadId: z.string().trim().min(1).optional(),
        mode: z.enum(["unstaged", "staged"]).default("unstaged"),
        paths: z.array(z.string().min(1).max(4096)).max(100).default([]),
        maxBytes: z.number().int().min(1_024).max(1_000_000).default(100_000),
      },
      outputSchema: gitDiffOutputSchema,
      annotations: { readOnlyHint: true, destructiveHint: false, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_git_diff", args, () => gateway.gitDiff(args satisfies GitDiffInput)),
  );

  server.registerTool(
    "t3_git_compare",
    {
      title: "Compare T3 workspace Git revisions",
      description:
        "Read a bounded committed patch between two explicit revisions in a T3-selected project workspace or thread worktree. Revisions are resolved to commit IDs before diffing; ranges and option-like values are rejected. Dirty workspace attribution is reported separately because uncommitted changes are not part of this comparison.",
      inputSchema: {
        projectId: z.string().trim().min(1),
        threadId: z.string().trim().min(1).optional(),
        baseRevision: z.string().trim().min(1).max(200),
        headRevision: z.string().trim().min(1).max(200).default("HEAD"),
        paths: z.array(z.string().min(1).max(4096)).max(100).default([]),
        maxBytes: z.number().int().min(1_024).max(1_000_000).default(100_000),
      },
      outputSchema: gitCompareOutputSchema,
      annotations: { readOnlyHint: true, destructiveHint: false, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_git_compare", args, () => gateway.gitCompare(args satisfies GitCompareInput)),
  );

  server.registerTool(
    "t3_threads_list",
    {
      title: "List T3 threads",
      description: "Find threads by project and case-insensitive title, branch, or ID substring. status filters lifecycle (open/snoozed/settled/archived) only; activity/onlyRunning/sessionStatus filter execution, and running is not a lifecycle value. needsAttention filters pending/inconsistent/stale/failed. sort is deterministic (recent/title/status). Zero results return a retry hint; one exact candidate may be selected; multiple candidates need clarification. detail=full adds latest-response enrichment on the page and excludes full transcripts, which come from t3_thread_messages.",
      inputSchema: {
        projectId: z.string().trim().min(1).optional(),
        includeArchived: z.boolean().default(false),
        query,
        status: z.enum(["all", "open", "snoozed", "settled", "archived"]).optional(),
        onlyRunning: z.boolean().optional(),
        sessionStatus: z.string().trim().min(1).max(100).optional(),
        activity: z.enum(["running", "starting", "awaiting_approval", "awaiting_input", "failed", "idle"]).optional(),
        needsAttention: z.boolean().optional(),
        sort: z.enum(["recent", "title", "status"]).default("recent"),
        detail: z.enum(["summary", "full"]).default("summary"),
        cursor,
        limit,
      },
      outputSchema: threadsListOutputSchema,
      annotations: { readOnlyHint: true, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_threads_list", args, () => gateway.threadsList(args)),
  );

  server.registerTool(
    "t3_threads_overview",
    {
      title: "Summarize T3 threads",
      description: "One bounded call for 'what's running and does anything need me': lifecycle counts, execution counts, needsAttentionCount, running summaries capped by runningLimit, and up to five deterministic highlights (pending approval/input, inconsistent/stale, failed, running, recent) with project title and a 200-char response excerpt. All rows share one observedAt. Transcripts and full records need detail tools.",
      inputSchema: {
        projectId: z.string().trim().min(1).optional(),
        includeArchived: z.boolean().default(false),
        query,
        runningLimit: z.number().int().min(1).max(50).default(10),
      },
      outputSchema: threadsOverviewOutputSchema,
      annotations: { readOnlyHint: true, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_threads_overview", args, () => gateway.threadsOverview(args)),
  );

  server.registerTool(
    "t3_providers_list",
    {
      title: "List T3 provider options",
      description: "Read-only discovery of model selections observed in this environment: distinct instanceId/provider/model labels, per-project defaults, and thread usage. These are the selections valid for new threads in projects without a default model.",
      inputSchema: {},
      outputSchema: providersListOutputSchema,
      annotations: { readOnlyHint: true, openWorldHint: false },
    },
    async () => runTool(auditLog, "t3_providers_list", {}, () => gateway.providersList()),
  );

  server.registerTool(
    "t3_thread_create",
    {
      title: "Create and start a T3 thread",
      description:
        "Create a thread and start its required initial message as one MCP operation. The gateway safely sequences T3's ordinary thread.create and thread.turn.start commands and never sends if creation is uncertain. Set workspaceMode=worktree with branch and optional startFromOrigin to have the gateway create an explicit isolated Git worktree and attach it to the T3 thread before the turn starts. Use t3_thread_send only for follow-up messages on existing threads.",
      inputSchema: {
        projectId: z.string().trim().min(1),
        title: z.string().trim().min(1).max(200),
        message: z.string().min(1).max(120_000),
        modelSelection: modelSelection.optional(),
        runtimeMode: z.enum(["approval-required", "auto-accept-edits", "auto", "full-access"]),
        interactionMode: z.enum(["default", "plan"]).optional(),
        workspaceMode: z.enum(["local", "worktree"]).optional(),
        branch: z.string().trim().min(1).nullable().optional(),
        worktreePath: z.string().trim().min(1).nullable().optional(),
        startFromOrigin: z.boolean().optional(),
        idempotencyKey,
      },
      outputSchema: threadSendOutputSchema,
      annotations: { readOnlyHint: false, destructiveHint: false, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_thread_create", args, () => gateway.threadStart(args satisfies ThreadStartInput)),
  );

  server.registerTool(
    "t3_task_start",
    {
      title: "Start a recoverable T3 task",
      description:
        "Create one T3 thread and dispatch its required initial instruction as a recoverable two-command operation. Set workspaceMode=worktree with branch and optional startFromOrigin to have the gateway create an explicit isolated Git worktree from the selected local or origin base branch and attach it through ordinary HTTP thread creation. runtimeMode is required. Retries must reuse the same idempotencyKey and identical input; the gateway never stores the instruction text in its journal.",
      inputSchema: {
        projectId: z.string().trim().min(1),
        title: z.string().trim().min(1).max(200),
        instruction: z.string().min(1).max(120_000),
        runtimeMode: z.enum(["approval-required", "auto-accept-edits", "auto", "full-access"]),
        modelSelection: modelSelection.optional(),
        interactionMode: z.enum(["default", "plan"]).optional(),
        workspaceMode: z.enum(["local", "worktree"]).optional(),
        branch: z.string().trim().min(1).nullable().optional(),
        worktreePath: z.string().trim().min(1).nullable().optional(),
        startFromOrigin: z.boolean().optional(),
        idempotencyKey,
      },
      outputSchema: taskDetailOutput,
      annotations: { readOnlyHint: false, destructiveHint: false, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_task_start", args, () => gateway.taskStart(args satisfies TaskStartInput)),
  );

  server.registerTool(
    "t3_task_get",
    {
      title: "Get a recoverable T3 task",
      description:
        "Find a journaled composite task by taskRef and enrich it with fresh T3 thread/run state when available. Partial or uncertain stages remain explicit and never trigger a replacement dispatch.",
      inputSchema: { taskRef: z.string().trim().min(1) },
      outputSchema: taskGetOutputSchema,
      annotations: { readOnlyHint: true, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_task_get", args, () => gateway.taskGet(args.taskRef)),
  );

  server.registerTool(
    "t3_tasks_list",
    {
      title: "List recoverable T3 tasks",
      description:
        "List bounded task-start receipts stored by this gateway, optionally filtered by exact project or title/task/thread substring. Use t3_task_get for fresh thread and run detail.",
      inputSchema: {
        projectId: z.string().trim().min(1).optional(),
        query,
        cursor,
        limit,
      },
      outputSchema: tasksListOutputSchema,
      annotations: { readOnlyHint: true, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_tasks_list", args, () => gateway.tasksList(args)),
  );

  server.registerTool(
    "t3_result_get",
    {
      title: "Get an inspectable T3 task result",
      description:
        "Compose one task's fresh T3 state with Git-observed committed, staged, unstaged, untracked, and conflict evidence from its captured baseline. Agent responses remain labeled by an explicit limitation and unavailable evidence is reported rather than inferred.",
      inputSchema: {
        taskRef: z.string().trim().min(1),
        paths: z.array(z.string().min(1).max(4096)).max(100).default([]),
        maxBytes: z.number().int().min(1_024).max(1_000_000).default(100_000),
      },
      outputSchema: resultPackageOutputSchema,
      annotations: { readOnlyHint: true, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_result_get", args, () => gateway.resultGet(args)),
  );

  server.registerTool(
    "t3_thread_get",
    {
      title: "Get a T3 thread",
      description: "Return a compact thread summary, latest response, active run, pending flags, and workspace.",
      inputSchema: { threadId: z.string().trim().min(1) },
      outputSchema: threadGetOutputSchema,
      annotations: { readOnlyHint: true, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_thread_get", args, () => gateway.threadGet(args.threadId)),
  );

  server.registerTool(
    "t3_thread_messages",
    {
      title: "Read T3 thread messages",
      description: "Read paginated thread history. Message text is bounded by maxChars and reports truncation explicitly.",
      inputSchema: {
        threadId: z.string().trim().min(1),
        cursor,
        limit,
        maxChars: z.number().int().min(100).max(100_000).default(20_000),
      },
      outputSchema: threadMessagesOutputSchema,
      annotations: { readOnlyHint: true, openWorldHint: false },
    },
    async (args) =>
      runTool(auditLog, "t3_thread_messages", args, () => gateway.threadMessages(args.threadId, { cursor: args.cursor, limit: args.limit, maxChars: args.maxChars })),
  );

  server.registerTool(
    "t3_thread_send",
    {
      title: "Send a T3 thread message",
      description:
        "Send a new or follow-up message to an idle existing thread, start one T3 agent turn, and return after command intent is accepted. Busy threads return thread_busy with the active turn/session and valid next actions. Uncertain results carry a durable operation handle for status lookup. Each idempotencyKey maps to one input.",
      inputSchema: {
        threadId: z.string().trim().min(1),
        message: z.string().min(1).max(120_000),
        modelSelection: modelSelection.optional(),
        runtimeMode: z.enum(["approval-required", "auto-accept-edits", "auto", "full-access"]).optional(),
        interactionMode: z.enum(["default", "plan"]).optional(),
        titleSeed: z.string().trim().min(1).max(200).optional(),
        idempotencyKey,
      },
      outputSchema: threadSendOutputSchema,
      annotations: { readOnlyHint: false, destructiveHint: false, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_thread_send", args, () => gateway.threadSend(args satisfies ThreadSendInput)),
  );

  server.registerTool(
    "t3_run_get",
    {
      title: "Get T3 run status",
      description: "Reconcile a gateway run handle against T3 state. A disconnected result is not evidence that the run failed.",
      inputSchema: { runId: z.string().trim().min(1) },
      outputSchema: runResultOutputSchema,
      annotations: { readOnlyHint: true, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_run_get", args, () => gateway.runGet(args.runId)),
  );

  server.registerTool(
    "t3_run_wait",
    {
      title: "Wait for a T3 run change",
      description: "Poll for a relevant run change for a bounded interval. A timeout only means no change was observed; it does not cancel the run.",
      inputSchema: {
        runId: z.string().trim().min(1),
        timeoutSeconds: z.number().int().min(1).max(30).default(10),
      },
      outputSchema: runResultOutputSchema,
      annotations: { readOnlyHint: true, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_run_wait", args, () => gateway.runWait(args.runId, args.timeoutSeconds)),
  );

  server.registerTool(
    "t3_run_interrupt",
    {
      title: "Interrupt a T3 run",
      description: "Request interruption of one accepted T3 run. The interruption itself is journaled and is never replayed automatically.",
      inputSchema: { runId: z.string().trim().min(1), idempotencyKey },
      outputSchema: mutationResultShape,
      annotations: { readOnlyHint: false, destructiveHint: false, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_run_interrupt", args, () => gateway.runInterrupt(args)),
  );

  server.registerTool(
    "t3_thread_interrupt",
    {
      title: "Interrupt work in an existing T3 thread",
      description: "Request stopping the active turn in a thread, including one started outside this gateway. expectedTurnId is the observedTarget.turnId from a current thread read. Returns acceptance plus post-dispatch verification (interrupted/still_running/target_changed/not_running/inconsistent/unknown). Acceptance records dispatch and is not a confirmed stop. Stale observations return turn_changed/thread_not_running. T3 interrupts by provider session without an atomic turn condition.",
      inputSchema: {
        threadId: z.string().trim().min(1),
        expectedTurnId: z.string().trim().min(1),
        idempotencyKey,
      },
      outputSchema: threadInterruptOutputSchema,
      annotations: { readOnlyHint: false, destructiveHint: false, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_thread_interrupt", args, () => gateway.threadInterrupt(args)),
  );

  server.registerTool(
    "t3_pending_actions_list",
    {
      title: "List pending T3 actions",
      description: "Surface pending approval and user-input flags, plus stable action details when the T3 projection exposes them.",
      inputSchema: { threadId: z.string().trim().min(1) },
      outputSchema: pendingActionsListOutputSchema,
      annotations: { readOnlyHint: true, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_pending_actions_list", args, () => gateway.pendingActionsList(args.threadId)),
  );

  server.registerTool(
    "t3_pending_action_respond",
    {
      title: "Respond to a T3 pending action",
      description: "Respond to a T3 approval or user-input request using its stable requestId. The gateway forwards the supplied response and does not independently verify human confirmation.",
      inputSchema: {
        threadId: z.string().trim().min(1),
        requestId: z.string().trim().min(1),
        kind: z.enum(["approval", "user_input"]),
        decision: z.enum(["accept", "acceptForSession", "decline", "cancel"]).optional(),
        answers: z.record(z.string(), z.unknown()).optional(),
        idempotencyKey,
      },
      outputSchema: mutationResultShape,
      annotations: { readOnlyHint: false, destructiveHint: false, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_pending_action_respond", args, () => gateway.pendingActionRespond(args satisfies PendingActionRespondInput)),
  );

  server.registerTool(
    "t3_thread_archive",
    {
      title: "Archive a T3 thread",
      description: "Archive a thread without deleting its remote workspace files.",
      inputSchema: { threadId: z.string().trim().min(1), idempotencyKey },
      outputSchema: mutationResultShape,
      annotations: { readOnlyHint: false, destructiveHint: false, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_thread_archive", args, () => gateway.threadArchive(args)),
  );

  server.registerTool(
    "t3_thread_snooze",
    {
      title: "Snooze a T3 thread",
      description:
        "Hide a thread from the inbox until a wake time. Omit preset and snoozedUntil to snooze until this evening (before evening) or tomorrow morning. Preset options: hour, three-hours, evening, tomorrow, next-week. Alternatively supply an explicit future ISO snoozedUntil. Snooze never stops a running agent; pending approvals, user input, or queued turns are rejected.",
      inputSchema: {
        threadId: z.string().trim().min(1),
        preset: z.enum(["default", "hour", "three-hours", "evening", "tomorrow", "next-week"]).optional(),
        snoozedUntil: z.string().trim().min(1).optional(),
        idempotencyKey,
      },
      outputSchema: threadSnoozeOutputSchema,
      annotations: { readOnlyHint: false, destructiveHint: false, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_thread_snooze", args, () => gateway.threadSnooze(args)),
  );

  server.registerTool(
    "t3_thread_unsnooze",
    {
      title: "Wake a snoozed T3 thread",
      description: "Bring a snoozed thread back to the inbox immediately.",
      inputSchema: { threadId: z.string().trim().min(1), idempotencyKey },
      outputSchema: { ...mutationResultShape, threadId: z.string() },
      annotations: { readOnlyHint: false, destructiveHint: false, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_thread_unsnooze", args, () => gateway.threadUnsnooze(args)),
  );

  server.registerTool(
    "t3_thread_settle",
    {
      title: "Settle a T3 thread",
      description:
        "Mark a thread done. Clears snooze and pin. Blocked while the thread is running, has a pending approval, or has a queued turn start.",
      inputSchema: { threadId: z.string().trim().min(1), idempotencyKey },
      outputSchema: threadSettleOutputSchema,
      annotations: { readOnlyHint: false, destructiveHint: false, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_thread_settle", args, () => gateway.threadSettle(args)),
  );

  server.registerTool(
    "t3_thread_unsettle",
    {
      title: "Reopen a settled T3 thread",
      description: "Return a settled thread to the active list.",
      inputSchema: { threadId: z.string().trim().min(1), idempotencyKey },
      outputSchema: threadSettleOutputSchema,
      annotations: { readOnlyHint: false, destructiveHint: false, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_thread_unsettle", args, () => gateway.threadUnsettle(args)),
  );

  return server;
}

async function runTool<T extends object>(
  auditLog: AuditLog,
  toolName: string,
  input: unknown,
  operation: () => Promise<T>,
): Promise<CallToolResult> {
  const correlationId = `mcp_${randomUUID()}`;
  const startedAt = Date.now();
  await auditLog.record({
    source: "mcp",
    event: "tool.call",
    correlationId,
    operation: toolName,
    outcome: "started",
    details: { arguments: summarizeForAudit(input) },
  });
  try {
    const result = await operation();
    await auditLog.record({
      source: "mcp",
      event: "tool.result",
      correlationId,
      operation: toolName,
      outcome: "completed",
      durationMs: Date.now() - startedAt,
      details: { result: summarizeForAudit(result) },
    });
    return {
      content: [{ type: "text", text: JSON.stringify(result, null, 2) }],
      structuredContent: result as Record<string, unknown>,
    };
  } catch (error) {
    const code = error instanceof GatewayError ? error.code : "gateway_error";
    const message = error instanceof Error ? error.message : String(error);
    await auditLog.record({
      source: "mcp",
      event: "tool.result",
      correlationId,
      operation: toolName,
      outcome: "error",
      durationMs: Date.now() - startedAt,
      details: { errorCode: code, errorMessage: message },
    });
    return {
      isError: true,
      content: [{ type: "text", text: JSON.stringify({ error: { code, message } }, null, 2) }],
      structuredContent: { error: { code, message } },
    };
  }
}
