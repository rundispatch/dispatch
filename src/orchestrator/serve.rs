//! The project owner and its view (`docs/attach.md`, "The project owner").
//! One owner per integration root, `dispatch start` in the background or
//! `dispatch serve` in the foreground: every tick it observes the root's
//! world, adopts attached Work whose owner has gone, follows unowned attached
//! Work, keeps Ready results' verdicts what `check` would show, and
//! auto-applies attached Work whose `capabilities.integrate` is true. It never
//! finishes, launches, kills or refreshes anything; wrapped attach and human
//! review remain the only things that do. `dispatch watch` renders the same
//! view from canonical state and owns nothing.

use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use chrono::Utc;
use sha2::{Digest, Sha256};

use super::{ApplyOutcome, WorkLine, apply, auto_apply, persist_event, work_line};
use crate::{
    ApplicationState, Config, Decision, EventRecord, LifecycleState, OwnerState, ReviewState,
    RunMode, RunRecord, SourceKind, Validity, WorkResult,
    coherence::{
        self, WorkView,
        interactions::{self, Analysis, Edge, Footprint, Participant, Projection},
        measure::{self, Counterpart, Evaluation, Landing, Outcome},
        world,
    },
    db::Database,
    lock::{OperationLock, shutdown_signal},
    process::{IdentityState, ProcessIdentity, identity_state},
    source,
    state::State,
};

/// `dispatch serve [--root <path>] [--json]`. Loops until Ctrl+C, SIGTERM or
/// SIGHUP; a second `serve` on the same root refuses to start. The owner
/// decides and records; this loop only renders what it did. `background`
/// is `dispatch start`'s owner: it renders nothing, and its stderr is a log
/// that names each distinct error once.
pub async fn serve(
    state: &State,
    root: Option<PathBuf>,
    json: bool,
    background: bool,
) -> Result<()> {
    state.initialize()?;
    let root = source::resolve_source(root.as_deref())?;
    let _serve_lock = super::background::acquire(state, &root)?;
    let (kind, _head) = source::inspect_source(&root)?;
    let (config, _config_path) = Config::discover(&root, None)?;
    let poll = Duration::from_secs(config.coherence.poll_secs.max(1));
    let mut owner = Owner::new(root.clone(), kind)?;
    super::background::write_record(state, &root, background)?;
    if background {
        eprintln!(
            "{} watching {} (pid {})",
            Utc::now().to_rfc3339(),
            root.display(),
            std::process::id()
        );
    }
    let result = run(state, &mut owner, poll, json, background).await;
    super::background::remove_record(state, &root);
    if background {
        eprintln!("{} stopped", Utc::now().to_rfc3339());
    }
    result
}

async fn run(
    state: &State,
    owner: &mut Owner,
    poll: Duration,
    json: bool,
    background: bool,
) -> Result<()> {
    let mut ticker = tokio::time::interval(poll);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Listening for the whole loop, so a signal that arrives mid-tick is
    // still seen at the next select.
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let mut view = View::default();
    let mut last_error: Option<String> = None;

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = &mut shutdown => return Ok(()),
        }
        let mut tick = owner.tick(state);
        // Work whose runtime removed its workspace, finished where the person
        // consented to the project's checks running by themselves.
        let waiting: Vec<String> = tick
            .runs
            .iter()
            .filter(|run| awaits_finish_by_consent(run))
            .map(|run| run.id.clone())
            .collect();
        for id in waiting {
            match super::attach::finish_by_consent(state, &id).await {
                Ok(true) => tick.persisted = true,
                Ok(false) => {}
                Err(error) => tick.report(&error),
            }
        }
        if tick.cost.idle() && !tick.moved {
            tracing::debug!("tick: {}", tick.cost);
        } else {
            tracing::info!("tick: {}", tick.cost);
        }
        if background {
            if tick.error.is_some() && tick.error != last_error {
                eprintln!(
                    "{} {}",
                    Utc::now().to_rfc3339(),
                    tick.error.as_deref().unwrap_or_default()
                );
            }
            last_error = tick.error;
            continue;
        }
        if let Some(error) = &tick.error {
            eprintln!("serve: {error}");
        }
        if json
            && tick.moved
            && let Some(digest) = &tick.digest
        {
            println!("{}", serde_json::json!({"type": "world", "digest": digest}));
        }
        // Render every tick: a run attached, finished or applied by another
        // process changes the view without any verdict or apply of our own.
        // `render` prints only what differs from what is already shown.
        view.render(None, &tick.runs, json, owner.projection());
        let _ = io::stdout().flush();
    }
}

/// `dispatch watch [--root <path>] [--json]`: the project view, live, read
/// from canonical state. It takes no lock and records nothing, so leaving it
/// never stops project watching. It redraws when an event is committed, when
/// who watches changes, and every 30 s so finished Work ages out.
pub async fn watch(state: &State, root: Option<PathBuf>, json: bool) -> Result<()> {
    let root = source::resolve_source(root.as_deref())?;
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let mut view = View::default();
    let mut shown: Option<(Option<i64>, String, Option<chrono::DateTime<Utc>>)> = None;
    let mut rendered_at = Instant::now();
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = &mut shutdown => return Ok(()),
        }
        let header = format!(
            "{} · {}",
            root.display(),
            super::background::project_line(state, &root)
        );
        let journal = Database::open_read_only(state.db_path())
            .and_then(|db| db.latest_event_id())
            .ok()
            .flatten();
        let interactions = super::background::interactions(state, &root);
        let computed = interactions.as_ref().map(|p| p.computed_at);
        let now = (journal, header, computed);
        if shown.as_ref() == Some(&now) && rendered_at.elapsed() < Duration::from_secs(30) {
            continue;
        }
        match load_source_runs(state, &root) {
            Ok(runs) => view.render(Some(&now.1), &runs, json, interactions.as_ref()),
            Err(error) => eprintln!("watch: {error:#}"),
        }
        let _ = io::stdout().flush();
        shown = Some(now);
        rendered_at = Instant::now();
    }
}

/// The project owner for one integration root: everything `serve` decides
/// and records, and nothing it prints. One `tick` observes the world, adopts
/// orphaned attached Work, re-evaluates unowned Work and auto-applies what
/// policy allows, all under the same locks as before.
pub(crate) struct Owner {
    root: PathBuf,
    kind: SourceKind,
    last_signal: Option<world::Signal>,
    /// Each followed run's work signal when it was last evaluated.
    work: HashMap<String, String>,
    /// Each Ready result's world signal when it was last checked.
    checked: HashMap<String, world::Signal>,
    /// Each unintegrated Work's footprint, with the digest of the Δ it was
    /// derived from, whether that Δ was frozen, and how far it can be trusted.
    footprints: HashMap<String, ((String, bool), Footprint, Analysis)>,
    /// Each live Work's last cleanly parsed footprint.
    clean: HashMap<String, Footprint>,
    /// What the interaction view says now; `None` until it is first written.
    shown: Option<Projection>,
    /// Landed runs whose every counterpart has its outcome recorded, or that
    /// are too old to measure: never looked at again by this owner.
    measured: HashSet<String>,
    scratch: tempfile::TempDir,
}

/// What one tick did, and the root's runs as they stand after it.
#[derive(Default)]
pub(crate) struct Tick {
    pub moved: bool,
    pub adopted: bool,
    pub persisted: bool,
    pub applied: bool,
    pub digest: Option<String>,
    pub runs: Vec<RunRecord>,
    /// Runs whose patch so far was snapshotted into the scratch this tick.
    pub followed: HashSet<String>,
    /// The interaction view changed.
    pub interactions: bool,
    /// The first error of the tick. SQLite busy and other transient
    /// failures are retried on the next tick rather than ending the loop.
    pub error: Option<String>,
    pub cost: Cost,
}

/// Where one tick's time went, for `-v` diagnostics: the world signal,
/// loading the root's runs, following unowned attached work (snapshotting
/// its patch), evaluating it, re-checking Ready results, and auto-apply.
#[derive(Default)]
pub(crate) struct Cost {
    pub signal: Duration,
    pub load: Duration,
    pub follow: Duration,
    pub evaluate: Duration,
    pub recheck: Duration,
    pub apply: Duration,
    pub footprint: Duration,
    pub pairs: Duration,
    pub runs: usize,
    pub followed: usize,
    pub evaluated: usize,
    pub rechecked: usize,
    pub footprinted: usize,
}

impl Cost {
    fn idle(&self) -> bool {
        self.evaluated == 0 && self.rechecked == 0 && self.footprinted == 0
    }
}

impl std::fmt::Display for Cost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ms = |d: Duration| d.as_millis();
        write!(
            f,
            "signal {} ms · {} runs loaded in {} ms · {} followed in {} ms · {} evaluated in {} ms · {} rechecked in {} ms · auto-apply {} ms · {} footprinted in {} ms · pairs in {} ms",
            ms(self.signal),
            self.runs,
            ms(self.load),
            self.followed,
            ms(self.follow),
            self.evaluated,
            ms(self.evaluate),
            self.rechecked,
            ms(self.recheck),
            ms(self.apply),
            self.footprinted,
            ms(self.footprint),
            ms(self.pairs)
        )
    }
}

impl Tick {
    fn report(&mut self, error: &anyhow::Error) {
        self.error.get_or_insert_with(|| format!("{error:#}"));
    }
}

impl Owner {
    pub(crate) fn new(root: PathBuf, kind: SourceKind) -> Result<Self> {
        Ok(Self {
            root,
            kind,
            last_signal: None,
            work: HashMap::new(),
            checked: HashMap::new(),
            footprints: HashMap::new(),
            clean: HashMap::new(),
            shown: None,
            measured: HashSet::new(),
            scratch: tempfile::Builder::new()
                .prefix("dispatch-owner-")
                .tempdir()
                .context("failed to create the owner's scratch directory")?,
        })
    }

    pub(crate) fn tick(&mut self, state: &State) -> Tick {
        let mut tick = Tick::default();
        let started = Instant::now();
        let signal_now = match world::signal(&self.root, &self.kind) {
            Ok(signal) => Some(signal),
            Err(error) => {
                tick.report(&error);
                None
            }
        };
        tick.moved = match (&signal_now, &self.last_signal) {
            (Some(now), Some(previous)) => now != previous,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if let Some(signal_now) = &signal_now {
            self.last_signal = Some(signal_now.clone());
        }
        tick.cost.signal = started.elapsed();

        let started = Instant::now();
        let runs = self.load(state, &mut tick);
        tick.cost.load = started.elapsed();
        tick.cost.runs = runs.len();
        tick.adopted = adopt_orphans(state, &runs, &mut tick);
        let moved = tick.moved || tick.adopted;
        self.reevaluate(state, &runs, &mut tick, moved);
        let started = Instant::now();
        self.recheck_ready(state, &runs, &mut tick);
        tick.cost.recheck = started.elapsed();
        // The view must be current before anything lands: an application
        // records what it showed (`measure::landing`).
        self.interact(state, &runs, &mut tick);
        let started = Instant::now();
        for run in ready_for_auto_apply(&runs) {
            match auto_apply(state, &run.id) {
                Ok(ApplyOutcome::Applied { .. }) => {
                    tick.applied = true;
                    // The apply is the doorbell: re-observe immediately
                    // rather than waiting for the next tick's signal.
                    self.reevaluate(state, &runs, &mut tick, true);
                }
                Ok(_) => {}
                Err(error) => tick.report(&error),
            }
        }
        tick.cost.apply = started.elapsed();
        if tick.digest.is_none() {
            tick.digest = signal_now.map(|signal| signal.0);
        }
        tick.runs = if tick.adopted || tick.persisted || tick.applied {
            self.load(state, &mut tick)
        } else {
            runs
        };
        let runs = std::mem::take(&mut tick.runs);
        if tick.adopted || tick.persisted || tick.applied {
            self.interact(state, &runs, &mut tick);
        }
        self.measure(state, &runs, &mut tick);
        tick.runs = runs;
        tick
    }

    fn load(&self, state: &State, tick: &mut Tick) -> Vec<RunRecord> {
        load_source_runs(state, &self.root).unwrap_or_else(|error| {
            tick.report(&error);
            Vec::new()
        })
    }
}

fn run_lock_path(state: &State, run_id: &str) -> PathBuf {
    state.run_dir(run_id).join(".operation.lock")
}

/// Every run (any mode) whose `source_path` is `root`, loaded fresh from
/// disk/DB. A run's source never changes, so the projected metadata decides
/// which runs to load; other projects' runs are never loaded.
pub(crate) fn load_source_runs(state: &State, root: &Path) -> Result<Vec<RunRecord>> {
    let mut runs = Vec::new();
    for path in state.list_metadata_paths()? {
        let projected: RunRecord = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("invalid metadata at {}", path.display()))?;
        if projected.source_path == root {
            runs.push(state.load_run(&projected.id)?);
        }
    }
    Ok(runs)
}

/// `ExactLive` maps to `Live`, `Gone`/`Reused` to `Gone`; no owner, or a
/// liveness check that could not tell, is `Unknown` (part 14.4's step (b)).
fn live_owner_state(owner: Option<&ProcessIdentity>) -> OwnerState {
    match owner {
        None => OwnerState::Unknown,
        Some(identity) => match identity_state(identity) {
            IdentityState::ExactLive => OwnerState::Live,
            IdentityState::Gone | IdentityState::Reused => OwnerState::Gone,
            IdentityState::Unknown => OwnerState::Unknown,
        },
    }
}

/// Adopt every active attached run whose stored owner state is `Live` but
/// whose owner process is now gone: commit `attach.adopted` once and mark
/// `owner_state: Adopted`. A run whose lock is held elsewhere is skipped,
/// not forced. Returns whether anything was adopted this tick.
fn adopt_orphans(state: &State, runs: &[RunRecord], errors: &mut Tick) -> bool {
    let mut adopted = false;
    for candidate in runs {
        if candidate.mode != RunMode::Attached
            || candidate.outcome.lifecycle == LifecycleState::Finished
        {
            continue;
        }
        let is_orphaned = candidate.attachment.as_ref().is_some_and(|attachment| {
            attachment.owner_state == OwnerState::Live
                && live_owner_state(attachment.owner.as_ref()) == OwnerState::Gone
        });
        if !is_orphaned {
            continue;
        }
        let Ok(_lock) = OperationLock::acquire(
            &run_lock_path(state, &candidate.id),
            "run has a foreground owner",
        ) else {
            continue; // busy: another owner is active; skip, don't force it
        };
        let mut run = match state.load_run(&candidate.id) {
            Ok(run) => run,
            Err(error) => {
                errors.report(&error);
                continue;
            }
        };
        // Re-check under the lock: the state may have changed since the
        // unlocked scan above (finished, or already adopted).
        let still_orphaned = run.attachment.as_ref().is_some_and(|attachment| {
            attachment.owner_state == OwnerState::Live
                && live_owner_state(attachment.owner.as_ref()) == OwnerState::Gone
        });
        if run.outcome.lifecycle == LifecycleState::Finished || !still_orphaned {
            continue;
        }
        if let Some(attachment) = run.attachment.as_mut() {
            attachment.owner_state = OwnerState::Adopted;
        }
        let database = match Database::open(state.db_path()) {
            Ok(database) => database,
            Err(error) => {
                errors.report(&error);
                continue;
            }
        };
        let event = EventRecord {
            run_id: run.id.clone(),
            candidate_label: run.candidates.first().map(|c| c.label.clone()),
            event_type: "attach.adopted".into(),
            timestamp: Utc::now(),
            payload: serde_json::json!({"owner_state": "adopted"}),
            ..EventRecord::default()
        };
        if let Err(error) = persist_event(state, &database, event, &mut run) {
            errors.report(&error);
            continue;
        }
        adopted = true;
    }
    adopted
}

impl Owner {
    /// Re-evaluate every active attached run whose live owner state is not
    /// `Live` (a live wrapper owns those; part 14.4 step (b)/(c)) when the
    /// world moved or the work did: observe the world against that run's own
    /// baseline, evaluate, drop `analysis_uncertain`-only reasons, and persist
    /// through `apply::persist_verdict` what is worth recording. The first
    /// world digest observed is the tick's: `observe` hashes the current tree
    /// whichever baseline it is given.
    fn reevaluate(
        &mut self,
        state: &State,
        runs: &[RunRecord],
        tick: &mut Tick,
        world_moved: bool,
    ) {
        let (root, kind) = (self.root.as_path(), &self.kind);
        let mut followed = HashSet::new();
        for candidate in runs {
            if candidate.mode != RunMode::Attached
                || candidate.outcome.lifecycle == LifecycleState::Finished
            {
                continue;
            }
            let Some(attachment) = candidate.attachment.as_ref() else {
                continue;
            };
            if live_owner_state(attachment.owner.as_ref()) == OwnerState::Live {
                continue; // the wrapper's own business
            }
            if attachment.workspace_removed.is_some() {
                continue; // nothing left to follow
            }
            // The work's signal is its patch so far, taken without the run's
            // lock into a scratch file: `finish` takes that lock without
            // waiting, so the owner holds it only when something moved.
            let scratch = self.scratch.path().join(format!("{}.patch", candidate.id));
            if !attachment.workspace.exists() {
                // Gone without a word: keep the last Δ this owner followed.
                if let Err(error) =
                    super::attach::note_workspace_gone(state, &candidate.id, Some(&scratch))
                {
                    tick.report(&error);
                }
                tick.persisted = true;
                continue;
            }
            let started = Instant::now();
            let index = scratch.with_extension("index");
            let work = source::snapshot_delta_indexed(
                &candidate.baseline_path,
                &attachment.workspace,
                &scratch,
                &index,
            )
            .and_then(|()| Ok(hex::encode(Sha256::digest(fs::read(&scratch)?))));
            tick.cost.follow += started.elapsed();
            tick.cost.followed += 1;
            let work = match work {
                Ok(work) => {
                    tick.followed.insert(candidate.id.clone());
                    work
                }
                Err(error) => {
                    tick.report(&error);
                    continue;
                }
            };
            followed.insert(candidate.id.clone());
            if !world_moved && self.work.get(&candidate.id) == Some(&work) {
                continue;
            }
            let Ok(_lock) = OperationLock::acquire(
                &run_lock_path(state, &candidate.id),
                "run has a foreground owner",
            ) else {
                continue; // busy: skip this tick, don't force it
            };
            let mut run = match state.load_run(&candidate.id) {
                Ok(run) => run,
                Err(error) => {
                    tick.report(&error);
                    continue;
                }
            };
            if run.outcome.lifecycle == LifecycleState::Finished {
                continue; // finished meanwhile (e.g. `dispatch finish` raced us)
            }
            let Some(attachment) = run.attachment.clone() else {
                continue;
            };
            if live_owner_state(attachment.owner.as_ref()) == OwnerState::Live {
                continue;
            }
            let started = Instant::now();
            let delta_path = state.run_dir(&run.id).join("delta-live.patch");
            if let Err(error) =
                source::snapshot_delta(&run.baseline_path, &attachment.workspace, &delta_path)
            {
                tick.report(&error);
                continue;
            }
            let world = match world::observe(root, &run.baseline_path, &run.baseline_commit, kind) {
                Ok(world) => world,
                Err(error) => {
                    tick.report(&error);
                    continue;
                }
            };
            tick.digest.get_or_insert_with(|| world.digest.clone());
            let validity = match coherence::evaluate(
                &world,
                &WorkView {
                    source: root,
                    delta_patch: &delta_path,
                    baseline: &run.baseline_path,
                    baseline_commit: &run.baseline_commit,
                },
            ) {
                Ok(validity) => validity,
                Err(error) => {
                    tick.report(&error);
                    continue;
                }
            };
            tick.cost.evaluate += started.elapsed();
            tick.cost.evaluated += 1;
            // Compared with the verdict stored on the run, not with anything
            // this process remembers: a restarted owner still records a change.
            let validity = coherence::watch::settle(validity);
            let stored = run.coherence.as_ref().and_then(|c| c.validity.as_ref());
            if !worth_storing(stored, &validity) {
                self.work.insert(run.id.clone(), work);
                continue;
            }
            let database = match Database::open(state.db_path()) {
                Ok(database) => database,
                Err(error) => {
                    tick.report(&error);
                    continue;
                }
            };
            if let Err(error) = apply::persist_verdict(state, &database, &mut run, &validity) {
                tick.report(&error);
                continue;
            }
            self.work.insert(run.id.clone(), work);
            tick.persisted = true;
        }
        self.work.retain(|id, _| followed.contains(id));
    }
}

impl Owner {
    /// Derive each unintegrated Work's footprint from its own (S0, Δ), compare
    /// every pair, and publish where they interact for `watch` and `status`.
    /// Advisory only: nothing here changes a verdict, a lock or a decision. A
    /// footprint is derived again only when its Δ changed.
    fn interact(&mut self, state: &State, runs: &[RunRecord], tick: &mut Tick) {
        let mut works = Vec::new();
        let mut participants = Vec::new();
        for run in runs {
            let Some((patch, frozen)) = self.delta_of(run, tick) else {
                continue;
            };
            let digest = match fs::read(&patch) {
                Ok(bytes) => hex::encode(Sha256::digest(&bytes)),
                Err(error) => {
                    tick.report(&error.into());
                    continue;
                }
            };
            let known = self
                .footprints
                .get(&run.id)
                .filter(|(derived_from, _, _)| *derived_from == (digest.clone(), frozen))
                .map(|(_, footprint, analysis)| (footprint.clone(), *analysis));
            let (footprint, analysis) = match known {
                Some(known) => known,
                None => {
                    let started = Instant::now();
                    let fresh = match interactions::footprint(&WorkView {
                        source: &self.root,
                        delta_patch: &patch,
                        baseline: &run.baseline_path,
                        baseline_commit: &run.baseline_commit,
                    }) {
                        Ok(fresh) => fresh,
                        Err(error) => {
                            tick.report(&error);
                            continue;
                        }
                    };
                    if fresh.unparsed.is_empty() {
                        self.clean.insert(run.id.clone(), fresh.clone());
                    }
                    let (footprint, analysis) =
                        interactions::settle(fresh, frozen, self.clean.get(&run.id));
                    tracing::trace!(
                        "footprint {}: {:?}, writes {:?}, reads {:?}, text {:?}, {} unresolved",
                        run.id,
                        analysis,
                        footprint.writes,
                        footprint.reads,
                        footprint.text.keys().collect::<Vec<_>>(),
                        footprint.unresolved
                    );
                    self.footprints.insert(
                        run.id.clone(),
                        ((digest.clone(), frozen), footprint.clone(), analysis),
                    );
                    tick.cost.footprinted += 1;
                    tick.cost.footprint += started.elapsed();
                    (footprint, analysis)
                }
            };
            participants.push(Participant {
                run_id: run.id.clone(),
                delta_sha256: digest,
                analysis,
                unresolved: footprint.unresolved,
            });
            works.push((run.id.clone(), footprint));
        }
        let ids: HashSet<String> = participants.iter().map(|p| p.run_id.clone()).collect();
        self.footprints.retain(|id, _| ids.contains(id));
        self.clean.retain(|id, _| ids.contains(id));
        let started = Instant::now();
        let edges: Vec<Edge> = interactions::edges(&works)
            .into_iter()
            .map(|(a, b, interactions)| Edge {
                a: a.to_owned(),
                b: b.to_owned(),
                interactions,
            })
            .collect();
        tick.cost.pairs = started.elapsed();
        if self
            .shown
            .as_ref()
            .is_some_and(|shown| shown.participants == participants && shown.edges == edges)
        {
            return;
        }
        let projection = Projection {
            version: 1,
            computed_at: Utc::now(),
            participants,
            edges,
        };
        match super::background::write_interactions(state, &self.root, &projection) {
            Ok(()) => {
                self.shown = Some(projection);
                tick.interactions = true;
            }
            Err(error) => tick.report(&error),
        }
    }

    /// The interaction view as this owner last published it.
    pub(crate) fn projection(&self) -> Option<&Projection> {
        self.shown.as_ref()
    }

    /// For each recent landing in this project, record Dispatch's first
    /// evaluation of every counterpart the landing listed, once
    /// (`interaction.outcome`, `docs/plan-0.4.9.md` §3.3). The evaluation is the
    /// one `check` makes, against the source now, and never stored as the
    /// counterpart's verdict. Whatever it finds is recorded, CONTINUE or an
    /// unscorable class alike; nothing waits for a better moment. Outcomes are
    /// written on the landed run alone, under its lock, so live runs keep a
    /// single writer. A busy lock is tried again on the next tick.
    fn measure(&mut self, state: &State, runs: &[RunRecord], tick: &mut Tick) {
        let landed: Vec<&RunRecord> = runs
            .iter()
            .filter(|run| {
                run.outcome.application == ApplicationState::Applied
                    && !self.measured.contains(&run.id)
            })
            .collect();
        if landed.is_empty() {
            return;
        }
        let database = match Database::open(state.db_path()) {
            Ok(database) => database,
            Err(error) => return tick.report(&error),
        };
        for run in landed {
            match self.measure_landing(state, &database, run, runs, tick) {
                Ok(true) => {
                    self.measured.insert(run.id.clone());
                }
                Ok(false) => {}
                Err(error) => tick.report(&error),
            }
        }
    }

    /// Record the missing outcomes of `landed`'s landing; `true` once none is
    /// missing, or the landing is older than a day, or there is none.
    fn measure_landing(
        &mut self,
        state: &State,
        database: &Database,
        landed: &RunRecord,
        runs: &[RunRecord],
        tick: &mut Tick,
    ) -> Result<bool> {
        let events = database.events_for_run(&landed.id)?;
        let Some(event) = events.iter().find(|e| e.event_type == measure::LANDED) else {
            return Ok(true);
        };
        if Utc::now() - event.timestamp > chrono::Duration::hours(24) {
            return Ok(true);
        }
        let landing: Landing = serde_json::from_value(event.payload.clone())?;
        let recorded = recorded_outcomes(&events);
        let missing: Vec<&Counterpart> = landing
            .counterparts
            .iter()
            .filter(|c| !recorded.contains(&c.identity.run_id))
            .collect();
        if missing.is_empty() {
            return Ok(true);
        }
        let outcomes: Vec<Outcome> = missing
            .into_iter()
            .map(|counterpart| {
                let current = runs.iter().find(|r| r.id == counterpart.identity.run_id);
                let evaluation = match current
                    .and_then(|run| self.delta_of(run, tick).map(|(patch, _)| (run, patch)))
                {
                    None => Evaluation::Gone,
                    Some((run, patch)) => self.evaluate_counterpart(run, &patch),
                };
                measure::outcome::outcome(&landing, counterpart, evaluation, Utc::now())
            })
            .collect();
        let Ok(_lock) = OperationLock::acquire(
            &run_lock_path(state, &landed.id),
            "run has a foreground owner",
        ) else {
            return Ok(false); // busy: try again on the next tick
        };
        let mut run = state.load_run(&landed.id)?;
        // Read again under the lock: another writer may have recorded some.
        let recorded = recorded_outcomes(&database.events_for_run(&landed.id)?);
        for outcome in outcomes {
            if recorded.contains(&outcome.counterpart_run_id) {
                continue;
            }
            persist_event(
                state,
                database,
                EventRecord {
                    run_id: run.id.clone(),
                    candidate_label: run.applied_candidate.clone(),
                    event_type: measure::OUTCOME.into(),
                    timestamp: Utc::now(),
                    payload: serde_json::to_value(&outcome)?,
                    ..EventRecord::default()
                },
                &mut run,
            )?;
            tick.persisted = true;
        }
        Ok(true)
    }

    /// The coherence verdict on a counterpart's Δ against the source now.
    fn evaluate_counterpart(&self, run: &RunRecord, patch: &Path) -> Evaluation {
        let delta_sha256 = fs::read(patch)
            .ok()
            .map(|bytes| hex::encode(Sha256::digest(&bytes)));
        let world = world::observe(
            &self.root,
            &run.baseline_path,
            &run.baseline_commit,
            &self.kind,
        );
        let evaluated = world
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{e:#}"))
            .and_then(|world| {
                coherence::evaluate(
                    world,
                    &WorkView {
                        source: &self.root,
                        delta_patch: patch,
                        baseline: &run.baseline_path,
                        baseline_commit: &run.baseline_commit,
                    },
                )
            });
        match (evaluated, delta_sha256) {
            (Ok(validity), Some(delta_sha256)) => Evaluation::Evaluated {
                world_digest: validity.world_digest,
                delta_sha256,
                decision: validity.decision,
                reasons: validity.reasons.iter().map(|reason| reason.code).collect(),
            },
            (evaluated, delta_sha256) => Evaluation::Failed {
                error: evaluated.err().map_or_else(
                    || "its patch could not be read".into(),
                    |e| format!("{e:#}"),
                ),
                world_digest: world.ok().map(|world| world.digest),
                delta_sha256,
            },
        }
    }

    /// The Δ that stands for `run` among Work not yet integrated, and whether
    /// it is frozen: the kept patch of a delivered result or of a workspace
    /// removed with its exact changes; the workspace as it is now for live
    /// Work, snapshotted into the owner's scratch unless this tick already
    /// did. Applied, closed and lost Work never lands, so it has none.
    fn delta_of(&self, run: &RunRecord, tick: &mut Tick) -> Option<(PathBuf, bool)> {
        let [candidate] = run.candidates.as_slice() else {
            return None;
        };
        if coherence::is_ready_unapplied(run) && run.outcome.review == ReviewState::Pending {
            return Some((candidate.diff_path.clone(), true));
        }
        if run.outcome.lifecycle != LifecycleState::Working {
            return None;
        }
        let workspace = match (&run.mode, &run.attachment) {
            (RunMode::Attached, Some(attachment)) => match &attachment.workspace_removed {
                Some(removal) if removal.exact => return Some((candidate.diff_path.clone(), true)),
                Some(_) => return None,
                None => attachment.workspace.clone(),
            },
            (RunMode::Attached, None) => return None,
            (RunMode::Native, _) => candidate.workspace_path.clone(),
        };
        let scratch = self.scratch.path().join(format!("{}.patch", run.id));
        if !tick.followed.contains(&run.id) {
            if !workspace.exists() {
                return None;
            }
            let started = Instant::now();
            if let Err(error) = source::snapshot_delta_indexed(
                &run.baseline_path,
                &workspace,
                &scratch,
                &scratch.with_extension("index"),
            ) {
                tick.report(&error);
                return None;
            }
            tick.cost.follow += started.elapsed();
            tick.cost.followed += 1;
            tick.followed.insert(run.id.clone());
        }
        Some((scratch, false))
    }

    /// Keep the stored verdict of every Ready result awaiting review, native
    /// or attached, what `check` would show: `coherence::live_validity`, which
    /// keeps a refusal by the merged-tree checks for as long as the world has
    /// not moved. A result is checked when it becomes Ready and again whenever
    /// the world signal moves. The evaluation takes no lock; the run's lock is
    /// held only to record a verdict that is worth storing, and only if the
    /// run has not changed since it was read.
    fn recheck_ready(&mut self, state: &State, runs: &[RunRecord], tick: &mut Tick) {
        let Some(signal) = self.last_signal.clone() else {
            return;
        };
        let mut waiting = HashSet::new();
        for candidate in runs {
            if !awaits_review(candidate) {
                continue;
            }
            waiting.insert(candidate.id.clone());
            if self.checked.get(&candidate.id) == Some(&signal) {
                continue;
            }
            // An evaluation that fails is logged by `live_validity` and
            // tried again when the world next moves.
            tick.cost.rechecked += 1;
            let Some(validity) = coherence::live_validity(candidate) else {
                self.checked.insert(candidate.id.clone(), signal.clone());
                continue;
            };
            let stored = candidate
                .coherence
                .as_ref()
                .and_then(|c| c.validity.as_ref());
            if !worth_storing(stored, &validity) {
                self.checked.insert(candidate.id.clone(), signal.clone());
                continue;
            }
            let Ok(_lock) = OperationLock::acquire(
                &run_lock_path(state, &candidate.id),
                "run has a foreground owner",
            ) else {
                continue; // busy: a review or apply is under way; try next tick
            };
            let recorded = state.load_run(&candidate.id).and_then(|mut run| {
                if run.state_revision != candidate.state_revision {
                    return Ok(false); // changed since it was read; next tick
                }
                let database = Database::open(state.db_path())?;
                apply::persist_verdict(state, &database, &mut run, &validity)?;
                Ok(true)
            });
            match recorded {
                Ok(true) => {
                    self.checked.insert(candidate.id.clone(), signal.clone());
                    tick.persisted = true;
                }
                Ok(false) => {}
                Err(error) => tick.report(&error),
            }
        }
        self.checked.retain(|id, _| waiting.contains(id));
    }
}

/// Active attached Work whose workspace was removed with its exact changes
/// kept and that carries the authority to run checks: a candidate for
/// `attach::finish_by_consent`, which checks the consent itself.
fn awaits_finish_by_consent(run: &RunRecord) -> bool {
    run.outcome.lifecycle == LifecycleState::Working
        && run.environment.unsafe_local
        && run
            .attachment
            .as_ref()
            .and_then(|attachment| attachment.workspace_removed.as_ref())
            .is_some_and(|removal| removal.exact)
}

/// A Ready result nobody has accepted, rejected or applied yet.
fn awaits_review(run: &RunRecord) -> bool {
    run.outcome.review == ReviewState::Pending && coherence::is_ready_unapplied(run)
}

/// Whether the owner stores `validity` over `stored`: when it says something
/// new (`worth_recording`), the first time the Work is checked at all, and
/// when the world first moves under it, so the view never says "not checked"
/// or "unmoved" about something it has seen move.
/// The counterparts whose outcome a landed run already records.
fn recorded_outcomes(events: &[EventRecord]) -> HashSet<String> {
    events
        .iter()
        .filter(|event| event.event_type == measure::OUTCOME)
        .filter_map(|event| {
            event.payload["counterpart_run_id"]
                .as_str()
                .map(str::to_owned)
        })
        .collect()
}

fn worth_storing(stored: Option<&Validity>, validity: &Validity) -> bool {
    stored.is_none_or(|stored| stored.world_changed != validity.world_changed)
        || coherence::watch::worth_recording(stored, validity)
}

/// Every attached run ready to auto-apply: finished, `Ready`, unreviewed,
/// unapplied, and `capabilities.integrate`.
fn ready_for_auto_apply(runs: &[RunRecord]) -> impl Iterator<Item = &RunRecord> {
    runs.iter().filter(|run| {
        run.mode == RunMode::Attached
            && run.outcome.lifecycle == LifecycleState::Finished
            && run.outcome.work_result == WorkResult::Ready
            && run.outcome.review == ReviewState::Pending
            && run.outcome.application == ApplicationState::NotApplied
            && run
                .attachment
                .as_ref()
                .is_some_and(|attachment| attachment.capabilities.integrate)
    })
}

/// The view's line for one run: the shared Work line from its stored
/// validity. Native runs' verdicts are never recomputed here.
fn describe(run: &RunRecord) -> (WorkLine, Option<Decision>) {
    let validity = run.coherence.as_ref().and_then(|c| c.validity.as_ref());
    (work_line(run, validity), validity.map(|v| v.decision))
}

/// Runs whose `source_path` is `root` and that are still active, still wait
/// for your review, or finished within the last hour (part 4/14.11 of the
/// view), sorted for a stable redraw.
fn view_rows(runs: &[RunRecord]) -> Vec<&RunRecord> {
    let now = Utc::now();
    let mut rows: Vec<&RunRecord> = runs
        .iter()
        .filter(|run| {
            run.outcome.lifecycle != LifecycleState::Finished
                || awaits_review(run)
                || run
                    .completed_at
                    .is_some_and(|at| now - at < chrono::Duration::hours(1))
        })
        .collect();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    rows
}

/// The project view's rows for `watch`'s interactive form: each Work item's
/// id and its line, in the view's order.
pub(crate) fn project_rows(
    runs: &[RunRecord],
    interactions: Option<&Projection>,
) -> Vec<(String, String)> {
    let rows = view_rows(runs);
    let ids = short_ids(&rows);
    rows.into_iter()
        .map(|run| (run.id.clone(), row(run, ids[run.id.as_str()], interactions)))
        .collect()
}

fn short_ids<'a>(rows: &[&'a RunRecord]) -> HashMap<&'a str, &'a str> {
    let ids: Vec<&str> = rows.iter().map(|run| run.id.as_str()).collect();
    crate::state::short_ids(&ids)
}

/// One row of the project view: the Work line, then whom it interacts with,
/// kept apart from the verdict.
fn row(run: &RunRecord, id: &str, interactions: Option<&Projection>) -> String {
    let mut line = format!("{id} · {}", describe(run).0);
    if let Some(summary) = interactions.and_then(|p| p.summary(&run.id)) {
        line.push_str(" · ");
        line.push_str(&summary);
    }
    line
}

/// The project view: what is shown, so each tick prints only what changed.
/// Shared by `serve` in the foreground and `watch`.
#[derive(Default)]
pub(crate) struct View {
    shown: HashMap<String, String>,
    last_block: Vec<String>,
    last_header: Option<String>,
}

impl View {
    /// Print the project view under an optional header line. `--json`
    /// emits one `{"type":"work",...}` object per run whose displayed fields
    /// changed since the last tick, and a `{"type":"watcher",...}` object
    /// when the header changed; otherwise the whole block is printed,
    /// redrawn in place on a TTY and appended as lines otherwise.
    pub(crate) fn render(
        &mut self,
        header: Option<&str>,
        runs: &[RunRecord],
        json: bool,
        interactions: Option<&Projection>,
    ) {
        if json && header.is_some() && self.last_header.as_deref() != header {
            println!(
                "{}",
                serde_json::json!({"type": "watcher", "watcher": header})
            );
        }
        self.last_header = header.map(str::to_owned);
        let (shown, last_block) = (&mut self.shown, &mut self.last_block);
        let rows = view_rows(runs);

        if json {
            for run in rows {
                let (line, decision) = describe(run);
                // `null` while nothing watches the project: not known.
                let concurrent = interactions.map(|p| p.json(&run.id));
                let key = format!("{line} {concurrent:?}");
                if shown.get(&run.id) != Some(&key) {
                    println!(
                        "{}",
                        serde_json::json!({
                            "type": "work",
                            "run_id": run.id,
                            "agent": line.agent,
                            "verdict": decision,
                            "state": line.state,
                            "reason": line.reason.as_deref().unwrap_or("—"),
                            "origin": line.origin,
                            "s0": line.s0,
                            "verification": line.verification,
                            "review": line.review,
                            "applied_by": line.applied_by,
                            "overridden": line.verdict == "overridden",
                            "interactions": concurrent,
                        })
                    );
                    shown.insert(run.id.clone(), key);
                }
            }
            return;
        }

        let empty = header.is_some() && rows.is_empty();
        let lines: Vec<String> = header
            .map(str::to_owned)
            .into_iter()
            .chain({
                let ids = short_ids(&rows);
                rows.iter()
                    .map(move |run| row(run, ids[run.id.as_str()], interactions))
                    .collect::<Vec<_>>()
            })
            .chain(empty.then(|| "no Work in the last hour".to_owned()))
            .collect();

        // Rendered every tick, so redraw only when the block would differ.
        if *last_block == lines {
            return;
        }
        let tty = std::io::stdout().is_terminal();
        if tty && !last_block.is_empty() {
            print!("\x1b[{}A\x1b[0J", last_block.len());
        }
        for line in &lines {
            println!("{line}");
        }
        *last_block = lines;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_owner_state_maps_identity_outcomes() {
        assert_eq!(live_owner_state(None), OwnerState::Unknown);
    }

    #[test]
    fn the_first_move_of_the_world_is_stored_even_when_the_verdict_holds() {
        let validity = |world_changed| Validity {
            decision: Decision::Continue,
            evaluated_at: Utc::now(),
            world_digest: format!("{world_changed}"),
            world_changed,
            changed_files: u32::from(world_changed),
            reasons: Vec::new(),
            analysis: crate::AnalysisLevel::FilesOnly,
        };
        assert!(worth_storing(None, &validity(false)));
        assert!(worth_storing(Some(&validity(false)), &validity(true)));
        assert!(!worth_storing(Some(&validity(true)), &validity(true)));
        assert!(!worth_storing(Some(&validity(false)), &validity(false)));
    }

    #[test]
    fn a_result_awaiting_review_stays_in_the_view() {
        let mut waiting: RunRecord = serde_json::from_value(serde_json::json!({
            "id":"waiting", "task":"t", "exact_prompt":"t",
            "source_path":"/source", "source_kind":"directory", "source_git_head":null,
            "source_fingerprint":"f", "baseline_path":"/baseline", "baseline_commit":"abc",
            "status":"ready_for_evaluation", "created_at":"2026-09-17T00:00:00Z",
            "completed_at":"2026-09-17T00:10:00Z",
            "environment":{"dispatch_version":"test","os":"test","architecture":"test","execution_backend":"local","timeout_secs":30,"cpus":1.0,"memory":"1g","max_parallel":1},
            "evaluation":null,"applied_candidate":null
        }))
        .unwrap();
        waiting.outcome.lifecycle = LifecycleState::Finished;
        waiting.outcome.work_result = WorkResult::Ready;
        waiting.outcome.review = ReviewState::Pending;
        let mut reviewed = waiting.clone();
        reviewed.id = "reviewed".into();
        reviewed.outcome.review = ReviewState::Rejected;
        let runs = [waiting, reviewed];
        let rows: Vec<&str> = view_rows(&runs).iter().map(|run| run.id.as_str()).collect();
        assert_eq!(rows, ["waiting"]);
    }
}
