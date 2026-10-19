import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { strict as assert } from "node:assert";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { createMcpServer } from "../src/mcp/server.js";
import { listenHttp } from "../src/mcp/transport.js";
import { gatewayFixture } from "../tests/support/gateway-fixture.js";
import { FakeT3 } from "../tests/support/fake-t3.js";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const fixturePath = join(root, "tests/fixtures/node-compat-baseline.json");
const outputDirectory = await mkdtemp(join(tmpdir(), "t3-code-mcp-baseline-"));
const fake = new FakeT3();
const temporaryPaths: string[] = [outputDirectory];
let fixture: Awaited<ReturnType<typeof gatewayFixture>> | undefined;
let httpServer: Awaited<ReturnType<typeof listenHttp>> | undefined;
let client: Client | undefined;
let mcpServer: Awaited<ReturnType<typeof createMcpServer>> | undefined;

class LineReader {
  private buffer = "";
  private readonly lines: string[] = [];
  private readonly waiters: Array<(line: string) => void> = [];

  constructor(stream: NodeJS.ReadableStream) {
    stream.setEncoding("utf8");
    stream.on("data", (chunk: string) => {
      this.buffer += chunk;
      while (true) {
        const newline = this.buffer.indexOf("\n");
        if (newline < 0) break;
        const line = this.buffer.slice(0, newline).replace(/\r$/, "");
        this.buffer = this.buffer.slice(newline + 1);
        const waiter = this.waiters.shift();
        if (waiter) waiter(line);
        else this.lines.push(line);
      }
    });
  }

  next(): Promise<string> {
    const line = this.lines.shift();
    if (line !== undefined) return Promise.resolve(line);
    return new Promise((resolveLine, reject) => {
      const timer = setTimeout(() => reject(new Error("stdio baseline response timed out")), 10_000);
      this.waiters.push((value) => {
        clearTimeout(timer);
        resolveLine(value);
      });
    });
  }
}

function normalize(value: unknown, paths: ReadonlyArray<readonly [string, string]>): unknown {
  if (Array.isArray(value)) return value.map((item) => normalize(item, paths));
  if (typeof value === "string") {
    let normalized = value;
    for (const [path, placeholder] of paths) normalized = normalized.replaceAll(path, placeholder);
    return normalized
      .replaceAll(fake.accessToken, "<T3_ACCESS_TOKEN>")
      .replaceAll("gateway-test-token", "<MCP_BEARER_TOKEN>")
      .replaceAll(fake.baseUrl, "http://127.0.0.1:<PORT>")
      .replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi, "<UUID>")
      .replace(/msg_[0-9a-f]{32}/gi, "msg_<ID>")
      .replace(/\b\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z\b/g, "<ISO_TIME>");
  }
  if (typeof value !== "object" || value === null) return value;
  return Object.fromEntries(Object.entries(value).filter(([, entry]) => entry !== undefined).map(([key, entry]) => {
    if (["durationMs", "latencyMs", "meanMs", "minMs", "maxMs", "medianUpperBoundMs", "p95UpperBoundMs", "bytes"].includes(key) && typeof entry === "number") return [key, "<NUMBER>"];
    if (key === "processId" && typeof entry === "number") return [key, "<PID>"];
    if (key === "payloadHash" && typeof entry === "string") return [key, "<HASH>"];
    if (key === "port" && typeof entry === "number" && entry !== 8787 && entry !== 8080) return [key, "<PORT_NUMBER>"];
    if (["observedAt", "createdAt", "updatedAt", "requestedAt", "startedAt", "completedAt", "expiresAt", "fetchedAt"].includes(key) && typeof entry === "string") {
      return [key, "<ISO_TIME>"];
    }
    if (key === "remoteAddress") return [key, "<ADDRESS>"];
    return [key, normalize(entry, paths)];
  }));
}

async function postMcp(baseUrl: string, id: number | undefined, method: string, params: unknown) {
  const message = id === undefined
    ? { jsonrpc: "2.0", method, params }
    : { jsonrpc: "2.0", id, method, params };
  const response = await fetch(`${baseUrl}/mcp`, {
    method: "POST",
    headers: {
      authorization: "Bearer gateway-test-token",
      accept: "application/json, text/event-stream",
      "content-type": "application/json",
    },
    body: JSON.stringify(message),
  });
  const text = await response.text();
  const headers = Object.fromEntries(["content-type", "mcp-session-id", "www-authenticate", "allow"]
    .flatMap((name) => response.headers.has(name) ? [[name, response.headers.get(name)]] : []));
  let body: unknown = null;
  if (text.length > 0) {
    try { body = JSON.parse(text); } catch { body = text; }
  }
  return { status: response.status, headers, body };
}

function runCli(args: ReadonlyArray<string>, env: NodeJS.ProcessEnv) {
  const result = spawnSync(process.execPath, ["--import", "tsx", join(root, "src/cli.ts"), ...args], {
    cwd: root,
    env,
    encoding: "utf8",
    timeout: 30_000,
  });
  if (result.error) throw result.error;
  return { status: result.status, stdout: result.stdout, stderr: result.stderr };
}

async function captureStdio(dataDir: string) {
  const child = spawn(process.execPath, ["--import", "tsx", join(root, "src/cli.ts"), "serve"], {
    cwd: root,
    env: {
      PATH: process.env.PATH,
      HOME: outputDirectory,
      T3_HTTP_BASE_URL: fake.baseUrl,
      T3_ACCESS_TOKEN: fake.accessToken,
      T3_MCP_DATA_DIR: dataDir,
      MCP_TRANSPORT: "stdio",
    },
    stdio: ["pipe", "pipe", "pipe"],
  });
  const reader = new LineReader(child.stdout);
  let stderr = "";
  child.stderr.setEncoding("utf8");
  child.stderr.on("data", (chunk: string) => { stderr += chunk; });
  const exchange = async (message: unknown) => {
    child.stdin.write(`${JSON.stringify(message)}\n`);
    return JSON.parse(await reader.next()) as unknown;
  };

  try {
    const initialize = await exchange({
      jsonrpc: "2.0",
      id: 1,
      method: "initialize",
      params: { protocolVersion: "2024-10-07", capabilities: {}, clientInfo: { name: "node-baseline", version: "1" } },
    });
    child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" })}\n`);
    const tools = await exchange({ jsonrpc: "2.0", id: 2, method: "tools/list", params: {} });
    const status = await exchange({ jsonrpc: "2.0", id: 3, method: "tools/call", params: { name: "t3_connection_status", arguments: {} } });
    return { initialize, tools, status, stderr };
  } finally {
    child.kill("SIGTERM");
    await new Promise<void>((resolveClose, reject) => {
      child.once("close", () => resolveClose());
      child.once("error", reject);
    });
  }
}

try {
  await fake.start();
  fixture = await gatewayFixture(fake);
  const { gateway, config, directory } = fixture;
  fake.addProject({ id: "project-baseline", title: "Baseline project" });
  fake.addThread({ id: "thread-baseline", projectId: "project-baseline", title: "Baseline thread" });

  mcpServer = createMcpServer(gateway);
  client = new Client({ name: "node-baseline", version: "1" }, { capabilities: {} });
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await mcpServer.connect(serverTransport);
  await client.connect(clientTransport);
  const inMemoryTools = await client.listTools();
  const inMemoryStatus = await client.callTool({ name: "t3_connection_status", arguments: {} });

  httpServer = await listenHttp(gateway, config);
  const address = httpServer.address();
  assert(address && typeof address !== "string");
  const httpBase = `http://127.0.0.1:${address.port}`;
  const http = {
    initialize: await postMcp(httpBase, 1, "initialize", {
      protocolVersion: "2024-10-07",
      capabilities: {},
      clientInfo: { name: "node-baseline", version: "1" },
    }),
    initialized: await postMcp(httpBase, undefined, "notifications/initialized", {}),
    tools: await postMcp(httpBase, 2, "tools/list", {}),
    status: await postMcp(httpBase, 3, "tools/call", { name: "t3_connection_status", arguments: {} }),
    invalidInput: await postMcp(httpBase, 4, "tools/call", { name: "t3_threads_list", arguments: { limit: 0 } }),
    health: await fetch(`${httpBase}/healthz`).then(async (response) => ({ status: response.status, body: await response.json() })),
    unauthorized: await fetch(`${httpBase}/mcp`, { method: "POST", headers: { authorization: "Bearer wrong", accept: "application/json, text/event-stream", "content-type": "application/json" }, body: "{}" }).then(async (response) => ({ status: response.status, headers: { challenge: response.headers.get("www-authenticate") }, body: await response.json() })),
  };

  const stdioDir = await mkdtemp(join(tmpdir(), "t3-code-mcp-stdio-baseline-"));
  temporaryPaths.push(stdioDir);
  const stdio = await captureStdio(stdioDir);

  const task = await gateway.taskStart({
    projectId: "project-baseline",
    title: "Baseline task",
    instruction: "Report status only.",
    runtimeMode: "approval-required",
    idempotencyKey: "baseline-task",
  });
  const operations = await readFile(join(directory, "operations.json"), "utf8").then(JSON.parse);
  const audit = await readFile(join(directory, "audit.jsonl"), "utf8").then((text) => text.trim().split("\n").map((line) => JSON.parse(line)));
  const setupRoot = await mkdtemp(join(tmpdir(), "t3-code-mcp-setup-baseline-"));
  const setupOut = join(setupRoot, "out");
  const setupHome = join(setupRoot, "home");
  const setupEnv = join(setupRoot, "config", "t3-code-mcp.env");
  temporaryPaths.push(setupRoot);
  await mkdir(setupHome, { recursive: true });
  await mkdir(dirname(setupEnv), { recursive: true });
  const cliEnv: NodeJS.ProcessEnv = { PATH: process.env.PATH, HOME: setupHome };
  const setup = runCli([
    "setup",
    `--workspace=${setupRoot}`,
    "--node=/usr/bin/node",
    "--tunnel-bin=/usr/local/bin/tunnel-client",
    "--tunnel-id=tunnel-baseline",
    "--environment-id=environment-baseline",
    `--out=${setupOut}`,
    `--env-path=${setupEnv}`,
  ], cliEnv);
  assert.equal(setup.status, 0, setup.stderr);
  const setupFiles = Object.fromEntries(await Promise.all([
    ["gatewayService", join(setupOut, "t3-code-mcp.service")],
    ["tunnelService", join(setupOut, "t3-code-mcp-tunnel.service")],
    ["tunnelLauncher", join(setupRoot, "deploy/systemd/t3-code-mcp-tunnel.sh.generated")],
    ["environment", setupEnv],
  ].map(async ([name, path]) => [name, await readFile(path as string, "utf8")])));
  const usageSummary = runCli(["usage-summary", "--file", join(directory, "audit.jsonl")], cliEnv);
  assert.equal(usageSummary.status, 0, usageSummary.stderr);
  const normalizedUsageSummary = `${JSON.stringify(normalize(JSON.parse(usageSummary.stdout) as unknown, []), null, 2)}\n`;

  const paths: Array<readonly [string, string]> = [
    [root, "<ROOT>"],
    [directory, "<DATA_DIR>"],
    [stdioDir, "<STDIO_DATA_DIR>"],
    [setupRoot, "<SETUP_ROOT>"],
    [setupOut, "<SETUP_OUT>"],
    [setupEnv, "<ENV_FILE>"],
  ];
  const toolList = normalize(http.tools.body && typeof http.tools.body === "object"
    ? (http.tools.body as { result?: { tools?: unknown } }).result?.tools
    : undefined, paths);
  assert(Array.isArray(toolList));
  assert.deepEqual(normalize(inMemoryTools.tools, paths), toolList);
  assert.deepEqual(normalize((stdio.tools as { result: { tools: unknown } }).result.tools, paths), toolList);
  const toolsPath = join(root, "tests/fixtures/node-compat-tools.json");
  await writeFile(toolsPath, `${JSON.stringify(toolList, null, 2)}\n`);
  const toolsSha256 = createHash("sha256").update(JSON.stringify(toolList)).digest("hex");
  const hash = (value: unknown) => createHash("sha256").update(JSON.stringify(normalize(value, paths))).digest("hex");
  const captured = normalize({
    mcp: {
      toolsSha256,
      inMemoryStatusSha256: hash(inMemoryStatus),
      http: {
        initialize: http.initialize,
        initialized: http.initialized,
        tools: { status: http.tools.status, headers: http.tools.headers },
        status: http.status,
        invalidInput: http.invalidInput,
        health: http.health,
        unauthorized: http.unauthorized,
      },
      stdio: {
        initialize: stdio.initialize,
        toolsSha256: hash((stdio.tools as { result: { tools: unknown } }).result.tools),
        statusSha256: hash(stdio.status),
        stderr: stdio.stderr,
      },
    },
    t3: {
      requests: fake.requests,
      dispatches: fake.dispatches,
      task,
    },
    files: { operations, audit, setup: setupFiles },
    cli: { setup, usageSummary: { status: usageSummary.status, stdout: normalizedUsageSummary, stderr: usageSummary.stderr } },
  }, paths);
  await mkdir(dirname(fixturePath), { recursive: true });
  await writeFile(fixturePath, `${JSON.stringify(captured)}\n`);
  process.stdout.write(`${fixturePath}\n`);
} finally {
  await client?.close().catch(() => undefined);
  await mcpServer?.close().catch(() => undefined);
  if (httpServer?.listening) await new Promise<void>((resolveClose) => httpServer!.close(() => resolveClose()));
  await fixture?.cleanup();
  await fake.close();
  await Promise.all(temporaryPaths.map((path) => rm(path, { recursive: true, force: true })));
}
