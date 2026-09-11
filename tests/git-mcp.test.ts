import { execFile } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { afterEach, describe, expect, it } from "vitest";
import { createMcpServer } from "../src/mcp/server.js";
import { gatewayFixture, type GatewayFixture } from "./support/gateway-fixture.js";
import { FakeT3 } from "./support/fake-t3.js";

const execute = promisify(execFile);
const directories: string[] = [];
const fakes: FakeT3[] = [];
const fixtures: GatewayFixture[] = [];
const clients: Client[] = [];
const servers: Array<{ close: () => Promise<void> }> = [];

afterEach(async () => {
  await Promise.all(clients.splice(0).map((client) => client.close().catch(() => undefined)));
  await Promise.all(servers.splice(0).map((server) => server.close().catch(() => undefined)));
  await Promise.all(fixtures.splice(0).map((fixture) => fixture.cleanup()));
  await Promise.all(fakes.splice(0).map((fake) => fake.close()));
  await Promise.all(directories.splice(0).map((directory) => rm(directory, { recursive: true, force: true })));
});

async function git(cwd: string, ...args: string[]): Promise<void> {
  await execute("git", args, { cwd });
}

async function connect(fake: FakeT3): Promise<Client> {
  fakes.push(fake);
  await fake.start();
  const fixture = await gatewayFixture(fake);
  fixtures.push(fixture);
  const server = createMcpServer(fixture.gateway);
  const client = new Client({ name: "git-mcp-test", version: "1.0.0" }, { capabilities: {} });
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await server.connect(serverTransport);
  await client.connect(clientTransport);
  servers.push(server);
  clients.push(client);
  return client;
}

describe("Git MCP tools", () => {
  it("uses the T3 thread worktree rather than the gateway cwd or project root", async () => {
    const projectRoot = await mkdtemp(join(tmpdir(), "t3-git-project-"));
    const worktreeRoot = await mkdtemp(join(tmpdir(), "t3-git-worktree-parent-"));
    await rm(worktreeRoot, { recursive: true });
    directories.push(projectRoot, worktreeRoot);
    await git(projectRoot, "init", "-b", "main");
    await git(projectRoot, "config", "user.name", "T3 Test");
    await git(projectRoot, "config", "user.email", "t3@example.test");
    await writeFile(join(projectRoot, "file.txt"), "initial\n");
    await git(projectRoot, "add", ".");
    await git(projectRoot, "commit", "-m", "initial");
    await git(projectRoot, "worktree", "add", "-b", "thread-branch", worktreeRoot);
    await writeFile(join(worktreeRoot, "file.txt"), "thread worktree change\n");

    const fake = new FakeT3();
    fake.addProject({ id: "project-git", workspaceRoot: projectRoot });
    fake.addThread({
      id: "thread-git",
      projectId: "project-git",
      branch: "thread-branch",
      worktreePath: worktreeRoot,
    });
    const client = await connect(fake);

    const status = await client.callTool({
      name: "t3_git_status",
      arguments: { projectId: "project-git", threadId: "thread-git" },
    });
    const diff = await client.callTool({
      name: "t3_git_diff",
      arguments: { projectId: "project-git", threadId: "thread-git", mode: "unstaged", paths: ["file.txt"] },
    });

    expect(status.isError).not.toBe(true);
    expect(status.structuredContent).toMatchObject({
      environmentId: fake.environmentId,
      workspace: {
        projectId: "project-git",
        threadId: "thread-git",
        source: "thread_worktree",
        selectedPath: worktreeRoot,
        repositoryRoot: worktreeRoot,
        threadBranch: "thread-branch",
      },
      branch: { name: "thread-branch" },
      unstaged: { count: 1 },
    });
    expect(diff.structuredContent).toMatchObject({
      workspace: { selectedPath: worktreeRoot },
      comparison: { base: "index", target: "working_tree" },
      paths: ["file.txt"],
      truncation: { truncated: false },
    });
    expect((diff.structuredContent as { patch: string }).patch).toContain("thread worktree change");
  });

  it("rejects relative, missing, non-Git, and mismatched T3 workspace identities", async () => {
    const nonGit = await mkdtemp(join(tmpdir(), "t3-git-nonrepo-"));
    directories.push(nonGit);
    const fake = new FakeT3();
    fake.addProject({ id: "relative", workspaceRoot: "relative/path" });
    fake.addProject({ id: "missing", workspaceRoot: join(nonGit, "absent") });
    fake.addProject({ id: "non-git", workspaceRoot: nonGit });
    fake.addProject({ id: "other", workspaceRoot: nonGit });
    fake.addThread({ id: "other-thread", projectId: "other" });
    const client = await connect(fake);

    const cases = [
      { arguments: { projectId: "relative" }, code: "workspace_path_not_absolute" },
      { arguments: { projectId: "missing" }, code: "workspace_missing" },
      { arguments: { projectId: "non-git" }, code: "not_git_worktree" },
      { arguments: { projectId: "non-git", threadId: "other-thread" }, code: "thread_project_mismatch" },
      { arguments: { projectId: "does-not-exist" }, code: "project_not_found" },
    ];
    for (const testCase of cases) {
      const result = await client.callTool({ name: "t3_git_status", arguments: testCase.arguments });
      expect(result.isError).toBe(true);
      expect(result.structuredContent).toMatchObject({ error: { code: testCase.code } });
    }
  });
});
