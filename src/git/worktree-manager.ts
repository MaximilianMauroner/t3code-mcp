import { randomUUID } from "node:crypto";
import { spawn } from "node:child_process";
import { mkdir, realpath, stat } from "node:fs/promises";
import { basename, dirname, join, resolve } from "node:path";
import { summarizeForAudit, type AuditLog } from "../operations/audit-log.js";

const GIT_TIMEOUT_MS = 60_000;
const OUTPUT_LIMIT_BYTES = 64 * 1024;

export interface ManagedWorktreeRequest {
  readonly projectId: string;
  readonly projectWorkspaceRoot: string;
  readonly baseBranch: string;
  readonly startFromOrigin: boolean;
  readonly operationKey: string;
}

export interface ManagedWorktree {
  readonly branch: string;
  readonly path: string;
  readonly baseCommit: string;
}

export class GitWorktreeError extends Error {
  override readonly name = "GitWorktreeError";

  constructor(
    readonly code: string,
    message: string,
  ) {
    super(message);
  }
}

/**
 * Creates the explicit worktree that T3's HTTP bootstrap route currently
 * fails to attach. The derived branch/path make the local mutation recoverable
 * without storing another source of workspace identity.
 */
export class GitWorktreeManager {
  private readonly pending = new Map<string, Promise<ManagedWorktree>>();

  constructor(
    private readonly configuredRoot: string | null = null,
    private readonly auditLog?: AuditLog,
  ) {}

  async prepare(input: ManagedWorktreeRequest): Promise<ManagedWorktree> {
    const projectRoot = await realpath(input.projectWorkspaceRoot).catch(() => {
      throw new GitWorktreeError(
        "project_workspace_missing",
        `The T3 project workspace is not accessible: ${input.projectWorkspaceRoot}`,
      );
    });
    await assertDirectory(projectRoot);
    await validateBranchName(this.auditLog, projectRoot, input.baseBranch);

    const suffix = input.operationKey.toLowerCase().replaceAll(/[^a-z0-9]/g, "").slice(0, 16);
    if (suffix.length < 8) {
      throw new GitWorktreeError("invalid_operation_key", "The durable operation key cannot identify a managed worktree.");
    }
    const branch = `t3code/mcp-${suffix}`;
    const root = this.configuredRoot === null
      ? join(dirname(projectRoot), ".t3-code-mcp-worktrees", `${safeSegment(basename(projectRoot))}-${safeSegment(input.projectId)}`)
      : join(this.configuredRoot, safeSegment(input.projectId));
    const worktreePath = resolve(root, suffix);

    const existing = this.pending.get(worktreePath);
    if (existing) return existing;
    const preparation = this.prepareOnce(projectRoot, worktreePath, branch, input)
      .finally(() => this.pending.delete(worktreePath));
    this.pending.set(worktreePath, preparation);
    return preparation;
  }

  private async prepareOnce(
    projectRoot: string,
    worktreePath: string,
    branch: string,
    input: ManagedWorktreeRequest,
  ): Promise<ManagedWorktree> {
    if (await pathExists(worktreePath)) {
      const baseCommit = await verifyExistingWorktree(this.auditLog, projectRoot, worktreePath, branch);
      return { branch, path: worktreePath, baseCommit };
    }

    await mkdir(dirname(worktreePath), { recursive: true, mode: 0o700 });
    let baseRef = `refs/heads/${input.baseBranch}`;
    if (input.startFromOrigin && await remoteExists(this.auditLog, projectRoot, "origin")) {
      await runGit(this.auditLog, projectRoot, ["fetch", "--", "origin"]);
      baseRef = `refs/remotes/origin/${input.baseBranch}`;
    }
    const baseCommit = (await runGit(
      this.auditLog,
      projectRoot,
      ["rev-parse", "--verify", `${baseRef}^{commit}`],
    )).stdout.trim();
    if (!/^[0-9a-f]{40,64}$/i.test(baseCommit)) {
      throw new GitWorktreeError("git_base_invalid", `Git returned an invalid commit for base branch ${input.baseBranch}.`);
    }

    const branchExists = (await runGit(
      this.auditLog,
      projectRoot,
      ["show-ref", "--verify", "--quiet", `refs/heads/${branch}`],
      [0, 1],
    )).exitCode === 0;
    if (branchExists) {
      await runGit(this.auditLog, projectRoot, ["worktree", "add", "--", worktreePath, branch]);
    } else {
      await runGit(this.auditLog, projectRoot, ["worktree", "add", "-b", branch, "--", worktreePath, baseCommit]);
    }
    const observedCommit = await verifyExistingWorktree(this.auditLog, projectRoot, worktreePath, branch);
    return { branch, path: worktreePath, baseCommit: observedCommit };
  }
}

async function validateBranchName(auditLog: AuditLog | undefined, cwd: string, branch: string): Promise<void> {
  if (branch.startsWith("-")) {
    throw new GitWorktreeError("invalid_base_branch", `Invalid worktree base branch: ${branch}`);
  }
  try {
    await runGit(auditLog, cwd, ["check-ref-format", "--branch", branch]);
  } catch {
    throw new GitWorktreeError("invalid_base_branch", `Invalid worktree base branch: ${branch}`);
  }
}

async function remoteExists(auditLog: AuditLog | undefined, cwd: string, remote: string): Promise<boolean> {
  const result = await runGit(auditLog, cwd, ["remote", "get-url", "--", remote], [0, 2]);
  return result.exitCode === 0;
}

async function verifyExistingWorktree(
  auditLog: AuditLog | undefined,
  projectRoot: string,
  worktreePath: string,
  expectedBranch: string,
): Promise<string> {
  await assertDirectory(worktreePath);
  const [projectCommon, worktreeCommon, branch, commit] = await Promise.all([
    runGit(auditLog, projectRoot, ["rev-parse", "--path-format=absolute", "--git-common-dir"]),
    runGit(auditLog, worktreePath, ["rev-parse", "--path-format=absolute", "--git-common-dir"]),
    runGit(auditLog, worktreePath, ["branch", "--show-current"]),
    runGit(auditLog, worktreePath, ["rev-parse", "--verify", "HEAD^{commit}"]),
  ]);
  const expectedCommon = await realpath(projectCommon.stdout.trim());
  const observedCommon = await realpath(worktreeCommon.stdout.trim());
  if (expectedCommon !== observedCommon) {
    throw new GitWorktreeError("worktree_identity_mismatch", `Existing path is not a worktree of the selected project: ${worktreePath}`);
  }
  if (branch.stdout.trim() !== expectedBranch) {
    throw new GitWorktreeError(
      "worktree_branch_mismatch",
      `Existing managed worktree ${worktreePath} uses branch ${branch.stdout.trim() || "detached"}, expected ${expectedBranch}.`,
    );
  }
  return commit.stdout.trim();
}

async function assertDirectory(path: string): Promise<void> {
  try {
    if (!(await stat(path)).isDirectory()) throw new Error("not a directory");
  } catch {
    throw new GitWorktreeError("workspace_not_directory", `Workspace is not a readable directory: ${path}`);
  }
}

async function pathExists(path: string): Promise<boolean> {
  try {
    await stat(path);
    return true;
  } catch (error) {
    if (typeof error === "object" && error !== null && "code" in error && error.code === "ENOENT") return false;
    throw error;
  }
}

function safeSegment(value: string): string {
  const normalized = value.toLowerCase().replaceAll(/[^a-z0-9._-]/g, "-").replaceAll(/-+/g, "-").slice(0, 80);
  return normalized || "project";
}

interface GitResult {
  readonly exitCode: number;
  readonly stdout: string;
  readonly stderr: string;
}

async function runGit(
  auditLog: AuditLog | undefined,
  cwd: string,
  args: ReadonlyArray<string>,
  acceptedExitCodes: ReadonlyArray<number> = [0],
): Promise<GitResult> {
  const correlationId = `git_${randomUUID()}`;
  const startedAt = Date.now();
  await auditLog?.record({
    source: "git",
    event: "git.command",
    correlationId,
    operation: `git ${args[0] ?? "unknown"}`,
    outcome: "started",
    details: { cwd, args: summarizeForAudit(args), mutation: true },
  });
  const environment = Object.fromEntries(
    Object.entries(process.env).filter(([key, value]) => !key.startsWith("GIT_") && value !== undefined),
  ) as NodeJS.ProcessEnv;
  environment.GIT_PAGER = "cat";
  environment.GIT_TERMINAL_PROMPT = "0";

  return new Promise<GitResult>((resolvePromise, rejectPromise) => {
    const child = spawn("git", ["--no-pager", ...args], { cwd, env: environment, shell: false, stdio: ["ignore", "pipe", "pipe"] });
    const stdout: Buffer[] = [];
    const stderr: Buffer[] = [];
    let stdoutBytes = 0;
    let stderrBytes = 0;
    let timedOut = false;
    let settled = false;
    const capture = (chunks: Buffer[], chunk: Buffer, used: number): number => {
      const remaining = OUTPUT_LIMIT_BYTES - used;
      if (remaining > 0) chunks.push(chunk.subarray(0, remaining));
      return used + chunk.length;
    };
    child.stdout.on("data", (chunk: Buffer) => { stdoutBytes = capture(stdout, chunk, stdoutBytes); });
    child.stderr.on("data", (chunk: Buffer) => { stderrBytes = capture(stderr, chunk, stderrBytes); });
    const timeout = setTimeout(() => {
      timedOut = true;
      child.kill("SIGKILL");
    }, GIT_TIMEOUT_MS);
    const finish = async (outcome: "completed" | "error", details: Record<string, unknown>): Promise<void> => {
      await auditLog?.record({
        source: "git",
        event: "git.result",
        correlationId,
        operation: `git ${args[0] ?? "unknown"}`,
        outcome,
        durationMs: Date.now() - startedAt,
        details,
      });
    };
    child.on("error", (error) => {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      void finish("error", { error: error.message }).then(() => rejectPromise(
        new GitWorktreeError("git_unavailable", `Git could not be started: ${error.message}`),
      ));
    });
    child.on("close", (code, signal) => {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      const exitCode = code ?? -1;
      const stdoutText = Buffer.concat(stdout).toString("utf8");
      const stderrText = Buffer.concat(stderr).toString("utf8").trim();
      if (timedOut) {
        void finish("error", { error: "timeout", exitCode, signal }).then(() => rejectPromise(
          new GitWorktreeError("git_timeout", `Git worktree preparation exceeded ${GIT_TIMEOUT_MS / 1000} seconds.`),
        ));
        return;
      }
      if (!acceptedExitCodes.includes(exitCode)) {
        const detail = stderrText ? `: ${stderrText}` : "";
        void finish("error", { exitCode, signal, stderr: stderrText, stdoutBytes, stderrBytes }).then(() => rejectPromise(
          new GitWorktreeError("git_command_failed", `Git ${args[0] ?? "command"} failed (exit ${exitCode})${detail}`),
        ));
        return;
      }
      void finish("completed", { exitCode, signal, stdoutBytes, stderrBytes }).then(() => resolvePromise({
        exitCode,
        stdout: stdoutText,
        stderr: stderrText,
      }));
    });
  });
}
