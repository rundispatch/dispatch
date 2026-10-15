//! `dispatch attach` (foreign form and wrapped form) and `dispatch finish`:
//! attached external work Dispatch did not select or route, as a `RunRecord`
//! with `mode: Attached`. See `docs/plan-0.3-auto-apply-and-attach.md`, parts
//! 6.1–6.9 and 14.3–14.10 (the design freeze this module implements exactly).
//! `dispatch close` retires Work, attached or not, without judging it.
//!
//! The wrapped form (`dispatch attach -- <command...>`) is the owner loop of
//! its own Work (part 6.1): `create` it, spawn the agent under the terminal,
//! watch the integration world while it runs, and on exit finish and,
//! authorized, apply. `run_wrapped` is that loop.

use std::process::Stdio;

use tokio::signal::unix::{SignalKind, signal};

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use ulid::Ulid;

use super::{
    ApplyOutcome, apply, auto_apply, awaits_review, patch_line, persist_event,
    print_review_commands, print_standing, ready_heading, refresh_outcome, registration,
    result_verification,
};
use crate::{
    ApplicationState, AttachCapabilities, AttachConfidence, AttachmentRecord, AttemptDetail,
    AttemptRecord, BaselineProvenance, CandidateRecord, CandidateStatus, CheckPhase, CloseReason,
    Closure, Config, DiffStats, EnvironmentRecord, EventRecord, FinishReason, LifecycleState,
    ManagedWorkspace, OwnerState, ReviewState, RunMode, RunOutcome, RunPhase, RunRecord, RunStatus,
    RuntimeSession, SourceAtStart, StartObservation, StartPath, VerificationState, WaitingOn,
    WorkResult, WorkspaceOwner, WorkspaceRemoval,
    coherence::{
        watch::{WatchSpec, Watcher},
        world,
    },
    db::Database,
    lock::OperationLock,
    process, source,
    state::{State, write_atomically, write_text},
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

/// Work a runtime's session start registers (`registration`): S0 is `commit`,
/// the workspace's exact world captured before the session's first turn. A
/// resume into a workspace Dispatch has not seen may follow edits made before
/// it, so it is partial. Work the project owner found without a hook
/// (`discovery`) begins at the worktree's merge base with the root instead.
pub struct RuntimeStart {
    pub session: RuntimeSession,
    pub resumed: bool,
    /// The commit `source::world_commit` made of the workspace, or the merge
    /// base when `discovered`.
    pub commit: String,
    pub discovered: bool,
}

/// The `source_fingerprint` of attached Work outside strict mode: not a
/// SHA-256, so it never equals a fingerprint of any tree.
pub(crate) const NOT_FINGERPRINTED: &str = "not fingerprinted (attached work outside strict mode)";

/// `dispatch attach --workspace <path> ...` (foreign form): create a
/// `RunMode::Attached` run observing an already-running agent, without
/// spawning anything or touching the workspace. See part 14.8 for the
/// refusal order and exact messages. The run is built in
/// `registrations/<id>/` and published whole into `runs/<id>/`, or removed.
pub fn create(state: &State, request: AttachRequest) -> Result<RunRecord> {
    let prepared = prepare(state, request)?;
    let run_id = Ulid::new().to_string();
    let building = registration::begin(state, &run_id)?;
    let mut run = match build(state, &prepared, &run_id)
        .and_then(|built| publish(state, &run_id, Some(built), None))
    {
        Ok(run) => run,
        Err(error) => {
            building.remove();
            return Err(error);
        }
    };
    if !prepared.managed && prepared.request.runtime.is_none() {
        match super::serve::load_source_runs(state, &prepared.root)
            .and_then(|runs| observe_start(state, &run.id, &runs))
        {
            Ok(observed) => run = observed,
            Err(error) => tracing::warn!("the source at start was not observed: {error:#}"),
        }
    }

    // The wrapped form writes nothing to the terminal while the agent runs
    // (part 6.7): its owner loop prints its own single line only after the
    // child exits. The foreign form prints this banner immediately, as
    // before.
    if prepared.request.command.is_none() && prepared.request.runtime.is_none() {
        let provenance_text = match &run.attachment.as_ref().unwrap().provenance {
            BaselineProvenance::GitMergeBase { commit } => format!("commit {commit} (merge base)"),
            BaselineProvenance::SnapshotAtAttach => "snapshot at attach".to_owned(),
            BaselineProvenance::WorkspaceAtStart { commit } => {
                format!("commit {commit} (the workspace at start)")
            }
            BaselineProvenance::Discovered { commit } => {
                format!("commit {commit} (merge base; found after watching began)")
            }
        };
        let confidence_text = match run.attachment.as_ref().unwrap().confidence {
            AttachConfidence::Full => "full",
            AttachConfidence::Partial => "partial",
        };
        println!("ATTACHED {}", run.id);
        println!("Workspace  {}", prepared.workspace.display());
        println!("Root       {}", prepared.root.display());
        println!("S0         {provenance_text}, {confidence_text} confidence");
        let at_start = super::source_at_start(run.attachment.as_ref().unwrap());
        for (n, line) in at_start.iter().enumerate() {
            println!(
                "{}{line}",
                if n == 0 { "Source     " } else { "           " }
            );
        }
        println!("Next: dispatch finish {} when the agent is done", run.id);
    }

    Ok(run)
}

/// An attach request that passed the refusals of part 14.8, before anything
/// is written.
pub(crate) struct Prepared {
    request: AttachRequest,
    workspace: PathBuf,
    root: PathBuf,
    managed: bool,
    merge_base: Option<String>,
    repo_key: Option<String>,
    config: Config,
    config_path: Option<PathBuf>,
}

pub(crate) fn prepare(state: &State, request: AttachRequest) -> Result<Prepared> {
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
    let merge_base = match &workspace_identity {
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

    if !managed && let Some(existing_id) = find_active_attachment(state, &workspace)? {
        bail!("workspace already attached as {existing_id}");
    }

    Ok(Prepared {
        request,
        workspace,
        root,
        managed,
        merge_base,
        repo_key: workspace_identity.map(|identity| identity.key),
        config,
        config_path,
    })
}

/// A run assembled in its registration directory, not yet recorded.
pub(crate) struct Built {
    run: RunRecord,
    created: EventRecord,
}

/// Assemble run `run_id` in `registrations/<run_id>/`, which the caller made
/// and holds. Every path recorded names `runs/<run_id>/`, where `publish`
/// moves it. Whoever removes a failed build removes this directory.
pub(crate) fn build(state: &State, prepared: &Prepared, run_id: &str) -> Result<Built> {
    let Prepared {
        request,
        workspace,
        root,
        managed,
        merge_base,
        repo_key,
        config,
        config_path,
    } = prepared;
    let is_wrapped = request.command.is_some();
    let runtime = request.runtime.as_ref();
    let unsafe_local = request.allow_unsafe_local;
    let dir = registration::directory(state, run_id);
    let run_dir = state.run_dir(run_id);
    let published = |path: PathBuf| match path.strip_prefix(&dir) {
        Ok(inside) => run_dir.join(inside),
        Err(_) => path,
    };

    let task = request.task.clone().unwrap_or_else(|| {
        let basename = workspace
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| workspace.display().to_string());
        format!("attached work in {basename}")
    });
    write_text(&dir.join("task.md"), &task)?;
    write_text(
        &dir.join("config.snapshot.yml"),
        &serde_yaml::to_string(&config).context("failed to serialize effective configuration")?,
    )?;

    let (snapshot, provenance, confidence, workspace, managed_workspace) = if *managed {
        let (snapshot, provenance, workspace, made) = make_workspace(state, root, run_id, &dir)
            .map_err(|error| error.context("could not make a workspace"))?;
        (
            snapshot,
            provenance,
            AttachConfidence::Full,
            workspace,
            Some(made),
        )
    } else if let Some(runtime) = runtime {
        let snapshot = source::materialize_baseline_from_commit(workspace, &runtime.commit, &dir)
            .map_err(|error| error.context("could not record S0"))?;
        let commit = runtime.commit.clone();
        let (provenance, confidence) = if runtime.discovered {
            // The worktree appeared after watching began: every change in it
            // is its runtime's.
            (
                BaselineProvenance::Discovered { commit },
                AttachConfidence::Full,
            )
        } else if runtime.resumed {
            (
                BaselineProvenance::WorkspaceAtStart { commit },
                AttachConfidence::Partial,
            )
        } else {
            (
                BaselineProvenance::WorkspaceAtStart { commit },
                AttachConfidence::Full,
            )
        };
        (snapshot, provenance, confidence, workspace.clone(), None)
    } else {
        let (snapshot, provenance, confidence) = match merge_base {
            Some(commit) => {
                let snapshot = source::materialize_baseline_from_commit(root, commit, &dir)
                    .map_err(|error| error.context("baseline creation failed"))?;
                (
                    snapshot,
                    BaselineProvenance::GitMergeBase {
                        commit: commit.clone(),
                    },
                    AttachConfidence::Full,
                )
            }
            None => {
                let snapshot = source::create_snapshot(workspace, &dir)
                    .map_err(|error| error.context("baseline creation failed"))?;
                (
                    snapshot,
                    BaselineProvenance::SnapshotAtAttach,
                    AttachConfidence::Partial,
                )
            }
        };
        (snapshot, provenance, confidence, workspace.clone(), None)
    };

    let (source_kind, source_git_head) = source::inspect_source(root)?;
    // Attached runs consult the root's whole-tree fingerprint only in strict
    // mode (`coherence::gate`). Walking the root otherwise costs the most of
    // anything here: it hashes ignored build output, including that of
    // runtime worktrees inside the root, and races its deletion. In any other
    // mode the value can never match, so every reader fails closed.
    let source_fingerprint = if config.coherence.accept == crate::config::AcceptMode::Strict {
        source::fingerprint_tree(root)?
    } else {
        NOT_FINGERPRINTED.to_owned()
    };

    // Runtime Work began when its session did, even when published later.
    let now = runtime.map_or_else(Utc::now, |runtime| runtime.session.started_at);
    let candidate_id = Ulid::new().to_string();
    let harness_id = request.agent.clone().unwrap_or_else(|| "external".into());
    for name in [
        "delta.patch",
        "attach-stdout.log",
        "attach-stderr.log",
        "harness.jsonl",
    ] {
        write_text(&dir.join(name), "")?;
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
        stdout_path: run_dir.join("attach-stdout.log"),
        stderr_path: run_dir.join("attach-stderr.log"),
        diff_path: run_dir.join("delta.patch"),
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
        run_id: run_id.to_owned(),
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
        raw_telemetry_path: run_dir.join("harness.jsonl"),
        resource: None,
    };

    let attachment = AttachmentRecord {
        version: 1,
        workspace: workspace.clone(),
        integration_root: root.clone(),
        repo_key: repo_key.clone(),
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
        } else if runtime.is_some_and(|runtime| runtime.discovered)
            || (runtime.is_some() && workspace.starts_with(root.join(".claude").join("worktrees")))
        {
            // Where Claude Code documents it makes `--worktree` workspaces,
            // or one discovery recognized by its runtime's sign.
            WorkspaceOwner::Runtime
        } else {
            WorkspaceOwner::User
        },
        // A workspace Dispatch made begins from the current source; any other
        // is observed once after publication (`observe_start`).
        source_at_start: managed_workspace
            .is_none()
            .then_some(SourceAtStart::NotObserved),
        managed: managed_workspace,
        sessions: runtime
            .map(|runtime| vec![runtime.session.clone()])
            .unwrap_or_default(),
        workspace_removed: None,
    };

    let run = RunRecord {
        execution: None,
        id: run_id.to_owned(),
        task: task.clone(),
        exact_prompt: task,
        source_path: root.clone(),
        source_kind,
        source_git_head,
        source_fingerprint,
        baseline_path: published(snapshot.baseline_path),
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

    let created = EventRecord {
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
    Ok(Built { run, created })
}

/// Who published runtime-registered Work, and when its hook started.
pub(crate) struct Registered {
    pub started_at: DateTime<Utc>,
    /// `hook`, `owner`, or `discovery` when the owner found the session
    /// without a hook; then `started_at` is the session's start.
    pub via: &'static str,
}

/// Publish run `run_id` from its registration directory: record it in the
/// database, then rename the directory into `runs/<run_id>/`. Idempotent: a
/// record already there is completed, not repeated, and a rename already
/// done is the published run. `built` is needed only when nothing is
/// recorded yet. The caller holds the registration directory.
pub(crate) fn publish(
    state: &State,
    run_id: &str,
    built: Option<Built>,
    registered: Option<Registered>,
) -> Result<RunRecord> {
    let dir = registration::directory(state, run_id);
    let run_dir = state.run_dir(run_id);
    if !dir.is_dir() {
        anyhow::ensure!(
            run_dir.is_dir(),
            "registration {run_id} is gone and was never published"
        );
        return state.load_run(run_id);
    }
    let mut db = Database::open(state.db_path())?;
    let mut run = match db.committed_run_projection(run_id)? {
        Some(run) => run,
        None => {
            let Built { mut run, created } = built.context("the Work was not built")?;
            db.commit_run_transition(&mut run, created)?;
            run
        }
    };
    registration::fault("after_db_insert");
    let recorded = db.events_for_run(run_id)?;
    let has = |kind: &str| recorded.iter().any(|event| event.event_type == kind);
    if !has("attach.created") {
        let mut payload = serde_json::json!({"attachment": run.attachment});
        if let Some(registered) = &registered {
            payload["registration_ms"] = (Utc::now() - registered.started_at)
                .num_milliseconds()
                .max(0)
                .into();
            payload["via"] = registered.via.into();
        }
        db.commit_transition(
            &mut run,
            EventRecord {
                run_id: run_id.to_owned(),
                candidate_label: None,
                event_type: "attach.created".into(),
                timestamp: Utc::now(),
                payload,
                ..EventRecord::default()
            },
        )?;
    }
    if registered.is_some() && run.environment.unsafe_local && !has("attach.authorized") {
        db.commit_transition(
            &mut run,
            EventRecord {
                run_id: run_id.to_owned(),
                candidate_label: None,
                event_type: "attach.authorized".into(),
                timestamp: Utc::now(),
                payload: serde_json::json!({"unsafe_local": true, "by": "project consent"}),
                ..EventRecord::default()
            },
        )?;
    }
    // The projections go in before the rename: `runs/` lists only runs whose
    // metadata is there.
    let mut events = Vec::new();
    for event in db.events_for_run(run_id)? {
        serde_json::to_writer(&mut events, &event)?;
        events.push(b'\n');
    }
    write_atomically(&dir.join("events.jsonl"), &events)?;
    write_atomically(
        &dir.join("metadata.json"),
        &serde_json::to_vec_pretty(&run)?,
    )?;
    fs::rename(&dir, &run_dir)
        .with_context(|| format!("failed to publish {} into runs/", dir.display()))?;
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
pub(super) fn release_workspace(state: &State, run: &mut RunRecord, reason: &str) -> Result<()> {
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
            payload: serde_json::json!({"workspace": workspace, "reason": reason}),
            ..EventRecord::default()
        },
        run,
    )?;
    Ok(())
}

/// A workspace `dispatch clean` may remove: Dispatch made it, it is still
/// there, its Work is over (rejected, closed, or applied and not yet
/// released), and no Work in progress or waiting for review is attached to
/// the same path.
pub struct Cleanable {
    pub run_id: String,
    pub workspace: PathBuf,
    pub branch: Option<String>,
    pub why: &'static str,
}

/// A workspace you or an agent's runtime made, still there, whose Work is
/// over: `dispatch clean` keeps it and says how to remove it.
pub struct Kept {
    pub workspace: PathBuf,
    pub owner: WorkspaceOwner,
    /// The Work that was done there, and how each ended.
    pub work: Vec<(String, &'static str)>,
}

/// What `dispatch clean` may remove, and the terminal Work it keeps because
/// Dispatch did not make its workspace.
pub fn cleanable(state: &State) -> Result<(Vec<Cleanable>, Vec<Kept>)> {
    let mut runs = Vec::new();
    for path in state.list_metadata_paths()? {
        let projected: RunRecord = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("invalid metadata at {}", path.display()))?;
        if projected.attachment.is_some() {
            runs.push(state.load_run(&projected.id)?);
        }
    }
    let attached = runs
        .iter()
        .filter_map(|run| Some((run, run.attachment.as_ref()?)))
        .collect::<Vec<_>>();
    // The paths some Work still needs, and the paths Dispatch made.
    let (mut in_use, mut made_here) = (HashSet::new(), HashSet::new());
    for (run, attachment) in &attached {
        if run.outcome.lifecycle != LifecycleState::Finished || awaits_review(run) {
            in_use.insert(&attachment.workspace);
        }
        if attachment.managed.is_some() {
            made_here.insert(&attachment.workspace);
        }
    }
    let (mut found, mut kept) = (Vec::new(), Vec::<Kept>::new());
    for (run, attachment) in &attached {
        if run.outcome.lifecycle != LifecycleState::Finished
            || in_use.contains(&attachment.workspace)
            || !attachment.workspace.exists()
        {
            continue;
        }
        let why = if run.outcome.application == ApplicationState::Applied {
            "applied"
        } else if run.outcome.review == ReviewState::Rejected {
            "rejected"
        } else if run.outcome.closed.is_some() || run.outcome.work_result != WorkResult::Ready {
            "closed"
        } else {
            continue; // waiting for review
        };
        match &attachment.managed {
            Some(made) if !made.removed => found.push(Cleanable {
                run_id: run.id.clone(),
                workspace: attachment.workspace.clone(),
                branch: made.branch.clone(),
                why,
            }),
            Some(_) => {}
            None if made_here.contains(&attachment.workspace)
                || attachment.workspace_owner == WorkspaceOwner::Dispatch => {}
            None => match kept
                .iter_mut()
                .find(|item| item.workspace == attachment.workspace)
            {
                Some(item) => item.work.push((run.id.clone(), why)),
                None => kept.push(Kept {
                    workspace: attachment.workspace.clone(),
                    owner: attachment.workspace_owner,
                    work: vec![(run.id.clone(), why)],
                }),
            },
        }
    }
    Ok((found, kept))
}

/// Remove what `cleanable` listed, each under its run's lock and only if it
/// is still cleanable then. Returns how many were removed.
pub fn clean(state: &State, listed: &[Cleanable]) -> Result<usize> {
    let mut removed = 0;
    for item in listed {
        let _lock = OperationLock::acquire_wait(
            &state.run_dir(&item.run_id).join(".operation.lock"),
            "the work is in use",
            Duration::from_secs(5),
        )?;
        let still = cleanable(state)?
            .0
            .into_iter()
            .any(|now| now.run_id == item.run_id && now.workspace == item.workspace);
        if !still {
            continue;
        }
        let mut run = state.load_run(&item.run_id)?;
        release_workspace(state, &mut run, "cleaned")?;
        removed += 1;
    }
    Ok(removed)
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

/// Most differing paths an observation of the source at start keeps.
const MAX_START_PATHS: usize = 20;

/// Record, once, where the integration source differed from the Work's S0
/// when it began: `world::observe` lists every path where they differ,
/// whichever side has the change. A path in the retained patch of Work
/// applied to this source (`runs`) names that Work; nothing else is
/// attributed. Advisory: no verdict depends on it. `attach` calls this right
/// after publishing, the project owner for Work a runtime registered; never
/// registration. On failure nothing is recorded, and it stays not observed.
pub(crate) fn observe_start(state: &State, run_id: &str, runs: &[RunRecord]) -> Result<RunRecord> {
    let pending = |run: &RunRecord| {
        run.attachment.as_ref().is_some_and(|attachment| {
            attachment.source_at_start == Some(SourceAtStart::NotObserved)
        })
    };
    // Compared without the run's lock: on a large source this takes seconds,
    // and hooks wait for that lock only briefly. The lock guards the record.
    let run = state.load_run(run_id)?;
    if !pending(&run) {
        return Ok(run);
    }
    let world = world::observe(
        &run.source_path,
        &run.baseline_path,
        &run.baseline_commit,
        &run.source_kind,
    )?;
    let (_, source_head) = source::inspect_source(&run.source_path)?;
    let mut applied = std::collections::HashMap::new();
    for other in runs.iter().filter(|other| {
        other.outcome.application == ApplicationState::Applied && other.id != run.id
    }) {
        let candidate = other
            .candidates
            .iter()
            .find(|candidate| other.applied_candidate.as_ref() == Some(&candidate.label));
        for path in candidate.iter().flat_map(|c| &c.diff_stats.changed_files) {
            applied.insert(path.as_str(), other.id.as_str());
        }
    }
    let observation = StartObservation {
        at: Utc::now(),
        source_head,
        differing: world.changes.len(),
        paths: world
            .changes
            .iter()
            .take(MAX_START_PATHS)
            .map(|change| StartPath {
                path: change.path.clone(),
                applied_work: applied.get(change.path.as_str()).map(|id| (*id).to_owned()),
            })
            .collect(),
    };
    let _lock = OperationLock::acquire_wait(
        &state.run_dir(run_id).join(".operation.lock"),
        "attached work has a foreground owner",
        Duration::from_secs(5),
    )?;
    let mut run = state.load_run(run_id)?;
    if !pending(&run) {
        return Ok(run); // recorded meanwhile: the first observation stands
    }
    if let Some(attachment) = run.attachment.as_mut() {
        attachment.source_at_start = Some(SourceAtStart::Observed(observation.clone()));
    }
    let mut db = Database::open(state.db_path())?;
    db.sync_run(&run)?;
    persist_event(
        state,
        &db,
        EventRecord {
            run_id: run.id.clone(),
            candidate_label: None,
            event_type: "attach.source_observed".into(),
            timestamp: observation.at,
            payload: serde_json::to_value(&observation)?,
            ..EventRecord::default()
        },
        &mut run,
    )?;
    Ok(run)
}

pub(crate) fn find_active_attachment(state: &State, workspace: &Path) -> Result<Option<String>> {
    find_active(state, |attachment| attachment.workspace == workspace)
}

/// Active Work of the project at `root` holding the open runtime session
/// `session_id` of `provider`.
pub(crate) fn find_session_attachment(
    state: &State,
    root: &Path,
    provider: &str,
    session_id: &str,
) -> Result<Option<String>> {
    find_active(state, |attachment| {
        attachment.integration_root == root
            && attachment.sessions.iter().any(|session| {
                session.provider == provider
                    && session.session_id == session_id
                    && session.ended_at.is_none()
            })
    })
}

/// One pass over the runs: the first unfinished attached Work `matches`.
fn find_active(
    state: &State,
    matches: impl Fn(&AttachmentRecord) -> bool,
) -> Result<Option<String>> {
    for path in state.list_metadata_paths()? {
        let projected: RunRecord = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("invalid metadata at {}", path.display()))?;
        let run = state.load_run(&projected.id)?;
        if run.mode == RunMode::Attached
            && run.outcome.lifecycle == LifecycleState::Working
            && run.attachment.as_ref().is_some_and(&matches)
        {
            return Ok(Some(run.id));
        }
    }
    Ok(None)
}

/// `dispatch finish <id>`: freeze Δ, run verification in the workspace
/// itself, and become an ordinary Ready result. See part 14.9.
pub async fn finish(state: &State, run_id: &str, allow_unsafe_local: bool) -> Result<RunRecord> {
    let run = finish_quietly(state, run_id, allow_unsafe_local).await?;
    print_finish_summary(
        state,
        &crate::coherence::with_kept_validity(&run, &state.run_dir(&run.id)),
    );
    Ok(run)
}

/// `finish` without printing, for `watch`.
pub(crate) async fn finish_quietly(
    state: &State,
    run_id: &str,
    allow_unsafe_local: bool,
) -> Result<RunRecord> {
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
                run_id: run_id.to_owned(),
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
    if removal.as_ref().is_some_and(|removal| !removal.exact) {
        let kept = super::last_seen_patch(&run).with_context(|| {
            format!(
                "the workspace of {} was removed and nothing was kept; it cannot be finished. \
             Close this work: dispatch close {} --reason workspace-removed",
                run.id, run.id
            )
        })?;
        crate::state::write_durably(&diff_path, &fs::read(kept)?)?;
    }
    // A removed workspace is rebuilt from S0 and its kept Δ, so the checks
    // still run on exactly the work.
    let rebuilt = tempfile::Builder::new()
        .prefix("dispatch-rebuilt-")
        .tempdir()
        .context("failed to prepare a workspace for the checks")?;
    let (workspace, diff_stats) = match &removal {
        Some(_) => {
            let workspace = source::rebuild_workspace(
                &run.baseline_path,
                &diff_path,
                &rebuilt.path().join("w"),
            )?;
            // Derive statistics from the rebuilt tree too: the live patch's
            // statistics were not necessarily retained when removal was noticed.
            let stats = source::collect_diff(
                &run.baseline_path,
                &workspace,
                &rebuilt.path().join("delta.patch"),
            );
            (workspace, stats)
        }
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
        FinishReason::Explicit | FinishReason::ByConsent => None,
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
            payload: serde_json::json!({"reason": reason, "workspace_removed": removal}),
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
/// closes it. Only an empty Δ ends it, as there is nothing to review.
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
            last_seen_at: None,
        });
    }
    if files_changed == 0 {
        cancel_work(&mut run, now, "no_changes");
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

/// Finish Work whose workspace was removed with its exact changes kept, when
/// the person consented to the project's checks running by themselves: the
/// checks run in a workspace rebuilt from S0 and those changes, and the Work
/// becomes an ordinary result awaiting review. Only when the consent still
/// holds for exactly the commands this Work would run. Returns whether it
/// finished; a run another process holds is left for the next try.
pub(super) async fn finish_by_consent(state: &State, run_id: &str) -> Result<bool> {
    let Ok(_lock) = OperationLock::acquire(
        &state.run_dir(run_id).join(".operation.lock"),
        "attached work has a foreground owner",
    ) else {
        return Ok(false);
    };
    let run = state.load_run(run_id)?;
    let removed_exactly = run
        .attachment
        .as_ref()
        .and_then(|attachment| attachment.workspace_removed.as_ref())
        .is_some_and(|removal| removal.exact);
    if run.mode != RunMode::Attached
        || run.outcome.lifecycle != LifecycleState::Working
        || !removed_exactly
        || !run.environment.unsafe_local
        || run.candidates.len() != 1
    {
        return Ok(false);
    }
    let project = crate::consent::project_root(&run.source_path)?;
    let crate::consent::CheckConsent::Valid(consented) = crate::consent::consent(state, &project)?
    else {
        return Ok(false);
    };
    let runs = crate::coherence::run_config(&state.run_dir(run_id))
        .map(|config| config.checks.verify)
        .unwrap_or_default();
    if runs != consented {
        return Ok(false);
    }
    let mut db = Database::open(state.db_path())?;
    finish_locked(state, &mut db, run, false, FinishReason::ByConsent).await?;
    Ok(true)
}

/// The project owner found the workspace of active Work gone without being
/// told first: only the Δ it last followed survives, kept beside the run as
/// `delta-last-seen.patch`. A person can explicitly finish from those changes.
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
    let mut last_seen_at = None;
    let last_seen = match last_seen.filter(|path| path.is_file()) {
        Some(path) => {
            let mut file = fs::File::open(path)?;
            last_seen_at = Some(DateTime::<Utc>::from(file.metadata()?.modified()?));
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut file, &mut bytes)?;
            Some(bytes)
        }
        None => None,
    };
    attachment.workspace_removed = Some(WorkspaceRemoval {
        at: now,
        exact: false,
        last_seen_at,
    });
    if let Some(bytes) = &last_seen {
        crate::state::write_durably(&state.run_dir(run_id).join("delta-last-seen.patch"), bytes)?;
    }
    // Last seen with no changes at all: nothing was lost, so nothing waits.
    let empty = last_seen.as_ref().is_some_and(Vec::is_empty);
    if empty {
        cancel_work(&mut run, now, "no_changes");
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
            payload: serde_json::json!({"exact": false, "no_changes": empty, "last_seen_at": last_seen_at}),
            ..EventRecord::default()
        },
        &mut run,
    )
}

/// End unfinished Work without a result: when a person closes it, or when
/// its workspace turns out to have held no changes. `attempt_outcome` says
/// which.
fn cancel_work(run: &mut RunRecord, at: DateTime<Utc>, attempt_outcome: &str) {
    if let Some(candidate) = run.candidates.first_mut() {
        candidate.status = CandidateStatus::Cancelled;
    }
    run.status = RunStatus::Interrupted;
    run.completed_at = Some(at);
    run.outcome.lifecycle = LifecycleState::Finished;
    run.outcome.work_result = WorkResult::Cancelled;
    run.outcome.phase = RunPhase::Finished;
    run.outcome.review = ReviewState::NotRequested;
    if let Some(attempt) = run.attempts.first_mut() {
        attempt.completed_at = Some(at);
        attempt.outcome = attempt_outcome.into();
    }
}

/// Whether the workspace of attached Work is gone: removed as a runtime
/// reported, found missing, or no longer there.
pub(crate) fn workspace_gone(run: &RunRecord) -> bool {
    run.attachment.as_ref().is_some_and(|attachment| {
        attachment.workspace_removed.is_some() || !attachment.workspace.exists()
    })
}

/// `dispatch close <id> --reason <reason>`.
pub struct CloseRequest {
    /// The Work, by ID or unique prefix.
    pub run_id: String,
    pub reason: CloseReason,
    /// The Work that replaces it, by ID or unique prefix; with `superseded`
    /// and only then.
    pub replacement: Option<String>,
    pub explanation: Option<String>,
}

/// The reasons `close` takes for `run` as it is now, or why nothing may close
/// it. Unfinished attached Work is unfinished, or its workspace is gone; a
/// result waiting for review is already present. Either may be superseded.
pub(crate) fn close_reasons(run: &RunRecord) -> Result<Vec<CloseReason>> {
    let id = &run.id;
    if let Some(closed) = &run.outcome.closed {
        bail!("Work {id} is already closed ({})", closed.describe());
    }
    if run.outcome.application == ApplicationState::Applied {
        bail!("Work {id} was applied; there is nothing to close");
    }
    if matches!(
        run.outcome.review,
        ReviewState::Accepted | ReviewState::Rejected
    ) {
        bail!("Work {id} was already reviewed; closing it would not change that");
    }
    if run.mode == RunMode::Attached
        && run.outcome.lifecycle == LifecycleState::Working
        && run.candidates.len() == 1
    {
        let why = if workspace_gone(run) {
            CloseReason::WorkspaceRemoved
        } else {
            CloseReason::Unfinished
        };
        return Ok(vec![why, CloseReason::Superseded]);
    }
    if run.candidates.len() == 1 && super::awaits_review(run) {
        return Ok(vec![CloseReason::AlreadyPresent, CloseReason::Superseded]);
    }
    bail!(
        "Work {id} is neither unfinished attached Work nor a result waiting for review; there is nothing to close"
    )
}

/// `dispatch close`: a person retires Work without judging it. It records
/// no review and no preference, and never applies, commits, signals an
/// agent or deletes a workspace. Unfinished Work is frozen first, as
/// `finish` freezes it but without checks; when its workspace is gone, what
/// was kept of it stays: the exact Δ, or the changes last seen. A workspace
/// gone with nothing recorded yet is refused while an owner watches the
/// project, until it has kept what it last saw. A delivered result keeps its
/// Δ. A live wrapper holds the run's lock for its whole session, so Work it
/// owns is refused. Returns what happened, in words.
pub fn close(state: &State, request: CloseRequest) -> Result<String> {
    let id = state.resolve_run_id(&request.run_id)?;
    let superseded = request.reason == CloseReason::Superseded;
    anyhow::ensure!(
        superseded == request.replacement.is_some(),
        "--replacement names the Work that supersedes this one: it goes with --reason superseded, and only with it"
    );
    let replacement = match &request.replacement {
        Some(other) => Some(state.resolve_run_id(other)?),
        None => None,
    };
    let _lock = OperationLock::acquire_wait(
        &state.run_dir(&id).join(".operation.lock"),
        "Work is held by its Dispatch owner",
        Duration::from_secs(5),
    )?;
    let mut run = state.load_run(&id)?;
    let reasons = close_reasons(&run)?;
    anyhow::ensure!(
        reasons.contains(&request.reason),
        "--reason {} does not fit Work {id} as it is; it takes {}",
        request.reason.flag(),
        reasons
            .iter()
            .map(|reason| reason.flag())
            .collect::<Vec<_>>()
            .join(" or ")
    );

    let mut payload = serde_json::json!({"reason": request.reason, "by": "human"});
    if request.reason == CloseReason::AlreadyPresent {
        let label = run.candidates[0].label.clone();
        let Some(digest) = crate::coherence::patch_present(&run, &label)? else {
            bail!(
                "the whole of Work {id}'s patch is not in the source now, so it was not closed; dispatch check {id} shows where it stands"
            );
        };
        payload["world_digest"] = digest.into();
    }
    if let Some(other) = &replacement {
        anyhow::ensure!(other != &id, "Work {id} cannot supersede itself");
        let other = state.load_run(other)?;
        anyhow::ensure!(
            other.source_path == run.source_path,
            "Work {} belongs to another project, so it does not replace {id}",
            other.id
        );
        anyhow::ensure!(
            other.outcome.lifecycle == LifecycleState::Finished
                && other.outcome.work_result == WorkResult::Ready,
            "Work {0} has no finished result, so {id} stays open; finish it first: dispatch finish {0}",
            other.id
        );
        anyhow::ensure!(
            other.outcome.closed.is_none() && other.outcome.review != ReviewState::Rejected,
            "Work {} was itself {}, so it replaces nothing and {id} stays open",
            other.id,
            if other.outcome.closed.is_some() {
                "closed"
            } else {
                "rejected"
            }
        );
        let kept = other
            .candidates
            .first()
            .and_then(|candidate| fs::metadata(&candidate.diff_path).ok())
            .is_some_and(|patch| patch.len() > 0);
        anyhow::ensure!(
            kept,
            "Work {} kept no changes, so it replaces nothing and {id} stays open",
            other.id
        );
        payload["replacement"] = other.id.clone().into();
    }

    let now = Utc::now();
    let run_dir = state.run_dir(&id);
    let mut kept = run.candidates[0].diff_path.clone();
    let mut unobserved = false;
    if run.outcome.lifecycle == LifecycleState::Working {
        let removal = run
            .attachment
            .as_ref()
            .and_then(|attachment| attachment.workspace_removed.clone());
        match removal {
            Some(removal) if !removal.exact => kept = run_dir.join("delta-last-seen.patch"),
            Some(_) => {}
            // Gone without a word: a watching owner keeps the changes it last
            // saw on its next tick, which closed Work would stop. Without one,
            // nobody followed the workspace.
            None if workspace_gone(&run) => {
                anyhow::ensure!(
                    matches!(
                        super::background::watcher(state, &run.source_path)?,
                        super::background::Watcher::NotWatched
                    ),
                    "the workspace of Work {id} vanished, and the project's owner has not yet kept the changes it last saw; nothing was closed, so try again in a few seconds. If this persists, the owner may be stuck: dispatch stop ends it, and close then goes ahead without those changes"
                );
                unobserved = true;
            }
            None => {
                let workspace = run.candidates[0].workspace_path.clone();
                run.candidates[0].diff_stats =
                    source::collect_diff(&run.baseline_path, &workspace, &kept)?;
            }
        }
        cancel_work(&mut run, now, request.reason.as_str());
    } else {
        run.outcome.review = ReviewState::NotRequested;
        run.outcome.phase = RunPhase::Finished;
    }
    let closure = Closure {
        reason: request.reason,
        at: now,
        replacement,
        explanation: request.explanation,
    };
    let closed = closure.describe();
    run.outcome.closed = Some(closure);
    let mut db = Database::open(state.db_path())?;
    db.sync_run(&run)?;
    persist_event(
        state,
        &db,
        EventRecord {
            run_id: id.clone(),
            candidate_label: None,
            event_type: "work.closed".into(),
            timestamp: now,
            payload,
            ..EventRecord::default()
        },
        &mut run,
    )?;
    let kept = match fs::metadata(&kept) {
        _ if unobserved => "no changes were kept, because its workspace vanished while \
                            nothing watched the project"
            .into(),
        Ok(patch) if patch.len() > 0 => format!("its changes stay at {}", kept.display()),
        _ => "it kept no changes".into(),
    };
    Ok(format!(
        "Closed {id}: {closed}. No review was recorded, nothing was applied or deleted, and {kept}."
    ))
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
/// work never has. `run` is a `with_kept_validity` copy.
fn print_finish_summary(state: &State, run: &RunRecord) {
    let candidate = &run.candidates[0];
    println!("\n{}\n", ready_heading(run));
    println!("Task\n  {}\n", run.task);
    if let Some(label) = super::last_seen_changes(run) {
        println!("Changes\n  {label}\n");
    }
    println!("Agent\n  {}\n", candidate.harness_id);
    println!(
        "Work {}\n{}\n  {}\n",
        run.id,
        patch_line(candidate),
        super::delta_digest(&candidate.diff_path).map_or_else(
            |_| "Δ unknown".to_owned(),
            |digest| super::digest_line(&digest)
        )
    );
    println!("Verification\n  {}", result_verification(candidate));
    for check in &candidate.checks {
        println!("{}", super::check_line(check));
    }
    println!();
    let validity = run.coherence.as_ref().and_then(|c| c.validity.as_ref());
    if awaits_review(run) {
        print_standing(state, run, validity);
    }
    print_review_commands(run, validity);
}

#[cfg(test)]
mod last_seen_tests {
    use super::*;

    fn attached() -> (tempfile::TempDir, State, RunRecord) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("file.txt"), "before\n").unwrap();
        fs::write(root.join("dispatch.yml"), "checks:\n  verify: ['true']\n").unwrap();
        let git = |args: &[&str]| {
            assert!(
                source::git_command(&root)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["init", "--quiet"]);
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "initial",
        ]);
        let workspace = temp.path().join("worktree");
        git(&[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "work",
            workspace.to_str().unwrap(),
        ]);
        let state = State {
            root: temp.path().join("state"),
        };
        state.initialize().unwrap();
        let run = create(
            &state,
            AttachRequest {
                workspace: workspace.clone(),
                root: Some(root),
                task: None,
                agent: None,
                pid: None,
                command: None,
                allow_unsafe_local: true,
                auto_apply: false,
                runtime: None,
            },
        )
        .unwrap();
        fs::write(workspace.join("file.txt"), "after\n").unwrap();
        (temp, state, run)
    }

    #[tokio::test]
    async fn last_seen_time_is_the_patch_mtime_and_consent_does_not_finish_it() {
        let (temp, state, run) = attached();
        crate::consent::grant(&state, &run.source_path).unwrap();
        let scratch = temp.path().join("followed.patch");
        source::collect_diff(
            &run.baseline_path,
            &run.candidates[0].workspace_path,
            &scratch,
        )
        .unwrap();
        let at = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        fs::File::options()
            .write(true)
            .open(&scratch)
            .unwrap()
            .set_modified(at)
            .unwrap();
        fs::remove_dir_all(&run.candidates[0].workspace_path).unwrap();
        note_workspace_gone(&state, &run.id, Some(&scratch)).unwrap();
        let kept = state.load_run(&run.id).unwrap();
        let value = serde_json::to_value(&kept).unwrap();
        assert_eq!(
            value["attachment"]["workspace_removed"]["last_seen_at"],
            "2023-11-14T22:13:20Z"
        );
        assert!(!finish_by_consent(&state, &run.id).await.unwrap());
        assert_eq!(
            state.load_run(&run.id).unwrap().outcome.lifecycle,
            LifecycleState::Working
        );
        // A complete old run record with no capture-time field still loads.
        let mut old = value;
        old["attachment"]["workspace_removed"]
            .as_object_mut()
            .unwrap()
            .remove("last_seen_at");
        let restored: RunRecord = serde_json::from_value(old).unwrap();
        let restored = serde_json::to_value(restored).unwrap();
        assert!(restored["attachment"]["workspace_removed"]["last_seen_at"].is_null());
    }

    #[tokio::test]
    async fn a_removed_workspace_without_a_last_seen_patch_can_only_be_closed() {
        let (_temp, state, run) = attached();
        fs::remove_dir_all(&run.candidates[0].workspace_path).unwrap();
        note_workspace_gone(&state, &run.id, None).unwrap();
        let error = finish_quietly(&state, &run.id, false).await.unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("nothing was kept"), "{message}");
        assert!(
            message.contains(&format!(
                "dispatch close {} --reason workspace-removed",
                run.id
            )),
            "{message}"
        );
        assert_eq!(
            state.load_run(&run.id).unwrap().outcome.lifecycle,
            LifecycleState::Working
        );
    }
}
