//! Watching a project in the background: `dispatch start`, `dispatch stop`,
//! and the record that says who watches. The serve lock decides whether a
//! project is watched: the kernel releases it when its owner exits, crashes
//! or the machine reboots. The record only says whom `stop` may signal, and
//! is trusted only while it names the exact live process.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    Config, VERSION,
    lock::OperationLock,
    process::{IdentityState, ProcessIdentity, identity_state},
    source,
    state::{State, write_atomically},
};

/// Who owns a root's project watching, written by `serve` once it holds the
/// serve lock and removed when it stops.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WatcherRecord {
    pub version: u32,
    pub root: PathBuf,
    pub identity: ProcessIdentity,
    pub started_at: DateTime<Utc>,
    pub dispatch_version: String,
    pub background: bool,
    #[serde(default)]
    pub discovery_ready: bool,
    /// The canonical path of the executable that started the owner (version
    /// 2); none in version 1 records.
    #[serde(default)]
    pub exe: Option<PathBuf>,
    /// The variables checks receive, as the owner `start` spawned had them
    /// (version 2): a revived or upgraded owner runs with these, not with
    /// whatever its reviver had.
    #[serde(default)]
    pub environment: Option<BTreeMap<String, String>>,
}

pub(crate) enum Watcher {
    /// The serve lock is held; the record, when there is a readable one for
    /// this root, says by whom.
    Watched(Option<WatcherRecord>),
    NotWatched,
}

fn key(root: &Path) -> String {
    hex::encode(Sha256::digest(root.to_string_lossy().as_bytes()))
}

pub(crate) fn lock_path(state: &State, root: &Path) -> PathBuf {
    state
        .root
        .join("locks")
        .join(format!("serve-{}.lock", key(root)))
}

fn record_path(state: &State, root: &Path) -> PathBuf {
    state
        .root
        .join("watchers")
        .join(format!("{}.json", key(root)))
}

/// Another file beside the watcher's record of `root`: `watchers/<key>.<suffix>`.
pub(crate) fn watcher_file(state: &State, root: &Path, suffix: &str) -> PathBuf {
    state
        .root
        .join("watchers")
        .join(format!("{}.{suffix}", key(root)))
}

/// Taken by `serve`. It waits a moment, because `watcher` probes the lock by
/// taking it briefly.
pub(crate) fn acquire(state: &State, root: &Path) -> Result<OperationLock> {
    OperationLock::acquire_wait(
        &lock_path(state, root),
        "already serving this root",
        Duration::from_secs(1),
    )
}

/// `watchers/`, private to the user: records and logs name paths and carry
/// detailed errors. Made private before anything is written into it.
fn watchers_dir(state: &State) -> Result<PathBuf> {
    let directory = state.root.join("watchers");
    fs::create_dir_all(&directory)
        .with_context(|| format!("failed to create {}", directory.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }
    Ok(directory)
}

pub(crate) fn write_record(state: &State, root: &Path, background: bool) -> Result<()> {
    watchers_dir(state)?;
    let path = record_path(state, root);
    let record = WatcherRecord {
        version: 2,
        root: root.to_owned(),
        identity: ProcessIdentity::current(),
        started_at: Utc::now(),
        dispatch_version: VERSION.to_owned(),
        background,
        discovery_ready: false,
        exe: this_exe(),
        environment: Some(crate::executor::safe_local_environment()),
    };
    write_atomically(&path, &serde_json::to_vec_pretty(&record)?)
}

/// `start` must not return until the first successful discovery scan has
/// inventoried worktrees that were already present.
pub(crate) fn mark_discovery_ready(state: &State, root: &Path) -> Result<()> {
    let path = record_path(state, root);
    let mut record = read_record(state, root).context("watcher record disappeared")?;
    if !record.discovery_ready {
        record.discovery_ready = true;
        write_atomically(&path, &serde_json::to_vec_pretty(&record)?)?;
    }
    Ok(())
}

pub(crate) fn remove_record(state: &State, root: &Path) {
    let _ = fs::remove_file(record_path(state, root));
    let _ = fs::remove_file(interactions_path(state, root));
    let _ = fs::remove_file(super::discovery::path(state, root));
}

/// Where the owner keeps its disposable view of how the project's Work
/// interacts (`coherence::interactions::Projection`), next to its record.
fn interactions_path(state: &State, root: &Path) -> PathBuf {
    state
        .root
        .join("watchers")
        .join(format!("{}.interactions.json", key(root)))
}

pub(crate) fn write_interactions(
    state: &State,
    root: &Path,
    projection: &crate::coherence::interactions::Projection,
) -> Result<()> {
    watchers_dir(state)?;
    write_atomically(
        &interactions_path(state, root),
        &serde_json::to_vec_pretty(projection)?,
    )
}

fn read_record(state: &State, root: &Path) -> Option<WatcherRecord> {
    let bytes = fs::read(record_path(state, root)).ok()?;
    serde_json::from_slice::<WatcherRecord>(&bytes)
        .ok()
        .filter(|record| record.root == root)
}

/// The owner's interaction view, only while an owner holds the project: a
/// file left by an owner that is gone says nothing about Work now.
pub(crate) fn interactions(
    state: &State,
    root: &Path,
) -> Option<crate::coherence::interactions::Projection> {
    if !matches!(watcher(state, root), Ok(Watcher::Watched(_))) {
        return None;
    }
    let bytes = fs::read(interactions_path(state, root)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Whether `root` is watched now: whether another process holds its serve
/// lock. Anything else that goes wrong probing the lock is an error.
pub(crate) fn watcher(state: &State, root: &Path) -> Result<Watcher> {
    let path = lock_path(state, root);
    if fs::symlink_metadata(&path).is_err() {
        return Ok(Watcher::NotWatched);
    }
    Ok(if OperationLock::is_held(&path)? {
        Watcher::Watched(read_record(state, root))
    } else {
        Watcher::NotWatched
    })
}

/// The project line for `watch` and `status`: who watches, and whether Work
/// here may run its checks by themselves.
pub(crate) fn project_line(state: &State, root: &Path) -> String {
    let consent = crate::consent::project_root(root)
        .and_then(|project| crate::consent::consent(state, &project))
        .map(|consent| consent.describe());
    match consent {
        Ok(Some(consent)) => format!("{} · {consent}", describe(state, root)),
        Ok(None) => describe(state, root),
        Err(error) => format!(
            "{} · check consent unknown: {error:#}",
            describe(state, root)
        ),
    }
}

/// One line on who watches `root`, for `watch` and `status`.
pub(crate) fn describe(state: &State, root: &Path) -> String {
    watcher_line(&watcher(state, root), true)
}

/// `describe`'s line for what `watcher` found; the interactive `watch`
/// leaves out the pid.
pub(crate) fn watcher_line(watcher: &Result<Watcher>, pid: bool) -> String {
    match watcher {
        Ok(Watcher::Watched(Some(record))) => {
            let how = if record.background {
                "in the background"
            } else {
                "by dispatch serve"
            };
            let since = record.started_at.with_timezone(&Local).format("%H:%M");
            let mut line = format!("watched {how} since {since}");
            if pid {
                line.push_str(&format!(" (pid {})", record.identity.pid));
            }
            if record.dispatch_version != VERSION {
                line.push_str(&format!(
                    "; it runs Dispatch {}, this is {VERSION}: dispatch stop && dispatch start",
                    record.dispatch_version
                ));
            }
            line
        }
        Ok(Watcher::Watched(None)) => "watched by a Dispatch process that left no record".into(),
        Ok(Watcher::NotWatched) => "not watched · dispatch start".into(),
        Err(error) => format!("watching unknown: {error:#}"),
    }
}

/// Whether the record still names the exact process that wrote it: same pid,
/// start time and boot. Anything less is never signalled.
fn is_exact(record: &WatcherRecord) -> bool {
    identity_state(&record.identity) == IdentityState::ExactLive
}

#[cfg(unix)]
fn wait_for_existing_discovery(state: &State, root: &Path) -> Result<bool> {
    let deadline = std::time::Instant::now() + OWNER_WAIT;
    loop {
        match watcher(state, root)? {
            Watcher::Watched(Some(record)) if is_exact(&record) && record.discovery_ready => {
                return Ok(true);
            }
            Watcher::Watched(_) => {}
            Watcher::NotWatched => return Ok(false),
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "the owner for {} did not complete discovery in {} s",
            root.display(),
            OWNER_WAIT.as_secs()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// How long `start` waits for a live owner to become ready, and `stop` for
/// it to exit. Either is a step of fixed work: opening the state and
/// inspecting the project, or the tick under way when the signal came. A
/// loaded machine took more than 10 s for them; only a hung owner takes
/// minutes. A dead owner is noticed at once.
const OWNER_WAIT: Duration = Duration::from_secs(120);

/// What `start` says, once watching, in a project without checks.
const NO_CHECKS: &str = "  No checks are configured, so Work here is not verified. Add them: \
    dispatch setup --checks, or dispatch setup --checks --verify '<command>'";

/// `dispatch start [--root <path>]`: watch the project in the background and
/// return. The owner is `dispatch serve --background` in its own session, so
/// closing the terminal does not stop it; its stderr is
/// `watchers/<key>.log`. In a project without checks, a terminal is offered
/// the detected ones first; without a terminal it watches at once and says
/// so. Nothing of the project runs here. An owner of another version is
/// replaced when `ensure_current` may.
#[cfg(unix)]
pub async fn start(
    state: &State,
    root: Option<PathBuf>,
    verbose: u8,
    options: crate::presenter::Options,
) -> Result<()> {
    state.initialize()?;
    let root = source::resolve_source(root.as_deref())?;
    // A broken configuration fails here, before anything is spawned.
    let (config, _) = Config::discover(&root, None)?;
    if let Watcher::Watched(_) = watcher(state, &root)? {
        ensure_current(state, &root)?;
        if let Watcher::Watched(_) = watcher(state, &root)?
            && wait_for_existing_discovery(state, &root)?
        {
            println!(
                "Already watching {}: {}",
                root.display(),
                describe(state, &root)
            );
            return Ok(());
        }
    }
    let mut unchecked = config.checks.verify.is_empty();
    if unchecked && crate::presenter::suitable() {
        let Some(saved) = crate::presenter::start_checks(&root, options).await? else {
            println!("Not watching {}: nothing was changed.", root.display());
            return Ok(());
        };
        unchecked = !saved;
    }
    let log = open_log(state, &root, true)?;
    match spawn_owner(state, &root, verbose, log, None)? {
        Ready::Started => {
            println!("✓ Watching {}", root.display());
            println!(
                "  Dispatch is running in the background. See it: dispatch watch · Stop: dispatch stop"
            );
            if unchecked {
                println!("{NO_CHECKS}");
            }
        }
        Ready::Existing => println!(
            "Already watching {}: {}",
            root.display(),
            describe(state, &root)
        ),
    }
    Ok(())
}

#[cfg(not(unix))]
pub async fn start(
    _state: &State,
    _root: Option<PathBuf>,
    _verbose: u8,
    _options: crate::presenter::Options,
) -> Result<()> {
    anyhow::bail!("watching in the background needs a Unix system; use dispatch serve")
}

/// The owner's log, `watchers/<key>.log`, private to the user: `start`
/// begins it again, a restart adds to it.
#[cfg(unix)]
fn open_log(state: &State, root: &Path, truncate: bool) -> Result<(fs::File, PathBuf)> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    watchers_dir(state)?;
    let log = record_path(state, root).with_extension("log");
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(truncate)
        .append(!truncate)
        .mode(0o600)
        .open(&log)
        .with_context(|| format!("failed to create {}", log.display()))?;
    // `mode` applies only when the file is new; a log left by an earlier
    // version keeps whatever it had.
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok((file, log))
}

/// `dispatch serve --background` for `root`, in its own session, writing to
/// `log`.
#[cfg(unix)]
fn command(
    state: &State,
    root: &Path,
    verbose: u8,
    log: &fs::File,
    environment: Option<&BTreeMap<String, String>>,
) -> Result<std::process::Command> {
    use std::os::unix::process::CommandExt;

    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.arg("--state-dir").arg(&state.root);
    for _ in 0..verbose {
        command.arg("-v");
    }
    command
        .args(["serve", "--background", "--root"])
        .arg(root)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?);
    if let Some(environment) = environment {
        for name in crate::executor::safe_local_environment().keys() {
            command.env_remove(name);
        }
        command.envs(environment);
    }
    // SAFETY: setsid is async-signal-safe and touches only the child.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(command)
}

/// How a spawned owner became ready.
#[cfg(unix)]
enum Ready {
    /// The owner spawned here watches, its discovery done.
    Started,
    /// Another process's owner won the race for the root, and is ready.
    Existing,
}

/// Spawn the background owner for `root`, with `environment` when it is
/// given, and wait until it, or one that won the race for the root, has
/// completed its first discovery.
#[cfg(unix)]
fn spawn_owner(
    state: &State,
    root: &Path,
    verbose: u8,
    (file, log): (fs::File, PathBuf),
    environment: Option<&BTreeMap<String, String>>,
) -> Result<Ready> {
    let mut child = command(state, root, verbose, &file, environment)?
        .spawn()
        .context("failed to start the background owner")?;
    let deadline = std::time::Instant::now() + OWNER_WAIT;
    loop {
        if let Some(record) = read_record(state, root)
            && record.identity.pid == child.id()
            && is_exact(&record)
            && record.discovery_ready
        {
            return Ok(Ready::Started);
        }
        if child.try_wait()?.is_some() {
            // Another `start` may have won the race for this root.
            if let Watcher::Watched(_) = watcher(state, root)?
                && wait_for_existing_discovery(state, root)?
            {
                return Ok(Ready::Existing);
            }
            let text = fs::read_to_string(&log).unwrap_or_default();
            let tail: Vec<&str> = text.lines().rev().take(5).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            anyhow::bail!(
                "Dispatch could not start watching {}:\n{}",
                root.display(),
                tail.join("\n")
            );
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "the background owner for {} did not become ready in {} s; see {}",
            root.display(),
            OWNER_WAIT.as_secs(),
            log.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// This process's executable, canonicalized: what a version 2 record names.
fn this_exe() -> Option<PathBuf> {
    std::env::current_exe().and_then(fs::canonicalize).ok()
}

/// Whether `version` is older than this one, by its numeric dotted parts. A
/// version that does not read as one is not older.
fn older(version: &str) -> bool {
    fn parts(version: &str) -> Option<Vec<u64>> {
        version
            .split(['-', '+'])
            .next()?
            .split('.')
            .map(|part| part.parse().ok())
            .collect()
    }
    matches!((parts(version), parts(VERSION)), (Some(old), Some(new)) if old < new)
}

/// Whether this binary may replace the owner `record` names: it was
/// started from this very executable, upgraded in place since; or, for a
/// record that does not say, it runs an older Dispatch. An older binary, or
/// another install, never replaces an owner.
fn replaceable(record: &WatcherRecord) -> bool {
    match &record.exe {
        Some(exe) => this_exe().as_ref() == Some(exe),
        None => older(&record.dispatch_version),
    }
}

/// Keep `root` watched as `dispatch start` left it (contract A):
/// - its background owner stopped without `dispatch stop` (a reboot, a
///   crash), so the record is there and the serve lock free: start it again;
/// - it runs another Dispatch and `replaceable` allows it: stop it and start
///   this one.
///
/// Either runs with the environment the record keeps. Each says so in one
/// line on stderr; stdout is left to the command. A configuration that does
/// not read spawns nothing, says why, and leaves the record. Anything else
/// is left alone, and an owner left on another version keeps the note in
/// `watcher_line`.
#[cfg(unix)]
pub fn ensure_current(state: &State, root: &Path) -> Result<()> {
    match watcher(state, root)? {
        Watcher::NotWatched => {
            let Some(record) = read_record(state, root).filter(|record| record.background) else {
                return Ok(());
            };
            if let Err(error) = Config::discover(root, None) {
                eprintln!("Could not watch {} again: {error:#}", root.display());
                return Ok(());
            }
            let log = open_log(state, root, false)?;
            if let Ready::Started = spawn_owner(state, root, 0, log, record.environment.as_ref())? {
                eprintln!("Watching {} again: its owner had stopped.", root.display());
            }
        }
        Watcher::Watched(Some(record))
            if record.background
                && record.dispatch_version != VERSION
                && replaceable(&record)
                && is_exact(&record) =>
        {
            // The owner there keeps watching until this one can.
            if let Err(error) = Config::discover(root, None) {
                eprintln!(
                    "Could not restart watching {} with Dispatch {VERSION}: {error:#}",
                    root.display()
                );
                return Ok(());
            }
            terminate(state, root, &record)?;
            let log = open_log(state, root, false)?;
            if let Ready::Started = spawn_owner(state, root, 0, log, record.environment.as_ref())? {
                eprintln!(
                    "Restarted watching {} with Dispatch {VERSION} (was {}).",
                    root.display(),
                    record.dispatch_version
                );
            }
        }
        Watcher::Watched(_) => {}
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn ensure_current(_state: &State, _root: &Path) -> Result<()> {
    Ok(())
}

/// `ensure_current` for the project `path` is in: its repository's main
/// worktree, or the directory itself. It never fails the command it runs
/// before, which reports a path that does not resolve itself; anything else
/// that goes wrong is one line on stderr.
pub fn ensure_current_at(state: &State, path: &Path) {
    let Ok(path) = fs::canonicalize(path) else {
        return;
    };
    let root = match crate::runtime::main_worktree(&path) {
        Some(Some(root)) => Ok(root),
        Some(None) => Ok(path),
        None => source::repo_identity(&path).map(|repo| repo.map_or(path, |r| r.main_worktree)),
    };
    if let Err(error) = root.and_then(|root| ensure_current(state, &root)) {
        eprintln!("Dispatch could not bring back watching here: {error:#}");
    }
}

/// Whether `root`'s owner stopped without `dispatch stop`: what a hook that
/// found the serve lock free asks before going to Git.
pub(crate) fn stopped_owner(state: &State, root: &Path) -> bool {
    read_record(state, root).is_some_and(|record| record.background)
}

/// A hook's `ensure_current`, for a root whose serve lock it found free:
/// when its background owner stopped without `dispatch stop`, start it again
/// with the environment its record keeps, and return at once, without
/// waiting for discovery. A configuration that does not read spawns nothing.
/// The line goes to the owner's log: a hook's stdout reaches the agent.
/// Whether an owner is on its way.
#[cfg(unix)]
pub(crate) fn revive_in_background(state: &State, root: &Path) -> bool {
    use std::io::Write;

    let Some(record) = read_record(state, root).filter(|record| record.background) else {
        return false;
    };
    let Ok((mut file, _)) = open_log(state, root, false) else {
        return false;
    };
    let spawned = Config::discover(root, None)
        .and_then(|_| command(state, root, 0, &file, record.environment.as_ref()))
        .and_then(|mut command| {
            command
                .spawn()
                .context("failed to start the background owner")
        });
    let line = match &spawned {
        Ok(_) => format!("Watching {} again: its owner had stopped.", root.display()),
        Err(error) => format!("Could not watch {} again: {error:#}", root.display()),
    };
    let _ = writeln!(file, "{} {line}", Utc::now().to_rfc3339());
    spawned.is_ok()
}

#[cfg(not(unix))]
pub(crate) fn revive_in_background(_state: &State, _root: &Path) -> bool {
    false
}

/// Signal exactly the owner `record` names, which holds `root`'s serve lock,
/// and wait until it lets go.
#[cfg(unix)]
fn terminate(state: &State, root: &Path, record: &WatcherRecord) -> Result<()> {
    let pid = i32::try_from(record.identity.pid).context("watcher pid out of range")?;
    // SAFETY: SIGTERM to the one process identified by the caller.
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        return Err(std::io::Error::last_os_error()).context("failed to signal the watcher");
    }
    let deadline = std::time::Instant::now() + OWNER_WAIT;
    while let Watcher::Watched(_) = watcher(state, root)? {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "the watcher for {} (pid {pid}) did not stop within {} s",
            root.display(),
            OWNER_WAIT.as_secs()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

/// `dispatch stop [--root <path>]`: stop exactly the process that watches
/// this root. It is signalled only while the lock is held and the record
/// names that exact live process.
#[cfg(unix)]
pub fn stop(state: &State, root: Option<PathBuf>) -> Result<()> {
    let root = source::resolve_source(root.as_deref())?;
    let record = match watcher(state, &root)? {
        Watcher::NotWatched => {
            remove_record(state, &root);
            println!("Not watching {}.", root.display());
            return Ok(());
        }
        Watcher::Watched(record) => record,
    };
    let Some(record) = record.filter(is_exact) else {
        anyhow::bail!(
            "{} is watched, but not by a process Dispatch can identify, so nothing was \
             signalled. The watching process holds {}.",
            root.display(),
            lock_path(state, &root).display()
        );
    };
    terminate(state, &root, &record)?;
    println!("✓ Dispatch stopped watching {}.", root.display());
    Ok(())
}

#[cfg(not(unix))]
pub fn stop(_state: &State, _root: Option<PathBuf>) -> Result<()> {
    anyhow::bail!("watching in the background needs a Unix system")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `describe` says exactly what it said before `watch` shared its line.
    #[test]
    fn describe_is_unchanged_and_watch_leaves_out_the_pid() {
        let home = tempfile::TempDir::new().unwrap();
        let state = State {
            root: home.path().join("state"),
        };
        let root = home.path().join("project");
        assert_eq!(describe(&state, &root), "not watched · dispatch start");

        let _lock = acquire(&state, &root).unwrap();
        assert_eq!(
            describe(&state, &root),
            "watched by a Dispatch process that left no record"
        );
        write_record(&state, &root, true).unwrap();
        let record = read_record(&state, &root).unwrap();
        let since = record.started_at.with_timezone(&Local).format("%H:%M");
        let pid = std::process::id();
        assert_eq!(
            describe(&state, &root),
            format!("watched in the background since {since} (pid {pid})")
        );
        assert_eq!(
            watcher_line(&watcher(&state, &root), false),
            format!("watched in the background since {since}")
        );

        let mut record = record;
        record.background = false;
        record.dispatch_version = "0.0.1".into();
        write_atomically(
            &record_path(&state, &root),
            &serde_json::to_vec_pretty(&record).unwrap(),
        )
        .unwrap();
        let mismatch =
            format!("; it runs Dispatch 0.0.1, this is {VERSION}: dispatch stop && dispatch start");
        assert_eq!(
            describe(&state, &root),
            format!("watched by dispatch serve since {since} (pid {pid}){mismatch}")
        );
        assert_eq!(
            watcher_line(&watcher(&state, &root), false),
            format!("watched by dispatch serve since {since}{mismatch}")
        );
        assert_eq!(
            watcher_line(&Err(anyhow::anyhow!("no probe")), true),
            "watching unknown: no probe"
        );
    }
}
