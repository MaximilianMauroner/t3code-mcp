pub mod inspection;
pub mod worktree_manager;

pub use inspection::*;
pub use worktree_manager::*;

use std::future::Future;
use std::io::Read;
use std::path::Path;
use std::pin::Pin;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::operations::audit_log::{
    AuditEventInput, AuditLog, AuditSource, current_audit_context, with_audit_context,
};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitProcessEvidence {
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    pub signal_number: Option<i32>,
    pub raw_exit_status: Option<i32>,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub stderr_captured_bytes: usize,
    pub stderr: String,
    pub stderr_truncated: bool,
    pub timed_out: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitIoEvidence {
    pub operation: String,
    pub path: String,
    pub raw_os_error: Option<i32>,
    pub kind: String,
    pub message: String,
}

impl std::fmt::Display for GitIoEvidence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for GitIoEvidence {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitErrorEvidence {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process: Option<Box<GitProcessEvidence>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io: Option<Box<GitIoEvidence>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitError {
    pub code: String,
    pub message: String,
    pub evidence: GitErrorEvidence,
}

impl GitError {
    fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            evidence: GitErrorEvidence::default(),
        }
    }

    fn relabel(mut self, code: &str, message: impl Into<String>) -> Self {
        self.code = code.into();
        self.message = message.into();
        self
    }

    fn io(operation: &str, path: &Path, error: std::io::Error) -> Self {
        let code = match error.kind() {
            std::io::ErrorKind::NotFound => "ENOENT",
            std::io::ErrorKind::PermissionDenied => "EACCES",
            std::io::ErrorKind::NotADirectory => "ENOTDIR",
            std::io::ErrorKind::IsADirectory => "EISDIR",
            std::io::ErrorKind::AlreadyExists => "EEXIST",
            std::io::ErrorKind::InvalidInput => "EINVAL",
            _ => "filesystem_error",
        };
        let mut failure = Self::new(
            code,
            format!("{code}: {error}, {operation} '{}'", path.display()),
        );
        failure.evidence.io = Some(Box::new(GitIoEvidence {
            operation: operation.into(),
            path: path.to_string_lossy().into_owned(),
            raw_os_error: error.raw_os_error(),
            kind: format!("{:?}", error.kind()),
            message: error.to_string(),
        }));
        failure
    }
}

impl std::fmt::Display for GitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for GitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.evidence
            .io
            .as_deref()
            .map(|error| error as &dyn std::error::Error)
    }
}

macro_rules! git_error_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
        pub struct $name {
            #[serde(flatten)]
            error: GitError,
        }
        impl $name {
            pub fn name(&self) -> &'static str {
                stringify!($name)
            }
        }
        impl From<GitError> for $name {
            fn from(error: GitError) -> Self {
                Self { error }
            }
        }
        impl std::ops::Deref for $name {
            type Target = GitError;
            fn deref(&self) -> &GitError {
                &self.error
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.error.fmt(formatter)
            }
        }
        impl std::error::Error for $name {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.error)
            }
        }
    };
}

git_error_type!(GitInspectionError);
git_error_type!(GitWorktreeError);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitAuditEventInput {
    pub source: String,
    pub event: String,
    pub correlation_id: Option<String>,
    pub operation: Option<String>,
    pub outcome: Option<String>,
    pub duration_ms: Option<f64>,
    pub details: Option<Value>,
}

/// The sink applies AuditLog's summarization and best-effort write policy.
/// Bind request context in the sink so canceled callers retain audit attribution.
pub trait GitAuditSink: Send + Sync {
    fn record(&self, input: GitAuditEventInput) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

pub fn audit_log_sink(log: Arc<AuditLog>) -> Arc<dyn GitAuditSink> {
    Arc::new(move |input: GitAuditEventInput| {
        let log = Arc::clone(&log);
        let context = current_audit_context();
        let runtime = tokio::runtime::Handle::current();
        async move {
            let event = AuditEventInput {
                source: AuditSource::Git,
                event: input.event,
                correlation_id: input.correlation_id,
                operation: input.operation,
                outcome: input.outcome,
                duration_ms: input.duration_ms,
                details: input.details.map(Into::into),
            };
            let _ = tokio::task::spawn_blocking(move || {
                if let Some(context) = context {
                    runtime.block_on(with_audit_context(
                        context.correlation_id,
                        context.operation,
                        async move { log.record(event) },
                    ));
                } else {
                    log.record(event);
                }
            })
            .await;
        }
    })
}

impl<F, Fut> GitAuditSink for F
where
    F: Fn(GitAuditEventInput) -> Fut + Send + Sync,
    Fut: Future<Output = ()> + Send + 'static,
{
    fn record(&self, input: GitAuditEventInput) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(self(input))
    }
}

#[derive(Clone, Copy)]
enum GitPurpose {
    Inspection,
    Worktree,
}

impl GitPurpose {
    fn timeout(self) -> Duration {
        Duration::from_secs(match self {
            Self::Inspection => 30,
            Self::Worktree => 60,
        })
    }
    fn timeout_error(self) -> GitError {
        GitError::new(
            "git_timeout",
            match self {
                Self::Inspection => "Git inspection exceeded 30 seconds.",
                Self::Worktree => "Git worktree preparation exceeded 60 seconds.",
            },
        )
    }
}

const SAFETY_ARGS: &[&str] = &[
    "--no-pager",
    "--no-optional-locks",
    "--no-replace-objects",
    "--literal-pathspecs",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.untrackedCache=false",
];

fn git_command(cwd: &Path, args: &[String]) -> Command {
    let mut command = Command::new("git");
    command.current_dir(cwd).env_clear();
    command.envs(std::env::vars_os().filter(|(key, _)| !key.to_string_lossy().starts_with("GIT_")));
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(SAFETY_ARGS)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

#[derive(Clone)]
struct GitRunner {
    purpose: GitPurpose,
    audit: Option<Arc<dyn GitAuditSink>>,
}

impl GitRunner {
    fn new(purpose: GitPurpose) -> Self {
        Self {
            purpose,
            audit: None,
        }
    }

    async fn run<S, F>(
        &self,
        cwd: &Path,
        args: &[String],
        accepted: &[i32],
        state: S,
        consume: F,
    ) -> Result<(S, GitProcessEvidence), GitError>
    where
        S: Send + 'static,
        F: FnMut(&mut S, &[u8]) -> Result<(), GitError> + Send + 'static,
    {
        self.run_process(
            git_command(cwd, args),
            args,
            accepted,
            state,
            consume,
            self.purpose.timeout(),
        )
        .await
    }

    async fn run_process<S, F>(
        &self,
        command: Command,
        args: &[String],
        accepted: &[i32],
        mut state: S,
        mut consume: F,
        timeout: Duration,
    ) -> Result<(S, GitProcessEvidence), GitError>
    where
        S: Send + 'static,
        F: FnMut(&mut S, &[u8]) -> Result<(), GitError> + Send + 'static,
    {
        let correlation_id = format!("git_{}", uuid::Uuid::new_v4());
        let operation = format!(
            "git {}",
            args.first().map(String::as_str).unwrap_or("unknown")
        );
        let started = Instant::now();
        let mut logged_args = args.to_vec();
        if matches!(self.purpose, GitPurpose::Inspection) {
            logged_args.splice(0..0, SAFETY_ARGS.iter().map(|arg| (*arg).to_owned()));
        }
        let mut details = json!({"cwd": command.get_current_dir().unwrap_or(Path::new(".")).to_string_lossy(), "args": logged_args});
        if matches!(self.purpose, GitPurpose::Worktree) {
            details["mutation"] = json!(true);
        }
        self.record(GitAuditEventInput {
            source: "git".into(),
            event: "git.command".into(),
            correlation_id: Some(correlation_id.clone()),
            operation: Some(operation.clone()),
            outcome: Some("started".into()),
            duration_ms: None,
            details: Some(details),
        })
        .await;
        let owned_args = args.to_vec();
        let accepted = accepted.to_vec();
        let purpose = self.purpose;
        let task = tokio::task::spawn_blocking(move || {
            let result = run_command(
                command,
                &owned_args,
                purpose,
                &accepted,
                timeout,
                &mut |chunk| consume(&mut state, chunk),
            );
            (state, result)
        })
        .await;
        let (state, result) = match task {
            Ok(result) => result,
            Err(error) => {
                let error = GitError::new(
                    "git_command_failed",
                    format!("Git process task failed: {error}"),
                );
                self.record_result(&correlation_id, &operation, started, Err(&error))
                    .await;
                return Err(error);
            }
        };
        self.record_result(&correlation_id, &operation, started, result.as_ref())
            .await;
        result.map(|evidence| (state, evidence))
    }

    async fn record(&self, input: GitAuditEventInput) {
        if let Some(audit) = &self.audit {
            audit.record(input).await;
        }
    }

    async fn record_result(
        &self,
        correlation_id: &str,
        operation: &str,
        started: Instant,
        result: Result<&GitProcessEvidence, &GitError>,
    ) {
        let mut details = json!({ "stdoutBytes": 0, "stderrBytes": 0 });
        let (process, error) = match result {
            Ok(process) => (Some(process), None),
            Err(error) => (error.evidence.process.as_deref(), Some(error)),
        };
        if let Some(process) = process {
            let exit = match self.purpose {
                GitPurpose::Inspection => json!(process.exit_code),
                GitPurpose::Worktree => json!(process.exit_code.unwrap_or(-1)),
            };
            details = json!({
                "exitCode": exit, "signal": process.signal, "signalNumber": process.signal_number,
                "rawExitStatus": process.raw_exit_status, "stdoutBytes": process.stdout_bytes,
                "stderrBytes": if matches!(self.purpose, GitPurpose::Inspection) { process.stderr_captured_bytes } else { process.stderr_bytes },
                "stderrTotalBytes": process.stderr_bytes, "stderrCapturedBytes": process.stderr_captured_bytes,
                "stderrTruncated": process.stderr_truncated, "timedOut": process.timed_out,
            });
            if matches!(self.purpose, GitPurpose::Inspection) || error.is_some() {
                details["stderr"] = json!(process.stderr);
            }
        }
        if let Some(error) = error {
            let message = match (self.purpose, error.code.as_str()) {
                (GitPurpose::Worktree, "git_timeout") => "timeout".into(),
                (_, "git_unavailable") => error
                    .evidence
                    .io
                    .as_ref()
                    .map(|io| io.message.clone())
                    .unwrap_or_else(|| error.message.clone()),
                (GitPurpose::Inspection, "git_command_failed") if process.is_some() => {
                    let process = process.unwrap();
                    let exit = process
                        .exit_code
                        .map(|code| code.to_string())
                        .or_else(|| process.signal.clone())
                        .unwrap_or_else(|| "unknown".into());
                    format!("Git command failed (exit {exit})")
                }
                _ => error.message.clone(),
            };
            details["error"] = json!(message);
            if let Some(io) = &error.evidence.io {
                details["io"] = json!(io);
            }
        }
        self.record(GitAuditEventInput {
            source: "git".into(),
            event: "git.result".into(),
            correlation_id: Some(correlation_id.into()),
            operation: Some(operation.into()),
            outcome: Some(
                if error.is_some() {
                    "error"
                } else {
                    "completed"
                }
                .into(),
            ),
            duration_ms: Some(started.elapsed().as_millis() as f64),
            details: Some(details),
        })
        .await;
    }
}

async fn blocking_io<T, F>(operation: &'static str, path: &Path, action: F) -> Result<T, GitError>
where
    T: Send + 'static,
    F: FnOnce(&Path) -> std::io::Result<T> + Send + 'static,
{
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        action(&path).map_err(|error| GitError::io(operation, &path, error))
    })
    .await
    .map_err(|error| {
        GitError::new(
            "filesystem_error",
            format!("Filesystem task failed: {error}"),
        )
    })?
}

struct CompleteOnDrop<F: Future + Send + 'static>
where
    F::Output: Send,
{
    future: Option<Pin<Box<F>>>,
}

impl<F: Future + Send + 'static> Drop for CompleteOnDrop<F>
where
    F::Output: Send,
{
    fn drop(&mut self) {
        if let Some(future) = self.future.take() {
            tokio::spawn(future);
        }
    }
}

async fn complete_on_drop<F>(future: F) -> F::Output
where
    F: Future + Send + 'static,
    F::Output: Send,
{
    let context = current_audit_context();
    let future = async move {
        if let Some(context) = context {
            with_audit_context(context.correlation_id, context.operation, future).await
        } else {
            future.await
        }
    };
    let mut completion = CompleteOnDrop {
        future: Some(Box::pin(future)),
    };
    let output =
        std::future::poll_fn(|context| completion.future.as_mut().unwrap().as_mut().poll(context))
            .await;
    completion.future.take();
    output
}

fn run_command<F>(
    mut command: Command,
    args: &[String],
    purpose: GitPurpose,
    accepted: &[i32],
    timeout: Duration,
    stdout: &mut F,
) -> Result<GitProcessEvidence, GitError>
where
    F: FnMut(&[u8]) -> Result<(), GitError> + Send,
{
    let mut child = command.spawn().map_err(|error| {
        let message = format!("Git could not be started: {error}");
        GitError::io("spawn", Path::new(command.get_program()), error)
            .relabel("git_unavailable", message)
    })?;
    let mut output = child.stdout.take().expect("piped stdout");
    let mut errors = child.stderr.take().expect("piped stderr");
    let started = Instant::now();
    thread::scope(|scope| {
        let (sender, receiver) = mpsc::channel();
        let reader = scope.spawn(move || {
            let mut buffer = [0; 16 * 1024];
            let mut failure = None;
            let mut total = 0;
            loop {
                let count = match output.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        let error = GitError::io("read stdout", Path::new("git"), error)
                            .relabel("git_output_invalid", "Git stdout could not be read.");
                        let _ = sender.send(error.clone());
                        failure = Some(error);
                        break;
                    }
                };
                total += count;
                if failure.is_none()
                    && let Err(error) = stdout(&buffer[..count])
                {
                    let _ = sender.send(error.clone());
                    failure = Some(error);
                }
            }
            (failure, total)
        });
        let error_reader = scope.spawn(move || {
            let mut captured = Vec::new();
            let mut total = 0usize;
            let mut buffer = [0; 16 * 1024];
            let mut failure = None;
            loop {
                match errors.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        total += count;
                        let keep = count.min((64 * 1024usize).saturating_sub(captured.len()));
                        captured.extend_from_slice(&buffer[..keep]);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        failure = Some(GitError::io("read stderr", Path::new("git"), error));
                        break;
                    }
                }
            }
            (captured, total, failure)
        });
        let mut timed_out = false;
        let mut parse_error = None;
        let mut exit = None;
        let mut wait_error = None;
        loop {
            if parse_error.is_none() {
                parse_error = receiver.try_recv().ok();
            }
            if !timed_out && started.elapsed() >= timeout {
                timed_out = true;
                let _ = child.kill();
            }
            if parse_error.is_some() {
                let _ = child.kill();
            }
            if exit.is_none() {
                match child.try_wait() {
                    Ok(status) => exit = status,
                    Err(error) => {
                        wait_error = Some(GitError::io("wait", Path::new("git"), error));
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
            if exit.is_some() && reader.is_finished() && error_reader.is_finished() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let (output_error, stdout_bytes) = reader.join().expect("Git stdout reader panicked");
        let (stderr, stderr_bytes, stderr_error) =
            error_reader.join().expect("Git stderr reader panicked");
        let (signal_number, raw_exit_status) = exit_details(exit);
        let evidence = GitProcessEvidence {
            exit_code: exit.and_then(|status| status.code()),
            signal: signal_number.map(signal_name),
            signal_number,
            raw_exit_status,
            stdout_bytes,
            stderr_bytes,
            stderr_captured_bytes: stderr.len(),
            stderr: String::from_utf8_lossy(&stderr).trim().into(),
            stderr_truncated: stderr_bytes > stderr.len(),
            timed_out,
        };
        let failure = if let Some(error) = parse_error.or(output_error) {
            Some(error)
        } else if timed_out {
            Some(purpose.timeout_error())
        } else if let Some(error) = wait_error.or(stderr_error) {
            Some(error)
        } else if !evidence
            .exit_code
            .is_some_and(|code| accepted.contains(&code))
        {
            let detail = if evidence.stderr.is_empty() {
                String::new()
            } else {
                format!(
                    ": {}{}",
                    evidence.stderr,
                    if evidence.stderr_truncated && matches!(purpose, GitPurpose::Inspection) {
                        " [stderr truncated]"
                    } else {
                        ""
                    }
                )
            };
            let observed = evidence
                .exit_code
                .map(|code| code.to_string())
                .or_else(|| evidence.signal.clone())
                .unwrap_or_else(|| "unknown".into());
            let message = match purpose {
                GitPurpose::Inspection => {
                    format!("Read-only Git command failed (exit {observed}){detail}")
                }
                GitPurpose::Worktree => format!(
                    "Git {} failed (exit {}){detail}",
                    args.first().map(String::as_str).unwrap_or("command"),
                    evidence.exit_code.unwrap_or(-1)
                ),
            };
            Some(GitError::new("git_command_failed", message))
        } else {
            None
        };
        if let Some(mut error) = failure {
            error.evidence.process = Some(Box::new(evidence));
            Err(error)
        } else {
            Ok(evidence)
        }
    })
}

#[cfg(unix)]
fn exit_details(exit: Option<ExitStatus>) -> (Option<i32>, Option<i32>) {
    use std::os::unix::process::ExitStatusExt;
    (
        exit.and_then(|status| status.signal()),
        exit.map(|status| status.into_raw()),
    )
}
#[cfg(not(unix))]
fn exit_details(_: Option<ExitStatus>) -> (Option<i32>, Option<i32>) {
    (None, None)
}

fn signal_name(number: i32) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        let description = ExitStatus::from_raw(number).to_string();
        if let Some((_, rest)) = description.split_once('(')
            && let Some((name, _)) = rest.split_once(')')
            && name.starts_with("SIG")
        {
            return name.into();
        }
    }
    format!("SIG{number}")
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

fn is_commit(value: &str) -> bool {
    (40..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn observed_at() -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after Unix epoch");
    let seconds = elapsed.as_secs();
    let days = (seconds / 86400) as i64 + 719468;
    let era = days / 146097;
    let day_of_era = days - era * 146097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60,
        elapsed.subsec_millis()
    )
}

#[cfg(test)]
mod tests;
