import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import type { CallToolResult } from "@modelcontextprotocol/sdk/types.js";
import { z } from "zod";
import {
  GatewayError,
  T3Gateway,
  type PendingActionRespondInput,
  type ProjectCreateInput,
  type ThreadCreateInput,
  type ThreadSendInput,
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
const cursor = z.string().regex(/^\d+$/).optional();
const limit = z.number().int().min(1).max(100).default(50);

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

const threadSummaryOutput = z
  .object({
    id: z.string(),
    projectId: z.string(),
    title: z.string(),
    modelSelection: modelSelectionOutput,
    runtimeMode: z.enum(["approval-required", "auto-accept-edits", "auto", "full-access"]),
    interactionMode: z.enum(["default", "plan"]),
    branch: z.string().nullable(),
    worktreePath: z.string().nullable(),
    latestTurn: latestTurnOutput.nullable(),
    sessionStatus: z.string().nullable(),
    archivedAt: z.string().nullable(),
    createdAt: z.string().nullable(),
    updatedAt: z.string().nullable(),
    hasPendingApprovals: z.boolean(),
    hasPendingUserInput: z.boolean(),
  })
  .passthrough();

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

const connectionStatusOutputSchema = {
  environment: environmentIdentityOutput.nullable(),
  connectionStatus: z.enum(["connected", "disconnected"]),
  stateFreshness: z.enum(["fresh", "stale", "unknown"]),
  lastObservedAt: z.string().nullable(),
  supportedCapabilities: z.array(z.string()),
  permittedOperations: z.array(z.string()),
  t3Scopes: z.array(z.string()),
  gatewayOperations: z.array(z.string()),
  error: z.string().optional(),
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

const projectCreateOutputSchema = {
  ...mutationResultShape,
  projectId: z.string(),
  workspaceRoot: z.string(),
};

const threadsListOutputSchema = {
  environmentId: z.string(),
  page: z
    .object({
      items: z.array(threadSummaryOutput),
      nextCursor: z.string().nullable(),
      hasMore: z.boolean(),
      total: z.number(),
    })
    .passthrough(),
};

const threadCreateOutputSchema = {
  ...mutationResultShape,
  projectId: z.string(),
  threadId: z.string(),
  workspace: z
    .object({
      branch: z.string().nullable(),
      worktreePath: z.string().nullable(),
    })
    .passthrough(),
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
  latestResponse: messageOutput.nullable(),
  pendingActions: z
    .object({
      approvals: z.boolean(),
      userInput: z.boolean(),
    })
    .passthrough(),
  timedOut: z.boolean().optional(),
  error: z.string().optional(),
};

const pendingActionsListOutputSchema = {
  environmentId: z.string(),
  threadId: z.string(),
  hasPendingApprovals: z.boolean(),
  hasPendingUserInput: z.boolean(),
  actions: z.array(pendingActionOutput),
  detailsAvailable: z.boolean(),
};

export function createMcpServer(gateway: T3Gateway): McpServer {
  const server = new McpServer(
    { name: "t3-code-mcp", version: "0.1.0" },
    {
      instructions:
        "This gateway controls one configured remote T3 Code environment. T3 remains authoritative for projects, threads, messages, runs, and workspaces. Mutation tools return after T3 accepts command intent; poll a returned runId with t3_run_get or t3_run_wait.",
    },
  );

  server.registerTool(
    "t3_connection_status",
    {
      title: "T3 connection status",
      description: "Inspect the configured T3 environment, connection health, freshness, capabilities, and scopes.",
      inputSchema: {},
      outputSchema: connectionStatusOutputSchema,
      annotations: { readOnlyHint: true, openWorldHint: false },
    },
    async () => runTool(() => gateway.connectionStatus()),
  );

  server.registerTool(
    "t3_projects_list",
    {
      title: "List T3 projects",
      description: "List projects registered in the remote T3 environment with cursor pagination.",
      inputSchema: { cursor, limit },
      outputSchema: projectsListOutputSchema,
      annotations: { readOnlyHint: true, openWorldHint: false },
    },
    async (args) => runTool(() => gateway.projectsList(args)),
  );

  server.registerTool(
    "t3_project_create",
    {
      title: "Register a T3 project",
      description:
        "Register an existing remote workspace as a T3 project. This does not clone a repository or create a missing directory. Requires orchestration control scope and an idempotencyKey.",
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
    async (args) => runTool(() => gateway.projectCreate(args satisfies ProjectCreateInput)),
  );

  server.registerTool(
    "t3_threads_list",
    {
      title: "List T3 threads",
      description: "List compact thread summaries from T3. Full history is fetched separately.",
      inputSchema: {
        projectId: z.string().trim().min(1).optional(),
        includeArchived: z.boolean().default(false),
        cursor,
        limit,
      },
      outputSchema: threadsListOutputSchema,
      annotations: { readOnlyHint: true, openWorldHint: false },
    },
    async (args) => runTool(() => gateway.threadsList(args)),
  );

  server.registerTool(
    "t3_thread_create",
    {
      title: "Create a T3 thread",
      description:
        "Create a thread in an existing T3 project. Supply modelSelection when the project has no default. The returned branch and worktree are the values T3 accepted.",
      inputSchema: {
        projectId: z.string().trim().min(1),
        title: z.string().trim().min(1).max(200),
        modelSelection: modelSelection.optional(),
        runtimeMode: z.enum(["approval-required", "auto-accept-edits", "auto", "full-access"]).optional(),
        interactionMode: z.enum(["default", "plan"]).optional(),
        branch: z.string().trim().min(1).nullable().optional(),
        worktreePath: z.string().trim().min(1).nullable().optional(),
        idempotencyKey,
      },
      outputSchema: threadCreateOutputSchema,
      annotations: { readOnlyHint: false, destructiveHint: false, openWorldHint: false },
    },
    async (args) => runTool(() => gateway.threadCreate(args satisfies ThreadCreateInput)),
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
    async ({ threadId }) => runTool(() => gateway.threadGet(threadId)),
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
    async ({ threadId, cursor: messageCursor, limit: messageLimit, maxChars }) =>
      runTool(() => gateway.threadMessages(threadId, { cursor: messageCursor, limit: messageLimit, maxChars })),
  );

  server.registerTool(
    "t3_thread_send",
    {
      title: "Send a T3 thread message",
      description:
        "Start one T3 agent turn and return immediately after command intent is accepted. Busy threads are rejected; queueing and steering are not enabled. Never reuse an idempotencyKey for different input.",
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
    async (args) => runTool(() => gateway.threadSend(args satisfies ThreadSendInput)),
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
    async ({ runId }) => runTool(() => gateway.runGet(runId)),
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
    async ({ runId, timeoutSeconds }) => runTool(() => gateway.runWait(runId, timeoutSeconds)),
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
    async (args) => runTool(() => gateway.runInterrupt(args)),
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
    async ({ threadId }) => runTool(() => gateway.pendingActionsList(threadId)),
  );

  server.registerTool(
    "t3_pending_action_respond",
    {
      title: "Respond to a T3 pending action",
      description: "Respond to a T3 approval or user-input request using its stable requestId. A model-provided approval is not treated as human confirmation by this gateway.",
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
    async (args) => runTool(() => gateway.pendingActionRespond(args satisfies PendingActionRespondInput)),
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
    async (args) => runTool(() => gateway.threadArchive(args)),
  );

  return server;
}

async function runTool<T extends object>(operation: () => Promise<T>): Promise<CallToolResult> {
  try {
    const result = await operation();
    return {
      content: [{ type: "text", text: JSON.stringify(result, null, 2) }],
      structuredContent: result as Record<string, unknown>,
    };
  } catch (error) {
    const code = error instanceof GatewayError ? error.code : "gateway_error";
    const message = error instanceof Error ? error.message : String(error);
    return {
      isError: true,
      content: [{ type: "text", text: JSON.stringify({ error: { code, message } }, null, 2) }],
      structuredContent: { error: { code, message } },
    };
  }
}
