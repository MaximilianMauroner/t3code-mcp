pub use super::GitWorktreeError;
use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, OnceCell};

use serde::{Deserialize, Serialize};

use super::{
    GitAuditSink, GitError, GitPurpose, GitRunner, blocking_io, complete_on_drop, is_commit,
    strings,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedWorktreeRequest {
    pub project_id: String,
    pub project_workspace_root: String,
    pub base_branch: String,
    pub start_from_origin: bool,
    pub operation_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedWorktree {
    pub branch: String,
    pub path: String,
    pub base_commit: String,
}

type Preparation = OnceCell<Result<ManagedWorktree, GitError>>;

#[derive(Clone)]
pub struct GitWorktreeManager {
    configured_root: Option<PathBuf>,
    pending: Arc<Mutex<HashMap<PathBuf, Arc<Preparation>>>>,
    runner: GitRunner,
}

impl Default for GitWorktreeManager {
    fn default() -> Self {
        Self::new(None)
    }
}

impl GitWorktreeManager {
    pub fn new(configured_root: Option<PathBuf>) -> Self {
        Self {
            configured_root,
            pending: Arc::new(Mutex::new(HashMap::new())),
            runner: GitRunner::new(GitPurpose::Worktree),
        }
    }

    pub fn with_audit_log(mut self, audit: Arc<dyn GitAuditSink>) -> Self {
        self.runner.audit = Some(audit);
        self
    }

    pub async fn prepare(
        &self,
        input: &ManagedWorktreeRequest,
    ) -> Result<ManagedWorktree, GitWorktreeError> {
        let manager = self.clone();
        let input = input.clone();
        complete_on_drop(async move { manager.prepare_inner(&input).await.map_err(Into::into) })
            .await
    }

    async fn prepare_inner(
        &self,
        input: &ManagedWorktreeRequest,
    ) -> Result<ManagedWorktree, GitError> {
        let project_root = blocking_io(
            "realpath",
            Path::new(&input.project_workspace_root),
            |path| fs::canonicalize(path),
        )
        .await
        .map_err(|error| {
            error.relabel(
                "project_workspace_missing",
                format!(
                    "The T3 project workspace is not accessible: {}",
                    input.project_workspace_root
                ),
            )
        })?;
        assert_directory(&project_root).await?;
        validate_branch(&self.runner, &project_root, &input.base_branch).await?;
        let suffix: String = input
            .operation_key
            .to_lowercase()
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .take(16)
            .collect();
        if suffix.len() < 8 {
            return Err(GitError::new(
                "invalid_operation_key",
                "The durable operation key cannot identify a managed worktree.",
            ));
        }
        let branch = format!("t3code/mcp-{suffix}");
        let root = match &self.configured_root {
            Some(root) => root.join(safe_segment(&input.project_id)),
            None => project_root
                .parent()
                .unwrap_or(&project_root)
                .join(".t3-code-mcp-worktrees")
                .join(format!(
                    "{}-{}",
                    safe_segment(
                        &project_root
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                    ),
                    safe_segment(&input.project_id)
                )),
        };
        let worktree_path = absolute_normalized(&root.join(suffix)).await?;
        let preparation = {
            let mut pending = self.pending.lock().await;
            Arc::clone(
                pending
                    .entry(worktree_path.clone())
                    .or_insert_with(|| Arc::new(Preparation::new())),
            )
        };
        let result = preparation
            .get_or_init(|| {
                prepare_once(&self.runner, &project_root, &worktree_path, &branch, input)
            })
            .await
            .clone();
        let mut pending = self.pending.lock().await;
        if pending
            .get(&worktree_path)
            .is_some_and(|current| Arc::ptr_eq(current, &preparation))
        {
            pending.remove(&worktree_path);
        }
        result
    }
}

async fn prepare_once(
    runner: &GitRunner,
    project_root: &Path,
    worktree_path: &Path,
    branch: &str,
    input: &ManagedWorktreeRequest,
) -> Result<ManagedWorktree, GitError> {
    let exists = match blocking_io("stat", worktree_path, |path| fs::metadata(path)).await {
        Ok(_) => true,
        Err(error) if error.code == "ENOENT" => false,
        Err(error) => return Err(error),
    };
    let base_commit = if exists {
        verify_existing(runner, project_root, worktree_path, branch).await?
    } else {
        blocking_io(
            "mkdir",
            worktree_path.parent().expect("absolute worktree parent"),
            |path| {
                let mut builder = fs::DirBuilder::new();
                builder.recursive(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                builder.create(path)
            },
        )
        .await?;
        let mut base_ref = format!("refs/heads/{}", input.base_branch);
        if input.start_from_origin
            && worktree_git(
                runner,
                project_root,
                &["remote", "get-url", "--", "origin"],
                &[0, 2],
            )
            .await?
            .0 == 0
        {
            worktree_git(runner, project_root, &["fetch", "--", "origin"], &[0]).await?;
            base_ref = format!("refs/remotes/origin/{}", input.base_branch);
        }
        let base = worktree_git(
            runner,
            project_root,
            &["rev-parse", "--verify", &format!("{base_ref}^{{commit}}")],
            &[0],
        )
        .await?
        .1
        .trim()
        .to_owned();
        if !is_commit(&base) {
            return Err(GitError::new(
                "git_base_invalid",
                format!(
                    "Git returned an invalid commit for base branch {}.",
                    input.base_branch
                ),
            ));
        }
        let branch_exists = worktree_git(
            runner,
            project_root,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
            &[0, 1],
        )
        .await?
        .0 == 0;
        let path = worktree_path.to_string_lossy();
        if branch_exists {
            worktree_git(
                runner,
                project_root,
                &["worktree", "add", "--", &path, branch],
                &[0],
            )
            .await?;
        } else {
            worktree_git(
                runner,
                project_root,
                &["worktree", "add", "-b", branch, "--", &path, &base],
                &[0],
            )
            .await?;
        }
        verify_existing(runner, project_root, worktree_path, branch).await?
    };
    Ok(ManagedWorktree {
        branch: branch.into(),
        path: worktree_path.to_string_lossy().into_owned(),
        base_commit,
    })
}

async fn validate_branch(runner: &GitRunner, cwd: &Path, branch: &str) -> Result<(), GitError> {
    let message = || format!("Invalid worktree base branch: {branch}");
    if branch.starts_with('-') {
        return Err(GitError::new("invalid_base_branch", message()));
    }
    worktree_git(runner, cwd, &["check-ref-format", "--branch", branch], &[0])
        .await
        .map_err(|error| error.relabel("invalid_base_branch", message()))?;
    Ok(())
}

async fn verify_existing(
    runner: &GitRunner,
    project_root: &Path,
    worktree_path: &Path,
    expected_branch: &str,
) -> Result<String, GitError> {
    assert_directory(worktree_path).await?;
    let (project_common, worktree_common, branch, commit) = tokio::try_join!(
        worktree_git(
            runner,
            project_root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            &[0]
        ),
        worktree_git(
            runner,
            worktree_path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            &[0]
        ),
        worktree_git(runner, worktree_path, &["branch", "--show-current"], &[0]),
        worktree_git(
            runner,
            worktree_path,
            &["rev-parse", "--verify", "HEAD^{commit}"],
            &[0]
        ),
    )?;
    let project_common = project_common.1;
    let worktree_common = worktree_common.1;
    let branch = branch.1;
    let commit = commit.1;
    let expected_common = blocking_io("realpath", Path::new(project_common.trim()), |path| {
        fs::canonicalize(path)
    })
    .await?;
    let observed_common = blocking_io("realpath", Path::new(worktree_common.trim()), |path| {
        fs::canonicalize(path)
    })
    .await?;
    if expected_common != observed_common {
        return Err(GitError::new(
            "worktree_identity_mismatch",
            format!(
                "Existing path is not a worktree of the selected project: {}",
                worktree_path.display()
            ),
        ));
    }
    let observed_branch = branch.trim();
    if observed_branch != expected_branch {
        return Err(GitError::new(
            "worktree_branch_mismatch",
            format!(
                "Existing managed worktree {} uses branch {}, expected {expected_branch}.",
                worktree_path.display(),
                if observed_branch.is_empty() {
                    "detached"
                } else {
                    observed_branch
                }
            ),
        ));
    }
    Ok(commit.trim().into())
}

async fn assert_directory(path: &Path) -> Result<(), GitError> {
    let metadata = blocking_io("stat", path, |path| fs::metadata(path))
        .await
        .map_err(|error| {
            error.relabel(
                "workspace_not_directory",
                format!("Workspace is not a readable directory: {}", path.display()),
            )
        })?;
    if !metadata.is_dir() {
        return Err(GitError::new(
            "workspace_not_directory",
            format!("Workspace is not a readable directory: {}", path.display()),
        ));
    }
    Ok(())
}

fn safe_segment(value: &str) -> String {
    let mut normalized = String::new();
    for character in value.to_lowercase().chars() {
        let character = if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        {
            character
        } else {
            '-'
        };
        if character == '-' && normalized.ends_with('-') {
            continue;
        }
        normalized.push(character);
        if normalized.len() == 80 {
            break;
        }
    }
    if normalized.is_empty() {
        "project".into()
    } else {
        normalized
    }
}

async fn absolute_normalized(path: &Path) -> Result<PathBuf, GitError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        blocking_io("getcwd", Path::new("."), |_| std::env::current_dir())
            .await?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component),
        }
    }
    Ok(normalized)
}

async fn worktree_git(
    runner: &GitRunner,
    cwd: &Path,
    args: &[&str],
    accepted: &[i32],
) -> Result<(i32, String), GitError> {
    let runner = runner.clone();
    let cwd = cwd.to_path_buf();
    let args = strings(args);
    let accepted = accepted.to_vec();
    complete_on_drop(async move {
        let (captured, evidence) = runner
            .run(
                &cwd,
                &args,
                &accepted,
                Vec::<u8>::new(),
                |captured, chunk| {
                    let keep = chunk
                        .len()
                        .min((64 * 1024usize).saturating_sub(captured.len()));
                    captured.extend_from_slice(&chunk[..keep]);
                    Ok(())
                },
            )
            .await?;
        Ok((
            evidence.exit_code.unwrap_or(-1),
            String::from_utf8_lossy(&captured).into_owned(),
        ))
    })
    .await
}
