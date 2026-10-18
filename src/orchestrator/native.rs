//! The native engine: one agent attempt per drive, in an isolated workspace
//! made from the run's snapshot, with the coherence watcher, the durable
//! launch record, configured verification and clarification questions. Every
//! native run uses it, whether a configured profile or `--agent` chose the
//! agent; a profile-bound run also carries its funding contract.

use super::*;
use crate::{
    AttemptDetail, Clarification, FailureKind, GoalExecution, QuestionState,
    coherence::watch::{WatchMsg, WatchSpec, Watcher},
    config::MidRunMode,
};

pub(crate) fn local_uid() -> u32 {
    #[cfg(unix)]
    {
        unsafe { libc::geteuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

pub(super) struct DeadlineGuard(tokio::task::JoinHandle<()>);
impl DeadlineGuard {
    pub(super) fn new(
        policy: Option<&GoalExecution>,
        cancellation: CancellationToken,
    ) -> Option<Self> {
        let deadline = policy?.deadline_at;
        Some(Self(tokio::spawn(async move {
            tokio::time::sleep((deadline - Utc::now()).to_std().unwrap_or_default()).await;
            cancellation.cancel();
        })))
    }
}
impl Drop for DeadlineGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn expects_execution(run: &RunRecord) -> bool {
    run.execution.is_some()
        && run.outcome.lifecycle != LifecycleState::Finished
        && run.outcome.waiting_on != WaitingOn::Human
        && run.outcome.work_result != WorkResult::Cancelled
        && run.outcome.review != ReviewState::Rejected
}

/// Whether any agent this run launched may still be running.
fn any_launch_alive(db: &Database, id: &str) -> Result<bool> {
    Ok(crate::launch::launches_for_run(db, id)?
        .iter()
        .any(crate::launch::LaunchRecord::possibly_alive))
}

fn interrupt_run(state: &State, db: &Database, run: &mut RunRecord, reason: &str) -> Result<()> {
    run.status = RunStatus::Interrupted;
    run.completed_at = Some(Utc::now());
    run.outcome.lifecycle = LifecycleState::Finished;
    run.outcome.work_result = WorkResult::Interrupted;
    run.outcome.phase = RunPhase::Finished;
    run.outcome.waiting_on = if any_launch_alive(db, &run.id)? {
        WaitingOn::Reconciliation
    } else {
        WaitingOn::None
    };
    run.execution
        .as_mut()
        .unwrap()
        .failure
        .get_or_insert(FailureKind::InternalState);
    transition(
        state,
        db,
        run,
        "run.interrupted",
        serde_json::json!({"reason":reason}),
    )
}

// Called only after this foreground owner's work/guards have returned. Read
// the committed projection so a publication error cannot rewrite evidence.
pub(super) fn finish_error<T>(
    state: &State,
    db: &Database,
    id: &str,
    result: Result<T>,
) -> Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            let repair = (|| -> Result<()> {
                if let Some(mut run) = db.committed_run_projection(id)?
                    && expects_execution(&run)
                {
                    interrupt_run(
                        state,
                        db,
                        &mut run,
                        &format!("foreground execution stopped: {error:#}"),
                    )?;
                }
                Ok(())
            })();
            match repair {
                Ok(()) => Err(error),
                Err(repair) => {
                    Err(error.context(format!("could not persist interruption: {repair:#}")))
                }
            }
        }
    }
}

pub(crate) fn repair_abandoned(state: &State, db: &Database, run: &mut RunRecord) -> Result<()> {
    if !expects_execution(run) {
        return Ok(());
    }
    let Ok(_lock) = OperationLock::acquire(
        &state.run_dir(&run.id).join(".operation.lock"),
        "run is supervised",
    ) else {
        return Ok(());
    };
    // The owner may have committed a question, delivery or new supervisor
    // since the caller read its projection. Re-read while holding the lock.
    let Some(current) = db.committed_run_projection(&run.id)? else {
        return Ok(());
    };
    *run = current;
    // A run recorded without a supervisor is never presumed abandoned.
    let supervisor = run.execution.as_ref().and_then(|p| p.supervisor.clone());
    let gone = supervisor.as_ref().is_some_and(|owner| {
        matches!(
            crate::process::identity_state(owner),
            crate::process::IdentityState::Gone | crate::process::IdentityState::Reused
        )
    });
    // Completed attempts affirm that verification has also returned. An
    // unfinished attempt, or a launch that may still be alive, can still be
    // writing; leave that uncertainty to supervision and reconciliation.
    let completed =
        !run.attempts.is_empty() && run.attempts.iter().all(|a| a.completed_at.is_some());
    if expects_execution(run) && gone && completed && !any_launch_alive(db, &run.id)? {
        interrupt_run(
            state,
            db,
            run,
            "foreground supervisor is gone; work was not replayed",
        )?;
    }
    Ok(())
}

pub(super) fn sync_transition(
    state: &State,
    db: &mut Database,
    run: &mut RunRecord,
    kind: &str,
    payload: serde_json::Value,
) -> Result<()> {
    let event = db.commit_run_transition(
        run,
        EventRecord {
            run_id: run.id.clone(),
            attempt_id: run.attempts.last().map(|a| a.id.clone()),
            event_type: kind.into(),
            timestamp: Utc::now(),
            payload,
            ..Default::default()
        },
    )?;
    publish_event(state, event, run)
}

pub(super) fn emit(state: &State, run: &RunRecord, output: RunOutputMode) -> Result<()> {
    match output {
        RunOutputMode::Silent => {}
        RunOutputMode::Json => println!("{}", serde_json::to_string(&run_result(run))?),
        RunOutputMode::Jsonl => println!(
            "{}",
            serde_json::json!({"type":"result","result":run_result(run)})
        ),
        RunOutputMode::Human => {
            if let Some(q) = run
                .execution
                .as_ref()
                .and_then(|p| p.questions.last())
                .filter(|q| q.state == QuestionState::Pending)
            {
                println!(
                    "{}\nQuestion {} (revision {}) — answer with dispatch answer {} {} --revision {} --answer <text>",
                    q.report.question, q.id, q.revision, run.id, q.id, q.revision
                );
            } else if !run.candidates.is_empty() {
                let human = match run.outcome.review {
                    ReviewState::Accepted => Some(crate::ReviewOutcome::Accepted),
                    ReviewState::Rejected => Some(crate::ReviewOutcome::Rejected),
                    _ => None,
                };
                let heading = match run.outcome.work_result {
                    _ if human.is_some() => "Reviewed",
                    WorkResult::Ready => ready_heading(run),
                    _ => "Work stopped; inspect verification and attempt details",
                };
                print_single_result_summary(state, run, heading, false, human.as_ref());
            } else {
                println!(
                    "Work stopped: {}",
                    stopped_because(run.execution.as_ref().and_then(|p| p.failure))
                );
            }
            for attempt in &run.attempts {
                println!(
                    "  Attempt {}: {} / {} — {}{}",
                    attempt.ordinal,
                    attempt.resolved_model.as_deref().unwrap_or("unknown"),
                    attempt.resolved_effort.as_deref().unwrap_or("unknown"),
                    attempt.outcome,
                    attempt
                        .detail
                        .reason
                        .as_ref()
                        .map(|r| format!(" ({r})"))
                        .unwrap_or_default()
                );
            }
            println!(
                "{} attempt(s); final attempt: {}",
                run.attempts.len(),
                run.execution
                    .as_ref()
                    .and_then(|p| p.final_attempt_id.as_deref())
                    .unwrap_or("none")
            );
        }
    }
    Ok(())
}

/// Why work stopped before any result, in words.
fn stopped_because(failure: Option<FailureKind>) -> String {
    match failure {
        Some(FailureKind::BaselineInfrastructure) => {
            "the project's checks could not run on its starting point (S0), so the agent was not started".into()
        }
        Some(other) => serde_json::to_value(other)
            .ok()
            .and_then(|value| value.as_str().map(|name| name.replace('_', " ")))
            .unwrap_or_else(|| "for an unrecorded reason".into()),
        None => "no failure was recorded".into(),
    }
}

pub(super) fn stop(
    state: &State,
    db: &mut Database,
    run: &mut RunRecord,
    failure: FailureKind,
    reason: &str,
) -> Result<()> {
    if RUN_OUTPUT_MODE.load(Ordering::Relaxed) != RunOutputMode::Silent.code() {
        eprintln!("{reason}");
    }
    run.execution.as_mut().unwrap().failure = Some(failure);
    run.completed_at = Some(Utc::now());
    run.status = match failure {
        FailureKind::CapacityAdmission => RunStatus::Deferred,
        FailureKind::StaleWork => RunStatus::Interrupted,
        _ => RunStatus::Failed,
    };
    run.outcome.lifecycle = LifecycleState::Finished;
    run.outcome.work_result = match failure {
        FailureKind::Cancelled | FailureKind::StaleWork => WorkResult::Cancelled,
        FailureKind::CapacityAdmission => WorkResult::Deferred,
        _ => WorkResult::Failed,
    };
    run.outcome.phase = RunPhase::Finished;
    run.outcome.review = ReviewState::NotRequested;
    run.outcome.waiting_on = if any_launch_alive(db, &run.id)? {
        WaitingOn::Reconciliation
    } else {
        WaitingOn::None
    };
    if let Some(attempt) = run.attempts.last_mut().filter(|a| a.completed_at.is_none()) {
        attempt.completed_at = run.completed_at;
        let launched = crate::launch::launches_for_run(db, &attempt.run_id)?
            .iter()
            .any(|launch| {
                launch.attempt_id == attempt.id
                    && launch.state != crate::launch::LaunchState::SpawnFailed
            });
        attempt.outcome = if !launched {
            "not_launched"
        } else {
            "interrupted"
        }
        .into();
        attempt.detail.failure = Some(failure);
    }
    sync_transition(
        state,
        db,
        run,
        "run.stopped",
        serde_json::json!({"failure":failure,"reason":reason}),
    )
}

pub(super) fn interrupted(run: &RunRecord) -> FailureKind {
    if Utc::now() >= run.execution.as_ref().unwrap().deadline_at {
        FailureKind::Deadline
    } else {
        FailureKind::Cancelled
    }
}

/// Like `interrupted`, for a cancellation that the coherence watcher itself
/// requested (`stale`): the deadline still wins, otherwise the work is stale.
fn interrupted_by(run: &RunRecord, stale: bool) -> FailureKind {
    match interrupted(run) {
        FailureKind::Cancelled if stale => FailureKind::StaleWork,
        failure => failure,
    }
}

/// The next verdict from the attempt's watcher; never ready without one.
async fn next_watch_message(watcher: &mut Option<Watcher>) -> Option<WatchMsg> {
    match watcher {
        Some(watcher) => watcher.rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Apply one watcher verdict on the owning loop: remember it on the run and
/// persist it as an event. In `stop` mode (`stale` is `Some` only while the
/// attempt is running) a verdict that warrants it also cancels the run.
fn apply_watch(
    state: &State,
    db: &Database,
    run: &mut RunRecord,
    config: &Config,
    cancellation: &CancellationToken,
    message: WatchMsg,
    stale: Option<&mut Option<String>>,
) -> Result<()> {
    let validity = message.validity;
    apply::persist_verdict(state, db, run, &validity)?;
    let policy = &config.coherence;
    let stops = validity.decision == Decision::Stop
        || (validity.decision == Decision::Refresh && policy.stop_on_refresh);
    if let Some(stale) = stale
        && policy.mid_run == MidRunMode::Stop
        && stops
    {
        transition(
            state,
            db,
            run,
            "coherence.stopped",
            serde_json::json!({"coherence": validity}),
        )?;
        *stale = Some(
            validity
                .reasons
                .first()
                .map_or_else(|| "no detail".into(), |reason| reason.detail.clone()),
        );
        cancellation.cancel();
    }
    Ok(())
}

/// Resolve simple check programs in the same shell/backend as verification,
/// without executing the checks. Compound shell scripts resolve their shell;
/// their inner programs can only be diagnosed when verification runs.
pub(super) async fn resolve_check_programs(
    state: &State,
    run: &mut RunRecord,
    config: &Config,
    cancellation: CancellationToken,
) {
    let commands: Vec<_> = config
        .checks
        .baseline
        .iter()
        .chain(&config.checks.verify)
        .collect();
    let probes: Vec<String> = commands
        .iter()
        .map(|command| {
            format!(
                "command -v {} >/dev/null || exit 127",
                shell_words::quote(&probed_program(command))
            )
        })
        .collect();
    let results = run_checks_with_observer(
        &run.baseline_path,
        &probes,
        CheckPhase::Baseline,
        &state.run_dir(&run.id).join("checks/programs"),
        config.execution.clone(),
        cancellation,
        None,
        None,
    )
    .await;
    run.baseline_checks = results
        .into_iter()
        .zip(commands)
        .filter_map(|(mut check, command)| {
            if check.status == CheckStatus::Passed {
                return None;
            }
            check.command = command.clone();
            Some(check)
        })
        .collect();
}

/// The program a check command's lookup probes: its first word, or the
/// shell for anything that is shell syntax rather than a program.
fn probed_program(command: &str) -> String {
    let words = shell_words::split(command).unwrap_or_default();
    let program = words.first().map(String::as_str).unwrap_or("/bin/sh");
    if program.contains('=')
        || program.contains([';', '&', '|', '<', '>', '(', ')', '$', '`', '\n'])
        || matches!(
            program,
            "!" | "if" | "for" | "while" | "until" | "case" | "{"
        )
    {
        "/bin/sh".into()
    } else {
        program.into()
    }
}

/// What stops a run before its agent: a check program that cannot be
/// found, or a baseline check that could not run at all.
fn unrunnable_baseline(run_dir: &Path, checks: &[crate::CheckResult]) -> Option<String> {
    let check = checks.iter().find(|c| {
        !matches!(c.status, CheckStatus::Passed | CheckStatus::Failed)
            || matches!(c.exit_code, Some(126 | 127))
            || (c.status == CheckStatus::Failed && c.exit_code.is_none())
    })?;
    let how = match (&check.status, check.exit_code) {
        (CheckStatus::TimedOut, _) => "timed out".to_owned(),
        (_, Some(code)) => format!("exit {code}"),
        (_, None) => "no exit status".to_owned(),
    };
    let command = &check.command;
    Some(
        if check
            .stdout_path
            .starts_with(run_dir.join("checks/programs"))
        {
            if check.exit_code == Some(127) {
                format!(
                    "check program not found: {} (from `{command}`); the agent was not started",
                    probed_program(command)
                )
            } else {
                format!(
                    "check program could not be looked up for `{command}` ({how}); the agent was not started"
                )
            }
        } else {
            format!("baseline check could not run: `{command}` ({how}); the agent was not started")
        },
    )
}

/// The default S0 verification path, without `checks.baseline`: only
/// normally failing candidate commands need a baseline result to
/// distinguish a target defect from a broken S0.
async fn check_failed_baseline(
    state: &State,
    db: &Database,
    run: &mut RunRecord,
    candidate: &CandidateRecord,
    config: &Config,
    cancellation: CancellationToken,
) -> Result<()> {
    let commands: Vec<_> = candidate
        .checks
        .iter()
        .filter(|c| c.status == CheckStatus::Failed && matches!(c.exit_code, Some(1..=125)))
        .map(|c| c.command.clone())
        .collect();
    if commands.is_empty() {
        return Ok(());
    }
    check_on_s0(state, db, run, &commands, config, cancellation).await
}

/// Run `commands` on a fresh copy of S0, recording each check's lifecycle,
/// as `run.baseline_checks`.
pub(super) async fn check_on_s0(
    state: &State,
    db: &Database,
    run: &mut RunRecord,
    commands: &[String],
    config: &Config,
    cancellation: CancellationToken,
) -> Result<()> {
    let dir = state.run_dir(&run.id);
    let workspace = source::create_candidate_workspace(
        &run.baseline_path,
        &dir.join("baseline-check/workspace"),
    )?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let output_dir = dir.join("checks/baseline");
    let checks = run_checks_with_observer(
        &workspace,
        commands,
        CheckPhase::Baseline,
        &output_dir,
        config.execution.clone(),
        cancellation.clone(),
        Some(tx),
        None,
    );
    tokio::pin!(checks);
    run.baseline_checks = loop {
        tokio::select! {
            results = &mut checks => break results,
            Some(event) = rx.recv() => {
                if let Err(error) = persist_check_lifecycle(state, db, run, event) {
                    cancellation.cancel();
                    let _ = (&mut checks).await;
                    return Err(error);
                }
            }
        }
    };
    while let Ok(event) = rx.try_recv() {
        persist_check_lifecycle(state, db, run, event)?;
    }
    Ok(())
}

pub(super) fn verification_failure(
    run: &RunRecord,
    candidate: &CandidateRecord,
) -> Option<FailureKind> {
    // Required checks must all be evaluable before any target failure can
    // justify spending. Missing exits include signals and failed spawns.
    if candidate
        .checks
        .iter()
        .any(|c| matches!(c.exit_code, Some(126 | 127)) || c.status == CheckStatus::TimedOut)
    {
        return Some(FailureKind::VerificationInfrastructure);
    }
    if candidate.checks.iter().any(|c| {
        !matches!(
            (&c.status, c.exit_code),
            (CheckStatus::Passed, Some(0)) | (CheckStatus::Failed, Some(1..=125))
        )
    }) {
        return Some(FailureKind::VerificationUnknown);
    }
    let failed = candidate
        .checks
        .iter()
        .filter(|c| c.status == CheckStatus::Failed);
    failed.clone().next()?;
    if run
        .baseline_checks
        .iter()
        .all(|b| b.status == CheckStatus::Passed)
        && failed.clone().any(|c| {
            run.baseline_checks
                .iter()
                .any(|b| b.command == c.command && b.status == CheckStatus::Passed)
        })
    {
        Some(FailureKind::TargetVerification)
    } else {
        Some(FailureKind::BaselineInfrastructure)
    }
}

pub(super) fn diagnostics(attempt: &AttemptRecord) -> String {
    let Some(candidate) = &attempt.detail.result else {
        return String::new();
    };
    let mut text = format!(
        "Previous attempt {} restarted from the same original baseline. Its workspace is retained and is not your input.\n",
        attempt.id
    );
    for check in candidate
        .checks
        .iter()
        .filter(|c| c.status != CheckStatus::Passed)
    {
        text.push_str(&format!("Failed check: {}\n", check.command));
        for path in [&check.stdout_path, &check.stderr_path] {
            let mut bytes = Vec::new();
            if let Ok(file) = fs::File::open(path) {
                let _ = file.take(4096).read_to_end(&mut bytes);
            }
            text.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
    text.truncate(text.floor_char_boundary(16_384.min(text.len())));
    text
}

fn prepare(
    state: &State,
    run: &mut RunRecord,
    decision: Option<AllocationDecision>,
    reason: Option<String>,
) -> Result<CandidateRecord> {
    let mut prompt = run.exact_prompt.clone();
    if let Some(parent) = run.attempts.last() {
        prompt.push_str(&format!("\n\n{}", diagnostics(parent)));
    }
    if let Some(question) = run
        .execution
        .as_ref()
        .unwrap()
        .questions
        .last()
        .filter(|q| q.state == QuestionState::Answered)
    {
        prompt.push_str(&format!(
            "\nClarification: {}\nAuthorized answer: {}\n",
            question.report.question,
            question.answer.as_deref().unwrap_or_default()
        ));
    }
    prompt.push_str("\nIf an essential ambiguity prevents safe completion, stop work and emit exactly this JSON as your final agent message: {\"dispatch_checkpoint\":{\"version\":1,\"question\":\"the essential question\",\"choices\":[]}}. You may add \"category\":\"factual\" inside dispatch_checkpoint only for factual task clarification. Omit category for funding, unsafe execution, permissions, or human review decisions. Do not wait in a live process. Otherwise complete the task normally.\n");
    let input = run.baseline_path.clone();
    prepare_input(state, run, decision, reason, &input, &prompt)
}

pub(super) fn prepare_input(
    state: &State,
    run: &mut RunRecord,
    decision: Option<AllocationDecision>,
    reason: Option<String>,
    input: &Path,
    prompt: &str,
) -> Result<CandidateRecord> {
    // A profile-bound attempt runs the profile's resource; an explicit agent
    // runs its project configuration.
    let goal = run
        .execution
        .as_ref()
        .context("native run without execution policy")?;
    let (harness, model, effort) = match &decision {
        Some(decision) => (
            decision.selected.harness.clone(),
            Some(decision.selected.resolved_model.clone()),
            decision.selected.effort.clone(),
        ),
        None => (
            goal.fixed_harness
                .clone()
                .context("native run without an agent")?,
            goal.fixed_model.clone(),
            goal.fixed_effort.clone(),
        ),
    };
    let id = Ulid::new().to_string();
    let dir = state.run_dir(&run.id).join("attempts").join(&id);
    let workspace = source::create_candidate_workspace(input, &dir.join("workspace"))?;
    let candidate = CandidateRecord {
        id: id.clone(),
        label: "A".into(),
        harness_id: harness,
        harness_version: None,
        model: None,
        status: CandidateStatus::Preparing,
        workspace_path: workspace,
        prompt_path: dir.join("prompt.txt"),
        stdout_path: dir.join("stdout.log"),
        stderr_path: dir.join("stderr.log"),
        diff_path: dir.join("diff.patch"),
        duration_ms: 0,
        exit_code: None,
        timed_out: false,
        tokens: None,
        token_semantics: None,
        cost_usd: None,
        error: None,
        diff_stats: DiffStats::default(),
        checks: vec![],
    };
    write_text(&candidate.prompt_path, prompt)?;
    run.attempts.push(AttemptRecord {
        detail: AttemptDetail {
            parent_attempt_id: run.attempts.last().map(|a| a.id.clone()),
            reason,
            input_baseline: Some(input.to_path_buf()),
            decision: decision.clone(),
            ..Default::default()
        },
        id,
        run_id: run.id.clone(),
        candidate_id: candidate.id.clone(),
        role: "executor".into(),
        ordinal: (run.attempts.len() + 1) as u32,
        generation: 1,
        harness_id: candidate.harness_id.clone(),
        harness_version: None,
        requested_model: model.clone(),
        resolved_model: model,
        observed_model: None,
        requested_effort: effort.clone(),
        resolved_effort: effort,
        observed_effort: None,
        started_at: Utc::now(),
        completed_at: None,
        outcome: "preparing".into(),
        raw_telemetry_path: dir.join("harness.jsonl"),
        resource: decision.map(|decision| decision.selected),
    });
    run.candidates = vec![candidate.clone()];
    Ok(candidate)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn drive(
    state: &State,
    db: &mut Database,
    run: RunRecord,
    config: Config,
    resources: crate::config::ResourceConfig,
    cancellation: CancellationToken,
    output: RunOutputMode,
) -> Result<RunRecord> {
    let id = run.id.clone();
    let result = drive_inner(state, db, run, config, resources, cancellation, output).await;
    finish_error(state, db, &id, result)
}

#[allow(clippy::too_many_arguments)]
async fn drive_inner(
    state: &State,
    db: &mut Database,
    mut run: RunRecord,
    mut config: Config,
    resources: crate::config::ResourceConfig,
    cancellation: CancellationToken,
    output: RunOutputMode,
) -> Result<RunRecord> {
    anyhow::ensure!(
        run.outcome.review != ReviewState::Rejected
            && run.outcome.work_result != WorkResult::Cancelled,
        "closed work cannot continue"
    );
    let initial = run.attempts.is_empty();
    if initial
        && let Some(reason) = unrunnable_baseline(&state.run_dir(&run.id), &run.baseline_checks)
    {
        stop(
            state,
            db,
            &mut run,
            FailureKind::BaselineInfrastructure,
            &reason,
        )?;
        emit(state, &run, output)?;
        return Ok(run);
    }
    // A run bound to a configured profile carries a funding contract; an
    // explicit `--agent` run without a profile has none.
    let decision = if run.allocation.is_none() {
        None
    } else if initial {
        run.allocation.clone()
    } else {
        Some(
            run.attempts
                .last()
                .unwrap()
                .detail
                .decision
                .clone()
                .context("continuation route missing")?,
        )
    };
    let mut reason = (!initial).then(|| "clarification_answer".to_owned());
    'attempt: {
        if cancellation.is_cancelled() || Utc::now() >= run.execution.as_ref().unwrap().deadline_at
        {
            let failure = interrupted(&run);
            stop(
                state,
                db,
                &mut run,
                failure,
                "goal cancelled or deadline expired",
            )?;
            break 'attempt;
        }
        if run.attempts.len() >= run.execution.as_ref().unwrap().max_invocations.min(2) as usize {
            stop(
                state,
                db,
                &mut run,
                FailureKind::InvocationLimit,
                "goal invocation limit reached",
            )?;
            break 'attempt;
        }
        // A source that moved since S0 does not stop an attempt, including the
        // continuation after an answer: every attempt starts from the run's
        // original snapshot, the mid-run watcher reports what the move means
        // for the work, and the accept gate judges the result against the
        // source as it is then.
        let profile = match &decision {
            None => None,
            Some(decision) => {
                let selected = &decision.selected;
                let Some(profile) = resources.profiles.iter().find(|p| {
                    p.provider == selected.provider
                        && p.funding_source == selected.funding_source
                        && p.pool == selected.pool
                        && p.model == selected.resolved_model
                        && p.effort == selected.effort
                        && p.harness == selected.harness
                        && p.enabled
                        && p.runtime == selected.runtime
                        && p.service_mode == selected.service_mode
                        && p.included
                        && p.no_overage_verified
                }) else {
                    stop(
                        state,
                        db,
                        &mut run,
                        FailureKind::Authorization,
                        "selected route is no longer authorized by the current resource configuration",
                    )?;
                    break 'attempt;
                };
                let profile = profile.clone();
                if let Some(refusal) =
                    db.funding_refusal(&profile.funding_key(), profile.authorization_revision)?
                {
                    let message = super::funding_refused_message(&profile, &refusal);
                    stop(state, db, &mut run, FailureKind::Authorization, &message)?;
                    break 'attempt;
                }
                Some(profile)
            }
        };
        if let Some(decision) = &decision {
            config.harnesses.bind(&decision.selected, &resources);
        }
        run.execution.as_mut().unwrap().final_attempt_id = None;
        run.execution.as_mut().unwrap().failure = None;
        let candidate = prepare(state, &mut run, decision.clone(), reason.take())?;
        run.status = RunStatus::Running;
        run.completed_at = None;
        run.outcome.lifecycle = LifecycleState::Working;
        run.outcome.work_result = WorkResult::Pending;
        run.outcome.review = ReviewState::NotRequested;
        run.outcome.waiting_on = WaitingOn::None;
        run.outcome.verification = VerificationState::NotRun;
        run.outcome.phase = RunPhase::Executing;
        db.sync_run(&run)?;
        #[cfg(test)]
        if CANCEL_AT_HANDOFF.try_with(|p| *p == 2).unwrap_or(false) {
            cancellation.cancel();
        }
        // Like the check before the attempt: record the stop and deliver the
        // result, so `--json` always gets the run and the deadline's exit code.
        if cancellation.is_cancelled() {
            let failure = interrupted(&run);
            stop(state, db, &mut run, failure, "cancelled before handoff")?;
            break 'attempt;
        }
        let attempt_id = update_attempt_started(&mut run, &candidate.id);
        db.sync_run(&run)?;
        let payload = serde_json::json!({"reason":run.attempts.last().unwrap().detail.reason});
        transition(state, db, &mut run, "attempt.started", payload)?;
        let prompt = fs::read_to_string(&candidate.prompt_path)?;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // The watcher lives exactly as long as this attempt: it is finished
        // below when the attempt returns, and aborted if an error leaves early.
        let mut watcher = candidate.prompt_path.parent().map(|dir| {
            Watcher::spawn(WatchSpec {
                source: run.source_path.clone(),
                kind: run.source_kind.clone(),
                baseline: run.baseline_path.clone(),
                baseline_commit: run.baseline_commit.clone(),
                workspace: candidate.workspace_path.clone(),
                delta_patch: dir.join("delta-live.patch"),
                poll: Duration::from_secs(config.coherence.poll_secs),
            })
        });
        let mut stale: Option<String> = None;
        let work = execute_candidate(
            candidate,
            prompt,
            config.clone(),
            run.baseline_path.clone(),
            cancellation.clone(),
            (tx, attempt_id),
            Some(super::LaunchContext {
                db_path: state.db_path(),
                run_id: run.id.clone(),
                guard: profile.clone().map(|profile| crate::launch::LaunchGuard {
                    state_root: state.root.clone(),
                    profile,
                }),
            }),
        );
        tokio::pin!(work);
        let execution = loop {
            tokio::select! {
                result=&mut work=>break result,
                Some(event)=rx.recv()=>{
                    if let Err(error)=persist_check_lifecycle(state,db,&mut run,event) {
                        cancellation.cancel(); let _=(&mut work).await;
                        return Err(error.context("failed to persist attempt verification"));
                    }
                }
                Some(message)=next_watch_message(&mut watcher)=>{
                    if let Err(error)=apply_watch(state,db,&mut run,&config,&cancellation,message,Some(&mut stale)) {
                        cancellation.cancel(); let _=(&mut work).await;
                        return Err(error.context("failed to persist coherence verdict"));
                    }
                }
            }
        };
        if let Some(watcher) = watcher.as_mut() {
            watcher.finish().await;
            // A verdict sent just before the attempt ended is still evidence,
            // but it can no longer stop anything.
            while let Ok(message) = watcher.rx.try_recv() {
                apply_watch(state, db, &mut run, &config, &cancellation, message, None)?;
            }
        }
        drop(watcher);
        while let Ok(event) = rx.try_recv() {
            persist_check_lifecycle(state, db, &mut run, event)?;
        }
        // A refused preflight is sticky for this authorization revision.
        if let (Some(reason), Some(profile)) = (&execution.preflight_refusal, &profile) {
            db.record_funding_refusal(
                &profile.funding_key(),
                profile.authorization_revision,
                reason,
            )?;
        }
        let candidate = execution.candidate.clone();
        // An explicit `checks.baseline` already ran on S0 before the agent.
        if candidate.status == CandidateStatus::Completed
            && !cancellation.is_cancelled()
            && config.checks.baseline.is_empty()
        {
            check_failed_baseline(
                state,
                db,
                &mut run,
                &candidate,
                &config,
                cancellation.clone(),
            )
            .await?;
        }
        run.candidates = vec![candidate.clone()];
        refresh_outcome(&mut run);
        run.outcome.lifecycle = LifecycleState::Working;
        run.outcome.work_result = WorkResult::Pending;
        run.outcome.review = ReviewState::NotRequested;
        update_attempt_finished(&mut run, &candidate, &execution);
        // Only an execution failure stops the goal. A completed attempt whose
        // checks failed is delivered for review; its classification is kept.
        let stopped = if cancellation.is_cancelled() {
            Some(interrupted_by(&run, stale.is_some()))
        } else if !execution.cleanup_confirmed {
            Some(FailureKind::InternalState)
        } else if candidate.status != CandidateStatus::Completed {
            Some(execution.failure.unwrap_or(FailureKind::HarnessProcess))
        } else if execution.checkpoint.as_ref().is_some_and(|r| r.is_err()) {
            Some(FailureKind::UnsupportedCheckpoint)
        } else if execution.checkpoint.is_some()
            && run.attempts.len() >= run.execution.as_ref().unwrap().max_invocations.min(2) as usize
        {
            Some(FailureKind::InvocationLimit)
        } else {
            None
        };
        let failure = stopped.or_else(|| verification_failure(&run, &candidate));
        let last = run.attempts.last_mut().unwrap();
        last.detail.result = Some(candidate.clone());
        last.detail.failure = failure;
        let final_id = last.id.clone();
        // Persist completed evidence before any new attempt or question.
        db.sync_run(&run)?;
        transition(
            state,
            db,
            &mut run,
            "attempt.finished",
            serde_json::json!({"failure":failure}),
        )?;
        if let Some(failure) = stopped {
            let message = match (&stale, failure) {
                (Some(detail), FailureKind::StaleWork) => {
                    format!("work stopped: the source changed underneath it ({detail})")
                }
                _ => candidate
                    .error
                    .clone()
                    .unwrap_or_else(|| "attempt did not safely complete".into()),
            };
            stop(state, db, &mut run, failure, &message)?;
            break 'attempt;
        }
        if let Some(Ok(report)) = execution.checkpoint {
            let question = Clarification {
                id: Ulid::new().to_string(),
                run_id: run.id.clone(),
                attempt_id: final_id,
                generation: 1,
                revision: 1,
                report,
                state: QuestionState::Pending,
                answer: None,
                created_at: Utc::now(),
                resolved_at: None,
                actor_uid: None,
                actor: None,
            };
            run.execution.as_mut().unwrap().questions.push(question);
            run.outcome.lifecycle = LifecycleState::Waiting;
            run.outcome.waiting_on = WaitingOn::Human;
            run.outcome.work_result = WorkResult::Pending;
            run.outcome.verification = VerificationState::NotRun;
            let payload =
                serde_json::json!({"question":run.execution.as_ref().unwrap().questions.last()});
            transition(state, db, &mut run, "question.pending", payload)?;
            break 'attempt;
        }
        let policy = run.execution.as_mut().unwrap();
        policy.final_attempt_id = Some(final_id);
        policy.contributing_attempts = run.attempts.iter().map(|a| a.id.clone()).collect();
        if run.attempts.len() > 1 {
            policy.provenance = "mixed_attempt_context".into();
            let delivery = &mut run.candidates[0];
            delivery.harness_id = "mixed".into();
            delivery.harness_version = None;
            delivery.model = None;
            delivery.tokens = None;
            delivery.cost_usd = None;
            delivery.token_semantics = Some("see_per_attempt".into());
        }
        refresh_outcome(&mut run);
        run.execution.as_mut().unwrap().failure = failure;
        run.completed_at = Some(Utc::now());
        run.status = if run.outcome.work_result == WorkResult::Ready {
            RunStatus::ReadyForEvaluation
        } else {
            RunStatus::Failed
        };
        let payload = serde_json::json!({"outcome":run.outcome});
        sync_transition(state, db, &mut run, "run.finished", payload)?;
        break 'attempt;
    }
    if output == RunOutputMode::Human {
        // What holds now, as finish shows it for attached Work.
        let shown = crate::coherence::with_kept_validity(&run, &state.run_dir(&run.id));
        emit(state, &shown, output)?;
    } else {
        emit(state, &run, output)?;
    }
    Ok(run)
}

/// Local typed commands are authorized by the OS identity owning the run.
/// There is no caller-supplied actor or remote transport.
#[derive(Debug)]
pub struct QuestionCommand {
    pub run_id: String,
    pub question_id: String,
    pub revision: u64,
    pub generation: u32,
}

fn resolve(
    state: &State,
    command: &QuestionCommand,
    answer: Option<String>,
) -> Result<(RunRecord, OperationLock)> {
    let id = state.resolve_run_id(&command.run_id)?;
    let lock = OperationLock::acquire(
        &state.run_dir(&id).join(".operation.lock"),
        "run has a foreground owner",
    )?;
    let mut run = state.load_run(&id)?;
    let policy = run
        .execution
        .as_mut()
        .context("run has no clarification policy")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            fs::metadata(state.run_dir(&id))?.uid() == local_uid(),
            "local caller does not own the run"
        );
    }
    anyhow::ensure!(policy.owner_uid == local_uid(), "unauthorized local caller");
    anyhow::ensure!(
        run.outcome.lifecycle == LifecycleState::Waiting
            && run.outcome.waiting_on == WaitingOn::Human
            && run.outcome.review != ReviewState::Rejected
            && run.outcome.work_result != WorkResult::Cancelled,
        "run is not awaiting an answer"
    );
    let question = policy
        .questions
        .iter_mut()
        .find(|q| q.id == command.question_id)
        .context("wrong question")?;
    anyhow::ensure!(
        question.run_id == command.run_id
            && question.revision == command.revision
            && question.generation == command.generation
            && question.state == QuestionState::Pending,
        "stale, duplicate, or wrong-run question command"
    );
    if let Some(value) = &answer {
        anyhow::ensure!(
            !value.trim().is_empty() && value.len() <= 16_384,
            "invalid answer"
        );
    }
    question.state = if answer.is_some() {
        QuestionState::Answered
    } else {
        QuestionState::Cancelled
    };
    question.answer = answer;
    question.revision += 1;
    question.resolved_at = Some(Utc::now());
    question.actor_uid = Some(local_uid());
    question.actor = None;
    run.outcome.waiting_on = WaitingOn::None;
    if question.state == QuestionState::Cancelled {
        run.outcome.lifecycle = LifecycleState::Finished;
        run.outcome.work_result = WorkResult::Cancelled;
        run.outcome.phase = RunPhase::Finished;
        run.status = RunStatus::Interrupted;
        run.completed_at = Some(Utc::now());
        policy.failure = Some(FailureKind::Cancelled);
    } else {
        run.outcome.lifecycle = LifecycleState::Preparing;
        run.outcome.phase = RunPhase::Preparing;
        policy.supervisor = Some(crate::process::ProcessIdentity::current());
    }
    let db = Database::open(state.db_path())?;
    let result = transition(
        state,
        &db,
        &mut run,
        "question.resolved",
        serde_json::json!({"question_id":command.question_id,"revision":command.revision+1,"actor_uid":local_uid()}),
    );
    finish_error(state, &db, &run.id, result)?;
    Ok((run, lock))
}

pub async fn answer_question(
    state: &State,
    command: QuestionCommand,
    answer: String,
    output: RunOutputMode,
) -> Result<RunRecord> {
    RUN_OUTPUT_MODE.store(output.code(), Ordering::Relaxed);
    let (run, _lock) = resolve(state, &command, Some(answer))?;
    let mut db = Database::open(state.db_path())?;
    let setup = (|| -> Result<_> {
        let config: Config = serde_yaml::from_str(&fs::read_to_string(
            state.run_dir(&run.id).join("config.snapshot.yml"),
        )?)?;
        config.validate()?;
        Ok((config, crate::config::ResourceConfig::load(&state.root)?))
    })();
    let (config, resources) = finish_error(state, &db, &run.id, setup)?;
    let cancellation = operation_cancellation();
    let _signals = SignalListener::install(cancellation.clone());
    let _deadline = DeadlineGuard::new(run.execution.as_ref(), cancellation.clone());
    drive(state, &mut db, run, config, resources, cancellation, output).await
}

pub fn cancel_question(
    state: &State,
    command: QuestionCommand,
    output: RunOutputMode,
) -> Result<RunRecord> {
    RUN_OUTPUT_MODE.store(output.code(), Ordering::Relaxed);
    let (run, _lock) = resolve(state, &command, None)?;
    emit(state, &run, output)?;
    Ok(run)
}
