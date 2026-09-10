import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { type GatewayConfig } from "../src/config.js";
import { makeGateway } from "../src/gateway.js";

const liveEnabled = process.env.T3_LIVE_TESTS === "1";
const liveToken = process.env.T3_LIVE_ACCESS_TOKEN?.trim();
const liveDescribe = describe.skipIf(!liveEnabled || !liveToken);
const liveDirectories: string[] = [];

afterEach(async () => {
  await Promise.all(liveDirectories.splice(0).map((directory) => rm(directory, { recursive: true, force: true })));
});

liveDescribe("live T3 interface acceptance", () => {
  it("uses the configured remote environment and Codex Pro Spark for a durable run", async () => {
    const directory = await mkdtemp(join(tmpdir(), "t3-code-mcp-live-"));
    liveDirectories.push(directory);
    const config: GatewayConfig = {
      t3HttpBaseUrl: process.env.T3_LIVE_HTTP_BASE_URL?.trim() || "http://127.0.0.1:3773",
      t3AccessToken: liveToken!,
      mcpBearerToken: null,
      readOnly: false,
      host: "127.0.0.1",
      port: 0,
      environmentId: process.env.T3_LIVE_ENVIRONMENT_ID?.trim() || null,
      environmentLabel: null,
      dataDir: directory,
      staleAfterMs: 30_000,
    };
    const first = makeGateway(config);
    let threadId: string | null = null;
    try {
      const status = await first.gateway.connectionStatus();
      expect(status.connectionStatus).toBe("connected");
      expect(status.environment).not.toBeNull();

      const projects = await first.gateway.projectsList({ limit: 100 });
      const projectId = process.env.T3_LIVE_PROJECT_ID?.trim() || projects.page.items[0]?.id;
      expect(projectId, "T3_LIVE_PROJECT_ID or at least one remote project is required").toBeTruthy();

      const created = await first.gateway.threadCreate({
        projectId: projectId!,
        title: "t3-code-mcp live Spark acceptance",
        modelSelection: {
          // Codex Pro provider in the installed T3 environment.
          instanceId: "codex_openai",
          model: "gpt-5.3-codex-spark",
        },
        runtimeMode: "approval-required",
        idempotencyKey: `live-thread-${Date.now()}`,
      });
      expect(created.status).toBe("accepted");
      threadId = created.threadId;

      const run = await first.gateway.threadSend({
        threadId,
        message: "Run the smallest available project check and report only the result. Do not modify files.",
        modelSelection: {
          instanceId: "codex_openai",
          model: "gpt-5.3-codex-spark",
        },
        runtimeMode: "approval-required",
        idempotencyKey: `live-turn-${Date.now()}`,
      });
      expect(run.status).toBe("accepted");
      expect(run.providerTurnId).toBeNull();

      // This second gateway represents a different MCP host after the first has disconnected.
      const second = makeGateway(config);
      let observed = await second.gateway.runGet(run.runId);
      const deadline = Date.now() + 120_000;
      while (!isTerminal(observed.runStatus) && Date.now() < deadline) {
        observed = await second.gateway.runWait(run.runId, 5);
      }

      expect(["completed", "awaiting_approval", "awaiting_input", "failed", "interrupted"]).toContain(observed.runStatus);
      expect(observed.connectionStatus).toBe("connected");
      if (observed.runStatus === "completed") {
        expect(observed.latestResponse?.role).toBe("assistant");
      }
    } finally {
      if (threadId !== null) {
        const cleanup = makeGateway(config).gateway;
        await cleanup.threadArchive({ threadId, idempotencyKey: `live-archive-${Date.now()}` }).catch(() => undefined);
      }
    }
  }, 180_000);
});

function isTerminal(status: string): boolean {
  return status === "completed" || status === "awaiting_approval" || status === "awaiting_input" || status === "failed" || status === "interrupted";
}
