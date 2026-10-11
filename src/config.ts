import { resolve } from "node:path";

/** Settings shared by the gateway and the credential-free `native-login` command. */
export interface NativeEndpointConfig {
  readonly t3HttpBaseUrl: string;
  /** T3's own MCP endpoint. The gateway proxies native tools through it. */
  readonly t3NativeMcpUrl: string;
  readonly dataDir: string;
}

export interface GatewayConfig extends NativeEndpointConfig {
  readonly t3AccessToken: string;
  readonly mcpBearerToken: string | null;
  readonly readOnly: boolean;
  readonly host: string;
  readonly port: number;
  readonly maxBodyBytes: number;
  readonly environmentId: string | null;
  readonly environmentLabel: string | null;
  readonly worktreeRoot: string | null;
  readonly staleAfterMs: number;
}

function required(value: string | undefined, name: string): string {
  const normalized = value?.trim();
  if (!normalized) {
    throw new Error(`${name} must be set.`);
  }
  return normalized;
}

function optional(value: string | undefined): string | null {
  const normalized = value?.trim();
  return normalized ? normalized : null;
}

function integer(value: string | undefined, fallback: number, name: string): number {
  if (value === undefined || value.trim() === "") {
    return fallback;
  }
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed < 1) {
    throw new Error(`${name} must be a positive integer.`);
  }
  return parsed;
}

function boolean(value: string | undefined, fallback: boolean, name: string): boolean {
  if (value === undefined || value.trim() === "") {
    return fallback;
  }
  if (value === "1" || value.toLowerCase() === "true") {
    return true;
  }
  if (value === "0" || value.toLowerCase() === "false") {
    return false;
  }
  throw new Error(`${name} must be true or false.`);
}

function httpUrl(value: string, name: string): string {
  let parsed: URL;
  try {
    parsed = new URL(value);
  } catch {
    throw new Error(`${name} must be a valid URL.`);
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    throw new Error(`${name} must use http or https.`);
  }
  return parsed.toString().replace(/\/$/, "");
}

export function loadNativeEndpointConfig(env: NodeJS.ProcessEnv = process.env): NativeEndpointConfig {
  const t3HttpBaseUrl = httpUrl(required(env.T3_HTTP_BASE_URL ?? "http://127.0.0.1:3773", "T3_HTTP_BASE_URL"), "T3_HTTP_BASE_URL");
  const configuredNativeUrl = optional(env.T3_NATIVE_MCP_URL);
  return {
    t3HttpBaseUrl,
    t3NativeMcpUrl: configuredNativeUrl === null ? `${t3HttpBaseUrl}/mcp` : httpUrl(configuredNativeUrl, "T3_NATIVE_MCP_URL"),
    dataDir: resolve(env.T3_MCP_DATA_DIR?.trim() || "./data"),
  };
}

export function loadConfig(env: NodeJS.ProcessEnv = process.env): GatewayConfig {
  const endpoint = loadNativeEndpointConfig(env);
  const configuredWorktreeRoot = optional(env.T3_MCP_WORKTREE_ROOT);

  return {
    ...endpoint,
    t3AccessToken: required(env.T3_ACCESS_TOKEN, "T3_ACCESS_TOKEN"),
    mcpBearerToken: optional(env.MCP_BEARER_TOKEN),
    readOnly: boolean(env.MCP_READ_ONLY, false, "MCP_READ_ONLY"),
    host: env.MCP_HOST?.trim() || "127.0.0.1",
    port: integer(env.MCP_PORT, 8787, "MCP_PORT"),
    maxBodyBytes: integer(env.MCP_MAX_BODY_BYTES, 2 * 1024 * 1024, "MCP_MAX_BODY_BYTES"),
    environmentId: optional(env.T3_ENVIRONMENT_ID),
    environmentLabel: optional(env.T3_ENVIRONMENT_LABEL),
    worktreeRoot: configuredWorktreeRoot === null ? null : resolve(configuredWorktreeRoot),
    staleAfterMs: integer(env.T3_STALE_AFTER_MS, 30_000, "T3_STALE_AFTER_MS"),
  };
}
