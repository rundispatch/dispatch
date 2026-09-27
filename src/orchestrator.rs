mod apply;
pub mod attach;
pub mod background;
pub(crate) mod native;
pub mod serve;
pub use apply::{ApplyAuthority, ApplyOutcome, auto_apply};
pub use native::{QuestionCommand, answer_question, cancel_question};
use std::{
    ffi::OsString,
    fs,
    future::Future,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use ulid::Ulid;

use crate::{
    AllocationDecision, ApplicationState, AppliedBy, AttemptRecord, CandidateRecord,
    CandidateStatus, CheckPhase, CheckStatus, CoherenceRecord, Config, Decision, DiffStats,
    EnvironmentRecord, EventRecord, LifecycleState, ReviewOutcome, ReviewState, RunMode,
    RunOutcome, RunPhase, RunRecord, RunResult, RunStatus, VERSION, VerificationState, WaitingOn,
    WorkResult,
    db::Database,
    executor::{
        CancellationToken, CheckLifecycleEvent, ExecutionStatus, Executor,
        run_checks_with_observer, trusted_host_executable,
    },
    harness::{HarnessRunRequest, adapter_for, build_prompt, probe_version, run_harness},
    lock::{OperationLock, SignalListener},
    source,
    state::{State, write_text},
};

static RUN_OUTPUT_MODE: AtomicU8 = AtomicU8::new(0);

#[cfg(test)]
tokio::task_local! { static CANCEL_AT_HANDOFF: u8; }

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RunOutputMode {
    #[default]
    Human,
    Json,
    Jsonl,
    /// In-process presenter owns all terminal output.
    Silent,
}

impl RunOutputMode {
    fn code(self) -> u8 {
        match self {
            Self::Human => 0,
            Self::Json => 1,
            Self::Jsonl => 2,
            Self::Silent => 3,
        }
    }
}

/// Scoped to one foreground operation; latest committed projection replaces the
/// previous one instead of growing a render queue. Policy remains in the core.
pub struct Presentation {
    pub updates: tokio::sync::watch::Sender<Option<(EventRecord, RunRecord)>>,
    pub cancellation: CancellationToken,
}
tokio::task_local! { static PRESENTATION: Presentation; }

pub async fn present<F: Future>(presentation: Presentation, work: F) -> F::Output {
    PRESENTATION.scope(presentation, work).await
}

fn operation_cancellation() -> CancellationToken {
    PRESENTATION
        .try_with(|p| p.cancellation.clone())
        .unwrap_or_default()
}

const EVALUATION_REASONS: &[&str] = &[
    "rework",
    "changed-requirements",
    "source-drift",
    "correctness",
    "completeness",
    "architecture",
    "maintainability",
    "readability",
    "tests",
    "edge-cases",
    "performance",
    "cost",
    "latency",
    "other",
];

pub struct RunRequest {
    pub source: PathBuf,
    pub task: String,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub config_path: Option<PathBuf>,
    pub backend: Option<String>,
    pub timeout_secs: Option<u64>,
    pub allow_unsafe_local: bool,
    pub allow_forwarded_env: bool,
    pub output: RunOutputMode,
    /// The run this one redoes on the current source (`dispatch refresh`).
    pub refreshed_from: Option<String>,
}

pub fn init(state: &State, path: &Path, force: bool) -> Result<()> {
    let project = if path.exists() {
        path.canonicalize()
            .with_context(|| format!("failed to resolve {}", path.display()))?
    } else {
        fs::create_dir_all(path)?;
        path.canonicalize()?
    };
    let projected_state_root = canonicalize_allow_missing(&state.root)?;
    anyhow::ensure!(
        !projected_state_root.starts_with(&project),
        "Dispatch state directory must be outside the project tree: {}",
        state.root.display()
    );
    let config_path = project.join("dispatch.yml");
    write_project_config(&config_path, Config::example_yaml(), force)?;
    state.initialize()?;
    Database::open(state.db_path())?;
    println!("Created {}", config_path.display());
    println!("State directory: {}", state.root.display());
    Ok(())
}

fn write_project_config(path: &Path, contents: &str, force: bool) -> Result<()> {
    let existing = match fs::symlink_metadata(path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    if let Some(metadata) = &existing {
        anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "refusing to write configuration through symlink {}",
            path.display()
        );
        anyhow::ensure!(
            metadata.is_file(),
            "{} is not a regular file",
            path.display()
        );
        if !force {
            bail!(
                "{} already exists (use --force to replace it)",
                path.display()
            );
        }
    }

    if existing.is_none() {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        return Ok(());
    }

    let parent = path.parent().context("configuration path has no parent")?;
    let temp = parent.join(format!(".dispatch-config-{}.tmp", Ulid::new()));
    let replacement = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp, path).with_context(|| format!("failed to replace {}", path.display()))?;
        Ok(())
    })();
    if replacement.is_err() {
        let _ = fs::remove_file(&temp);
    }
    replacement
}

pub async fn doctor(state: &State, source_path: &Path, config_path: Option<&Path>) -> Result<()> {
    let source = source::resolve_source(Some(source_path))?;
    let projected_state_root = canonicalize_allow_missing(&state.root)?;
    anyhow::ensure!(
        !projected_state_root.starts_with(&source),
        "Dispatch state directory must be outside the source tree: {}",
        state.root.display()
    );
    let (config, discovered) = Config::discover(&source, config_path)?;
    config.validate()?;
    state.initialize()?;
    let db = Database::open(state.db_path())?;
    db.health_check()?;

    println!("Dispatch doctor\n");
    print_detection("Git", &command_version("git", &["--version"]));
    print_detection("Docker", &command_version("docker", &["--version"]));
    println!(
        "{:<17} ✓ bundled (schema {})",
        "SQLite",
        db.schema_version()?
    );
    println!();

    let harnesses = [
        ("Claude Code", "claude"),
        ("Codex", "codex"),
        ("Cursor", "cursor"),
    ];
    let mut ready = 0usize;
    for (label, harness_id) in harnesses {
        let adapter = adapter_for(harness_id, &config.harnesses)?;
        let detection = adapter.detect().await;
        let detail = if detection.available {
            ready += 1;
            let custom = match harness_id {
                "claude" => config.harnesses.claude.executable.is_some(),
                "codex" => config.harnesses.codex.executable.is_some(),
                "cursor" => config.harnesses.cursor.executable.is_some(),
                _ => false,
            };
            match detection.executable {
                Some(path) if !custom && !path.starts_with(&source) => probe_version(&path)
                    .await
                    .ok()
                    .flatten()
                    .or_else(|| Some(path.display().to_string())),
                Some(path) => Some(format!(
                    "{} (version probe skipped for untrusted path)",
                    path.display()
                )),
                None => None,
            }
        } else {
            None
        };
        print_detection(label, &detail);
    }
    println!("{:<17} ✓ built in", "Fake harnesses");

    let (kind, head) = source::inspect_source(&source)?;
    println!();
    println!("{:<17} {}", "Source", source.display());
    println!("{:<17} {}", "Source type", kind.as_str());
    if let Some(head) = head {
        println!("{:<17} {}", "Git HEAD", head);
    }
    println!(
        "{:<17} {}",
        "Configuration",
        discovered
            .as_ref()
            .map_or_else(|| "defaults".into(), |p| p.display().to_string())
    );
    println!(
        "{:<17} {} baseline / {} verify",
        "Checks",
        config.checks.baseline.len(),
        config.checks.verify.len()
    );
    println!("{:<17} {}", "Backend", config.execution.backend);
    println!("{:<17} ✓ ready", "Snapshotting");
    println!(
        "{:<17} {}",
        "Verification",
        if config.checks.verify.is_empty() {
            "not configured"
        } else {
            "configured"
        }
    );
    println!();
    if config.execution.backend == "local" {
        println!(
            "Found {ready} installed real harness(es). Real harnesses and project checks are blocked on the host unless each run explicitly uses --allow-unsafe-local."
        );
        println!("Deterministic fake harnesses are ready without that opt-in.");
    } else {
        println!(
            "Docker isolation is selected. Docker daemon access, image contents, harness versions, and authentication are not validated by doctor; run with a harness-enabled image. Deterministic fakes are ready."
        );
    }
    Ok(())
}

fn print_allocation_decision(decision: &AllocationDecision) {
    println!("Profile");
    println!(
        "  {} · {} · {} effort",
        decision.selected.harness,
        decision.selected.requested_model,
        decision.selected.effort.as_deref().unwrap_or("default")
    );
    println!("Why:\n  {}\n", decision.reason);
}

/// The one selection rule: the first configured profile, in file order, that
/// is eligible, matches the backend and any explicit model or effort, and was
/// not excluded by `select_available_resource`'s availability, preflight and
/// funding checks. The decision records every profile and why it was not
/// chosen.
fn choose_profile(
    resources: &crate::config::ResourceConfig,
    backend: &str,
    model: Option<&str>,
    effort: Option<&str>,
    exclusions: &[Option<String>],
) -> Result<crate::AllocationDecision> {
    let alternatives = resources
        .profiles
        .iter()
        .enumerate()
        .map(|(index, profile)| {
            let exclusion = if let Some(reason) = exclusions.get(index).and_then(Option::as_ref) {
                Some(reason.clone())
            } else if let Err(error) = profile.eligibility() {
                Some(error.to_string())
            } else if profile.runtime != backend {
                Some(format!(
                    "profile runtime does not match the {backend} execution backend"
                ))
            } else if model.is_some_and(|model| profile.model != model) {
                Some("model does not satisfy the explicit constraint".to_owned())
            } else if effort.is_some_and(|effort| profile.effort.as_deref() != Some(effort)) {
                Some("effort does not satisfy the explicit constraint".to_owned())
            } else {
                None
            };
            crate::AllocationAlternative {
                choice: crate::ResourceChoice {
                    provider: profile.provider.clone(),
                    funding_source: profile.funding_source.clone(),
                    harness: profile.harness.clone(),
                    requested_model: profile.model.clone(),
                    resolved_model: profile.model.clone(),
                    effort: profile.effort.clone(),
                    service_mode: profile.service_mode.clone(),
                    runtime: profile.runtime.clone(),
                    pool: profile.pool.clone(),
                    tier: profile.tier.clone(),
                    no_overage_verified: profile.no_overage_verified,
                    internal_composition: "unknown".into(),
                },
                eligible: exclusion.is_none(),
                exclusion,
            }
        })
        .collect::<Vec<_>>();
    let selected = alternatives
        .iter()
        .find(|alternative| alternative.eligible)
        .map(|alternative| alternative.choice.clone())
        .with_context(|| match (model, effort) {
            (None, None) => {
                "no included, no-overage-verified resource profile is available".to_owned()
            }
            _ => format!(
                "no included, no-overage-verified resource profile matches{}{}",
                model.map(|m| format!(" model {m:?}")).unwrap_or_default(),
                effort.map(|e| format!(" effort {e:?}")).unwrap_or_default()
            ),
        })?;
    let capability = crate::CapabilitySnapshot {
        version: 1,
        source: "user_validated_profile".into(),
        harness: selected.harness.clone(),
        observed_at: Utc::now(),
        models: resources
            .profiles
            .iter()
            .filter(|profile| profile.harness == selected.harness)
            .map(|profile| crate::ModelCapability {
                model: profile.model.clone(),
                efforts: profile.effort.iter().cloned().collect(),
                included: profile.included,
                no_overage_verified: profile.no_overage_verified,
            })
            .collect(),
    };
    let reason = if model.is_some() || effort.is_some() {
        "The explicit model or effort constraint selected this profile."
    } else {
        "The first available configured profile was selected."
    };
    Ok(crate::AllocationDecision {
        version: 1,
        policy_version: "first-available-profile-v1".into(),
        selected,
        reason: reason.into(),
        capability,
        alternatives,
    })
}

/// Why a profile whose funding was refused cannot launch until setup
/// re-authorizes it (a new `authorization_revision`).
fn funding_refused_message(profile: &crate::config::ResourceProfile, reason: &str) -> String {
    format!(
        "funding authorization revision {} of this {} profile was refused ({reason}); run `dispatch setup {}` to revalidate",
        profile.authorization_revision, profile.harness, profile.harness
    )
}

fn harness_name(id: &str) -> &str {
    match id {
        "claude" => "Claude Code",
        "codex" => "Codex",
        "cursor" => "Cursor",
        _ => id,
    }
}

fn authorize_local_commands(label: &str, output: RunOutputMode) -> Result<()> {
    anyhow::ensure!(
        output == RunOutputMode::Human,
        "machine output requires --allow-unsafe-local when local execution needs authorization"
    );
    print!(
        "Dispatch will run {label} with your user permissions in an isolated candidate workspace. Continue? [y/N] "
    );
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    anyhow::ensure!(
        matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        "local execution was not authorized; explicitly accept the risk with --allow-unsafe-local for non-interactive use, or use --backend docker"
    );
    Ok(())
}

pub async fn run_dispatch(state: &State, request: RunRequest) -> Result<RunRecord> {
    RUN_OUTPUT_MODE.store(request.output.code(), Ordering::Relaxed);
    anyhow::ensure!(!request.task.trim().is_empty(), "task must not be empty");
    let source_path = source::resolve_source(Some(&request.source))?;
    let projected_state_root = canonicalize_allow_missing(&state.root)?;
    anyhow::ensure!(
        !projected_state_root.starts_with(&source_path),
        "Dispatch state directory must be outside the source tree: {}",
        state.root.display()
    );
    let (mut config, config_path) = Config::discover(&source_path, request.config_path.as_deref())?;
    if let Some(backend) = request.backend {
        config.execution.backend = backend;
    }
    if let Some(timeout) = request.timeout_secs {
        config.execution.timeout_secs = timeout;
    }
    config.validate()?;
    crate::config::validate_model(request.model.as_deref())?;
    crate::config::validate_effort(request.effort.as_deref())?;
    anyhow::ensure!(
        config.execution.forwarded_env.is_empty() || request.allow_forwarded_env,
        "configuration requests forwarding environment variables ({names}); review them and re-run with --allow-forwarded-env",
        names = config.execution.forwarded_env.join(", ")
    );

    let resources = crate::config::ResourceConfig::load(&state.root)?;
    let allocation_requested =
        resources.allocation_enabled || request.model.is_some() || request.effort.is_some();
    anyhow::ensure!(
        allocation_requested || request.agent.is_some(),
        "no coding agent is set up for Dispatch to launch: run `dispatch setup`, or pass --agent claude|codex|cursor. To protect work you run yourself, use `dispatch attach` (no setup needed)."
    );
    let fixed_harness = request.agent.clone();
    let mut local_authorized = request.allow_unsafe_local;
    if allocation_requested && config.execution.backend == "local" && !local_authorized {
        // Discovery executes configured programs too: obtain host consent before probes.
        authorize_local_commands("configured coding-agent commands", request.output)?;
        local_authorized = true;
    }
    // One agent per run, chosen by a configured profile or by `--agent`.
    let (harness, allocation) = if allocation_requested {
        let decision = select_available_resource(
            state,
            &source_path,
            &resources,
            &config,
            request.agent.as_deref(),
            request.model.as_deref(),
            request.effort.as_deref(),
        )
        .await?;
        (decision.selected.harness.clone(), Some(decision))
    } else {
        (
            request
                .agent
                .clone()
                .context("no coding agent was chosen")?,
            None,
        )
    };
    let fixed_config = config.harnesses.get(&harness);
    let fixed_model = request.model.clone().or(fixed_config.model.clone());
    let fixed_effort = request.effort.clone().or(fixed_config.effort.clone());
    if let Some(decision) = &allocation {
        config.harnesses.bind(&decision.selected, &resources);
    }
    adapter_for(&harness, &config.harnesses)?;
    let executes_untrusted_host_code = !harness.starts_with("fake-")
        || !config.checks.baseline.is_empty()
        || !config.checks.verify.is_empty();
    let unsafe_local = config.execution.backend == "local" && executes_untrusted_host_code;
    if unsafe_local && !local_authorized {
        authorize_local_commands(harness_name(&harness), request.output)?;
    }

    if request.output == RunOutputMode::Human
        && let Some(decision) = &allocation
    {
        print_allocation_decision(decision);
    }

    state.initialize()?;
    let mut db = Database::open(state.db_path())?;
    let run_id = Ulid::new().to_string();
    let run_dir = state.run_dir(&run_id);
    fs::create_dir(&run_dir)
        .with_context(|| format!("failed to create run directory {}", run_dir.display()))?;
    let _run_lock = OperationLock::acquire(
        &run_dir.join(".operation.lock"),
        "run is already supervised",
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o700))?;
    }
    write_text(&run_dir.join("task.md"), &request.task)?;
    write_text(
        &run_dir.join("config.snapshot.yml"),
        &serde_yaml::to_string(&config).context("failed to serialize effective configuration")?,
    )?;
    let cancellation = operation_cancellation();
    let _signal_listener = SignalListener::install(cancellation.clone());

    if request.output == RunOutputMode::Human {
        println!("RUN {run_id}\n");
        println!("Creating baseline...");
    }
    let snapshot = match source::create_snapshot(&source_path, &run_dir) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let _ = fs::remove_dir_all(&run_dir);
            return Err(error.context("baseline creation failed; incomplete run state was removed"));
        }
    };
    if request.output == RunOutputMode::Human {
        println!("Creating baseline... done ({})", snapshot.baseline_commit);
    }

    let exact_prompt = build_prompt(&request.task);
    let now = Utc::now();
    let mut run = RunRecord {
        execution: None,
        id: run_id.clone(),
        task: request.task,
        exact_prompt: exact_prompt.clone(),
        source_path,
        source_kind: snapshot.kind,
        source_git_head: snapshot.git_head,
        source_fingerprint: snapshot.fingerprint,
        baseline_path: snapshot.baseline_path,
        baseline_commit: snapshot.baseline_commit,
        status: RunStatus::Preparing,
        mode: RunMode::Native,
        state_revision: 0,
        outcome: RunOutcome {
            lifecycle: LifecycleState::Preparing,
            work_result: WorkResult::Pending,
            verification: if config.checks.verify.is_empty() {
                VerificationState::NotConfigured
            } else {
                VerificationState::NotRun
            },
            review: ReviewState::NotRequested,
            application: ApplicationState::NotApplied,
            phase: RunPhase::Preparing,
            waiting_on: WaitingOn::None,
            ..RunOutcome::default()
        },
        created_at: now,
        completed_at: None,
        environment: EnvironmentRecord {
            dispatch_version: VERSION.into(),
            os: std::env::consts::OS.into(),
            architecture: std::env::consts::ARCH.into(),
            execution_backend: config.execution.backend.clone(),
            timeout_secs: config.execution.timeout_secs,
            cpus: config.execution.cpus,
            memory: config.execution.memory.clone(),
            max_parallel: 1,
            docker_image: (config.execution.backend == "docker")
                .then(|| config.execution.docker_image.clone()),
            resource_limits_enforced: config.execution.backend == "docker",
            unsafe_local,
            forwarded_env: config.execution.forwarded_env.clone(),
        },
        baseline_checks: Vec::new(),
        candidates: Vec::new(),
        attempts: Vec::new(),
        allocation,
        coherence: request.refreshed_from.map(|old| CoherenceRecord {
            version: 1,
            refreshed_from: Some(old),
            validity: None,
            first_invalid_at: None,
            overridden: None,
        }),
        applied_candidate: None,
        // Native runs never carry attachment provenance; only `dispatch attach`
        // (S3) sets this field.
        attachment: None,
        historical: Default::default(),
    };
    {
        run.execution = Some(crate::GoalExecution {
            max_invocations: 2,
            deadline_at: run.created_at
                + chrono::TimeDelta::seconds(
                    i64::try_from(config.execution.timeout_secs).unwrap_or(i64::MAX),
                ),
            fixed_harness,
            fixed_model,
            fixed_effort,
            owner_uid: native::local_uid(),
            supervisor: Some(crate::process::ProcessIdentity::current()),
            final_attempt_id: None,
            contributing_attempts: Vec::new(),
            provenance: "single_attempt".into(),
            failure: None,
            questions: Vec::new(),
        });
    }
    let _deadline = native::DeadlineGuard::new(run.execution.as_ref(), cancellation.clone());
    let created_event = EventRecord {
        run_id: run.id.clone(),
        candidate_label: None,
        event_type: "run.created".into(),
        timestamp: now,
        payload: serde_json::json!({
            "source": run.source_path,
            "source_kind": run.source_kind.as_str(),
            "config": config_path,
        }),
        ..EventRecord::default()
    };
    let created = db.commit_run_transition(&mut run, created_event)?;
    publish_event(state, created, &run)?;

    persist_event(
        state,
        &db,
        EventRecord {
            run_id: run.id.clone(),
            candidate_label: None,
            event_type: "baseline.started".into(),
            timestamp: Utc::now(),
            payload: serde_json::json!({"commit": run.baseline_commit}),
            ..EventRecord::default()
        },
        &mut run,
    )?;

    let mut baseline_passed = true;
    let baseline_commands = if config.checks.baseline.is_empty() {
        &config.checks.verify
    } else {
        &config.checks.baseline
    };
    if !baseline_commands.is_empty() {
        if request.output == RunOutputMode::Human {
            println!("Running baseline checks...");
        }
        let baseline_check_dir = run_dir.join("baseline-check");
        let baseline_check_workspace = match source::create_candidate_workspace(
            &run.baseline_path,
            &baseline_check_dir.join("workspace"),
        ) {
            Ok(workspace) => workspace,
            Err(error) => {
                let message = format!("failed to prepare baseline checks: {error:#}");
                finish_failed(state, &mut db, &mut run, &message)?;
                bail!(message);
            }
        };
        let (check_tx, mut check_rx) = tokio::sync::mpsc::unbounded_channel();
        let check_output_dir = run_dir.join("checks/baseline");
        let baseline_future = run_checks_with_observer(
            &baseline_check_workspace,
            baseline_commands,
            CheckPhase::Baseline,
            &check_output_dir,
            config.execution.clone(),
            cancellation.clone(),
            Some(check_tx),
            None,
        );
        tokio::pin!(baseline_future);
        run.baseline_checks = loop {
            tokio::select! {
                results = &mut baseline_future => break results,
                Some(event) = check_rx.recv() => persist_check_lifecycle(state, &db, &mut run, event)?,
            }
        };
        while let Ok(event) = check_rx.try_recv() {
            persist_check_lifecycle(state, &db, &mut run, event)?;
        }
        baseline_passed = run
            .baseline_checks
            .iter()
            .all(|check| check.status == CheckStatus::Passed);
        if request.output == RunOutputMode::Human {
            println!(
                "Running baseline checks... {}",
                if baseline_passed { "pass" } else { "FAIL" }
            );
        }
        db.sync_run(&run)?;
    }
    if cancellation.is_cancelled() {
        if run.execution.is_some() {
            let failure = native::interrupted(&run);
            native::stop(
                state,
                &mut db,
                &mut run,
                failure,
                "goal interrupted during baseline checks",
            )?;
            native::emit(&run, request.output)?;
            return Ok(run);
        }
        finish_interrupted(state, &mut db, &mut run)?;
        bail!(
            "run {} interrupted; the frozen baseline was preserved",
            run.id
        );
    }
    persist_event(
        state,
        &db,
        EventRecord {
            run_id: run.id.clone(),
            candidate_label: None,
            event_type: "baseline.finished".into(),
            timestamp: Utc::now(),
            payload: serde_json::json!({
                "commit": run.baseline_commit,
                "checks_passed": baseline_passed,
            }),
            ..EventRecord::default()
        },
        &mut run,
    )?;

    native::drive(
        state,
        &mut db,
        run,
        config,
        resources,
        cancellation,
        request.output,
    )
    .await
}

fn result_verification(candidate: &CandidateRecord) -> &'static str {
    if candidate.checks.is_empty() {
        if candidate.status == CandidateStatus::TimedOut {
            "Not run — agent timed out"
        } else if candidate.status != CandidateStatus::Completed {
            "Not run — agent failed"
        } else {
            "Not configured"
        }
    } else if candidate
        .checks
        .iter()
        .all(|check| check.status == CheckStatus::Passed)
    {
        "PASS"
    } else if candidate
        .checks
        .iter()
        .any(|check| check.status == CheckStatus::Failed)
    {
        "FAIL"
    } else if candidate
        .checks
        .iter()
        .any(|check| check.status == CheckStatus::TimedOut)
    {
        "TIMED_OUT"
    } else {
        "NOT_RUN"
    }
}

fn print_single_result_summary(
    run: &RunRecord,
    heading: &str,
    show_status: bool,
    human_outcome: Option<&ReviewOutcome>,
) {
    let candidate = run
        .candidates
        .first()
        .expect("a completed selected run has one candidate");
    println!("\n{heading}\n");
    println!("Task\n  {}\n", one_line(&run.task, 120));
    if let Some(decision) = run
        .attempts
        .last()
        .and_then(|a| a.detail.decision.as_ref())
        .or(run.allocation.as_ref())
    {
        let selected = &decision.selected;
        println!("Agent\n  {}", harness_name(&selected.harness));
        println!(
            "  Profile: {} · {} · {}\n",
            selected.harness,
            selected.requested_model,
            selected.effort.as_deref().unwrap_or("default effort")
        );
    } else if let Some(candidate) = run.candidates.first() {
        println!("Agent\n  {}", harness_name(&candidate.harness_id));
        println!("  Chosen with --agent (no profile)\n");
    }
    let validity = run.coherence.as_ref().and_then(|c| c.validity.as_ref());
    let line = work_line(run, validity);
    println!(
        "Work\n  {} {} · began against {} at {}\n",
        line.origin,
        line.agent,
        line.s0,
        run.created_at.format("%Y-%m-%d %H:%M UTC")
    );
    if let Some(attempt) = run.attempts.last()
        && (attempt.requested_model.is_some() || attempt.observed_model.is_some())
    {
        println!(
            "Model\n  requested: {}\n  observed: {}\n",
            attempt.requested_model.as_deref().unwrap_or("unknown"),
            attempt.observed_model.as_deref().unwrap_or("unknown")
        );
    }
    println!("Verification\n  {}\n", result_verification(candidate));
    if let Some(line) = coherence_line(run) {
        println!("{line}\n");
    }
    if show_status {
        println!("Status\n  {}\n", line.state);
    }
    if run.status == RunStatus::Applied {
        println!("Result\n  Applied to the source tree");
    } else if let Some(outcome) = human_outcome {
        match outcome {
            ReviewOutcome::Accepted => println!("Result\n  Accepted; not applied"),
            ReviewOutcome::Rejected => {
                println!("Result\n  Rejected; source tree unchanged")
            }
        }
    } else {
        println!("Review\n  dispatch diff\n");
        println!("Then\n  dispatch accept\n  dispatch reject");
    }
}

struct CandidateExecution {
    usage_categories: std::collections::BTreeMap<String, u64>,
    checkpoint: Option<std::result::Result<crate::CheckpointReport, String>>,
    cleanup_confirmed: bool,
    failure: Option<crate::FailureKind>,
    /// The adapter preflight refused the launch; nothing was spawned.
    preflight_refusal: Option<String>,
    candidate: CandidateRecord,
    requested_model: Option<String>,
    resolved_model: Option<String>,
    observed_model: Option<String>,
    requested_effort: Option<String>,
    resolved_effort: Option<String>,
    observed_effort: Option<String>,
}

/// Resolve optional resources before binding a request or reserving any slot.
/// Profile order is the explicit portfolio preference; capacity is never summed.
#[allow(clippy::too_many_arguments)]
async fn select_available_resource(
    state: &State,
    source: &Path,
    resources: &crate::config::ResourceConfig,
    config: &Config,
    agent: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<crate::AllocationDecision> {
    let mut exclusions = Vec::new();
    let db = Database::open(state.db_path())?;
    for profile in &resources.profiles {
        let harness = config.harnesses.get(&profile.harness);
        if profile.enabled && agent.is_none_or(|a| a == profile.harness) {
            anyhow::ensure!(
                harness.extra_args.is_empty(),
                "allocation profiles require harnesses.{}.extra_args to be empty so controlled invocation cannot be shadowed",
                profile.harness
            );
        }
        let reason = if !profile.enabled {
            Some("resource disabled".into())
        } else if agent.is_some_and(|a| a != profile.harness) {
            Some("explicit harness constraint".into())
        } else if harness.model.as_deref().is_some_and(|m| m != profile.model)
            || harness
                .effort
                .as_deref()
                .is_some_and(|e| Some(e) != profile.effort.as_deref())
        {
            Some("resource conflicts with fixed project harness configuration".into())
        } else if model.is_some_and(|m| m != profile.model)
            || effort.is_some_and(|e| Some(e) != profile.effort.as_deref())
        {
            Some("explicit model or effort constraint".into())
        } else {
            let adapter = adapter_for(&profile.harness, &config.harnesses)?;
            if config.execution.backend == "local" && !adapter.detect().await.available {
                Some(format!("{} executable unavailable", profile.harness))
            } else if let Err(error) = profile.eligibility() {
                Some(error.to_string())
            } else if let Some(refusal) =
                db.funding_refusal(&profile.funding_key(), profile.authorization_revision)?
            {
                Some(funding_refused_message(profile, &refusal))
            } else {
                let mut harnesses = config.harnesses.clone();
                harnesses.bind_profile(profile);
                let adapter = adapter_for(&profile.harness, &harnesses)?;
                let request = HarnessRunRequest::new(source, "", source);
                match adapter
                    .preflight(&Executor::new(config.execution.clone()), &request)
                    .await
                {
                    Ok(()) => None,
                    Err(error) => {
                        db.record_funding_refusal(
                            &profile.funding_key(),
                            profile.authorization_revision,
                            &format!("{error:#}"),
                        )?;
                        Some(error.to_string())
                    }
                }
            }
        };
        exclusions.push(reason);
    }
    choose_profile(
        resources,
        &config.execution.backend,
        model,
        effort,
        &exclusions,
    )
    .map_err(|e| {
        anyhow::anyhow!(
            "{e}; {}",
            exclusions
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("; ")
        )
    })
}

/// Where and for which run an attempt's launch is recorded, and the profile
/// re-checked at the launch boundary.
pub(super) struct LaunchContext {
    pub db_path: PathBuf,
    pub run_id: String,
    pub guard: Option<crate::launch::LaunchGuard>,
}

async fn execute_candidate(
    mut candidate: CandidateRecord,
    prompt: String,
    config: Config,
    baseline_path: PathBuf,
    cancellation: CancellationToken,
    verification: (
        tokio::sync::mpsc::UnboundedSender<CheckLifecycleEvent>,
        String,
    ),
    launch: Option<LaunchContext>,
) -> CandidateExecution {
    let (check_observer, attempt_id) = verification;
    let candidate_dir = candidate
        .prompt_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| candidate.workspace_path.clone());
    let timeout = Duration::from_secs(config.execution.timeout_secs);

    // The native engine records its launches and may deliver a clarification.
    let native = launch.is_some();
    let execution = async {
        let adapter = adapter_for(&candidate.harness_id, &config.harnesses)?;
        let executor = Executor::new(config.execution.clone());
        let mut request = HarnessRunRequest::new(&candidate.workspace_path, prompt, &candidate_dir)
            .with_timeout(timeout)
            .with_cancellation(cancellation.clone());
        request.read_only = config.harnesses.get(&candidate.harness_id).read_only;
        if let Some(launch) = launch {
            request = request.with_observer(Arc::new(crate::launch::LaunchObserver::new(
                &launch.db_path,
                &launch.run_id,
                &attempt_id,
                launch.guard,
                None,
            )));
        }
        run_harness(adapter.as_ref(), &executor, request).await
    }
    .await;
    // Work may be verified or delivered only once the agent is known stopped.
    let cleanup_confirmed = execution
        .as_ref()
        .is_ok_and(|result| result.execution.cleanup_confirmed)
        || execution.is_err();

    let mut checkpoint = None;
    let mut usage_categories = std::collections::BTreeMap::new();
    let mut failure;
    let mut preflight_refusal = None;
    let mut identities = (None, None, None, None, None, None);
    match execution {
        Ok(result) => {
            usage_categories = result.usage_categories;
            failure = result.failure;
            if native && result.execution.status == ExecutionStatus::Succeeded && cleanup_confirmed
            {
                checkpoint = result.checkpoint;
            }
            let _ = write_text(
                &candidate_dir.join("harness.jsonl"),
                &result.execution.stdout,
            );
            candidate.harness_version = result.harness_version;
            candidate.model = result.observed_model.clone();
            identities = (
                result.requested_model,
                result.resolved_model,
                result.observed_model,
                result.requested_effort,
                result.resolved_effort,
                result.observed_effort,
            );
            candidate.duration_ms = result.execution.duration_ms;
            candidate.exit_code = result.execution.exit_code;
            candidate.timed_out = result.execution.timed_out;
            candidate.tokens = result.tokens;
            candidate.token_semantics = result.token_semantics;
            candidate.cost_usd = result.cost_usd;
            candidate.error = result.execution.error;
            candidate.stdout_path = result.execution.stdout_path;
            candidate.stderr_path = result.execution.stderr_path;
            candidate.status = match result.execution.status {
                ExecutionStatus::Succeeded => CandidateStatus::Verifying,
                ExecutionStatus::TimedOut => CandidateStatus::TimedOut,
                ExecutionStatus::Cancelled => CandidateStatus::Cancelled,
                ExecutionStatus::Failed | ExecutionStatus::SpawnFailed => CandidateStatus::Failed,
            };

            if candidate.status == CandidateStatus::Verifying
                && cleanup_confirmed
                && checkpoint.is_some()
            {
                candidate.status = CandidateStatus::Completed;
            } else if candidate.status == CandidateStatus::Verifying && cleanup_confirmed {
                candidate.checks = run_checks_with_observer(
                    &candidate.workspace_path,
                    &config.checks.verify,
                    CheckPhase::Verify,
                    &candidate_dir.join("checks"),
                    config.execution.clone(),
                    cancellation.clone(),
                    Some(check_observer),
                    Some(attempt_id),
                )
                .await;
                candidate.status = if cancellation.is_cancelled() {
                    CandidateStatus::Cancelled
                } else {
                    CandidateStatus::Completed
                };
            } else if candidate.status == CandidateStatus::Verifying {
                candidate.status = CandidateStatus::Failed;
                append_candidate_error(
                    &mut candidate,
                    "model process cleanup could not be confirmed; its work is not delivered"
                        .into(),
                );
            }
        }
        Err(error) => {
            preflight_refusal = error
                .downcast_ref::<crate::harness::PreflightRefused>()
                .map(|refusal| refusal.0.clone());
            failure = Some(if error.downcast_ref::<rusqlite::Error>().is_some() {
                crate::FailureKind::InternalState
            } else if preflight_refusal.is_some()
                || error
                    .downcast_ref::<crate::launch::LaunchRefused>()
                    .is_some()
                || error.to_string().contains("funding revalidation")
            {
                crate::FailureKind::Authorization
            } else {
                crate::FailureKind::HarnessProcess
            });
            let message = format!("{error:#}");
            candidate.status = if message.contains(" is unavailable:") {
                CandidateStatus::MissingHarness
            } else {
                CandidateStatus::Failed
            };
            candidate.error = Some(message.clone());
            let _ = write_text(&candidate.stdout_path, "");
            let _ = write_text(&candidate.stderr_path, &format!("{message}\n"));
            let _ = write_text(&candidate_dir.join("harness.jsonl"), "");
        }
    }

    match source::collect_diff(
        &baseline_path,
        &candidate.workspace_path,
        &candidate.diff_path,
    ) {
        Ok(stats) => candidate.diff_stats = stats,
        Err(error) => {
            failure = Some(crate::FailureKind::InternalState);
            append_candidate_error(&mut candidate, format!("diff collection failed: {error:#}"));
            if candidate.status == CandidateStatus::Completed {
                candidate.status = CandidateStatus::Failed;
            }
            let _ = write_text(&candidate.diff_path, "");
        }
    }
    CandidateExecution {
        usage_categories,
        checkpoint,
        cleanup_confirmed,
        preflight_refusal,
        failure,
        candidate,
        requested_model: identities.0,
        resolved_model: identities.1,
        observed_model: identities.2,
        requested_effort: identities.3,
        resolved_effort: identities.4,
        observed_effort: identities.5,
    }
}

fn update_attempt_started(run: &mut RunRecord, candidate_id: &str) -> String {
    let attempt = run
        .attempts
        .iter_mut()
        .find(|attempt| attempt.candidate_id == candidate_id)
        .expect("candidate attempt exists");
    attempt.started_at = Utc::now();
    attempt.outcome = "running".into();
    attempt.id.clone()
}

fn update_attempt_finished(
    run: &mut RunRecord,
    candidate: &CandidateRecord,
    execution: &CandidateExecution,
) -> String {
    let attempt = run
        .attempts
        .iter_mut()
        .find(|attempt| attempt.candidate_id == candidate.id)
        .expect("candidate attempt exists");
    attempt.detail.usage_categories = execution.usage_categories.clone();
    attempt.harness_version = candidate.harness_version.clone();
    attempt.requested_model = execution
        .requested_model
        .clone()
        .or_else(|| attempt.requested_model.clone());
    attempt.resolved_model = execution
        .resolved_model
        .clone()
        .or_else(|| attempt.resolved_model.clone());
    attempt.observed_model = execution.observed_model.clone();
    attempt.requested_effort = execution.requested_effort.clone();
    attempt.resolved_effort = execution.resolved_effort.clone();
    attempt.observed_effort = execution.observed_effort.clone();
    attempt.completed_at = Some(Utc::now());
    attempt.outcome = candidate.status.as_str().into();
    attempt.id.clone()
}

fn append_candidate_error(candidate: &mut CandidateRecord, error: String) {
    match &mut candidate.error {
        Some(existing) => {
            existing.push_str("; ");
            existing.push_str(&error);
        }
        None => candidate.error = Some(error),
    }
}

fn refresh_outcome(run: &mut RunRecord) {
    run.outcome.lifecycle = LifecycleState::Finished;
    run.outcome.work_result = if run
        .candidates
        .iter()
        .any(|candidate| candidate.status == CandidateStatus::Completed)
    {
        WorkResult::Ready
    } else {
        WorkResult::Failed
    };
    let checks = run
        .candidates
        .iter()
        .flat_map(|candidate| &candidate.checks)
        .collect::<Vec<_>>();
    run.outcome.verification = if checks.is_empty() {
        if run.outcome.work_result == WorkResult::Ready {
            VerificationState::NotConfigured
        } else {
            VerificationState::NotRun
        }
    } else if checks
        .iter()
        .all(|check| check.status == CheckStatus::Passed)
    {
        VerificationState::Passed
    } else if checks
        .iter()
        .any(|check| check.status == CheckStatus::Failed)
    {
        VerificationState::Failed
    } else {
        VerificationState::Inconclusive
    };
    run.outcome.review = if run.outcome.work_result == WorkResult::Ready {
        run.outcome.phase = RunPhase::Reviewing;
        ReviewState::Pending
    } else {
        run.outcome.phase = RunPhase::Finished;
        ReviewState::NotRequested
    };
}

fn persist_event(
    state: &State,
    db: &Database,
    event: EventRecord,
    run: &mut RunRecord,
) -> Result<()> {
    let event = db.commit_transition(run, event)?;
    publish_event(state, event, run)
}

/// Commit a semantic event for `run`, attributed to its latest attempt.
pub(super) fn transition(
    state: &State,
    db: &Database,
    run: &mut RunRecord,
    kind: &str,
    payload: serde_json::Value,
) -> Result<()> {
    persist_event(
        state,
        db,
        EventRecord {
            run_id: run.id.clone(),
            attempt_id: run.attempts.last().map(|a| a.id.clone()),
            event_type: kind.into(),
            timestamp: Utc::now(),
            payload,
            ..Default::default()
        },
        run,
    )
}

fn publish_event(state: &State, event: EventRecord, run: &RunRecord) -> Result<()> {
    state.save_run(run)?;
    state.append_event(&event)?;
    let _ = PRESENTATION.try_with(|p| {
        p.updates.send_replace(Some((event.clone(), run.clone())));
    });
    if RUN_OUTPUT_MODE.load(Ordering::Relaxed) == RunOutputMode::Jsonl.code() {
        println!("{}", serde_json::json!({"type": "event", "event": event}));
        io::stdout().flush()?;
    }
    Ok(())
}

fn persist_check_lifecycle(
    state: &State,
    db: &Database,
    run: &mut RunRecord,
    event: CheckLifecycleEvent,
) -> Result<()> {
    let (attempt_id, event_type, payload) = match event {
        CheckLifecycleEvent::Started {
            owner_id,
            ordinal,
            phase,
            name,
            command,
        } => {
            if phase == CheckPhase::Verify {
                run.outcome.phase = RunPhase::Verifying;
            }
            (
                owner_id,
                "check.started",
                serde_json::json!({
                    "ordinal": ordinal,
                    "name": name,
                    "phase": phase,
                    "command": command,
                }),
            )
        }
        CheckLifecycleEvent::Finished {
            owner_id,
            ordinal,
            result,
        } => {
            let checks = if let Some(owner_id) = &owner_id {
                let candidate_id = run
                    .attempts
                    .iter()
                    .find(|attempt| &attempt.id == owner_id)
                    .map(|attempt| attempt.candidate_id.clone());
                candidate_id.and_then(|candidate_id| {
                    run.candidates
                        .iter_mut()
                        .find(|candidate| candidate.id == candidate_id)
                        .map(|candidate| &mut candidate.checks)
                })
            } else {
                Some(&mut run.baseline_checks)
            };
            if let Some(checks) = checks {
                if let Some(existing) = checks.get_mut(ordinal.saturating_sub(1)) {
                    *existing = result.clone();
                } else {
                    checks.push(result.clone());
                }
            }
            (
                owner_id,
                "check.finished",
                serde_json::json!({
                    "ordinal": ordinal,
                    "name": result.name,
                    "phase": result.phase,
                    "status": result.status,
                    "exit_code": result.exit_code,
                    "duration_ms": result.duration_ms,
                }),
            )
        }
    };
    persist_event(
        state,
        db,
        EventRecord {
            run_id: run.id.clone(),
            attempt_id,
            event_type: event_type.into(),
            timestamp: Utc::now(),
            payload,
            generation: 1,
            ..EventRecord::default()
        },
        run,
    )
}

pub fn run_result(run: &RunRecord) -> RunResult {
    let exit_code = if run.outcome.waiting_on == WaitingOn::Human {
        4
    } else if run
        .execution
        .as_ref()
        .is_some_and(|p| p.failure == Some(crate::FailureKind::Deadline))
    {
        124
    } else {
        match (run.outcome.work_result, run.outcome.verification) {
            (WorkResult::Ready, VerificationState::Failed) => 3,
            (WorkResult::Ready, _) => 0,
            (WorkResult::Deferred, _) => 5,
            _ => 1,
        }
    };
    RunResult {
        execution: run.execution.clone(),
        elapsed_ms: (run.completed_at.unwrap_or_else(Utc::now) - run.created_at)
            .num_milliseconds()
            .max(0),
        schema_version: 1,
        run_id: run.id.clone(),
        mode: run.mode,
        state_revision: run.state_revision,
        outcome: run.outcome.clone(),
        exit_code,
        attempts: run.attempts.clone(),
        allocation: run.allocation.clone(),
        coherence: run
            .coherence
            .as_ref()
            .and_then(|record| record.validity.as_ref())
            .filter(|validity| validity.world_changed || validity.decision != Decision::Continue)
            .map(|validity| crate::CoherenceSummary {
                decision: validity.decision,
                reasons: validity.reasons.iter().take(5).cloned().collect(),
                changed_files: validity.changed_files,
                analysis: validity.analysis,
            }),
        auto_apply: None,
    }
}

fn finish_interrupted(state: &State, db: &mut Database, run: &mut RunRecord) -> Result<()> {
    run.status = RunStatus::Interrupted;
    run.outcome.lifecycle = LifecycleState::Finished;
    run.outcome.work_result = WorkResult::Interrupted;
    run.outcome.verification = VerificationState::NotRun;
    run.outcome.review = ReviewState::NotRequested;
    run.outcome.phase = RunPhase::Finished;
    run.completed_at = Some(Utc::now());
    run.candidates
        .sort_by(|left, right| left.label.cmp(&right.label));
    db.sync_run(run)?;
    persist_event(
        state,
        db,
        EventRecord {
            run_id: run.id.clone(),
            candidate_label: None,
            event_type: "run.interrupted".into(),
            timestamp: Utc::now(),
            payload: serde_json::json!({"candidate_count": run.candidates.len()}),
            ..EventRecord::default()
        },
        run,
    )
}

fn finish_failed(
    state: &State,
    db: &mut Database,
    run: &mut RunRecord,
    message: &str,
) -> Result<()> {
    run.status = RunStatus::Failed;
    run.outcome.lifecycle = LifecycleState::Finished;
    run.outcome.work_result = WorkResult::Failed;
    run.outcome.verification = VerificationState::NotRun;
    run.outcome.review = ReviewState::NotRequested;
    run.outcome.phase = RunPhase::Finished;
    run.completed_at = Some(Utc::now());
    run.candidates
        .sort_by(|left, right| left.label.cmp(&right.label));
    db.sync_run(run)?;
    persist_event(
        state,
        db,
        EventRecord {
            run_id: run.id.clone(),
            candidate_label: None,
            event_type: "run.failed".into(),
            timestamp: Utc::now(),
            payload: serde_json::json!({"error": message}),
            ..EventRecord::default()
        },
        run,
    )
}

pub fn status(state: &State, id: Option<&str>, source_path: &Path) -> Result<()> {
    let run = match id {
        Some(id) => state.load_run(id)?,
        None => load_latest_for_source(state, source_path, false)?,
    };
    // Display only: the live verdict is never written back.
    let run = crate::coherence::with_live_validity(&run);
    if run.execution.is_some() {
        native::emit(&run, RunOutputMode::Human)?;
    } else if id.is_none() && run.allocation.is_some() && run.candidates.len() == 1 {
        let feedback = Database::open(state.db_path())?.latest_goal_feedback(&run.id)?;
        let human_outcome = feedback.as_ref().map(|feedback| &feedback.outcome);
        print_single_result_summary(&run, "Latest task", true, human_outcome);
    } else {
        print_run_header(&run);
        println!("\nTask\n  {}\n", one_line(&run.task, 120));
        print_candidates(&run);
        if let Some(line) = coherence_line(&run) {
            println!("{line}");
        }
    }
    if let Some(attachment) = &run.attachment {
        println!("\nWorkspace");
        for line in workspace_details(attachment) {
            println!("{line}");
        }
    }
    println!(
        "\nProject: {}",
        background::project_line(state, &run.source_path)
    );
    Ok(())
}

pub fn status_json(state: &State, id: Option<&str>, source_path: &Path) -> Result<()> {
    let run = match id {
        Some(id) => state.load_run(id)?,
        None => load_latest_for_source(state, source_path, false)?,
    };
    let run = crate::coherence::with_live_validity(&run);
    println!("{}", serde_json::to_string(&run_result(&run))?);
    Ok(())
}

pub fn status_jsonl(state: &State, id: Option<&str>, source_path: &Path) -> Result<()> {
    let run = match id {
        Some(id) => state.load_run(id)?,
        None => load_latest_for_source(state, source_path, false)?,
    };
    let run = crate::coherence::with_live_validity(&run);
    let database = Database::open(state.db_path())?;
    for event in database.events_for_run(&run.id)? {
        println!("{}", serde_json::json!({"type": "event", "event": event}));
    }
    println!(
        "{}",
        serde_json::json!({"type": "result", "result": run_result(&run)})
    );
    Ok(())
}

pub fn history(state: &State, limit: usize) -> Result<()> {
    state.initialize()?;
    let db = Database::open(state.db_path())?;
    let rows = db.list_runs(limit)?;
    if rows.is_empty() {
        println!("No Dispatch runs yet.");
        return Ok(());
    }
    println!(
        "{:<28} {:<17} {:<20} {:<9} {:<11} TASK",
        "RUN", "CREATED", "WORK", "STATE", "VERDICT"
    );
    for row in rows {
        let task = one_line(&row.task, 52);
        let created = row
            .created_at
            .get(..16)
            .unwrap_or(&row.created_at)
            .replace('T', " ");
        // The committed projection, read as stored: listing never repairs or
        // rewrites a run.
        let (work, state, verdict) = match db.committed_run_projection(&row.id)? {
            Some(run) => {
                let validity = run.coherence.as_ref().and_then(|c| c.validity.as_ref());
                let line = work_line(&run, validity);
                (
                    format!("{} {}", line.origin, line.agent),
                    line.state.to_owned(),
                    line.verdict.to_owned(),
                )
            }
            None => ("—".into(), row.status.clone(), "—".into()),
        };
        println!(
            "{:<28} {:<17} {:<20} {:<9} {:<11} {}",
            row.id,
            created,
            one_line(&work, 20),
            state,
            verdict,
            task
        );
    }
    Ok(())
}

pub fn show(state: &State, run_id: &str) -> Result<()> {
    let resolved_run_id = state.resolve_run_id(run_id)?;
    let run = state.load_run(&resolved_run_id)?;
    print_run_header(&run);
    if let Some(decision) = &run.allocation {
        println!();
        print_allocation_details(decision);
    }
    println!("\nTask\n{}", run.task);
    println!("\nBaseline checks");
    print_checks(&run.baseline_checks);
    println!("\nCandidates");
    print_candidates(&run);
    for candidate in &run.candidates {
        println!("Candidate {} artifacts", candidate.label);
        println!("  stdout   {}", candidate.stdout_path.display());
        println!("  stderr   {}", candidate.stderr_path.display());
        println!("  diff     {}", candidate.diff_path.display());
        println!("  checks");
        print_checks(&candidate.checks);
        println!();
    }
    if let Some(label) = &run.applied_candidate {
        println!("\nApplied candidate: {label}");
    }
    Ok(())
}

pub fn diff(
    state: &State,
    run_id: Option<&str>,
    candidate: Option<&str>,
    source_path: &Path,
    stat: bool,
    name_only: bool,
) -> Result<()> {
    let run = match run_id {
        Some(run_id) => state.load_run(run_id)?,
        None => load_latest_for_source(state, source_path, true)?,
    };
    let candidate = match candidate {
        Some(candidate) => find_candidate(&run, candidate)?,
        None => sole_candidate(&run)?,
    };
    if name_only {
        for path in &candidate.diff_stats.changed_files {
            println!("{path}");
        }
        return Ok(());
    }
    if stat {
        print_diff_stats(candidate);
        return Ok(());
    }
    println!("Candidate {}\n", candidate.label);
    print_diff_stats(candidate);
    println!();
    let patch = fs::read_to_string(&candidate.diff_path)
        .with_context(|| format!("failed to read {}", candidate.diff_path.display()))?;
    print!("{patch}");
    Ok(())
}

/// Captured from the displayed delivery, never resolved through "latest".
#[derive(Debug, Clone)]
pub struct ReviewCommand {
    pub run_id: String,
    pub candidate_id: String,
    pub revision: u64,
}

pub(crate) fn review_target(
    state: &State,
    command: &ReviewCommand,
) -> Result<(RunRecord, OperationLock)> {
    let (run, lock) = locked_delivery(state, command, true)?;
    anyhow::ensure!(
        run.outcome.review == ReviewState::Pending,
        "result has already been reviewed"
    );
    Ok((run, lock))
}

/// Lock the run and confirm it is still the delivered result `command` names.
/// A review judges a delivered result: attached work that is still active has
/// no patch yet (`dispatch finish` produces it), and a native run waiting on a
/// question is answered, not reviewed.
/// The run's delivered result, under its lock. A person's command waits a
/// moment for the lock, which the project owner holds briefly while it checks
/// the Work. `exact_revision` is for a review of the result as it was shown:
/// the command line names a run, not a revision, and a verdict the owner
/// records meanwhile is not a change to the result.
fn locked_delivery(
    state: &State,
    command: &ReviewCommand,
    exact_revision: bool,
) -> Result<(RunRecord, OperationLock)> {
    let id = state.resolve_run_id(&command.run_id)?;
    anyhow::ensure!(
        id == command.run_id,
        "review requires the complete run identity"
    );
    let lock = OperationLock::acquire_wait(
        &state.run_dir(&id).join(".operation.lock"),
        "run has a foreground owner",
        std::time::Duration::from_secs(5),
    )?;
    let run = state.load_run(&id)?;
    anyhow::ensure!(
        !exact_revision || run.state_revision == command.revision,
        "stale review; the result changed"
    );
    anyhow::ensure!(
        sole_candidate(&run)?.id == command.candidate_id,
        "candidate does not belong to this delivery"
    );
    anyhow::ensure!(
        run.outcome.lifecycle == LifecycleState::Finished
            && run.outcome.work_result == WorkResult::Ready
            && run
                .execution
                .as_ref()
                .is_none_or(|execution| execution.final_attempt_id.is_some()),
        "run {} has no delivered result to review; finish attached work or answer the pending question first",
        run.id
    );
    Ok((run, lock))
}

/// Re-enter review after a read-only inspector without resolving a new delivery.
/// The fresh revision is returned only if the original candidate is still pending.
pub fn refresh_review_target(state: &State, command: &ReviewCommand) -> Result<RunRecord> {
    let current = state.load_run(&command.run_id)?;
    let fresh = ReviewCommand {
        revision: current.state_revision,
        ..command.clone()
    };
    let (run, _lock) = review_target(state, &fresh)?;
    Ok(run)
}

pub fn review_diff(state: &State, command: &ReviewCommand) -> Result<String> {
    let (run, _lock) = review_target(state, command)?;
    // Bound display memory; the complete patch remains an artifact.
    let mut bytes = Vec::new();
    fs::File::open(&sole_candidate(&run)?.diff_path)?
        .take(256 * 1024)
        .read_to_end(&mut bytes)?;
    let mut patch = String::from_utf8_lossy(&bytes).into_owned();
    if patch.len() >= 256 * 1024 {
        patch.push_str("\n[Preview limit reached; complete patch is in Details.]\n");
    }
    Ok(patch)
}

pub fn review_delivery(state: &State, command: &ReviewCommand, accept: bool) -> Result<RunRecord> {
    let (run, _lock) = review_target(state, command)?;
    let decision = if accept {
        ReviewDecision::Accept {
            despite_refresh: false,
        }
    } else {
        ReviewDecision::Reject
    };
    review_locked(state, run, decision, Vec::new(), None, true)?;
    state.load_run(&command.run_id)
}

/// A human's decision about a delivered result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewDecision {
    /// Apply it; `despite_refresh` overrides an analysis-only REFRESH.
    Accept {
        despite_refresh: bool,
    },
    Reject,
}

pub fn accept_or_reject_latest(
    state: &State,
    run_id: Option<&str>,
    source_path: &Path,
    decision: ReviewDecision,
    reasons: Vec<String>,
    explanation: Option<String>,
) -> Result<()> {
    decide(
        state,
        run_id,
        source_path,
        decision,
        reasons,
        explanation,
        false,
    )
    .map(|_| ())
}

/// `accept` or `reject`, the same decision wherever the person makes it.
/// `quiet` prints nothing, for `watch`, which shows the returned line itself.
pub(crate) fn decide(
    state: &State,
    run_id: Option<&str>,
    source_path: &Path,
    decision: ReviewDecision,
    reasons: Vec<String>,
    explanation: Option<String>,
    quiet: bool,
) -> Result<String> {
    let accept = matches!(decision, ReviewDecision::Accept { .. });
    let despite_refresh = matches!(
        decision,
        ReviewDecision::Accept {
            despite_refresh: true
        }
    );
    anyhow::ensure!(
        !despite_refresh
            || explanation
                .as_deref()
                .is_some_and(|text| !text.trim().is_empty()),
        "--despite-refresh needs --explanation or --explanation-file: say why the work still holds"
    );
    let run = match run_id {
        Some(run_id) => state.load_run(run_id)?,
        None => load_latest_unresolved_single(state, source_path)?,
    };
    // Work whose workspace vanished before its final changes were kept has no
    // result to review, and rejecting it is how a person closes it.
    let lost = run
        .attachment
        .as_ref()
        .and_then(|attachment| attachment.workspace_removed.as_ref())
        .is_some_and(|removal| !removal.exact);
    if !accept && lost && run.outcome.lifecycle == LifecycleState::Working {
        attach::close_lost(state, &run.id)?;
        let line = format!(
            "Closed {}: its workspace is gone, and the changes last seen stay at {}.",
            run.id,
            state
                .run_dir(&run.id)
                .join("delta-last-seen.patch")
                .display()
        );
        if !quiet {
            println!("{line}");
        }
        return Ok(line);
    }
    let command = ReviewCommand {
        run_id: run.id.clone(),
        candidate_id: sole_candidate(&run)?.id.clone(),
        revision: run.state_revision,
    };
    // One lock covers the review and the apply. A result may be reviewed
    // again (the latest revision stands), so this does not require a
    // pending review the way the interactive delivery does.
    let (run, _lock) = locked_delivery(state, &command, false)?;
    let already_auto_applied = run.outcome.application == ApplicationState::Applied
        && run.outcome.applied_by == Some(AppliedBy::AutoApply);
    let id = run.id.clone();
    let kept = run
        .attachment
        .as_ref()
        .filter(|attachment| {
            attachment
                .managed
                .as_ref()
                .is_some_and(|made| !made.removed)
        })
        .map(|attachment| attachment.workspace.clone());
    review_locked(state, run, decision, reasons, explanation, quiet)?;
    let line = if accept {
        if already_auto_applied {
            "Result was already applied by auto-apply; your review is recorded.".to_owned()
        } else {
            format!("Accepted and applied {id}.")
        }
    } else {
        let mut line = format!("Rejected {id}; the source tree was not changed.");
        if let Some(kept) = kept {
            line.push_str(&format!(" Its workspace is kept at {}.", kept.display()));
        }
        line
    };
    if !quiet {
        if accept {
            if already_auto_applied {
                println!("{line}");
            }
        } else {
            println!("Result rejected. The source tree was not changed.");
            if already_auto_applied {
                println!("Rejection does not revert the source.");
            }
        }
    }
    Ok(line)
}

/// A finished, unapplied result: the only kind `check` and `refresh` act on.
fn ensure_unapplied_ready(run: &RunRecord, action: &str) -> Result<()> {
    anyhow::ensure!(
        crate::coherence::is_ready_unapplied(run),
        "run {} is not a ready, unapplied result; nothing to {action}",
        run.id
    );
    Ok(())
}

/// `dispatch check`: evaluate the run's sole candidate against the source as it
/// is now. Reads only; the verdict is printed, never stored.
pub fn check(state: &State, run_id: Option<&str>, source_path: &Path, json: bool) -> Result<()> {
    let run = match run_id {
        Some(run_id) => state.load_run(run_id)?,
        None => load_latest_for_source(state, source_path, true)?,
    };
    ensure_unapplied_ready(&run, "check")?;
    let candidate = sole_candidate(&run)?;
    let validity = crate::coherence::shown_validity(&run, &candidate.label)?;
    let landed_by = (validity.decision == Decision::Stop)
        .then(|| apply::landed_by(state, &run))
        .flatten();
    if json {
        let mut shown = serde_json::json!({"run_id": run.id, "validity": validity});
        if let Some(id) = &landed_by {
            shown["landed_by"] = serde_json::json!(id);
        }
        println!("{shown}");
        return Ok(());
    }
    println!("Run {}", run.id);
    println!(
        "Coherence: {}",
        crate::coherence::verdict(validity.decision)
    );
    println!("Analysis: {}", snake_case(&validity.analysis));
    println!("World changed files: {}", validity.changed_files);
    for reason in &validity.reasons {
        println!("  {}", reason_line(reason));
    }
    if let Some(id) = &landed_by {
        println!("  already landed by run {id}");
    }
    match validity.decision {
        Decision::Continue => println!("Next: dispatch accept {}", run.id),
        Decision::Refresh if run.mode == RunMode::Attached => println!(
            "Next: run your agent again on the current source and attach it (or dispatch reject {})",
            run.id
        ),
        Decision::Refresh => println!(
            "Next: dispatch refresh {id}  (or dispatch reject {id})",
            id = run.id
        ),
        Decision::Stop => println!("Next: dispatch reject {}", run.id),
    }
    Ok(())
}

const REFRESH_MARKER: &str = "Context: this task was previously attempted against an earlier";

/// The task sent to the agent when a run is refreshed: the original text plus a
/// fixed addendum naming the earlier run and up to ten reasons it went stale.
/// An addendum from an earlier refresh is replaced, not stacked.
pub fn refresh_task(original: &str, old_id: &str, reasons: &[crate::Reason]) -> String {
    let base = original
        .split_once(&format!("\n\n{REFRESH_MARKER}"))
        .map_or(original, |(base, _)| base)
        .trim_end();
    let mut lines = reasons
        .iter()
        .take(10)
        .map(|reason| {
            reason
                .detail
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|detail| !detail.is_empty())
        .map(|detail| format!("- {detail}"))
        .collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push("- files changed underneath the work".into());
    }
    format!(
        "{base}\n\n{REFRESH_MARKER} version of the repository (run {old_id}). The repository has changed since then:\n{}\nRe-inspect the current code before making changes; do not assume the earlier attempt's assumptions still hold.",
        lines.join("\n")
    )
}

pub struct RefreshOptions {
    pub allow_unsafe_local: bool,
    pub allow_forwarded_env: bool,
    pub config_path: Option<PathBuf>,
    pub output: RunOutputMode,
}

/// `dispatch refresh`: the request for a new run of the same task on the
/// current source. It repeats the original launch choices and demands the same
/// explicit acknowledgements again; it never touches the old run.
pub fn refresh_request(
    state: &State,
    run_id: Option<&str>,
    source_path: &Path,
    options: RefreshOptions,
) -> Result<RunRequest> {
    let run = match run_id {
        Some(run_id) => state.load_run(run_id)?,
        None => load_latest_for_source(state, source_path, true)?,
    };
    anyhow::ensure!(
        run.mode != RunMode::Attached,
        "attached work has no Dispatch task to refresh; finish or reject it"
    );
    ensure_unapplied_ready(&run, "refresh")?;
    let mut missing = Vec::new();
    if run.environment.unsafe_local && !options.allow_unsafe_local {
        missing.push("--allow-unsafe-local");
    }
    if !run.environment.forwarded_env.is_empty() && !options.allow_forwarded_env {
        missing.push("--allow-forwarded-env");
    }
    anyhow::ensure!(
        missing.is_empty(),
        "refresh launches new work and never inherits authority; pass {} again, as for the original run",
        missing.join(" and ")
    );
    let reasons = crate::coherence::live_validity(&run)
        .map(|validity| validity.reasons)
        .unwrap_or_default();
    // The same agent choice again; runs made before 0.4.1 without an explicit
    // choice redo the agent that produced their result.
    let goal = run.execution.as_ref();
    let agent = goal
        .and_then(|goal| goal.fixed_harness.clone())
        .or_else(|| {
            run.allocation
                .is_none()
                .then(|| run.candidates.first().map(|c| c.harness_id.clone()))
                .flatten()
        });
    Ok(RunRequest {
        source: run.source_path.clone(),
        task: refresh_task(&run.task, &run.id, &reasons),
        agent,
        model: goal.and_then(|goal| goal.fixed_model.clone()),
        effort: goal.and_then(|goal| goal.fixed_effort.clone()),
        config_path: options.config_path,
        backend: Some(run.environment.execution_backend.clone()),
        timeout_secs: Some(run.environment.timeout_secs),
        allow_unsafe_local: options.allow_unsafe_local,
        allow_forwarded_env: options.allow_forwarded_env,
        output: options.output,
        refreshed_from: Some(run.id.clone()),
    })
}

/// A reason on one line: Git's multi-line complaints included.
fn line_reason(detail: &str) -> String {
    detail
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(60)
        .collect()
}

/// The Coherence line for a Ready run that carries a validity, else `None`.
/// One Work item as every list shows it: where it came from and what it
/// began against, the verdict and why, and where verification, review and
/// integration stand. Built from the run and the validity given (stored or
/// live); nothing is evaluated or written here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkLine {
    pub origin: &'static str,
    pub agent: String,
    pub s0: String,
    /// `CONTINUE`, `REFRESH`, `STOP`, `unmoved`, `not checked`, or
    /// `overridden` when a human applied it over a REFRESH.
    pub verdict: &'static str,
    pub reason: Option<String>,
    /// `working`, `question`, `idle` (discovered, no live session), `removed`
    /// or `lost` (its workspace is gone, with its exact or only its last-seen
    /// changes kept), `ready`, `blocked`, `applied` or `finished`.
    pub state: &'static str,
    /// Who applied it: `human` or `auto_apply`; `None` when not applied.
    pub applied_by: Option<&'static str>,
    pub verification: &'static str,
    /// `pending`, `accepted` or `rejected`; `None` before a result exists.
    pub review: Option<&'static str>,
}

pub(crate) fn work_line(run: &RunRecord, validity: Option<&crate::Validity>) -> WorkLine {
    let attached = run.mode == RunMode::Attached;
    let agent = run
        .candidates
        .first()
        .map(|candidate| candidate.harness_id.clone())
        .or_else(|| run.attachment.as_ref().and_then(|a| a.agent.clone()))
        .or_else(|| run.execution.as_ref().and_then(|e| e.fixed_harness.clone()))
        .or_else(|| run.allocation.as_ref().map(|a| a.selected.harness.clone()))
        .unwrap_or_else(|| if attached { "external" } else { "unknown" }.into());
    let short = |commit: &str| commit.chars().take(8).collect::<String>();
    let s0 = match run.attachment.as_ref().map(|a| &a.provenance) {
        Some(crate::BaselineProvenance::GitMergeBase { commit }) => {
            format!("merge-base {} (full)", short(commit))
        }
        Some(crate::BaselineProvenance::SnapshotAtAttach) => "snapshot at attach (partial)".into(),
        Some(crate::BaselineProvenance::WorkspaceAtStart { commit }) => {
            format!("workspace at start {}", short(commit))
        }
        // Native S0 is the project as it was, named by its commit when it has
        // one; Dispatch's own baseline commit means nothing to the reader.
        None => run.source_git_head.as_deref().map_or_else(
            || "directory snapshot".to_owned(),
            |head| format!("snapshot at {}", short(head)),
        ),
    };
    let overridden = run
        .coherence
        .as_ref()
        .and_then(|record| record.overridden.as_ref())
        .filter(|_| run.outcome.application == ApplicationState::Applied);
    let (verdict, reason) = match validity {
        _ if overridden.is_some() => (
            "overridden",
            overridden
                .and_then(|v| v.reasons.first())
                .map(|r| line_reason(&r.detail)),
        ),
        // Applied with no stored verdict: accept found the source unmoved.
        None if run.outcome.application == ApplicationState::Applied => ("unmoved", None),
        None => ("not checked", None),
        Some(v) if v.decision == Decision::Continue && !v.world_changed => ("unmoved", None),
        Some(v) => (
            crate::coherence::verdict(v.decision),
            v.reasons.first().map(|r| line_reason(&r.detail)),
        ),
    };
    let attachment = run.attachment.as_ref();
    let removal = attachment.and_then(|attachment| attachment.workspace_removed.as_ref());
    let state = if run.outcome.waiting_on == WaitingOn::Human {
        "question"
    } else if run.outcome.lifecycle != LifecycleState::Finished {
        match removal {
            // Waiting for a person to finish or reject it.
            Some(removal) if removal.exact => "removed",
            Some(_) => "lost",
            // Discovered work with no session open right now.
            None if attachment
                .and_then(|attachment| attachment.sessions.last())
                .is_some_and(|session| session.ended_at.is_some()) =>
            {
                "idle"
            }
            None => "working",
        }
    } else {
        match run.outcome.application {
            ApplicationState::Applied => "applied",
            ApplicationState::BlockedBySourceDrift | ApplicationState::Failed => "blocked",
            ApplicationState::NotApplied if run.outcome.work_result != WorkResult::Ready => {
                "finished"
            }
            ApplicationState::NotApplied if run.outcome.review == ReviewState::Rejected => {
                "rejected"
            }
            ApplicationState::NotApplied
                if validity.is_some_and(|v| v.decision != Decision::Continue) =>
            {
                "blocked"
            }
            ApplicationState::NotApplied => "ready",
        }
    };
    let verification = match run.outcome.verification {
        VerificationState::NotConfigured => "no checks",
        VerificationState::NotRun => "checks not run",
        VerificationState::Passed => "checks passed",
        VerificationState::Failed => "checks failed",
        VerificationState::Inconclusive => "checks inconclusive",
    };
    let review = match run.outcome.review {
        ReviewState::Accepted => Some("accepted"),
        ReviewState::Rejected => Some("rejected"),
        _ if run.outcome.work_result == WorkResult::Ready => Some("pending"),
        _ => None,
    };
    let applied_by = match run.outcome.applied_by {
        _ if run.outcome.application != ApplicationState::Applied => None,
        Some(AppliedBy::AutoApply) => Some("auto_apply"),
        _ => Some("human"),
    };
    let origin = match attachment {
        None => "native",
        Some(attachment) if attachment.workspace_owner == crate::WorkspaceOwner::Dispatch => {
            "isolated"
        }
        Some(attachment) if !attachment.sessions.is_empty() => "discovered",
        Some(_) => "attached",
    };
    WorkLine {
        origin,
        agent,
        s0,
        verdict,
        reason,
        state,
        applied_by,
        verification,
        review,
    }
}

impl std::fmt::Display for WorkLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} · S0 {} · {}",
            self.origin, self.agent, self.s0, self.verdict
        )?;
        if let Some(reason) = &self.reason {
            write!(f, ": {reason}")?;
        }
        match self.applied_by {
            Some("auto_apply") => write!(f, " · applied by auto-apply")?,
            _ => write!(f, " · {}", self.state)?,
        }
        write!(f, " · {}", self.verification)?;
        if let Some(review) = self.review {
            write!(f, " · review {review}")?;
        }
        Ok(())
    }
}

fn coherence_line(run: &RunRecord) -> Option<String> {
    let validity = run.coherence.as_ref()?.validity.as_ref()?;
    (run.outcome.work_result == WorkResult::Ready)
        .then(|| crate::presenter::coherence_text(validity))
}

fn snake_case<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn reason_line(reason: &crate::Reason) -> String {
    let detail = reason
        .detail
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    format!("{}: {detail}", snake_case(&reason.code))
}

/// Wall-clock seconds the candidate's attempts ran after `invalid_at`, and in
/// total, from attempt timestamps only. `None` when no attempt has a duration.
fn agent_seconds_after(run: &RunRecord, invalid_at: chrono::DateTime<Utc>) -> Option<(i64, i64)> {
    let candidate = sole_candidate(run).ok()?;
    let (mut after, mut total) = (0, 0);
    for attempt in run
        .attempts
        .iter()
        .filter(|attempt| attempt.candidate_id == candidate.id)
    {
        let end = attempt.completed_at?;
        total += (end - attempt.started_at).num_seconds().max(0);
        after += (end - attempt.started_at.max(invalid_at))
            .num_seconds()
            .max(0);
    }
    (total > 0).then_some((after, total))
}

fn minutes_seconds(seconds: i64) -> String {
    format!("{}m{}s", seconds / 60, seconds % 60)
}

/// Print the Coherence section when the run has coherence data (a moved world
/// or a lineage); returns whether anything was printed.
fn print_coherence_details(run: &RunRecord) -> bool {
    let Some(record) = &run.coherence else {
        return false;
    };
    let moved = record
        .validity
        .as_ref()
        .filter(|v| v.world_changed || v.decision != Decision::Continue);
    if moved.is_none() && record.refreshed_from.is_none() {
        return false;
    }
    println!("\nCoherence");
    if let Some(validity) = moved {
        println!(
            "  decision: {}",
            crate::coherence::verdict(validity.decision)
        );
        println!("  analysis: {}", snake_case(&validity.analysis));
        println!(
            "  world changed: {} ({} file(s))",
            if validity.world_changed { "yes" } else { "no" },
            validity.changed_files
        );
        for reason in validity.reasons.iter().take(10) {
            println!("  reason: {}", reason_line(reason));
        }
    }
    if let Some(at) = record.first_invalid_at {
        println!("  first invalid at: {}", at.to_rfc3339());
        if let Some((after, total)) = agent_seconds_after(run, at) {
            println!(
                "  Agent time after the work became invalid: {} of {} ({}%)",
                minutes_seconds(after),
                minutes_seconds(total),
                after * 100 / total
            );
        }
    }
    if let Some(old) = &record.refreshed_from {
        println!("  refreshed from: {old}");
    }
    true
}

pub fn explain(state: &State, run_id: Option<&str>, source_path: &Path) -> Result<()> {
    let run = match run_id {
        Some(run_id) => state.load_run(run_id)?,
        None => load_latest_for_source(state, source_path, true)?,
    };
    // Display only: the live verdict is never written back.
    let run = crate::coherence::with_live_validity(&run);
    if run.mode == RunMode::Attached {
        // Attached work was neither selected nor allocated by Dispatch: there
        // is no selection to explain, only where its S0 came from and whether
        // the work still holds against the root.
        print_attachment_details(&run);
        if !print_coherence_details(&run)
            && let Some(validity) = run.coherence.as_ref().and_then(|r| r.validity.as_ref())
        {
            println!("\n{}", crate::presenter::coherence_text(validity));
        }
        return Ok(());
    }
    let selection = explain_selection(&run);
    let coherence = print_coherence_details(&run);
    match selection {
        Err(error) if coherence => {
            println!("\n{error}");
            Ok(())
        }
        other => other,
    }
}

/// The attachment record as `explain` shows it: provenance and confidence of
/// S0 (the honesty rule for work Dispatch did not launch), who owns it, and
/// what it is allowed to do.
/// Where attached Work happens and what became of the place, for `status`
/// and `explain`: the recovery facts live on the run, not in a list.
fn workspace_details(attachment: &crate::AttachmentRecord) -> Vec<String> {
    let mut lines = vec![format!("  workspace: {}", attachment.workspace.display())];
    lines.push(format!(
        "  made by: {}",
        match attachment.workspace_owner {
            crate::WorkspaceOwner::User => "you",
            crate::WorkspaceOwner::Runtime => "the agent's runtime",
            crate::WorkspaceOwner::Dispatch => "Dispatch",
        }
    ));
    if let Some(made) = &attachment.managed {
        lines.push(match (made.removed, &made.branch) {
            (true, _) => "  released: the work was applied".to_owned(),
            (false, Some(branch)) => format!("  branch: {branch}; kept until the work is applied"),
            (false, None) => "  a private copy; kept until the work is applied".to_owned(),
        });
    }
    if let Some(last) = attachment.sessions.last() {
        lines.push(format!(
            "  sessions: {} ({} {}, {})",
            attachment.sessions.len(),
            last.provider,
            last.session_id,
            if last.ended_at.is_some() {
                "ended"
            } else {
                "open"
            }
        ));
    }
    if let Some(removal) = &attachment.workspace_removed {
        lines.push(format!(
            "  removed {}: {}",
            removal.at.format("%Y-%m-%d %H:%M UTC"),
            if removal.exact {
                "its exact changes are kept; finish or reject it"
            } else {
                "only the changes last seen were kept; it cannot be finished"
            }
        ));
    }
    lines
}

fn print_attachment_details(run: &RunRecord) {
    let Some(attachment) = &run.attachment else {
        return;
    };
    println!("Attached work");
    for line in workspace_details(attachment) {
        println!("{line}");
    }
    println!("  root: {}", attachment.integration_root.display());
    match &attachment.provenance {
        crate::BaselineProvenance::GitMergeBase { commit } => {
            println!("  S0: commit {commit} (merge base with the root)");
        }
        crate::BaselineProvenance::SnapshotAtAttach => {
            println!("  S0: snapshot of the workspace at attach; earlier edits are not attributed");
        }
        crate::BaselineProvenance::WorkspaceAtStart { commit } => {
            println!("  S0: the workspace as it was when the work began ({commit})");
        }
    }
    println!("  confidence: {}", snake_case(&attachment.confidence));
    println!(
        "  agent: {}",
        attachment.agent.as_deref().unwrap_or("external")
    );
    println!("  owner: {}", snake_case(&attachment.owner_state));
    let capabilities = &attachment.capabilities;
    println!(
        "  may: observe={} signal={} control={} integrate={}",
        capabilities.observe, capabilities.signal, capabilities.control, capabilities.integrate
    );
    if let Some(reason) = &attachment.finish_reason {
        println!("  finished: {}", snake_case(reason));
    }
}

fn explain_selection(run: &RunRecord) -> Result<()> {
    if let Some(decision) = &run.allocation {
        print_allocation_details(decision);
        return Ok(());
    }
    if let Some(agent) = run
        .execution
        .as_ref()
        .and_then(|goal| goal.fixed_harness.as_deref())
    {
        println!(
            "Agent\n  {}\n\nSelection\n  Chosen explicitly with --agent",
            harness_name(agent)
        );
        return Ok(());
    }
    bail!("this run has no single-agent selection to explain")
}

fn print_allocation_details(decision: &AllocationDecision) {
    println!("Selected resource");
    println!("  provider: {}", decision.selected.provider);
    println!("  funding source: {}", decision.selected.funding_source);
    println!("  harness: {}", decision.selected.harness);
    println!("  requested model: {}", decision.selected.requested_model);
    println!("  resolved model: {}", decision.selected.resolved_model);
    println!(
        "  effort: {}",
        decision.selected.effort.as_deref().unwrap_or("default")
    );
    println!("  service mode: {}", decision.selected.service_mode);
    println!("  runtime: {}", decision.selected.runtime);
    println!(
        "  profile no-overage assertion: {}",
        decision.selected.no_overage_verified
    );
    println!("\nSelection reason\n  {}", decision.reason);
    println!("Capability provenance\n  {}", decision.capability.source);
    println!("\nConfigured profiles");
    for alternative in &decision.alternatives {
        println!(
            "  {} / {} / {}: {}",
            alternative.choice.harness,
            alternative.choice.requested_model,
            alternative.choice.effort.as_deref().unwrap_or("default"),
            alternative
                .exclusion
                .as_deref()
                .unwrap_or("eligible and selected")
        );
    }
}

/// On accept apply a delivered result, then record the human review, for
/// every kind of work; a reject only records the review. The review is a
/// revision (reasons and explanation verbatim), `outcome.review` and a
/// `review.accepted|rejected` event. The caller holds the run lock from
/// `locked_delivery`, so the apply and the review judge the same revision.
fn review_locked(
    state: &State,
    mut run: RunRecord,
    decision: ReviewDecision,
    reasons: Vec<String>,
    explanation: Option<String>,
    quiet: bool,
) -> Result<()> {
    let (accept, despite_refresh) = match decision {
        ReviewDecision::Accept { despite_refresh } => (true, despite_refresh),
        ReviewDecision::Reject => (false, false),
    };
    // A run already applied by the auto-apply policy only has its human
    // review recorded here; applying again would be a second, redundant
    // application and rejection must never revert the source.
    let already_auto_applied = run.outcome.application == ApplicationState::Applied
        && run.outcome.applied_by == Some(AppliedBy::AutoApply);
    let reasons = normalize_reasons(reasons)?;
    // An accept is recorded only once the work has landed. When coherence or
    // the apply refuses it, the refusal is recorded and the review stays
    // pending: no human acceptance of work that never reached the source.
    if accept && !already_auto_applied {
        let id = run.id.clone();
        let overridden =
            apply::apply_locked(state, run, quiet, ApplyAuthority::Human, despite_refresh)?;
        let database = Database::open(state.db_path())?;
        run = database
            .committed_run_projection(&id)?
            .context("applied run has no committed projection")?;
        // The override is evidence in its own right: which verdict a human
        // set aside, and why.
        if let Some(overridden) = overridden {
            persist_event(
                state,
                &database,
                EventRecord {
                    run_id: run.id.clone(),
                    candidate_label: run
                        .candidates
                        .first()
                        .map(|candidate| candidate.label.clone()),
                    event_type: "coherence.overridden".into(),
                    timestamp: Utc::now(),
                    payload: serde_json::json!({
                        "coherence": overridden,
                        "explanation": explanation,
                    }),
                    ..EventRecord::default()
                },
                &mut run,
            )?;
            if !quiet {
                println!(
                    "Applied over REFRESH on your explanation; the checks passed on the merged tree."
                );
            }
        }
    }
    let outcome = if accept {
        ReviewOutcome::Accepted
    } else {
        ReviewOutcome::Rejected
    };
    let mut database = Database::open(state.db_path())?;
    let feedback = database.save_goal_feedback(&run.id, outcome, reasons, explanation)?;
    run.outcome.review = if accept {
        ReviewState::Accepted
    } else {
        ReviewState::Rejected
    };
    persist_event(
        state,
        &database,
        EventRecord {
            run_id: run.id.clone(),
            candidate_label: run
                .candidates
                .first()
                .map(|candidate| candidate.label.clone()),
            event_type: if accept {
                "review.accepted"
            } else {
                "review.rejected"
            }
            .into(),
            timestamp: feedback.created_at,
            payload: serde_json::json!({"feedback_revision": feedback.revision}),
            ..EventRecord::default()
        },
        &mut run,
    )?;
    if !quiet {
        println!("Review recorded (revision {}).", feedback.revision);
        if let Some(attachment) = run.attachment.as_ref().filter(|attachment| {
            attachment
                .managed
                .as_ref()
                .is_some_and(|made| !made.removed)
        }) && run.outcome.review == ReviewState::Rejected
        {
            println!(
                "The workspace Dispatch made for it is kept at {}.",
                attachment.workspace.display()
            );
        }
    }
    Ok(())
}

pub fn read_verbatim(path: &Path) -> Result<String> {
    if path == Path::new("-") {
        let mut value = String::new();
        io::stdin().read_to_string(&mut value)?;
        return Ok(value);
    }
    fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))
}

fn normalize_reasons(reasons: Vec<String>) -> Result<Vec<String>> {
    let mut output = Vec::new();
    for reason in reasons {
        let reason = reason.trim().to_lowercase();
        if !EVALUATION_REASONS.contains(&reason.as_str()) {
            bail!(
                "unknown structured reason: {reason:?}\nvalid reasons: {}",
                EVALUATION_REASONS.join(", ")
            );
        }
        if !output.contains(&reason) {
            output.push(reason);
        }
    }
    Ok(output)
}

fn find_candidate<'a>(run: &'a RunRecord, value: &str) -> Result<&'a CandidateRecord> {
    run.candidates
        .iter()
        .find(|candidate| {
            candidate.label.eq_ignore_ascii_case(value) || candidate.id.eq_ignore_ascii_case(value)
        })
        .with_context(|| format!("run {} has no candidate {value}", run.id))
}

fn sole_candidate(run: &RunRecord) -> Result<&CandidateRecord> {
    let [candidate] = run.candidates.as_slice() else {
        bail!("run {} does not have exactly one result", run.id);
    };
    Ok(candidate)
}

fn load_latest_for_source(state: &State, source_path: &Path, single: bool) -> Result<RunRecord> {
    let source_path = source::resolve_source(Some(source_path))?;
    for path in state.list_metadata_paths()? {
        let projected: RunRecord = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("invalid metadata at {}", path.display()))?;
        let run = state.load_run(&projected.id)?;
        if run.source_path == source_path && (!single || run.candidates.len() == 1) {
            return Ok(run);
        }
    }
    bail!("no Dispatch runs for {}", source_path.display())
}

fn load_latest_unresolved_single(state: &State, source_path: &Path) -> Result<RunRecord> {
    let source_path = source::resolve_source(Some(source_path))?;
    for path in state.list_metadata_paths()? {
        let projected: RunRecord = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("invalid metadata at {}", path.display()))?;
        let run = state.load_run(&projected.id)?;
        if run.source_path != source_path
            || run.candidates.len() != 1
            || (run.allocation.is_none()
                && run.execution.is_none()
                && run.mode != RunMode::Attached)
        {
            continue;
        }
        if run.outcome.review == ReviewState::Pending {
            return Ok(run);
        }
    }
    bail!(
        "no unresolved single-result Dispatch run for {}",
        source_path.display()
    )
}

fn print_run_header(run: &RunRecord) {
    println!("RUN {}", run.id);
    let validity = run.coherence.as_ref().and_then(|c| c.validity.as_ref());
    println!("Status           {}", work_line(run, validity).state);
    println!("Source           {}", run.source_path.display());
    println!("Source type      {}", run.source_kind.as_str());
    println!("Baseline         {}", run.baseline_commit);
    if let Some(decision) = &run.allocation {
        println!();
        print_allocation_decision(decision);
    }
}

fn print_candidates(run: &RunRecord) {
    if run.candidates.is_empty() {
        println!("No candidates recorded.");
        return;
    }
    for candidate in &run.candidates {
        println!("Candidate {} ({})", candidate.label, candidate.harness_id);
        println!("  Status          {}", candidate.status.as_str());
        println!("  Verification    {}", verification_summary(candidate));
        println!(
            "  Runtime         {}",
            format_duration(candidate.duration_ms)
        );
        println!(
            "  Cost            {}",
            candidate
                .cost_usd
                .map_or_else(|| "unknown".into(), |v| format!("${v:.4}"))
        );
        println!(
            "  Task tokens     {}",
            candidate
                .tokens
                .map_or_else(|| "unknown".into(), |v| v.to_string())
        );
        println!(
            "  Token basis     {}",
            candidate.token_semantics.as_deref().unwrap_or("unknown")
        );
        println!("  Files changed   {}", candidate.diff_stats.files_changed);
        println!(
            "  Diff             +{} / -{}",
            candidate.diff_stats.lines_added, candidate.diff_stats.lines_removed
        );
        if let Some(error) = &candidate.error {
            println!("  Error           {}", one_line(error, 100));
        }
        println!();
    }
}

fn print_checks(checks: &[crate::CheckResult]) {
    if checks.is_empty() {
        println!("  none configured");
        return;
    }
    for check in checks {
        println!(
            "  {:<8} {:<8} {:>8}  {}",
            format!("{:?}", check.phase).to_lowercase(),
            format!("{:?}", check.status).to_uppercase(),
            format_duration(check.duration_ms),
            check.command
        );
        println!("    stdout {}", check.stdout_path.display());
        println!("    stderr {}", check.stderr_path.display());
    }
}

fn print_diff_stats(candidate: &CandidateRecord) {
    println!("Files changed: {}", candidate.diff_stats.files_changed);
    println!(
        "+{} / -{}",
        candidate.diff_stats.lines_added, candidate.diff_stats.lines_removed
    );
}

fn verification_summary(candidate: &CandidateRecord) -> &'static str {
    if candidate.checks.is_empty() && candidate.status == CandidateStatus::TimedOut {
        return "not run — candidate timed out";
    }
    checks_summary(&candidate.checks)
}

fn checks_summary(checks: &[crate::CheckResult]) -> &'static str {
    if checks.is_empty() {
        return "not configured";
    }
    if checks.iter().all(|c| c.status == CheckStatus::Passed) {
        "PASS"
    } else {
        "FAIL"
    }
}

fn format_duration(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{}.{:01}s", seconds, (ms % 1000) / 100)
    } else {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    }
}

fn command_version(program: &str, args: &[&str]) -> Option<String> {
    let executable = trusted_host_executable(program).ok()?;
    let output = Command::new(executable).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = if output.stdout.is_empty() {
        &output.stderr
    } else {
        &output.stdout
    };
    Some(one_line(&String::from_utf8_lossy(value), 80))
}

fn print_detection(label: &str, version: &Option<String>) {
    match version {
        Some(version) => println!("{label:<17} ✓ {version}"),
        None => println!("{label:<17} – not found"),
    }
}

fn one_line(value: &str, max_chars: usize) -> String {
    let mut text = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() > max_chars {
        text = text
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect::<String>();
        text.push('…');
    }
    text
}

fn canonicalize_allow_missing(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut cursor = absolute.as_path();
    let mut missing = Vec::<OsString>::new();
    loop {
        match fs::canonicalize(cursor) {
            Ok(mut resolved) => {
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(
                    cursor
                        .file_name()
                        .with_context(|| format!("could not resolve {}", path.display()))?
                        .to_os_string(),
                );
                cursor = cursor
                    .parent()
                    .with_context(|| format!("could not resolve {}", path.display()))?;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to resolve {}", path.display()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Validity;

    #[test]
    fn a_work_line_reason_is_one_line() {
        assert_eq!(
            line_reason("error: patch failed: a.py:3\nerror: a.py: patch does not apply"),
            "error: patch failed: a.py:3 error: a.py: patch does not appl"
        );
    }

    fn reason(detail: &str) -> crate::Reason {
        crate::Reason {
            code: crate::ReasonCode::FactBroken,
            fact_id: None,
            path: None,
            detail: detail.into(),
        }
    }

    fn validity_with(decision: Decision, reasons: usize) -> Validity {
        Validity {
            decision,
            evaluated_at: Utc::now(),
            world_digest: String::new(),
            world_changed: true,
            changed_files: 1,
            reasons: (0..reasons).map(|i| reason(&format!("fact {i}"))).collect(),
            analysis: crate::AnalysisLevel::FilesOnly,
        }
    }

    fn sample_candidate() -> CandidateRecord {
        CandidateRecord {
            id: "candidate".into(),
            label: "A".into(),
            harness_id: "fake-good".into(),
            harness_version: None,
            model: None,
            status: CandidateStatus::Completed,
            workspace_path: PathBuf::new(),
            prompt_path: PathBuf::new(),
            stdout_path: PathBuf::new(),
            stderr_path: PathBuf::new(),
            diff_path: PathBuf::new(),
            duration_ms: 1_000,
            exit_code: Some(0),
            timed_out: false,
            tokens: None,
            token_semantics: None,
            cost_usd: None,
            error: None,
            diff_stats: DiffStats::default(),
            checks: vec![],
        }
    }

    #[test]
    fn run_result_summarizes_stored_validity_only_when_the_world_moved() {
        let mut run: RunRecord = serde_json::from_value(serde_json::json!({
            "id":"01TEST", "task":"t", "exact_prompt":"t",
            "source_path":"/source", "source_kind":"directory", "source_git_head":null,
            "source_fingerprint":"b", "baseline_path":"/b", "baseline_commit":"abc",
            "status":"running", "created_at":"2026-09-17T00:00:00Z", "completed_at":null,
            "environment":{"dispatch_version":"t","os":"t","architecture":"t","execution_backend":"local","timeout_secs":30,"cpus":1.0,"memory":"1g","max_parallel":1},
            "evaluation":null,"applied_candidate":null
        }))
        .unwrap();
        assert!(run_result(&run).coherence.is_none());
        let mut validity = Validity {
            decision: Decision::Continue,
            evaluated_at: Utc::now(),
            world_digest: String::new(),
            world_changed: false,
            changed_files: 0,
            reasons: Vec::new(),
            analysis: crate::AnalysisLevel::FilesOnly,
        };
        let record = |validity: &Validity| CoherenceRecord {
            version: 1,
            refreshed_from: None,
            validity: Some(validity.clone()),
            first_invalid_at: None,
            overridden: None,
        };
        run.coherence = Some(record(&validity));
        let json = serde_json::to_value(run_result(&run)).unwrap();
        assert!(json.get("coherence").is_none(), "{json}");
        validity.decision = Decision::Refresh;
        validity.world_changed = true;
        validity.changed_files = 3;
        validity.reasons = (0..8).map(|n| reason(&format!("r{n}"))).collect();
        run.coherence = Some(record(&validity));
        let json = serde_json::to_value(run_result(&run)).unwrap();
        assert_eq!(json["coherence"]["decision"], "refresh");
        assert_eq!(json["coherence"]["changed_files"], 3);
        assert_eq!(json["coherence"]["analysis"], "files_only");
        assert_eq!(json["coherence"]["reasons"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn refresh_addendum_is_deterministic_and_bounded() {
        let reasons = (0..12)
            .map(|n| reason(&format!("fact {n} changed\nacross lines")))
            .collect::<Vec<_>>();
        let task = refresh_task("Fix the cache.\n", "RUN1", &reasons);
        assert_eq!(task, refresh_task("Fix the cache.\n", "RUN1", &reasons));
        assert!(task.starts_with(
            "Fix the cache.\n\nContext: this task was previously attempted against an earlier version of the repository (run RUN1). The repository has changed since then:\n- fact 0 changed across lines\n"
        ));
        assert_eq!(task.lines().filter(|l| l.starts_with("- ")).count(), 10);
        assert!(task.contains("- fact 9 changed across lines\nRe-inspect"));
        assert!(!task.contains("fact 10"));
        assert!(task.ends_with(
            "Re-inspect the current code before making changes; do not assume the earlier attempt's assumptions still hold."
        ));
        assert_eq!(
            refresh_task("Task", "R", &[]),
            "Task\n\nContext: this task was previously attempted against an earlier version of the repository (run R). The repository has changed since then:\n- files changed underneath the work\nRe-inspect the current code before making changes; do not assume the earlier attempt's assumptions still hold."
        );
        // Refreshing a refreshed run replaces the addendum instead of stacking.
        let again = refresh_task(&task, "RUN2", &[reason("new")]);
        assert_eq!(again.matches("Context: this task").count(), 1);
        assert!(again.starts_with("Fix the cache.\n\nContext:") && again.contains("(run RUN2)"));
    }

    #[test]
    fn work_line_names_origin_agent_s0_verdict_and_review() {
        let mut run: RunRecord = serde_json::from_value(serde_json::json!({
            "id":"01WORK", "task":"t", "exact_prompt":"t",
            "source_path":"/source", "source_kind":"directory", "source_git_head":null,
            "source_fingerprint":"b", "baseline_path":"/b", "baseline_commit":"abcdef0123456789",
            "status":"running", "created_at":"2026-09-17T00:00:00Z", "completed_at":null,
            "environment":{"dispatch_version":"t","os":"t","architecture":"t","execution_backend":"local","timeout_secs":30,"cpus":1.0,"memory":"1g","max_parallel":1},
            "applied_candidate":null
        }))
        .unwrap();
        run.candidates.push(CandidateRecord {
            harness_id: "claude".into(),
            ..sample_candidate()
        });
        run.outcome.lifecycle = LifecycleState::Working;
        let working = work_line(&run, None);
        // S0 names the project, never Dispatch's own baseline commit.
        assert_eq!(
            (working.origin, working.agent.as_str(), working.s0.as_str()),
            ("native", "claude", "directory snapshot")
        );
        run.source_git_head = Some("eec41cc70c0a40aa".into());
        assert_eq!(work_line(&run, None).s0, "snapshot at eec41cc7");
        assert_eq!(
            (working.verdict, working.state, working.review),
            ("not checked", "working", None)
        );

        run.outcome.lifecycle = LifecycleState::Finished;
        run.outcome.work_result = WorkResult::Ready;
        run.outcome.verification = VerificationState::Passed;
        let mut unmoved = validity_with(Decision::Continue, 0);
        unmoved.world_changed = false;
        let ready = work_line(&run, Some(&unmoved));
        assert_eq!(
            (ready.verdict, ready.state, ready.review),
            ("unmoved", "ready", Some("pending"))
        );
        assert_eq!(
            ready.to_string(),
            "native claude · S0 snapshot at eec41cc7 · unmoved · ready · checks passed · review pending"
        );

        let stale = work_line(&run, Some(&validity_with(Decision::Refresh, 1)));
        assert_eq!((stale.verdict, stale.state), ("REFRESH", "blocked"));
        assert!(stale.reason.is_some());

        run.outcome.application = ApplicationState::Applied;
        run.outcome.applied_by = Some(AppliedBy::AutoApply);
        let applied = work_line(&run, None);
        assert_eq!(
            (applied.state, applied.applied_by),
            ("applied", Some("auto_apply"))
        );
        // Applied with no stored verdict means accept found the source unmoved.
        assert_eq!(applied.verdict, "unmoved");
        assert!(applied.to_string().contains("applied by auto-apply"));
    }

    #[test]
    fn agent_time_after_invalid_uses_only_attempt_timestamps() {
        let mut run: RunRecord = serde_json::from_value(serde_json::json!({
            "id":"01TEST", "task":"t", "exact_prompt":"t",
            "source_path":"/source", "source_kind":"directory", "source_git_head":null,
            "source_fingerprint":"b", "baseline_path":"/b", "baseline_commit":"abc",
            "status":"running", "created_at":"2026-09-17T00:00:00Z", "completed_at":null,
            "environment":{"dispatch_version":"t","os":"t","architecture":"t","execution_backend":"local","timeout_secs":30,"cpus":1.0,"memory":"1g","max_parallel":1},
            "evaluation":null,"applied_candidate":null
        }))
        .unwrap();
        let start: chrono::DateTime<Utc> = "2026-09-17T10:00:00Z".parse().unwrap();
        let attempt = |end: Option<chrono::DateTime<Utc>>| -> AttemptRecord {
            serde_json::from_value(serde_json::json!({
                "id":"a1","run_id":"01TEST","candidate_id":"c1","role":"work","ordinal":1,
                "generation":1,"harness_id":"h","harness_version":null,"requested_model":null,
                "resolved_model":null,"observed_model":null,"requested_effort":null,
                "resolved_effort":null,"observed_effort":null,"started_at":start,
                "completed_at":end,"outcome":"completed","raw_telemetry_path":""
            }))
            .unwrap()
        };
        let mut candidate: CandidateRecord = serde_json::from_value(serde_json::json!({
            "id":"c1","label":"A","harness_id":"h","harness_version":null,"model":null,
            "status":"completed","workspace_path":"","prompt_path":"","stdout_path":"",
            "stderr_path":"","diff_path":"","duration_ms":0,"exit_code":0,"timed_out":false,
            "tokens":null,"token_semantics":null,"cost_usd":null,"error":null,
            "diff_stats":{"files_changed":0,"lines_added":0,"lines_removed":0},"checks":[]
        }))
        .unwrap();
        candidate.id = "c1".into();
        run.candidates = vec![candidate];
        let end = start + chrono::TimeDelta::seconds(580);
        run.attempts = vec![attempt(Some(end))];
        let invalid = start + chrono::TimeDelta::seconds(328);
        let (after, total) = agent_seconds_after(&run, invalid).unwrap();
        assert_eq!((after, total), (252, 580));
        assert_eq!(minutes_seconds(after), "4m12s");
        assert_eq!(minutes_seconds(total), "9m40s");
        assert_eq!(after * 100 / total, 43);
        // Invalid before the attempt began: all of it; after it ended: none.
        assert_eq!(
            agent_seconds_after(&run, start - chrono::TimeDelta::seconds(5)),
            Some((580, 580))
        );
        assert_eq!(
            agent_seconds_after(&run, end + chrono::TimeDelta::seconds(5)),
            Some((0, 580))
        );
        // An unfinished attempt means the timing is not known.
        run.attempts = vec![attempt(None)];
        assert_eq!(agent_seconds_after(&run, invalid), None);
        run.attempts.clear();
        assert_eq!(agent_seconds_after(&run, invalid), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_before_handoff_spawns_nothing() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        for point in [2] {
            let temp = tempfile::tempdir()?;
            let state = State::discover(Some(temp.path().join("state")))?;
            state.initialize()?;
            let project = temp.path().join("project");
            fs::create_dir(&project)?;
            fs::write(project.join("original.txt"), "original")?;
            let agent = temp.path().join("codex");
            let marker = temp.path().join("spawned");
            fs::write(
                &agent,
                format!(
                    "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo fixture; exit 0; fi\nif [ \"$1\" = 'app-server' ]; then\nwhile IFS= read -r line; do\ncase \"$line\" in\n*'\"id\":0'*) printf '%s\\n' '{{\"id\":0,\"result\":{{\"userAgent\":\"fixture\"}}}}' ;;\n*'\"id\":1'*) printf '%s\\n' '{{\"id\":1,\"result\":{{\"account\":{{\"type\":\"chatgpt\",\"planType\":\"plus\",\"email\":\"fixture@example.invalid\"}}}}}}' ;;\n*'\"id\":2'*) printf '%s\\n' '{{\"id\":2,\"result\":{{\"rateLimitsByLimitId\":{{}}}}}}'; exit 0 ;;\nesac\ndone\nexit 0\nfi\ntouch '{}'\n",
                    marker.display()
                ),
            )?;
            fs::set_permissions(&agent, fs::Permissions::from_mode(0o755))?;
            fs::write(
                project.join("dispatch.yml"),
                format!(
                    "execution:\n  timeout_secs: 60\nchecks:\n  baseline: ['true']\n  verify: ['true']\nharnesses:\n  codex:\n    executable: '{}'\n",
                    agent.display()
                ),
            )?;
            fs::write(
                state.root.join("resources.yml"),
                "version: 1\nallocation_enabled: true\nprofiles:\n  - provider: openai\n    funding_source: chatgpt-plus\n    harness: codex\n    model: fixture\n    effort: medium\n    runtime: local\n    service_mode: standard\n    pool: pool\n    provider_buckets: [codex]\n    tier: standard\n    included: true\n    no_overage_verified: true\n    authorization_revision: 1\n    codex_account: {\"account_sha256\":\"cc6d96611cffa9f02c3626f0b9ee897dc171e2d540a5cae349d4ec316104997b\",\"checked_at\":\"2026-01-01T00:00:00Z\"}\n",
            )?;
            let db = Database::open(state.db_path())?;
            let result = CANCEL_AT_HANDOFF
                .scope(
                    point,
                    run_dispatch(
                        &state,
                        RunRequest {
                            source: project,
                            task: "Deterministic handoff fixture".into(),
                            agent: Some("codex".into()),
                            model: Some("fixture".into()),
                            effort: Some("medium".into()),
                            config_path: None,
                            backend: None,
                            timeout_secs: None,
                            allow_unsafe_local: true,
                            allow_forwarded_env: false,
                            output: RunOutputMode::Json,
                            refreshed_from: None,
                        },
                    ),
                )
                .await;
            // The run is stopped and its result delivered, never launched.
            let run = result?;
            assert_eq!(run.outcome.lifecycle, LifecycleState::Finished);
            assert!(
                run.execution.as_ref().unwrap().failure.is_some(),
                "fault point {point} must stop the run"
            );
            assert!(run.attempts.iter().all(|a| a.completed_at.is_some()));
            assert!(!marker.exists(), "fault point {point} spawned a model");
            let launches: i64 =
                db.connection()
                    .query_row("SELECT COUNT(*) FROM attempt_launches", [], |r| r.get(0))?;
            assert_eq!(launches, 0, "fault point {point} recorded a launch");
        }
        Ok(())
    }

    #[test]
    fn duration_format_is_compact() {
        assert_eq!(format_duration(1_234), "1.2s");
        assert_eq!(format_duration(125_000), "2m05s");
    }

    #[test]
    fn timed_out_candidate_reports_verification_not_run() {
        let candidate = CandidateRecord {
            id: "candidate".into(),
            label: "A".into(),
            harness_id: "fake-timeout".into(),
            harness_version: None,
            model: None,
            status: CandidateStatus::TimedOut,
            workspace_path: PathBuf::new(),
            prompt_path: PathBuf::new(),
            stdout_path: PathBuf::new(),
            stderr_path: PathBuf::new(),
            diff_path: PathBuf::new(),
            duration_ms: 1_000,
            exit_code: None,
            timed_out: true,
            tokens: None,
            token_semantics: None,
            cost_usd: None,
            error: None,
            diff_stats: DiffStats::default(),
            checks: vec![],
        };

        assert_eq!(
            verification_summary(&candidate),
            "not run — candidate timed out"
        );
    }

    #[test]
    fn validates_structured_reasons_and_deduplicates() {
        let reasons = normalize_reasons(vec![
            " Correctness ".into(),
            "correctness".into(),
            "tests".into(),
        ])
        .unwrap();
        assert_eq!(reasons, vec!["correctness", "tests"]);
        let error = normalize_reasons(vec!["vibes".into()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("valid reasons:"));
        assert!(error.contains("edge-cases, performance"));
        assert!(normalize_reasons(vec!["cleaner-change".into()]).is_err());
    }

    #[test]
    fn init_generates_valid_config_with_verification_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let state = State {
            root: temp.path().join("state"),
        };

        init(&state, &project, false).unwrap();
        let contents = fs::read_to_string(project.join("dispatch.yml")).unwrap();
        assert!(contents.contains("Replace [] with your project's verification commands"));
        let config: Config = serde_yaml::from_str(&contents).unwrap();
        config.validate().unwrap();
        assert!(config.checks.verify.is_empty());
    }

    #[test]
    fn init_rejects_state_nested_in_project_before_writing() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();
        let state = State {
            root: project.join(".dispatch"),
        };

        let error = init(&state, &project, false).unwrap_err().to_string();
        assert!(error.contains("outside the project tree"));
        assert!(!project.join("dispatch.yml").exists());
        assert!(!state.root.exists());
    }

    #[cfg(unix)]
    #[test]
    fn init_refuses_configuration_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let outside = temp.path().join("outside.yml");
        fs::create_dir(&project).unwrap();
        symlink(&outside, project.join("dispatch.yml")).unwrap();
        let state = State {
            root: temp.path().join("state"),
        };

        let error = init(&state, &project, true).unwrap_err().to_string();
        assert!(error.contains("through symlink"));
        assert!(!outside.exists());
        assert!(!state.root.exists());
    }
}
