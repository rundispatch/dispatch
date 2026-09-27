//! Watching a project in the background: `dispatch start`, `dispatch stop`,
//! and the record that says who watches. The serve lock decides whether a
//! project is watched: the kernel releases it when its owner exits, crashes
//! or the machine reboots. The record only says whom `stop` may signal, and
//! is trusted only while it names the exact live process.

use std::{
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
        version: 1,
        root: root.to_owned(),
        identity: ProcessIdentity::current(),
        started_at: Utc::now(),
        dispatch_version: VERSION.to_owned(),
        background,
    };
    write_atomically(&path, &serde_json::to_vec_pretty(&record)?)
}

pub(crate) fn remove_record(state: &State, root: &Path) {
    let _ = fs::remove_file(record_path(state, root));
}

fn read_record(state: &State, root: &Path) -> Option<WatcherRecord> {
    let bytes = fs::read(record_path(state, root)).ok()?;
    serde_json::from_slice::<WatcherRecord>(&bytes)
        .ok()
        .filter(|record| record.root == root)
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
    match watcher(state, root) {
        Ok(Watcher::Watched(Some(record))) => {
            let how = if record.background {
                "in the background"
            } else {
                "by dispatch serve"
            };
            let since = record.started_at.with_timezone(&Local).format("%H:%M");
            let mut line = format!("watched {how} since {since} (pid {})", record.identity.pid);
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

/// `dispatch start [--root <path>]`: watch the project in the background and
/// return. The owner is `dispatch serve --background` in its own session, so
/// closing the terminal does not stop it; its stderr is
/// `watchers/<key>.log`.
#[cfg(unix)]
pub fn start(state: &State, root: Option<PathBuf>, verbose: u8) -> Result<()> {
    use std::os::unix::process::CommandExt;

    state.initialize()?;
    let root = source::resolve_source(root.as_deref())?;
    // A broken configuration fails here, before anything is spawned.
    Config::discover(&root, None)?;
    if let Watcher::Watched(_) = watcher(state, &root)? {
        println!(
            "Already watching {}: {}",
            root.display(),
            describe(state, &root)
        );
        return Ok(());
    }
    watchers_dir(state)?;
    let log = record_path(state, &root).with_extension("log");
    let file = {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&log)
            .with_context(|| format!("failed to create {}", log.display()))?;
        // `mode` applies only when the file is new; a log left by an earlier
        // version keeps whatever it had.
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file
    };
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.arg("--state-dir").arg(&state.root);
    for _ in 0..verbose {
        command.arg("-v");
    }
    command
        .args(["serve", "--background", "--root"])
        .arg(&root)
        .current_dir(&root)
        .stdin(std::process::Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file);
    // SAFETY: setsid is async-signal-safe and touches only the child.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .context("failed to start the background owner")?;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(record) = read_record(state, &root)
            && record.identity.pid == child.id()
            && is_exact(&record)
        {
            println!("✓ Watching {}", root.display());
            println!(
                "  Dispatch is running in the background. See it: dispatch watch · Stop: dispatch stop"
            );
            return Ok(());
        }
        if child.try_wait()?.is_some() {
            // Another `start` may have won the race for this root.
            if let Watcher::Watched(Some(_)) = watcher(state, &root)? {
                println!(
                    "Already watching {}: {}",
                    root.display(),
                    describe(state, &root)
                );
                return Ok(());
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
            "the background owner for {} did not become ready in 10 s; see {}",
            root.display(),
            log.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(not(unix))]
pub fn start(_state: &State, _root: Option<PathBuf>, _verbose: u8) -> Result<()> {
    anyhow::bail!("watching in the background needs a Unix system; use dispatch serve")
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
    let pid = i32::try_from(record.identity.pid).context("watcher pid out of range")?;
    // SAFETY: SIGTERM to the one process identified above.
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        return Err(std::io::Error::last_os_error()).context("failed to signal the watcher");
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while let Watcher::Watched(_) = watcher(state, &root)? {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "the watcher for {} (pid {pid}) did not stop within 10 s",
            root.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("✓ Dispatch stopped watching {}.", root.display());
    Ok(())
}

#[cfg(not(unix))]
pub fn stop(_state: &State, _root: Option<PathBuf>) -> Result<()> {
    anyhow::bail!("watching in the background needs a Unix system")
}
