import { spawn } from "node:child_process";
import { realpath, stat } from "node:fs/promises";

const GIT_TIMEOUT_MS = 30_000;
const STDERR_LIMIT_BYTES = 64 * 1024;

export type GitDiffMode = "unstaged" | "staged";

export interface GitWorkspaceSelection {
  readonly environmentId: string;
  readonly projectId: string;
  readonly projectTitle: string;
  readonly projectWorkspaceRoot: string;
  readonly threadId: string | null;
  readonly threadBranch: string | null;
  readonly threadWorktreePath: string | null;
  readonly source: "project_workspace" | "thread_worktree";
  readonly selectedPath: string;
}

export interface GitWorkspaceIdentity extends GitWorkspaceSelection {
  readonly resolvedPath: string;
  readonly repositoryRoot: string;
  readonly gitDirectory: string;
  readonly gitCommonDirectory: string;
}

export interface GitChange {
  readonly path: string;
  readonly originalPath?: string;
  readonly kind: "ordinary" | "rename_or_copy";
  readonly indexStatus: string;
  readonly worktreeStatus: string;
  readonly score?: string;
  readonly submodule: string;
}

export interface GitConflict {
  readonly path: string;
  readonly indexStatus: string;
  readonly worktreeStatus: string;
  readonly submodule: string;
}

export interface GitStatusResult {
  readonly environmentId: string;
  readonly observedAt: string;
  readonly workspace: GitWorkspaceIdentity;
  readonly branch: {
    readonly name: string | null;
    readonly detached: boolean;
    readonly unborn: boolean;
    readonly headCommit: string | null;
    readonly upstream: string | null;
    readonly ahead: number | null;
    readonly behind: number | null;
  };
  readonly staged: GitCategory<GitChange>;
  readonly unstaged: GitCategory<GitChange>;
  readonly untracked: GitCategory<{ readonly path: string }>;
  readonly conflicts: GitCategory<GitConflict>;
  readonly clean: boolean;
  readonly truncation: {
    readonly truncated: boolean;
    readonly itemLimitPerCategory: number;
    readonly omittedItems: {
      readonly staged: number;
      readonly unstaged: number;
      readonly untracked: number;
      readonly conflicts: number;
    };
  };
}

export interface GitCategory<T> {
  readonly count: number;
  readonly items: ReadonlyArray<T>;
  readonly truncated: boolean;
}

export interface GitDiffResult {
  readonly environmentId: string;
  readonly observedAt: string;
  readonly workspace: GitWorkspaceIdentity;
  readonly mode: GitDiffMode;
  readonly comparison: {
    readonly base: "index" | "HEAD";
    readonly target: "working_tree" | "index";
  };
  readonly paths: ReadonlyArray<string>;
  readonly patch: string;
  readonly truncation: {
    readonly truncated: boolean;
    readonly reason: "byte_limit" | null;
    readonly maxBytes: number;
    readonly capturedBytes: number;
    readonly totalBytes: number;
    readonly omittedBytes: number;
  };
}

export class GitInspectionError extends Error {
  override readonly name = "GitInspectionError";

  constructor(
    readonly code: string,
    message: string,
  ) {
    super(message);
  }
}

interface GitProcessResult {
  readonly stderr: string;
  readonly stderrTruncated: boolean;
}

interface StatusAccumulator {
  headCommit: string | null;
  branchName: string | null;
  detached: boolean;
  unborn: boolean;
  upstream: string | null;
  ahead: number | null;
  behind: number | null;
  staged: GitChange[];
  unstaged: GitChange[];
  untracked: Array<{ path: string }>;
  conflicts: GitConflict[];
  counts: { staged: number; unstaged: number; untracked: number; conflicts: number };
  pendingRename: GitChange | null;
}

export class GitInspector {
  async status(selection: GitWorkspaceSelection, itemLimitPerCategory: number): Promise<GitStatusResult> {
    const workspace = await this.identifyWorkspace(selection);
    const accumulator: StatusAccumulator = {
      headCommit: null,
      branchName: null,
      detached: false,
      unborn: false,
      upstream: null,
      ahead: null,
      behind: null,
      staged: [],
      unstaged: [],
      untracked: [],
      conflicts: [],
      counts: { staged: 0, unstaged: 0, untracked: 0, conflicts: 0 },
      pendingRename: null,
    };
    let pending = Buffer.alloc(0);
    const processRecord = (recordBuffer: Buffer): void => {
      const record = recordBuffer.toString("utf8");
      if (accumulator.pendingRename !== null) {
        addChange(accumulator, { ...accumulator.pendingRename, originalPath: record }, itemLimitPerCategory);
        accumulator.pendingRename = null;
        return;
      }
      parseStatusRecord(record, accumulator, itemLimitPerCategory);
    };

    await runGit(workspace.resolvedPath, [
      "status",
      "--porcelain=v2",
      "--branch",
      "-z",
      "--untracked-files=all",
      "--ignore-submodules=none",
    ], (chunk) => {
      pending = Buffer.concat([pending, chunk]);
      let separator = pending.indexOf(0);
      while (separator >= 0) {
        processRecord(pending.subarray(0, separator));
        pending = pending.subarray(separator + 1);
        separator = pending.indexOf(0);
      }
    });
    if (pending.length > 0 || accumulator.pendingRename !== null) {
      throw new GitInspectionError("git_output_invalid", "Git status returned an incomplete porcelain record.");
    }

    const omittedItems = {
      staged: accumulator.counts.staged - accumulator.staged.length,
      unstaged: accumulator.counts.unstaged - accumulator.unstaged.length,
      untracked: accumulator.counts.untracked - accumulator.untracked.length,
      conflicts: accumulator.counts.conflicts - accumulator.conflicts.length,
    };
    const truncated = Object.values(omittedItems).some((count) => count > 0);
    return {
      environmentId: selection.environmentId,
      observedAt: new Date().toISOString(),
      workspace,
      branch: {
        name: accumulator.branchName,
        detached: accumulator.detached,
        unborn: accumulator.unborn,
        headCommit: accumulator.headCommit,
        upstream: accumulator.upstream,
        ahead: accumulator.ahead,
        behind: accumulator.behind,
      },
      staged: category(accumulator.staged, accumulator.counts.staged),
      unstaged: category(accumulator.unstaged, accumulator.counts.unstaged),
      untracked: category(accumulator.untracked, accumulator.counts.untracked),
      conflicts: category(accumulator.conflicts, accumulator.counts.conflicts),
      clean: accumulator.counts.staged + accumulator.counts.unstaged + accumulator.counts.untracked + accumulator.counts.conflicts === 0,
      truncation: { truncated, itemLimitPerCategory, omittedItems },
    };
  }

  async diff(
    selection: GitWorkspaceSelection,
    mode: GitDiffMode,
    paths: ReadonlyArray<string>,
    maxBytes: number,
  ): Promise<GitDiffResult> {
    const workspace = await this.identifyWorkspace(selection);
    for (const candidate of paths) validatePathFilter(candidate);

    const chunks: Buffer[] = [];
    let capturedBytes = 0;
    let totalBytes = 0;
    const args = [
      "diff",
      "--no-ext-diff",
      "--no-textconv",
      "--no-color",
      "--find-renames",
      "--find-copies",
      ...(mode === "staged" ? ["--cached"] : []),
      ...(paths.length > 0 ? ["--", ...paths] : []),
    ];
    await runGit(workspace.resolvedPath, args, (chunk) => {
      totalBytes += chunk.length;
      const remaining = maxBytes - capturedBytes;
      if (remaining > 0) {
        const captured = chunk.subarray(0, remaining);
        chunks.push(captured);
        capturedBytes += captured.length;
      }
    });
    const truncated = totalBytes > capturedBytes;
    return {
      environmentId: selection.environmentId,
      observedAt: new Date().toISOString(),
      workspace,
      mode,
      comparison: mode === "staged"
        ? { base: "HEAD", target: "index" }
        : { base: "index", target: "working_tree" },
      paths: [...paths],
      patch: Buffer.concat(chunks).toString("utf8"),
      truncation: {
        truncated,
        reason: truncated ? "byte_limit" : null,
        maxBytes,
        capturedBytes,
        totalBytes,
        omittedBytes: totalBytes - capturedBytes,
      },
    };
  }

  private async identifyWorkspace(selection: GitWorkspaceSelection): Promise<GitWorkspaceIdentity> {
    let resolvedPath: string;
    try {
      const workspaceStat = await stat(selection.selectedPath);
      if (!workspaceStat.isDirectory()) {
        throw new GitInspectionError("workspace_not_directory", `T3 workspace path is not a directory: ${selection.selectedPath}`);
      }
      resolvedPath = await realpath(selection.selectedPath);
    } catch (error) {
      if (error instanceof GitInspectionError) throw error;
      const code = typeof error === "object" && error !== null && "code" in error ? String(error.code) : null;
      if (code === "ENOENT") {
        throw new GitInspectionError("workspace_missing", `T3 workspace path does not exist on the gateway host: ${selection.selectedPath}`);
      }
      throw new GitInspectionError("workspace_unreadable", `T3 workspace path cannot be inspected: ${selection.selectedPath}`);
    }

    const lines: string[] = [];
    let pending = "";
    try {
      await runGit(resolvedPath, ["rev-parse", "--path-format=absolute", "--show-toplevel", "--absolute-git-dir", "--git-common-dir"], (chunk) => {
        pending += chunk.toString("utf8");
        let newline = pending.indexOf("\n");
        while (newline >= 0) {
          lines.push(pending.slice(0, newline));
          pending = pending.slice(newline + 1);
          newline = pending.indexOf("\n");
        }
      });
    } catch (error) {
      if (error instanceof GitInspectionError && error.code === "git_command_failed") {
        throw new GitInspectionError("not_git_worktree", `T3 workspace is not a readable Git worktree: ${selection.selectedPath}`);
      }
      throw error;
    }
    if (pending.length > 0) lines.push(pending);
    if (lines.length !== 3 || lines.some((line) => line.length === 0)) {
      throw new GitInspectionError("git_identity_ambiguous", `Git returned ambiguous worktree identity for T3 workspace: ${selection.selectedPath}`);
    }
    const [repositoryRoot, gitDirectory, gitCommonDirectory] = lines as [string, string, string];
    return { ...selection, resolvedPath, repositoryRoot, gitDirectory, gitCommonDirectory };
  }
}

function category<T>(items: T[], count: number): GitCategory<T> {
  return { count, items, truncated: items.length < count };
}

function parseStatusRecord(record: string, accumulator: StatusAccumulator, limit: number): void {
  if (record.startsWith("# branch.oid ")) {
    const oid = record.slice("# branch.oid ".length);
    accumulator.unborn = oid === "(initial)";
    accumulator.headCommit = accumulator.unborn ? null : oid;
    return;
  }
  if (record.startsWith("# branch.head ")) {
    const head = record.slice("# branch.head ".length);
    accumulator.detached = head === "(detached)";
    accumulator.branchName = accumulator.detached ? null : head;
    return;
  }
  if (record.startsWith("# branch.upstream ")) {
    accumulator.upstream = record.slice("# branch.upstream ".length);
    return;
  }
  if (record.startsWith("# branch.ab ")) {
    const match = /^# branch\.ab \+(\d+) -(\d+)$/.exec(record);
    if (match) {
      accumulator.ahead = Number(match[1]);
      accumulator.behind = Number(match[2]);
    }
    return;
  }
  if (record.startsWith("? ")) {
    accumulator.counts.untracked += 1;
    if (accumulator.untracked.length < limit) accumulator.untracked.push({ path: record.slice(2) });
    return;
  }
  if (record.startsWith("! ") || record.length === 0) return;
  if (record.startsWith("u ")) {
    const fields = splitFixedFields(record, 10);
    const xy = fields[1];
    const submodule = fields[2];
    const path = fields[10];
    if (!xy || !submodule || path === undefined) throw invalidStatusRecord(record);
    accumulator.counts.conflicts += 1;
    if (accumulator.conflicts.length < limit) {
      accumulator.conflicts.push({ path, indexStatus: xy[0] ?? ".", worktreeStatus: xy[1] ?? ".", submodule });
    }
    return;
  }
  if (record.startsWith("1 ")) {
    const fields = splitFixedFields(record, 8);
    const xy = fields[1];
    const submodule = fields[2];
    const path = fields[8];
    if (!xy || !submodule || path === undefined) throw invalidStatusRecord(record);
    addChange(accumulator, {
      path,
      kind: "ordinary",
      indexStatus: xy[0] ?? ".",
      worktreeStatus: xy[1] ?? ".",
      submodule,
    }, limit);
    return;
  }
  if (record.startsWith("2 ")) {
    const fields = splitFixedFields(record, 9);
    const xy = fields[1];
    const submodule = fields[2];
    const score = fields[8];
    const path = fields[9];
    if (!xy || !submodule || !score || path === undefined) throw invalidStatusRecord(record);
    accumulator.pendingRename = {
      path,
      kind: "rename_or_copy",
      indexStatus: xy[0] ?? ".",
      worktreeStatus: xy[1] ?? ".",
      score,
      submodule,
    };
    return;
  }
  throw invalidStatusRecord(record);
}

function splitFixedFields(record: string, fixedFieldCount: number): string[] {
  const fields: string[] = [];
  let start = 0;
  for (let index = 0; index < fixedFieldCount; index += 1) {
    const separator = record.indexOf(" ", start);
    if (separator < 0) return [];
    fields.push(record.slice(start, separator));
    start = separator + 1;
  }
  fields.push(record.slice(start));
  return fields;
}

function addChange(accumulator: StatusAccumulator, change: GitChange, limit: number): void {
  if (change.indexStatus !== ".") {
    accumulator.counts.staged += 1;
    if (accumulator.staged.length < limit) accumulator.staged.push(change);
  }
  if (change.worktreeStatus !== ".") {
    accumulator.counts.unstaged += 1;
    if (accumulator.unstaged.length < limit) accumulator.unstaged.push(change);
  }
}

function invalidStatusRecord(record: string): GitInspectionError {
  return new GitInspectionError("git_output_invalid", `Git status returned an unrecognized porcelain record: ${record.slice(0, 120)}`);
}

function validatePathFilter(candidate: string): void {
  if (candidate.includes("\0")) {
    throw new GitInspectionError("invalid_git_path", "Git path filters cannot contain NUL bytes.");
  }
  if (candidate.startsWith("/") || /^[A-Za-z]:[\\/]/.test(candidate) || candidate.startsWith("\\\\")) {
    throw new GitInspectionError("invalid_git_path", `Git path filters must be relative to the selected T3 workspace: ${candidate}`);
  }
  if (candidate.startsWith(":")) {
    throw new GitInspectionError("invalid_git_path", `Git pathspec magic is not allowed: ${candidate}`);
  }
  const segments = candidate.split(/[\\/]/);
  if (segments.some((segment) => segment === "..")) {
    throw new GitInspectionError("invalid_git_path", `Git path filters cannot traverse outside the selected T3 workspace: ${candidate}`);
  }
}

async function runGit(
  cwd: string,
  commandArgs: ReadonlyArray<string>,
  onStdout: (chunk: Buffer) => void,
): Promise<GitProcessResult> {
  const environment = Object.fromEntries(
    Object.entries(process.env).filter(([key, value]) => !key.startsWith("GIT_") && value !== undefined),
  ) as NodeJS.ProcessEnv;
  environment.GIT_OPTIONAL_LOCKS = "0";
  environment.GIT_PAGER = "cat";
  environment.GIT_TERMINAL_PROMPT = "0";
  const args = [
    "--no-pager",
    "--no-optional-locks",
    "--no-replace-objects",
    "--literal-pathspecs",
    "-c", "core.fsmonitor=false",
    "-c", "core.untrackedCache=false",
    ...commandArgs,
  ];

  return new Promise<GitProcessResult>((resolve, reject) => {
    const child = spawn("git", args, { cwd, env: environment, shell: false, stdio: ["ignore", "pipe", "pipe"] });
    const stderrChunks: Buffer[] = [];
    let stderrBytes = 0;
    let stderrTruncated = false;
    let timedOut = false;
    let stdoutError: unknown = null;
    const timeout = setTimeout(() => {
      timedOut = true;
      child.kill("SIGKILL");
    }, GIT_TIMEOUT_MS);
    child.stdout.on("data", (chunk: Buffer) => {
      if (stdoutError !== null) return;
      try {
        onStdout(chunk);
      } catch (error) {
        stdoutError = error;
        child.kill("SIGKILL");
      }
    });
    child.stderr.on("data", (chunk: Buffer) => {
      const remaining = STDERR_LIMIT_BYTES - stderrBytes;
      if (remaining > 0) {
        const captured = chunk.subarray(0, remaining);
        stderrChunks.push(captured);
        stderrBytes += captured.length;
      }
      if (chunk.length > remaining) stderrTruncated = true;
    });
    child.on("error", (error) => {
      clearTimeout(timeout);
      reject(new GitInspectionError("git_unavailable", `Git could not be started: ${error.message}`));
    });
    child.on("close", (code, signal) => {
      clearTimeout(timeout);
      const stderr = Buffer.concat(stderrChunks).toString("utf8").trim();
      if (stdoutError !== null) {
        reject(stdoutError instanceof GitInspectionError
          ? stdoutError
          : new GitInspectionError("git_output_invalid", stdoutError instanceof Error ? stdoutError.message : String(stdoutError)));
        return;
      }
      if (timedOut) {
        reject(new GitInspectionError("git_timeout", `Git inspection exceeded ${GIT_TIMEOUT_MS / 1000} seconds.`));
        return;
      }
      if (code !== 0) {
        const suffix = stderr.length > 0 ? `: ${stderr}${stderrTruncated ? " [stderr truncated]" : ""}` : "";
        reject(new GitInspectionError("git_command_failed", `Read-only Git command failed (exit ${code ?? signal ?? "unknown"})${suffix}`));
        return;
      }
      resolve({ stderr, stderrTruncated });
    });
  });
}
