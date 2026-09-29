pub mod claude;
pub mod codex;

use std::{
    env,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    config::{ExecutionConfig, HarnessConfig, HarnessesConfig},
    executor::{
        CancellationToken, CommandSpec, ExecutionObserver, ExecutionRequest, ExecutionResult,
        ExecutionStatus, Executor, trusted_host_executable,
    },
};

pub const SUPPORTED_HARNESSES: &[&str] = &[
    "claude",
    "codex",
    "cursor",
    "fake-good",
    "fake-bad",
    "fake-timeout",
    "fake-crash",
];

const PROMPT_PREAMBLE: &str = "You are modifying the provided source tree.\n\
\n\
Implement the requested software task.\n\
\n\
Inspect the existing code and architecture before modifying it.\n\
Preserve unrelated behavior.\n\
Do not ask the user interactive questions.\n\
Run appropriate checks where useful.\n\
Leave the workspace in the best complete state you can.\n\
\n\
TASK:\n\n";

/// Builds the common prompt used for every competing harness.
pub fn build_prompt(task: &str) -> String {
    format!("{PROMPT_PREAMBLE}{task}")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DetectionResult {
    pub available: bool,
    pub executable: Option<PathBuf>,
    pub detail: Option<String>,
}

impl DetectionResult {
    fn found(executable: PathBuf) -> Self {
        Self {
            available: true,
            executable: Some(executable),
            detail: None,
        }
    }

    fn missing(executable: &Path, error: impl ToString) -> Self {
        Self {
            available: false,
            executable: None,
            detail: Some(format!(
                "{} was not found: {}",
                executable.display(),
                error.to_string()
            )),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HarnessTelemetry {
    #[serde(default)]
    pub usage_categories: std::collections::BTreeMap<String, u64>,
    #[serde(default)]
    pub events: Vec<Value>,
    pub tokens: Option<u64>,
    pub token_semantics: Option<String>,
    pub cost_usd: Option<f64>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub harness_version: Option<String>,
    pub semantic_error: Option<String>,
    pub failure: Option<crate::FailureKind>,
}

/// A model setup can offer for selection, with the efforts Dispatch accepts
/// for it (the first-listed default is focused).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOption {
    pub id: String,
    pub efforts: Vec<String>,
    pub default_effort: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HarnessRunRequest {
    pub read_only: bool,
    pub workspace: PathBuf,
    pub prompt: String,
    pub output_dir: PathBuf,
    pub timeout: Option<Duration>,
    pub cancellation: CancellationToken,
    pub observer: Option<Arc<dyn ExecutionObserver>>,
}

impl HarnessRunRequest {
    pub fn new(
        workspace: impl Into<PathBuf>,
        prompt: impl Into<String>,
        output_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            read_only: false,
            workspace: workspace.into(),
            prompt: prompt.into(),
            output_dir: output_dir.into(),
            timeout: None,
            cancellation: CancellationToken::new(),
            observer: None,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    pub fn with_observer(mut self, observer: Arc<dyn ExecutionObserver>) -> Self {
        self.observer = Some(observer);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessRunResult {
    pub final_result: std::result::Result<String, String>,
    #[serde(default)]
    pub usage_categories: std::collections::BTreeMap<String, u64>,
    pub harness_id: String,
    pub harness_version: Option<String>,
    pub requested_model: Option<String>,
    pub resolved_model: Option<String>,
    pub observed_model: Option<String>,
    pub requested_effort: Option<String>,
    pub resolved_effort: Option<String>,
    pub observed_effort: Option<String>,
    pub execution: ExecutionResult,
    pub tokens: Option<u64>,
    pub token_semantics: Option<String>,
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub events: Vec<Value>,
    pub failure: Option<crate::FailureKind>,
    pub checkpoint: Option<std::result::Result<crate::CheckpointReport, String>>,
}

#[async_trait]
pub trait HarnessAdapter: Send + Sync {
    fn id(&self) -> &'static str;

    fn model(&self) -> Option<&str> {
        None
    }

    fn effort(&self) -> Option<&str> {
        None
    }

    async fn detect(&self) -> DetectionResult;

    async fn version(&self) -> Result<Option<String>>;

    /// Constructs argv only. Process spawning remains the executor's job.
    fn build_command(&self, request: &HarnessRunRequest) -> Result<CommandSpec>;

    fn final_result(&self, _stdout: &str) -> std::result::Result<String, String> {
        Err("structured final results unsupported by this adapter".into())
    }

    fn checkpoint(
        &self,
        _events: &[Value],
    ) -> Option<std::result::Result<crate::CheckpointReport, String>> {
        None
    }

    async fn preflight(&self, _executor: &Executor, _request: &HarnessRunRequest) -> Result<()> {
        Ok(())
    }

    fn parse_output(&self, stdout: &str, _stderr: &str) -> HarnessTelemetry {
        parse_jsonl_telemetry(stdout)
    }
}

/// The adapter's preflight refused to launch the agent: for an included-only
/// profile, a funding or invocation-contract check failed. Nothing was spawned.
#[derive(Debug)]
pub struct PreflightRefused(pub String);

impl std::fmt::Display for PreflightRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PreflightRefused {}

/// The adapter's preflight could not observe what it checks, for example a
/// probe that did not answer in time. Nothing was spawned, but nothing was
/// seen to change either, so unlike `PreflightRefused` this is never recorded
/// as a refusal of the profile.
#[derive(Debug)]
pub struct PreflightInconclusive(pub String);

impl std::fmt::Display for PreflightInconclusive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PreflightInconclusive {}

pub async fn run_harness(
    adapter: &dyn HarnessAdapter,
    executor: &Executor,
    mut request: HarnessRunRequest,
) -> Result<HarnessRunResult> {
    if request.timeout.is_none() {
        request.timeout = Some(Duration::from_secs(executor.config().timeout_secs));
    }
    // Local detection says nothing about what a configured Docker image
    // contains. Refusing a Docker run because the host lacks that executable
    // would make image-contained harnesses impossible, and reporting the host
    // version as the container version would be misleading.
    let harness_version = if executor.config().backend == "local" {
        let detection = adapter.detect().await;
        anyhow::ensure!(
            detection.available,
            "harness {} is unavailable: {}",
            adapter.id(),
            detection
                .detail
                .as_deref()
                .unwrap_or("executable not found")
        );
        adapter.version().await.unwrap_or(None)
    } else {
        None
    };
    if let Err(error) = adapter.preflight(executor, &request).await {
        if let Some(observer) = &request.observer {
            observer.preflight_failed()?;
        }
        if error.downcast_ref::<PreflightInconclusive>().is_some() {
            return Err(error);
        }
        return Err(PreflightRefused(format!("{error:#}")).into());
    }
    let command = adapter.build_command(&request)?;
    let mut execution_request = ExecutionRequest::new(
        command,
        &request.workspace,
        request.output_dir.join("stdout.log"),
        request.output_dir.join("stderr.log"),
    );
    execution_request.timeout = request.timeout;
    execution_request.observer = request.observer.clone();
    let mut execution = executor
        .execute_with_cancel(execution_request, request.cancellation)
        .await?;
    let telemetry = {
        let stdout = execution.raw_stdout_lossy();
        let stderr = execution.raw_stderr_lossy();
        adapter.parse_output(&stdout, &stderr)
    };
    if execution.status == ExecutionStatus::Succeeded
        && let Some(error) = &telemetry.semantic_error
    {
        execution.status = ExecutionStatus::Failed;
        execution.error = Some(format!("harness reported an error: {error}"));
    }
    if let (Some(observer), Some(failure)) = (&request.observer, telemetry.failure) {
        observer.provider_failure(failure)?;
    }
    let final_result = adapter.final_result(&execution.raw_stdout_lossy());
    execution.clear_raw_capture();

    let requested_model = adapter.model().map(str::to_owned);
    let requested_effort = adapter.effort().map(str::to_owned);

    Ok(HarnessRunResult {
        final_result,
        usage_categories: telemetry.usage_categories,
        harness_id: adapter.id().into(),
        harness_version: harness_version.or(telemetry.harness_version.clone()),
        requested_model: requested_model.clone(),
        resolved_model: requested_model,
        observed_model: telemetry.model.clone(),
        requested_effort: requested_effort.clone(),
        resolved_effort: requested_effort,
        observed_effort: telemetry.effort.clone(),
        execution,
        tokens: telemetry.tokens,
        token_semantics: telemetry.token_semantics,
        cost_usd: telemetry.cost_usd,
        // A provider rejection after spawn is a failed invocation, not a
        // pre-launch deferral.
        failure: telemetry.failure.map(|failure| {
            if failure == crate::FailureKind::CapacityAdmission {
                crate::FailureKind::HarnessProcess
            } else {
                failure
            }
        }),
        checkpoint: adapter.checkpoint(&telemetry.events),
        events: telemetry.events,
    })
}

pub fn adapter_for(id: &str, configs: &HarnessesConfig) -> Result<Box<dyn HarnessAdapter>> {
    let adapter: Box<dyn HarnessAdapter> = match id {
        "claude" => Box::new(ClaudeAdapter::new(configs.claude.clone())),
        "codex" => Box::new(CodexAdapter::new(configs.codex.clone())),
        "cursor" => Box::new(CursorAdapter::new(configs.cursor.clone())),
        "fake-good" => Box::new(FakeAdapter::good()),
        "fake-bad" => Box::new(FakeAdapter::bad()),
        "fake-timeout" => Box::new(FakeAdapter::timeout()),
        "fake-crash" => Box::new(FakeAdapter::crash()),
        unknown => anyhow::bail!(
            "unknown harness {unknown:?}; supported harnesses: {}",
            SUPPORTED_HARNESSES.join(", ")
        ),
    };
    Ok(adapter)
}

#[derive(Debug, Clone)]
pub struct ClaudeAdapter {
    config: HarnessConfig,
}

impl ClaudeAdapter {
    pub fn new(config: HarnessConfig) -> Self {
        Self { config }
    }

    fn executable(&self) -> PathBuf {
        self.config
            .executable
            .clone()
            .unwrap_or_else(|| PathBuf::from("claude"))
    }
}

#[async_trait]
impl HarnessAdapter for ClaudeAdapter {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn model(&self) -> Option<&str> {
        self.config.model.as_deref()
    }

    async fn detect(&self) -> DetectionResult {
        detect_executable(&self.executable())
    }

    async fn version(&self) -> Result<Option<String>> {
        probe_version(&self.executable()).await
    }

    fn effort(&self) -> Option<&str> {
        self.config.effort.as_deref()
    }

    async fn preflight(&self, executor: &Executor, request: &HarnessRunRequest) -> Result<()> {
        if let Some(evidence) = &self.config.claude_subscription {
            claude::preflight(&self.executable(), evidence, executor, request).await?;
        }
        Ok(())
    }

    fn checkpoint(
        &self,
        events: &[Value],
    ) -> Option<std::result::Result<crate::CheckpointReport, String>> {
        claude::checkpoint(events)
    }

    fn final_result(&self, stdout: &str) -> std::result::Result<String, String> {
        structured_final(stdout, "claude")
    }

    fn parse_output(&self, stdout: &str, _stderr: &str) -> HarnessTelemetry {
        claude::parse_output(
            stdout,
            self.config
                .allocation_service_mode
                .as_ref()
                .and(self.config.model.as_deref()),
        )
    }

    fn build_command(&self, request: &HarnessRunRequest) -> Result<CommandSpec> {
        claude::command(&self.executable(), &self.config, request)
    }
}

#[derive(Debug, Clone)]
pub struct CodexAdapter {
    config: HarnessConfig,
}

impl CodexAdapter {
    pub fn new(config: HarnessConfig) -> Self {
        Self { config }
    }

    fn executable(&self) -> PathBuf {
        self.config
            .executable
            .clone()
            .unwrap_or_else(|| PathBuf::from("codex"))
    }
}

#[async_trait]
impl HarnessAdapter for CodexAdapter {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn model(&self) -> Option<&str> {
        self.config.model.as_deref()
    }

    fn effort(&self) -> Option<&str> {
        self.config.effort.as_deref()
    }

    async fn detect(&self) -> DetectionResult {
        detect_executable(&self.executable())
    }

    async fn version(&self) -> Result<Option<String>> {
        probe_version(&self.executable()).await
    }

    fn checkpoint(
        &self,
        events: &[Value],
    ) -> Option<std::result::Result<crate::CheckpointReport, String>> {
        codex_checkpoint(events)
    }

    fn final_result(&self, stdout: &str) -> std::result::Result<String, String> {
        structured_final(stdout, "codex")
    }

    async fn preflight(&self, _executor: &Executor, request: &HarnessRunRequest) -> Result<()> {
        // Only a run bound to an included-only profile carries a funding
        // contract; an explicit `--agent codex` run has none to check.
        if self.config.allocation_service_mode.is_some() {
            codex::preflight(
                &self.executable(),
                self.config.codex_account.as_ref(),
                self.config.funding_source.as_deref().unwrap_or_default(),
                request
                    .timeout
                    .unwrap_or(Duration::from_secs(5))
                    .min(Duration::from_secs(5)),
            )
            .await?;
        }
        Ok(())
    }

    fn build_command(&self, request: &HarnessRunRequest) -> Result<CommandSpec> {
        let mut args = vec![
            "exec".into(),
            "--ephemeral".into(),
            "--sandbox".into(),
            if request.read_only {
                "read-only"
            } else {
                "workspace-write"
            }
            .into(),
            "--json".into(),
            "-C".into(),
            ".".into(),
        ];
        push_model_and_extra_args(&mut args, &self.config);
        if let Some(effort) = &self.config.effort {
            args.push("-c".into());
            args.push(format!("model_reasoning_effort=\"{effort}\""));
        }
        if self.config.allocation_service_mode.as_deref() == Some("standard") {
            args.push("-c".into());
            args.push("service_tier=\"default\"".into());
        }
        args.push(request.prompt.clone());
        Ok(CommandSpec::new(path_string(&self.executable())).args(args))
    }
}

#[derive(Debug, Clone)]
pub struct CursorAdapter {
    config: HarnessConfig,
}

impl CursorAdapter {
    pub fn new(config: HarnessConfig) -> Self {
        Self { config }
    }

    fn executable(&self) -> PathBuf {
        self.config
            .executable
            .clone()
            .unwrap_or_else(|| PathBuf::from("cursor-agent"))
    }
}

#[async_trait]
impl HarnessAdapter for CursorAdapter {
    fn id(&self) -> &'static str {
        "cursor"
    }

    fn model(&self) -> Option<&str> {
        self.config.model.as_deref()
    }

    async fn detect(&self) -> DetectionResult {
        detect_executable(&self.executable())
    }

    async fn version(&self) -> Result<Option<String>> {
        probe_version(&self.executable()).await
    }

    fn build_command(&self, request: &HarnessRunRequest) -> Result<CommandSpec> {
        let mut args = vec![
            "-p".into(),
            "--force".into(),
            "--trust".into(),
            "--output-format".into(),
            "stream-json".into(),
        ];
        push_model_and_extra_args(&mut args, &self.config);
        args.push(request.prompt.clone());
        Ok(CommandSpec::new(path_string(&self.executable())).args(args))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FakeKind {
    Good,
    Bad,
    Timeout,
    Crash,
}

#[derive(Debug, Clone)]
pub struct FakeAdapter {
    kind: FakeKind,
}

impl FakeAdapter {
    pub fn good() -> Self {
        Self {
            kind: FakeKind::Good,
        }
    }

    pub fn bad() -> Self {
        Self {
            kind: FakeKind::Bad,
        }
    }

    pub fn timeout() -> Self {
        Self {
            kind: FakeKind::Timeout,
        }
    }

    pub fn crash() -> Self {
        Self {
            kind: FakeKind::Crash,
        }
    }

    fn executable(&self) -> Result<PathBuf> {
        match self.kind {
            FakeKind::Good | FakeKind::Bad => trusted_host_executable("sh"),
            FakeKind::Timeout => trusted_host_executable("sleep"),
            FakeKind::Crash => trusted_host_executable("env"),
        }
    }
}

#[async_trait]
impl HarnessAdapter for FakeAdapter {
    fn id(&self) -> &'static str {
        match self.kind {
            FakeKind::Good => "fake-good",
            FakeKind::Bad => "fake-bad",
            FakeKind::Timeout => "fake-timeout",
            FakeKind::Crash => "fake-crash",
        }
    }

    async fn detect(&self) -> DetectionResult {
        match self.executable() {
            Ok(executable) => DetectionResult::found(executable),
            Err(error) => DetectionResult::missing(Path::new(self.id()), error),
        }
    }

    async fn version(&self) -> Result<Option<String>> {
        Ok(Some("builtin-v0".into()))
    }

    fn build_command(&self, request: &HarnessRunRequest) -> Result<CommandSpec> {
        const SAFE_CREATE: &str = "base=$1; path=$base; index=0; while [ -e \"$path\" ]; do [ ! -L \"$path\" ] || exit 73; index=$((index + 1)); [ \"$index\" -le 1000 ] || exit 73; path=\"$base.$index\"; done; [ ! -L \"$path\" ] || exit 73; set -C; : > \"$path\"";
        let command = match self.kind {
            FakeKind::Good => CommandSpec::new(path_string(&self.executable()?)).args([
                "-c",
                SAFE_CREATE,
                "dispatch-fake",
                "dispatch-fake-good.txt",
            ]),
            FakeKind::Bad => CommandSpec::new(path_string(&self.executable()?)).args([
                "-c",
                SAFE_CREATE,
                "dispatch-fake",
                "dispatch-fake-bad.txt",
            ]),
            FakeKind::Timeout => {
                let delay = request
                    .timeout
                    .unwrap_or_else(|| Duration::from_secs(60))
                    .saturating_add(Duration::from_secs(1));
                CommandSpec::new(path_string(&self.executable()?))
                    .arg(format!("{:.3}", delay.as_secs_f64()))
            }
            // `env` reports a useful error to stderr and returns 127 when it
            // cannot find the requested program.
            FakeKind::Crash => CommandSpec::new(path_string(&self.executable()?))
                .arg("dispatch-intentional-fake-crash-command"),
        };
        Ok(command)
    }

    fn parse_output(&self, _stdout: &str, _stderr: &str) -> HarnessTelemetry {
        HarnessTelemetry::default()
    }
}

fn push_model_and_extra_args(args: &mut Vec<String>, config: &HarnessConfig) {
    if let Some(model) = &config.model {
        args.push("--model".into());
        args.push(model.clone());
    }
    args.extend(config.extra_args.iter().cloned());
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn detect_executable(executable: &Path) -> DetectionResult {
    match which::which(executable) {
        Ok(path) => DetectionResult::found(path),
        Err(error) => DetectionResult::missing(executable, error),
    }
}

pub(crate) async fn probe_version(executable: &Path) -> Result<Option<String>> {
    let temporary = tempfile::Builder::new()
        .prefix("dispatch-version-")
        .tempdir()
        .context("failed to create version-probe workspace")?;
    let mut command = CommandSpec::new(path_string(executable)).arg("--version");
    let path = if let Some(parent) = executable.parent().filter(|_| executable.is_absolute()) {
        let mut paths = vec![parent.to_path_buf()];
        paths.extend(env::split_paths(&std::ffi::OsString::from(
            crate::executor::trusted_host_path(),
        )));
        env::join_paths(paths).ok()
    } else {
        env::var_os("PATH")
    };
    if let Some(path) = path {
        command = command.env("PATH", path.to_string_lossy());
    }
    let config = ExecutionConfig {
        timeout_secs: 5,
        forwarded_env: vec!["PATH".into()],
        ..ExecutionConfig::default()
    };
    let request = ExecutionRequest::new(
        command,
        temporary.path(),
        temporary.path().join("stdout.log"),
        temporary.path().join("stderr.log"),
    )
    .with_timeout(Duration::from_secs(5));
    let output = Executor::new(config)
        .execute(request)
        .await
        .with_context(|| format!("failed to run {} --version", executable.display()))?;
    if output.status != ExecutionStatus::Succeeded {
        return Ok(None);
    }
    let version = output
        .stdout
        .lines()
        .chain(output.stderr.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned);
    Ok(version)
}

/// Parses newline-delimited structured harness events while leaving unknown
/// values unknown. Token and cost values come only from fields the harness
/// reports directly.
pub fn parse_jsonl_telemetry(output: &str) -> HarnessTelemetry {
    let events: Vec<Value> = output
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    let reported_usage = events
        .iter()
        .filter_map(reported_tokens)
        .filter(|usage| usage.tokens <= i64::MAX as u64)
        .max_by_key(|usage| usage.tokens);
    let tokens = reported_usage.map(|usage| usage.tokens);
    let token_semantics = reported_usage.map(|usage| usage.semantics.to_owned());
    let cost_usd = events.iter().filter_map(reported_cost).reduce(f64::max);
    let model = events
        .iter()
        .rev()
        .find_map(|event| reported_text(event, &["model", "model_name", "modelName"]));
    let effort = events
        .iter()
        .rev()
        .find_map(|event| reported_text(event, &["effort", "reasoning_effort", "reasoningEffort"]));
    let harness_version = events.iter().rev().find_map(|event| {
        reported_text(event, &["harness_version", "cli_version", "agent_version"])
    });
    let semantic_error = events.iter().find_map(reported_semantic_error);
    HarnessTelemetry {
        usage_categories: Default::default(),
        events,
        tokens,
        token_semantics,
        cost_usd,
        model,
        effort,
        harness_version,
        semantic_error,
        failure: None,
    }
}

fn reported_semantic_error(value: &Value) -> Option<String> {
    let object = value.as_object()?;
    let is_error = object.get("is_error").and_then(Value::as_bool) == Some(true);
    let error_type = object
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind == "error" || kind.ends_with(".failed"));
    if !is_error && !error_type {
        return None;
    }
    reported_text(value, &["error", "message", "result"])
        .or_else(|| Some("unspecified structured harness error".into()))
}

fn reported_text(value: &Value, keys: &[&str]) -> Option<String> {
    if let Some(object) = value.as_object() {
        for key in keys {
            if let Some(text) = object.get(*key).and_then(Value::as_str) {
                let text = text.trim();
                if !text.is_empty() && text.len() <= 256 {
                    return Some(text.to_owned());
                }
            }
        }
        return object
            .values()
            .find_map(|nested| reported_text(nested, keys));
    }
    value
        .as_array()
        .and_then(|items| items.iter().find_map(|nested| reported_text(nested, keys)))
}

#[derive(Clone, Copy)]
struct ReportedTokens {
    tokens: u64,
    semantics: &'static str,
}

fn reported_tokens(value: &Value) -> Option<ReportedTokens> {
    if let Some(tokens) = field_u64(value, &["total_tokens", "tokens_used"]) {
        return Some(ReportedTokens {
            tokens,
            semantics: "harness-reported-total",
        });
    }

    for key in ["usage", "token_usage", "usage_metadata"] {
        if let Some(usage) = value.get(key) {
            let input = field_u64(
                usage,
                &[
                    "input_tokens",
                    "prompt_tokens",
                    "inputTokens",
                    "promptTokens",
                ],
            );
            let output = field_u64(
                usage,
                &[
                    "output_tokens",
                    "completion_tokens",
                    "outputTokens",
                    "completionTokens",
                ],
            );
            let cached_input = field_u64(usage, &["cached_input_tokens", "cachedInputTokens"])
                .or_else(|| {
                    usage
                        .get("input_tokens_details")
                        .and_then(|details| field_u64(details, &["cached_tokens"]))
                });
            let cache_creation = field_u64(
                usage,
                &["cache_creation_input_tokens", "cacheCreationInputTokens"],
            );
            if input.is_some() || output.is_some() || cache_creation.is_some() {
                let semantics = if cached_input.is_some() || cache_creation.is_some() {
                    "uncached-input+output+cache-creation"
                } else {
                    "input+output"
                };
                return Some(ReportedTokens {
                    tokens: input
                        .unwrap_or(0)
                        .saturating_sub(cached_input.unwrap_or(0))
                        .saturating_add(output.unwrap_or(0))
                        .saturating_add(cache_creation.unwrap_or(0)),
                    semantics,
                });
            }
            if let Some(tokens) = field_u64(usage, &["total_tokens", "tokens_used"]) {
                return Some(ReportedTokens {
                    tokens,
                    semantics: "harness-reported-total",
                });
            }
        }
    }

    value
        .as_object()
        .and_then(|object| {
            object
                .values()
                .filter_map(reported_tokens)
                .max_by_key(|usage| usage.tokens)
        })
        .or_else(|| {
            value.as_array().and_then(|items| {
                items
                    .iter()
                    .filter_map(reported_tokens)
                    .max_by_key(|usage| usage.tokens)
            })
        })
}

fn reported_cost(value: &Value) -> Option<f64> {
    if let Some(cost) = field_f64(value, &["cost_usd", "total_cost_usd", "costUsd"]) {
        return Some(cost);
    }
    value
        .as_object()
        .and_then(|object| object.values().filter_map(reported_cost).reduce(f64::max))
        .or_else(|| {
            value
                .as_array()
                .and_then(|items| items.iter().filter_map(reported_cost).reduce(f64::max))
        })
}

fn field_u64(value: &Value, names: &[&str]) -> Option<u64> {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(Value::as_u64))
}

fn field_f64(value: &Value, names: &[&str]) -> Option<f64> {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(Value::as_f64))
}

/// Strict final boundary for plans and scoped dependency reports; never scan fragments.
fn structured_final(stdout: &str, harness: &str) -> std::result::Result<String, String> {
    (|| -> Result<String> {
        let events = stdout
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(serde_json::from_str::<Value>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let last = events.last().context("missing final event")?;
        let text = if harness == "claude" {
            anyhow::ensure!(
                last["type"] == "result"
                    && last["subtype"] == "success"
                    && last["is_error"] == false
                    && events.iter().filter(|e| e["type"] == "result").count() == 1,
                "missing or nonfinal successful result"
            );
            last["result"].as_str().context("malformed final result")?
        } else {
            anyhow::ensure!(
                last["type"] == "turn.completed"
                    && !events
                        .iter()
                        .any(|e| matches!(e["type"].as_str(), Some("error" | "turn.failed"))),
                "missing or failed terminal turn"
            );
            let message = events
                .iter()
                .rev()
                .find(|e| e["type"] == "item.completed" && e["item"]["type"] == "agent_message")
                .context("missing final agent message")?;
            message["item"]["text"]
                .as_str()
                .context("malformed final agent message")?
        };
        anyhow::ensure!(
            text.len() <= 32768,
            "structured final result exceeds 32 KiB"
        );
        Ok(text.to_owned())
    })()
    .map_err(|e| e.to_string())
}

/// Only the last completed `agent_message` item can supply a checkpoint.
/// Its text must be a string containing one bounded, versioned JSON envelope.
/// A malformed final item fails closed; earlier malformed items cannot hide a
/// valid final report, and an earlier checkpoint never substitutes for it.
pub(crate) fn codex_checkpoint(
    events: &[Value],
) -> Option<std::result::Result<crate::CheckpointReport, String>> {
    let mut messages = events.iter().filter(|event| {
        event["type"] == "item.completed" && event["item"]["type"] == "agent_message"
    });
    let final_message = messages.next_back()?;
    let Some(text) = final_message["item"]["text"].as_str() else {
        return Some(Err(
            "final agent message has a malformed text payload".into()
        ));
    };
    if !text.contains("dispatch_checkpoint") {
        return messages
            .any(|event| {
                event["item"]["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("dispatch_checkpoint"))
            })
            .then(|| Err("checkpoint was not the final agent message".into()));
    }
    checkpoint_envelope(text)
}

pub(crate) fn checkpoint_envelope(
    text: &str,
) -> Option<std::result::Result<crate::CheckpointReport, String>> {
    Some(
        (|| -> Result<crate::CheckpointReport> {
            anyhow::ensure!(text.len() <= 16_384, "checkpoint report too large");
            let envelope: Value = serde_json::from_str(text)?;
            anyhow::ensure!(
                envelope.as_object().is_some_and(|v| v.len() == 1),
                "checkpoint must be a single structured envelope"
            );
            let value: crate::CheckpointReport =
                serde_json::from_value(envelope["dispatch_checkpoint"].clone())?;
            anyhow::ensure!(
                value.version == 1
                    && !value.question.trim().is_empty()
                    && value.question.len() <= 4096
                    && value.choices.len() <= 8
                    && value
                        .choices
                        .iter()
                        .all(|s| !s.trim().is_empty() && s.len() <= 512),
                "invalid checkpoint payload"
            );
            Ok(value)
        })()
        .map_err(|e| format!("unsupported checkpoint: {e}")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::ExecutionConfig, executor::ExecutionStatus};

    fn request(temp: &tempfile::TempDir) -> HarnessRunRequest {
        HarnessRunRequest::new(temp.path(), "do the task", temp.path().join("logs"))
    }

    #[test]
    fn constructs_current_official_agent_commands() {
        let temp = tempfile::tempdir().unwrap();
        let configured = HarnessConfig {
            read_only: false,
            model: Some("test-model".into()),
            effort: Some("high".into()),
            executable: Some(PathBuf::from("custom-agent")),
            extra_args: vec!["--extra".into()],
            allocation_service_mode: None,
            codex_account: None,
            funding_source: None,
            claude_subscription: None,
        };

        let codex = CodexAdapter::new(configured.clone())
            .build_command(&request(&temp))
            .unwrap();
        assert_eq!(codex.program, "custom-agent");
        assert_eq!(
            &codex.args[..7],
            [
                "exec",
                "--ephemeral",
                "--sandbox",
                "workspace-write",
                "--json",
                "-C",
                "."
            ]
        );
        assert!(!codex.args.iter().any(|arg| arg == "--full-auto"));
        assert_eq!(
            &codex.args[codex.args.len() - 6..],
            [
                "--model",
                "test-model",
                "--extra",
                "-c",
                "model_reasoning_effort=\"high\"",
                "do the task"
            ]
        );

        let cursor = CursorAdapter::new(configured.clone())
            .build_command(&request(&temp))
            .unwrap();
        assert_eq!(
            &cursor.args[..5],
            ["-p", "--force", "--trust", "--output-format", "stream-json"]
        );

        let claude = ClaudeAdapter::new(configured)
            .build_command(&request(&temp))
            .unwrap();
        assert_eq!(
            &claude.args[..5],
            [
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--permission-mode"
            ]
        );
        assert_eq!(claude.args.last().unwrap(), "do the task");
    }

    #[tokio::test]
    async fn fake_good_and_bad_mutate_only_their_workspace() {
        let parent = tempfile::tempdir().unwrap();
        let good_workspace = parent.path().join("good");
        let bad_workspace = parent.path().join("bad");
        std::fs::create_dir_all(&good_workspace).unwrap();
        std::fs::create_dir_all(&bad_workspace).unwrap();
        let executor = Executor::new(ExecutionConfig::default());

        let good = run_harness(
            &FakeAdapter::good(),
            &executor,
            HarnessRunRequest::new(&good_workspace, "ignored", parent.path().join("good-logs")),
        )
        .await
        .unwrap();
        let bad = run_harness(
            &FakeAdapter::bad(),
            &executor,
            HarnessRunRequest::new(&bad_workspace, "ignored", parent.path().join("bad-logs")),
        )
        .await
        .unwrap();

        assert_eq!(good.execution.status, ExecutionStatus::Succeeded);
        assert_eq!(bad.execution.status, ExecutionStatus::Succeeded);
        assert_eq!(good.observed_model, None);
        assert_eq!(good.observed_effort, None);
        assert!(good_workspace.join("dispatch-fake-good.txt").is_file());
        assert!(!good_workspace.join("dispatch-fake-bad.txt").exists());
        assert!(bad_workspace.join("dispatch-fake-bad.txt").is_file());
        assert!(!parent.path().join("dispatch-fake-good.txt").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_file_creation_refuses_symlink_escape() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let workspace = parent.path().join("workspace");
        let outside = parent.path().join("outside.txt");
        std::fs::create_dir_all(&workspace).unwrap();
        symlink(&outside, workspace.join("dispatch-fake-good.txt")).unwrap();

        let result = run_harness(
            &FakeAdapter::good(),
            &Executor::new(ExecutionConfig::default()),
            HarnessRunRequest::new(&workspace, "ignored", parent.path().join("logs")),
        )
        .await
        .unwrap();

        assert_eq!(result.execution.status, ExecutionStatus::Failed);
        assert!(!outside.exists());
    }

    #[tokio::test]
    async fn fake_file_creation_uses_a_safe_suffix_on_repeated_runs() {
        let parent = tempfile::tempdir().unwrap();
        let workspace = parent.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("dispatch-fake-good.txt"), "original\n").unwrap();

        let result = run_harness(
            &FakeAdapter::good(),
            &Executor::new(ExecutionConfig::default()),
            HarnessRunRequest::new(&workspace, "ignored", parent.path().join("logs")),
        )
        .await
        .unwrap();

        assert_eq!(result.execution.status, ExecutionStatus::Succeeded);
        assert_eq!(
            std::fs::read_to_string(workspace.join("dispatch-fake-good.txt")).unwrap(),
            "original\n"
        );
        assert!(workspace.join("dispatch-fake-good.txt.1").is_file());
    }

    #[tokio::test]
    async fn fake_timeout_and_crash_have_distinct_results() {
        let temp = tempfile::tempdir().unwrap();
        let timeout_workspace = temp.path().join("timeout");
        let crash_workspace = temp.path().join("crash");
        std::fs::create_dir_all(&timeout_workspace).unwrap();
        std::fs::create_dir_all(&crash_workspace).unwrap();
        let executor = Executor::new(ExecutionConfig::default());

        let timed_out = run_harness(
            &FakeAdapter::timeout(),
            &executor,
            HarnessRunRequest::new(
                &timeout_workspace,
                "ignored",
                temp.path().join("timeout-logs"),
            )
            .with_timeout(Duration::from_millis(30)),
        )
        .await
        .unwrap();
        assert_eq!(timed_out.execution.status, ExecutionStatus::TimedOut);

        let crashed = run_harness(
            &FakeAdapter::crash(),
            &executor,
            HarnessRunRequest::new(&crash_workspace, "ignored", temp.path().join("crash-logs")),
        )
        .await
        .unwrap();
        assert_eq!(crashed.execution.status, ExecutionStatus::Failed);
        assert_ne!(crashed.execution.exit_code, Some(0));
        assert!(!crashed.execution.stderr.trim().is_empty());
    }

    #[test]
    fn parses_jsonl_events_and_directly_reported_usage() {
        let telemetry = parse_jsonl_telemetry(
            "not json\n{\"type\":\"start\"}\n{\"type\":\"result\",\"model\":\"agent-model-v1\",\"cli_version\":\"2.4.0\",\"usage\":{\"input_tokens\":12,\"cache_creation_input_tokens\":2,\"cache_read_input_tokens\":3,\"output_tokens\":5},\"total_cost_usd\":0.031}\n",
        );
        assert_eq!(telemetry.events.len(), 2);
        assert_eq!(telemetry.tokens, Some(19));
        assert_eq!(
            telemetry.token_semantics.as_deref(),
            Some("uncached-input+output+cache-creation")
        );
        assert_eq!(telemetry.cost_usd, Some(0.031));
        assert_eq!(telemetry.model.as_deref(), Some("agent-model-v1"));
        assert_eq!(telemetry.harness_version.as_deref(), Some("2.4.0"));

        let codex = parse_jsonl_telemetry(
            "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":49365,\"cached_input_tokens\":42240,\"output_tokens\":367}}\n",
        );
        assert_eq!(codex.tokens, Some(7_492));
        assert_eq!(
            codex.token_semantics.as_deref(),
            Some("uncached-input+output+cache-creation")
        );

        let overflow = parse_jsonl_telemetry("{\"total_tokens\":18446744073709551615}\n");
        assert_eq!(overflow.tokens, None);
    }

    #[test]
    fn recognizes_structured_harness_errors_and_observed_effort() {
        let telemetry = parse_jsonl_telemetry(
            "{\"type\":\"turn.failed\",\"message\":\"provider rejected the request\",\"model\":\"observed-model\",\"reasoning_effort\":\"high\"}\n",
        );
        assert_eq!(telemetry.model.as_deref(), Some("observed-model"));
        assert_eq!(telemetry.effort.as_deref(), Some("high"));
        assert_eq!(
            telemetry.semantic_error.as_deref(),
            Some("provider rejected the request")
        );
    }

    #[test]
    fn prompt_preserves_task_verbatim() {
        let task = "Fix it.\nDo not change the API.\n";
        let prompt = build_prompt(task);
        assert!(prompt.ends_with(task));
        assert!(prompt.contains("Do not ask the user interactive questions."));
    }
}
