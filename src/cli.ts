import { loadConfig } from "./config.js";
import { makeGateway } from "./gateway.js";
import { listenHttp, runStdio } from "./mcp/transport.js";

async function main(): Promise<void> {
  const command = process.argv[2] ?? "serve";
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
      throw new Error(`Unknown command ${command}. Use serve, status, or spike.`);
  }
}

main().catch((error: unknown) => {
  console.error(error instanceof Error ? error.message : String(error));
  process.exitCode = 1;
});
