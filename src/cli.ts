import { loadConfig } from "./config.js";
import { runDoctor } from "./doctor.js";
import { makeGateway } from "./gateway.js";
import { listenHttp, runStdio } from "./mcp/transport.js";
import { parseSetupArgs, renderSetup } from "./setup.js";

async function main(): Promise<void> {
  const command = process.argv[2] ?? "serve";
  if (command === "setup") {
    const options = parseSetupArgs(process.argv.slice(3));
    const result = await renderSetup(options);
    console.log(JSON.stringify({ ok: true, files: result.files }, null, 2));
    console.error(
      "Setup rendered without secrets. Edit the env file to set T3_ACCESS_TOKEN, MCP_BEARER_TOKEN, and CONTROL_PLANE_API_KEY (mode 600), then enable the user services.",
    );
    return;
  }
  const config = loadConfig();
  const { gateway } = makeGateway(config);

  switch (command) {
    case "serve": {
      const transport = process.env.MCP_TRANSPORT?.trim() || "stdio";
      if (transport === "stdio") {
        await runStdio(gateway);
        return;
      }
      if (transport !== "http") {
        throw new Error(`MCP_TRANSPORT must be stdio or http, received ${transport}.`);
      }
      const server = await listenHttp(gateway, config);
      console.error(`t3-code-mcp listening on http://${config.host}:${config.port}/mcp`);
      await new Promise<void>((resolve) => {
        const stop = () => {
          server.close(() => resolve());
        };
        process.once("SIGINT", stop);
        process.once("SIGTERM", stop);
      });
      return;
    }
    case "status": {
      console.log(JSON.stringify(await gateway.connectionStatus(), null, 2));
      return;
    }
    case "doctor": {
      const result = await runDoctor(gateway, config);
      console.log(JSON.stringify(result, null, 2));
      if (!result.ok) process.exitCode = 1;
      return;
    }
    case "smoke": {
      // Deployment smoke: same read path the voice client uses, through the gateway.
      // Run after every interface deployment, then repeat from the actual client.
      const status = await gateway.connectionStatus();
      const overview = await gateway.threadsOverview({ includeArchived: false, runningLimit: 5 });
      const providers = await gateway.providersList();
      const bounded = overview.highlights.length <= 5 &&
        overview.highlights.every((h) => (h.latestResponseExcerpt ?? "").length <= 201);
      console.log(JSON.stringify({ status, overview, providers: { options: providers.options.length } }, null, 2));
      if (!bounded) throw new Error("Smoke failed: overview highlights exceed bounds.");
      console.error(
        `Smoke ok: fingerprint=${status.toolSchemaFingerprint} total=${overview.total} needsAttention=${overview.needsAttentionCount}. ` +
          `Next: force fresh client discovery and ask "What's running and does anything need me?" by voice.`,
      );
      return;
    }
    case "spike": {
      const status = await gateway.connectionStatus();
      const projects = await gateway.projectsList({ limit: 5 });
      console.log(JSON.stringify({ status, projects: projects.page }, null, 2));
      if (process.env.T3_SPIKE_ENABLE_MUTATIONS !== "true") {
        console.error(
          "Read-only connection spike complete. Set T3_SPIKE_ENABLE_MUTATIONS=true and T3_SPIKE_CONFIRM=I_UNDERSTAND to create a thread and submit a prompt.",
        );
        return;
      }
      if (process.env.T3_SPIKE_CONFIRM !== "I_UNDERSTAND") {
        throw new Error("Mutation spike requires T3_SPIKE_CONFIRM=I_UNDERSTAND.");
      }
      const projectId = process.env.T3_SPIKE_PROJECT_ID?.trim();
      const prompt = process.env.T3_SPIKE_PROMPT?.trim();
      if (!projectId || !prompt) {
        throw new Error("Mutation spike requires T3_SPIKE_PROJECT_ID and T3_SPIKE_PROMPT.");
      }
      const title = process.env.T3_SPIKE_THREAD_TITLE?.trim() || "t3-code-mcp connection spike";
      const keyPrefix = process.env.T3_SPIKE_IDEMPOTENCY_KEY?.trim() || `spike-${Date.now()}`;
      const thread = await gateway.threadCreate({
        projectId,
        title,
        idempotencyKey: `${keyPrefix}-thread`,
      });
      const run = await gateway.threadSend({
        threadId: thread.threadId,
        message: prompt,
        idempotencyKey: `${keyPrefix}-turn`,
      });
      console.log(JSON.stringify({ thread, run }, null, 2));
      return;
    }
    default:
      throw new Error(`Unknown command ${command}. Use serve, status, doctor, smoke, spike, or setup.`);
  }
}

main().catch((error: unknown) => {
  console.error(error instanceof Error ? error.message : String(error));
  process.exitCode = 1;
});
