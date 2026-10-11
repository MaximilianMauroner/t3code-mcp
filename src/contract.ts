import { createHash } from "node:crypto";

// Single source for build identity and contract version.
// GATEWAY_COMMIT is injected at build/deploy time via T3_CODE_MCP_COMMIT.
// Falls back to "unknown" for local dev without an injected commit.
export const GATEWAY_VERSION = "0.1.0";
export const GATEWAY_COMMIT = (process.env.T3_CODE_MCP_COMMIT ?? "").trim() || "unknown";

// Bump when any MCP output shape changes in a way a stale client must detect.
export const TOOL_SCHEMA_VERSION = 10;

/** Tools this gateway implements itself. */
export const GATEWAY_TOOL_NAMES = [
  "t3_connection_status",
  "t3_audit_log",
  "t3_git_status",
  "t3_git_diff",
  "t3_git_compare",
  "t3_threads_list",
  "t3_threads_overview",
  "t3_pending_actions_list",
  "t3_pending_action_respond",
  "t3_attachment_upload",
  "t3_task_start",
  "t3_task_get",
  "t3_tasks_list",
  "t3_task_cancel",
  "t3_result_get",
] as const;

// Native T3 MCP tools forwarded unchanged. The allowlist excludes tools that
// need a calling T3 thread, which an outside client never has, and native
// pending-request tools, which the gateway's pending tools supersede. Native
// annotations are incomplete (t3_thread_read is not marked read-only), so the
// gateway keeps its own read/mutate split for read-only enforcement.
export const NATIVE_READ_TOOL_NAMES = [
  "t3_project_list",
  "t3_project_read",
  "t3_thread_list",
  "t3_thread_search",
  "t3_thread_read",
  "t3_thread_wait",
  "t3_thread_configuration",
  "t3_thread_transfers",
  "t3_queue_list",
  "t3_queue_read",
  "list_scheduled_tasks",
  "list_thread_pull_requests",
  "read_pull_request_guide",
  "t3_worktree_list",
  "orchestrator_capabilities",
] as const;

export const NATIVE_MUTATING_TOOL_NAMES = [
  "t3_project_create",
  "t3_project_update",
  "t3_project_clone",
  "t3_project_delete",
  "t3_thread_launch",
  "t3_thread_send",
  "t3_thread_interrupt",
  "t3_thread_update",
  "t3_thread_organize",
  "t3_thread_configure",
  "t3_thread_fork",
  "t3_thread_merge_back",
  "t3_queue_edit",
  "t3_queue_cancel",
  "t3_queue_reorder",
  "t3_queue_promote_to_steer",
  "t3_attachment_discard",
  "t3_thread_send_attachments",
  "schedule_task",
  "update_scheduled_task",
  "delete_scheduled_task",
  "run_scheduled_task_now",
  "link_pull_request",
  "unlink_pull_request",
  "watch_pull_request",
  "unwatch_pull_request",
  "write_pull_request_guides",
] as const;

export const NATIVE_TOOL_NAMES = [...NATIVE_READ_TOOL_NAMES, ...NATIVE_MUTATING_TOOL_NAMES] as const;

export type NativeToolName = (typeof NATIVE_TOOL_NAMES)[number];

/** Every tool a fully connected gateway advertises. */
export const TOOL_NAMES = [...GATEWAY_TOOL_NAMES, ...NATIVE_TOOL_NAMES] as const;

export type ToolName = (typeof TOOL_NAMES)[number];

const NATIVE_TOOLS = new Set<string>(NATIVE_TOOL_NAMES);
const NATIVE_MUTATING_TOOLS = new Set<string>(NATIVE_MUTATING_TOOL_NAMES);

export function isNativeToolName(name: string): name is NativeToolName {
  return NATIVE_TOOLS.has(name);
}

export function isNativeMutatingTool(name: string): boolean {
  return NATIVE_MUTATING_TOOLS.has(name);
}

export function toolSchemaFingerprint(): string {
  const canonical = JSON.stringify({
    version: GATEWAY_VERSION,
    schemaVersion: TOOL_SCHEMA_VERSION,
    tools: [...TOOL_NAMES].sort(),
  });
  return createHash("sha256").update(canonical).digest("hex").slice(0, 16);
}

export function buildId(): string {
  return `${GATEWAY_VERSION}+${GATEWAY_COMMIT}`;
}
