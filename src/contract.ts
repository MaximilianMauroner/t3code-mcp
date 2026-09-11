import { createHash } from "node:crypto";

// Single source for build identity and contract version.
// GATEWAY_COMMIT is injected at build/deploy time via T3_CODE_MCP_COMMIT.
// Falls back to "unknown" for local dev without an injected commit.
export const GATEWAY_VERSION = "0.1.0";
export const GATEWAY_COMMIT = (process.env.T3_CODE_MCP_COMMIT ?? "").trim() || "unknown";

// Bump when any MCP output shape changes in a way a stale client must detect.
export const TOOL_SCHEMA_VERSION = 4;

export const TOOL_NAMES = [
  "t3_connection_status",
  "t3_projects_list",
  "t3_git_status",
  "t3_git_diff",
  "t3_project_create",
  "t3_threads_list",
  "t3_threads_overview",
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
  "t3_providers_list",
  "t3_thread_snooze",
  "t3_thread_unsnooze",
  "t3_thread_settle",
  "t3_thread_unsettle",
] as const;

export type ToolName = (typeof TOOL_NAMES)[number];

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
