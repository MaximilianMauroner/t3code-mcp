import { execFile } from "node:child_process";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import { afterEach, describe, expect, it } from "vitest";
import { makeGateway } from "../src/gateway.js";
import { IdempotencyConflictError } from "../src/operations/journal.js";
import { FakeT3 } from "./support/fake-t3.js";
import { gatewayFixture, type GatewayFixture } from "./support/gateway-fixture.js";

const fakes: FakeT3[] = [];
const fixtures: GatewayFixture[] = [];
const directories: string[] = [];
const execute = promisify(execFile);

afterEach(async () => {
  await Promise.all(fixtures.splice(0).map((fixture) => fixture.cleanup()));
  await Promise.all(fakes.splice(0).map((fake) => fake.close()));
  await Promise.all(directories.splice(0).map((directory) => rm(directory, { recursive: true, force: true })));
});

async function setup(options: ConstructorParameters<typeof FakeT3>[0] = {}) {
  const fake = new FakeT3(options);
  fakes.push(fake);
  await fake.start();
  const fixture = await gatewayFixture(fake);
  fixtures.push(fixture);
  fake.addProject({ id: "project-task", title: "Task project" });
  return { fake, fixture, gateway: fixture.gateway };
}

const taskInput = {
  projectId: "project-task",
  title: "Recoverable assignment",
  instruction: "Inspect the project and report the result without changing files.",
  runtimeMode: "approval-required" as const,
  idempotencyKey: "task-key",
};

describe("recoverable task delegation", () => {
  it("starts a managed worktree from the selected origin branch", async () => {
    const { fake, gateway } = await setup();
    const input = {
      ...taskInput,
      idempotencyKey: "worktree-task-key",
      workspaceMode: "worktree",
      branch: "main",
      startFromOrigin: true,
    } as const;
    const started = await gateway.taskStart(input);
    const retried = await gateway.taskStart(input);

    expect(started.stage).toBe("run_accepted");
    expect(retried.taskRef).toBe(started.taskRef);
    expect(fake.dispatches).toHaveLength(1);
    expect(fake.dispatches[0]?.command).toMatchObject({
      type: "thread.turn.start",
      bootstrap: {
        createThread: {
          projectId: "project-task",
          branch: "main",
          worktreePath: null,
        },
        prepareWorktree: {
          projectCwd: "/remote/project-1",
          baseBranch: "main",
          startFromOrigin: true,
          requireWorktree: true,
        },
        runSetupScript: true,
      },
    });
    if (fake.dispatches[0]?.command.type !== "thread.turn.start") throw new Error("expected turn start");
    expect(fake.dispatches[0].command.bootstrap?.prepareWorktree?.branch).toMatch(/^t3code\/[0-9a-f]{8}$/);
  });

  it("rejects incomplete or conflicting worktree selections before dispatch", async () => {
    const { fake, gateway } = await setup();

    await expect(gateway.taskStart({ ...taskInput, idempotencyKey: "missing-base", workspaceMode: "worktree" }))
      .rejects.toMatchObject({ code: "worktree_base_branch_required" });
    await expect(gateway.taskStart({
      ...taskInput,
      idempotencyKey: "managed-path",
      workspaceMode: "worktree",
      branch: "main",
      worktreePath: "/tmp/caller-selected",
    })).rejects.toMatchObject({ code: "worktree_path_not_allowed" });
    await expect(gateway.taskStart({ ...taskInput, idempotencyKey: "local-origin", startFromOrigin: false }))
      .rejects.toMatchObject({ code: "origin_selection_requires_worktree" });
    expect(fake.dispatches).toHaveLength(0);
  });

  it("starts once, lists by durable reference, and recovers through a fresh gateway", async () => {
    const { fake, fixture, gateway } = await setup();
    const started = await gateway.taskStart(taskInput);

    expect(started).toMatchObject({
      projectId: "project-task",
      title: "Recoverable assignment",
      stage: "run_accepted",
      baselineAttribution: "unavailable",
    });
    expect(started.taskRef).toMatch(/^task_/);
    expect(started.threadId).toBeTruthy();
    expect(started.runId).toBeTruthy();
    expect(fake.dispatches.map((entry) => entry.command.type)).toEqual(["thread.turn.start"]);
    expect(fake.dispatches[0]?.command).toMatchObject({
      bootstrap: { createThread: { projectId: "project-task" } },
      message: { text: taskInput.instruction },
    });

    const second = makeGateway(fixture.config).gateway;
    const listed = await second.tasksList({ query: "recoverable", limit: 10 });
    expect(listed.page).toMatchObject({ total: 1, items: [{ taskRef: started.taskRef, stage: "run_accepted" }] });
    const recovered = await second.taskGet(started.taskRef);
    expect(recovered.task).toMatchObject({
      taskRef: started.taskRef,
      threadId: started.threadId,
      runId: started.runId,
      thread: { id: started.threadId },
      run: { runId: started.runId, runStatus: "running" },
    });

    const journalText = await readFile(join(fixture.config.dataDir, "operations.json"), "utf8");
    expect(journalText).not.toContain(taskInput.instruction);
  });

  it("coalesces concurrent starts and rejects changed input for the same key", async () => {
    const { fake, gateway } = await setup();
    const attempts = await Promise.all([gateway.taskStart(taskInput), gateway.taskStart(taskInput)]);
    const settled = await gateway.taskStart(taskInput);

    expect(new Set(attempts.map((task) => task.taskRef))).toEqual(new Set([settled.taskRef]));
    expect(settled.stage).toBe("run_accepted");
    expect(fake.dispatches.filter((entry) => entry.command.type === "thread.create")).toHaveLength(0);
    expect(fake.dispatches.filter((entry) => entry.command.type === "thread.turn.start")).toHaveLength(1);
    await expect(gateway.taskStart({ ...taskInput, instruction: "Different work" }))
      .rejects.toBeInstanceOf(IdempotencyConflictError);
  });

  it("reconciles applied timeouts before advancing and never duplicates either child command", async () => {
    const { fake, gateway } = await setup({
      dispatchStatus: 503,
      dispatchErrorMessage: "response lost",
      applyBeforeDispatchFailure: true,
    });

    const afterStart = await gateway.taskStart(taskInput);
    expect(afterStart.stage).toBe("dispatch_uncertain");
    expect(fake.dispatches.map((entry) => entry.command.type)).toEqual(["thread.turn.start"]);
    const reconciledStart = await gateway.taskGet(afterStart.taskRef);
    expect(reconciledStart.task).toMatchObject({ stage: "run_accepted", lastError: null });

    const recovered = await gateway.taskStart(taskInput);
    expect(recovered.stage).toBe("run_accepted");
    expect(fake.dispatches.filter((entry) => entry.command.type === "thread.create")).toHaveLength(0);
    expect(fake.dispatches.filter((entry) => entry.command.type === "thread.turn.start")).toHaveLength(1);
  });

  it("fails closed before persistence or dispatch without operate permission", async () => {
    const { fake, fixture } = await setup();
    const readOnly = makeGateway({ ...fixture.config, readOnly: true, dataDir: join(fixture.directory, "read-only") }).gateway;
    await expect(readOnly.taskStart(taskInput)).rejects.toMatchObject({ code: "gateway_read_only" });
    expect(fake.dispatches).toHaveLength(0);
  });

  it("fails closed when the upstream credential lacks operate scope", async () => {
    const { fake, gateway } = await setup({ scopes: ["orchestration:read"] });
    await expect(gateway.taskStart(taskInput)).rejects.toMatchObject({ code: "t3_scope_required" });
    expect(fake.dispatches).toHaveLength(0);
  });

  it("captures a clean accepted workspace HEAD as the review baseline", async () => {
    const repository = await mkdtemp(join(tmpdir(), "t3-task-baseline-"));
    directories.push(repository);
    await execute("git", ["init", "-b", "main"], { cwd: repository });
    await execute("git", ["config", "user.name", "T3 Test"], { cwd: repository });
    await execute("git", ["config", "user.email", "t3@example.test"], { cwd: repository });
    await writeFile(join(repository, "file.txt"), "initial\n");
    await execute("git", ["add", "."], { cwd: repository });
    await execute("git", ["commit", "-m", "initial"], { cwd: repository });
    const head = (await execute("git", ["rev-parse", "HEAD"], { cwd: repository })).stdout.trim();

    const fake = new FakeT3();
    fakes.push(fake);
    await fake.start();
    const fixture = await gatewayFixture(fake);
    fixtures.push(fixture);
    fake.addProject({ id: "project-task", workspaceRoot: repository });

    const started = await fixture.gateway.taskStart(taskInput);
    expect(started).toMatchObject({ baselineRevision: head, baselineAttribution: "clean", stage: "run_accepted" });

    await writeFile(join(repository, "file.txt"), "committed result\n");
    await execute("git", ["commit", "-am", "task result"], { cwd: repository });
    await writeFile(join(repository, "file.txt"), "working result\n");
    await writeFile(join(repository, "untracked.txt"), "untracked result\n");
    const result = await fixture.gateway.resultGet({ taskRef: started.taskRef, paths: [], maxBytes: 100_000 });
    expect(result).toMatchObject({
      evidence: {
        taskStateSource: "t3_observed",
        git: {
          source: "git_observed",
          baselineRevision: head,
          status: { clean: false, untracked: { count: 1 } },
          committed: { attribution: { quality: "working_tree_dirty" } },
        },
      },
    });
    expect(result.evidence.git?.committed.patch).toContain("+committed result");
    expect(result.evidence.git?.unstaged.patch).toContain("+working result");
    expect(result.limitations).toEqual(expect.arrayContaining([expect.stringContaining("uncommitted")]));
  });
});
