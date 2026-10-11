import { ProviderRetrySchema } from "../t3/types.js";
import { settingsReceiptSchema } from "../operations/settings.js";
import { randomUUID } from "node:crypto";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import type { CallToolResult } from "@modelcontextprotocol/sdk/types.js";
import { z } from "zod";
import { GATEWAY_VERSION, isNativeMutatingTool } from "../contract.js";
import { summarizeForAudit, withAuditContext, type AuditLog } from "../operations/audit-log.js";
import {
  GatewayError,
  T3Gateway,
  type PendingActionRespondInput,
  type GitDiffInput,
  type GitCompareInput,
  type TaskStartInput,
} from "../gateway.js";
import type { NativeSnapshot, NativeTool } from "../t3/native-mcp.js";

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

const observedTargetOutput = z
  .object({
    environmentId: z.string(),
    threadId: z.string(),
    turnId: z.string().nullable(),
    observedAt: z.string(),
  })
  .passthrough();

const failureOutput = z.object({
  category: z.enum(["quota", "rate_limit", "auth_billing", "provider_internal", "provider_error", "unknown"]),
  class: z.string().nullable().optional(),
  retryable: z.boolean().nullable().optional(),
  retry: ProviderRetrySchema.nullable().optional(),
  code: z.string().nullable(),
  message: z.string(),
  provider: z.string().nullable(),
  model: z.string(),
  turnId: z.string().nullable(),
  resetAt: z.string().nullable(),
  retryAfter: z.string().nullable(),
  source: z.enum(["t3_session", "t3_turn", "t3_activity", "t3_message", "t3_v2_turn_item"]),
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
    reasonCode: z.enum(["gateway_read_only", "t3_scope_required", "native_unavailable"]),
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
  native: z.object({
    url: z.string().nullable(),
    credential: z.enum(["missing", "expired", "valid"]),
    access: z.string().nullable(),
    scopes: z.array(z.string()),
    expiresAt: z.string().nullable(),
    catalogFetchedAt: z.string().nullable(),
    availableTools: z.number().int().nonnegative(),
    missingTools: z.array(z.string()),
    error: z.string().nullable(),
  }),
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
    parentCorrelationId: z.string().optional(),
    parentOperation: z.string().optional(),
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

const settingsObservationOutput = settingsReceiptSchema.extend({
  effective: settingsReceiptSchema.shape.resolved.extend({ observedAt: z.string() }).nullable(),
  state: z.enum(["observed", "unresolved"]),
  matchesResolved: z.boolean().nullable(),
});

const runResultOutputSchema = {
  settings: settingsObservationOutput.optional(),
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
  settings: settingsObservationOutput.optional(),
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

const pendingActionsListOutputSchema = {
  environmentId: z.string(),
  threadId: z.string(),
  hasPendingApprovals: z.boolean(),
  hasPendingUserInput: z.boolean(),
  actions: z.array(pendingActionOutput),
  detailsAvailable: z.boolean(),
};

/**
 * Builds the MCP surface: the gateway's own tools plus the allowlisted native
 * T3 tools in `native`, which are forwarded unchanged. Callers fetch the
 * snapshot first because tool registration is synchronous.
 */
export function createMcpServer(gateway: T3Gateway, native: NativeSnapshot | null = null): McpServer {
  const auditLog = gateway.audit;
  const server = new McpServer(
    { name: "t3-code-mcp", version: GATEWAY_VERSION },
    {
      instructions:
        "This gateway controls one configured T3 Code environment for an orchestrator outside T3. T3 remains authoritative for projects, threads, messages, runs, and workspaces. " +
        "Native T3 tools (t3_project_*, t3_thread_*, t3_queue_*, schedules, pull requests, orchestrator_capabilities) are forwarded unchanged. As an outside client, pass projectId or threadId explicitly. " +
        "Use t3_threads_list and t3_threads_overview to see every project's threads and what needs attention, t3_thread_read (view=activity for tool calls) for detail, and t3_thread_send with mode=steer, queue, or restart to redirect running work. " +
        "Prefer t3_task_start for delegated work: it is retry-safe with an idempotencyKey, can prepare a worktree, and pairs with t3_task_get, t3_result_get, and t3_task_cancel. Monitor it with t3_thread_wait using the task's threadId. " +
        "Answer approvals with t3_pending_action_respond; native tools cannot. Interrupts are requests: confirm them with t3_thread_read or t3_thread_wait. " +
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
      description: "Find threads by project and case-insensitive title, branch, or ID substring. status filters lifecycle (open/snoozed/settled/archived) only; activity/onlyRunning/sessionStatus filter execution, and running is not a lifecycle value. needsAttention filters pending/inconsistent/stale/failed. sort is deterministic (recent/title/status). Zero results return a retry hint; one exact candidate may be selected; multiple candidates need clarification. detail=full adds latest-response enrichment on the page and excludes full transcripts, which come from t3_thread_read. Unlike native t3_thread_list, it spans every project when projectId is omitted.",
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
    "t3_task_start",
    {
      title: "Start a recoverable T3 task",
      description:
        `Create one T3 thread and dispatch its required initial instruction as a recoverable two-command operation. Set workspaceMode=worktree with branch and optional startFromOrigin to have the gateway create an explicit isolated Git worktree from the selected local or origin base branch and attach it through ordinary HTTP thread creation. runtimeMode is required. Put requested model/runtime settings in arguments; omitted modelSelection uses the project default. branch is the base (for example main), not the generated task branch; omit worktreePath in worktree mode. Example: {"projectId":"project-id","title":"Repair Undo","instruction":"Repair the owned Undo defect and report checks","runtimeMode":"full-access","workspaceMode":"worktree","branch":"main","idempotencyKey":"unique-assignment-key"}. Add modelSelection:{"instanceId":"configured-instance","model":"requested-model"} when a model preference is known. Save taskRef and threadId: monitor with t3_thread_wait (threadId) or t3_task_get, steer with t3_thread_send, and stop with t3_task_cancel. Retries must reuse the same idempotencyKey and identical input; the gateway never stores the instruction text in its journal.`,
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
    "t3_task_cancel",
    {
      title: "Cancel a recoverable T3 task",
      description:
        "Ask T3 to stop the run that a t3_task_start task started. It interrupts only that run, never a later one in the same thread, and is safe to repeat. Acceptance is not proof of a stop; confirm with t3_thread_wait or t3_task_get.",
      inputSchema: {
        taskRef: z.string().trim().min(1),
        reason: z.string().trim().min(1).max(2000).optional(),
      },
      outputSchema: {
        taskRef: z.string(),
        threadId: z.string(),
        t3RunId: z.string(),
        status: z.string(),
        nextAction: z.string(),
      },
      annotations: { readOnlyHint: false, destructiveHint: true, idempotentHint: true, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_task_cancel", args, () => gateway.taskCancel(args)),
  );

  server.registerTool(
    "t3_attachment_upload",
    {
      title: "Upload a T3 attachment",
      description:
        "Upload base64 file bytes to T3 for a later t3_thread_send_attachments call. The gateway performs the signed upload because T3's upload URL is not reachable from remote clients. Images (gif, jpeg, png, webp) may be up to 10 MiB; other files up to 50 MiB, within the gateway's request body limit. The upload expires if no message uses it.",
      inputSchema: {
        name: z.string().trim().min(1).max(255),
        mimeType: z.string().trim().min(1).max(255),
        dataBase64: z.string().min(1),
      },
      outputSchema: {
        attachment: z.object({
          type: z.enum(["image", "file"]),
          id: z.string(),
          name: z.string(),
          mimeType: z.string(),
          sizeBytes: z.number().int().positive(),
        }),
        nextAction: z.string(),
      },
      annotations: { readOnlyHint: false, destructiveHint: false, openWorldHint: false },
    },
    async (args) => runTool(auditLog, "t3_attachment_upload", args, () => gateway.attachmentUpload(args)),
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

  for (const tool of native?.tools ?? []) {
    registerNativeTool(server, gateway, tool);
  }

  return server;
}

function registerNativeTool(server: McpServer, gateway: T3Gateway, tool: NativeTool): void {
  const mutating = isNativeMutatingTool(tool.name);
  server.registerTool(
    tool.name,
    {
      ...(tool.title === undefined ? {} : { title: tool.title }),
      ...(tool.description === undefined ? {} : { description: tool.description }),
      inputSchema: nativeInputSchema(tool.inputSchema),
      annotations: { ...tool.annotations, readOnlyHint: !mutating },
    },
    async (args) => auditedTool(gateway.audit, tool.name, args, async () => {
      const result = await gateway.callNative(tool.name, args);
      return { result, auditSummary: { isError: result.isError === true, keys: Object.keys(result.structuredContent ?? {}) } };
    }),
  );
}

/**
 * Converts a native JSON Schema for advertisement and local shape checks.
 * Unknown keys pass through unless the schema sets `additionalProperties:
 * false`. The conversion drops a few keywords such as `format`, so T3 remains
 * the authoritative validator.
 */
function nativeInputSchema(schema: NativeTool["inputSchema"]): z.ZodObject {
  try {
    // MCP types nested schemas loosely as `object`; by protocol they are JSON Schema.
    const converted = z.fromJSONSchema(schema as z.core.JSONSchema.JSONSchema);
    if (converted instanceof z.ZodObject) return converted;
  } catch {
    // Fall through below rather than hiding the tool.
  }
  // A schema that does not convert to an object accepts any arguments; T3 checks them.
  return z.looseObject({});
}

async function runTool<T extends object>(
  auditLog: AuditLog,
  toolName: string,
  input: unknown,
  operation: () => Promise<T>,
): Promise<CallToolResult> {
  return auditedTool(auditLog, toolName, input, async () => {
    const value = await operation();
    return {
      result: {
        content: [{ type: "text", text: JSON.stringify(value, null, 2) }],
        structuredContent: value as Record<string, unknown>,
      },
      auditSummary: value,
    };
  });
}

/**
 * Records one MCP tool call and its outcome. `auditSummary` lets proxied tools
 * log only the result shape: native results carry thread content under keys
 * the redactor does not know.
 */
async function auditedTool(
  auditLog: AuditLog,
  toolName: string,
  input: unknown,
  operation: () => Promise<{ readonly result: CallToolResult; readonly auditSummary: unknown }>,
): Promise<CallToolResult> {
  const correlationId = `mcp_${randomUUID()}`;
  return withAuditContext(correlationId, toolName, async () => {
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
      const { result, auditSummary } = await operation();
      await auditLog.record({
        source: "mcp",
        event: "tool.result",
        correlationId,
        operation: toolName,
        outcome: result.isError === true ? "error" : "completed",
        durationMs: Date.now() - startedAt,
        details: { result: summarizeForAudit(auditSummary) },
      });
      return result;
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
  });
}
