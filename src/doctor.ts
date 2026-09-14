import { access, constants, stat } from "node:fs/promises";
import type { GatewayConfig } from "./config.js";
import { TOOL_NAMES, toolSchemaFingerprint } from "./contract.js";
import type { T3Gateway } from "./gateway.js";
import { createMcpServer } from "./mcp/server.js";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";

export interface DoctorCheck {
  readonly name: string;
  readonly ok: boolean;
  readonly detail: string;
}

export interface DoctorResult {
  readonly ok: boolean;
  readonly checks: ReadonlyArray<DoctorCheck>;
  readonly manifest: CapabilityManifest | null;
}

export interface CapabilityManifest {
  readonly gatewayVersion: string;
  readonly gatewayCommit: string;
  readonly toolSchemaFingerprint: string;
  readonly declaredOperations: ReadonlyArray<string>;
  readonly locallyDiscoveredOperations: ReadonlyArray<string>;
  readonly callableOperations: ReadonlyArray<string>;
  readonly disabledOperations: ReadonlyArray<{ readonly operation: string; readonly reasonCode: string }>;
  readonly effectiveAccessMode: "read-only" | "read-write";
  readonly upstreamScopes: ReadonlyArray<string>;
  readonly hostDiscoveredOperations: ReadonlyArray<string> | null;
}

async function canWrite(dir: string): Promise<boolean> {
  try {
    await access(dir, constants.W_OK);
    return true;
  } catch {
    return false;
  }
}

export async function runDoctor(
  gateway: T3Gateway,
  config: GatewayConfig,
  options: { readonly tunnelHealthUrl?: string; readonly hostToolNames?: ReadonlyArray<string> } = {},
): Promise<DoctorResult> {
  const checks: DoctorCheck[] = [];
  let manifest: CapabilityManifest | null = null;
  checks.push({
    name: "config",
    ok: true,
    detail: `readOnly=${config.readOnly} host=${config.host} port=${config.port} dataDir=${config.dataDir} staleAfterMs=${config.staleAfterMs}`,
  });

  try {
    const mode = (await stat(config.dataDir).catch(() => null))?.mode;
    const writable = await canWrite(config.dataDir).catch(() => false);
    // Journal path writability without mutating the journal itself.
    checks.push({
      name: "journal",
      ok: writable,
      detail: writable
        ? `dataDir ${config.dataDir} writable${mode !== undefined ? ` mode=${(mode & 0o777).toString(8)}` : ""}`
        : `dataDir ${config.dataDir} not writable; preserve T3_MCP_DATA_DIR across restarts and run one gateway per directory`,
    });
  } catch (error) {
    checks.push({ name: "journal", ok: false, detail: error instanceof Error ? error.message : String(error) });
  }

  let fingerprintOk = false;
  try {
    const status = await gateway.connectionStatus();
    const expiry = status.sessionExpiresAt ? Date.parse(status.sessionExpiresAt) : Number.NaN;
    const expiryNote = Number.isFinite(expiry)
      ? Date.now() > expiry
        ? ` credential EXPIRED at ${status.sessionExpiresAt}`
        : expiry - Date.now() < 7 * 24 * 3600_000
          ? ` credential expires soon at ${status.sessionExpiresAt}`
          : ` credential expires at ${status.sessionExpiresAt}`
      : "";
    checks.push({
      name: "t3_identity",
      ok: status.connectionStatus === "connected",
      detail: `env=${status.environment?.environmentId ?? "unknown"} version=${status.environment?.serverVersion ?? "unknown"} scopes=[${status.upstreamScopes.join(",")}]${expiryNote}${status.error ? ` error=${status.error}` : ""}`,
    });
    checks.push({
      name: "effective_access",
      ok: true,
      detail: `mode=${status.effectiveAccessMode} callable=${status.callableOperations.length} disabled=${status.disabledOperations.map((d) => `${d.operation}:${d.reasonCode}`).join(",") || "none"}`,
    });
    checks.push({
      name: "freshness",
      ok: status.stateFreshness !== "unknown",
      detail: `freshness=${status.stateFreshness} lastObservedAt=${status.lastObservedAt ?? "never"} observedAt=${status.observedAt}`,
    });
    checks.push({
      name: "build",
      ok: status.toolSchemaFingerprint === toolSchemaFingerprint(),
      detail: `version=${status.gatewayVersion} commit=${status.gatewayCommit} fingerprint=${status.toolSchemaFingerprint}`,
    });
    fingerprintOk = status.toolSchemaFingerprint === toolSchemaFingerprint();
    manifest = {
      gatewayVersion: status.gatewayVersion,
      gatewayCommit: status.gatewayCommit,
      toolSchemaFingerprint: status.toolSchemaFingerprint,
      declaredOperations: [...TOOL_NAMES],
      locallyDiscoveredOperations: [],
      callableOperations: [...status.callableOperations],
      disabledOperations: status.disabledOperations.map(({ operation, reasonCode }) => ({ operation, reasonCode })),
      effectiveAccessMode: status.effectiveAccessMode,
      upstreamScopes: [...status.upstreamScopes],
      hostDiscoveredOperations: options.hostToolNames ? [...options.hostToolNames].sort() : null,
    };
    void fingerprintOk;
  } catch (error) {
    checks.push({
      name: "t3_identity",
      ok: false,
      detail: error instanceof Error ? error.message : String(error),
    });
  }

  try {
    // MCP discovery without mutation: exercise a real tools/list exchange against this build.
    const server = createMcpServer(gateway);
    const client = new Client({ name: "t3-code-mcp-doctor", version: "1.0.0" }, { capabilities: {} });
    const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
    await server.connect(serverTransport);
    await client.connect(clientTransport);
    const discovered = (await client.listTools()).tools.map((tool) => tool.name).sort();
    await client.close();
    await server.close();
    const expected = [...TOOL_NAMES].sort();
    const localMatch = JSON.stringify(discovered) === JSON.stringify(expected);
    if (manifest) manifest = { ...manifest, locallyDiscoveredOperations: discovered };
    checks.push({
      name: "mcp_discovery",
      ok: localMatch,
      detail: `declared=${expected.length} discovered=${discovered.length} fingerprint=${toolSchemaFingerprint()}`,
    });
  } catch (error) {
    checks.push({
      name: "mcp_discovery",
      ok: false,
      detail: error instanceof Error ? error.message : String(error),
    });
  }

  if (options.hostToolNames) {
    const expected = [...TOOL_NAMES].sort();
    const observed = [...new Set(options.hostToolNames)].sort();
    const missing = expected.filter((name) => !observed.includes(name));
    const unexpected = observed.filter((name) => !expected.includes(name as (typeof TOOL_NAMES)[number]));
    checks.push({
      name: "host_discovery",
      ok: missing.length === 0 && unexpected.length === 0,
      detail: `observed=${observed.length} missing=[${missing.join(",")}] unexpected=[${unexpected.join(",")}]`,
    });
  }

  try {
    const overview = await gateway.threadsOverview({ includeArchived: false, runningLimit: 5 });
    const bounded = overview.highlights.length <= 5 &&
      overview.highlights.every((h) => (h.latestResponseExcerpt ?? "").length <= 201);
    checks.push({
      name: "overview_read",
      ok: bounded,
      detail: `total=${overview.total} needsAttention=${overview.needsAttentionCount} highlights=${overview.highlights.length} observedAt=${overview.observedAt}`,
    });
  } catch (error) {
    checks.push({
      name: "overview_read",
      ok: false,
      detail: error instanceof Error ? error.message : String(error),
    });
  }

  const tunnelUrl = options.tunnelHealthUrl ?? "http://127.0.0.1:8080/readyz";
  try {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 2000);
    const response = await fetch(tunnelUrl, { signal: controller.signal }).catch(() => null);
    clearTimeout(timer);
    checks.push({
      name: "tunnel",
      ok: response?.ok === true,
      detail: response?.ok === true ? `tunnel ready at ${tunnelUrl}` : `tunnel not ready at ${tunnelUrl}; start t3-code-mcp-tunnel.service`,
    });
  } catch (error) {
    checks.push({ name: "tunnel", ok: false, detail: error instanceof Error ? error.message : String(error) });
  }

  const ok = checks.filter((c) => c.name !== "tunnel").every((c) => c.ok);
  return { ok, checks, manifest };
}
