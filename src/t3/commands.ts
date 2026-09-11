import type { ModelSelection } from "./types.js";

export type RuntimeMode = "approval-required" | "auto-accept-edits" | "auto" | "full-access";
export type InteractionMode = "default" | "plan";

export interface ProjectCreateCommand {
  readonly type: "project.create";
  readonly commandId: string;
  readonly projectId: string;
  readonly title: string;
  readonly workspaceRoot: string;
  readonly createWorkspaceRootIfMissing?: boolean;
  readonly defaultModelSelection?: ModelSelection | null;
  readonly createdAt: string;
}

export interface ThreadCreateCommand {
  readonly type: "thread.create";
  readonly commandId: string;
  readonly threadId: string;
  readonly projectId: string;
  readonly title: string;
  readonly modelSelection: ModelSelection;
  readonly runtimeMode: RuntimeMode;
  readonly interactionMode: InteractionMode;
  readonly branch: string | null;
  readonly worktreePath: string | null;
  readonly createdAt: string;
}

export interface ThreadTurnStartCommand {
  readonly type: "thread.turn.start";
  readonly commandId: string;
  readonly threadId: string;
  readonly message: {
    readonly messageId: string;
    readonly role: "user";
    readonly text: string;
    readonly attachments: readonly unknown[];
  };
  readonly modelSelection?: ModelSelection;
  readonly titleSeed?: string;
  readonly runtimeMode: RuntimeMode;
  readonly interactionMode: InteractionMode;
  readonly createdAt: string;
}

export interface ThreadTurnInterruptCommand {
  readonly type: "thread.turn.interrupt";
  readonly commandId: string;
  readonly threadId: string;
  readonly turnId?: string;
  readonly createdAt: string;
}

export interface ThreadApprovalResponseCommand {
  readonly type: "thread.approval.respond";
  readonly commandId: string;
  readonly threadId: string;
  readonly requestId: string;
  readonly decision: "accept" | "acceptForSession" | "decline" | "cancel";
  readonly createdAt: string;
}

export interface ThreadUserInputResponseCommand {
  readonly type: "thread.user-input.respond";
  readonly commandId: string;
  readonly threadId: string;
  readonly requestId: string;
  readonly answers: Record<string, unknown>;
  readonly createdAt: string;
}

export interface ThreadArchiveCommand {
  readonly type: "thread.archive";
  readonly commandId: string;
  readonly threadId: string;
}

export interface ThreadSnoozeCommand {
  readonly type: "thread.snooze";
  readonly commandId: string;
  readonly threadId: string;
  readonly snoozedUntil: string;
}

export interface ThreadUnsnoozeCommand {
  readonly type: "thread.unsnooze";
  readonly commandId: string;
  readonly threadId: string;
  readonly reason: "user";
}

export interface ThreadSettleCommand {
  readonly type: "thread.settle";
  readonly commandId: string;
  readonly threadId: string;
}

export interface ThreadUnsettleCommand {
  readonly type: "thread.unsettle";
  readonly commandId: string;
  readonly threadId: string;
  readonly reason: "user";
}

export type T3Command =
  | ProjectCreateCommand
  | ThreadCreateCommand
  | ThreadTurnStartCommand
  | ThreadTurnInterruptCommand
  | ThreadApprovalResponseCommand
  | ThreadUserInputResponseCommand
  | ThreadArchiveCommand
  | ThreadSnoozeCommand
  | ThreadUnsnoozeCommand
  | ThreadSettleCommand
  | ThreadUnsettleCommand;
