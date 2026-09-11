import { randomUUID } from "node:crypto";
import {
  GATEWAY_COMMIT,
  GATEWAY_VERSION,
  TOOL_NAMES,
  toolSchemaFingerprint,
} from "./contract.js";
import {
  hasQueuedTurnStart,
  isThreadRunning,
  needsAttentionFor,
  observeThread,
  threadActivity,
  threadStatus,
  threadStatusReason,
  type ObservationQuality,
  type ThreadActivity,
  type ThreadStatus,
} from "./t3/thread-state.js";
import {
  defaultSnoozePreset,
  resolveSnoozePresets,
  snoozeWakeDescription,
  type SnoozePresetId,
} from "./t3/snooze.js";
import type { GatewayConfig } from "./config.js";
import {
  OperationJournal,
  hashPayload,
  type OperationKind,
  type OperationRecord,
} from "./operations/journal.js";
import {
  type InteractionMode,
  type ProjectCreateCommand,
  type RuntimeMode,
  type T3Command,
  type ThreadApprovalResponseCommand,
  type ThreadArchiveCommand,
  type ThreadCreateCommand,
  type ThreadSettleCommand,
  type ThreadSnoozeCommand,
  type ThreadTurnInterruptCommand,
  type ThreadTurnStartCommand,
  type ThreadUnsettleCommand,
  type ThreadUnsnoozeCommand,
  type ThreadUserInputResponseCommand,
} from "./t3/commands.js";
import { T3HttpClient, T3HttpError } from "./t3/http-client.js";
import type {
  Descriptor,
  LatestTurn,
  Message,
  ModelSelection,
  Project,
  Thread,
  ThreadShell,
} from "./t3/types.js";

export type ConnectionStatus = "connected" | "disconnected";
export type StateFreshness = "fresh" | "stale" | "unknown";

export interface EnvironmentIdentity {
  readonly environmentId: string;
  readonly label: string;
  readonly serverVersion: string;
}

export interface ConnectionStatusResult {
  readonly environment: EnvironmentIdentity | null;
  readonly connectionStatus: ConnectionStatus;
  readonly stateFreshness: StateFreshness;
  readonly lastObservedAt: string | null;
  readonly observedAt: string;
  readonly gatewayVersion: string;
  readonly gatewayCommit: string;
  readonly toolSchemaFingerprint: string;
  readonly effectiveAccessMode: "read-only" | "read-write";
  readonly callableOperations: ReadonlyArray<string>;
  readonly disabledOperations: ReadonlyArray<DisabledOperation>;
  readonly supportedCapabilities: ReadonlyArray<string>;
  readonly permittedOperations: ReadonlyArray<string>;
  readonly t3Scopes: ReadonlyArray<string>;
  /** Upstream T3 scopes. `t3Scopes` is kept for compatibility and mirrors this value. */
  readonly upstreamScopes: ReadonlyArray<string>;
  readonly gatewayOperations: ReadonlyArray<string>;
  readonly sessionExpiresAt: string | null;
  readonly error?: string;
}

export interface DisabledOperation {
  readonly operation: string;
  readonly reasonCode: "gateway_read_only" | "t3_scope_required";
  readonly reason: string;
}

export interface ObservedTarget {
  readonly environmentId: string;
  readonly threadId: string;
  readonly turnId: string | null;
  readonly observedAt: string;
}

export interface Page<T> {
  readonly items: ReadonlyArray<T>;
  readonly nextCursor: string | null;
  readonly hasMore: boolean;
  readonly total: number;
}

export interface ProjectSummary {
  readonly id: string;
  readonly title: string;
  readonly workspaceRoot: string;
  readonly defaultModelSelection: ModelSelection | null;
  readonly createdAt: string | null;
  readonly updatedAt: string | null;
}

export interface ThreadSummary {
  readonly id: string;
  readonly projectId: string;
  readonly projectTitle: string | null;
  readonly title: string;
  readonly modelSelection: ModelSelection;
  readonly runtimeMode: RuntimeMode;
  readonly interactionMode: InteractionMode;
  readonly branch: string | null;
  readonly worktreePath: string | null;
  readonly latestTurn: LatestTurn | null;
  readonly sessionStatus: string | null;
  readonly sessionUpdatedAt: string | null;
  readonly status: ThreadStatus;
  readonly statusReason: string;
  readonly activity: ThreadActivity;
  readonly isRunning: boolean;
  readonly hasConflictingSignals: boolean;
  readonly quality: ObservationQuality;
  readonly warning: string | null;
  readonly observedTurnId: string | null;
  readonly observedAt: string;
  readonly observedTarget: ObservedTarget;
  readonly settledOverride: "settled" | "active" | null;
  readonly settledAt: string | null;
  readonly snoozedUntil: string | null;
  readonly snoozedAt: string | null;
  readonly latestUserMessageAt: string | null;
  readonly pinnedAt: string | null;
  readonly hasActionableProposedPlan: boolean;
  readonly backgroundLiveness: "working" | "monitoring" | null;
  readonly archivedAt: string | null;
  readonly createdAt: string | null;
  readonly updatedAt: string | null;
  readonly hasPendingApprovals: boolean;
  readonly hasPendingUserInput: boolean;
}

export interface OverviewHighlight extends ThreadSummary {
  readonly latestResponseExcerpt: string | null;
}

export interface ThreadsOverview {
  readonly environmentId: string;
  readonly observedAt: string;
  readonly total: number;
  readonly counts: Record<ThreadStatus, number>;
  readonly executionCounts: Record<ThreadActivity, number>;
  readonly runningCount: number;
  readonly needsAttentionCount: number;
  readonly running: ReadonlyArray<ThreadSummary>;
  readonly highlights: ReadonlyArray<OverviewHighlight>;
}

export interface ThreadDetail extends ThreadSummary {
  readonly latestResponse: Message | null;
  readonly messageCount: number;
  readonly activityCount: number;
  readonly checkpointCount: number;
  readonly proposedPlanCount: number;
}

export interface MessagePage {
  readonly threadId: string;
  readonly messages: ReadonlyArray<Message>;
  readonly nextCursor: string | null;
  readonly hasMore: boolean;
  readonly total: number;
  readonly truncated: boolean;
}

export interface MutationAccepted {
  readonly environmentId: string;
  readonly operationId: string;
  readonly commandId: string;
  readonly status: "accepted";
  readonly t3Sequence: number | null;
}

export interface MutationUncertain {
  readonly environmentId: string;
  readonly operationId: string;
  readonly commandId: string;
  readonly status: "uncertain";
  readonly reason: string;
}

export interface MutationRejected {
  readonly environmentId: string;
  readonly operationId: string;
  readonly commandId: string;
  readonly status: "rejected";
  readonly reason: string;
}

export type MutationResult = MutationAccepted | MutationUncertain | MutationRejected;

export type ProjectCreateResult = MutationResult & {
  readonly projectId: string;
  readonly workspaceRoot: string;
};

export type ThreadCreateResult = MutationResult & {
  readonly projectId: string;
  readonly threadId: string;
  readonly modelSelection: ModelSelection | null;
  readonly workspace: {
    readonly branch: string | null;
    readonly worktreePath: string | null;
  };
};

export type ThreadSendResult = MutationResult & {
  readonly projectId: string;
  readonly threadId: string;
  readonly runId: string;
  readonly messageId: string;
  /** T3's orchestration turn identifier, not a provider-owned turn identifier. */
  readonly t3TurnId: string | null;
  readonly providerTurnId: string | null;
  readonly nextAction: "Use t3_run_get or t3_run_wait.";
};

export type RunStatus =
  | "accepted"
  | "running"
  | "completed"
  | "awaiting_approval"
  | "awaiting_input"
  | "failed"
  | "interrupted"
  | "unknown";

export interface RunResult {
  readonly environmentId: string;
  readonly operationId: string;
  readonly projectId: string | null;
  readonly threadId: string | null;
  readonly runId: string;
  /** T3's orchestration turn identifier, not a provider-owned turn identifier. */
  readonly t3TurnId: string | null;
  readonly runStatus: RunStatus;
  readonly providerTurnId: string | null;
  readonly connectionStatus: ConnectionStatus;
  readonly stateFreshness: StateFreshness;
  readonly lastObservedAt: string | null;
  readonly observedAt: string;
  readonly threadQuality: ObservationQuality | null;
  readonly threadWarning: string | null;
  readonly latestResponse: Message | null;
  readonly pendingActions: {
    readonly approvals: boolean;
    readonly userInput: boolean;
  };
  readonly timedOut?: boolean;
  readonly error?: string;
}

export interface PendingActionsResult {
  readonly environmentId: string;
  readonly threadId: string;
  readonly hasPendingApprovals: boolean;
  readonly hasPendingUserInput: boolean;
  readonly actions: ReadonlyArray<PendingAction>;
  readonly detailsAvailable: boolean;
}

export interface PendingAction {
  readonly kind: "approval" | "user_input" | "unknown";
  readonly requestId: string | null;
  readonly summary: string;
  readonly payload: unknown;
}

export interface MutationCommonInput {
  readonly idempotencyKey: string;
}

export interface ProjectCreateInput extends MutationCommonInput {
  readonly title: string;
  readonly workspaceRoot: string;
  readonly createWorkspaceRootIfMissing?: boolean;
  readonly defaultModelSelection?: ModelSelection | null;
}

export interface ThreadCreateInput extends MutationCommonInput {
  readonly projectId: string;
  readonly title: string;
  readonly modelSelection?: ModelSelection;
  readonly runtimeMode?: RuntimeMode;
  readonly interactionMode?: InteractionMode;
  readonly branch?: string | null;
  readonly worktreePath?: string | null;
}

export interface ThreadSendInput extends MutationCommonInput {
  readonly threadId: string;
  readonly message: string;
  readonly modelSelection?: ModelSelection;
  readonly runtimeMode?: RuntimeMode;
  readonly interactionMode?: InteractionMode;
  readonly titleSeed?: string;
}

export interface ThreadInterruptInput extends MutationCommonInput {
  readonly threadId: string;
  readonly expectedTurnId: string;
}

export interface RunInterruptInput extends MutationCommonInput {
  readonly runId: string;
}

export interface ArchiveThreadInput extends MutationCommonInput {
  readonly threadId: string;
}

export interface PendingActionRespondInput extends MutationCommonInput {
  readonly threadId: string;
  readonly requestId: string;
  readonly kind: "approval" | "user_input";
  readonly decision?: "accept" | "acceptForSession" | "decline" | "cancel";
  readonly answers?: Record<string, unknown>;
}

export type SnoozePresetInput = SnoozePresetId | "default";

export interface ThreadSnoozeInput extends MutationCommonInput {
  readonly threadId: string;
  /** Named wake time. Defaults to "default" (this evening, else tomorrow morning). */
  readonly preset?: SnoozePresetInput;
  /** Explicit ISO wake time. Specify either preset or snoozedUntil, not both. */
  readonly snoozedUntil?: string;
}

export interface ThreadUnsnoozeInput extends MutationCommonInput {
  readonly threadId: string;
}

export interface ThreadSettleInput extends MutationCommonInput {
  readonly threadId: string;
}

export interface ThreadUnsettleInput extends MutationCommonInput {
  readonly threadId: string;
}

export type ThreadSnoozeResult = MutationResult & {
  readonly threadId: string;
  /** Requested wake time (also the accepted time when status is accepted). */
  readonly snoozedUntil: string;
  readonly preset: SnoozePresetId | "custom";
  readonly wakeDescription: string;
  readonly note?: string;
};

export type ThreadUnsnoozeResult = MutationResult & {
  readonly threadId: string;
};

export type ThreadSettleResult = MutationResult & {
  readonly threadId: string;
  readonly settledOverride: "settled" | "active" | null;
  readonly lifecycle: ThreadStatus | null;
  readonly note?: string;
};

export type ThreadUnsettleResult = MutationResult & {
  readonly threadId: string;
  readonly settledOverride: "settled" | "active" | null;
  readonly lifecycle: ThreadStatus | null;
};

export interface ProviderOption {
  readonly instanceId: string | null;
  readonly provider: string | null;
  readonly model: string;
  readonly label: string;
  readonly projectsWithDefault: ReadonlyArray<string>;
  readonly threadsUsing: number;
}

export interface ProvidersResult {
  readonly environmentId: string;
  readonly observedAt: string;
  readonly defaultsByProject: Record<string, ModelSelection | null>;
  readonly options: ReadonlyArray<ProviderOption>;
}

export type InterruptVerification =
  | "interrupted"
  | "still_running"
  | "target_changed"
  | "not_running"
  | "inconsistent"
  | "unknown";

export interface ThreadInterruptVerification {
  readonly observed: InterruptVerification;
  readonly observedTurnId: string | null;
  readonly observedAt: string;
  readonly detail: string;
}

export type ThreadInterruptResult = MutationResult & {
  readonly threadId: string;
  readonly expectedTurnId: string;
  readonly verification: ThreadInterruptVerification | null;
};

export interface ThreadsListResult {
  readonly environmentId: string;
  readonly observedAt: string;
  readonly page: Page<ThreadSummary>;
  readonly resolutionHint: string | null;
}

const GATEWAY_OPERATIONS: ReadonlyArray<string> = [...TOOL_NAMES];

const MUTATING_GATEWAY_OPERATIONS = new Set<string>([
  "t3_project_create",
  "t3_thread_create",
  "t3_thread_send",
  "t3_run_interrupt",
  "t3_pending_action_respond",
  "t3_thread_archive",
  "t3_thread_interrupt",
  "t3_thread_snooze",
  "t3_thread_unsnooze",
  "t3_thread_settle",
  "t3_thread_unsettle",
]);

export class GatewayError extends Error {
  override readonly name = "GatewayError";

  constructor(
    readonly code: string,
    message: string,
  ) {
    super(message);
  }
}

export class T3Gateway {
  constructor(
    private readonly client: T3HttpClient,
    private readonly journal: OperationJournal,
    private readonly config: GatewayConfig,
  ) {}

  async connectionStatus(): Promise<ConnectionStatusResult> {
    let descriptor: Descriptor | null = null;
    let sessionScopes: ReadonlyArray<string> = [];
    let sessionExpiresAt: string | null = null;
    let failure: string | null = null;
    try {
      descriptor = await this.client.getDescriptor();
      this.assertEnvironment(descriptor);
      const session = await this.client.getSession();
      sessionScopes = session.scopes ?? [];
      sessionExpiresAt = session.expiresAt ?? null;
      await this.client.getShell();
    } catch (error) {
      failure = safeErrorMessage(error);
      descriptor = this.client.getCachedDescriptor();
      if (descriptor === null && failure.includes("does not match")) {
        // Environment mismatch is a hard failure; keep descriptor null only when truly unknown.
      }
    }

    const telemetry = this.client.telemetry();
    const environment = descriptor
      ? {
          environmentId: descriptor.environmentId,
          label: descriptor.label,
          serverVersion: descriptor.serverVersion,
        }
      : this.config.environmentId
        ? {
            environmentId: this.config.environmentId,
            label: this.config.environmentLabel ?? "configured T3 environment",
            serverVersion: "unknown",
          }
        : null;
    const lastObservedAt = telemetry.lastSnapshotAt ?? telemetry.lastSuccessfulAt;
    const observedAt = new Date().toISOString();
    const readOnly = this.config.readOnly;
    const hasOperate = sessionScopes.includes("orchestration:operate");
    // When disconnected we cannot confirm scopes; report read-only to fail closed.
    const effective: "read-only" | "read-write" = readOnly ? "read-only" : failure === null && hasOperate ? "read-write" : "read-only";
    const gatewayOps = readOnly
      ? GATEWAY_OPERATIONS.filter((operation) => !MUTATING_GATEWAY_OPERATIONS.has(operation))
      : [...GATEWAY_OPERATIONS];
    const { callableOperations, disabledOperations } = partitionOperations([...TOOL_NAMES], {
      readOnly,
      hasOperate: failure === null ? hasOperate : false,
    });

    return {
      environment,
      connectionStatus: failure === null ? "connected" : "disconnected",
      stateFreshness: freshness(lastObservedAt, this.config.staleAfterMs),
      lastObservedAt,
      observedAt,
      gatewayVersion: GATEWAY_VERSION,
      gatewayCommit: GATEWAY_COMMIT,
      toolSchemaFingerprint: toolSchemaFingerprint(),
      effectiveAccessMode: effective,
      callableOperations,
      disabledOperations,
      supportedCapabilities: descriptor ? Object.keys(descriptor.capabilities).sort() : [],
      permittedOperations: permittedOperations(sessionScopes),
      t3Scopes: sessionScopes,
      upstreamScopes: sessionScopes,
      gatewayOperations: gatewayOps,
      sessionExpiresAt,
      ...(failure === null ? {} : { error: failure }),
    };
  }

  async projectsList(input: { readonly query?: string; readonly cursor?: string; readonly limit: number }): Promise<{
    readonly environmentId: string;
    readonly page: Page<ProjectSummary>;
  }> {
    const shell = await this.client.getShell();
    const projects = shell.projects.filter((project) =>
      matchesQuery(input.query, project.title, project.workspaceRoot, project.id),
    );
    const page = paginate(projects.map(projectSummary), input.cursor, input.limit);
    return { environmentId: await this.environmentId(), page };
  }

  async threadsList(input: {
    readonly projectId?: string;
    readonly includeArchived: boolean;
    readonly query?: string;
    readonly status?: ThreadStatus | "all";
    readonly onlyRunning?: boolean;
    readonly sessionStatus?: string;
    readonly activity?: ThreadActivity;
    readonly needsAttention?: boolean;
    readonly sort?: "recent" | "title" | "status";
    readonly detail?: "summary" | "full";
    readonly cursor?: string;
    readonly limit: number;
  }): Promise<ThreadsListResult> {
    const shell = await this.client.getShell();
    const environmentId = await this.environmentId();
    const now = Date.now();
    const observedAt = new Date(now).toISOString();
    const projectTitles = new Map(shell.projects.map((project) => [project.id, project.title]));
    const filtered = shell.threads.filter((thread) => {
      if (input.projectId !== undefined && thread.projectId !== input.projectId) return false;
      if (!matchesQuery(input.query, thread.title, thread.branch, thread.id)) return false;
      if (input.onlyRunning === true && !isThreadRunning(thread)) return false;
      if (input.sessionStatus !== undefined && (thread.session?.status ?? null) !== input.sessionStatus) return false;
      if (input.activity !== undefined && threadActivity(thread) !== input.activity) return false;
      if (input.needsAttention === true && !needsAttentionFor(thread, now)) return false;
      if (input.needsAttention === false && needsAttentionFor(thread, now)) return false;
      if (input.status !== undefined && input.status !== "all") {
        return threadStatus(thread, now) === input.status;
      }
      return input.includeArchived || !thread.archivedAt;
    });
    const sorted = sortThreads(filtered, input.sort ?? "recent", now);
    const summaries = sorted.map((thread) => threadSummary(thread, projectTitles.get(thread.projectId) ?? null, environmentId, now));
    // detail=full is served by the same shell rows plus latest-response enrichment on the page only.
    // Full transcripts still require t3_thread_messages.
    let items: ReadonlyArray<ThreadSummary> = summaries;
    if (input.detail === "full") {
      items = await this.enrichWithLatestResponse(summaries, 200);
    }
    const page = paginate(items, input.cursor, input.limit);
    return {
      environmentId,
      observedAt,
      page,
      resolutionHint: resolutionHint(filtered.length, input),
    };
  }

  async threadsOverview(input: {
    readonly projectId?: string;
    readonly includeArchived: boolean;
    readonly query?: string;
    readonly runningLimit: number;
  }): Promise<ThreadsOverview> {
    const shell = await this.client.getShell();
    const environmentId = await this.environmentId();
    const now = Date.now();
    const observedAt = new Date(now).toISOString();
    const projectTitles = new Map(shell.projects.map((project) => [project.id, project.title]));
    const filtered = shell.threads.filter((thread) => {
      if (input.projectId !== undefined && thread.projectId !== input.projectId) return false;
      if (!matchesQuery(input.query, thread.title, thread.branch, thread.id)) return false;
      return input.includeArchived || !thread.archivedAt;
    });
    const counts: Record<ThreadStatus, number> = { open: 0, snoozed: 0, settled: 0, archived: 0 };
    const executionCounts: Record<ThreadActivity, number> = {
      running: 0,
      starting: 0,
      awaiting_approval: 0,
      awaiting_input: 0,
      failed: 0,
      idle: 0,
    };
    let needsAttentionCount = 0;
    for (const thread of filtered) {
      counts[threadStatus(thread, now)] += 1;
      executionCounts[threadActivity(thread)] += 1;
      if (needsAttentionFor(thread, now)) needsAttentionCount += 1;
    }
    const running = filtered
      .filter(isThreadRunning)
      .map((thread) => threadSummary(thread, projectTitles.get(thread.projectId) ?? null, environmentId, now));
    const highlights = await this.buildHighlights(filtered, projectTitles, environmentId, now);
    return {
      environmentId,
      observedAt,
      total: filtered.length,
      counts,
      executionCounts,
      runningCount: running.length,
      needsAttentionCount,
      running: running.slice(0, input.runningLimit),
      highlights,
    };
  }

  async providersList(): Promise<ProvidersResult> {
    const shell = await this.client.getShell();
    const environmentId = await this.environmentId();
    const observedAt = new Date().toISOString();
    const defaultsByProject: Record<string, ModelSelection | null> = {};
    for (const project of shell.projects) {
      defaultsByProject[project.id] = project.defaultModelSelection ?? null;
    }
    const byKey = new Map<string, { selection: ModelSelection; projectsWithDefault: Set<string>; threadsUsing: number }>();
    const keyOf = (selection: ModelSelection): string =>
      `${selection.instanceId ?? ""}\u0000${selection.provider ?? ""}\u0000${selection.model}`;
    for (const project of shell.projects) {
      const def = project.defaultModelSelection;
      if (!def) continue;
      const key = keyOf(def);
      const entry = byKey.get(key) ?? { selection: def, projectsWithDefault: new Set<string>(), threadsUsing: 0 };
      entry.projectsWithDefault.add(project.id);
      byKey.set(key, entry);
    }
    for (const thread of shell.threads) {
      const key = keyOf(thread.modelSelection);
      const entry = byKey.get(key) ?? {
        selection: thread.modelSelection,
        projectsWithDefault: new Set<string>(),
        threadsUsing: 0,
      };
      entry.threadsUsing += 1;
      byKey.set(key, entry);
    }
    const options: ProviderOption[] = [...byKey.values()]
      .map((entry) => ({
        instanceId: entry.selection.instanceId ?? null,
        provider: entry.selection.provider ?? null,
        model: entry.selection.model,
        label: [entry.selection.instanceId ?? entry.selection.provider ?? "t3", entry.selection.model]
          .filter(Boolean)
          .join("/"),
        projectsWithDefault: [...entry.projectsWithDefault].sort(),
        threadsUsing: entry.threadsUsing,
      }))
      .sort((a, b) => a.label.localeCompare(b.label));
    return { environmentId, observedAt, defaultsByProject, options };
  }

  async threadGet(threadId: string): Promise<{ readonly environmentId: string; readonly thread: ThreadDetail }> {
    const snapshot = await this.client.getThread(threadId);
    // Full thread snapshots omit the shell's pending flags and latest-user timestamp.
    const shell = await this.client.getShell();
    const environmentId = await this.environmentId();
    const summary = shell.threads.find((thread) => thread.id === threadId);
    const projectTitle = shell.projects.find((project) => project.id === snapshot.thread.projectId)?.title ?? null;
    return {
      environmentId,
      thread: threadDetail(snapshot.thread, summary, projectTitle, environmentId, Date.now()),
    };
  }

  async threadMessages(
    threadId: string,
    input: { readonly cursor?: string; readonly limit: number; readonly maxChars: number },
  ): Promise<{ readonly environmentId: string; readonly page: MessagePage }> {
    const snapshot = await this.client.getThread(threadId);
    const { items, ...page } = paginate(snapshot.thread.messages, input.cursor, input.limit);
    let truncated = false;
    const messages = items.map((message) => {
      if (message.text.length <= input.maxChars) {
        return message;
      }
      truncated = true;
      return { ...message, text: `${message.text.slice(0, input.maxChars)}\n[truncated]` };
    });
    return {
      environmentId: await this.environmentId(),
      page: { threadId, ...page, messages, truncated },
    };
  }

  async projectCreate(input: ProjectCreateInput): Promise<ProjectCreateResult> {
    const payload = {
      title: input.title,
      workspaceRoot: input.workspaceRoot,
      createWorkspaceRootIfMissing: input.createWorkspaceRootIfMissing ?? false,
      defaultModelSelection: input.defaultModelSelection ?? null,
    };
    const commandKind: OperationKind = "project.create";
    if (this.config.readOnly) {
      await this.requireOperationScope();
    }
    const existing = await this.journal.getByIdempotencyKey(input.idempotencyKey);
    if (existing) {
      const begun = await this.journal.begin({
        kind: commandKind,
        idempotencyKey: input.idempotencyKey,
        payloadHash: hashPayload(payload),
        projectId: existing.projectId,
      });
      const reconciled = await this.reconcile(begun.record);
      return this.projectCreateResult(reconciled, input.workspaceRoot);
    }
    await this.requireOperationScope();
    const projectId = randomUUID();
    const begun = await this.journal.begin({
      kind: commandKind,
      idempotencyKey: input.idempotencyKey,
      payloadHash: hashPayload(payload),
      projectId,
    });
    if (begun.reused) {
      const reconciled = await this.reconcile(begun.record);
      return this.projectCreateResult(reconciled, input.workspaceRoot);
    }

    const command: ProjectCreateCommand = {
      type: "project.create",
      commandId: begun.record.commandId,
      projectId,
      title: input.title,
      workspaceRoot: input.workspaceRoot,
      ...(input.createWorkspaceRootIfMissing === undefined
        ? {}
        : { createWorkspaceRootIfMissing: input.createWorkspaceRootIfMissing }),
      ...(input.defaultModelSelection === undefined
        ? {}
        : { defaultModelSelection: input.defaultModelSelection }),
      createdAt: new Date().toISOString(),
    };
    const result = await this.dispatchNew(begun.record, command);
    return this.projectCreateResult(result, input.workspaceRoot);
  }

  async threadCreate(input: ThreadCreateInput): Promise<ThreadCreateResult> {
    const payload = {
      projectId: input.projectId,
      title: input.title,
      modelSelection: input.modelSelection ?? null,
      runtimeMode: input.runtimeMode ?? "full-access",
      interactionMode: input.interactionMode ?? "default",
      branch: input.branch ?? null,
      worktreePath: input.worktreePath ?? null,
    } satisfies Record<string, unknown>;
    if (this.config.readOnly) {
      await this.requireOperationScope();
    }
    const existing = await this.journal.getByIdempotencyKey(input.idempotencyKey);
    if (existing) {
      const begun = await this.journal.begin({
        kind: "thread.create",
        idempotencyKey: input.idempotencyKey,
        payloadHash: hashPayload(payload),
        projectId: existing.projectId ?? input.projectId,
        threadId: existing.threadId,
      });
      const reconciled = await this.reconcile(begun.record);
      return this.threadCreateResult(
        reconciled,
        input.projectId,
        input.modelSelection ?? null,
        input.branch ?? null,
        input.worktreePath ?? null,
      );
    }
    await this.requireOperationScope();
    const project = await this.findProject(input.projectId);
    const modelSelection = input.modelSelection ?? project.defaultModelSelection;
    if (modelSelection === null || modelSelection === undefined) {
      throw new GatewayError(
        "model_selection_required",
        "The project has no default model. Call t3_providers_list to discover available instanceId/model values, then supply modelSelection.",
      );
    }
    const threadId = randomUUID();
    const begun = await this.journal.begin({
      kind: "thread.create",
      idempotencyKey: input.idempotencyKey,
      payloadHash: hashPayload(payload),
      projectId: input.projectId,
      threadId,
    });
    if (begun.reused) {
      const reconciled = await this.reconcile(begun.record);
      return this.threadCreateResult(reconciled, input.projectId, modelSelection, input.branch ?? null, input.worktreePath ?? null);
    }
    const command: ThreadCreateCommand = {
      type: "thread.create",
      commandId: begun.record.commandId,
      threadId,
      projectId: input.projectId,
      title: input.title,
      modelSelection,
      runtimeMode: input.runtimeMode ?? "full-access",
      interactionMode: input.interactionMode ?? "default",
      branch: input.branch ?? null,
      worktreePath: input.worktreePath ?? null,
      createdAt: new Date().toISOString(),
    };
    const result = await this.dispatchNew(begun.record, command);
    return this.threadCreateResult(result, input.projectId, modelSelection, command.branch, command.worktreePath);
  }

  async threadSend(input: ThreadSendInput): Promise<ThreadSendResult> {
    if (this.config.readOnly) {
      await this.requireOperationScope();
    }
    const payload = {
      threadId: input.threadId,
      message: input.message,
      modelSelection: input.modelSelection ?? null,
      runtimeMode: input.runtimeMode ?? null,
      interactionMode: input.interactionMode ?? null,
      titleSeed: input.titleSeed ?? null,
    };
    const existing = await this.journal.getByIdempotencyKey(input.idempotencyKey);
    if (existing) {
      const begun = await this.journal.begin({
        kind: "thread.turn.start",
        idempotencyKey: input.idempotencyKey,
        payloadHash: hashPayload(payload),
        projectId: existing.projectId,
        threadId: input.threadId,
        runId: existing.runId ?? `run_${randomUUID()}`,
        messageId: existing.messageId ?? `user:msg_${randomUUID().replaceAll("-", "")}`,
      });
      return this.threadSendResult(await this.reconcile(begun.record));
    }
    await this.requireOperationScope();
    const snapshot = await this.client.getThread(input.threadId);
    if (threadIsBusy(snapshot.thread)) {
      const now = Date.now();
      const observation = observeThread(snapshot.thread, now);
      const activeTurn = snapshot.thread.latestTurn?.turnId ?? null;
      const sessionStatus = snapshot.thread.session != null
        ? (snapshot.thread.session as { status?: unknown }).status
        : null;
      throw new GatewayError(
        "thread_busy",
        `Thread ${input.threadId} is busy (execution=${observation.execution}, ` +
          `turn=${activeTurn ?? "unknown"}, session=${typeof sessionStatus === "string" ? sessionStatus : "unknown"}, ` +
          `quality=${observation.quality}). Valid next actions: poll t3_thread_get or t3_run_get, ` +
          `wait for completion, or interrupt the observed turn ${activeTurn ?? "once known"} with t3_thread_interrupt. ` +
          `Queueing and steering are not enabled.`,
      );
    }

    const runId = `run_${randomUUID()}`;
    const messageId = `user:msg_${randomUUID().replaceAll("-", "")}`;
    const begun = await this.journal.begin({
      kind: "thread.turn.start",
      idempotencyKey: input.idempotencyKey,
      payloadHash: hashPayload(payload),
      projectId: snapshot.thread.projectId,
      threadId: input.threadId,
      runId,
      messageId,
    });
    if (begun.reused) {
      const reconciled = await this.reconcile(begun.record);
      return this.threadSendResult(reconciled);
    }

    const command: ThreadTurnStartCommand = {
      type: "thread.turn.start",
      commandId: begun.record.commandId,
      threadId: input.threadId,
      message: { messageId, role: "user", text: input.message, attachments: [] },
      ...(input.modelSelection === undefined ? {} : { modelSelection: input.modelSelection }),
      ...(input.titleSeed === undefined ? {} : { titleSeed: input.titleSeed }),
      runtimeMode: input.runtimeMode ?? snapshot.thread.runtimeMode,
      interactionMode: input.interactionMode ?? snapshot.thread.interactionMode ?? "default",
      createdAt: new Date().toISOString(),
    };
    const result = await this.dispatchNew(begun.record, command);
    return this.threadSendResult(result);
  }

  async runGet(runId: string): Promise<RunResult> {
    const record = await this.journal.getByRunId(runId);
    const operation = record ?? (await this.findRun(runId));
    if (!operation || operation.kind !== "thread.turn.start") {
      throw new GatewayError("run_not_found", `Run ${runId} was not found in the gateway journal.`);
    }
    return this.observeRun(operation);
  }

  async runWait(runId: string, timeoutSeconds: number): Promise<RunResult> {
    const initial = await this.runGet(runId);
    const deadline = Date.now() + timeoutSeconds * 1000;
    let current = initial;
    while (Date.now() < deadline && !isTerminal(current.runStatus)) {
      await delay(Math.min(500, Math.max(50, deadline - Date.now())));
      current = await this.runGet(runId);
      if (hasRelevantChange(initial, current)) {
        return current;
      }
    }
    return { ...current, ...(isTerminal(current.runStatus) ? {} : { timedOut: true }) };
  }

  async runInterrupt(input: RunInterruptInput): Promise<MutationResult> {
    if (this.config.readOnly) {
      await this.requireOperationScope();
    }
    const existing = await this.journal.getByIdempotencyKey(input.idempotencyKey);
    if (existing) {
      const payload = {
        runId: input.runId,
        threadId: existing.threadId ?? null,
        turnId: existing.turnId ?? null,
      };
      const begun = await this.journal.begin({
        kind: "thread.turn.interrupt",
        idempotencyKey: input.idempotencyKey,
        payloadHash: hashPayload(payload),
        threadId: existing.threadId,
        runId: input.runId,
        ...(existing.turnId === undefined ? {} : { turnId: existing.turnId }),
      });
      return mutationResult(await this.reconcile(begun.record), await this.knownEnvironmentId());
    }
    await this.requireOperationScope();
    const run = await this.runGet(input.runId);
    if (!run.threadId) {
      throw new GatewayError("run_thread_missing", "The run has no associated thread.");
    }
    if (isTerminal(run.runStatus)) {
      throw new GatewayError("run_not_active", `Run ${input.runId} is already ${run.runStatus}.`);
    }
    const snapshot = await this.client.getThread(run.threadId);
    const turnId = run.t3TurnId ?? associatedTurnId(snapshot.thread, input.runId);
    const payload = { runId: input.runId, threadId: run.threadId, turnId: turnId ?? null };
    const begun = await this.journal.begin({
      kind: "thread.turn.interrupt",
      idempotencyKey: input.idempotencyKey,
      payloadHash: hashPayload(payload),
      threadId: run.threadId,
      runId: input.runId,
      ...(turnId === null || turnId === undefined ? {} : { turnId }),
    });
    if (begun.reused) {
      return mutationResult(await this.reconcile(begun.record), await this.knownEnvironmentId());
    }
    const command: ThreadTurnInterruptCommand = {
      type: "thread.turn.interrupt",
      commandId: begun.record.commandId,
      threadId: run.threadId,
      ...(turnId === null || turnId === undefined ? {} : { turnId }),
      createdAt: new Date().toISOString(),
    };
    const result = await this.dispatchNew(begun.record, command);
    return mutationResult(result, await this.knownEnvironmentId());
  }

  async threadInterrupt(input: ThreadInterruptInput): Promise<ThreadInterruptResult> {
    if (this.config.readOnly) await this.requireOperationScope();
    const payload = { threadId: input.threadId, expectedTurnId: input.expectedTurnId };
    const beginInput = {
      kind: "thread.turn.interrupt" as const,
      idempotencyKey: input.idempotencyKey,
      payloadHash: hashPayload(payload),
      threadId: input.threadId,
      turnId: input.expectedTurnId,
    };
    const existing = await this.journal.getByIdempotencyKey(input.idempotencyKey);
    if (existing) {
      const begun = await this.journal.begin(beginInput);
      const environmentId = await this.knownEnvironmentId();
      return {
        ...mutationResult(begun.record, environmentId),
        threadId: input.threadId,
        expectedTurnId: input.expectedTurnId,
        verification: await this.verifyInterruption(environmentId, input.threadId, input.expectedTurnId),
      };
    }
    await this.requireOperationScope();
    // Check identity before dispatch; a misconfigured endpoint must never stop work.
    // T3 interrupts by provider session, so this precheck is not atomic.
    const environmentId = await this.environmentId();
    const { thread } = await this.client.getThread(input.threadId);
    const observedAt = new Date().toISOString();
    if (thread.latestTurn?.turnId !== input.expectedTurnId) {
      throw new GatewayError(
        "turn_changed",
        `Thread ${input.threadId} latest turn is ${thread.latestTurn?.turnId ?? "unknown"} at ${observedAt}, ` +
          `not expected ${input.expectedTurnId}. Read t3_thread_get again and use the fresh observedTarget before interrupting.`,
      );
    }
    if (thread.latestTurn.state !== "running") {
      throw new GatewayError(
        "thread_not_running",
        `Thread ${input.threadId} turn ${input.expectedTurnId} is ${thread.latestTurn.state} at ${observedAt}, ` +
          `not running. Read t3_thread_get to reconcile; do not retry the interrupt.`,
      );
    }
    const begun = await this.journal.begin(beginInput);
    if (begun.reused) {
      return {
        ...mutationResult(begun.record, environmentId),
        threadId: input.threadId,
        expectedTurnId: input.expectedTurnId,
        verification: await this.verifyInterruption(environmentId, input.threadId, input.expectedTurnId),
      };
    }
    const result = await this.dispatchNew(begun.record, {
      type: "thread.turn.interrupt",
      commandId: begun.record.commandId,
      threadId: input.threadId,
      turnId: input.expectedTurnId,
      createdAt: new Date().toISOString(),
    });
    const verification = result.status === "accepted"
      ? await this.verifyInterruption(environmentId, input.threadId, input.expectedTurnId)
      : null;
    return {
      ...mutationResult(result, environmentId),
      threadId: input.threadId,
      expectedTurnId: input.expectedTurnId,
      verification,
    };
  }

  private async verifyInterruption(
    environmentId: string,
    threadId: string,
    expectedTurnId: string,
  ): Promise<ThreadInterruptVerification> {
    void environmentId;
    try {
      const { thread } = await this.client.getThread(threadId);
      const now = Date.now();
      const observation = observeThread(thread, now);
      const currentTurnId = thread.latestTurn?.turnId ?? null;
      const currentState = thread.latestTurn?.state ?? null;
      if (currentTurnId !== expectedTurnId) {
        return {
          observed: "target_changed",
          observedTurnId: currentTurnId,
          observedAt: observation.observedAt,
          detail: `Latest turn changed from ${expectedTurnId} to ${currentTurnId ?? "unknown"}. ` +
            `Acceptance did not confirm the observed run stopped; read t3_thread_get to reconcile. ` +
            `T3 interrupts by provider session without an atomic turn condition.`,
        };
      }
      if (currentState === "interrupted") {
        return {
          observed: "interrupted",
          observedTurnId: currentTurnId,
          observedAt: observation.observedAt,
          detail: `Turn ${expectedTurnId} is interrupted as observed at ${observation.observedAt}.`,
        };
      }
      if (currentState !== "running") {
        return {
          observed: "not_running",
          observedTurnId: currentTurnId,
          observedAt: observation.observedAt,
          detail: `Turn ${expectedTurnId} is ${currentState ?? "unknown"}; it is no longer running.`,
        };
      }
      if (observation.quality === "inconsistent") {
        return {
          observed: "inconsistent",
          observedTurnId: currentTurnId,
          observedAt: observation.observedAt,
          detail: observation.warning ?? "T3 signals disagree after interrupt acceptance.",
        };
      }
      return {
        observed: "still_running",
        observedTurnId: currentTurnId,
        observedAt: observation.observedAt,
        detail: `Interrupt accepted but turn ${expectedTurnId} still reports running at ${observation.observedAt}. ` +
          `Poll t3_thread_get; acceptance alone does not confirm a stop.`,
      };
    } catch (error) {
      return {
        observed: "unknown",
        observedTurnId: null,
        observedAt: new Date().toISOString(),
        detail: `Could not verify interruption: ${safeErrorMessage(error)}. ` +
          `Reconcile with t3_thread_get using the durable operation handle; do not resubmit with a fresh key.`,
      };
    }
  }

  async pendingActionsList(threadId: string): Promise<PendingActionsResult> {
    const snapshot = await this.client.getThread(threadId);
    const actions = extractPendingActions(snapshot.thread);
    return {
      environmentId: await this.environmentId(),
      threadId,
      hasPendingApprovals: snapshot.thread.hasPendingApprovals ?? false,
      hasPendingUserInput: snapshot.thread.hasPendingUserInput ?? false,
      actions,
      detailsAvailable: actions.length > 0,
    };
  }

  async pendingActionRespond(input: PendingActionRespondInput): Promise<MutationResult> {
    if (this.config.readOnly) {
      await this.requireOperationScope();
    }
    if (input.kind === "approval" && input.decision === undefined) {
      throw new GatewayError("decision_required", "Approval responses require a decision.");
    }
    if (input.kind === "user_input" && input.answers === undefined) {
      throw new GatewayError("answers_required", "User-input responses require answers.");
    }
    const payload = {
      threadId: input.threadId,
      requestId: input.requestId,
      kind: input.kind,
      decision: input.decision ?? null,
      answers: input.answers ?? null,
    };
    const existing = await this.journal.getByIdempotencyKey(input.idempotencyKey);
    if (existing) {
      const begun = await this.journal.begin({
        kind: input.kind === "approval" ? "thread.approval.respond" : "thread.user-input.respond",
        idempotencyKey: input.idempotencyKey,
        payloadHash: hashPayload(payload),
        threadId: existing.threadId ?? input.threadId,
      });
      return mutationResult(await this.reconcile(begun.record), await this.knownEnvironmentId());
    }
    await this.requireOperationScope();
    const begun = await this.journal.begin({
      kind: input.kind === "approval" ? "thread.approval.respond" : "thread.user-input.respond",
      idempotencyKey: input.idempotencyKey,
      payloadHash: hashPayload(payload),
      threadId: input.threadId,
    });
    if (begun.reused) {
      return mutationResult(await this.reconcile(begun.record), await this.knownEnvironmentId());
    }
    const command: ThreadApprovalResponseCommand | ThreadUserInputResponseCommand =
      input.kind === "approval"
        ? {
            type: "thread.approval.respond",
            commandId: begun.record.commandId,
            threadId: input.threadId,
            requestId: input.requestId,
            decision: input.decision as ThreadApprovalResponseCommand["decision"],
            createdAt: new Date().toISOString(),
          }
        : {
            type: "thread.user-input.respond",
            commandId: begun.record.commandId,
            threadId: input.threadId,
            requestId: input.requestId,
            answers: input.answers as Record<string, unknown>,
            createdAt: new Date().toISOString(),
          };
    const result = await this.dispatchNew(begun.record, command);
    return mutationResult(result, await this.knownEnvironmentId());
  }

  async threadArchive(input: ArchiveThreadInput): Promise<MutationResult> {
    if (this.config.readOnly) {
      await this.requireOperationScope();
    }
    const payload = { threadId: input.threadId };
    const existing = await this.journal.getByIdempotencyKey(input.idempotencyKey);
    if (existing) {
      const begun = await this.journal.begin({
        kind: "thread.archive",
        idempotencyKey: input.idempotencyKey,
        payloadHash: hashPayload(payload),
        threadId: existing.threadId ?? input.threadId,
      });
      return mutationResult(await this.reconcile(begun.record), await this.knownEnvironmentId());
    }
    await this.requireOperationScope();
    const begun = await this.journal.begin({
      kind: "thread.archive",
      idempotencyKey: input.idempotencyKey,
      payloadHash: hashPayload(payload),
      threadId: input.threadId,
    });
    if (begun.reused) {
      return mutationResult(await this.reconcile(begun.record), await this.knownEnvironmentId());
    }
    const command: ThreadArchiveCommand = {
      type: "thread.archive",
      commandId: begun.record.commandId,
      threadId: input.threadId,
    };
    const result = await this.dispatchNew(begun.record, command);
    return mutationResult(result, await this.knownEnvironmentId());
  }

  async threadSnooze(input: ThreadSnoozeInput): Promise<ThreadSnoozeResult> {
    if (this.config.readOnly) {
      await this.requireOperationScope();
    }
    if (input.snoozedUntil !== undefined && input.preset !== undefined && input.preset !== "default") {
      throw new GatewayError("invalid_snooze_input", "Specify either preset or snoozedUntil, not both.");
    }
    if (input.snoozedUntil !== undefined) {
      const parsed = Date.parse(input.snoozedUntil);
      if (!Number.isFinite(parsed)) {
        throw new GatewayError("invalid_snooze_time", `snoozedUntil ${JSON.stringify(input.snoozedUntil)} is not a valid date.`);
      }
      if (parsed <= Date.now()) {
        throw new GatewayError("invalid_snooze_time", "The snooze wake time must be in the future.");
      }
    }
    // The idempotency hash covers user intent (preset or explicit time), not
    // the resolved clock time, so a retry with the same key stays identical
    // while the resolved wake time is journaled with the operation.
    const payload = { threadId: input.threadId, preset: input.preset ?? "default", snoozedUntil: input.snoozedUntil ?? null };
    const existing = await this.journal.getByIdempotencyKey(input.idempotencyKey);
    if (existing) {
      const begun = await this.journal.begin({
        kind: "thread.snooze",
        idempotencyKey: input.idempotencyKey,
        payloadHash: hashPayload(payload),
        threadId: existing.threadId ?? input.threadId,
      });
      const snoozedUntil = begun.record.snoozedUntil ?? resolveSnoozeWakeTime(input);
      const reconciled = await this.reconcile(begun.record);
      return this.threadSnoozeResult(reconciled, input.threadId, snoozedUntil, presetOf(input, snoozedUntil), false);
    }
    await this.requireOperationScope();
    const snoozedUntil = resolveSnoozeWakeTime(input);
    const snapshot = await this.client.getThread(input.threadId);
    assertNotArchived(snapshot.thread, input.threadId);
    if (snapshot.thread.hasPendingApprovals === true || snapshot.thread.hasPendingUserInput === true) {
      throw new GatewayError(
        "snooze_not_allowed",
        `Thread ${input.threadId} has a pending approval or user-input request. Respond to the request first; snoozing would hide the agent waiting on you.`,
      );
    }
    if (hasQueuedTurnStart(snapshot.thread)) {
      throw new GatewayError(
        "snooze_not_allowed",
        `Thread ${input.threadId} has a queued turn start that no turn has adopted yet. Wait for the turn to start before snoozing.`,
      );
    }
    const running = isThreadRunning(snapshot.thread);
    const begun = await this.journal.begin({
      kind: "thread.snooze",
      idempotencyKey: input.idempotencyKey,
      payloadHash: hashPayload(payload),
      threadId: input.threadId,
      snoozedUntil,
    });
    if (begun.reused) {
      const reconciled = await this.reconcile(begun.record);
      const wakeTime = begun.record.snoozedUntil ?? snoozedUntil;
      return this.threadSnoozeResult(reconciled, input.threadId, wakeTime, presetOf(input, wakeTime), running);
    }
    const command: ThreadSnoozeCommand = {
      type: "thread.snooze",
      commandId: begun.record.commandId,
      threadId: input.threadId,
      snoozedUntil,
    };
    const result = await this.dispatchNew(begun.record, command);
    return this.threadSnoozeResult(result, input.threadId, snoozedUntil, presetOf(input, snoozedUntil), running);
  }

  async threadUnsnooze(input: ThreadUnsnoozeInput): Promise<ThreadUnsnoozeResult> {
    if (this.config.readOnly) {
      await this.requireOperationScope();
    }
    const payload = { threadId: input.threadId };
    const existing = await this.journal.getByIdempotencyKey(input.idempotencyKey);
    if (existing) {
      const begun = await this.journal.begin({
        kind: "thread.unsnooze",
        idempotencyKey: input.idempotencyKey,
        payloadHash: hashPayload(payload),
        threadId: existing.threadId ?? input.threadId,
      });
      return {
        ...mutationResult(await this.reconcile(begun.record), await this.knownEnvironmentId()),
        threadId: input.threadId,
      };
    }
    await this.requireOperationScope();
    const snapshot = await this.client.getThread(input.threadId);
    assertNotArchived(snapshot.thread, input.threadId);
    const begun = await this.journal.begin({
      kind: "thread.unsnooze",
      idempotencyKey: input.idempotencyKey,
      payloadHash: hashPayload(payload),
      threadId: input.threadId,
    });
    if (begun.reused) {
      return {
        ...mutationResult(await this.reconcile(begun.record), await this.knownEnvironmentId()),
        threadId: input.threadId,
      };
    }
    const command: ThreadUnsnoozeCommand = {
      type: "thread.unsnooze",
      commandId: begun.record.commandId,
      threadId: input.threadId,
      reason: "user",
    };
    const result = await this.dispatchNew(begun.record, command);
    return {
      ...mutationResult(result, await this.knownEnvironmentId()),
      threadId: input.threadId,
    };
  }

  async threadSettle(input: ThreadSettleInput): Promise<ThreadSettleResult> {
    if (this.config.readOnly) {
      await this.requireOperationScope();
    }
    const payload = { threadId: input.threadId };
    const existing = await this.journal.getByIdempotencyKey(input.idempotencyKey);
    if (existing) {
      const begun = await this.journal.begin({
        kind: "thread.settle",
        idempotencyKey: input.idempotencyKey,
        payloadHash: hashPayload(payload),
        threadId: existing.threadId ?? input.threadId,
      });
      const reconciled = await this.reconcile(begun.record);
      return this.threadSettleResult(reconciled, input.threadId);
    }
    await this.requireOperationScope();
    const snapshot = await this.client.getThread(input.threadId);
    assertNotArchived(snapshot.thread, input.threadId);
    const sessionStatus = snapshot.thread.session?.status ?? null;
    if (sessionStatus === "starting" || sessionStatus === "running" || snapshot.thread.latestTurn?.state === "running") {
      const turn = snapshot.thread.latestTurn?.turnId ?? null;
      throw new GatewayError(
        "settle_blocked",
        `Thread ${input.threadId} is still running${turn ? ` (turn ${turn})` : ""}. ` +
          `Interrupt the observed turn with t3_thread_interrupt or wait for completion before settling.`,
      );
    }
    if (snapshot.thread.hasPendingApprovals === true) {
      throw new GatewayError(
        "settle_blocked",
        `Thread ${input.threadId} has a pending approval. Respond with t3_pending_action_respond first; settling cannot answer approvals.`,
      );
    }
    if (hasQueuedTurnStart(snapshot.thread)) {
      throw new GatewayError(
        "settle_blocked",
        `Thread ${input.threadId} has a queued turn start that no turn has adopted yet. Wait for the turn to start before settling.`,
      );
    }
    const begun = await this.journal.begin({
      kind: "thread.settle",
      idempotencyKey: input.idempotencyKey,
      payloadHash: hashPayload(payload),
      threadId: input.threadId,
    });
    if (begun.reused) {
      const reconciled = await this.reconcile(begun.record);
      return this.threadSettleResult(reconciled, input.threadId);
    }
    const command: ThreadSettleCommand = {
      type: "thread.settle",
      commandId: begun.record.commandId,
      threadId: input.threadId,
    };
    const result = await this.dispatchNew(begun.record, command);
    return this.threadSettleResult(result, input.threadId);
  }

  async threadUnsettle(input: ThreadUnsettleInput): Promise<ThreadUnsettleResult> {
    if (this.config.readOnly) {
      await this.requireOperationScope();
    }
    const payload = { threadId: input.threadId };
    const existing = await this.journal.getByIdempotencyKey(input.idempotencyKey);
    if (existing) {
      const begun = await this.journal.begin({
        kind: "thread.unsettle",
        idempotencyKey: input.idempotencyKey,
        payloadHash: hashPayload(payload),
        threadId: existing.threadId ?? input.threadId,
      });
      const reconciled = await this.reconcile(begun.record);
      return this.threadUnsettleResult(reconciled, input.threadId);
    }
    await this.requireOperationScope();
    const snapshot = await this.client.getThread(input.threadId);
    assertNotArchived(snapshot.thread, input.threadId);
    const begun = await this.journal.begin({
      kind: "thread.unsettle",
      idempotencyKey: input.idempotencyKey,
      payloadHash: hashPayload(payload),
      threadId: input.threadId,
    });
    if (begun.reused) {
      const reconciled = await this.reconcile(begun.record);
      return this.threadUnsettleResult(reconciled, input.threadId);
    }
    const command: ThreadUnsettleCommand = {
      type: "thread.unsettle",
      commandId: begun.record.commandId,
      threadId: input.threadId,
      reason: "user",
    };
    const result = await this.dispatchNew(begun.record, command);
    return this.threadUnsettleResult(result, input.threadId);
  }

  private async threadSnoozeResult(
    record: OperationRecord,
    threadId: string,
    snoozedUntil: string,
    preset: SnoozePresetId | "custom",
    running: boolean,
  ): Promise<ThreadSnoozeResult> {
    const base = mutationResult(record, await this.knownEnvironmentId());
    const wakeDescription = snoozeWakeDescription(snoozedUntil);
    if (base.status === "accepted" && running) {
      return {
        ...base,
        threadId,
        snoozedUntil,
        preset,
        wakeDescription,
        note: "Snooze hides the thread; the agent keeps running.",
      };
    }
    return { ...base, threadId, snoozedUntil, preset, wakeDescription };
  }

  private async threadSettleResult(record: OperationRecord, threadId: string): Promise<ThreadSettleResult> {
    const base = mutationResult(record, await this.knownEnvironmentId());
    if (base.status !== "accepted") {
      return { ...base, threadId, settledOverride: null, lifecycle: null };
    }
    try {
      const snapshot = await this.client.getThread(threadId);
      const now = Date.now();
      const lifecycle = threadStatus(snapshot.thread, now);
      const override = snapshot.thread.settledOverride ?? null;
      if (lifecycle !== "settled") {
        return {
          ...base,
          threadId,
          settledOverride: override,
          lifecycle,
          note: `Settle accepted but the thread shows ${lifecycle} (${threadStatusReason(snapshot.thread, now)}).`,
        };
      }
      return { ...base, threadId, settledOverride: override, lifecycle };
    } catch {
      return { ...base, threadId, settledOverride: null, lifecycle: null };
    }
  }

  private async threadUnsettleResult(record: OperationRecord, threadId: string): Promise<ThreadUnsettleResult> {
    const base = mutationResult(record, await this.knownEnvironmentId());
    if (base.status !== "accepted") {
      return { ...base, threadId, settledOverride: null, lifecycle: null };
    }
    try {
      const snapshot = await this.client.getThread(threadId);
      const now = Date.now();
      return {
        ...base,
        threadId,
        settledOverride: snapshot.thread.settledOverride ?? null,
        lifecycle: threadStatus(snapshot.thread, now),
      };
    } catch {
      return { ...base, threadId, settledOverride: null, lifecycle: null };
    }
  }

  private async dispatchNew(record: OperationRecord, command: T3Command): Promise<OperationRecord> {
    try {
      const dispatch = await this.client.dispatch(command);
      return await this.journal.update(record.operationId, {
        status: "accepted",
        t3Sequence: dispatch.sequence,
      });
    } catch (error) {
      if (error instanceof T3HttpError && error.status >= 400 && error.status < 500) {
        return this.journal.update(record.operationId, {
          status: "rejected",
          lastError: safeErrorMessage(error),
        });
      }
      return this.journal.update(record.operationId, {
        status: "uncertain",
        lastError: `${safeErrorMessage(error)} Reconcile with operation ${record.operationId} via t3_run_get; do not resubmit with a fresh idempotency key.`,
      });
    }
  }

  private async reconcile(record: OperationRecord): Promise<OperationRecord> {
    if (record.status === "accepted" || record.status === "rejected") {
      return record;
    }
    try {
      const snapshot = await this.client.getShell();
      if (record.kind === "project.create" && record.projectId && snapshot.projects.some((project) => project.id === record.projectId)) {
        return this.journal.update(record.operationId, { status: "accepted" });
      }
      if (record.threadId) {
        const threadSnapshot = await this.client.getThread(record.threadId);
        if (record.kind === "thread.create") {
          return this.journal.update(record.operationId, { status: "accepted" });
        }
        if (record.kind === "thread.turn.start" && record.messageId) {
          const message = threadSnapshot.thread.messages.find((candidate) => candidate.id === record.messageId);
          if (message) {
            return this.journal.update(record.operationId, {
              status: "accepted",
              ...(message.turnId ? { turnId: message.turnId } : {}),
            });
          }
        }
        if (record.kind === "thread.archive" && threadSnapshot.thread.archivedAt) {
          return this.journal.update(record.operationId, { status: "accepted" });
        }
        if (record.kind === "thread.snooze" && threadSnapshot.thread.snoozedUntil != null &&
          Date.parse(threadSnapshot.thread.snoozedUntil) > Date.now()) {
          return this.journal.update(record.operationId, { status: "accepted" });
        }
        if (record.kind === "thread.unsnooze" && threadSnapshot.thread.snoozedUntil == null) {
          return this.journal.update(record.operationId, { status: "accepted" });
        }
        if (record.kind === "thread.settle" && threadSnapshot.thread.settledOverride === "settled") {
          return this.journal.update(record.operationId, { status: "accepted" });
        }
        if (record.kind === "thread.unsettle" && threadSnapshot.thread.settledOverride !== "settled") {
          return this.journal.update(record.operationId, { status: "accepted" });
        }
      }
    } catch {
      // Preserve uncertainty. A lost connection is not evidence of failure.
    }
    return record;
  }

  private async observeRun(record: OperationRecord): Promise<RunResult> {
    if (!record.runId || !record.threadId) {
      throw new GatewayError("run_invalid", "The journal entry does not contain a run handle.");
    }
    try {
      const snapshot = await this.client.getThread(record.threadId);
      const thread = snapshot.thread;
      const now = Date.now();
      const observedAt = new Date(now).toISOString();
      const observation = observeThread(thread, now);
      const message = record.messageId
        ? thread.messages.find((candidate) => candidate.id === record.messageId) ?? null
        : null;
      const turnId = record.turnId ?? message?.turnId ?? null;
      if (turnId && record.turnId !== turnId) {
        await this.journal.update(record.operationId, { turnId });
      }
      const latestTurn = thread.latestTurn ?? null;
      const sameTurn = latestTurn !== null && (turnId === null || latestTurn.turnId === turnId);
      const runStatus =
        record.status === "rejected"
          ? "failed"
          : record.status === "uncertain" && message === null && latestTurn === null
            ? "unknown"
            : sameTurn
              ? latestTurnToRunStatus(latestTurn, thread)
              : message?.role === "assistant"
                ? "completed"
                : "accepted";
      return {
        environmentId: await this.environmentId(),
        operationId: record.operationId,
        projectId: thread.projectId,
        threadId: thread.id,
        runId: record.runId,
        t3TurnId: turnId,
        runStatus,
        providerTurnId: null,
        connectionStatus: "connected",
        stateFreshness: "fresh",
        lastObservedAt: observedAt,
        observedAt,
        threadQuality: observation.quality,
        threadWarning: observation.warning,
        latestResponse: latestAssistant(thread.messages, turnId),
        pendingActions: {
          approvals: thread.hasPendingApprovals ?? false,
          userInput: thread.hasPendingUserInput ?? false,
        },
      };
    } catch (error) {
      const telemetry = this.client.telemetry();
      const observedAt = new Date().toISOString();
      return {
        environmentId: await this.environmentId().catch(() => this.config.environmentId ?? "unknown"),
        operationId: record.operationId,
        projectId: record.projectId ?? null,
        threadId: record.threadId ?? null,
        runId: record.runId,
        t3TurnId: record.turnId ?? null,
        runStatus: "unknown",
        providerTurnId: null,
        connectionStatus: "disconnected",
        stateFreshness: freshness(telemetry.lastSnapshotAt, this.config.staleAfterMs),
        lastObservedAt: telemetry.lastSnapshotAt,
        observedAt,
        threadQuality: null,
        threadWarning: null,
        latestResponse: null,
        pendingActions: { approvals: false, userInput: false },
        error: safeErrorMessage(error),
      };
    }
  }

  private async findProject(projectId: string): Promise<Project> {
    const shell = await this.client.getShell();
    const project = shell.projects.find((candidate) => candidate.id === projectId);
    if (!project) {
      throw new GatewayError(
        "project_not_found",
        `Project ${projectId} was not found. Call t3_projects_list to discover available project IDs.`,
      );
    }
    return project;
  }

  private async buildHighlights(
    filtered: ReadonlyArray<ThreadShell>,
    projectTitles: Map<string, string>,
    environmentId: string,
    now: number,
  ): Promise<ReadonlyArray<OverviewHighlight>> {
    const ranked = [...filtered]
      .map((thread) => ({
        thread,
        rank: highlightRank(thread, now),
        recency: threadRecency(thread),
      }))
      .sort((a, b) => a.rank - b.rank || b.recency - a.recency || a.thread.id.localeCompare(b.thread.id))
      .slice(0, 5);
    const summaries = ranked.map(({ thread }) =>
      threadSummary(thread, projectTitles.get(thread.projectId) ?? null, environmentId, now),
    );
    return this.enrichWithLatestResponse(summaries, 200);
  }

  private async enrichWithLatestResponse<T extends ThreadSummary>(
    summaries: ReadonlyArray<T>,
    excerptChars: number,
  ): Promise<Array<T & { latestResponseExcerpt: string | null }>> {
    const results: Array<T & { latestResponseExcerpt: string | null }> = [];
    for (const summary of summaries) {
      try {
        const snapshot = await this.client.getThread(summary.id);
        const latest = latestAssistant(snapshot.thread.messages, snapshot.thread.latestTurn?.turnId ?? null)
          ?? latestAssistant(snapshot.thread.messages, null);
        const excerpt = latest ? truncateExcerpt(latest.text, excerptChars) : null;
        results.push({ ...summary, latestResponseExcerpt: excerpt });
      } catch {
        results.push({ ...summary, latestResponseExcerpt: null });
      }
    }
    return results;
  }

  private async findRun(runId: string): Promise<OperationRecord | null> {
    const direct = await this.journal.getByOperationId(runId);
    if (direct) {
      return direct;
    }
    const normalized = runId.replace(/^run_/, "");
    for (const candidate of [normalized, `run_${normalized}`]) {
      const record = await this.journal.getByOperationId(candidate);
      if (record) {
        return record;
      }
    }
    return null;
  }

  private async environmentId(): Promise<string> {
    const descriptor = await this.client.getDescriptor();
    this.assertEnvironment(descriptor);
    return descriptor.environmentId;
  }

  private async knownEnvironmentId(): Promise<string> {
    try {
      return await this.environmentId();
    } catch {
      return this.client.getCachedDescriptor()?.environmentId ?? this.config.environmentId ?? "unknown";
    }
  }

  private async requireOperationScope(): Promise<void> {
    if (this.config.readOnly) {
      throw new GatewayError("gateway_read_only", "This gateway endpoint is configured as read-only.");
    }
    const session = await this.client.getSession();
    if (!session.authenticated || !(session.scopes ?? []).includes("orchestration:operate")) {
      throw new GatewayError(
        "t3_scope_required",
        "The configured T3 credential does not grant orchestration:operate.",
      );
    }
  }

  private assertEnvironment(descriptor: Descriptor): void {
    if (this.config.environmentId && descriptor.environmentId !== this.config.environmentId) {
      throw new GatewayError(
        "environment_mismatch",
        `Configured environment ${this.config.environmentId} does not match the connected T3 environment.`,
      );
    }
  }

  private async projectCreateResult(record: OperationRecord, workspaceRoot: string): Promise<ProjectCreateResult> {
    return {
      ...mutationResult(record, await this.knownEnvironmentId()),
      projectId: record.projectId ?? "unknown",
      workspaceRoot,
    };
  }

  private async threadCreateResult(
    record: OperationRecord,
    projectId: string,
    modelSelection: ModelSelection | null,
    branch: string | null,
    worktreePath: string | null,
  ): Promise<ThreadCreateResult> {
    // Return the T3-accepted workspace/model when the thread is already visible.
    if (record.status === "accepted" && record.threadId) {
      try {
        const snapshot = await this.client.getThread(record.threadId);
        return {
          ...mutationResult(record, await this.knownEnvironmentId()),
          projectId: snapshot.thread.projectId,
          threadId: snapshot.thread.id,
          modelSelection: snapshot.thread.modelSelection,
          workspace: {
            branch: snapshot.thread.branch ?? null,
            worktreePath: snapshot.thread.worktreePath ?? null,
          },
        };
      } catch {
        // Fall back to the requested values; acceptance is already journaled.
      }
    }
    return {
      ...mutationResult(record, await this.knownEnvironmentId()),
      projectId,
      threadId: record.threadId ?? "unknown",
      modelSelection,
      workspace: { branch, worktreePath },
    };
  }

  private async threadSendResult(record: OperationRecord): Promise<ThreadSendResult> {
    return {
      ...mutationResult(record, await this.knownEnvironmentId()),
      projectId: record.projectId ?? "unknown",
      threadId: record.threadId ?? "unknown",
      runId: record.runId ?? "unknown",
      messageId: record.messageId ?? "unknown",
      t3TurnId: record.turnId ?? null,
      providerTurnId: null,
      nextAction: "Use t3_run_get or t3_run_wait.",
    };
  }
}

function mutationResult(record: OperationRecord, environmentId: string): MutationResult {
  if (record.status === "accepted") {
    return {
      environmentId,
      operationId: record.operationId,
      commandId: record.commandId,
      status: "accepted",
      t3Sequence: record.t3Sequence ?? null,
    };
  }
  if (record.status === "rejected") {
    return {
      environmentId,
      operationId: record.operationId,
      commandId: record.commandId,
      status: "rejected",
      reason: record.lastError ?? "T3 rejected the command.",
    };
  }
  return {
    environmentId,
    operationId: record.operationId,
    commandId: record.commandId,
    status: "uncertain",
    reason:
      record.lastError ??
      `The command outcome could not be reconciled. Reconcile with operation ${record.operationId} via t3_run_get; do not resubmit with a fresh idempotency key.`,
  };
}

function partitionOperations(
  operations: ReadonlyArray<string>,
  access: { readonly readOnly: boolean; readonly hasOperate: boolean },
): { readonly callableOperations: string[]; readonly disabledOperations: DisabledOperation[] } {
  const callableOperations: string[] = [];
  const disabledOperations: DisabledOperation[] = [];
  for (const operation of operations) {
    if (!MUTATING_GATEWAY_OPERATIONS.has(operation)) {
      callableOperations.push(operation);
      continue;
    }
    if (access.readOnly) {
      disabledOperations.push({
        operation,
        reasonCode: "gateway_read_only",
        reason: "Gateway is configured read-only (MCP_READ_ONLY=true). Restart with MCP_READ_ONLY=false to enable control.",
      });
      continue;
    }
    if (!access.hasOperate) {
      disabledOperations.push({
        operation,
        reasonCode: "t3_scope_required",
        reason: "Upstream T3 credential lacks orchestration:operate. Mutations will be rejected until the scope is granted.",
      });
      continue;
    }
    callableOperations.push(operation);
  }
  return { callableOperations, disabledOperations };
}

function sortThreads(
  threads: ReadonlyArray<ThreadShell>,
  sort: "recent" | "title" | "status",
  now: number,
): ThreadShell[] {
  const copy = [...threads];
  if (sort === "title") {
    copy.sort((a, b) => a.title.localeCompare(b.title) || a.id.localeCompare(b.id));
    return copy;
  }
  if (sort === "status") {
    const order: Record<ThreadStatus, number> = { open: 0, snoozed: 1, settled: 2, archived: 3 };
    copy.sort(
      (a, b) =>
        order[threadStatus(a, now)] - order[threadStatus(b, now)] ||
        threadRecency(b) - threadRecency(a) ||
        a.id.localeCompare(b.id),
    );
    return copy;
  }
  copy.sort((a, b) => threadRecency(b) - threadRecency(a) || a.id.localeCompare(b.id));
  return copy;
}

function highlightRank(thread: ThreadShell, now: number): number {
  if (thread.hasPendingApprovals === true || thread.hasPendingUserInput === true) return 0;
  const observation = observeThread(thread, now);
  if (observation.quality === "inconsistent" || observation.quality === "stale") return 1;
  if (observation.execution === "failed") return 2;
  if (observation.execution === "running" || observation.execution === "starting") return 3;
  return 4;
}

function threadRecency(thread: ThreadShell): number {
  const candidates = [
    thread.latestUserMessageAt,
    thread.session?.updatedAt,
    thread.updatedAt,
    thread.createdAt,
  ];
  let best = 0;
  for (const value of candidates) {
    const parsed = Date.parse(value ?? "");
    if (Number.isFinite(parsed) && parsed > best) best = parsed;
  }
  return best;
}

function truncateExcerpt(text: string, maxChars: number): string {
  const normalized = text.replace(/\s+/g, " ").trim();
  if (normalized.length <= maxChars) return normalized;
  return `${normalized.slice(0, maxChars)}…`;
}

function resolutionHint(
  total: number,
  input: { readonly query?: string; readonly projectId?: string; readonly status?: ThreadStatus | "all" },
): string | null {
  if (total === 1) return "One exact candidate. The client may select it and carry its observedTarget into control calls.";
  if (total > 1) {
    return "Multiple candidates. Present project, title, branch, and recent activity for clarification; never invent a newest choice for a mutation.";
  }
  const scope: string[] = [];
  if (input.projectId) scope.push(`project ${input.projectId}`);
  if (input.query) scope.push(`query ${JSON.stringify(input.query)}`);
  if (input.status && input.status !== "all") scope.push(`status ${input.status}`);
  const where = scope.length > 0 ? ` for ${scope.join(", ")}` : "";
  return `No threads${where}. Retry with a broader query, another project, or includeArchived=true. ` +
    `Zero results must not select a thread for control.`;
}

function projectSummary(project: Project): ProjectSummary {
  return {
    id: project.id,
    title: project.title,
    workspaceRoot: project.workspaceRoot,
    defaultModelSelection: project.defaultModelSelection ?? null,
    createdAt: project.createdAt ?? null,
    updatedAt: project.updatedAt ?? null,
  };
}

function assertNotArchived(thread: Thread, threadId: string): void {
  if (thread.archivedAt) {
    throw new GatewayError("thread_archived", `Thread ${threadId} is archived. Unarchive it before changing snooze or settlement.`);
  }
}

function resolveSnoozeWakeTime(input: ThreadSnoozeInput, now: Date = new Date()): string {
  if (input.snoozedUntil !== undefined) {
    return new Date(Date.parse(input.snoozedUntil)).toISOString();
  }
  const presetId = input.preset ?? "default";
  const resolved = presetId === "default"
    ? defaultSnoozePreset(now)
    : resolveSnoozePresets(now).find((candidate) => candidate.id === presetId);
  if (!resolved) {
    throw new GatewayError("invalid_snooze_preset", `Unknown snooze preset ${JSON.stringify(presetId)}.`);
  }
  return resolved.snoozedUntil;
}

function presetOf(input: ThreadSnoozeInput, _snoozedUntil: string): SnoozePresetId | "custom" {
  if (input.snoozedUntil !== undefined) return "custom";
  const presetId = input.preset ?? "default";
  if (presetId !== "default") return presetId;
  return defaultSnoozePreset().id;
}

function threadSummary(
  thread: ThreadShell,
  projectTitle: string | null = null,
  environmentId = "unknown",
  now = Date.now(),
): ThreadSummary {
  const session = asRecord(thread.session);
  const observation = observeThread(thread, now);
  return {
    id: thread.id,
    projectId: thread.projectId,
    projectTitle,
    title: thread.title,
    modelSelection: thread.modelSelection,
    runtimeMode: thread.runtimeMode,
    interactionMode: thread.interactionMode ?? "default",
    branch: thread.branch ?? null,
    worktreePath: thread.worktreePath ?? null,
    latestTurn: thread.latestTurn ?? null,
    sessionStatus: typeof session?.status === "string" ? session.status : null,
    sessionUpdatedAt: typeof session?.updatedAt === "string" ? session.updatedAt : null,
    status: observation.lifecycle,
    statusReason: threadStatusReason(thread, now),
    activity: observation.execution,
    isRunning: isThreadRunning(thread),
    hasConflictingSignals: observation.quality === "inconsistent",
    quality: observation.quality,
    warning: observation.warning,
    observedTurnId: observation.observedTurnId,
    observedAt: observation.observedAt,
    observedTarget: {
      environmentId,
      threadId: thread.id,
      turnId: observation.observedTurnId,
      observedAt: observation.observedAt,
    },
    settledOverride: thread.settledOverride ?? null,
    settledAt: thread.settledAt ?? null,
    snoozedUntil: thread.snoozedUntil ?? null,
    snoozedAt: thread.snoozedAt ?? null,
    latestUserMessageAt: thread.latestUserMessageAt ?? null,
    pinnedAt: thread.pinnedAt ?? null,
    hasActionableProposedPlan: thread.hasActionableProposedPlan ?? false,
    backgroundLiveness: thread.backgroundLiveness ?? null,
    archivedAt: thread.archivedAt ?? null,
    createdAt: thread.createdAt ?? null,
    updatedAt: thread.updatedAt ?? null,
    hasPendingApprovals: thread.hasPendingApprovals ?? false,
    hasPendingUserInput: thread.hasPendingUserInput ?? false,
  };
}

function threadDetail(
  thread: Thread,
  summary: ThreadShell | undefined,
  projectTitle: string | null = null,
  environmentId = "unknown",
  now = Date.now(),
): ThreadDetail {
  const source: ThreadShell = summary ?? thread;
  return {
    ...threadSummary(source, projectTitle, environmentId, now),
    // Latest response is bounded to the observed turn; full history needs t3_thread_messages.
    latestResponse: latestAssistant(thread.messages, thread.latestTurn?.turnId ?? null),
    messageCount: thread.messages.length,
    activityCount: thread.activities.length,
    checkpointCount: thread.checkpoints.length,
    proposedPlanCount: thread.proposedPlans.length,
  };
}

function latestAssistant(messages: ReadonlyArray<Message>, turnId: string | null): Message | null {
  const candidates = messages.filter(
    (message) => message.role === "assistant" && (turnId === null || message.turnId === turnId),
  );
  return candidates.at(-1) ?? null;
}

function threadIsBusy(thread: Thread): boolean {
  if (thread.latestTurn?.state === "running") {
    return true;
  }
  const status = asRecord(thread.session)?.status;
  return status === "starting" || status === "running";
}

function associatedTurnId(thread: Thread, _runId: string): string | null {
  return thread.latestTurn?.state === "running" ? thread.latestTurn.turnId : null;
}

function latestTurnToRunStatus(latestTurn: LatestTurn, thread: Thread): RunStatus {
  if (thread.hasPendingApprovals) {
    return "awaiting_approval";
  }
  if (thread.hasPendingUserInput) {
    return "awaiting_input";
  }
  switch (latestTurn.state) {
    case "running":
      return "running";
    case "completed":
      return "completed";
    case "interrupted":
      return "interrupted";
    case "error":
      return "failed";
  }
}

function extractPendingActions(thread: Thread): PendingAction[] {
  const actions: PendingAction[] = [];
  for (const activity of thread.activities) {
    const record = asRecord(activity);
    const payload = asRecord(record?.payload);
    if (!record || !payload) {
      continue;
    }
    const kind: PendingAction["kind"] =
      record.tone === "approval" || String(record.kind ?? "").includes("approval")
        ? "approval"
        : String(record.kind ?? "").includes("input") || String(record.kind ?? "").includes("question")
          ? "user_input"
          : "unknown";
    const requestId = typeof payload.requestId === "string" ? payload.requestId : null;
    if (requestId !== null || kind !== "unknown") {
      actions.push({
        kind,
        requestId,
        summary: typeof record.summary === "string" ? record.summary : String(record.kind ?? "pending action"),
        payload,
      });
    }
  }
  return actions;
}

function matchesQuery(query: string | undefined, ...values: Array<string | null | undefined>): boolean {
  const normalized = query?.trim().toLowerCase();
  return !normalized || values.some((value) => value?.toLowerCase().includes(normalized));
}

function paginate<T>(items: ReadonlyArray<T>, cursor: string | undefined, limit: number): Page<T> {
  const offset = parseCursor(cursor);
  const pageItems = items.slice(offset, offset + limit);
  const nextOffset = offset + pageItems.length;
  return {
    items: pageItems,
    nextCursor: nextOffset < items.length ? String(nextOffset) : null,
    hasMore: nextOffset < items.length,
    total: items.length,
  };
}

function parseCursor(cursor: string | undefined): number {
  if (cursor === undefined || cursor === "") {
    return 0;
  }
  const value = Number(cursor);
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new GatewayError("invalid_cursor", "cursor must be a non-negative integer string.");
  }
  return value;
}

function freshness(lastObservedAt: string | null, staleAfterMs: number): StateFreshness {
  if (lastObservedAt === null) {
    return "unknown";
  }
  const timestamp = Date.parse(lastObservedAt);
  return Number.isFinite(timestamp) && Date.now() - timestamp <= staleAfterMs ? "fresh" : "stale";
}

function permittedOperations(scopes: ReadonlyArray<string>): string[] {
  const operations: string[] = [];
  if (scopes.includes("orchestration:read")) operations.push("orchestration:read");
  if (scopes.includes("orchestration:operate")) operations.push("orchestration:operate");
  if (scopes.includes("terminal:operate")) operations.push("terminal:operate");
  if (scopes.includes("review:write")) operations.push("review:write");
  return operations;
}

function isTerminal(status: RunStatus): boolean {
  return status === "completed" || status === "failed" || status === "interrupted";
}

function hasRelevantChange(initial: RunResult, current: RunResult): boolean {
  return (
    current.runStatus !== initial.runStatus ||
    current.t3TurnId !== initial.t3TurnId ||
    current.latestResponse?.id !== initial.latestResponse?.id ||
    current.pendingActions.approvals !== initial.pendingActions.approvals ||
    current.pendingActions.userInput !== initial.pendingActions.userInput ||
    current.threadQuality !== initial.threadQuality
  );
}

function asRecord(value: unknown): Record<string, any> | null {
  return typeof value === "object" && value !== null ? (value as Record<string, any>) : null;
}

function safeErrorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function delay(milliseconds: number): Promise<void> {
  return new Promise((resolvePromise) => setTimeout(resolvePromise, milliseconds));
}

export function makeGateway(config: GatewayConfig): { readonly gateway: T3Gateway; readonly client: T3HttpClient; readonly journal: OperationJournal } {
  const client = new T3HttpClient(config.t3HttpBaseUrl, config.t3AccessToken);
  const journal = new OperationJournal(`${config.dataDir}/operations.json`);
  return { gateway: new T3Gateway(client, journal, config), client, journal };
}
