import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { createHash } from "node:crypto";
import { createServer } from "node:http";
import { mkdtemp, readFile, rename, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { setTimeout as delay } from "node:timers/promises";

const [typescriptCli, rustBinary, output, catalogFile] = process.argv.slice(2);
assert(typescriptCli && rustBinary && output,
  "Use node scripts/benchmark-gateway-memory.mjs <main/dist/cli.js> <release-binary> <output.json> [native-catalog.json].");
assert.equal(process.platform, "linux", "RSS sampling requires Linux /proc.");
const { NATIVE_TOOL_NAMES } = await import(pathToFileURL(join(dirname(resolve(typescriptCli)), "contract.js")));
const schema = {
  type: "object",
  properties: {
    cursor: { type: "integer", minimum: 0 },
    limit: { type: "integer", minimum: 1, maximum: 100 },
    projectId: { type: "string" },
    filters: { type: "object", properties: {
      statuses: { type: "array", items: { type: "string", enum: ["running", "completed", "waiting"] } },
      archived: { type: "boolean", default: false },
    } },
  },
};
const tools = catalogFile ? JSON.parse(await readFile(catalogFile, "utf8")) : NATIVE_TOOL_NAMES.map((name) => ({
  name, description: `Isolated memory fixture for ${name}.`, inputSchema: schema,
  outputSchema: { type: "object", properties: { marker: { type: "string" }, items: { type: "array", items: { type: "string" } } }, required: ["marker", "items"] },
}));
assert.deepEqual(new Set(tools.map((tool) => tool.name)), new Set(NATIVE_TOOL_NAMES));
const catalogJson = JSON.stringify(tools);
const payload = { marker: "memory-fixture-complete", items: Array.from({ length: 20 }, (_, i) => `item-${i}`) };
const counts = { catalogs: 0, calls: 0, initializes: 0, descriptors: 0, sessions: 0, shells: 0, unexpected: 0, authFailures: 0 };
let expectedToken;
const upstream = createServer(async (request, response) => {
  const reply = (status, body) => {
    response.writeHead(status, { "content-type": "application/json" });
    response.end(body === undefined ? undefined : JSON.stringify(body));
  };
  try {
    if (request.url === "/.well-known/t3/environment") {
      counts.descriptors++;
      reply(200, { environmentId: "memory-fixture", label: "Isolated fixture", serverVersion: "fixture-v1", orchestrationProtocolVersion: 1 });
      return;
    }
    if (request.url === "/api/auth/session") {
      counts.sessions++;
      reply(200, { authenticated: true, scopes: ["orchestration:read", "orchestration:operate"] });
      return;
    }
    if (request.url === "/api/orchestration/shell") {
      counts.shells++;
      reply(200, { snapshotSequence: 1, projects: [], threads: [], updatedAt: new Date().toISOString() });
      return;
    }
    if (request.url !== "/mcp") {
      counts.unexpected++;
      reply(404, { error: "unexpected route" });
      return;
    }
    if (request.headers.authorization !== `Bearer ${expectedToken}`) {
      counts.authFailures++;
      reply(401, { error: "fixture credential mismatch" });
      return;
    }
    if (request.method === "GET") { reply(405); return; }
    if (request.method === "DELETE") { reply(200, {}); return; }
    let text = "";
    for await (const chunk of request) text += chunk;
    const rpc = JSON.parse(text);
    let result;
    if (rpc.method === "initialize") {
      counts.initializes++;
      result = { protocolVersion: "2025-11-25", capabilities: { tools: {} }, serverInfo: { name: "memory-fixture", version: "1" } };
    } else if (rpc.method === "tools/list") {
      counts.catalogs++;
      result = { tools };
    } else if (rpc.method === "tools/call") {
      assert.equal(rpc.params.name, "t3_thread_list");
      assert.deepEqual(rpc.params.arguments, { cursor: 0, limit: 20 });
      counts.calls++;
      result = { content: [{ type: "text", text: JSON.stringify(payload) }], structuredContent: payload };
    } else if (rpc.id === undefined) { reply(202); return; }
    else { counts.unexpected++; reply(400, { error: "unexpected RPC" }); return; }
    reply(200, { jsonrpc: "2.0", id: rpc.id, result });
  } catch (error) {
    counts.unexpected++;
    reply(500, { error: error.message });
  }
});
upstream.listen(0, "127.0.0.1");
await once(upstream, "listening");
const upstreamUrl = `http://127.0.0.1:${upstream.address().port}`;
const directory = await mkdtemp(join(tmpdir(), "t3-gateway-memory-"));
let sequence = 0;
const runs = [];
let child;

async function sample(pid) {
  const [statusText, rollup, processStat] = await Promise.all([
    readFile(`/proc/${pid}/status`, "utf8"), readFile(`/proc/${pid}/smaps_rollup`, "utf8"), readFile(`/proc/${pid}/stat`, "utf8"),
  ]);
  const field = (text, name) => Number(text.match(new RegExp(`^${name}:\\s+(\\d+)`, "m"))?.[1] ?? 0);
  const fields = processStat.slice(processStat.lastIndexOf(")") + 2).split(" ");
  return { at: Date.now(), rssKiB: field(statusText, "VmRSS"), peakKiB: field(statusText, "VmHWM"),
    anonymousKiB: field(rollup, "Anonymous"), pssKiB: field(rollup, "Pss"),
    fileKiB: field(statusText, "RssFile"), privateDirtyKiB: field(rollup, "Private_Dirty"),
    userTicks: Number(fields[11]), systemTicks: Number(fields[12]), startTicks: fields[19] };
}

async function rpc(port, method, params) {
  const response = await fetch(`http://127.0.0.1:${port}/mcp`, {
    method: "POST", signal: AbortSignal.timeout(15000),
    headers: { authorization: "Bearer isolated-benchmark", "content-type": "application/json", accept: "application/json, text/event-stream" },
    body: JSON.stringify({ jsonrpc: "2.0", id: ++sequence, method, params }),
  });
  assert.equal(response.status, 200);
  const body = await response.json();
  assert.equal(body.error, undefined, JSON.stringify(body));
  assert.notEqual(body.result.isError, true, JSON.stringify(body));
  return body.result;
}

async function stop() {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  const exited = once(child, "exit");
  child.kill("SIGTERM");
  const timer = setTimeout(() => child.kill("SIGKILL"), 5000);
  try { await exited; } finally { clearTimeout(timer); }
}

async function run(runtime, repetition) {
  const dataDir = await mkdtemp(join(directory, `${runtime}-${repetition}-`));
  const credentialFile = join(dataDir, "native-mcp-credential.json");
  async function credential(index) {
    expectedToken = `fixture-${runtime}-${repetition}-${index}`;
    await writeFile(`${credentialFile}.tmp`, JSON.stringify({
      resource: `${upstreamUrl}/mcp`, clientId: "isolated-benchmark", accessToken: expectedToken,
      access: "full-access", scopes: ["orchestration:read", "orchestration:operate"],
      obtainedAt: new Date().toISOString(), expiresAt: "2099-01-01T00:00:00Z",
    }), { mode: 0o600 });
    await rename(`${credentialFile}.tmp`, credentialFile);
  }
  await credential(0);
  const portServer = createServer();
  portServer.listen(0, "127.0.0.1");
  await once(portServer, "listening");
  const port = portServer.address().port;
  await new Promise((done) => portServer.close(done));
  const command = runtime === "typescript" ? process.execPath : resolve(rustBinary);
  const args = runtime === "typescript" ? [resolve(typescriptCli), "serve"] : ["serve"];
  let stderr = "";
  child = spawn(command, args, { cwd: dataDir, stdio: ["ignore", "ignore", "pipe"], env: {
    ...process.env, NODE_ENV: "production", MCP_TRANSPORT: "http", MCP_HOST: "127.0.0.1", MCP_PORT: String(port),
    MCP_BEARER_TOKEN: "isolated-benchmark", MCP_READ_ONLY: "true", T3_HTTP_BASE_URL: upstreamUrl,
    T3_NATIVE_MCP_URL: `${upstreamUrl}/mcp`, T3_ACCESS_TOKEN: "isolated-benchmark", T3_MCP_DATA_DIR: dataDir,
  } });
  child.stderr.on("data", (bytes) => { stderr = (stderr + bytes).slice(-8192); });
  child.on("error", (error) => { stderr += error.message; });
  const before = { ...counts };
  const samples = [];
  let sampling = false;
  const timer = setInterval(async () => {
    if (sampling) return;
    sampling = true;
    try { samples.push(await sample(child.pid)); } catch {} finally { sampling = false; }
  }, 100);
  try {
    let listening = false;
    for (let attempt = 0; attempt < 150; attempt++) {
      assert.equal(child.exitCode, null, stderr);
      try { listening = (await fetch(`http://127.0.0.1:${port}/healthz`)).ok; } catch {}
      if (listening) break;
      await delay(100);
    }
    assert(listening, `Gateway failed to listen: ${stderr}`);
    const startup = await sample(child.pid);
    await rpc(port, "initialize", { protocolVersion: "2025-11-25", capabilities: {}, clientInfo: { name: "memory-benchmark", version: "1" } });
    const initialCatalog = await rpc(port, "tools/list", {});
    assert(initialCatalog.tools.some((tool) => tool.name === "t3_thread_list"));
    let checkedCalls = 0;
    let checkedGatewayCalls = 0;
    async function work(refreshes, offset) {
      for (let i = 1; i <= refreshes; i++) {
        const previous = counts.catalogs;
        await credential(offset + i);
        const listed = await rpc(port, "tools/list", {});
        assert(listed.tools.some((tool) => tool.name === "t3_thread_list"));
        for (let wait = 0; counts.catalogs === previous && wait < 150; wait++) await delay(20);
        assert.equal(counts.catalogs, previous + 1, "Exactly one upstream catalog refresh per renewal.");
        for (let call = 0; call < 10; call++) {
          const result = await rpc(port, "tools/call", { name: "t3_thread_list", arguments: { cursor: 0, limit: 20 } });
          assert.deepEqual(result.structuredContent, payload);
          assert.deepEqual(JSON.parse(result.content[0].text), payload);
          checkedCalls++;
        }
        const status = await rpc(port, "tools/call", { name: "t3_connection_status", arguments: {} });
        assert.equal(status.structuredContent.connectionStatus, "connected");
        assert.equal(status.structuredContent.environment.environmentId, "memory-fixture");
        const overview = await rpc(port, "tools/call", { name: "t3_threads_overview", arguments: {} });
        assert.equal(overview.structuredContent.total, 0);
        assert.deepEqual(overview.structuredContent.highlights, []);
        checkedGatewayCalls += 2;
      }
    }
    await work(5, 0);
    await delay(1000);
    const warm = await sample(child.pid);
    await work(30, 5);
    const loaded = await sample(child.pid);
    await delay(2000);
    const settled = await sample(child.pid);
    const upstreamCounts = Object.fromEntries(Object.entries(counts).map(([key, value]) => [key, value - before[key]]));
    assert.equal(upstreamCounts.calls, checkedCalls);
    assert.equal(upstreamCounts.catalogs, 36);
    assert.equal(upstreamCounts.unexpected, 0);
    assert.equal(upstreamCounts.authFailures, 0);
    assert(upstreamCounts.sessions > 0 && upstreamCounts.shells > 0 && upstreamCounts.descriptors > 0);
    const result = { runtime, repetition, startup, warm, loaded, settled, samples, upstreamCounts, checkedCalls, checkedGatewayCalls,
      auditBytes: (await stat(join(dataDir, "audit.jsonl"))).size, stderr };
    runs.push(result);
    console.log(JSON.stringify({ runtime, repetition, warmKiB: warm.rssKiB, settledKiB: settled.rssKiB,
      growthKiB: settled.rssKiB - warm.rssKiB, upstreamCounts, checkedCalls, checkedGatewayCalls }));
  } finally { clearInterval(timer); await stop(); }
}

try {
  for (let repetition = 1; repetition <= 5; repetition++) {
    await run("typescript", repetition);
    await run("rust", repetition);
  }
} finally {
  await stop();
  upstream.closeAllConnections();
  await new Promise((done) => upstream.close(done));
  await writeFile(output, JSON.stringify({ complete: runs.length === 10, node: process.version,
    catalogSource: catalogFile ?? "synthetic allowlisted tool fixture", catalogBytes: Buffer.byteLength(catalogJson),
    catalogSha256: createHash("sha256").update(catalogJson).digest("hex"), toolCount: tools.length,
    method: "5 alternating pairs; 5 warmup renewals and 30 measured renewals with 10 native calls and status plus overview each; RSS sampled at 100ms; 2s final idle; native credential rotation forces catalog refresh", runs }, null, 2));
  await rm(directory, { recursive: true, force: true });
}
