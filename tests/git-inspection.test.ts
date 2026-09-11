import { execFile } from "node:child_process";
import { access, mkdtemp, mkdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import { afterEach, describe, expect, it } from "vitest";
import { GitInspectionError, GitInspector, type GitWorkspaceSelection } from "../src/git/inspection.js";

const execute = promisify(execFile);
const directories: string[] = [];

afterEach(async () => {
  await Promise.all(directories.splice(0).map((directory) => rm(directory, { recursive: true, force: true })));
});

async function git(cwd: string, ...args: string[]): Promise<string> {
  const result = await execute("git", args, { cwd, env: { ...process.env, GIT_CONFIG_NOSYSTEM: "1" } });
  return result.stdout;
}

async function repository(): Promise<string> {
  const directory = await mkdtemp(join(tmpdir(), "t3-git-inspection-"));
  directories.push(directory);
  await git(directory, "init", "-b", "main");
  await git(directory, "config", "user.name", "T3 Test");
  await git(directory, "config", "user.email", "t3@example.test");
  await writeFile(join(directory, "tracked.txt"), "one\ntwo\nthree\n");
  await writeFile(join(directory, "binary.dat"), Buffer.from([0, 1, 2, 3]));
  await git(directory, "add", ".");
  await git(directory, "commit", "-m", "initial");
  return directory;
}

function selection(directory: string, overrides: Partial<GitWorkspaceSelection> = {}): GitWorkspaceSelection {
  return {
    environmentId: "environment-test",
    projectId: "project-test",
    projectTitle: "Test project",
    projectWorkspaceRoot: directory,
    threadId: null,
    threadBranch: null,
    threadWorktreePath: null,
    source: "project_workspace",
    selectedPath: directory,
    ...overrides,
  };
}

describe("GitInspector", () => {
  it("separates staged, unstaged, untracked, rename, and binary changes", async () => {
    const directory = await repository();
    await git(directory, "mv", "tracked.txt", "renamed.txt");
    await git(directory, "add", "-A");
    await writeFile(join(directory, "renamed.txt"), "one\ntwo\nthree\nworking tree\n");
    await writeFile(join(directory, "binary.dat"), Buffer.from([0, 1, 9, 3]));
    await writeFile(join(directory, "untracked file.txt"), "new\n");

    const inspector = new GitInspector();
    const status = await inspector.status(selection(directory), 100);
    const staged = await inspector.diff(selection(directory), "staged", [], 100_000);
    const unstaged = await inspector.diff(selection(directory), "unstaged", [], 100_000);

    expect(status.workspace).toMatchObject({
      source: "project_workspace",
      selectedPath: directory,
      repositoryRoot: directory,
    });
    expect(status.branch).toMatchObject({ name: "main", detached: false, unborn: false });
    expect(status.clean).toBe(false);
    expect(status.staged.items).toContainEqual(expect.objectContaining({
      path: "renamed.txt",
      originalPath: "tracked.txt",
      kind: "rename_or_copy",
      indexStatus: "R",
    }));
    expect(status.unstaged.items.map((item) => item.path)).toEqual(expect.arrayContaining(["renamed.txt", "binary.dat"]));
    expect(status.untracked).toMatchObject({ count: 1, items: [{ path: "untracked file.txt" }], truncated: false });
    expect(staged.comparison).toEqual({ base: "HEAD", target: "index" });
    expect(staged.patch).toContain("rename from tracked.txt");
    expect(unstaged.comparison).toEqual({ base: "index", target: "working_tree" });
    expect(unstaged.patch).toContain("+working tree");
    expect(unstaged.patch).toContain("Binary files a/binary.dat and b/binary.dat differ");
    expect(unstaged.truncation.truncated).toBe(false);
  });

  it("applies literal relative path filters and rejects escape-oriented pathspecs", async () => {
    const directory = await repository();
    await mkdir(join(directory, "nested"));
    await writeFile(join(directory, "tracked.txt"), "changed\n");
    await writeFile(join(directory, "nested", "wanted.txt"), "wanted\n");
    await git(directory, "add", "nested/wanted.txt");
    await git(directory, "commit", "-m", "nested");
    await writeFile(join(directory, "nested", "wanted.txt"), "filtered change\n");
    await writeFile(join(directory, "--output=attacker"), "not an option\n");
    await writeFile(join(directory, "$(touch injected)"), "literal shell characters\n");
    await git(directory, "add", "$(touch injected)");
    await git(directory, "commit", "-m", "shell-like filename");
    await writeFile(join(directory, "$(touch injected)"), "changed literal shell characters\n");
    await git(directory, "config", "diff.external", "/bin/false");

    const inspector = new GitInspector();
    const filtered = await inspector.diff(selection(directory), "unstaged", ["nested/wanted.txt"], 100_000);
    const leadingDash = await inspector.diff(selection(directory), "unstaged", ["--output=attacker"], 100_000);
    const shellLike = await inspector.diff(selection(directory), "unstaged", ["$(touch injected)"], 100_000);

    expect(filtered.patch).toContain("filtered change");
    expect(filtered.patch).not.toContain("changed");
    expect(leadingDash.patch).toBe("");
    expect(shellLike.patch).toContain("changed literal shell characters");
    await expect(access(join(directory, "injected"))).rejects.toMatchObject({ code: "ENOENT" });
    for (const path of ["../outside", "/etc/passwd", ":(glob)**", "C:\\outside"] as const) {
      await expect(inspector.diff(selection(directory), "unstaged", [path], 100_000))
        .rejects.toMatchObject({ code: "invalid_git_path" });
    }
  });

  it("does not modify the worktree or index while inspecting it", async () => {
    const directory = await repository();
    await writeFile(join(directory, "tracked.txt"), "changed\n");
    const indexPath = (await git(directory, "rev-parse", "--path-format=absolute", "--git-path", "index")).trim();
    const indexBefore = await readFile(indexPath);
    const indexStatBefore = await stat(indexPath);
    const trackedBefore = await readFile(join(directory, "tracked.txt"));
    const inspector = new GitInspector();

    await inspector.status(selection(directory), 100);
    await inspector.diff(selection(directory), "unstaged", [], 100_000);
    await inspector.diff(selection(directory), "staged", [], 100_000);

    expect(await readFile(indexPath)).toEqual(indexBefore);
    expect((await stat(indexPath)).mtimeMs).toBe(indexStatBefore.mtimeMs);
    expect(await readFile(join(directory, "tracked.txt"))).toEqual(trackedBefore);
  });

  it("reports exact status and diff truncation metadata", async () => {
    const directory = await repository();
    for (let index = 0; index < 8; index += 1) {
      await writeFile(join(directory, `untracked-${index}.txt`), `${"x".repeat(8_000)}-${index}\n`);
    }
    await writeFile(join(directory, "tracked.txt"), `${"large line\n".repeat(20_000)}`);
    const inspector = new GitInspector();

    const status = await inspector.status(selection(directory), 3);
    const diff = await inspector.diff(selection(directory), "unstaged", ["tracked.txt"], 4_096);

    expect(status.untracked).toMatchObject({ count: 8, truncated: true });
    expect(status.untracked.items).toHaveLength(3);
    expect(status.truncation).toMatchObject({
      truncated: true,
      itemLimitPerCategory: 3,
      omittedItems: { untracked: 5 },
    });
    expect(diff.truncation).toMatchObject({
      truncated: true,
      reason: "byte_limit",
      maxBytes: 4_096,
      capturedBytes: 4_096,
    });
    expect(diff.truncation.totalBytes).toBeGreaterThan(4_096);
    expect(diff.truncation.omittedBytes).toBe(diff.truncation.totalBytes - 4_096);
    expect(Buffer.byteLength(diff.patch)).toBeLessThanOrEqual(4_098);
  });

  it("reports merge conflicts structurally", async () => {
    const directory = await repository();
    await git(directory, "switch", "-c", "other");
    await writeFile(join(directory, "tracked.txt"), "other branch\n");
    await git(directory, "commit", "-am", "other change");
    await git(directory, "switch", "main");
    await writeFile(join(directory, "tracked.txt"), "main branch\n");
    await git(directory, "commit", "-am", "main change");
    await expect(git(directory, "merge", "other")).rejects.toBeTruthy();

    const status = await new GitInspector().status(selection(directory), 100);

    expect(status.conflicts).toMatchObject({ count: 1, truncated: false });
    expect(status.conflicts.items[0]).toMatchObject({ path: "tracked.txt", indexStatus: "U", worktreeStatus: "U" });
    expect(status.staged.count).toBe(0);
    expect(status.unstaged.count).toBe(0);
  });

  it("rejects missing and non-Git workspaces clearly", async () => {
    const directory = await mkdtemp(join(tmpdir(), "t3-not-git-"));
    directories.push(directory);
    const missing = join(directory, "missing");
    const bare = await mkdtemp(join(tmpdir(), "t3-bare-git-"));
    directories.push(bare);
    await git(bare, "init", "--bare");
    const inspector = new GitInspector();

    await expect(inspector.status(selection(missing), 100)).rejects.toEqual(
      expect.objectContaining<Partial<GitInspectionError>>({ code: "workspace_missing" }),
    );
    await expect(inspector.status(selection(directory), 100)).rejects.toEqual(
      expect.objectContaining<Partial<GitInspectionError>>({ code: "not_git_worktree" }),
    );
    await expect(inspector.status(selection(bare), 100)).rejects.toEqual(
      expect.objectContaining<Partial<GitInspectionError>>({ code: "not_git_worktree" }),
    );
  });
});
