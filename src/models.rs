use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::process::ProcessIdentity;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewOutcome {
    Accepted,
    Rejected,
}

impl ReviewOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Git,
    GitWorktree,
    Directory,
}

impl SourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::GitWorktree => "git_worktree",
            Self::Directory => "directory",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Preparing,
    Running,
    ReadyForEvaluation,
    Evaluated,
    Applied,
    Interrupted,
    Deferred,
    Failed,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    /// Work Dispatch selected an agent for and executed. Runs made before
    /// 0.4.1 recorded `legacy`, `routed`, `allocation` or `comparison`.
    #[default]
    #[serde(
        alias = "legacy",
        alias = "routed",
        alias = "allocation",
        alias = "comparison"
    )]
    Native,
    /// External work Dispatch observes and can apply, but did not select,
    /// route or execute. See `AttachmentRecord`.
    Attached,
}

impl RunMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Attached => "attached",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Preparing,
    Working,
    Waiting,
    #[default]
    Finished,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkResult {
    #[default]
    Pending,
    Ready,
    Failed,
    Cancelled,
    Interrupted,
    Deferred,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerificationState {
    #[default]
    NotConfigured,
    NotRun,
    Passed,
    Failed,
    Inconclusive,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    NotRequested,
    #[default]
    Pending,
    Accepted,
    Rejected,
    Deferred,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationState {
    #[default]
    NotApplied,
    Applied,
    BlockedBySourceDrift,
    Failed,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunPhase {
    Preparing,
    Planning,
    Integrating,
    Executing,
    Verifying,
    Reviewing,
    Applying,
    #[default]
    Finished,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WaitingOn {
    #[default]
    None,
    Human,
    Capacity,
    Dependency,
    Authorization,
    Admission,
    Reconciliation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunOutcome {
    pub version: u32,
    pub lifecycle: LifecycleState,
    pub work_result: WorkResult,
    pub verification: VerificationState,
    pub review: ReviewState,
    pub application: ApplicationState,
    pub phase: RunPhase,
    pub waiting_on: WaitingOn,
    /// Who applied the result: a human decision or the auto-apply policy.
    /// `None` when nothing was applied, or for records written before the
    /// actor was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_by: Option<AppliedBy>,
}

/// The authority under which a candidate was applied to the source. It is
/// recorded on the outcome so that an application by policy can never be
/// mistaken for human acceptance.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AppliedBy {
    Human,
    AutoApply,
}

impl Default for RunOutcome {
    fn default() -> Self {
        Self {
            version: 1,
            lifecycle: LifecycleState::Finished,
            work_result: WorkResult::Pending,
            verification: VerificationState::NotConfigured,
            review: ReviewState::Pending,
            application: ApplicationState::NotApplied,
            phase: RunPhase::Finished,
            waiting_on: WaitingOn::None,
            applied_by: None,
        }
    }
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Running => "running",
            Self::ReadyForEvaluation => "ready_for_evaluation",
            Self::Evaluated => "evaluated",
            Self::Applied => "applied",
            Self::Interrupted => "interrupted",
            Self::Deferred => "deferred",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStatus {
    Preparing,
    Running,
    Verifying,
    Completed,
    Failed,
    TimedOut,
    Cancelled,
    MissingHarness,
}

impl CandidateStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Running => "running",
            Self::Verifying => "verifying",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            Self::MissingHarness => "missing_harness",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckPhase {
    Baseline,
    Verify,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Passed,
    Failed,
    TimedOut,
    NotRun,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub phase: CheckPhase,
    pub command: String,
    pub status: CheckStatus,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiffStats {
    pub files_changed: u64,
    pub lines_added: u64,
    pub lines_removed: u64,
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub untracked_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateRecord {
    pub id: String,
    pub label: String,
    pub harness_id: String,
    pub harness_version: Option<String>,
    pub model: Option<String>,
    pub status: CandidateStatus,
    pub workspace_path: PathBuf,
    pub prompt_path: PathBuf,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    pub diff_path: PathBuf,
    pub duration_ms: u64,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Uncached input plus output, with cache creation included when reported separately.
    pub tokens: Option<u64>,
    #[serde(default)]
    pub token_semantics: Option<String>,
    pub cost_usd: Option<f64>,
    pub error: Option<String>,
    pub diff_stats: DiffStats,
    #[serde(default)]
    pub checks: Vec<CheckResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentRecord {
    pub dispatch_version: String,
    pub os: String,
    pub architecture: String,
    pub execution_backend: String,
    pub timeout_secs: u64,
    pub cpus: f64,
    pub memory: String,
    /// Agents run at once for this run: always 1 since 0.4.1.
    pub max_parallel: usize,
    #[serde(default)]
    pub docker_image: Option<String>,
    #[serde(default)]
    pub resource_limits_enforced: bool,
    #[serde(default)]
    pub unsafe_local: bool,
    #[serde(default)]
    pub forwarded_env: Vec<String>,
}

/// Latest work-coherence state of a run against the moving source tree.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CoherenceRecord {
    #[serde(default = "coherence_version")]
    pub version: u32,
    #[serde(default)]
    pub refreshed_from: Option<String>,
    #[serde(default)]
    pub validity: Option<Validity>,
    #[serde(default)]
    pub first_invalid_at: Option<DateTime<Utc>>,
    /// The REFRESH a human overrode to apply this result (`dispatch accept
    /// --despite-refresh`), kept as evidence; `validity` then holds the
    /// merged-tree verification the override required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overridden: Option<Validity>,
}

fn coherence_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MustHold {
    pub id: String,
    pub kind: FactKind,
    pub path: String,
    #[serde(default)]
    pub subject: String,
    pub origin: FactOrigin,
    #[serde(default)]
    pub sig_fp: String,
    #[serde(default)]
    pub full_fp: Option<String>,
    #[serde(default)]
    pub display: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FactKind {
    Signature,
    File,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FactOrigin {
    Modified,
    Referenced,
    FileFallback,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Continue,
    Refresh,
    Stop,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisLevel {
    Symbols,
    FilesOnly,
    Integration,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    FactBroken,
    FactMissing,
    SameSymbolEdited,
    PatchConflict,
    AlreadyApplied,
    IntegrationCheckFailed,
    AnalysisUncertain,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Reason {
    pub code: ReasonCode,
    #[serde(default)]
    pub fact_id: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Validity {
    pub decision: Decision,
    pub evaluated_at: DateTime<Utc>,
    #[serde(default)]
    pub world_digest: String,
    #[serde(default)]
    pub world_changed: bool,
    #[serde(default)]
    pub changed_files: u32,
    /// Capped at 20.
    #[serde(default)]
    pub reasons: Vec<Reason>,
    pub analysis: AnalysisLevel,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CoherenceSummary {
    pub decision: Decision,
    /// Capped at 5.
    #[serde(default)]
    pub reasons: Vec<Reason>,
    #[serde(default)]
    pub changed_files: u32,
    pub analysis: AnalysisLevel,
}

/// The JSON/JSONL projection of an automatic application attempt
/// (`orchestrator::apply::ApplyOutcome`), carried on `RunResult.auto_apply`
/// only when `run --auto-apply`/`refresh --auto-apply` attempted one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutoApplySummary {
    /// "applied" | "blocked" | "skipped" | "failed".
    pub outcome: String,
    /// The skip/block reason, or the error text for a failed application.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The validity the decision was made against, present only when one
    /// exists (an unmoved world produces none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coherence: Option<CoherenceSummary>,
    #[serde(default)]
    pub files_changed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    /// Native execution policy and lineage; `phase3` before 0.4.1.
    #[serde(default, alias = "phase3")]
    pub execution: Option<GoalExecution>,
    pub id: String,
    pub task: String,
    pub exact_prompt: String,
    pub source_path: PathBuf,
    pub source_kind: SourceKind,
    pub source_git_head: Option<String>,
    pub source_fingerprint: String,
    pub baseline_path: PathBuf,
    pub baseline_commit: String,
    pub status: RunStatus,
    #[serde(default)]
    pub mode: RunMode,
    #[serde(default)]
    pub state_revision: u64,
    #[serde(default)]
    pub outcome: RunOutcome,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub environment: EnvironmentRecord,
    #[serde(default)]
    pub baseline_checks: Vec<CheckResult>,
    #[serde(default)]
    pub candidates: Vec<CandidateRecord>,
    #[serde(default)]
    pub attempts: Vec<AttemptRecord>,
    #[serde(default)]
    pub allocation: Option<AllocationDecision>,
    #[serde(default)]
    pub coherence: Option<CoherenceRecord>,
    pub applied_candidate: Option<String>,
    /// Present only for `mode: Attached` work: provenance, capabilities,
    /// process identities and timestamps for external work Dispatch observes
    /// rather than executes. `None` for every run made before this field
    /// existed and for every run Dispatch itself selected and ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<AttachmentRecord>,
    /// Fields written by earlier Dispatch versions and no longer read
    /// (routing, capacity, admission, blind evaluation). Kept verbatim so a
    /// rewrite of an old run's metadata never loses what was recorded.
    #[serde(flatten)]
    pub historical: serde_json::Map<String, serde_json::Value>,
}

/// Provenance, capabilities, process identities and timestamps for a
/// `RunMode::Attached` run: a `RunRecord` that Dispatch observes and can
/// apply, but did not select, route or execute. See
/// `docs/plan-0.3-auto-apply-and-attach.md` part 14.2/14.3.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachmentRecord {
    pub version: u32,
    /// Canonical external worktree the agent runs in.
    pub workspace: PathBuf,
    /// Equals `run.source_path`: the checkout the attachment targets.
    pub integration_root: PathBuf,
    /// `sha256(canonical git-common-dir)`; `None` for a plain directory.
    pub repo_key: Option<String>,
    pub provenance: BaselineProvenance,
    pub confidence: AttachConfidence,
    /// `"claude" | "codex" | "cursor"` or other free text; never guessed.
    pub agent: Option<String>,
    /// Wrapped attach: the argv. Foreign attach: `None`.
    pub command: Option<Vec<String>>,
    /// The wrapper process. `None` for a foreign attachment.
    pub owner: Option<ProcessIdentity>,
    /// Wrapped: the child. Foreign: `--pid` when given.
    pub agent_process: Option<ProcessIdentity>,
    pub owner_state: OwnerState,
    pub capabilities: AttachCapabilities,
    pub attached_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub finish_reason: Option<FinishReason>,
    /// Who made the workspace. Records before 0.4.6 are the user's.
    #[serde(default)]
    pub workspace_owner: WorkspaceOwner,
    /// A workspace Dispatch made for this Work, and whether it is gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed: Option<ManagedWorkspace>,
    /// Agent-runtime sessions that worked here, oldest first
    /// (`runtime::ingest`); the Work is the workspace, not any one session.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<RuntimeSession>,
    /// The workspace is gone. An observation, not an ending: the Work waits
    /// for a person, except an empty Δ, which leaves nothing to review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_removed: Option<WorkspaceRemoval>,
}

/// When the workspace went, and whether Dispatch kept its exact final Δ (a
/// runtime told it first) or only the last Δ it had seen (it found the
/// workspace gone).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceRemoval {
    pub at: DateTime<Utc>,
    pub exact: bool,
}

/// One agent-runtime session in a Work's workspace, as its runtime reported
/// it. `provider` is the runtime (`claude`); the rest is its own words.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeSession {
    pub provider: String,
    pub session_id: String,
    /// How it started: `startup`, `resume`, `fork`, `clear`, `compact`.
    pub source: String,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub end_reason: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

/// Who made the workspace the Work happens in.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceOwner {
    /// You made it (`git worktree add`, or any directory you attached).
    #[default]
    User,
    /// An agent runtime made it (a Claude Code `--worktree` session).
    Runtime,
    /// Dispatch made it (`dispatch attach -- <agent>` from the checkout).
    Dispatch,
}

/// A workspace Dispatch made under `<state>/workspaces/<run-id>`: a linked
/// worktree on `branch` for a Git checkout, a private copy otherwise. It is
/// removed only once its Work is applied.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedWorkspace {
    pub branch: Option<String>,
    pub removed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BaselineProvenance {
    GitMergeBase {
        commit: String,
    },
    SnapshotAtAttach,
    /// The workspace's exact world when its Work began, before any agent edit
    /// (`source::world_commit`): at a runtime's session start, or when
    /// Dispatch made the workspace.
    WorkspaceAtStart {
        commit: String,
    },
}

/// `Partial`: edits made before attach are invisible to Δ and the record
/// says so.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttachConfidence {
    Full,
    Partial,
}

/// `Adopted`: `serve` observes this attachment now (its original owner is
/// not the process observing it).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OwnerState {
    Live,
    Gone,
    Unknown,
    Adopted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachCapabilities {
    pub observe: bool,
    pub signal: bool,
    pub control: bool,
    pub integrate: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    ProcessExit {
        code: Option<i32>,
    },
    Explicit,
    /// Dispatch finished it under the person's consent to run the project's
    /// checks, once its workspace was removed with its exact changes kept.
    ByConsent,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventRecord {
    #[serde(default = "event_protocol_version")]
    pub protocol_version: u32,
    pub run_id: String,
    #[serde(default)]
    pub sequence: u64,
    #[serde(default)]
    pub attempt_id: Option<String>,
    #[serde(default)]
    pub generation: u32,
    #[serde(default = "default_event_actor")]
    pub actor: String,
    pub candidate_label: Option<String>,
    pub event_type: String,
    pub timestamp: DateTime<Utc>,
    pub payload: serde_json::Value,
}

fn event_protocol_version() -> u32 {
    1
}

fn default_event_actor() -> String {
    "legacy".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptRecord {
    #[serde(default)]
    pub detail: AttemptDetail,
    pub id: String,
    pub run_id: String,
    pub candidate_id: String,
    pub role: String,
    pub ordinal: u32,
    pub generation: u32,
    pub harness_id: String,
    pub harness_version: Option<String>,
    pub requested_model: Option<String>,
    pub resolved_model: Option<String>,
    pub observed_model: Option<String>,
    pub requested_effort: Option<String>,
    pub resolved_effort: Option<String>,
    pub observed_effort: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub outcome: String,
    pub raw_telemetry_path: PathBuf,
    #[serde(default)]
    pub resource: Option<ResourceChoice>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResourceTier {
    Light,
    Standard,
    Strong,
}

impl ResourceTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Standard => "standard",
            Self::Strong => "strong",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceChoice {
    pub provider: String,
    pub funding_source: String,
    pub harness: String,
    pub requested_model: String,
    pub resolved_model: String,
    pub effort: Option<String>,
    pub service_mode: String,
    pub runtime: String,
    pub pool: String,
    pub tier: ResourceTier,
    pub no_overage_verified: bool,
    pub internal_composition: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelCapability {
    pub model: String,
    #[serde(default)]
    pub efforts: Vec<String>,
    pub included: bool,
    pub no_overage_verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilitySnapshot {
    pub version: u32,
    pub source: String,
    pub harness: String,
    pub observed_at: DateTime<Utc>,
    pub models: Vec<ModelCapability>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AllocationAlternative {
    pub choice: ResourceChoice,
    pub eligible: bool,
    pub exclusion: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AllocationDecision {
    pub version: u32,
    pub policy_version: String,
    pub selected: ResourceChoice,
    pub reason: String,
    pub capability: CapabilitySnapshot,
    pub alternatives: Vec<AllocationAlternative>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GoalFeedbackRevision {
    pub id: String,
    pub run_id: String,
    pub revision: u32,
    pub outcome: ReviewOutcome,
    #[serde(default)]
    pub reasons: Vec<String>,
    pub explanation: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    #[serde(alias = "phase3")]
    pub execution: Option<GoalExecution>,
    pub elapsed_ms: i64,
    pub schema_version: u32,
    pub run_id: String,
    pub mode: RunMode,
    pub state_revision: u64,
    pub outcome: RunOutcome,
    pub exit_code: i32,
    pub attempts: Vec<AttemptRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allocation: Option<AllocationDecision>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coherence: Option<CoherenceSummary>,
    /// Present only when `--auto-apply` attempted an automatic application.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_apply: Option<AutoApplySummary>,
}

/// Local execution policy and delivery lineage, never part of v1 sync envelopes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalExecution {
    pub max_invocations: u32,
    pub deadline_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_harness: Option<String>,
    pub fixed_model: Option<String>,
    pub fixed_effort: Option<String>,
    pub owner_uid: u32,
    #[serde(default)]
    pub supervisor: Option<crate::process::ProcessIdentity>,
    pub final_attempt_id: Option<String>,
    pub contributing_attempts: Vec<String>,
    pub provenance: String,
    pub failure: Option<FailureKind>,
    pub questions: Vec<Clarification>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AttemptDetail {
    #[serde(default)]
    pub usage_categories: std::collections::BTreeMap<String, u64>,
    pub parent_attempt_id: Option<String>,
    pub reason: Option<String>,
    pub input_baseline: Option<PathBuf>,
    pub decision: Option<AllocationDecision>,
    pub result: Option<CandidateRecord>,
    pub failure: Option<FailureKind>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    TargetVerification,
    VerificationInfrastructure,
    VerificationUnknown,
    HarnessProcess,
    BaselineInfrastructure,
    Cancelled,
    Deadline,
    CapacityAdmission,
    Authorization,
    UnsupportedCheckpoint,
    InternalState,
    SourceDrift,
    InvocationLimit,
    /// The mid-run coherence watcher stopped the agent because the source
    /// moved underneath its work.
    StaleWork,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckpointReport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub version: u32,
    pub question: String,
    #[serde(default)]
    pub choices: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QuestionState {
    Pending,
    Answered,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Clarification {
    pub id: String,
    pub run_id: String,
    pub attempt_id: String,
    pub generation: u32,
    pub revision: u64,
    pub report: CheckpointReport,
    pub state: QuestionState,
    pub answer: Option<String>,
    pub created_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub actor_uid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment() -> AttachmentRecord {
        AttachmentRecord {
            version: 1,
            workspace: PathBuf::from("/repo-worktree"),
            integration_root: PathBuf::from("/repo"),
            repo_key: Some("sha256:deadbeef".to_owned()),
            provenance: BaselineProvenance::GitMergeBase {
                commit: "0123456789abcdef".to_owned(),
            },
            confidence: AttachConfidence::Full,
            agent: Some("claude".to_owned()),
            command: Some(vec!["claude".to_owned(), "--dangerously...".to_owned()]),
            owner: Some(ProcessIdentity {
                pid: 4242,
                start: Some("123456".to_owned()),
                boot: Some("789".to_owned()),
                process_group: Some(4242),
            }),
            agent_process: Some(ProcessIdentity {
                pid: 4243,
                start: None,
                boot: None,
                process_group: None,
            }),
            owner_state: OwnerState::Live,
            capabilities: AttachCapabilities {
                observe: true,
                signal: true,
                control: true,
                integrate: false,
            },
            attached_at: DateTime::parse_from_rfc3339("2026-09-22T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            finished_at: None,
            finish_reason: None,
            workspace_owner: Default::default(),
            managed: None,
            sessions: Vec::new(),
            workspace_removed: None,
        }
    }

    #[test]
    fn attachment_record_round_trips_through_json() {
        let original = attachment();
        let json = serde_json::to_string(&original).unwrap();
        let restored: AttachmentRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, original);
        assert!(json.contains("\"owner_state\":\"live\""), "{json}");
        assert!(
            json.contains("\"provenance\":{\"git_merge_base\":{\"commit\":\"0123456789abcdef\"}}"),
            "{json}"
        );
    }

    #[test]
    fn finish_reason_variants_serialize_as_expected() {
        assert_eq!(
            serde_json::to_string(&FinishReason::ProcessExit { code: Some(0) }).unwrap(),
            r#"{"process_exit":{"code":0}}"#
        );
        assert_eq!(
            serde_json::to_string(&FinishReason::Explicit).unwrap(),
            "\"explicit\""
        );
    }

    /// A `run.json` written before attach existed has no `attachment` key.
    /// Such a run must still deserialize, with `attachment: None`.
    #[test]
    fn old_run_json_without_attachment_deserializes_to_none() {
        let old_run_json = r#"{
            "id":"01TEST", "task":"Fix cache", "exact_prompt":"Fix cache",
            "source_path":"/source", "source_kind":"directory", "source_git_head":null,
            "source_fingerprint":"baseline", "baseline_path":"/baseline", "baseline_commit":"abc",
            "status":"running", "created_at":"2026-09-17T00:00:00Z", "completed_at":null,
            "environment":{"dispatch_version":"test","os":"test","architecture":"test","execution_backend":"local","timeout_secs":30,"cpus":1.0,"memory":"1g","max_parallel":1},
            "evaluation":null,"applied_candidate":null
        }"#;
        let run: RunRecord = serde_json::from_str(old_run_json).unwrap();
        assert!(run.attachment.is_none());
        assert_eq!(run.mode, RunMode::Native);
    }

    #[test]
    fn run_mode_attached_serializes_as_attached() {
        assert_eq!(RunMode::Attached.as_str(), "attached");
        assert_eq!(
            serde_json::to_string(&RunMode::Attached).unwrap(),
            "\"attached\""
        );
        assert_eq!(
            serde_json::from_str::<RunMode>("\"attached\"").unwrap(),
            RunMode::Attached
        );
    }

    /// A run with an attachment round-trips it through `run.json`.
    #[test]
    fn run_record_carries_attachment_through_json() {
        let mut run: RunRecord = serde_json::from_str(
            r#"{
            "id":"01ATTACHED", "task":"attached work in worktree", "exact_prompt":"attached work in worktree",
            "source_path":"/source", "source_kind":"git_worktree", "source_git_head":"abc123",
            "source_fingerprint":"baseline", "baseline_path":"/baseline", "baseline_commit":"abc",
            "status":"running", "mode":"attached", "created_at":"2026-09-22T00:00:00Z", "completed_at":null,
            "environment":{"dispatch_version":"test","os":"test","architecture":"test","execution_backend":"local","timeout_secs":30,"cpus":1.0,"memory":"1g","max_parallel":1},
            "evaluation":null,"applied_candidate":null
        }"#,
        )
        .unwrap();
        assert_eq!(run.mode, RunMode::Attached);
        assert!(run.attachment.is_none());
        run.attachment = Some(attachment());
        let json = serde_json::to_string(&run).unwrap();
        let restored: RunRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.attachment, run.attachment);
    }
}
