import { chmod, mkdir, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";

export interface SetupOptions {
  readonly workspaceDir: string;
  readonly nodeBin: string;
  readonly tunnelBin: string;
  readonly tunnelId: string;
  readonly environmentId: string;
  readonly environmentLabel: string;
  readonly host: string;
  readonly port: number;
  readonly tunnelHealthPort: number;
  readonly dataDir: string;
  readonly outDir: string;
  readonly envPath: string;
}

export function parseSetupArgs(argv: ReadonlyArray<string>): SetupOptions {
  const get = (name: string, fallback?: string): string => {
    const prefix = `--${name}=`;
    const found = argv.find((arg) => arg.startsWith(prefix));
    if (found) return found.slice(prefix.length);
    if (fallback !== undefined) return fallback;
    throw new Error(`setup requires --${name}=<value>.`);
  };
  const workspaceDir = resolve(get("workspace", process.cwd()));
  return {
    workspaceDir,
    nodeBin: get("node", "/usr/bin/node"),
    tunnelBin: get("tunnel-bin", "/usr/local/bin/tunnel-client"),
    tunnelId: get("tunnel-id"),
    environmentId: get("environment-id"),
    environmentLabel: get("environment-label", "coding"),
    host: get("host", "127.0.0.1"),
    port: Number(get("port", "8787")),
    tunnelHealthPort: Number(get("tunnel-health-port", "8080")),
    dataDir: get("data-dir", join(workspaceDir, "data")),
    outDir: resolve(get("out", `${process.env.HOME ?? "~"}/.config/systemd/user`)),
    envPath: resolve(get("env-path", `${process.env.HOME ?? "~"}/.config/t3-code-mcp.env`)),
  };
}

export async function renderSetup(options: SetupOptions): Promise<{ readonly files: ReadonlyArray<string> }> {
  if (!Number.isSafeInteger(options.port) || options.port < 1) throw new Error("setup --port must be a positive integer.");
  if (!Number.isSafeInteger(options.tunnelHealthPort) || options.tunnelHealthPort < 1) {
    throw new Error("setup --tunnel-health-port must be a positive integer.");
  }
  const gatewayService = [
    "[Unit]",
    "Description=T3 Code MCP gateway",
    "Wants=network-online.target",
    "After=network-online.target",
    "StartLimitIntervalSec=1min",
    "StartLimitBurst=5",
    "",
    "[Service]",
    "Type=simple",
    `WorkingDirectory=${options.workspaceDir}`,
    `EnvironmentFile=${options.envPath}`,
    `ExecStart=${options.nodeBin} ${join(options.workspaceDir, "dist/cli.js")} serve`,
    "Restart=on-failure",
    "RestartSec=5s",
    "TimeoutStopSec=10s",
    "PrivateTmp=yes",
    "ProtectSystem=full",
    "ProtectKernelTunables=yes",
    "ProtectControlGroups=yes",
    "RestrictSUIDSGID=yes",
    "NoNewPrivileges=yes",
    "UMask=0077",
    "",
    "[Install]",
    "WantedBy=default.target",
    "",
  ].join("\n");

  const tunnelService = [
    "[Unit]",
    "Description=OpenAI Secure MCP Tunnel for T3 Code",
    "Requires=t3-code-mcp.service",
    "Wants=network-online.target",
    "After=t3-code-mcp.service network-online.target",
    "StartLimitIntervalSec=1min",
    "StartLimitBurst=5",
    "",
    "[Service]",
    "Type=simple",
    `WorkingDirectory=${options.workspaceDir}`,
    `EnvironmentFile=${options.envPath}`,
    `ExecStart=${join(options.workspaceDir, "deploy/systemd/t3-code-mcp-tunnel.sh")}`,
    "Restart=on-failure",
    "RestartSec=10s",
    "TimeoutStopSec=20s",
    "PrivateTmp=yes",
    "ProtectSystem=full",
    "ProtectKernelTunables=yes",
    "ProtectControlGroups=yes",
    "RestrictSUIDSGID=yes",
    "NoNewPrivileges=yes",
    "UMask=0077",
    "",
    "[Install]",
    "WantedBy=default.target",
    "",
  ].join("\n");

  const tunnelLauncher = [
    "#!/bin/sh",
    "",
    "set -eu",
    "",
    ": \"${MCP_BEARER_TOKEN:?MCP_BEARER_TOKEN must be set}\"",
    ": \"${CONTROL_PLANE_API_KEY:?CONTROL_PLANE_API_KEY must be set}\"",
    "",
    "# tunnel-client resolves an entire header value from an env: reference. Build",
    "# the scheme-qualified value at launch so it stays synchronized with the",
    "# gateway token without putting the token in ExecStart or a second secret file.",
    "MCP_AUTH_HEADER=\"Bearer ${MCP_BEARER_TOKEN}\"",
    "export MCP_AUTH_HEADER",
    "",
    "attempt=0",
    `while [ "$attempt" -lt 60 ]; do`,
    `  if /usr/bin/curl --silent --fail --max-time 2 http://${options.host}:${options.port}/healthz >/dev/null 2>&1; then`,
    "    break",
    "  fi",
    "  attempt=$((attempt + 1))",
    "  /usr/bin/sleep 1",
    "done",
    `if [ "$attempt" -ge 60 ]; then`,
    `  echo "gateway health check did not become ready" >&2`,
    "  exit 1",
    "fi",
    "",
    `exec ${options.tunnelBin} run \\`,
    `  --control-plane.tunnel-id=${options.tunnelId} \\`,
    "  --control-plane.api-key=env:CONTROL_PLANE_API_KEY \\",
    `  --mcp.server-url=url=http://${options.host}:${options.port}/mcp,channel=main \\`,
    "  '--mcp.extra-headers=Authorization: env:MCP_AUTH_HEADER' \\",
    "  '--mcp.discovery-extra-headers=Authorization: env:MCP_AUTH_HEADER' \\",
    `  --health.listen-addr=${options.host}:${options.tunnelHealthPort}`,
    "",
  ].join("\n");

  const envTemplate = [
    "# Generated by `node dist/cli.js setup`. Keep credentials server-side with mode 600.",
    "# Never commit this file or paste tokens into voice prompts.",
    "",
    `T3_HTTP_BASE_URL=http://127.0.0.1:3773`,
    `T3_ENVIRONMENT_ID=${options.environmentId}`,
    `T3_ENVIRONMENT_LABEL=${options.environmentLabel}`,
    `T3_ACCESS_TOKEN=`,
    "",
    "# A separate, randomly generated gateway token. Do not reuse T3_ACCESS_TOKEN.",
    `MCP_BEARER_TOKEN=`,
    "",
    "# Start read-only for the first connection check; false enables control tools.",
    `MCP_READ_ONLY=true`,
    `MCP_TRANSPORT=http`,
    `MCP_HOST=${options.host}`,
    `MCP_PORT=${options.port}`,
    `T3_MCP_DATA_DIR=${options.dataDir}`,
    "",
    "# OpenAI Secure MCP Tunnel runtime key. This is not an admin key.",
    `CONTROL_PLANE_API_KEY=`,
    "",
  ].join("\n");

  await mkdir(options.outDir, { recursive: true, mode: 0o700 });
  const gatewayPath = join(options.outDir, "t3-code-mcp.service");
  const tunnelPath = join(options.outDir, "t3-code-mcp-tunnel.service");
  // Launcher and env template are written to the workspace deploy dir for review,
  // never with secrets. Actual secrets live only in the operator-owned env file.
  const launcherDir = join(options.workspaceDir, "deploy/systemd");
  await mkdir(launcherDir, { recursive: true });
  const launcherPath = join(launcherDir, "t3-code-mcp-tunnel.sh.generated");
  await writeFile(gatewayPath, gatewayService, { mode: 0o644 });
  await writeFile(tunnelPath, tunnelService, { mode: 0o644 });
  await writeFile(launcherPath, tunnelLauncher, { mode: 0o755 });

  // Write env template only when missing; never overwrite existing credentials.
  let envWritten = false;
  try {
    await writeFile(options.envPath, envTemplate, { flag: "wx", mode: 0o600 });
    envWritten = true;
  } catch (error) {
    if (typeof error === "object" && error !== null && "code" in error && error.code !== "EEXIST") throw error;
  }
  if (envWritten) await chmod(options.envPath, 0o600);

  return { files: envWritten ? [gatewayPath, tunnelPath, launcherPath, options.envPath] : [gatewayPath, tunnelPath, launcherPath] };
}
