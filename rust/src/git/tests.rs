use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::operations::audit_log::{AuditLog, AuditQueryInput, AuditSource, with_audit_context};

use super::*;

static NEXT_REPOSITORY: AtomicUsize = AtomicUsize::new(0);

struct Repository {
    root: PathBuf,
    path: PathBuf,
}

impl Repository {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/git-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT_REPOSITORY.fetch_add(1, Ordering::Relaxed)
            ));
        let path = root.join("repository");
        fs::create_dir_all(&path).unwrap();
        git(&path, &["init", "-b", "main"]);
        git(&path, &["config", "user.name", "T3 Test"]);
        git(&path, &["config", "user.email", "t3@example.test"]);
        fs::write(path.join("tracked.txt"), "one\ntwo\nthree\n").unwrap();
        git(&path, &["add", "."]);
        git(&path, &["commit", "-m", "initial"]);
        Self { root, path }
    }

    fn selection(&self) -> GitWorkspaceSelection {
        GitWorkspaceSelection {
            environment_id: "environment-test".into(),
            project_id: "project-test".into(),
            project_title: "Test project".into(),
            project_workspace_root: self.path.to_string_lossy().into_owned(),
            thread_id: None,
            thread_branch: None,
            thread_worktree_path: None,
            source: GitWorkspaceSource::ProjectWorkspace,
            selected_path: self.path.to_string_lossy().into_owned(),
        }
    }

    fn request(&self) -> ManagedWorktreeRequest {
        ManagedWorktreeRequest {
            project_id: "project-test".into(),
            project_workspace_root: self.path.to_string_lossy().into_owned(),
            base_branch: "main".into(),
            start_from_origin: false,
            operation_key: "ABCdef12-3456-7890-more".into(),
        }
    }

    fn manager(&self) -> GitWorktreeManager {
        GitWorktreeManager::new(Some(self.root.join("managed")))
    }
}

impl Drop for Repository {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let result = git_command(cwd, &strings(args)).output().unwrap();
    assert!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8_lossy(&result.stdout).into_owned()
}

#[tokio::test(flavor = "current_thread")]
async fn status_separates_nul_delimited_renames_and_bounds_categories() {
    let repository = Repository::new();
    let old = "old\n name.txt";
    let new = "new\n name.txt";
    fs::write(repository.path.join(old), "rename content\n").unwrap();
    git(&repository.path, &["add", "."]);
    git(&repository.path, &["commit", "-m", "old name"]);
    git(&repository.path, &["mv", old, new]);
    fs::write(repository.path.join(new), "rename content\nworking tree\n").unwrap();
    for name in ["untracked 1", "untracked\n2", "untracked 3"] {
        fs::write(repository.path.join(name), "new").unwrap();
    }
    let result = GitInspector::new()
        .status(&repository.selection(), 1)
        .await
        .unwrap();
    assert_eq!(result.branch.name.as_deref(), Some("main"));
    assert!(!result.clean);
    assert_eq!(result.staged.count, 1);
    assert_eq!(result.unstaged.count, 1);
    assert_eq!(result.staged.items[0].path, new);
    assert_eq!(result.staged.items[0].original_path.as_deref(), Some(old));
    assert_eq!(result.staged.items[0].kind, GitChangeKind::RenameOrCopy);
    assert_eq!(result.untracked.count, 3);
    assert_eq!(result.untracked.items.len(), 1);
    assert_eq!(result.truncation.omitted_items.untracked, 2);
    assert!(result.truncation.truncated);
    let staged = GitInspector::new()
        .diff(&repository.selection(), GitDiffMode::Staged, &[], 100_000)
        .await
        .unwrap();
    assert!(staged.patch.contains("rename from"));
    assert_eq!(staged.comparison.base, GitDiffBase::Head);
    let json = serde_json::to_value(result).unwrap();
    assert_eq!(json["workspace"]["source"], "project_workspace");
    assert!(json["workspace"].get("selection").is_none());
    assert_eq!(json["staged"]["items"][0]["kind"], "rename_or_copy");
    assert!(json["observedAt"].as_str().unwrap().ends_with('Z'));
}

#[tokio::test(flavor = "current_thread")]
async fn diff_uses_literal_paths_rejects_escapes_and_does_not_run_external_helpers() {
    let repository = Repository::new();
    for name in [
        "literal*.txt",
        "literal-other.txt",
        "--output=attacker",
        "$(touch injected)",
    ] {
        fs::write(repository.path.join(name), "old\n").unwrap();
    }
    git(&repository.path, &["add", "."]);
    git(&repository.path, &["commit", "-m", "literal paths"]);
    for name in [
        "literal*.txt",
        "literal-other.txt",
        "--output=attacker",
        "$(touch injected)",
    ] {
        fs::write(repository.path.join(name), format!("changed {name}\n")).unwrap();
    }
    fs::write(
        repository.path.join(".gitattributes"),
        "*.txt diff=unsafe\n",
    )
    .unwrap();
    git(&repository.path, &["config", "diff.external", "/bin/false"]);
    git(
        &repository.path,
        &["config", "diff.unsafe.textconv", "/bin/false"],
    );
    git(
        &repository.path,
        &["config", "core.fsmonitor", "/bin/false"],
    );
    let inspector = GitInspector::new();
    for name in ["literal*.txt", "--output=attacker", "$(touch injected)"] {
        let diff = inspector
            .diff(
                &repository.selection(),
                GitDiffMode::Unstaged,
                &[name.into()],
                100_000,
            )
            .await
            .unwrap();
        assert!(diff.patch.contains(&format!("+changed {name}")));
        assert!(!diff.patch.contains("changed literal-other.txt"));
    }
    assert!(!repository.path.join("injected").exists());
    for path in [
        "../outside",
        "x/../outside",
        "x\\..\\outside",
        "/etc/passwd",
        "C:\\outside",
        "\\\\server\\path",
        ":(glob)**",
        "bad\0path",
    ] {
        assert_eq!(
            inspector
                .diff(
                    &repository.selection(),
                    GitDiffMode::Unstaged,
                    &[path.into()],
                    100
                )
                .await
                .unwrap_err()
                .code,
            "invalid_git_path"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn patch_capture_drains_to_count_bytes_and_preserves_utf8_cutoff() {
    let repository = Repository::new();
    fs::write(
        repository.path.join("tracked.txt"),
        "large line\n".repeat(50_000),
    )
    .unwrap();
    let inspector = GitInspector::new();
    let full = inspector
        .diff(
            &repository.selection(),
            GitDiffMode::Unstaged,
            &[],
            1_000_000,
        )
        .await
        .unwrap();
    let small = inspector
        .diff(&repository.selection(), GitDiffMode::Unstaged, &[], 4096)
        .await
        .unwrap();
    assert_eq!(small.truncation.total_bytes, full.patch.len());
    assert_eq!(small.truncation.captured_bytes, 4096);
    assert_eq!(small.truncation.omitted_bytes, full.patch.len() - 4096);
    let zero = inspector
        .diff(&repository.selection(), GitDiffMode::Unstaged, &[], 0)
        .await
        .unwrap();
    assert!(zero.patch.is_empty());
    assert_eq!(zero.truncation.total_bytes, full.patch.len());
    fs::write(repository.path.join("tracked.txt"), "ééé\n").unwrap();
    let full = inspector
        .diff(&repository.selection(), GitDiffMode::Unstaged, &[], 100_000)
        .await
        .unwrap();
    let cutoff = full.patch.find("+é").unwrap() + 2;
    let partial = inspector
        .diff(&repository.selection(), GitDiffMode::Unstaged, &[], cutoff)
        .await
        .unwrap();
    assert!(partial.patch.ends_with('\u{fffd}'));
    assert_eq!(partial.truncation.captured_bytes, cutoff);
}

#[tokio::test(flavor = "current_thread")]
async fn compare_resolves_commits_and_keeps_dirty_changes_out_of_patch() {
    let repository = Repository::new();
    let base = git(&repository.path, &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    fs::write(repository.path.join("tracked.txt"), "committed change\n").unwrap();
    git(&repository.path, &["commit", "-am", "next"]);
    let inspector = GitInspector::new();
    let clean = inspector
        .compare(&repository.selection(), &base, "HEAD", &[], 100_000)
        .await
        .unwrap();
    assert!(clean.patch.contains("+committed change"));
    assert_eq!(
        clean.attribution.quality,
        GitAttributionQuality::CleanBaseline
    );
    assert_eq!(clean.comparison.base_commit, base);
    fs::write(repository.path.join("untracked"), "not committed\n").unwrap();
    let dirty = inspector
        .compare(&repository.selection(), &base, "HEAD", &[], 100_000)
        .await
        .unwrap();
    assert_eq!(
        dirty.attribution.quality,
        GitAttributionQuality::WorkingTreeDirty
    );
    assert_eq!(dirty.patch, clean.patch);
    for revision in ["--output=x", "HEAD..main", "HEAD value", "", "HEAD\0"] {
        assert_eq!(
            inspector
                .compare(&repository.selection(), revision, "HEAD", &[], 100)
                .await
                .unwrap_err()
                .code,
            "invalid_git_revision"
        );
    }
    assert_eq!(
        inspector
            .compare(&repository.selection(), "missing-ref", "HEAD", &[], 100)
            .await
            .unwrap_err()
            .code,
        "git_revision_not_found"
    );
    git(&repository.path, &["replace", &base, "HEAD"]);
    let replaced = inspector
        .compare(&repository.selection(), &base, "HEAD", &[], 100_000)
        .await
        .unwrap();
    assert_eq!(replaced.patch, clean.patch);
}

#[tokio::test(flavor = "current_thread")]
async fn inspection_preserves_index_and_reports_unborn_detached_and_conflicts() {
    let repository = Repository::new();
    fs::write(repository.path.join("tracked.txt"), "changed\n").unwrap();
    let index = repository.path.join(".git/index");
    let before = fs::read(&index).unwrap();
    let modified = fs::metadata(&index).unwrap().modified().unwrap();
    let inspector = GitInspector::new();
    inspector.status(&repository.selection(), 10).await.unwrap();
    inspector
        .diff(&repository.selection(), GitDiffMode::Unstaged, &[], 100_000)
        .await
        .unwrap();
    assert_eq!(fs::read(&index).unwrap(), before);
    assert_eq!(fs::metadata(&index).unwrap().modified().unwrap(), modified);
    git(&repository.path, &["checkout", "--", "tracked.txt"]);
    git(&repository.path, &["switch", "-c", "other"]);
    fs::write(repository.path.join("tracked.txt"), "other\n").unwrap();
    git(&repository.path, &["commit", "-am", "other"]);
    git(&repository.path, &["switch", "main"]);
    fs::write(repository.path.join("tracked.txt"), "main\n").unwrap();
    git(&repository.path, &["commit", "-am", "main"]);
    assert!(
        !git_command(&repository.path, &strings(&["merge", "other"]))
            .output()
            .unwrap()
            .status
            .success()
    );
    let status = inspector.status(&repository.selection(), 0).await.unwrap();
    assert_eq!(status.conflicts.count, 1);
    assert_eq!(status.truncation.omitted_items.conflicts, 1);
    let status = inspector.status(&repository.selection(), 1).await.unwrap();
    assert_eq!(status.conflicts.items[0].index_status, "U");
    assert_eq!(status.staged.count + status.unstaged.count, 0);
    git(&repository.path, &["merge", "--abort"]);
    git(&repository.path, &["checkout", "--detach"]);
    let detached = inspector.status(&repository.selection(), 1).await.unwrap();
    assert!(detached.branch.detached);
    assert_eq!(detached.branch.name, None);
    let unborn_path = repository.root.join("unborn");
    fs::create_dir(&unborn_path).unwrap();
    git(&unborn_path, &["init", "-b", "main"]);
    let mut selection = repository.selection();
    selection.selected_path = unborn_path.to_string_lossy().into_owned();
    let unborn = inspector.status(&selection, 1).await.unwrap();
    assert!(unborn.branch.unborn);
    assert_eq!(unborn.branch.head_commit, None);
}

#[tokio::test(flavor = "current_thread")]
async fn workspace_errors_and_thread_worktree_identity_are_explicit() {
    let repository = Repository::new();
    let mut selection = repository.selection();
    let inspector = GitInspector::new();
    let bare = repository.root.join("bare.git");
    fs::create_dir(&bare).unwrap();
    git(&bare, &["init", "--bare"]);
    for (path, expected) in [
        (repository.root.join("missing"), "workspace_missing"),
        (
            repository.path.join("tracked.txt"),
            "workspace_not_directory",
        ),
        (bare, "not_git_worktree"),
    ] {
        selection.selected_path = path.to_string_lossy().into_owned();
        assert_eq!(
            inspector.status(&selection, 1).await.unwrap_err().code,
            expected
        );
    }
    let worktree = repository.root.join("thread");
    git(
        &repository.path,
        &[
            "worktree",
            "add",
            "-b",
            "thread",
            worktree.to_str().unwrap(),
        ],
    );
    selection.selected_path = worktree.to_string_lossy().into_owned();
    selection.source = GitWorkspaceSource::ThreadWorktree;
    let status = inspector.status(&selection, 1).await.unwrap();
    assert_eq!(status.workspace.repository_root, selection.selected_path);
    assert_eq!(
        status.workspace.git_common_directory,
        repository.path.join(".git").to_string_lossy()
    );
    assert_eq!(
        status.workspace.selection.source,
        GitWorkspaceSource::ThreadWorktree
    );
    assert!(status.clean);
}

#[tokio::test(flavor = "current_thread")]
async fn worktrees_use_committed_state_deterministic_names_and_recover_existing_paths() {
    let repository = Repository::new();
    let manager = repository.manager();
    let request = repository.request();
    let commit = git(&repository.path, &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    fs::write(repository.path.join("tracked.txt"), "dirty source\n").unwrap();
    git(&repository.path, &["add", "."]);
    let result = manager.prepare(&request).await.unwrap();
    assert_eq!(result.branch, "t3code/mcp-abcdef1234567890");
    assert!(
        result
            .path
            .ends_with("managed/project-test/abcdef1234567890")
    );
    assert_eq!(result.base_commit, commit);
    assert_eq!(
        fs::read_to_string(Path::new(&result.path).join("tracked.txt")).unwrap(),
        "one\ntwo\nthree\n"
    );
    assert_eq!(manager.prepare(&request).await.unwrap(), result);
    fs::write(Path::new(&result.path).join("untracked"), "keep on retry").unwrap();
    assert_eq!(manager.prepare(&request).await.unwrap(), result);
    assert!(Path::new(&result.path).join("untracked").exists());
    git(Path::new(&result.path), &["checkout", "--detach"]);
    assert_eq!(
        manager.prepare(&request).await.unwrap_err().code,
        "worktree_branch_mismatch"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn worktrees_validate_keys_branches_and_repository_ownership() {
    let repository = Repository::new();
    let manager = repository.manager();
    let mut request = repository.request();
    for branch in ["--bad", "bad..name", "HEAD", "bad branch"] {
        request.base_branch = branch.into();
        assert_eq!(
            manager.prepare(&request).await.unwrap_err().code,
            "invalid_base_branch"
        );
    }
    request.base_branch = "main".into();
    request.operation_key = "short".into();
    assert_eq!(
        manager.prepare(&request).await.unwrap_err().code,
        "invalid_operation_key"
    );
    request = repository.request();
    let unrelated = repository
        .root
        .join("managed/project-test/abcdef1234567890");
    fs::create_dir_all(&unrelated).unwrap();
    git(&unrelated, &["init", "-b", "t3code/mcp-abcdef1234567890"]);
    git(&unrelated, &["config", "user.name", "Test"]);
    git(&unrelated, &["config", "user.email", "test@example.test"]);
    git(&unrelated, &["commit", "--allow-empty", "-m", "unrelated"]);
    assert_eq!(
        manager.prepare(&request).await.unwrap_err().code,
        "worktree_identity_mismatch"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn worktree_origin_fetch_is_explicit_and_missing_origin_falls_back_to_local() {
    let repository = Repository::new();
    let origin = repository.root.join("origin.git");
    git(
        &repository.path,
        &[
            "clone",
            "--bare",
            repository.path.to_str().unwrap(),
            origin.to_str().unwrap(),
        ],
    );
    git(
        &repository.path,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    let origin_head = git(&repository.path, &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    fs::write(repository.path.join("tracked.txt"), "local only\n").unwrap();
    git(&repository.path, &["commit", "-am", "local"]);
    let local_head = git(&repository.path, &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    let manager = repository.manager();
    let mut request = repository.request();
    let local = manager.prepare(&request).await.unwrap();
    assert_eq!(local.base_commit, local_head);
    assert!(!repository.path.join(".git/FETCH_HEAD").exists());
    request.operation_key = "origin1234567890".into();
    request.start_from_origin = true;
    assert_eq!(
        manager.prepare(&request).await.unwrap().base_commit,
        origin_head
    );
    assert!(repository.path.join(".git/FETCH_HEAD").exists());
    git(&repository.path, &["remote", "remove", "origin"]);
    request.operation_key = "fallback12345678".into();
    assert_eq!(
        manager.prepare(&request).await.unwrap().base_commit,
        local_head
    );
}

#[tokio::test(flavor = "current_thread")]
async fn simultaneous_worktree_preparation_converges() {
    let repository = Repository::new();
    let manager = repository.manager();
    let request = repository.request();
    let (first, second) = tokio::join!(manager.prepare(&request), manager.prepare(&request));
    assert_eq!(first.unwrap(), second.unwrap());
}

#[tokio::test(flavor = "current_thread")]
async fn inherited_git_environment_cannot_redirect_inspection_or_worktrees() {
    if std::env::var_os("T3_GIT_ENV_PROBE").is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "git::tests::inherited_git_environment_cannot_redirect_inspection_or_worktrees",
                "--nocapture",
            ])
            .env("T3_GIT_ENV_PROBE", "1")
            .env("GIT_DIR", "/nonexistent-t3-git-dir")
            .env("GIT_WORK_TREE", "/nonexistent-t3-git-tree")
            .env("GIT_INDEX_FILE", "/nonexistent-t3-git-index")
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "core.bare")
            .env("GIT_CONFIG_VALUE_0", "true")
            .env("GIT_EXTERNAL_DIFF", "/bin/false")
            .env("GIT_LITERAL_PATHSPECS", "0")
            .env("GIT_OPTIONAL_LOCKS", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let repository = Repository::new();
    let inspector = GitInspector::new();
    assert!(
        inspector
            .status(&repository.selection(), 1)
            .await
            .unwrap()
            .clean
    );
    fs::write(repository.path.join("tracked.txt"), "local change\n").unwrap();
    assert!(
        inspector
            .diff(&repository.selection(), GitDiffMode::Unstaged, &[], 100_000)
            .await
            .unwrap()
            .patch
            .contains("+local change")
    );
    assert!(
        inspector
            .compare(&repository.selection(), "HEAD", "HEAD", &[], 100_000)
            .await
            .unwrap()
            .patch
            .is_empty()
    );
    let worktree = repository
        .manager()
        .prepare(&repository.request())
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(Path::new(&worktree.path).join("tracked.txt")).unwrap(),
        "one\ntwo\nthree\n"
    );
}

fn process_probe(mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "git::tests::process_runner_probe", "--nocapture"])
        .env("T3_GIT_PROCESS_PROBE", mode)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    command
}

#[test]
fn process_runner_probe() {
    use std::io::Write;
    match std::env::var("T3_GIT_PROCESS_PROBE").as_deref() {
        Ok("delay") => std::thread::sleep(std::time::Duration::from_millis(300)),
        Ok("signal") => {
            Command::new("kill")
                .args(["-TERM", &std::process::id().to_string()])
                .status()
                .unwrap();
        }
        Ok("sleep") => std::thread::sleep(std::time::Duration::from_secs(60)),
        Ok("stderr") => {
            let bytes = vec![b'x'; 200_000];
            std::io::stderr().write_all(&bytes).unwrap();
            std::process::exit(1);
        }
        Ok("parse") => {
            std::io::stdout().write_all(b"invalid output").unwrap();
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
        _ => {}
    }
}

#[test]
fn process_runner_kills_timeouts_and_parser_failures_and_drains_stderr() {
    let args = strings(&["probe"]);
    for purpose in [GitPurpose::Inspection, GitPurpose::Worktree] {
        let started = std::time::Instant::now();
        let error = run_command(
            process_probe("sleep"),
            &args,
            purpose,
            &[0],
            std::time::Duration::from_millis(100),
            &mut |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(error.code, "git_timeout");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }
    let error = run_command(
        process_probe("stderr"),
        &args,
        GitPurpose::Inspection,
        &[0],
        std::time::Duration::from_secs(5),
        &mut |_| Ok(()),
    )
    .unwrap_err();
    assert_eq!(error.code, "git_command_failed");
    assert!(error.message.ends_with(" [stderr truncated]"));
    assert!(error.message.len() < 66_000);
    let started = std::time::Instant::now();
    let error = run_command(
        process_probe("parse"),
        &args,
        GitPurpose::Inspection,
        &[0],
        std::time::Duration::from_secs(5),
        &mut |_| Err(GitError::new("git_output_invalid", "invalid output")),
    )
    .unwrap_err();
    assert_eq!(error.code, "git_output_invalid");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[derive(Default)]
struct AuditProbe {
    records: std::sync::Mutex<Vec<GitAuditEventInput>>,
    identity_barrier: Option<tokio::sync::Barrier>,
}

tokio::task_local! { static TEST_AUDIT_PARENT: String; }

impl GitAuditSink for AuditProbe {
    fn record(&self, input: GitAuditEventInput) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            assert_eq!(
                TEST_AUDIT_PARENT.try_with(Clone::clone).ok().as_deref(),
                Some("parent-operation")
            );
            let identity = input.event == "git.command"
                && input.operation.as_deref() != Some("git check-ref-format");
            self.records.lock().unwrap().push(input);
            if identity && let Some(barrier) = &self.identity_barrier {
                barrier.wait().await;
            }
        })
    }
}

fn assert_audit_pairs(records: &[GitAuditEventInput]) {
    let starts: Vec<_> = records
        .iter()
        .filter(|record| record.event == "git.command")
        .collect();
    assert!(!starts.is_empty());
    assert_eq!(records.len(), starts.len() * 2);
    let mut ids = std::collections::HashSet::new();
    for start in starts {
        let id = start.correlation_id.as_ref().unwrap();
        assert!(id.starts_with("git_"));
        let uuid = &id[4..];
        assert_eq!(uuid.len(), 36);
        assert_eq!(uuid.as_bytes()[14], b'4');
        assert!(b"89ab".contains(&uuid.as_bytes()[19]));
        assert!(ids.insert(id));
        assert_eq!(start.source, "git");
        assert_eq!(start.outcome.as_deref(), Some("started"));
        let results: Vec<_> = records
            .iter()
            .filter(|record| {
                record.event == "git.result" && record.correlation_id.as_ref() == Some(id)
            })
            .collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].operation, start.operation);
        assert_eq!(results[0].source, "git");
        assert!(results[0].duration_ms.unwrap() >= 0.0);
        assert!(
            records
                .iter()
                .position(|record| std::ptr::eq(record, start))
                .unwrap()
                < records
                    .iter()
                    .position(|record| std::ptr::eq(record, results[0]))
                    .unwrap()
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn inspection_audits_every_command_with_bytes_and_parent_context() {
    let repository = Repository::new();
    let audit = Arc::new(AuditProbe::default());
    let inspector = GitInspector::new().with_audit_log(audit.clone());
    fs::write(
        repository.path.join("tracked.txt"),
        "large line\n".repeat(20_000),
    )
    .unwrap();
    let result = TEST_AUDIT_PARENT
        .scope(
            "parent-operation".into(),
            inspector.diff(&repository.selection(), GitDiffMode::Unstaged, &[], 32),
        )
        .await
        .unwrap();
    let records = audit.records.lock().unwrap();
    assert_eq!(records.len(), 4);
    assert_audit_pairs(&records);
    let patch_result = records
        .iter()
        .find(|record| {
            record.event == "git.result" && record.operation.as_deref() == Some("git diff")
        })
        .unwrap();
    let details = patch_result.details.as_ref().unwrap();
    assert_eq!(details["stdoutBytes"], result.truncation.total_bytes);
    assert_eq!(details["exitCode"], 0);
    assert!(details["signal"].is_null());
    assert_eq!(patch_result.outcome.as_deref(), Some("completed"));
    let command = records
        .iter()
        .find(|record| record.event == "git.command")
        .unwrap();
    let args = command.details.as_ref().unwrap()["args"]
        .as_array()
        .unwrap();
    assert!(args.iter().any(|arg| arg == "--no-replace-objects"));
    assert!(args.iter().any(|arg| arg == "--literal-pathspecs"));
}

#[tokio::test(flavor = "current_thread")]
async fn git_audit_sink_records_git_events_with_the_parent_operation() {
    let repository = Repository::new();
    let audit = Arc::new(AuditLog::new(repository.root.join("audit.jsonl")));
    let inspector = GitInspector::new().with_audit_log(audit_log_sink(audit.clone()));

    with_audit_context(
        "parent-correlation",
        "t3_git_status",
        inspector.status(&repository.selection(), 10),
    )
    .await
    .unwrap();

    let page = audit.query(&AuditQueryInput::default()).unwrap();
    assert!(!page.items.is_empty());
    assert!(
        page.items
            .iter()
            .all(|event| event.source == AuditSource::Git)
    );
    assert!(page.items.iter().any(|event| {
        event
            .parent_correlation_id
            .as_ref()
            .and_then(|value| value.as_str())
            == Some("parent-correlation")
            && event
                .parent_operation
                .as_ref()
                .and_then(|value| value.as_str())
                == Some("t3_git_status")
    }));
}

#[tokio::test(flavor = "current_thread")]
async fn worktree_identity_checks_are_concurrent_and_audited() {
    let repository = Repository::new();
    let manager = repository.manager();
    let expected = manager.prepare(&repository.request()).await.unwrap();
    let audit = Arc::new(AuditProbe {
        records: Default::default(),
        identity_barrier: Some(tokio::sync::Barrier::new(4)),
    });
    let manager = manager.with_audit_log(audit.clone());
    let actual = TEST_AUDIT_PARENT
        .scope(
            "parent-operation".into(),
            tokio::time::timeout(
                Duration::from_secs(5),
                manager.prepare(&repository.request()),
            ),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(actual, expected);
    let records = audit.records.lock().unwrap();
    assert_audit_pairs(&records);
    assert_eq!(
        records
            .iter()
            .filter(|record| record.event == "git.command")
            .count(),
        5
    );
    for command in records
        .iter()
        .filter(|record| record.event == "git.command")
    {
        assert_eq!(command.details.as_ref().unwrap()["mutation"], true);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn process_wait_does_not_block_a_single_thread_runtime() {
    let runner = GitRunner::new(GitPurpose::Inspection);
    let finished = std::sync::atomic::AtomicBool::new(false);
    let run = async {
        runner
            .run_process(
                process_probe("delay"),
                &strings(&["probe"]),
                &[0],
                (),
                |_, _| Ok(()),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        finished.store(true, Ordering::SeqCst);
    };
    let heartbeat = async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!finished.load(Ordering::SeqCst));
    };
    tokio::join!(run, heartbeat);
    assert!(finished.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "current_thread")]
async fn public_errors_keep_distinct_types_and_filesystem_evidence() {
    let repository = Repository::new();
    let mut selection = repository.selection();
    selection.selected_path = repository
        .root
        .join("missing")
        .to_string_lossy()
        .into_owned();
    let inspection: GitInspectionError =
        GitInspector::new().status(&selection, 1).await.unwrap_err();
    assert_eq!(inspection.name(), "GitInspectionError");
    assert_eq!(inspection.code, "workspace_missing");
    assert_eq!(
        inspection.message,
        format!(
            "T3 workspace path does not exist on the gateway host: {}",
            selection.selected_path
        )
    );
    let io = inspection.evidence.io.as_ref().unwrap();
    assert_eq!(io.operation, "stat");
    assert_eq!(io.path, selection.selected_path);
    assert!(io.raw_os_error.is_some());
    assert!(!io.message.is_empty());
    let manager = GitWorktreeManager::new(Some(repository.path.join("tracked.txt")));
    let worktree: GitWorktreeError = manager.prepare(&repository.request()).await.unwrap_err();
    assert_eq!(worktree.name(), "GitWorktreeError");
    assert_eq!(worktree.code, "ENOTDIR");
    assert!(worktree.message.contains("stat"));
    assert!(worktree.message.contains("tracked.txt"));
    assert!(
        worktree
            .evidence
            .io
            .as_ref()
            .unwrap()
            .raw_os_error
            .is_some()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn failed_worktree_preparation_is_audited_and_can_be_retried() {
    let repository = Repository::new();
    let audit = Arc::new(AuditProbe::default());
    let manager = repository.manager().with_audit_log(audit.clone());
    let mut request = repository.request();
    request.base_branch = "missing-base".into();
    let failure = TEST_AUDIT_PARENT
        .scope("parent-operation".into(), manager.prepare(&request))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "git_command_failed");
    assert!(
        failure
            .message
            .starts_with("Git rev-parse failed (exit 128)")
    );
    let evidence = failure.evidence.process.as_ref().unwrap();
    assert_eq!(evidence.exit_code, Some(128));
    assert!(!evidence.stderr.is_empty());
    assert!(evidence.stderr_bytes > 0);
    {
        let records = audit.records.lock().unwrap();
        assert_audit_pairs(&records);
        let result = records.last().unwrap();
        assert_eq!(result.outcome.as_deref(), Some("error"));
        assert_eq!(result.details.as_ref().unwrap()["stderr"], evidence.stderr);
        assert_eq!(result.details.as_ref().unwrap()["exitCode"], 128);
    }
    request.base_branch = "main".into();
    TEST_AUDIT_PARENT
        .scope("parent-operation".into(), manager.prepare(&request))
        .await
        .unwrap();
    assert_audit_pairs(&audit.records.lock().unwrap());
}

#[tokio::test(flavor = "current_thread")]
async fn audit_retains_spawn_timeout_parser_and_stderr_failures() {
    let audit = Arc::new(AuditProbe::default());
    let runner = GitRunner {
        purpose: GitPurpose::Inspection,
        audit: Some(audit.clone()),
    };
    for mode in ["sleep", "stderr", "parse", "missing"] {
        let command = if mode == "missing" {
            let mut command = Command::new("/nonexistent-t3-git-executable");
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            command
        } else {
            process_probe(mode)
        };
        let mode_owned = mode.to_owned();
        let error = TEST_AUDIT_PARENT
            .scope(
                "parent-operation".into(),
                runner.run_process(
                    command,
                    &strings(&["probe"]),
                    &[0],
                    (),
                    move |_, _| {
                        if mode_owned == "parse" {
                            Err(GitError::new("git_output_invalid", "invalid output"))
                        } else {
                            Ok(())
                        }
                    },
                    Duration::from_millis(100),
                ),
            )
            .await
            .unwrap_err();
        let records = audit.records.lock().unwrap();
        assert_audit_pairs(&records);
        let details = records.last().unwrap().details.as_ref().unwrap();
        assert_eq!(records.last().unwrap().outcome.as_deref(), Some("error"));
        match mode {
            "sleep" => {
                assert_eq!(error.code, "git_timeout");
                assert_eq!(error.message, "Git inspection exceeded 30 seconds.");
                assert_eq!(details["timedOut"], true);
                #[cfg(unix)]
                assert_eq!(details["signal"], "SIGKILL");
            }
            "stderr" => {
                assert!(error.message.ends_with(" [stderr truncated]"));
                assert_eq!(details["stderrTotalBytes"], 200_000);
                assert_eq!(details["stderrBytes"], 65_536);
                assert_eq!(details["stderrTruncated"], true);
            }
            "parse" => {
                assert_eq!(error.code, "git_output_invalid");
                assert_eq!(details["error"], "invalid output");
            }
            "missing" => {
                assert_eq!(error.code, "git_unavailable");
                assert!(error.message.contains("No such file"));
                assert!(details["io"]["rawOsError"].is_number());
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn signaled_exit_keeps_signal_name_number_and_raw_status() {
    let audit = Arc::new(AuditProbe::default());
    let runner = GitRunner {
        purpose: GitPurpose::Inspection,
        audit: Some(audit.clone()),
    };
    let error = TEST_AUDIT_PARENT
        .scope(
            "parent-operation".into(),
            runner.run_process(
                process_probe("signal"),
                &strings(&["probe"]),
                &[0],
                (),
                |_, _| Ok(()),
                Duration::from_secs(5),
            ),
        )
        .await
        .unwrap_err();
    assert!(error.message.contains("exit SIGTERM"));
    let evidence = error.evidence.process.as_ref().unwrap();
    assert_eq!(evidence.exit_code, None);
    assert_eq!(evidence.signal.as_deref(), Some("SIGTERM"));
    assert_eq!(evidence.signal_number, Some(15));
    assert_eq!(evidence.raw_exit_status, Some(15));
    let records = audit.records.lock().unwrap();
    assert_audit_pairs(&records);
    let details = records.last().unwrap().details.as_ref().unwrap();
    assert!(details["exitCode"].is_null());
    assert_eq!(details["signal"], "SIGTERM");
    assert_eq!(details["signalNumber"], 15);
    assert_eq!(details["rawExitStatus"], 15);
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_worktree_caller_keeps_preparation_and_audit_results() {
    let repository = Repository::new();
    let records = Arc::new(std::sync::Mutex::new(Vec::<GitAuditEventInput>::new()));
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let audit = {
        let (records, started, release) = (records.clone(), started.clone(), release.clone());
        Arc::new(move |input: GitAuditEventInput| {
            let (records, started, release) = (records.clone(), started.clone(), release.clone());
            async move {
                let add = input.event == "git.command"
                    && input.operation.as_deref() == Some("git worktree");
                records.lock().unwrap().push(input);
                if add {
                    started.notify_one();
                    release.notified().await;
                }
            }
        })
    };
    let manager = repository.manager().with_audit_log(audit);
    let worker = manager.clone();
    let request = repository.request();
    let task = tokio::spawn(async move { worker.prepare(&request).await });
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    release.notify_one();
    let prepared = tokio::time::timeout(
        Duration::from_secs(5),
        manager.prepare(&repository.request()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(prepared.branch, "t3code/mcp-abcdef1234567890");
    assert_eq!(
        git(Path::new(&prepared.path), &["branch", "--show-current"]).trim(),
        prepared.branch
    );
    let records = records.lock().unwrap();
    assert_audit_pairs(&records);
    assert_eq!(
        records
            .iter()
            .filter(|record| record.event == "git.command"
                && record.operation.as_deref() == Some("git worktree"))
            .count(),
        1
    );
}

#[tokio::test(flavor = "current_thread")]
async fn identity_failure_returns_early_and_remaining_commands_keep_audit_results() {
    let repository = Repository::new();
    let existing = repository
        .root
        .join("managed/project-test/abcdef1234567890");
    fs::create_dir_all(&existing).unwrap();
    git(&existing, &["init", "-b", "main"]);
    let records = Arc::new(std::sync::Mutex::new(Vec::<GitAuditEventInput>::new()));
    let release = Arc::new(tokio::sync::Notify::new());
    let finished = Arc::new(tokio::sync::Notify::new());
    let blocked = Arc::new(AtomicUsize::new(0));
    let audit = {
        let (records, release, finished, blocked) = (
            records.clone(),
            release.clone(),
            finished.clone(),
            blocked.clone(),
        );
        Arc::new(move |input: GitAuditEventInput| {
            let (records, release, finished, blocked) = (
                records.clone(),
                release.clone(),
                finished.clone(),
                blocked.clone(),
            );
            async move {
                let start = input.event == "git.command";
                let result = input.event == "git.result";
                let args = &input.details.as_ref().unwrap()["args"];
                let delay = start
                    && args
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|arg| arg == "--git-common-dir" || arg == "--show-current");
                records.lock().unwrap().push(input);
                if delay {
                    blocked.fetch_add(1, Ordering::SeqCst);
                    release.notified().await;
                }
                if result {
                    finished.notify_one();
                }
            }
        })
    };
    let manager = repository.manager().with_audit_log(audit);
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        manager.prepare(&repository.request()),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error.code, "git_command_failed");
    assert_eq!(
        error.evidence.process.as_ref().unwrap().exit_code,
        Some(128)
    );
    assert_eq!(blocked.load(Ordering::SeqCst), 3);
    release.notify_waiters();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let result_count = records
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.event == "git.result")
                .count();
            if result_count == 5 {
                break;
            }
            finished.notified().await;
        }
    })
    .await
    .unwrap();
    assert_audit_pairs(&records.lock().unwrap());
}
