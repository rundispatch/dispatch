//! Registering a runtime session as Work, without silent gaps (0.4.10).
//! Contract: `docs/plan-0.4.10.md` §3. Owned by packet A after P0; the
//! integrator does not edit this file while A owns it.
//!
//! An eligible session (its own worktree, in a watched project) ends in exactly
//! one of:
//! - **registered**: its Work was published (record and `runs/<id>/`) before the
//!   hook returned;
//! - **pending**: its S0 is durable in `registrations/<id>/registration.json`
//!   but the Work is not yet published; the hook or the project owner
//!   publishes it;
//! - **untracked**: no durable S0 by the deadline. The session is told so, and
//!   nothing can publish it afterwards.
//!
//! Work is assembled in `<state>/registrations/<id>/`, never in `runs/`, and
//! published by a database insert plus a rename into `runs/<id>/`. Publishing is
//! idempotent. The single `decision` file in the registration directory is
//! created exclusively (`create_new`), so the first writer wins: the worker
//! writes `publishing` (only after `registration.json` is durable), the deadline
//! writes `pending` or `untracked`. Whoever loses must not contradict it.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::attach::{self, AttachRequest, Registered, RuntimeStart};
use crate::{
    RuntimeSession,
    db::Database,
    lock::OperationLock,
    source,
    state::{State, write_durably},
};

/// The whole `SessionStart` path, from the hook process starting, must decide
/// within this. `dispatch setup` installs `SessionStart` with a 60 s timeout;
/// the margin covers process start and the reply under load.
pub const BUDGET: Duration = Duration::from_secs(40);

/// Under the state root: one directory per registration in progress.
pub const DIRECTORY: &str = "registrations";
/// The durable S0 boundary: once this exists, the session's Work can always be
/// published.
pub const RECORD: &str = "registration.json";
/// Created exclusively; holds a `Decision`.
pub const DECISION: &str = "decision";

pub const NOTICE_UNTRACKED: &str = "Dispatch could not capture this session's starting state in \
     time. This session is not being tracked.";

/// What a pending session is told; `id` is the short Work id.
pub fn notice_pending(id: &str) -> String {
    format!(
        "Dispatch captured this session's starting state and will finish tracking it as \
         Work {id} shortly; see it with dispatch watch."
    )
}

/// Everything needed to publish the Work later, from S0 alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    pub version: u32,
    pub run_id: String,
    pub workspace: PathBuf,
    pub root: PathBuf,
    /// The commit `source::world_commit` made of the workspace: S0.
    pub s0_commit: String,
    pub session: RuntimeSession,
    pub resumed: bool,
    /// Valid project check consent when the session started.
    pub consented: bool,
    /// When the hook process started: registration latency is measured from it.
    pub started_at: DateTime<Utc>,
}

/// The content of the `decision` file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// The worker, after `registration.json` was durable, about to publish.
    Publishing,
    /// The deadline, with a durable S0 and nothing published: publish later.
    Pending,
    /// The deadline, without a durable S0: never publish.
    Untracked,
}

/// What a `SessionStart` ended in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Registered { run_id: String },
    Pending { run_id: String },
    Untracked,
}

/// The directory of registration `run_id`.
pub fn directory(state: &State, run_id: &str) -> PathBuf {
    state.root.join(DIRECTORY).join(run_id)
}

/// Held by whoever is building or publishing a registration; it moves into
/// `runs/<id>/` with the rest.
const LOCK: &str = "registration.lock";
/// Written once by the owner beside a registration it cannot publish; the
/// registration is kept for inspection and not tried again.
const FAILED: &str = "failed";

/// `BUDGET`, or in debug builds `DISPATCH_REGISTRATION_BUDGET_MS` (tests).
pub(crate) fn budget() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("DISPATCH_REGISTRATION_BUDGET_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
    {
        return Duration::from_millis(ms);
    }
    BUDGET
}

/// Debug builds only: `DISPATCH_REGISTRATION_FAULT=<boundary>` ends this
/// process at `boundary` at once, with no reply and no cleanup;
/// `<boundary>:sleep` stalls there instead.
pub(crate) fn fault(boundary: &str) {
    #[cfg(debug_assertions)]
    if let Ok(fault) = std::env::var("DISPATCH_REGISTRATION_FAULT") {
        if fault == boundary {
            std::process::exit(86);
        }
        if fault.strip_suffix(":sleep") == Some(boundary) {
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
    }
    let _ = boundary;
}

/// A registration directory this process made and holds: nobody else acts on
/// it while the lock is held.
pub(crate) struct Building {
    path: PathBuf,
    _lock: OperationLock,
}

impl Building {
    /// Remove what was built; nothing was published.
    pub(crate) fn remove(self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Make and hold the registration directory of `run_id`.
pub(crate) fn begin(state: &State, run_id: &str) -> Result<Building> {
    state.initialize()?;
    let path = directory(state, run_id);
    fs::create_dir_all(&path).with_context(|| format!("failed to create {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for private in [state.root.join(DIRECTORY), path.clone()] {
            fs::set_permissions(&private, fs::Permissions::from_mode(0o700))?;
        }
    }
    let lock = OperationLock::acquire(&path.join(LOCK), "this registration is in use")?;
    Ok(Building { path, _lock: lock })
}

/// Create the `decision` file with `decision` unless one exists. Returns
/// whether this call decided.
fn claim(dir: &Path, decision: Decision) -> Result<bool> {
    fs::create_dir_all(dir)?;
    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(DECISION))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => return Err(error).context("failed to record the registration's decision"),
    };
    file.write_all(&serde_json::to_vec(&decision)?)?;
    file.sync_all()?;
    fs::File::open(dir)?.sync_all()?;
    Ok(true)
}

/// The decision in `dir`. An empty file is a writer that died before writing,
/// which cannot have replied to anyone, so it decided nothing.
fn decision(dir: &Path) -> Option<Decision> {
    serde_json::from_slice(&fs::read(dir.join(DECISION)).ok()?).ok()
}

fn read_record(dir: &Path) -> Result<Option<Registration>> {
    match fs::read(dir.join(RECORD)) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).context("unreadable registration.json")?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("failed to read registration.json"),
    }
}

/// One lock per workspace: a session start, a later hook and the owner never
/// act on one workspace at once.
fn workspace_lock(state: &State, workspace: &Path) -> PathBuf {
    let key = hex::encode(Sha256::digest(workspace.to_string_lossy().as_bytes()));
    state
        .root
        .join("locks")
        .join(format!("workspace-{key}.lock"))
}

/// Registration directories, by run id.
fn entries(state: &State) -> Result<Vec<(String, PathBuf)>> {
    let parent = state.root.join(DIRECTORY);
    let Ok(listing) = fs::read_dir(&parent) else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    for entry in listing {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if ulid::Ulid::from_string(&name).is_ok() && entry.file_type()?.is_dir() {
            found.push((name, entry.path()));
        }
    }
    found.sort();
    Ok(found)
}

/// How long ago registration `run_id` began: its id is a ULID made then.
fn age(run_id: &str) -> Duration {
    ulid::Ulid::from_string(run_id).map_or(Duration::ZERO, |id| {
        SystemTime::now()
            .duration_since(id.datetime())
            .unwrap_or_default()
    })
}

fn log(run_id: &str, message: &str) {
    eprintln!(
        "{} registration {run_id}: {message}",
        Utc::now().to_rfc3339()
    );
}

/// What the worker thread of a session start ended in.
enum Started {
    /// The workspace already had Work; the session is recorded on it.
    Existing,
    Published,
    /// The deadline decided first; the worker stopped.
    Lost,
}

/// Register a session starting in its own worktree of a watched project
/// (`docs/plan-0.4.10.md` §3), deciding by `BUDGET` after `began`, the
/// hook process's start. `registration` is complete but for `s0_commit` and
/// `consented`, which the worker fills in. `None`: the workspace already had
/// Work and the session was recorded on it. An error is untracked, with its
/// reason.
pub(crate) fn register(
    state: &State,
    registration: Registration,
    began: Instant,
) -> Result<Option<Outcome>> {
    let run_id = registration.run_id.clone();
    let building = begin(state, &run_id)?;
    let (sender, receiver) = mpsc::channel();
    let worker_state = state.clone();
    std::thread::spawn(move || {
        let _ = sender.send(work(&worker_state, registration));
    });
    let budget = budget();
    let failure = match receiver.recv_timeout(budget.saturating_sub(began.elapsed())) {
        Ok(Ok(Started::Existing)) => {
            building.remove();
            return Ok(None);
        }
        Ok(Ok(Started::Published)) => return Ok(Some(Outcome::Registered { run_id })),
        Ok(Ok(Started::Lost)) | Err(_) => None,
        Ok(Err(error)) => Some(error),
    };
    // The worker may still be running after the reply: the registration stays
    // held until this process exits, so nobody mistakes it for abandoned.
    std::mem::forget(building);
    let outcome = conclude(state, &run_id, &receiver, budget / 4)?;
    match (outcome, failure) {
        (Outcome::Untracked, Some(error)) => Err(error),
        (outcome, _) => Ok(Some(outcome)),
    }
}

/// The worker: S0, `registration.json`, the run, the decision, publishing.
fn work(state: &State, mut registration: Registration) -> Result<Started> {
    let workspace = registration.workspace.clone();
    let _lock = OperationLock::acquire_wait(
        &workspace_lock(state, &workspace),
        "another session is registering this workspace",
        Duration::from_secs(10),
    )?;
    fault("after_lock");
    publish_durable(state, &workspace)?;
    if let Some(run_id) = attach::find_active_attachment(state, &workspace)? {
        attach::record_session_start(state, &run_id, registration.session)?;
        return Ok(Started::Existing);
    }
    // Authority to run the project's checks comes only from the person's
    // consent for exactly these checks, never from the session.
    registration.consented = crate::consent::consent(state, &registration.root)?.is_valid();
    registration.s0_commit = source::world_commit(&workspace)?;
    fault("after_s0_commit");
    let prepared = attach::prepare(state, request(&registration))?;
    let dir = directory(state, &registration.run_id);
    write_durably(
        &dir.join(RECORD),
        &serde_json::to_vec_pretty(&registration)?,
    )?;
    fault("after_registration_json");
    let built = attach::build(state, &prepared, &registration.run_id)?;
    if !claim(&dir, Decision::Publishing)? {
        return Ok(Started::Lost);
    }
    fault("after_decision");
    attach::publish(
        state,
        &registration.run_id,
        Some(built),
        Some(Registered {
            started_at: registration.started_at,
            via: "hook",
        }),
    )?;
    Ok(Started::Published)
}

/// The deadline passed, or the worker gave up: decide pending or untracked,
/// unless the worker decided to publish first, in which case wait for it up
/// to `margin`.
fn conclude(
    state: &State,
    run_id: &str,
    worker: &mpsc::Receiver<Result<Started>>,
    margin: Duration,
) -> Result<Outcome> {
    let dir = directory(state, run_id);
    let durable = || {
        if dir.join(RECORD).is_file() {
            Decision::Pending
        } else {
            Decision::Untracked
        }
    };
    let wanted = durable();
    let decided = if claim(&dir, wanted)? {
        wanted
    } else {
        // Only this process's worker writes here, and it is mid-write at most.
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match decision(&dir) {
                Some(decided) => break decided,
                None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                None => break durable(),
            }
        }
    };
    let run_id = run_id.to_owned();
    Ok(match decided {
        Decision::Untracked => Outcome::Untracked,
        Decision::Pending => Outcome::Pending { run_id },
        Decision::Publishing => {
            let published = matches!(worker.recv_timeout(margin), Ok(Ok(Started::Published)))
                || (!dir.exists() && state.run_dir(&run_id).is_dir());
            if published {
                Outcome::Registered { run_id }
            } else {
                Outcome::Pending { run_id }
            }
        }
    })
}

/// The attach request that builds a registration's Work from its S0.
fn request(registration: &Registration) -> AttachRequest {
    let provider = registration.session.provider.clone();
    let name = registration
        .workspace
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    AttachRequest {
        workspace: registration.workspace.clone(),
        root: Some(registration.root.clone()),
        task: Some(format!("{provider} session in {name}")),
        agent: Some(provider),
        pid: None,
        command: None,
        allow_unsafe_local: registration.consented,
        auto_apply: false,
        runtime: Some(RuntimeStart {
            session: registration.session.clone(),
            resumed: registration.resumed,
            commit: registration.s0_commit.clone(),
        }),
    }
}

/// Before any hook acts on `workspace`: publish its registrations whose S0 is
/// durable, under the workspace lock, waiting for it at most `wait`.
pub(crate) fn publish_first(state: &State, workspace: &Path, wait: Duration) -> Result<()> {
    let mut durable = false;
    for (_, dir) in entries(state)? {
        if let Ok(Some(registration)) = read_record(&dir) {
            durable |= registration.workspace == workspace;
        }
    }
    if !durable {
        return Ok(());
    }
    let _lock = OperationLock::acquire_wait(
        &workspace_lock(state, workspace),
        "another session is registering this workspace",
        wait,
    )?;
    publish_durable(state, workspace)
}

/// `publish_first`, with the workspace lock held.
fn publish_durable(state: &State, workspace: &Path) -> Result<()> {
    for (_, dir) in entries(state)? {
        if let Ok(Some(registration)) = read_record(&dir)
            && registration.workspace == workspace
        {
            settle(state, &dir, &registration, "hook")?;
        }
    }
    Ok(())
}

enum Settled {
    Published,
    /// Its S0 can no longer be read; never published.
    Failed(String),
    Untouched,
}

/// Publish a registration whose S0 is durable unless it is untracked, building
/// its Work from S0 when nothing is recorded yet. The caller holds the
/// workspace lock.
fn settle(
    state: &State,
    dir: &Path,
    registration: &Registration,
    via: &'static str,
) -> Result<Settled> {
    let run_id = &registration.run_id;
    let Ok(_held) = OperationLock::acquire(&dir.join(LOCK), "this registration is in use") else {
        return Ok(Settled::Untouched);
    };
    if !dir.join(RECORD).is_file() {
        // Published or removed since it was read; taking the lock made the
        // directory again.
        let _ = fs::remove_dir_all(dir);
        return Ok(Settled::Untouched);
    }
    if dir.join(FAILED).exists() {
        return Ok(Settled::Untouched);
    }
    let decided = match decision(dir) {
        Some(decided) => decided,
        None if claim(dir, Decision::Publishing)? => Decision::Publishing,
        None => decision(dir).unwrap_or(Decision::Publishing),
    };
    if decided == Decision::Untracked {
        return Ok(Settled::Untouched);
    }
    let built = if Database::open(state.db_path())?
        .committed_run_projection(run_id)?
        .is_some()
    {
        None
    } else {
        let mut readable = source::git_command(&registration.root);
        readable.args([
            "cat-file",
            "-e",
            &format!("{}^{{commit}}", registration.s0_commit),
        ]);
        if !readable
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return Ok(Settled::Failed(format!(
                "its S0 commit {} can no longer be read",
                registration.s0_commit
            )));
        }
        // Whatever an interrupted build left is made again from S0.
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if [RECORD, DECISION, LOCK].contains(&entry.file_name().to_string_lossy().as_ref()) {
                continue;
            }
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(entry.path())?;
            } else {
                fs::remove_file(entry.path())?;
            }
        }
        let prepared = attach::prepare(state, request(registration))?;
        Some(attach::build(state, &prepared, run_id)?)
    };
    attach::publish(
        state,
        run_id,
        built,
        Some(Registered {
            started_at: registration.started_at,
            via,
        }),
    )?;
    Ok(Settled::Published)
}

/// Called by the project owner on every tick: publish pending registrations of
/// `root` whose S0 is durable, and remove those known to have failed
/// (`docs/plan-0.4.10.md` §3.5). Never deletes ambiguous state. Every
/// registration it removes, publishes or cannot publish is logged.
pub fn reconcile(state: &State, root: &Path) -> Result<()> {
    let mut first = None;
    for (run_id, dir) in entries(state)? {
        if let Err(error) = reconcile_one(state, root, &run_id, &dir) {
            first.get_or_insert(error.context(format!("registration {run_id}")));
        }
    }
    first.map_or(Ok(()), Err)
}

fn reconcile_one(state: &State, root: &Path, run_id: &str, dir: &Path) -> Result<()> {
    let old = age(run_id) >= budget();
    let decided = decision(dir);
    let registration = match read_record(dir) {
        Ok(Some(registration)) => registration,
        Ok(None) if decided == Some(Decision::Untracked) => {
            return remove(state, run_id, dir, "untracked; removed");
        }
        Ok(None) if old => {
            return remove(
                state,
                run_id,
                dir,
                "no starting state was captured; removed",
            );
        }
        Ok(None) => return Ok(()),
        Err(error) => {
            if !dir.join(FAILED).exists() {
                write_durably(&dir.join(FAILED), format!("{error:#}\n").as_bytes())?;
                log(
                    run_id,
                    &format!("not published: {error:#}; kept at {}", dir.display()),
                );
            }
            return Ok(());
        }
    };
    if registration.root != root || dir.join(FAILED).exists() {
        return Ok(());
    }
    match decided {
        Some(Decision::Untracked) => remove(state, run_id, dir, "untracked; removed"),
        None if !old => Ok(()),
        _ => {
            let Ok(_lock) = OperationLock::acquire(
                &workspace_lock(state, &registration.workspace),
                "another session is registering this workspace",
            ) else {
                return Ok(());
            };
            match settle(state, dir, &registration, "owner")? {
                Settled::Published => log(run_id, "published as Work"),
                Settled::Failed(reason) => {
                    write_durably(&dir.join(FAILED), format!("{reason}\n").as_bytes())?;
                    log(
                        run_id,
                        &format!("not published: {reason}; kept at {}", dir.display()),
                    );
                }
                Settled::Untouched => {}
            }
            Ok(())
        }
    }
}

/// Remove a registration nobody holds that will never be published.
fn remove(state: &State, run_id: &str, dir: &Path, why: &str) -> Result<()> {
    let Ok(_held) = OperationLock::acquire(&dir.join(LOCK), "this registration is in use") else {
        return Ok(());
    };
    fs::remove_dir_all(dir).with_context(|| format!("failed to remove {}", dir.display()))?;
    // Taking the lock of one just published made its directory again.
    if !state.run_dir(run_id).is_dir() {
        log(run_id, why);
    }
    Ok(())
}
