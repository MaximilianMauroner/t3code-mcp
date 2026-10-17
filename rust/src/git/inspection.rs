pub use super::GitInspectionError;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use std::sync::Arc;

use super::{
    GitAuditSink, GitError, GitPurpose, GitRunner, blocking_io, complete_on_drop, is_commit,
    observed_at, strings,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitDiffMode {
    Unstaged,
    Staged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitWorkspaceSource {
    ProjectWorkspace,
    ThreadWorktree,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitWorkspaceSelection {
    pub environment_id: String,
    pub project_id: String,
    pub project_title: String,
    pub project_workspace_root: String,
    pub thread_id: Option<String>,
    pub thread_branch: Option<String>,
    pub thread_worktree_path: Option<String>,
    pub source: GitWorkspaceSource,
    pub selected_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitWorkspaceIdentity {
    #[serde(flatten)]
    pub selection: GitWorkspaceSelection,
    pub resolved_path: String,
    pub repository_root: String,
    pub git_directory: String,
    pub git_common_directory: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitChangeKind {
    Ordinary,
    RenameOrCopy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitChange {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_path: Option<String>,
    pub kind: GitChangeKind,
    pub index_status: String,
    pub worktree_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<String>,
    pub submodule: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitConflict {
    pub path: String,
    pub index_status: String,
    pub worktree_status: String,
    pub submodule: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitUntracked {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitCategory<T> {
    pub count: usize,
    pub items: Vec<T>,
    pub truncated: bool,
}

impl<T> Default for GitCategory<T> {
    fn default() -> Self {
        Self {
            count: 0,
            items: Vec::new(),
            truncated: false,
        }
    }
}

impl<T> GitCategory<T> {
    fn add(&mut self, item: T, limit: usize) {
        self.count += 1;
        if self.items.len() < limit {
            self.items.push(item);
        }
        self.truncated = self.items.len() < self.count;
    }
    fn omitted(&self) -> usize {
        self.count - self.items.len()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitBranch {
    pub name: Option<String>,
    pub detached: bool,
    pub unborn: bool,
    pub head_commit: Option<String>,
    pub upstream: Option<String>,
    pub ahead: Option<u64>,
    pub behind: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitOmittedItems {
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
    pub conflicts: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitStatusTruncation {
    pub truncated: bool,
    pub item_limit_per_category: usize,
    pub omitted_items: GitOmittedItems,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitStatusResult {
    pub environment_id: String,
    pub observed_at: String,
    pub workspace: GitWorkspaceIdentity,
    pub branch: GitBranch,
    pub staged: GitCategory<GitChange>,
    pub unstaged: GitCategory<GitChange>,
    pub untracked: GitCategory<GitUntracked>,
    pub conflicts: GitCategory<GitConflict>,
    pub clean: bool,
    pub truncation: GitStatusTruncation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitDiffBase {
    #[serde(rename = "index")]
    Index,
    #[serde(rename = "HEAD")]
    Head,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitDiffTarget {
    WorkingTree,
    Index,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitDiffComparison {
    pub base: GitDiffBase,
    pub target: GitDiffTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitTruncationReason {
    ByteLimit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitByteTruncation {
    pub truncated: bool,
    pub reason: Option<GitTruncationReason>,
    pub max_bytes: usize,
    pub captured_bytes: usize,
    pub total_bytes: usize,
    pub omitted_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitDiffResult {
    pub environment_id: String,
    pub observed_at: String,
    pub workspace: GitWorkspaceIdentity,
    pub mode: GitDiffMode,
    pub comparison: GitDiffComparison,
    pub paths: Vec<String>,
    pub patch: String,
    pub truncation: GitByteTruncation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitCommitComparison {
    pub requested_base: String,
    pub requested_head: String,
    pub base_commit: String,
    pub head_commit: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitAttributionQuality {
    CleanBaseline,
    WorkingTreeDirty,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitAttribution {
    pub quality: GitAttributionQuality,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitCompareResult {
    pub environment_id: String,
    pub observed_at: String,
    pub workspace: GitWorkspaceIdentity,
    pub comparison: GitCommitComparison,
    pub paths: Vec<String>,
    pub patch: String,
    pub attribution: GitAttribution,
    pub truncation: GitByteTruncation,
}

#[derive(Clone)]
pub struct GitInspector {
    runner: GitRunner,
}

impl Default for GitInspector {
    fn default() -> Self {
        Self::new()
    }
}

impl GitInspector {
    pub fn new() -> Self {
        Self {
            runner: GitRunner::new(GitPurpose::Inspection),
        }
    }

    pub fn with_audit_log(mut self, audit: Arc<dyn GitAuditSink>) -> Self {
        self.runner.audit = Some(audit);
        self
    }

    pub async fn status(
        &self,
        selection: &GitWorkspaceSelection,
        item_limit_per_category: usize,
    ) -> Result<GitStatusResult, GitInspectionError> {
        let inspector = self.clone();
        let selection = selection.clone();
        complete_on_drop(async move {
            inspector
                .status_inner(&selection, item_limit_per_category)
                .await
        })
        .await
    }

    pub async fn diff(
        &self,
        selection: &GitWorkspaceSelection,
        mode: GitDiffMode,
        paths: &[String],
        max_bytes: usize,
    ) -> Result<GitDiffResult, GitInspectionError> {
        let inspector = self.clone();
        let selection = selection.clone();
        let paths = paths.to_vec();
        complete_on_drop(async move {
            inspector
                .diff_inner(&selection, mode, &paths, max_bytes)
                .await
        })
        .await
    }

    pub async fn compare(
        &self,
        selection: &GitWorkspaceSelection,
        base_revision: &str,
        head_revision: &str,
        paths: &[String],
        max_bytes: usize,
    ) -> Result<GitCompareResult, GitInspectionError> {
        let inspector = self.clone();
        let selection = selection.clone();
        let base_revision = base_revision.to_owned();
        let head_revision = head_revision.to_owned();
        let paths = paths.to_vec();
        complete_on_drop(async move {
            inspector
                .compare_inner(
                    &selection,
                    &base_revision,
                    &head_revision,
                    &paths,
                    max_bytes,
                )
                .await
        })
        .await
    }

    async fn status_inner(
        &self,
        selection: &GitWorkspaceSelection,
        item_limit_per_category: usize,
    ) -> Result<GitStatusResult, GitInspectionError> {
        let workspace = identify_workspace(&self.runner, selection).await?;
        let (status, _) = self
            .runner
            .run(
                Path::new(&workspace.resolved_path),
                &strings(&[
                    "status",
                    "--porcelain=v2",
                    "--branch",
                    "-z",
                    "--untracked-files=all",
                    "--ignore-submodules=none",
                ]),
                &[0],
                StatusAccumulator::default(),
                move |status, chunk| status.read(chunk, item_limit_per_category),
            )
            .await?;
        if !status.pending_record.is_empty() || status.pending_rename.is_some() {
            return Err(GitError::new(
                "git_output_invalid",
                "Git status returned an incomplete porcelain record.",
            )
            .into());
        }
        let omitted_items = GitOmittedItems {
            staged: status.staged.omitted(),
            unstaged: status.unstaged.omitted(),
            untracked: status.untracked.omitted(),
            conflicts: status.conflicts.omitted(),
        };
        let truncated = status.staged.truncated
            || status.unstaged.truncated
            || status.untracked.truncated
            || status.conflicts.truncated;
        let clean = status.staged.count
            + status.unstaged.count
            + status.untracked.count
            + status.conflicts.count
            == 0;
        Ok(GitStatusResult {
            environment_id: selection.environment_id.clone(),
            observed_at: observed_at(),
            workspace,
            branch: status.branch,
            staged: status.staged,
            unstaged: status.unstaged,
            untracked: status.untracked,
            conflicts: status.conflicts,
            clean,
            truncation: GitStatusTruncation {
                truncated,
                item_limit_per_category,
                omitted_items,
            },
        })
    }

    async fn diff_inner(
        &self,
        selection: &GitWorkspaceSelection,
        mode: GitDiffMode,
        paths: &[String],
        max_bytes: usize,
    ) -> Result<GitDiffResult, GitInspectionError> {
        let workspace = identify_workspace(&self.runner, selection).await?;
        validate_paths(paths)?;
        let mut args = diff_args();
        if mode == GitDiffMode::Staged {
            args.push("--cached".into());
        }
        append_paths(&mut args, paths);
        let (patch, truncation) =
            capture_patch(&self.runner, &workspace.resolved_path, &args, max_bytes).await?;
        Ok(GitDiffResult {
            environment_id: selection.environment_id.clone(),
            observed_at: observed_at(),
            workspace,
            mode,
            comparison: match mode {
                GitDiffMode::Staged => GitDiffComparison {
                    base: GitDiffBase::Head,
                    target: GitDiffTarget::Index,
                },
                GitDiffMode::Unstaged => GitDiffComparison {
                    base: GitDiffBase::Index,
                    target: GitDiffTarget::WorkingTree,
                },
            },
            paths: paths.to_vec(),
            patch,
            truncation,
        })
    }

    async fn compare_inner(
        &self,
        selection: &GitWorkspaceSelection,
        base_revision: &str,
        head_revision: &str,
        paths: &[String],
        max_bytes: usize,
    ) -> Result<GitCompareResult, GitInspectionError> {
        let workspace = identify_workspace(&self.runner, selection).await?;
        validate_paths(paths)?;
        validate_revision(base_revision)?;
        validate_revision(head_revision)?;
        let cwd = Path::new(&workspace.resolved_path);
        let base_commit = resolve_commit(&self.runner, cwd, base_revision).await?;
        let head_commit = resolve_commit(&self.runner, cwd, head_revision).await?;
        let (dirty, _) = self
            .runner
            .run(
                cwd,
                &strings(&[
                    "status",
                    "--porcelain=v1",
                    "--untracked-files=all",
                    "--ignore-submodules=none",
                ]),
                &[0],
                false,
                |dirty, chunk| {
                    *dirty |= !chunk.is_empty();
                    Ok(())
                },
            )
            .await?;
        let mut args = diff_args();
        args.extend([base_commit.clone(), head_commit.clone()]);
        append_paths(&mut args, paths);
        let (patch, truncation) =
            capture_patch(&self.runner, &workspace.resolved_path, &args, max_bytes).await?;
        Ok(GitCompareResult {
            environment_id: selection.environment_id.clone(),
            observed_at: observed_at(),
            workspace,
            comparison: GitCommitComparison {
                requested_base: base_revision.into(),
                requested_head: head_revision.into(),
                base_commit,
                head_commit,
            },
            paths: paths.to_vec(),
            patch,
            truncation,
            attribution: if dirty {
                GitAttribution {
                    quality: GitAttributionQuality::WorkingTreeDirty,
                    detail: concat!(
                        "The committed comparison is exact, but uncommitted workspace changes ",
                        "require separate status/diff inspection and cannot be attributed ",
                        "to this task automatically."
                    )
                    .into(),
                }
            } else {
                GitAttribution {
                    quality: GitAttributionQuality::CleanBaseline,
                    detail: "The working tree was clean when this comparison was observed.".into(),
                }
            },
        })
    }
}

async fn identify_workspace(
    runner: &GitRunner,
    selection: &GitWorkspaceSelection,
) -> Result<GitWorkspaceIdentity, GitError> {
    let path = &selection.selected_path;
    let map_error = |error: GitError| {
        if error.code == "ENOENT" {
            error.relabel(
                "workspace_missing",
                format!("T3 workspace path does not exist on the gateway host: {path}"),
            )
        } else {
            error.relabel(
                "workspace_unreadable",
                format!("T3 workspace path cannot be inspected: {path}"),
            )
        }
    };
    let metadata = blocking_io("stat", Path::new(path), |path| fs::metadata(path))
        .await
        .map_err(map_error)?;
    if !metadata.is_dir() {
        return Err(GitError::new(
            "workspace_not_directory",
            format!("T3 workspace path is not a directory: {path}"),
        ));
    }
    let resolved = blocking_io("realpath", Path::new(path), |path| fs::canonicalize(path))
        .await
        .map_err(map_error)?;
    let (output, _) = runner
        .run(
            &resolved,
            &strings(&[
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--absolute-git-dir",
                "--git-common-dir",
            ]),
            &[0],
            String::new(),
            |output, chunk| {
                output.push_str(&String::from_utf8_lossy(chunk));
                Ok(())
            },
        )
        .await
        .map_err(|error| {
            if error.code == "git_command_failed" {
                error.relabel(
                    "not_git_worktree",
                    format!("T3 workspace is not a readable Git worktree: {path}"),
                )
            } else {
                error
            }
        })?;
    let mut lines: Vec<_> = output.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    if lines.len() != 3 || lines.iter().any(|line| line.is_empty()) {
        return Err(GitError::new(
            "git_identity_ambiguous",
            format!("Git returned ambiguous worktree identity for T3 workspace: {path}"),
        ));
    }
    Ok(GitWorkspaceIdentity {
        selection: selection.clone(),
        resolved_path: resolved.to_string_lossy().into_owned(),
        repository_root: lines[0].into(),
        git_directory: lines[1].into(),
        git_common_directory: lines[2].into(),
    })
}

fn validate_paths(paths: &[String]) -> Result<(), GitError> {
    for candidate in paths {
        let bytes = candidate.as_bytes();
        let windows_absolute = bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\');
        let message = if candidate.contains('\0') {
            Some("Git path filters cannot contain NUL bytes.".to_owned())
        } else if candidate.starts_with('/') || windows_absolute || candidate.starts_with("\\\\") {
            Some(format!(
                "Git path filters must be relative to the selected T3 workspace: {candidate}"
            ))
        } else if candidate.starts_with(':') {
            Some(format!("Git pathspec magic is not allowed: {candidate}"))
        } else if candidate.split(['/', '\\']).any(|segment| segment == "..") {
            Some(format!(
                "Git path filters cannot traverse outside the selected T3 workspace: {candidate}"
            ))
        } else {
            None
        };
        if let Some(message) = message {
            return Err(GitError::new("invalid_git_path", message));
        }
    }
    Ok(())
}

fn validate_revision(revision: &str) -> Result<(), GitError> {
    if !(1..=200).contains(&revision.len())
        || !revision.as_bytes()[0].is_ascii_alphanumeric()
        || revision.contains("..")
        || !revision
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._/@{}~^:+-".contains(&byte))
    {
        return Err(GitError::new(
            "invalid_git_revision",
            "Git revisions must be explicit names or object IDs without ranges, whitespace, or option-like prefixes.",
        ));
    }
    Ok(())
}

async fn resolve_commit(
    runner: &GitRunner,
    cwd: &Path,
    revision: &str,
) -> Result<String, GitError> {
    let (output, _) = runner.run(
        cwd, &strings(&["rev-parse", "--verify", "--end-of-options", &format!("{revision}^{{commit}}")]),
        &[0], String::new(), |output, chunk| { output.push_str(&String::from_utf8_lossy(chunk)); Ok(()) },
    ).await.map_err(|error| {
        if error.code == "git_command_failed" {
            error.relabel("git_revision_not_found", format!("Git revision {revision:?} does not resolve to a commit in the selected workspace."))
        } else { error }
    })?;
    let commit = output.trim().to_owned();
    if !is_commit(&commit) {
        return Err(GitError::new(
            "git_revision_invalid",
            format!("Git returned an invalid commit identity for revision {revision:?}."),
        ));
    }
    Ok(commit)
}

fn diff_args() -> Vec<String> {
    strings(&[
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--find-renames",
        "--find-copies",
    ])
}

fn append_paths(args: &mut Vec<String>, paths: &[String]) {
    if !paths.is_empty() {
        args.push("--".into());
        args.extend_from_slice(paths);
    }
}

async fn capture_patch(
    runner: &GitRunner,
    cwd: &str,
    args: &[String],
    max_bytes: usize,
) -> Result<(String, GitByteTruncation), GitError> {
    let ((captured, total_bytes), _) = runner
        .run(
            Path::new(cwd),
            args,
            &[0],
            (Vec::<u8>::new(), 0usize),
            move |(captured, total_bytes), chunk| {
                *total_bytes += chunk.len();
                let keep = chunk.len().min(max_bytes.saturating_sub(captured.len()));
                captured.extend_from_slice(&chunk[..keep]);
                Ok(())
            },
        )
        .await?;
    let captured_bytes = captured.len();
    let truncated = total_bytes > captured_bytes;
    Ok((
        String::from_utf8_lossy(&captured).into_owned(),
        GitByteTruncation {
            truncated,
            reason: truncated.then_some(GitTruncationReason::ByteLimit),
            max_bytes,
            captured_bytes,
            total_bytes,
            omitted_bytes: total_bytes - captured_bytes,
        },
    ))
}

#[derive(Default)]
struct StatusAccumulator {
    pending_record: Vec<u8>,
    branch: GitBranch,
    staged: GitCategory<GitChange>,
    unstaged: GitCategory<GitChange>,
    untracked: GitCategory<GitUntracked>,
    conflicts: GitCategory<GitConflict>,
    pending_rename: Option<GitChange>,
}

impl StatusAccumulator {
    fn read(&mut self, chunk: &[u8], limit: usize) -> Result<(), GitError> {
        let mut start = 0;
        for (index, byte) in chunk.iter().enumerate() {
            if *byte == 0 {
                self.pending_record.extend_from_slice(&chunk[start..index]);
                let record = String::from_utf8_lossy(&self.pending_record).into_owned();
                self.record(&record, limit)?;
                self.pending_record.clear();
                start = index + 1;
            }
        }
        self.pending_record.extend_from_slice(&chunk[start..]);
        Ok(())
    }

    fn add_change(&mut self, change: GitChange, limit: usize) {
        if change.index_status != "." {
            self.staged.add(change.clone(), limit);
        }
        if change.worktree_status != "." {
            self.unstaged.add(change, limit);
        }
    }

    fn record(&mut self, record: &str, limit: usize) -> Result<(), GitError> {
        if let Some(mut change) = self.pending_rename.take() {
            change.original_path = Some(record.into());
            self.add_change(change, limit);
        } else if let Some(oid) = record.strip_prefix("# branch.oid ") {
            self.branch.unborn = oid == "(initial)";
            self.branch.head_commit = (!self.branch.unborn).then(|| oid.into());
        } else if let Some(head) = record.strip_prefix("# branch.head ") {
            self.branch.detached = head == "(detached)";
            self.branch.name = (!self.branch.detached).then(|| head.into());
        } else if let Some(upstream) = record.strip_prefix("# branch.upstream ") {
            self.branch.upstream = Some(upstream.into());
        } else if let Some(ab) = record.strip_prefix("# branch.ab ") {
            if let Some((ahead, behind)) = ab
                .strip_prefix('+')
                .and_then(|value| value.split_once(" -"))
                && !ahead.is_empty()
                && !behind.is_empty()
                && ahead.bytes().all(|b| b.is_ascii_digit())
                && behind.bytes().all(|b| b.is_ascii_digit())
            {
                self.branch.ahead = ahead.parse().ok();
                self.branch.behind = behind.parse().ok();
            }
        } else if let Some(path) = record.strip_prefix("? ") {
            self.untracked
                .add(GitUntracked { path: path.into() }, limit);
        } else if record.starts_with("! ") || record.is_empty() {
        } else {
            let count = match record.as_bytes().first() {
                Some(b'1') if record.starts_with("1 ") => 8,
                Some(b'2') if record.starts_with("2 ") => 9,
                Some(b'u') if record.starts_with("u ") => 10,
                _ => return Err(invalid_status(record)),
            };
            let fields: Vec<_> = record.splitn(count + 1, ' ').collect();
            if fields.len() != count + 1
                || fields[1].is_empty()
                || fields[2].is_empty()
                || (count == 9 && fields[8].is_empty())
            {
                return Err(invalid_status(record));
            }
            let index_status = fields[1].chars().next().unwrap_or('.').to_string();
            let worktree_status = fields[1].chars().nth(1).unwrap_or('.').to_string();
            if count == 10 {
                self.conflicts.add(
                    GitConflict {
                        path: fields[count].into(),
                        index_status,
                        worktree_status,
                        submodule: fields[2].into(),
                    },
                    limit,
                );
            } else {
                let change = GitChange {
                    path: fields[count].into(),
                    original_path: None,
                    kind: if count == 9 {
                        GitChangeKind::RenameOrCopy
                    } else {
                        GitChangeKind::Ordinary
                    },
                    index_status,
                    worktree_status,
                    score: (count == 9).then(|| fields[8].into()),
                    submodule: fields[2].into(),
                };
                if count == 9 {
                    self.pending_rename = Some(change);
                } else {
                    self.add_change(change, limit);
                }
            }
        }
        Ok(())
    }
}

fn invalid_status(record: &str) -> GitError {
    GitError::new(
        "git_output_invalid",
        format!(
            "Git status returned an unrecognized porcelain record: {}",
            record.chars().take(120).collect::<String>()
        ),
    )
}
