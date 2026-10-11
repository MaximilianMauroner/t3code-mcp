import type { ModelSelection } from "./types.js";

export type RuntimeMode = "approval-required" | "auto-accept-edits" | "auto" | "full-access";
export type InteractionMode = "default" | "plan";

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

export type T3Command =
  | ThreadCreateCommand
  | ThreadTurnStartCommand
  | ThreadApprovalResponseCommand
  | ThreadUserInputResponseCommand;
