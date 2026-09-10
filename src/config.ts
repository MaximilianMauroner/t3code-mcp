import { resolve } from "node:path";

export interface GatewayConfig {
  readonly t3HttpBaseUrl: string;
  readonly t3AccessToken: string;
  readonly mcpBearerToken: string | null;
  readonly readOnly: boolean;
  readonly host: string;
  readonly port: number;
  readonly environmentId: string | null;
  readonly environmentLabel: string | null;
  readonly dataDir: string;
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

export function loadConfig(env: NodeJS.ProcessEnv = process.env): GatewayConfig {
  const baseUrl = required(env.T3_HTTP_BASE_URL ?? "http://127.0.0.1:3773", "T3_HTTP_BASE_URL");
  let parsedBaseUrl: URL;
  try {
    parsedBaseUrl = new URL(baseUrl);
  } catch {
    throw new Error("T3_HTTP_BASE_URL must be a valid URL.");
  }
  if (parsedBaseUrl.protocol !== "http:" && parsedBaseUrl.protocol !== "https:") {
    throw new Error("T3_HTTP_BASE_URL must use http or https.");
  }

  return {
    t3HttpBaseUrl: parsedBaseUrl.toString().replace(/\/$/, ""),
    t3AccessToken: required(env.T3_ACCESS_TOKEN, "T3_ACCESS_TOKEN"),
    mcpBearerToken: optional(env.MCP_BEARER_TOKEN),
    readOnly: boolean(env.MCP_READ_ONLY, false, "MCP_READ_ONLY"),
    host: env.MCP_HOST?.trim() || "127.0.0.1",
    port: integer(env.MCP_PORT, 8787, "MCP_PORT"),
    environmentId: optional(env.T3_ENVIRONMENT_ID),
    environmentLabel: optional(env.T3_ENVIRONMENT_LABEL),
    dataDir: resolve(env.T3_MCP_DATA_DIR?.trim() || "./data"),
    staleAfterMs: integer(env.T3_STALE_AFTER_MS, 30_000, "T3_STALE_AFTER_MS"),
  };
}
