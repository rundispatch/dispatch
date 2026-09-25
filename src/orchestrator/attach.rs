//! `dispatch attach` (foreign form and wrapped form) and `dispatch finish`:
//! attached external work Dispatch did not select or route, as a `RunRecord`
//! with `mode: Attached`. See `docs/plan-0.3-auto-apply-and-attach.md`, parts
//! 6.1–6.9 and 14.3–14.10 (the design freeze this module implements exactly).
//!
//! The wrapped form (`dispatch attach -- <command...>`) is the owner loop of
//! its own Work (part 6.1): `create` it, spawn the agent under the terminal,
//! watch the integration world while it runs, and on exit finish and,
//! authorized, apply. `run_wrapped` is that loop.

use std::process::Stdio;

use tokio::signal::unix::{SignalKind, signal};

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use ulid::Ulid;

use super::{
    ApplyOutcome, apply, auto_apply, persist_event, publish_event, refresh_outcome,
    result_verification,
};
use crate::{
    ApplicationState, AttachCapabilities, AttachConfidence, AttachmentRecord, AttemptDetail,
    AttemptRecord, BaselineProvenance, CandidateRecord, CandidateStatus, CheckPhase, Config,
    DiffStats, EnvironmentRecord, EventRecord, FinishReason, LifecycleState, ManagedWorkspace,
    OwnerState, ReviewState, RunMode, RunOutcome, RunPhase, RunRecord, RunStatus, RuntimeSession,
    VerificationState, WaitingOn, WorkResult, WorkspaceOwner, WorkspaceRemoval,
    coherence::watch::{WatchSpec, Watcher},
    db::Database,
    lock::OperationLock,
    process, source,
    state::{State, write_text},
};

/// Request to attach external work Dispatch did not launch. See part 14.3.
pub struct AttachRequest {
    /// Default: the current directory.
    pub workspace: PathBuf,
    /// Default: the repository's main worktree when `workspace` is a linked
    /// Git worktree; required for a plain directory.
    pub root: Option<PathBuf>,
    pub task: Option<String>,
    pub agent: Option<String>,
    pub pid: Option<u32>,
    /// `Some` selects the wrapped form (`run_wrapped`): the argv Dispatch
    /// spawns and owns for the session.
    pub command: Option<Vec<String>>,
    pub allow_unsafe_local: bool,
    pub auto_apply: bool,
    /// `Some` when a runtime's session start registers the Work
    /// (`runtime::ingest`) rather than a person typing `attach`.
    pub runtime: Option<RuntimeStart>,
}

/// Work a runtime's session start registers: S0 is the workspace's exact
/// world now, before the session's first turn. A resume into a workspace
/// Dispatch has not seen may follow edits made before it, so it is partial.
pub struct RuntimeStart {
    pub session: RuntimeSession,
    pub resumed: bool,
}

/// `dispatch attach --workspace <path> ...` (foreign form): create a
/// `RunMode::Attached` run observing an already-running agent, without
/// spawning anything or touching the workspace. See part 14.8 for the
/// refusal order and exact messages.
pub fn create(state: &State, request: AttachRequest) -> Result<RunRecord> {
    let is_wrapped = request.command.is_some();

    let workspace = source::resolve_source(Some(&request.workspace))?;
    let root = match &request.root {
        Some(root) => source::resolve_source(Some(root))?,
        None => match source::repo_identity(&workspace)? {
            Some(identity) => identity.main_worktree,
            None => bail!("--root is required for a plain directory"),
        },
    };

    // Wrapped work started in the checkout itself gets a workspace Dispatch
    // makes (`make_workspace`); the checkout is never the workspace.
    let managed = is_wrapped && workspace == root;
    let runtime = request.runtime.as_ref();

    // 14.8, in order: same-checkout, different repository, no merge base,
    // plain-directory foreign attach, checks without acknowledgement,
    // already attached.
    anyhow::ensure!(
        managed || workspace != root,
        "attach needs a separate worktree; run git worktree add, or wrap the agent \
         (dispatch attach -- <agent>) and Dispatch makes one"
    );

    let workspace_identity = source::repo_identity(&workspace)?;
    let commit = match &workspace_identity {
        _ if managed || runtime.is_some() => None,
        Some(workspace_repo) => {
            let root_identity = source::repo_identity(&root)?;
            anyhow::ensure!(
                root_identity.is_some_and(|root_repo| root_repo.key == workspace_repo.key),
                "workspace belongs to a different repository than the integration root"
            );
            Some(
                source::merge_base(&root, &workspace)?
                    .context("no common history between the workspace and the integration root")?,
            )
        }
        None => {
            // Plain-directory S0 (part 6.4/14.3): the wrapped form snapshots
            // the workspace at attach time; a foreign attach has no honest
            // S0 for a plain directory and is refused.
            anyhow::ensure!(
                is_wrapped,
                "a plain directory can be attached only by wrapping the agent command"
            );
            None
        }
    };

    let (config, config_path) = Config::discover(&root, None)?;
    config.validate()?;
    // Runtime-registered Work carries no authority to run checks: a person
    // gives it at `dispatch finish`.
    anyhow::ensure!(
        config.checks.verify.is_empty() || request.allow_unsafe_local || runtime.is_some(),
        "finish runs your checks.verify on the host; pass --allow-unsafe-local"
    );
    let unsafe_local = request.allow_unsafe_local;

    if !managed && let Some(existing_id) = find_active_attachment(state, &workspace)? {
        bail!("workspace already attached as {existing_id}");
    }

    state.initialize()?;
    let mut db = Database::open(state.db_path())?;
    let run_id = Ulid::new().to_string();
    let run_dir = state.run_dir(&run_id);
    fs::create_dir(&run_dir)
        .with_context(|| format!("failed to create run directory {}", run_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o700))?;
    }

    let task = request.task.clone().unwrap_or_else(|| {
        let basename = workspace
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| workspace.display().to_string());
        format!("attached work in {basename}")
    });
    write_text(&run_dir.join("task.md"), &task)?;
    write_text(
        &run_dir.join("config.snapshot.yml"),
        &serde_yaml::to_string(&config).context("failed to serialize effective configuration")?,
    )?;

    let (snapshot, provenance, confidence, workspace, managed_workspace) = if managed {
        let (snapshot, provenance, workspace, made) =
            make_workspace(state, &root, &run_id, &run_dir).map_err(|error| {
                let _ = fs::remove_dir_all(&run_dir);
                error.context("could not make a workspace; incomplete run state was removed")
            })?;
        (
            snapshot,
            provenance,
            AttachConfidence::Full,
            workspace,
            Some(made),
        )
    } else if let Some(runtime) = runtime {
        let (snapshot, commit) = source::world_commit(&workspace)
            .and_then(|commit| {
                source::materialize_baseline_from_commit(&workspace, &commit, &run_dir)
                    .map(|snapshot| (snapshot, commit))
            })
            .map_err(|error| {
                let _ = fs::remove_dir_all(&run_dir);
                error.context("could not record S0; incomplete run state was removed")
            })?;
        let confidence = if runtime.resumed {
            AttachConfidence::Partial
        } else {
            AttachConfidence::Full
        };
        (
            snapshot,
            BaselineProvenance::WorkspaceAtStart { commit },
            confidence,
            workspace.clone(),
            None,
        )
    } else {
        let (snapshot, provenance, confidence) = match &commit {
            Some(commit) => {
                let snapshot = source::materialize_baseline_from_commit(&root, commit, &run_dir)
                    .map_err(|error| {
                        let _ = fs::remove_dir_all(&run_dir);
                        error.context("baseline creation failed; incomplete run state was removed")
                    })?;
                (
                    snapshot,
                    BaselineProvenance::GitMergeBase {
                        commit: commit.clone(),
                    },
                    AttachConfidence::Full,
                )
            }
            None => {
                let snapshot = source::create_snapshot(&workspace, &run_dir).map_err(|error| {
                    let _ = fs::remove_dir_all(&run_dir);
                    error.context("baseline creation failed; incomplete run state was removed")
                })?;
                (
                    snapshot,
                    BaselineProvenance::SnapshotAtAttach,
                    AttachConfidence::Partial,
                )
            }
        };
        (snapshot, provenance, confidence, workspace, None)
    };

    let (source_kind, source_git_head) = source::inspect_source(&root)?;
    let source_fingerprint = source::fingerprint_tree(&root)?;

    let now = Utc::now();
    let candidate_id = Ulid::new().to_string();
    let harness_id = request.agent.clone().unwrap_or_else(|| "external".into());
    let diff_path = run_dir.join("delta.patch");
    let stdout_path = run_dir.join("attach-stdout.log");
    let stderr_path = run_dir.join("attach-stderr.log");
    let raw_telemetry_path = run_dir.join("harness.jsonl");
    for path in [&diff_path, &stdout_path, &stderr_path, &raw_telemetry_path] {
        write_text(path, "")?;
    }

    let candidate = CandidateRecord {
        id: candidate_id.clone(),
        label: "A".into(),
        harness_id: harness_id.clone(),
        harness_version: None,
        model: None,
        status: CandidateStatus::Running,
        workspace_path: workspace.clone(),
        prompt_path: run_dir.join("task.md"),
        stdout_path,
        stderr_path,
        diff_path,
        duration_ms: 0,
        exit_code: None,
        timed_out: false,
        tokens: None,
        token_semantics: None,
        cost_usd: None,
        error: None,
        diff_stats: DiffStats::default(),
        checks: Vec::new(),
    };

    let attempt = AttemptRecord {
        detail: AttemptDetail::default(),
        id: Ulid::new().to_string(),
        run_id: run_id.clone(),
        candidate_id: candidate_id.clone(),
        role: "attached".into(),
        ordinal: 1,
        generation: 1,
        harness_id,
        harness_version: None,
        requested_model: None,
        resolved_model: None,
        observed_model: None,
        requested_effort: None,
        resolved_effort: None,
        observed_effort: None,
        started_at: now,
        completed_at: None,
        outcome: "running".into(),
        raw_telemetry_path,
        resource: None,
    };

    let attachment = AttachmentRecord {
        version: 1,
        workspace: workspace.clone(),
        integration_root: root.clone(),
        repo_key: workspace_identity.map(|identity| identity.key),
        provenance,
        confidence,
        agent: request.agent.clone(),
        command: request.command.clone(),
        owner: is_wrapped.then(process::ProcessIdentity::current),
        agent_process: request.pid.map(process::process_identity),
        owner_state: if is_wrapped {
            OwnerState::Live
        } else {
            OwnerState::Unknown
        },
        capabilities: AttachCapabilities {
            observe: true,
            signal: true,
            control: is_wrapped,
            integrate: request.auto_apply,
        },
        attached_at: now,
        finished_at: None,
        finish_reason: None,
        workspace_owner: if managed_workspace.is_some() {
            WorkspaceOwner::Dispatch
        } else if runtime.is_some() && workspace.starts_with(root.join(".claude").join("worktrees"))
        {
            // Where Claude Code documents it makes `--worktree` workspaces.
            WorkspaceOwner::Runtime
        } else {
            WorkspaceOwner::User
        },
        managed: managed_workspace,
        sessions: runtime
            .map(|runtime| vec![runtime.session.clone()])
            .unwrap_or_default(),
        workspace_removed: None,
    };

    let mut run = RunRecord {
        execution: None,
        id: run_id.clone(),
        task: task.clone(),
        exact_prompt: task,
        source_path: root.clone(),
        source_kind,
        source_git_head,
        source_fingerprint,
        baseline_path: snapshot.baseline_path,
        baseline_commit: snapshot.baseline_commit,
        status: RunStatus::Running,
        mode: RunMode::Attached,
        state_revision: 0,
        outcome: RunOutcome {
            lifecycle: LifecycleState::Working,
            work_result: WorkResult::Pending,
            verification: VerificationState::NotRun,
            review: ReviewState::NotRequested,
            application: ApplicationState::NotApplied,
            phase: RunPhase::Executing,
            waiting_on: WaitingOn::None,
            applied_by: None,
            ..RunOutcome::default()
        },
        created_at: now,
        completed_at: None,
        environment: EnvironmentRecord {
            dispatch_version: crate::VERSION.into(),
            os: std::env::consts::OS.into(),
            architecture: std::env::consts::ARCH.into(),
            execution_backend: "local".into(),
            timeout_secs: config.execution.timeout_secs,
            cpus: config.execution.cpus,
            memory: config.execution.memory.clone(),
            max_parallel: 1,
            docker_image: None,
            resource_limits_enforced: false,
            unsafe_local,
            forwarded_env: config.execution.forwarded_env.clone(),
        },
        baseline_checks: Vec::new(),
        candidates: vec![candidate],
        attempts: vec![attempt],
        allocation: None,
        coherence: None,
        applied_candidate: None,
        attachment: Some(attachment),
        historical: Default::default(),
    };

    let created_event = EventRecord {
        run_id: run.id.clone(),
        candidate_label: None,
        event_type: "run.created".into(),
        timestamp: now,
        payload: serde_json::json!({
            "source": run.source_path,
            "source_kind": run.source_kind.as_str(),
            "config": config_path,
            "attachment": run.attachment,
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
            event_type: "attach.created".into(),
            timestamp: Utc::now(),
            payload: serde_json::json!({"attachment": run.attachment}),
            ..EventRecord::default()
        },
        &mut run,
    )?;

    // The wrapped form writes nothing to the terminal while the agent runs
    // (part 6.7): its owner loop prints its own single line only after the
    // child exits. The foreign form prints this banner immediately, as
    // before.
    if !is_wrapped && runtime.is_none() {
        let provenance_text = match &run.attachment.as_ref().unwrap().provenance {
            BaselineProvenance::GitMergeBase { commit } => format!("commit {commit} (merge base)"),
            BaselineProvenance::SnapshotAtAttach => "snapshot at attach".to_owned(),
            BaselineProvenance::WorkspaceAtStart { commit } => {
                format!("commit {commit} (the workspace at start)")
            }
        };
        let confidence_text = match run.attachment.as_ref().unwrap().confidence {
            AttachConfidence::Full => "full",
            AttachConfidence::Partial => "partial",
        };
        println!("ATTACHED {}", run.id);
        println!("Workspace  {}", workspace.display());
        println!("Root       {}", root.display());
        println!("S0         {provenance_text}, {confidence_text} confidence");
        println!("Next: dispatch finish {} when the agent is done", run.id);
    }

    Ok(run)
}

/// The workspace Dispatch makes for wrapped work started in the checkout
/// itself, under `<state>/workspaces/<run-id>`, never inside the checkout.
/// S0 is the checkout's exact world now: for Git, `source::world_commit`
/// checked out as a linked worktree on its own `dispatch/<run-id>` branch;
/// for a plain directory, a snapshot and a private copy of it.
fn make_workspace(
    state: &State,
    root: &Path,
    run_id: &str,
    run_dir: &Path,
) -> Result<(
    source::SourceSnapshot,
    BaselineProvenance,
    PathBuf,
    ManagedWorkspace,
)> {
    let parent = state.root.join("workspaces");
    fs::create_dir_all(&parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
    }
    let path = parent.join(run_id);
    if source::repo_identity(root)?.is_none() {
        let snapshot = source::create_snapshot(root, run_dir)?;
        let workspace = source::create_candidate_workspace(&snapshot.baseline_path, &path)?;
        let provenance = BaselineProvenance::WorkspaceAtStart {
            commit: snapshot.baseline_commit.clone(),
        };
        let made = ManagedWorkspace {
            branch: None,
            removed: false,
        };
        return Ok((snapshot, provenance, workspace, made));
    }
    let commit = source::world_commit(root)?;
    let branch = format!("dispatch/{}", run_id.to_lowercase());
    source::create_linked_workspace(root, &commit, &path, &branch)?;
    let snapshot = match source::materialize_baseline_from_commit(root, &commit, run_dir) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            // Nothing has run in it yet, so there is no work to lose.
            let _ = source::remove_linked_workspace(root, &path, &branch);
            return Err(error);
        }
    };
    let workspace = source::resolve_source(Some(&path))?;
    let made = ManagedWorkspace {
        branch: Some(branch),
        removed: false,
    };
    Ok((
        snapshot,
        BaselineProvenance::WorkspaceAtStart { commit },
        workspace,
        made,
    ))
}

/// Remove the workspace Dispatch made for `run` once its Work is applied: its
/// Δ is in the checkout now, so nothing is lost. Only a workspace under
/// `<state>/workspaces/` recorded at creation is ever removed; a failure is
/// reported and the workspace kept.
pub(super) fn release_workspace(state: &State, run: &mut RunRecord) -> Result<()> {
    let Some(attachment) = run.attachment.as_ref() else {
        return Ok(());
    };
    let Some(made) = attachment.managed.as_ref().filter(|made| !made.removed) else {
        return Ok(());
    };
    let workspace = attachment.workspace.clone();
    let parent = fs::canonicalize(state.root.join("workspaces"))?;
    anyhow::ensure!(
        workspace.parent() == Some(parent.as_path()),
        "refusing to remove {}: not a workspace Dispatch made",
        workspace.display()
    );
    match &made.branch {
        Some(branch) => {
            source::remove_linked_workspace(&attachment.integration_root, &workspace, branch)?
        }
        None => fs::remove_dir_all(&workspace)
            .with_context(|| format!("failed to remove {}", workspace.display()))?,
    }
    if let Some(made) = run.attachment.as_mut().and_then(|a| a.managed.as_mut()) {
        made.removed = true;
    }
    let db = Database::open(state.db_path())?;
    persist_event(
        state,
        &db,
        EventRecord {
            run_id: run.id.clone(),
            candidate_label: None,
            event_type: "workspace.released".into(),
            timestamp: Utc::now(),
            payload: serde_json::json!({"workspace": workspace}),
            ..EventRecord::default()
        },
        run,
    )?;
    Ok(())
}

/// `dispatch attach -- <command...>`: the wrapped form's owner loop (parts
/// 6.1 and 6.7). `create`s the Work with `command: Some(argv)`, spawns the
/// agent under the terminal in the wrapper's own process group, watches the
/// integration world while it runs without ever stopping or signaling the
/// agent because of a verdict, and on exit finishes the Work and, if
/// `capabilities.integrate`, applies it. Returns the exit code the CLI
/// process should use: the agent's own exit code, when the wrapper's own
/// work (creation, then finishing) succeeded. A failure in either of those
/// propagates as `Err`, exiting 1 (rule 6 of part 6.7): creation failures
/// never start the agent; a finish failure after the agent exited leaves the
/// Work `Working` for a human `dispatch finish` to retry.
#[cfg(unix)]
pub async fn run_wrapped(state: &State, request: AttachRequest) -> Result<i32> {
    let argv = request
        .command
        .clone()
        .filter(|argv| !argv.is_empty())
        .context("wrapped attach requires a command after --")?;
    let allow_unsafe_local = request.allow_unsafe_local;
    let auto_apply_requested = request.auto_apply;

    let mut run = create(state, request)?;
    let run_dir = state.run_dir(&run.id);
    let workspace = run.candidates[0].workspace_path.clone();
    // The one line before the agent starts: where it works, when Dispatch
    // chose the place.
    if let Some(branch) = run
        .attachment
        .as_ref()
        .and_then(|attachment| attachment.managed.as_ref())
        .map(|made| made.branch.clone())
    {
        let on = branch
            .map(|branch| format!(" on branch {branch}"))
            .unwrap_or_default();
        eprintln!(
            "Working in {}{on}; your checkout is not touched.",
            workspace.display()
        );
    }

    // Held for the whole wrapped session: this is how `finish` and `serve`
    // know the Work is owned (rule 1).
    let run_lock = OperationLock::acquire(
        &run_dir.join(".operation.lock"),
        "attached work has a foreground owner",
    )?;

    // Rule 3: install the SIGTERM/SIGHUP forwarding handlers before the agent
    // ever runs. A handler-based disposition always resets to default on
    // `exec` regardless of when it is installed (unlike `SIG_IGN`, part
    // below), so installing it this early carries no inheritance risk, and
    // closes the race a fast-starting agent could otherwise hit: without
    // this, a signal arriving after `spawn` but before the wrapper finished
    // installing its handler would kill the wrapper by the default
    // disposition, orphaning the agent instead of forwarding to it.
    let mut sigterm = signal(SignalKind::terminate()).context("failed to watch for SIGTERM")?;
    let mut sighup = signal(SignalKind::hangup()).context("failed to watch for SIGHUP")?;

    // Rule 2: the agent inherits the wrapper's own stdio and stays in its
    // process group (no `process_group(0)`), so terminal job control and
    // Ctrl+C behave exactly as if the shell had started it directly. Not the
    // `Executor` path: no piped output, no token accounting, no timeout.
    // Rule 3: ignore SIGINT/SIGQUIT for the wrapper's own lifetime while the
    // child runs (as `time` does). `SIG_IGN` is inherited across `exec`, so
    // it is set before `spawn` and undone in the child between `fork` and
    // `exec` (`pre_exec` below): the child execs with the default disposition
    // and stays interruptible, and there is no window in which a Ctrl+C
    // arriving right after `spawn` could kill the wrapper and orphan it.
    // Nothing is printed to the terminal while the child lives.
    ignore_signal(libc::SIGINT);
    ignore_signal(libc::SIGQUIT);
    let mut command = std::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(&workspace)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: runs in the forked child before `exec`; `signal(2)` is
        // async-signal-safe and touches nothing shared with the parent.
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                libc::signal(libc::SIGQUIT, libc::SIG_DFL);
                Ok(())
            });
        }
    }
    let spawned = command
        .spawn()
        .with_context(|| format!("failed to start {}", argv.join(" ")));
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            restore_signal(libc::SIGINT);
            restore_signal(libc::SIGQUIT);
            return Err(error);
        }
    };
    let child_pid = child.id();
    let agent_process = process::process_identity(child_pid);

    let mut db = Database::open(state.db_path())?;
    if let Some(attachment) = run.attachment.as_mut() {
        attachment.agent_process = Some(agent_process.clone());
    }
    db.sync_run(&run)?;
    persist_event(
        state,
        &db,
        EventRecord {
            run_id: run.id.clone(),
            candidate_label: None,
            event_type: "attach.started".into(),
            timestamp: Utc::now(),
            payload: serde_json::json!({"agent_process": agent_process}),
            ..EventRecord::default()
        },
        &mut run,
    )?;

    // Rule 4: the same mid-run watcher allocation runs use, persisted through
    // the shared `persist_verdict` step. A verdict never stops or signals
    // the agent.
    let config = crate::coherence::run_config(&run_dir);
    let poll_secs = config
        .as_ref()
        .map_or(10, |config| config.coherence.poll_secs)
        .max(1);
    let mut watcher = Watcher::spawn(WatchSpec {
        source: run.source_path.clone(),
        kind: run.source_kind.clone(),
        baseline: run.baseline_path.clone(),
        baseline_commit: run.baseline_commit.clone(),
        workspace: workspace.clone(),
        delta_patch: run_dir.join("delta-live.patch"),
        poll: Duration::from_secs(poll_secs),
    });

    let (exit_tx, mut exit_rx) = tokio::sync::oneshot::channel();
    let wait_handle = tokio::task::spawn_blocking(move || {
        let status = child.wait();
        let _ = exit_tx.send(status);
    });

    let exit_status = loop {
        tokio::select! {
            result = &mut exit_rx => {
                let status = result.context("lost the agent process's exit status")?;
                break status.context("failed to wait for the agent process")?;
            }
            _ = sigterm.recv() => forward_signal(child_pid, libc::SIGTERM),
            _ = sighup.recv() => forward_signal(child_pid, libc::SIGHUP),
            message = watcher.rx.recv() => {
                if let Some(message) = message {
                    apply::persist_verdict(state, &db, &mut run, &message.validity)?;
                }
            }
        }
    };
    let _ = wait_handle.await;

    // Rule 3: the disposition is restored before the wrapper exits.
    restore_signal(libc::SIGINT);
    restore_signal(libc::SIGQUIT);

    // Rule 5: stop the watcher and drain whatever verdict it produced during
    // the exit race before finishing.
    watcher.finish().await;
    while let Ok(message) = watcher.rx.try_recv() {
        apply::persist_verdict(state, &db, &mut run, &message.validity)?;
    }

    let code = exit_status.code();
    let run = finish_locked(
        state,
        &mut db,
        run,
        allow_unsafe_local,
        FinishReason::ProcessExit { code },
    )
    .await?;
    let run_id = run.id.clone();

    // Rule 5: `auto_apply` takes the run lock itself.
    drop(run_lock);

    if auto_apply_requested {
        let outcome = auto_apply(state, &run_id)?;
        print_auto_apply_outcome(&run, &outcome);
    } else {
        println!("Attached work {run_id} finished; next: dispatch check {run_id}");
    }

    Ok(code.unwrap_or(1))
}

#[cfg(not(unix))]
pub async fn run_wrapped(_state: &State, _request: AttachRequest) -> Result<i32> {
    bail!("wrapped attach is only supported on Unix")
}

/// Send `signal` to the agent process. Best-effort: the child may already
/// have exited (a benign race with the exit-status wait), in which case this
/// is a no-op.
#[cfg(unix)]
fn forward_signal(pid: u32, signal: i32) {
    // SAFETY: `pid` names the agent process this wrapper spawned and still
    // owns; `signal` is always one of the fixed constants above.
    unsafe {
        libc::kill(pid as libc::pid_t, signal);
    }
}

#[cfg(unix)]
fn ignore_signal(signal: i32) {
    // SAFETY: `signal` is always one of the fixed constants above; `SIG_IGN`
    // is always a valid disposition.
    unsafe {
        libc::signal(signal, libc::SIG_IGN);
    }
}

#[cfg(unix)]
fn restore_signal(signal: i32) {
    // SAFETY: see `ignore_signal`.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
    }
}

/// Print `outcome` in the same words `dispatch run --auto-apply` uses (see
/// `finish_run` in `src/main.rs`): a wrapped attach with `--auto-apply`
/// reaches the same policy application through a different owner loop, and
/// the two should read identically from a terminal.
#[cfg(unix)]
fn print_auto_apply_outcome(run: &RunRecord, outcome: &ApplyOutcome) {
    match outcome {
        ApplyOutcome::Applied { report, .. } => println!(
            "Auto-applied Candidate {} to {} ({} file(s) changed). Review not performed.",
            report.candidate_label,
            run.source_path.display(),
            report.files_changed
        ),
        ApplyOutcome::Blocked { reason, .. } => println!(
            "Not applied automatically: {reason}. Review with dispatch check {0} or dispatch accept {0}.",
            run.id
        ),
        ApplyOutcome::Skipped { reason } => println!(
            "Not applied automatically: {reason}. Review with dispatch check {0} or dispatch accept {0}.",
            run.id
        ),
        ApplyOutcome::Failed { error } => println!(
            "Not applied automatically: {error}. Review with dispatch check {0} or dispatch accept {0}.",
            run.id
        ),
    }
}

/// A `RunMode::Attached` run whose lifecycle is still `Working` for this
/// workspace, if one exists (part 14.8's "already attached" refusal and part
/// 6.9's reattach rule).
/// Record another runtime session on active Work: a resume, a clear, a
/// compaction or a fork into the same workspace. The same session reported
/// twice is recorded once.
pub(crate) fn record_session_start(
    state: &State,
    run_id: &str,
    session: RuntimeSession,
) -> Result<()> {
    let _lock = OperationLock::acquire_wait(
        &state.run_dir(run_id).join(".operation.lock"),
        "attached work has a foreground owner",
        Duration::from_secs(5),
    )?;
    let mut run = state.load_run(run_id)?;
    let Some(attachment) = run.attachment.as_mut() else {
        return Ok(());
    };
    if attachment.sessions.iter().any(|known| {
        known.provider == session.provider
            && known.session_id == session.session_id
            && known.ended_at.is_none()
    }) {
        return Ok(());
    }
    attachment.sessions.push(session.clone());
    // A long-lived workspace sees many sessions; the Work keeps the latest.
    let excess = attachment.sessions.len().saturating_sub(MAX_SESSIONS);
    attachment.sessions.drain(..excess);
    let mut db = Database::open(state.db_path())?;
    db.sync_run(&run)?;
    persist_event(
        state,
        &db,
        EventRecord {
            run_id: run.id.clone(),
            candidate_label: None,
            event_type: "runtime.session_started".into(),
            timestamp: Utc::now(),
            payload: serde_json::json!({"session": session}),
            ..EventRecord::default()
        },
        &mut run,
    )
}

/// Record that a runtime session ended. Nothing else changes: an ended
/// session does not end the Work.
pub(crate) fn record_session_end(
    state: &State,
    run_id: &str,
    provider: &str,
    session_id: &str,
    reason: &str,
) -> Result<()> {
    let _lock = OperationLock::acquire_wait(
        &state.run_dir(run_id).join(".operation.lock"),
        "attached work has a foreground owner",
        // Within the 5 s `SessionEnd` timeout setup installs; the project owner
        // may hold the lock while it checks this Work.
        Duration::from_secs(3),
    )?;
    let mut run = state.load_run(run_id)?;
    let Some(session) = run.attachment.as_mut().and_then(|attachment| {
        attachment.sessions.iter_mut().rev().find(|known| {
            known.provider == provider && known.session_id == session_id && known.ended_at.is_none()
        })
    }) else {
        return Ok(());
    };
    session.ended_at = Some(Utc::now());
    session.end_reason = Some(reason.to_owned());
    let mut db = Database::open(state.db_path())?;
    db.sync_run(&run)?;
    persist_event(
        state,
        &db,
        EventRecord {
            run_id: run.id.clone(),
            candidate_label: None,
            event_type: "runtime.session_ended".into(),
            timestamp: Utc::now(),
            payload: serde_json::json!({
                "provider": provider,
                "session_id": session_id,
                "reason": reason,
            }),
            ..EventRecord::default()
        },
        &mut run,
    )
}

const MAX_SESSIONS: usize = 100;

pub(crate) fn find_active_attachment(state: &State, workspace: &Path) -> Result<Option<String>> {
    for path in state.list_metadata_paths()? {
        let projected: RunRecord = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("invalid metadata at {}", path.display()))?;
        let run = state.load_run(&projected.id)?;
        if run.mode == RunMode::Attached
            && run.outcome.lifecycle == LifecycleState::Working
            && run
                .attachment
                .as_ref()
                .is_some_and(|attachment| attachment.workspace == workspace)
        {
            return Ok(Some(run.id));
        }
    }
    Ok(None)
}

/// `dispatch finish <id>`: freeze Δ, run verification in the workspace
/// itself, and become an ordinary Ready result. See part 14.9.
pub async fn finish(state: &State, run_id: &str, allow_unsafe_local: bool) -> Result<RunRecord> {
    let resolved_run_id = state.resolve_run_id(run_id)?;
    // A moment's wait: the project owner holds this lock briefly while it
    // checks the Work. A live wrapper holds it for the whole session.
    let _run_lock = OperationLock::acquire_wait(
        &state.run_dir(&resolved_run_id).join(".operation.lock"),
        "attached work has a foreground owner",
        Duration::from_secs(5),
    )?;
    let run = state.load_run(&resolved_run_id)?;
    anyhow::ensure!(
        run.mode == RunMode::Attached && run.outcome.lifecycle == LifecycleState::Working,
        "attached work {} is not active",
        run.id
    );
    anyhow::ensure!(
        run.candidates.len() == 1,
        "attached work {} does not have exactly one candidate",
        run.id
    );

    let mut db = Database::open(state.db_path())?;
    let mut run = run;
    if allow_unsafe_local && !run.environment.unsafe_local {
        // The person's acknowledgement is the run's from now on, so the checks
        // on the merged tree run at accept too, as for work attached with it.
        run.environment.unsafe_local = true;
        db.sync_run(&run)?;
        persist_event(
            state,
            &db,
            EventRecord {
                run_id: run.id.clone(),
                candidate_label: None,
                event_type: "attach.authorized".into(),
                timestamp: Utc::now(),
                payload: serde_json::json!({"unsafe_local": true, "by": "human"}),
                ..EventRecord::default()
            },
            &mut run,
        )?;
    }
    let run = finish_locked(
        state,
        &mut db,
        run,
        allow_unsafe_local,
        FinishReason::Explicit,
    )
    .await?;

    print_finish_summary(&run);
    println!("Next: dispatch check {}", run.id);

    Ok(run)
}

/// The mechanics of "finished" (part 14.9), shared by `dispatch finish`
/// (`reason: Explicit`) and the wrapped owner loop (`reason: ProcessExit`,
/// once the agent has exited): freeze Δ, run `checks.verify` in the
/// workspace itself, set the candidate's outcome and `exit_code` from
/// `reason`, and commit `attach.finished` + `run.finished`. The caller holds
/// the run's operation lock and has already checked that the run is an
/// active, single-candidate attachment; this prints nothing, so each caller
/// presents the result in its own words.
pub(super) async fn finish_locked(
    state: &State,
    db: &mut Database,
    mut run: RunRecord,
    allow_unsafe_local: bool,
    reason: FinishReason,
) -> Result<RunRecord> {
    let run_dir = state.run_dir(&run.id);
    let workspace = run.candidates[0].workspace_path.clone();
    let diff_path = run.candidates[0].diff_path.clone();
    let removal = run
        .attachment
        .as_ref()
        .and_then(|attachment| attachment.workspace_removed.clone());
    if let Some(removal) = &removal {
        anyhow::ensure!(
            removal.exact,
            "the workspace of {} was removed before Dispatch could keep its final changes; \
             only the changes last seen are kept, at {}, and they cannot be finished. \
             Reject this work: dispatch reject {}",
            run.id,
            run_dir.join("delta-last-seen.patch").display(),
            run.id
        );
    }
    // A removed workspace is rebuilt from S0 and its kept Δ, so the checks
    // still run on exactly the work.
    let rebuilt = tempfile::Builder::new()
        .prefix("dispatch-rebuilt-")
        .tempdir()
        .context("failed to prepare a workspace for the checks")?;
    let (workspace, diff_stats) = match &removal {
        Some(_) => (
            source::rebuild_workspace(&run.baseline_path, &diff_path, &rebuilt.path().join("w"))?,
            Ok(run.candidates[0].diff_stats.clone()),
        ),
        None => (
            workspace.clone(),
            source::collect_diff(&run.baseline_path, &workspace, &diff_path),
        ),
    };

    let diff_stats = match diff_stats {
        Ok(stats) => stats,
        Err(error) => {
            run.candidates[0].status = CandidateStatus::Failed;
            run.candidates[0].error = Some(format!("{error:#}"));
            refresh_outcome(&mut run);
            run.status = RunStatus::Failed;
            run.completed_at = Some(Utc::now());
            finish_attachment(&mut run, reason);
            db.sync_run(&run)?;
            persist_event(state, db, run_finished_event(&run), &mut run)?;
            return Ok(run);
        }
    };
    run.candidates[0].diff_stats = diff_stats;

    let config = crate::coherence::run_config(&run_dir);
    let verify = config
        .as_ref()
        .map_or_else(Vec::new, |cfg| cfg.checks.verify.clone());
    if !verify.is_empty() {
        anyhow::ensure!(
            run.environment.unsafe_local || allow_unsafe_local,
            "finish runs your checks.verify on the host; pass --allow-unsafe-local"
        );
        let execution = config.map(|cfg| cfg.execution).unwrap_or_default();
        let results = crate::executor::run_checks_with_config(
            &workspace,
            &verify,
            CheckPhase::Verify,
            &run_dir.join("checks/verify"),
            execution,
        )
        .await;
        run.candidates[0].checks = results;
    }

    run.candidates[0].status = CandidateStatus::Completed;
    let attached_at = run
        .attachment
        .as_ref()
        .expect("attached run carries an attachment record")
        .attached_at;
    run.candidates[0].duration_ms = (Utc::now() - attached_at).num_milliseconds().max(0) as u64;
    run.candidates[0].exit_code = match &reason {
        FinishReason::ProcessExit { code } => *code,
        FinishReason::Explicit => None,
    };

    refresh_outcome(&mut run);
    run.status = if run.outcome.work_result == WorkResult::Ready {
        RunStatus::ReadyForEvaluation
    } else {
        RunStatus::Failed
    };
    run.completed_at = Some(Utc::now());
    let candidate_snapshot = run.candidates[0].clone();
    if let Some(attempt) = run.attempts.first_mut() {
        attempt.completed_at = run.completed_at;
        attempt.outcome = "completed".into();
        attempt.detail.result = Some(candidate_snapshot);
    }
    finish_attachment(&mut run, reason.clone());

    db.sync_run(&run)?;
    persist_event(
        state,
        db,
        EventRecord {
            run_id: run.id.clone(),
            candidate_label: None,
            event_type: "attach.finished".into(),
            timestamp: Utc::now(),
            payload: serde_json::json!({"reason": reason}),
            ..EventRecord::default()
        },
        &mut run,
    )?;
    persist_event(state, db, run_finished_event(&run), &mut run)?;

    Ok(run)
}

/// A runtime is about to delete the workspace of active Work, which destroys
/// the only authoritative copy of its Δ. The exact final Δ is written and
/// synced into the run and the removal committed before this returns; any
/// error means the deletion must not go ahead. The Work does not end: a
/// removed workspace is an observation, and a person still finishes or
/// rejects it. Only an empty Δ closes it, as there is nothing to review.
/// Returns how many files the kept Δ changes.
pub(crate) fn freeze_removed_workspace(state: &State, run_id: &str) -> Result<u64> {
    let _lock = OperationLock::acquire_wait(
        &state.run_dir(run_id).join(".operation.lock"),
        "attached work has a foreground owner",
        Duration::from_secs(60),
    )?;
    let mut run = state.load_run(run_id)?;
    anyhow::ensure!(
        run.mode == RunMode::Attached
            && run.outcome.lifecycle == LifecycleState::Working
            && run.candidates.len() == 1,
        "work {run_id} is not active attached work"
    );
    let already_kept = run
        .attachment
        .as_ref()
        .and_then(|attachment| attachment.workspace_removed.as_ref())
        .is_some_and(|removal| removal.exact);
    if already_kept {
        // A replayed event: the exact Δ is already durable.
        return Ok(run.candidates[0].diff_stats.files_changed);
    }
    let run_dir = state.run_dir(run_id);
    let staging = run_dir.join(".delta-at-removal.patch");
    let stats = source::collect_diff(
        &run.baseline_path,
        &run.candidates[0].workspace_path,
        &staging,
    )?;
    crate::state::write_durably(&run.candidates[0].diff_path, &fs::read(&staging)?)?;
    let _ = fs::remove_file(&staging);

    let files_changed = stats.files_changed;
    run.candidates[0].diff_stats = stats;
    let now = Utc::now();
    if let Some(attachment) = run.attachment.as_mut() {
        attachment.workspace_removed = Some(WorkspaceRemoval {
            at: now,
            exact: true,
        });
    }
    if files_changed == 0 {
        run.candidates[0].status = CandidateStatus::Cancelled;
        run.status = RunStatus::Interrupted;
        run.completed_at = Some(now);
        run.outcome.lifecycle = LifecycleState::Finished;
        run.outcome.work_result = WorkResult::Cancelled;
        run.outcome.phase = RunPhase::Finished;
        run.outcome.review = ReviewState::NotRequested;
        if let Some(attempt) = run.attempts.first_mut() {
            attempt.completed_at = Some(now);
            attempt.outcome = "no_changes".into();
        }
    }
    let mut db = Database::open(state.db_path())?;
    db.sync_run(&run)?;
    persist_event(
        state,
        &db,
        EventRecord {
            run_id: run.id.clone(),
            candidate_label: None,
            event_type: "workspace.removed".into(),
            timestamp: now,
            payload: serde_json::json!({"exact": true, "files_changed": files_changed}),
            ..EventRecord::default()
        },
        &mut run,
    )?;
    Ok(files_changed)
}

/// The project owner found the workspace of active Work gone without being
/// told first: only the Δ it last followed survives, kept beside the run as
/// `delta-last-seen.patch`. It can be read, never finished.
pub(crate) fn note_workspace_gone(
    state: &State,
    run_id: &str,
    last_seen: Option<&Path>,
) -> Result<()> {
    let _lock = OperationLock::acquire(
        &state.run_dir(run_id).join(".operation.lock"),
        "attached work has a foreground owner",
    )?;
    let mut run = state.load_run(run_id)?;
    let Some(attachment) = run.attachment.as_mut() else {
        return Ok(());
    };
    if attachment.workspace_removed.is_some()
        || attachment.workspace.exists()
        || run.outcome.lifecycle != LifecycleState::Working
    {
        return Ok(());
    }
    let now = Utc::now();
    attachment.workspace_removed = Some(WorkspaceRemoval {
        at: now,
        exact: false,
    });
    if let Some(last_seen) = last_seen.filter(|path| path.is_file()) {
        crate::state::write_durably(
            &state.run_dir(run_id).join("delta-last-seen.patch"),
            &fs::read(last_seen)?,
        )?;
    }
    let mut db = Database::open(state.db_path())?;
    db.sync_run(&run)?;
    persist_event(
        state,
        &db,
        EventRecord {
            run_id: run.id.clone(),
            candidate_label: None,
            event_type: "workspace.removed".into(),
            timestamp: now,
            payload: serde_json::json!({"exact": false}),
            ..EventRecord::default()
        },
        &mut run,
    )
}

/// Close Work whose workspace vanished before its final changes were kept: a
/// person's decision (`dispatch reject`), as it can never be finished. The
/// changes last seen stay beside the run.
pub(crate) fn close_lost(state: &State, run_id: &str) -> Result<()> {
    let _lock = OperationLock::acquire_wait(
        &state.run_dir(run_id).join(".operation.lock"),
        "attached work has a foreground owner",
        Duration::from_secs(5),
    )?;
    let mut run = state.load_run(run_id)?;
    anyhow::ensure!(
        run.outcome.lifecycle == LifecycleState::Working
            && run
                .attachment
                .as_ref()
                .and_then(|attachment| attachment.workspace_removed.as_ref())
                .is_some_and(|removal| !removal.exact),
        "work {run_id} is not work whose workspace was lost"
    );
    let now = Utc::now();
    if let Some(candidate) = run.candidates.first_mut() {
        candidate.status = CandidateStatus::Cancelled;
    }
    run.status = RunStatus::Interrupted;
    run.completed_at = Some(now);
    run.outcome.lifecycle = LifecycleState::Finished;
    run.outcome.work_result = WorkResult::Cancelled;
    run.outcome.phase = RunPhase::Finished;
    run.outcome.review = ReviewState::NotRequested;
    if let Some(attempt) = run.attempts.first_mut() {
        attempt.completed_at = Some(now);
        attempt.outcome = "workspace_lost".into();
    }
    let mut db = Database::open(state.db_path())?;
    db.sync_run(&run)?;
    persist_event(
        state,
        &db,
        EventRecord {
            run_id: run.id.clone(),
            candidate_label: None,
            event_type: "work.closed".into(),
            timestamp: now,
            payload: serde_json::json!({"reason": "workspace_lost", "by": "human"}),
            ..EventRecord::default()
        },
        &mut run,
    )
}

fn finish_attachment(run: &mut RunRecord, reason: FinishReason) {
    if let Some(attachment) = run.attachment.as_mut() {
        attachment.finished_at = Some(Utc::now());
        attachment.finish_reason = Some(reason);
    }
}

/// The same `run.finished` shape `run_dispatch` commits.
fn run_finished_event(run: &RunRecord) -> EventRecord {
    EventRecord {
        run_id: run.id.clone(),
        candidate_label: None,
        event_type: "run.finished".into(),
        timestamp: Utc::now(),
        payload: serde_json::json!({
            "candidate_count": run.candidates.len(),
            "outcome": run.outcome,
        }),
        ..EventRecord::default()
    }
}

/// A short equivalent of `print_single_result_summary` for attached work:
/// that function requires a routing or allocation decision, which attached
/// work never has.
fn print_finish_summary(run: &RunRecord) {
    let candidate = &run.candidates[0];
    let heading = if run.outcome.verification == VerificationState::Failed {
        "Verification failed"
    } else {
        "Ready for review"
    };
    println!("\n{heading}\n");
    println!("Task\n  {}\n", run.task);
    println!("Agent\n  {}\n", candidate.harness_id);
    println!("Verification\n  {}\n", result_verification(candidate));
    println!("Review\n  dispatch diff\n");
    println!("Then\n  dispatch accept\n  dispatch reject");
}
