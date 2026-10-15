use std::{
    collections::BTreeMap,
    env,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::{fs, io::AsyncReadExt, process::Command, sync::Notify};

use crate::{
    config::ExecutionConfig,
    models::{CheckPhase, CheckResult, CheckStatus},
    process::ProcessIdentity,
};

#[cfg(not(test))]
const MAX_CAPTURE_BYTES: usize = 16 * 1024 * 1024;
#[cfg(test)]
const MAX_CAPTURE_BYTES: usize = 64 * 1024;
const REDACTION_MARKER: &[u8] = b"[REDACTED]";
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(unix)]
const TRUSTED_HOST_DIRECTORIES: &[&str] = &[
    "/usr/bin",
    "/bin",
    "/usr/local/bin",
    "/opt/homebrew/bin",
    "/Applications/Docker.app/Contents/Resources/bin",
];

/// An argv-style command. Arguments are never reparsed by a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    /// Adapter-owned environment additions. The executor still filters these
    /// against `execution.forwarded_env` before passing them to a child, except
    /// explicit USER metadata needed by native credential stores.
    pub env: BTreeMap<String, String>,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
        }
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(name.into(), value.into());
        self
    }
}

pub trait ExecutionObserver: std::fmt::Debug + Send + Sync {
    fn provider_failure(&self, _failure: crate::FailureKind) -> Result<()> {
        Ok(())
    }
    fn preflight_failed(&self) -> Result<()> {
        Ok(())
    }
    fn authorize_launch(&self) -> Result<()>;
    fn spawn_failed(&self) -> Result<()>;
    fn child_spawned(
        &self,
        identity: &ProcessIdentity,
        backend_identity: Option<&str>,
    ) -> Result<()>;
    fn cleanup_confirmed(&self, identity: &ProcessIdentity) -> Result<()>;
    fn cleanup_after_unrecorded_spawn(
        &self,
        identity: &ProcessIdentity,
        backend_identity: Option<&str>,
        confirmed: bool,
    ) -> Result<()>;
}

#[derive(Clone)]
pub struct ExecutionRequest {
    pub command: CommandSpec,
    /// The only candidate tree made available to the command.
    pub workspace: PathBuf,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    /// Overrides the configured timeout when set.
    pub timeout: Option<Duration>,
    pub observer: Option<Arc<dyn ExecutionObserver>>,
}

impl ExecutionRequest {
    pub fn new(
        command: CommandSpec,
        workspace: impl Into<PathBuf>,
        stdout_path: impl Into<PathBuf>,
        stderr_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            command,
            workspace: workspace.into(),
            stdout_path: stdout_path.into(),
            stderr_path: stderr_path.into(),
            timeout: None,
            observer: None,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn with_observer(mut self, observer: Arc<dyn ExecutionObserver>) -> Self {
        self.observer = Some(observer);
        self
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
    SpawnFailed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionResult {
    pub status: ExecutionStatus,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    pub error: Option<String>,
    pub cleanup_confirmed: bool,
    #[serde(skip)]
    raw_stdout: Vec<u8>,
    #[serde(skip)]
    raw_stderr: Vec<u8>,
}

impl ExecutionResult {
    pub fn success(&self) -> bool {
        self.status == ExecutionStatus::Succeeded
    }

    pub(crate) fn raw_stdout_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.raw_stdout)
    }

    pub(crate) fn raw_stderr_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.raw_stderr)
    }

    pub(crate) fn clear_raw_capture(&mut self) {
        self.raw_stdout.clear();
        self.raw_stderr.clear();
    }
}

#[derive(Debug, Default)]
struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

/// A small cancellation primitive that avoids coupling the execution API to a
/// particular task owner. Clones observe the same cancellation event.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    state: Arc<CancellationState>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        if !self.state.cancelled.swap(true, Ordering::SeqCst) {
            self.state.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::SeqCst)
    }

    pub async fn cancelled(&self) {
        loop {
            let notified = self.state.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Debug, Clone)]
pub struct Executor {
    config: ExecutionConfig,
}

impl Executor {
    pub fn new(config: ExecutionConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &ExecutionConfig {
        &self.config
    }

    pub async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult> {
        self.execute_with_cancel(request, CancellationToken::new())
            .await
    }

    pub async fn execute_with_cancel(
        &self,
        request: ExecutionRequest,
        cancellation: CancellationToken,
    ) -> Result<ExecutionResult> {
        validate_workspace(&request.workspace)?;
        prepare_log_path(&request.stdout_path).await?;
        prepare_log_path(&request.stderr_path).await?;
        let redactions = redaction_values(&self.config.forwarded_env, &request.command.env);

        let started = Instant::now();
        if cancellation.is_cancelled() {
            return self
                .finish_without_child(
                    request,
                    ExecutionStatus::Cancelled,
                    started,
                    "execution cancelled before spawn".into(),
                )
                .await;
        }

        let container_name = (self.config.backend == "docker")
            .then(|| format!("dispatch-{}", ulid::Ulid::new().to_string().to_lowercase()));
        let mut docker_cleanup = container_name.as_deref().map(DockerCleanupGuard::new);
        let mut command = self.process_command(&request, container_name.as_deref())?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);

        if let Some(observer) = &request.observer {
            observer.authorize_launch()?;
        }
        // Authorization may block on SQLite. Recheck after it returns, then
        // perform no await or external work before the synchronous OS spawn.
        if cancellation.is_cancelled() {
            if let Some(observer) = &request.observer {
                // The executor knows it has not called spawn, despite the
                // durable conservative spawn-may-have-occurred marker.
                observer.spawn_failed()?;
            }
            return self
                .finish_without_child(
                    request,
                    ExecutionStatus::Cancelled,
                    started,
                    "execution cancelled at the launch boundary".into(),
                )
                .await;
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                if let Some(observer) = &request.observer {
                    observer.spawn_failed()?;
                }
                return self
                    .finish_without_child(
                        request,
                        ExecutionStatus::SpawnFailed,
                        started,
                        format!("failed to spawn process: {error}"),
                    )
                    .await;
            }
        };

        let mut process_group = ProcessGroupGuard::new(child.id());
        let child_identity = child.id().map(crate::process::process_identity);
        if let (Some(observer), Some(identity)) = (&request.observer, &child_identity)
            && let Err(error) = observer.child_spawned(identity, container_name.as_deref())
        {
            let group_cleanup_confirmed = terminate_child(&mut child, &mut process_group).await;
            let backend_cleanup_confirmed = match &container_name {
                Some(name) => cleanup_docker_container(name).await,
                None => true,
            };
            let cleanup_confirmed = group_cleanup_confirmed && backend_cleanup_confirmed;
            // The conservative spawn-may-have-occurred marker was committed
            // before spawn. Resolve it explicitly even when child identity
            // persistence failed and later artifact writing also fails.
            if let Some(observer) = &request.observer {
                observer.cleanup_after_unrecorded_spawn(
                    identity,
                    container_name.as_deref(),
                    cleanup_confirmed,
                )?;
            }
            if cleanup_confirmed && let Some(guard) = &mut docker_cleanup {
                guard.disarm();
            }
            let mut result = self
                .finish_without_child(
                    request,
                    ExecutionStatus::Failed,
                    started,
                    format!("failed to persist child start: {error:#}"),
                )
                .await?;
            result.cleanup_confirmed = cleanup_confirmed;
            return Ok(result);
        }
        let stdout = child
            .stdout
            .take()
            .context("child stdout was not captured")?;
        let stderr = child
            .stderr
            .take()
            .context("child stderr was not captured")?;
        let stdout_task = tokio::spawn(read_capped(stdout));
        let stderr_task = tokio::spawn(read_capped(stderr));
        let timeout = request
            .timeout
            .unwrap_or_else(|| Duration::from_secs(self.config.timeout_secs));

        enum Completion {
            Exited(std::io::Result<std::process::ExitStatus>),
            TimedOut,
            Cancelled,
        }

        let completion = tokio::select! {
            status = child.wait() => Completion::Exited(status),
            _ = tokio::time::sleep(timeout) => Completion::TimedOut,
            _ = cancellation.cancelled() => Completion::Cancelled,
        };

        let mut forced_group_cleanup = None;
        let (status, exit_code, error) = match completion {
            Completion::Exited(Ok(exit)) if exit.success() => {
                (ExecutionStatus::Succeeded, exit.code(), None)
            }
            Completion::Exited(Ok(exit)) => (
                ExecutionStatus::Failed,
                exit.code(),
                Some(format!("process exited unsuccessfully: {exit}")),
            ),
            Completion::Exited(Err(error)) => (
                ExecutionStatus::Failed,
                None,
                Some(format!("failed while waiting for process: {error}")),
            ),
            Completion::TimedOut => {
                forced_group_cleanup = Some(terminate_child(&mut child, &mut process_group).await);
                (
                    ExecutionStatus::TimedOut,
                    None,
                    Some(format!(
                        "process exceeded timeout of {} ms",
                        timeout.as_millis()
                    )),
                )
            }
            Completion::Cancelled => {
                forced_group_cleanup = Some(terminate_child(&mut child, &mut process_group).await);
                (
                    ExecutionStatus::Cancelled,
                    None,
                    Some("execution cancelled".into()),
                )
            }
        };

        // Kill background descendants that outlived a normally exiting parent,
        // and make sure they cannot keep captured pipe descriptors open.
        let group_cleanup_confirmed = match forced_group_cleanup {
            Some(confirmed) => confirmed,
            None => match process_group.kill() {
                Some(group) => confirm_process_group_gone(group).await,
                None => true,
            },
        };
        let backend_cleanup_confirmed = match &container_name {
            Some(name) => cleanup_docker_container(name).await,
            None => true,
        };
        let cleanup_confirmed = group_cleanup_confirmed && backend_cleanup_confirmed;
        if !cleanup_confirmed
            && let (Some(observer), Some(identity)) = (&request.observer, &child_identity)
        {
            observer.cleanup_after_unrecorded_spawn(identity, container_name.as_deref(), false)?;
        }
        let (mut stdout, mut stderr) = join_readers(stdout_task, stderr_task).await?;
        if matches!(
            status,
            ExecutionStatus::TimedOut | ExecutionStatus::Cancelled
        ) && stderr.is_empty()
            && let Some(message) = &error
        {
            stderr.extend_from_slice(message.as_bytes());
            stderr.push(b'\n');
        }
        let raw_stdout = stdout.clone();
        let raw_stderr = stderr.clone();
        redact_bytes(&mut stdout, &redactions);
        redact_bytes(&mut stderr, &redactions);

        write_logs(&request.stdout_path, &stdout, &request.stderr_path, &stderr).await?;
        if cleanup_confirmed && let Some(guard) = &mut docker_cleanup {
            guard.disarm();
        }
        if cleanup_confirmed
            && let (Some(observer), Some(identity)) = (&request.observer, &child_identity)
        {
            observer.cleanup_confirmed(identity)?;
        }

        Ok(ExecutionResult {
            status,
            exit_code,
            duration_ms: duration_ms(started.elapsed()),
            timed_out: status == ExecutionStatus::TimedOut,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            stdout_path: request.stdout_path,
            stderr_path: request.stderr_path,
            error,
            cleanup_confirmed,
            raw_stdout,
            raw_stderr,
        })
    }

    /// Returns the exact host command used for a request. This is public both
    /// for diagnostics and so callers can display an auditable execution plan.
    pub fn resolved_command(&self, request: &ExecutionRequest) -> Result<CommandSpec> {
        self.resolved_command_with_container(request, None)
    }

    fn resolved_command_with_container(
        &self,
        request: &ExecutionRequest,
        container_name: Option<&str>,
    ) -> Result<CommandSpec> {
        match self.config.backend.as_str() {
            "local" => {
                let mut command = request.command.clone();
                command
                    .env
                    .retain(|name, _| name == "USER" || self.config.forwarded_env.contains(name));
                for name in &self.config.forwarded_env {
                    if !command.env.contains_key(name)
                        && let Ok(value) = env::var(name)
                    {
                        command.env.insert(name.clone(), value);
                    }
                }
                Ok(command)
            }
            "docker" => self.docker_command(request, container_name),
            backend => anyhow::bail!("unsupported execution backend {backend:?}"),
        }
    }

    fn process_command(
        &self,
        request: &ExecutionRequest,
        container_name: Option<&str>,
    ) -> Result<Command> {
        let spec = self.resolved_command_with_container(request, container_name)?;
        let mut command = Command::new(&spec.program);
        command.args(&spec.args);
        command.current_dir(&request.workspace);
        command.env_clear();

        // These values configure the host process. For the Docker backend they
        // help the Docker CLI itself; container environment is still supplied
        // exclusively through explicit `--env` arguments below.
        for (name, value) in safe_local_environment() {
            command.env(name, value);
        }
        if self.config.backend == "docker" {
            command.env("PATH", trusted_host_path());
            for (name, value) in docker_host_environment() {
                command.env(name, value);
            }
        }
        for (name, value) in &spec.env {
            command.env(name, value);
        }

        Ok(command)
    }

    fn docker_command(
        &self,
        request: &ExecutionRequest,
        container_name: Option<&str>,
    ) -> Result<CommandSpec> {
        anyhow::ensure!(
            !self.config.docker_image.trim().is_empty(),
            "execution.docker_image must not be empty"
        );
        anyhow::ensure!(
            !self.config.docker_image.starts_with('-'),
            "execution.docker_image must not start with '-'"
        );

        let workspace = request.workspace.canonicalize().with_context(|| {
            format!(
                "failed to resolve workspace {}",
                request.workspace.display()
            )
        })?;
        let workspace_text = workspace.to_str().with_context(|| {
            format!(
                "Docker backend does not support a non-UTF-8 workspace path: {}",
                workspace.display()
            )
        })?;
        anyhow::ensure!(
            !workspace_text.contains(','),
            "Docker backend does not support a workspace path containing a comma: {}",
            workspace.display()
        );
        let mount = format!("type=bind,source={workspace_text},target=/workspace");
        let mut args = vec!["run".into(), "--rm".into()];
        if let Some(name) = container_name {
            args.extend(["--name".into(), name.into()]);
        }
        args.extend([
            "--cpus".into(),
            self.config.cpus.to_string(),
            "--memory".into(),
            self.config.memory.clone(),
            "--pids-limit".into(),
            "512".into(),
        ]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = std::fs::metadata(&workspace).with_context(|| {
                format!("failed to inspect workspace owner {}", workspace.display())
            })?;
            args.extend([
                "--user".into(),
                format!("{}:{}", metadata.uid(), metadata.gid()),
            ]);
        }
        args.extend([
            "--mount".into(),
            mount,
            "--workdir".into(),
            "/workspace".into(),
        ]);

        let mut forwarded = BTreeMap::new();
        for name in &self.config.forwarded_env {
            if forwarded.contains_key(name) {
                continue;
            }
            if let Some(value) = explicitly_forwarded_value(name, &request.command.env) {
                args.push("--env".into());
                args.push(name.clone());
                forwarded.insert(name.clone(), value);
            }
        }

        args.push(self.config.docker_image.clone());
        args.push(request.command.program.clone());
        args.extend(request.command.args.iter().cloned());
        let docker = trusted_host_executable("docker")?;
        let mut command = CommandSpec::new(docker.to_string_lossy()).args(args);
        command.env = forwarded;
        Ok(command)
    }

    async fn finish_without_child(
        &self,
        request: ExecutionRequest,
        status: ExecutionStatus,
        started: Instant,
        message: String,
    ) -> Result<ExecutionResult> {
        write_logs(
            &request.stdout_path,
            &[],
            &request.stderr_path,
            format!("{message}\n").as_bytes(),
        )
        .await?;
        Ok(ExecutionResult {
            status,
            exit_code: None,
            duration_ms: duration_ms(started.elapsed()),
            timed_out: status == ExecutionStatus::TimedOut,
            stdout: String::new(),
            stderr: format!("{message}\n"),
            stdout_path: request.stdout_path,
            stderr_path: request.stderr_path,
            error: Some(message),
            cleanup_confirmed: true,
            raw_stdout: Vec::new(),
            raw_stderr: Vec::new(),
        })
    }
}

async fn read_capped<R: tokio::io::AsyncRead + Unpin>(mut reader: R) -> std::io::Result<Vec<u8>> {
    let head_limit = MAX_CAPTURE_BYTES / 2;
    let tail_limit = MAX_CAPTURE_BYTES - head_limit;
    let mut head = Vec::with_capacity(head_limit.min(64 * 1024));
    let mut tail = vec![0_u8; tail_limit];
    let mut tail_seen = 0_u64;
    let mut buffer = [0_u8; 8192];
    let mut total = 0_u64;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        let head_remaining = head_limit.saturating_sub(head.len());
        let head_bytes = read.min(head_remaining);
        head.extend_from_slice(&buffer[..head_bytes]);
        for byte in &buffer[head_bytes..read] {
            tail[(tail_seen % tail_limit as u64) as usize] = *byte;
            tail_seen = tail_seen.saturating_add(1);
        }
    }
    if total <= MAX_CAPTURE_BYTES as u64 {
        let tail_len = usize::try_from(tail_seen).unwrap_or(tail_limit);
        head.extend_from_slice(&tail[..tail_len]);
    } else {
        head.extend_from_slice(
            format!(
                "\n[dispatch: output truncated after {MAX_CAPTURE_BYTES} bytes; {} bytes omitted]\n",
                total - MAX_CAPTURE_BYTES as u64
            )
            .as_bytes(),
        );
        let start = (tail_seen % tail_limit as u64) as usize;
        head.extend_from_slice(&tail[start..]);
        head.extend_from_slice(&tail[..start]);
    }
    Ok(head)
}

fn redaction_values(allowlist: &[String], command_env: &BTreeMap<String, String>) -> Vec<Vec<u8>> {
    let mut values = allowlist
        .iter()
        .filter_map(|name| explicitly_forwarded_value(name, command_env))
        .filter(|value| !value.is_empty())
        .map(String::into_bytes)
        .collect::<Vec<_>>();
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    values.dedup();
    values
}

fn redact_bytes(bytes: &mut Vec<u8>, secrets: &[Vec<u8>]) {
    for secret in secrets {
        if secret.is_empty() || bytes.len() < secret.len() {
            continue;
        }
        let mut cursor = 0;
        let mut redacted = Vec::with_capacity(bytes.len());
        while cursor + secret.len() <= bytes.len() {
            let Some(relative) = bytes[cursor..]
                .windows(secret.len())
                .position(|window| window == secret.as_slice())
            else {
                break;
            };
            let start = cursor + relative;
            redacted.extend_from_slice(&bytes[cursor..start]);
            redacted.extend_from_slice(REDACTION_MARKER);
            cursor = start + secret.len();
        }
        if cursor > 0 {
            redacted.extend_from_slice(&bytes[cursor..]);
            *bytes = redacted;
        }
    }
}

async fn join_reader(
    task: tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
    stream: &str,
) -> Result<Vec<u8>> {
    task.await
        .with_context(|| format!("{stream} capture task failed"))?
        .with_context(|| format!("failed to read child {stream}"))
}

async fn join_readers(
    stdout_task: tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
    stderr_task: tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let stdout_abort = stdout_task.abort_handle();
    let stderr_abort = stderr_task.abort_handle();
    let joined = tokio::time::timeout(OUTPUT_DRAIN_TIMEOUT, async move {
        let stdout = join_reader(stdout_task, "stdout").await?;
        let stderr = join_reader(stderr_task, "stderr").await?;
        Ok::<_, anyhow::Error>((stdout, stderr))
    })
    .await;
    match joined {
        Ok(result) => result,
        Err(_) => {
            stdout_abort.abort();
            stderr_abort.abort();
            anyhow::bail!(
                "child output pipes remained open after process exit for more than {} ms",
                OUTPUT_DRAIN_TIMEOUT.as_millis()
            )
        }
    }
}

pub(crate) struct ProcessGroupGuard {
    #[cfg(unix)]
    pgid: Option<i32>,
}

impl ProcessGroupGuard {
    pub(crate) fn new(child_id: Option<u32>) -> Self {
        Self {
            #[cfg(unix)]
            pgid: child_id.and_then(|id| i32::try_from(id).ok()),
        }
    }

    /// A GUI application's lifetime can outlive its explicitly reviewed document.
    /// Execution supervision never disarms its own cleanup guard.
    pub(crate) fn disarm(&mut self) {
        #[cfg(unix)]
        {
            self.pgid = None;
        }
    }

    pub(crate) fn kill(&mut self) -> Option<i32> {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid.take() {
            // SAFETY: `pgid` came directly from the successfully spawned child,
            // which was placed in a new process group before exec. A negative
            // PID asks kill(2) to signal that group only.
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
            return Some(pgid);
        }
        None
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

async fn terminate_child(child: &mut tokio::process::Child, group: &mut ProcessGroupGuard) -> bool {
    let killed_group = group.kill();
    // `kill` waits for the direct process on supported Tokio platforms. If it
    // races with group cleanup or natural exit there is nothing left to do.
    let _ = child.kill().await;
    let _ = child.wait().await;
    match killed_group {
        Some(group) => confirm_process_group_gone(group).await,
        None => true,
    }
}

/// How long a killed process group may take to be gone before its cleanup
/// counts as unconfirmed. SIGKILL cannot be ignored, but the group lasts until
/// each member is reaped, which a loaded machine delays past the earlier
/// 500 ms; only a process stuck in the kernel takes longer.
const GROUP_EXIT: Duration = Duration::from_secs(10);

pub(crate) async fn confirm_process_group_gone(group: i32) -> bool {
    let deadline = Instant::now() + GROUP_EXIT;
    while Instant::now() < deadline {
        if crate::process::process_group_exists(Some(group)) == Some(false) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    crate::process::process_group_exists(Some(group)) == Some(false)
}

async fn cleanup_docker_container(name: &str) -> bool {
    let Ok(docker) = trusted_host_executable("docker") else {
        return false;
    };
    let mut command = Command::new(docker);
    command
        .args(["rm", "-f", "--", name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_clear()
        .kill_on_drop(true);
    for (key, value) in safe_local_environment()
        .into_iter()
        .chain(docker_host_environment())
    {
        command.env(key, value);
    }
    command.env("PATH", trusted_host_path());
    matches!(
        tokio::time::timeout(Duration::from_secs(10), command.status()).await,
        Ok(Ok(status)) if status.success()
    )
}

struct DockerCleanupGuard {
    name: Option<String>,
}

impl DockerCleanupGuard {
    fn new(name: &str) -> Self {
        Self {
            name: Some(name.to_owned()),
        }
    }

    fn disarm(&mut self) {
        self.name = None;
    }
}

impl Drop for DockerCleanupGuard {
    fn drop(&mut self) {
        let Some(name) = self.name.take() else {
            return;
        };
        let Ok(docker) = trusted_host_executable("docker") else {
            return;
        };
        let mut command = std::process::Command::new(docker);
        command
            .args(["rm", "-f", "--", &name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env_clear();
        for (key, value) in safe_local_environment()
            .into_iter()
            .chain(docker_host_environment())
        {
            command.env(key, value);
        }
        command.env("PATH", trusted_host_path());
        // Do not wait in Drop. The detached Docker CLI owns the cleanup even
        // if orchestration is unwinding because persistence failed.
        let _ = command.spawn();
    }
}

/// Resolve Dispatch-owned host tools without consulting relative or
/// project-controlled PATH entries. Local real-agent executables are not
/// routed through this helper because those already require an explicit
/// unsafe opt-in.
pub(crate) fn trusted_host_executable(program: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        !program.is_empty()
            && !program.contains('/')
            && !program.contains('\\')
            && !program.starts_with('-'),
        "invalid trusted host tool name {program:?}"
    );

    #[cfg(unix)]
    {
        #[cfg(not(test))]
        let directories = TRUSTED_HOST_DIRECTORIES;
        // Unit tests may stand in for the system directories, so that they
        // need no host tool such as `docker`.
        #[cfg(test)]
        let directories = &tests::trusted_directories();
        for directory in directories {
            let candidate = Path::new(directory).join(program);
            if candidate.is_file() {
                return candidate.canonicalize().with_context(|| {
                    format!(
                        "failed to resolve trusted host tool {}",
                        candidate.display()
                    )
                });
            }
        }
        anyhow::bail!("trusted host tool {program:?} was not found in system locations");
    }

    #[cfg(not(unix))]
    {
        let path = which::which(program)
            .with_context(|| format!("trusted host tool {program:?} was not found"))?;
        anyhow::ensure!(
            path.is_absolute(),
            "resolved host tool path is not absolute"
        );
        Ok(path)
    }
}

pub(crate) fn trusted_host_path() -> String {
    #[cfg(unix)]
    {
        TRUSTED_HOST_DIRECTORIES.join(":")
    }
    #[cfg(not(unix))]
    {
        env::var("PATH").unwrap_or_default()
    }
}

fn validate_workspace(workspace: &Path) -> Result<()> {
    anyhow::ensure!(
        workspace.is_dir(),
        "workspace is not a directory: {}",
        workspace.display()
    );
    Ok(())
}

async fn prepare_log_path(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create log directory {}", parent.display()))?;
    }
    Ok(())
}

async fn write_logs(
    stdout_path: &Path,
    stdout: &[u8],
    stderr_path: &Path,
    stderr: &[u8],
) -> Result<()> {
    fs::write(stdout_path, stdout)
        .await
        .with_context(|| format!("failed to write {}", stdout_path.display()))?;
    fs::write(stderr_path, stderr)
        .await
        .with_context(|| format!("failed to write {}", stderr_path.display()))?;
    Ok(())
}

fn explicitly_forwarded_value(
    name: &str,
    command_env: &BTreeMap<String, String>,
) -> Option<String> {
    command_env
        .get(name)
        .cloned()
        .or_else(|| env::var(name).ok())
}

pub(crate) fn safe_local_environment() -> BTreeMap<String, String> {
    const SAFE_NAMES: [&str; 4] = ["PATH", "HOME", "TMPDIR", "LANG"];
    let mut values = BTreeMap::new();
    for name in SAFE_NAMES {
        if let Ok(value) = env::var(name) {
            values.insert(name.to_owned(), value);
        }
    }
    values
}

fn docker_host_environment() -> BTreeMap<String, String> {
    const NAMES: [&str; 6] = [
        "DOCKER_HOST",
        "DOCKER_CONTEXT",
        "DOCKER_CONFIG",
        "DOCKER_TLS_VERIFY",
        "DOCKER_CERT_PATH",
        "XDG_RUNTIME_DIR",
    ];
    let mut values = BTreeMap::new();
    for name in NAMES {
        if let Ok(value) = env::var(name) {
            values.insert(name.to_owned(), value);
        }
    }
    values
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

/// Runs configured repository checks sequentially and preserves an independent
/// result and pair of log files for every command, even after failures.
pub async fn run_checks(
    workspace: &Path,
    commands: &[String],
    phase: CheckPhase,
    output_dir: &Path,
    timeout_secs: u64,
) -> Vec<CheckResult> {
    let config = ExecutionConfig {
        backend: "local".into(),
        timeout_secs,
        ..ExecutionConfig::default()
    };
    run_checks_with_config(workspace, commands, phase, output_dir, config).await
}

/// Runs repository checks with the same local or Docker boundary selected for
/// harness execution. Agent credentials are deliberately not forwarded into
/// project-controlled check commands.
pub async fn run_checks_with_config(
    workspace: &Path,
    commands: &[String],
    phase: CheckPhase,
    output_dir: &Path,
    config: ExecutionConfig,
) -> Vec<CheckResult> {
    run_checks_with_config_and_cancel(
        workspace,
        commands,
        phase,
        output_dir,
        config,
        CancellationToken::new(),
    )
    .await
}

pub async fn run_checks_with_config_and_cancel(
    workspace: &Path,
    commands: &[String],
    phase: CheckPhase,
    output_dir: &Path,
    config: ExecutionConfig,
    cancellation: CancellationToken,
) -> Vec<CheckResult> {
    run_checks_with_observer(
        workspace,
        commands,
        phase,
        output_dir,
        config,
        cancellation,
        None,
        None,
    )
    .await
}

#[derive(Debug, Clone)]
pub enum CheckLifecycleEvent {
    Started {
        owner_id: Option<String>,
        ordinal: usize,
        phase: CheckPhase,
        name: String,
        command: String,
    },
    Finished {
        owner_id: Option<String>,
        ordinal: usize,
        result: CheckResult,
    },
}

#[allow(clippy::too_many_arguments)]
pub async fn run_checks_with_observer(
    workspace: &Path,
    commands: &[String],
    phase: CheckPhase,
    output_dir: &Path,
    mut config: ExecutionConfig,
    cancellation: CancellationToken,
    observer: Option<tokio::sync::mpsc::UnboundedSender<CheckLifecycleEvent>>,
    owner_id: Option<String>,
) -> Vec<CheckResult> {
    config.forwarded_env.clear();
    let executor = Executor::new(config);
    let mut results = Vec::with_capacity(commands.len());

    for (index, configured_command) in commands.iter().enumerate() {
        let ordinal = index + 1;
        let stem = format!("{}-{ordinal:02}", check_phase_name(&phase));
        let stdout_path = output_dir.join(format!("{stem}.stdout.log"));
        let stderr_path = output_dir.join(format!("{stem}.stderr.log"));
        // A configured check is intentionally a shell command. Passing it as a
        // single argv element avoids a second layer of interpolation here.
        let command =
            CommandSpec::new("/bin/sh").args(["-lc".to_owned(), configured_command.clone()]);
        let request =
            ExecutionRequest::new(command, workspace, stdout_path.clone(), stderr_path.clone());

        if let Some(observer) = &observer {
            let _ = observer.send(CheckLifecycleEvent::Started {
                owner_id: owner_id.clone(),
                ordinal,
                phase: phase.clone(),
                name: format!("{} {ordinal}", check_phase_name(&phase)),
                command: configured_command.clone(),
            });
        }

        let result = match executor
            .execute_with_cancel(request, cancellation.clone())
            .await
        {
            Ok(execution) => CheckResult {
                name: format!("{} {ordinal}", check_phase_name(&phase)),
                phase: phase.clone(),
                command: configured_command.clone(),
                status: match execution.status {
                    ExecutionStatus::Succeeded => CheckStatus::Passed,
                    ExecutionStatus::TimedOut => CheckStatus::TimedOut,
                    ExecutionStatus::Cancelled => CheckStatus::NotRun,
                    ExecutionStatus::Failed | ExecutionStatus::SpawnFailed => CheckStatus::Failed,
                },
                exit_code: execution.exit_code,
                duration_ms: execution.duration_ms,
                stdout_path,
                stderr_path,
            },
            Err(error) => {
                let _ = fs::create_dir_all(output_dir).await;
                let _ = fs::write(&stdout_path, []).await;
                let _ = fs::write(&stderr_path, format!("{error:#}\n")).await;
                CheckResult {
                    name: format!("{} {ordinal}", check_phase_name(&phase)),
                    phase: phase.clone(),
                    command: configured_command.clone(),
                    status: CheckStatus::NotRun,
                    exit_code: None,
                    duration_ms: 0,
                    stdout_path,
                    stderr_path,
                }
            }
        };
        if let Some(observer) = &observer {
            let _ = observer.send(CheckLifecycleEvent::Finished {
                owner_id: owner_id.clone(),
                ordinal,
                result: result.clone(),
            });
        }
        results.push(result);
    }

    results
}

fn check_phase_name(phase: &CheckPhase) -> &'static str {
    match phase {
        CheckPhase::Baseline => "baseline",
        CheckPhase::Verify => "verify",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        static TRUSTED_DIRECTORIES: std::cell::RefCell<Option<Vec<String>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// The system directories, or this test thread's stand-ins for them.
    pub(super) fn trusted_directories() -> Vec<String> {
        TRUSTED_DIRECTORIES
            .with_borrow(Clone::clone)
            .unwrap_or_else(|| {
                TRUSTED_HOST_DIRECTORIES
                    .iter()
                    .map(|directory| (*directory).to_owned())
                    .collect()
            })
    }

    #[tokio::test]
    async fn captures_success_and_failure_without_merging_streams() {
        let temp = tempfile::tempdir().unwrap();
        let success = ExecutionRequest::new(
            CommandSpec::new("/bin/sh").args(["-c", "printf out; printf err >&2"]),
            temp.path(),
            temp.path().join("stdout"),
            temp.path().join("stderr"),
        );
        let result = Executor::new(ExecutionConfig::default())
            .execute(success)
            .await
            .unwrap();

        assert_eq!(result.status, ExecutionStatus::Succeeded);
        assert_eq!(result.stdout, "out");
        assert_eq!(result.stderr, "err");

        let failure = ExecutionRequest::new(
            CommandSpec::new("/bin/sh").args(["-c", "exit 7"]),
            temp.path(),
            temp.path().join("failed.stdout"),
            temp.path().join("failed.stderr"),
        );
        let result = Executor::new(ExecutionConfig::default())
            .execute(failure)
            .await
            .unwrap();
        assert_eq!(result.status, ExecutionStatus::Failed);
        assert_eq!(result.exit_code, Some(7));
    }

    #[tokio::test]
    async fn local_environment_is_cleared_and_allowlisted() {
        let temp = tempfile::tempdir().unwrap();
        let config = ExecutionConfig {
            forwarded_env: vec!["DISPATCH_ALLOWED_TEST_VALUE".into()],
            ..ExecutionConfig::default()
        };
        let request = ExecutionRequest::new(
            CommandSpec::new("/usr/bin/env")
                .env("DISPATCH_ALLOWED_TEST_VALUE", "visible")
                .env("DISPATCH_BLOCKED_TEST_VALUE", "hidden"),
            temp.path(),
            temp.path().join("stdout"),
            temp.path().join("stderr"),
        );
        let result = Executor::new(config).execute(request).await.unwrap();

        assert!(
            result
                .stdout
                .lines()
                .any(|line| { line == "DISPATCH_ALLOWED_TEST_VALUE=[REDACTED]" })
        );
        assert!(!result.stdout.contains("DISPATCH_BLOCKED_TEST_VALUE"));
    }

    #[tokio::test]
    async fn adapter_username_reaches_child_without_forwarding_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let request = ExecutionRequest::new(
            CommandSpec::new("/bin/sh")
                .args(["-c", r#"test "$USER" = dispatch-fixture-user && test -z "${ANTHROPIC_API_KEY+x}" && test -z "${DISPATCH_CONTROL_GRANT_FD+x}""#])
                .env("USER", "dispatch-fixture-user")
                .env("ANTHROPIC_API_KEY", "forbidden")
                .env("DISPATCH_CONTROL_GRANT_FD", "199"),
            temp.path(),
            temp.path().join("stdout"),
            temp.path().join("stderr"),
        );
        let result = Executor::new(ExecutionConfig::default())
            .execute(request)
            .await
            .unwrap();
        assert_eq!(result.status, ExecutionStatus::Succeeded);
    }

    #[tokio::test]
    async fn redacts_forwarded_values_before_persisting_logs() {
        let temp = tempfile::tempdir().unwrap();
        let secret = "dispatch-test-secret-value";
        let config = ExecutionConfig {
            forwarded_env: vec!["DISPATCH_SECRET_TEST_VALUE".into()],
            ..ExecutionConfig::default()
        };
        let stdout_path = temp.path().join("stdout");
        let stderr_path = temp.path().join("stderr");
        let request = ExecutionRequest::new(
            CommandSpec::new("/bin/sh")
                .args([
                    "-c",
                    "printf '\\377%s' \"$DISPATCH_SECRET_TEST_VALUE\"; printf '%s' \"$DISPATCH_SECRET_TEST_VALUE\" >&2",
                ])
                .env("DISPATCH_SECRET_TEST_VALUE", secret),
            temp.path(),
            &stdout_path,
            &stderr_path,
        );
        let result = Executor::new(config).execute(request).await.unwrap();

        assert!(!result.stdout.contains(secret));
        assert!(!result.stderr.contains(secret));
        assert!(result.stdout.contains("[REDACTED]"));
        assert!(result.stderr.contains("[REDACTED]"));
        assert!(
            !fs::read(&stdout_path)
                .await
                .unwrap()
                .windows(secret.len())
                .any(|v| v == secret.as_bytes())
        );
        assert!(
            !fs::read(&stderr_path)
                .await
                .unwrap()
                .windows(secret.len())
                .any(|v| v == secret.as_bytes())
        );
    }

    #[tokio::test]
    async fn drains_but_caps_noisy_process_output() {
        let temp = tempfile::tempdir().unwrap();
        let stdout_path = temp.path().join("stdout");
        let request = ExecutionRequest::new(
            CommandSpec::new("/bin/sh").args(["-c", "head -c 100000 /dev/zero"]),
            temp.path(),
            &stdout_path,
            temp.path().join("stderr"),
        );
        let result = Executor::new(ExecutionConfig::default())
            .execute(request)
            .await
            .unwrap();

        assert_eq!(result.status, ExecutionStatus::Succeeded);
        assert!(result.stdout.contains("[dispatch: output truncated"));
        assert!(fs::metadata(stdout_path).await.unwrap().len() < 70_000);
    }

    #[tokio::test]
    async fn enforces_timeout() {
        let temp = tempfile::tempdir().unwrap();
        let request = ExecutionRequest::new(
            CommandSpec::new("/bin/sleep").arg("600"),
            temp.path(),
            temp.path().join("stdout"),
            temp.path().join("stderr"),
        )
        .with_timeout(Duration::from_millis(30));
        let result = Executor::new(ExecutionConfig::default())
            .execute(request)
            .await
            .unwrap();

        assert_eq!(result.status, ExecutionStatus::TimedOut);
        assert!(result.timed_out);
        // Ended by the timeout, not by `sleep` finishing.
        assert!(result.duration_ms < 600_000);
    }

    #[tokio::test]
    async fn cancellation_kills_a_running_process() {
        let temp = tempfile::tempdir().unwrap();
        let request = ExecutionRequest::new(
            CommandSpec::new("/bin/sleep").arg("600"),
            temp.path(),
            temp.path().join("stdout"),
            temp.path().join("stderr"),
        );
        let cancellation = CancellationToken::new();
        let cancel_from_task = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            cancel_from_task.cancel();
        });

        let result = Executor::new(ExecutionConfig::default())
            .execute_with_cancel(request, cancellation)
            .await
            .unwrap();
        assert_eq!(result.status, ExecutionStatus::Cancelled);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn detached_descendant_cannot_hold_capture_pipes_forever() {
        let temp = tempfile::tempdir().unwrap();
        let helper_flag = "DISPATCH_DETACHED_PIPE_HELPER";
        let pid_file = temp.path().join("descendant.pid");
        let config = ExecutionConfig {
            forwarded_env: vec![helper_flag.into()],
            ..ExecutionConfig::default()
        };
        let request = ExecutionRequest::new(
            CommandSpec::new(std::env::current_exe().unwrap().to_string_lossy())
                .args(["--exact", "executor::tests::detached_pipe_helper"])
                .env(helper_flag, pid_file.to_string_lossy()),
            temp.path(),
            temp.path().join("stdout"),
            temp.path().join("stderr"),
        );
        let error = Executor::new(config)
            .execute(request)
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("output pipes remained open"), "{error}");
        // The descendant still holds the pipes: the executor did not wait it out.
        let pid: i32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
        // SAFETY: signal 0 and SIGKILL to the descendant this test started.
        unsafe {
            assert_eq!(libc::kill(pid, 0), 0, "the descendant already exited");
            libc::kill(pid, libc::SIGKILL);
        }
    }

    #[cfg(unix)]
    #[test]
    fn detached_pipe_helper() {
        let Some(pid_file) = std::env::var_os("DISPATCH_DETACHED_PIPE_HELPER") else {
            return;
        };
        // SAFETY: the forked child restricts itself to async-signal-safe libc
        // calls and terminates with _exit; it holds the inherited pipes far
        // longer than any drain bound, until the test kills it. The parent
        // exits only once the child has left its process group, which the
        // executor kills when the parent exits.
        unsafe {
            let mut detached = [0; 2];
            assert_eq!(libc::pipe(detached.as_mut_ptr()), 0);
            let child = libc::fork();
            if child == 0 {
                libc::setsid();
                libc::write(detached[1], b"d".as_ptr().cast(), 1);
                libc::sleep(600);
                libc::_exit(0);
            }
            libc::close(detached[1]);
            let mut byte = 0u8;
            assert_eq!(libc::read(detached[0], (&raw mut byte).cast(), 1), 1);
            std::fs::write(pid_file, child.to_string()).unwrap();
            libc::_exit(0);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_killed_group_reaped_late_is_confirmed_gone() {
        use std::os::unix::process::CommandExt;
        // A loaded machine: the killed group's last member is reaped seconds
        // later, here by this test, its parent.
        let mut leader = std::process::Command::new("/bin/sleep")
            .arg("600")
            .process_group(0)
            .spawn()
            .unwrap();
        let group = leader.id() as i32;
        // SAFETY: SIGKILL to the process group this test created.
        unsafe { libc::kill(-group, libc::SIGKILL) };
        let reaper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(2));
            leader.wait().unwrap();
        });
        assert!(confirm_process_group_gone(group).await);
        reaper.join().unwrap();
    }

    #[test]
    fn trusted_directories_are_absolute_system_ones() {
        assert!(
            TRUSTED_HOST_DIRECTORIES
                .iter()
                .all(|directory| Path::new(directory).is_absolute())
        );
    }

    #[cfg(unix)]
    #[test]
    fn docker_command_has_limits_single_mount_and_forwarded_environment() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        // Stand-ins for the system directories: `docker` is resolved from the
        // trusted one, never from the workspace.
        let (missing, trusted) = (temp.path().join("missing"), temp.path().join("trusted"));
        std::fs::create_dir(&trusted).unwrap();
        for decoy in [&trusted.join("docker"), &temp.path().join("docker")] {
            std::fs::write(decoy, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(decoy, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        TRUSTED_DIRECTORIES.set(Some(vec![
            missing.to_string_lossy().into_owned(),
            trusted.to_string_lossy().into_owned(),
        ]));
        let config = ExecutionConfig {
            backend: "docker".into(),
            cpus: 1.5,
            memory: "768m".into(),
            docker_image: "dispatch-test:latest".into(),
            forwarded_env: vec!["DISPATCH_TEST_TOKEN".into()],
            ..ExecutionConfig::default()
        };
        let request = ExecutionRequest::new(
            CommandSpec::new("agent")
                .arg("run")
                .env("DISPATCH_TEST_TOKEN", "secret"),
            temp.path(),
            temp.path().join("stdout"),
            temp.path().join("stderr"),
        );
        let executor = Executor::new(config);
        let command = executor.resolved_command(&request).unwrap();

        assert_eq!(
            Path::new(&command.program),
            trusted.join("docker").canonicalize().unwrap()
        );
        assert_eq!(command.args[0..2], ["run", "--rm"]);
        assert!(command.args.windows(2).any(|a| a == ["--cpus", "1.5"]));
        assert!(command.args.windows(2).any(|a| a == ["--memory", "768m"]));
        assert!(
            command
                .args
                .windows(2)
                .any(|a| a == ["--pids-limit", "512"])
        );
        assert!(
            command
                .args
                .windows(2)
                .any(|a| a == ["--workdir", "/workspace"])
        );
        assert!(
            command
                .args
                .windows(2)
                .any(|a| a == ["--env", "DISPATCH_TEST_TOKEN"])
        );
        assert_eq!(
            command.env.get("DISPATCH_TEST_TOKEN").map(String::as_str),
            Some("secret")
        );
        assert_eq!(
            command.args.iter().filter(|arg| *arg == "--mount").count(),
            1
        );
        assert_eq!(
            &command.args[command.args.len() - 3..],
            ["dispatch-test:latest", "agent", "run"]
        );

        // Without `docker` in a trusted directory there is no command, even
        // with one in the workspace.
        TRUSTED_DIRECTORIES.set(Some(vec![missing.to_string_lossy().into_owned()]));
        let error = executor.resolved_command(&request).unwrap_err().to_string();
        assert!(error.contains("not found in system locations"), "{error}");
    }

    #[tokio::test]
    async fn checks_keep_individual_pass_and_fail_results() {
        let temp = tempfile::tempdir().unwrap();
        let commands = vec![
            "printf passed".to_owned(),
            "printf failed >&2; exit 9".to_owned(),
        ];
        let results = run_checks(
            temp.path(),
            &commands,
            CheckPhase::Verify,
            &temp.path().join("checks"),
            600,
        )
        .await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].status, CheckStatus::Passed);
        assert_eq!(results[1].status, CheckStatus::Failed);
        assert_eq!(results[1].exit_code, Some(9));
        assert_eq!(
            fs::read_to_string(&results[0].stdout_path).await.unwrap(),
            "passed"
        );
        assert_eq!(
            fs::read_to_string(&results[1].stderr_path).await.unwrap(),
            "failed"
        );
    }
}
