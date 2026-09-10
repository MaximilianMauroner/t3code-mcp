import { randomUUID } from "node:crypto";
import { threadStatus, type ThreadStatus } from "./t3/thread-state.js";
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
  type ThreadTurnInterruptCommand,
  type ThreadTurnStartCommand,
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
  readonly supportedCapabilities: ReadonlyArray<string>;
  readonly permittedOperations: ReadonlyArray<string>;
  readonly t3Scopes: ReadonlyArray<string>;
  readonly gatewayOperations: ReadonlyArray<string>;
  readonly error?: string;
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
  readonly title: string;
  readonly modelSelection: ModelSelection;
  readonly runtimeMode: RuntimeMode;
  readonly interactionMode: InteractionMode;
  readonly branch: string | null;
  readonly worktreePath: string | null;
  readonly latestTurn: LatestTurn | null;
  readonly sessionStatus: string | null;
  readonly status: ThreadStatus;
  readonly settledOverride: "settled" | "active" | null;
  readonly settledAt: string | null;
  readonly snoozedUntil: string | null;
  readonly pinnedAt: string | null;
  readonly hasActionableProposedPlan: boolean;
  readonly backgroundLiveness: "working" | "monitoring" | null;
  readonly archivedAt: string | null;
  readonly createdAt: string | null;
  readonly updatedAt: string | null;
  readonly hasPendingApprovals: boolean;
  readonly hasPendingUserInput: boolean;
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

const GATEWAY_OPERATIONS = [
  "t3_connection_status",
  "t3_projects_list",
  "t3_project_create",
  "t3_threads_list",
  "t3_thread_create",
  "t3_thread_get",
  "t3_thread_messages",
  "t3_thread_send",
  "t3_run_get",
  "t3_run_wait",
  "t3_run_interrupt",
  "t3_pending_actions_list",
  "t3_pending_action_respond",
  "t3_thread_archive",
  "t3_thread_interrupt",
] as const;

const MUTATING_GATEWAY_OPERATIONS = new Set<string>([
  "t3_project_create",
  "t3_thread_create",
  "t3_thread_send",
  "t3_run_interrupt",
  "t3_pending_action_respond",
  "t3_thread_archive",
  "t3_thread_interrupt",
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
    let failure: string | null = null;
    try {
      descriptor = await this.client.getDescriptor();
      this.assertEnvironment(descriptor);
      const session = await this.client.getSession();
      sessionScopes = session.scopes ?? [];
      await this.client.getShell();
    } catch (error) {
      failure = safeErrorMessage(error);
      descriptor = this.client.getCachedDescriptor();
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

    return {
      environment,
      connectionStatus: failure === null ? "connected" : "disconnected",
      stateFreshness: freshness(lastObservedAt, this.config.staleAfterMs),
      lastObservedAt,
      supportedCapabilities: descriptor ? Object.keys(descriptor.capabilities).sort() : [],
      permittedOperations: permittedOperations(sessionScopes),
      t3Scopes: sessionScopes,
      gatewayOperations: this.config.readOnly
        ? GATEWAY_OPERATIONS.filter((operation) => !MUTATING_GATEWAY_OPERATIONS.has(operation))
        : [...GATEWAY_OPERATIONS],
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
    readonly cursor?: string;
    readonly limit: number;
  }): Promise<{ readonly environmentId: string; readonly page: Page<ThreadSummary> }> {
    const shell = await this.client.getShell();
    const now = Date.now();
    const filtered = shell.threads.filter((thread) => {
      if (input.projectId !== undefined && thread.projectId !== input.projectId) return false;
      if (!matchesQuery(input.query, thread.title, thread.branch, thread.id)) return false;
      if (input.status !== undefined && input.status !== "all") {
        return threadStatus(thread, now) === input.status;
      }
      return input.includeArchived || !thread.archivedAt;
    });
    const page = paginate(filtered.map((thread) => threadSummary(thread, now)), input.cursor, input.limit);
    return { environmentId: await this.environmentId(), page };
  }

  async threadGet(threadId: string): Promise<{ readonly environmentId: string; readonly thread: ThreadDetail }> {
    const snapshot = await this.client.getThread(threadId);
    // Full thread snapshots omit the shell's pending flags and latest-user timestamp.
    const shell = await this.client.getShell();
    const summary = shell.threads.find((thread) => thread.id === threadId);
    return { environmentId: await this.environmentId(), thread: threadDetail(snapshot.thread, summary) };
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
      return this.threadCreateResult(reconciled, input.projectId, input.branch ?? null, input.worktreePath ?? null);
    }
    await this.requireOperationScope();
    const project = await this.findProject(input.projectId);
    const modelSelection = input.modelSelection ?? project.defaultModelSelection;
    if (modelSelection === null || modelSelection === undefined) {
      throw new GatewayError(
        "model_selection_required",
        "The project has no default model. Supply modelSelection from the connected environment.",
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
      return this.threadCreateResult(reconciled, input.projectId, input.branch ?? null, input.worktreePath ?? null);
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
    return this.threadCreateResult(result, input.projectId, command.branch, command.worktreePath);
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
      throw new GatewayError(
        "thread_busy",
        "The thread already has an active turn. Queueing and steering are not enabled in this gateway version.",
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

  async threadInterrupt(input: ThreadInterruptInput): Promise<MutationResult> {
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
      return mutationResult(begun.record, await this.knownEnvironmentId());
    }
    await this.requireOperationScope();
    // Check identity before dispatch; a misconfigured endpoint must never stop work.
    const environmentId = await this.environmentId();
    const { thread } = await this.client.getThread(input.threadId);
    if (thread.latestTurn?.turnId !== input.expectedTurnId) {
      throw new GatewayError("turn_changed", "The thread's latest turn changed. Read the thread again before interrupting.");
    }
    if (thread.latestTurn.state !== "running") {
      throw new GatewayError("thread_not_running", "The observed turn is no longer running.");
    }
    const begun = await this.journal.begin(beginInput);
    if (begun.reused) return mutationResult(begun.record, environmentId);
    const result = await this.dispatchNew(begun.record, {
      type: "thread.turn.interrupt",
      commandId: begun.record.commandId,
      threadId: input.threadId,
      turnId: input.expectedTurnId,
      createdAt: new Date().toISOString(),
    });
    return mutationResult(result, environmentId);
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
        lastError: safeErrorMessage(error),
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
        lastObservedAt: new Date().toISOString(),
        latestResponse: latestAssistant(thread.messages, turnId),
        pendingActions: {
          approvals: thread.hasPendingApprovals ?? false,
          userInput: thread.hasPendingUserInput ?? false,
        },
      };
    } catch (error) {
      const telemetry = this.client.telemetry();
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
      throw new GatewayError("project_not_found", `Project ${projectId} was not found.`);
    }
    return project;
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
    branch: string | null,
    worktreePath: string | null,
  ): Promise<ThreadCreateResult> {
    return {
      ...mutationResult(record, await this.knownEnvironmentId()),
      projectId,
      threadId: record.threadId ?? "unknown",
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
    reason: record.lastError ?? "The command outcome could not be reconciled without replaying it.",
  };
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

function threadSummary(thread: ThreadShell, now = Date.now()): ThreadSummary {
  const session = asRecord(thread.session);
  return {
    id: thread.id,
    projectId: thread.projectId,
    title: thread.title,
    modelSelection: thread.modelSelection,
    runtimeMode: thread.runtimeMode,
    interactionMode: thread.interactionMode ?? "default",
    branch: thread.branch ?? null,
    worktreePath: thread.worktreePath ?? null,
    latestTurn: thread.latestTurn ?? null,
    sessionStatus: typeof session?.status === "string" ? session.status : null,
    status: threadStatus(thread, now),
    settledOverride: thread.settledOverride ?? null,
    settledAt: thread.settledAt ?? null,
    snoozedUntil: thread.snoozedUntil ?? null,
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

function threadDetail(thread: Thread, summary: ThreadShell = thread): ThreadDetail {
  return {
    ...threadSummary(summary),
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
    current.pendingActions.userInput !== initial.pendingActions.userInput
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
