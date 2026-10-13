//! Worktrees a runtime made, found in a watched project without hooks
//! (`docs/plan-0.4.13.md` §3 B). Each owner tick lists the root's Git
//! worktrees, never walking a directory, and asks the runtime's adapter which
//! ones it made. Those already there when the watch began go into the
//! `Inventory` and are only listed. One that appears later with a `claude
//! session` lock becomes Work through the same registration and publication
//! path as a hook's, its session already ended when that process is gone; one
//! with any other lock, or none, is listed.
//!
//! A worktree that has Work is the Work's: it is neither listed nor
//! registered again, unless that Work finished after its worktree was
//! removed, so a worktree at that path now is another one.

use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    LifecycleState, RunRecord, RuntimeSession,
    process::ProcessIdentity,
    runtime::claude::{self, Liveness, Lock, WorktreeSignals},
    source,
    state::{State, write_atomically},
};

/// Worktrees a runtime made that Dispatch lists with an attach command and
/// never turns into Work by itself: those already there when this watch
/// began, and those that appeared later without a running session's lock.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inventory {
    pub version: u32,
    /// The watch it belongs to, as its owner's record said on the first scan.
    /// It lives beside that record for as long as the watch: an owner
    /// restarted after a crash keeps it, and the watch's end removes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch_started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch_owner: Option<ProcessIdentity>,
    pub entries: Vec<InventoryEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryEntry {
    pub path: PathBuf,
    pub first_seen: DateTime<Utc>,
    /// What the runtime recorded it branched from (Claude's `CLAUDE_BASE`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// The worktree's lock reason when first seen, verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<String>,
}

impl Inventory {
    /// The inventory of the watch of `root`, or an empty one.
    pub fn load(state: &State, root: &Path) -> Inventory {
        read(state, root).unwrap_or_default()
    }
}

/// Beside the watcher's record: `watchers/<key>.inventory.json`.
pub(crate) fn path(state: &State, root: &Path) -> PathBuf {
    super::background::watcher_file(state, root, "inventory.json")
}

/// `None` before this watch's first scan. An unreadable inventory is taken
/// again, which only ever lists more.
fn read(state: &State, root: &Path) -> Option<Inventory> {
    serde_json::from_slice(&fs::read(path(state, root)).ok()?).ok()
}

/// Claude Code writes `CLAUDE_BASE` a moment before it locks a new worktree
/// (14 ms apart in the 0.4.12 trial): a younger worktree without a session
/// lock is looked at again on the next tick rather than listed for good.
const GRACE: Duration = Duration::from_secs(3);

/// Registrations that failed, by worktree, so each is noted once.
static FAILED: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

/// A linked worktree of the root that Claude Code made.
struct Found {
    path: PathBuf,
    signals: WorktreeSignals,
    /// Its lock reason as Git lists it.
    lock: Option<String>,
}

/// One owner tick for `root`, with the runs the owner loaded.
pub(crate) fn scan(state: &State, root: &Path, runs: &[RunRecord]) -> Result<()> {
    // A plain directory has no worktrees.
    if !root.join(".git").exists() {
        return Ok(());
    }
    let found = claude_worktrees(root)?;
    // The latest Work of each workspace, from the runs already loaded.
    let mut latest: HashMap<&Path, &RunRecord> = HashMap::new();
    for run in runs {
        if let Some(attachment) = &run.attachment {
            let slot = latest.entry(attachment.workspace.as_path()).or_insert(run);
            if run.created_at > slot.created_at {
                *slot = run;
            }
        }
    }
    let has_work = |path: &Path| {
        latest.get(path).is_some_and(|run| {
            let removed = run
                .attachment
                .as_ref()
                .is_some_and(|attachment| attachment.workspace_removed.is_some());
            !(removed && run.outcome.lifecycle == LifecycleState::Finished)
        })
    };
    let listed = |found: &Found| InventoryEntry {
        path: found.path.clone(),
        first_seen: Utc::now(),
        base: found.signals.base.clone(),
        lock: found.lock.clone(),
    };

    let Some(mut inventory) = read(state, root) else {
        let record: Option<super::background::WatcherRecord> =
            fs::read(super::background::watcher_file(state, root, "json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let inventory = Inventory {
            version: 1,
            watch_started_at: record.as_ref().map(|record| record.started_at),
            watch_owner: record.map(|record| record.identity),
            entries: found
                .iter()
                .filter(|found| !has_work(&found.path))
                .map(listed)
                .collect(),
        };
        write(state, root, &inventory)?;
        if !inventory.entries.is_empty() {
            note(&format!(
                "{} worktree(s) Claude Code made were already here; listed, not tracked",
                inventory.entries.len()
            ));
        }
        return Ok(());
    };

    // A worktree that is gone, or has Work now, is no longer listed.
    let before = inventory.entries.len();
    inventory.entries.retain(|entry| {
        found.iter().any(|found| found.path == entry.path) && !has_work(&entry.path)
    });
    let mut changed = inventory.entries.len() != before;
    for found in &found {
        if has_work(&found.path) || inventory.entries.iter().any(|e| e.path == found.path) {
            continue;
        }
        if let Some(lock @ Lock::Session { .. }) = &found.signals.lock {
            match claude::liveness(lock) {
                Liveness::Running => register(state, root, &found.path, lock, false),
                // It appeared after watching began and its session already
                // ended: a `claude -p` run between two ticks leaves its lock.
                Liveness::Gone => register(state, root, &found.path, lock, true),
                // Unknown proves nothing: look again next tick.
                Liveness::Unknown => {}
            }
            continue;
        }
        if young(&found.path) {
            continue;
        }
        note(&format!(
            "{} was made by Claude Code without a session's lock ({}); listed, not \
             tracked. Track it: dispatch attach --workspace {}",
            found.path.display(),
            found.lock.as_deref().unwrap_or("not locked"),
            found.path.display()
        ));
        inventory.entries.push(listed(found));
        changed = true;
    }
    if changed {
        write(state, root, &inventory)?;
    }
    Ok(())
}

/// Register the session `lock` names as Work in `workspace`, `ended` when
/// its process is gone, noting a failure once.
fn register(state: &State, root: &Path, workspace: &Path, lock: &Lock, ended: bool) {
    let result = session(lock, ended)
        .and_then(|session| super::registration::discovered(state, root, workspace, session));
    let mut failed = FAILED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match result {
        Ok(Some(run_id)) => {
            failed.remove(workspace);
            note(&format!(
                "found a{} Claude Code session in {}; tracking it as Work {run_id}",
                if ended { "n ended" } else { "" },
                workspace.display()
            ));
        }
        // Someone else is registering it, or has: the next tick sees which.
        Ok(None) => {}
        Err(error) => {
            if failed.insert(workspace.to_owned()) {
                note(&format!(
                    "could not track the Claude Code session in {}: {error:#}",
                    workspace.display()
                ));
            }
        }
    }
}

/// The observed session a `claude session` lock names; `ended` now, when its
/// process was seen gone.
fn session(lock: &Lock, ended: bool) -> Result<RuntimeSession> {
    let Lock::Session { name, pid, start } = lock else {
        anyhow::bail!("not a session lock");
    };
    // `ps -o lstart=` in UTC, as Claude Code writes it.
    let started = NaiveDateTime::parse_from_str(start, "%a %b %e %H:%M:%S %Y")
        .with_context(|| format!("unreadable session start {start:?}"))?;
    Ok(RuntimeSession {
        provider: "claude".into(),
        session_id: format!("lock:{name}@{pid}"),
        source: "discovered".into(),
        started_at: started.and_utc(),
        ended_at: ended.then(Utc::now),
        end_reason: ended.then(|| "process_gone".into()),
        model: None,
    })
}

/// The root's linked worktrees that Claude Code made, from `git worktree
/// list`; missing and prunable ones are skipped.
fn claude_worktrees(root: &Path) -> Result<Vec<Found>> {
    let mut list = source::git_command(root);
    list.args(["worktree", "list", "--porcelain"]);
    let output = source::checked_output(list, "failed to list the project's worktrees")?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut found = Vec::new();
    for block in text.split("\n\n") {
        let (mut path, mut lock, mut prunable) = (None, None, false);
        for line in block.lines() {
            if let Some(listed) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(listed));
            } else if line == "locked" || line.starts_with("locked ") {
                lock = Some(line["locked".len()..].trim_start().to_owned());
            } else if line == "prunable" || line.starts_with("prunable ") {
                prunable = true;
            }
        }
        let Some(Ok(path)) = path.filter(|_| !prunable).map(fs::canonicalize) else {
            continue;
        };
        if path == root {
            continue;
        }
        let signals = claude::worktree_signals(&path);
        if claude::made_by_claude(root, &path, &signals) {
            found.push(Found {
                path,
                signals,
                lock: lock.filter(|reason| !reason.is_empty()),
            });
        }
    }
    Ok(found)
}

/// Whether the worktree was made less than `GRACE` ago: its `.git` file is
/// written once, when Git makes it.
fn young(worktree: &Path) -> bool {
    fs::metadata(worktree.join(".git"))
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|made| made.elapsed().ok())
        .is_some_and(|age| age < GRACE)
}

fn write(state: &State, root: &Path, inventory: &Inventory) -> Result<()> {
    write_atomically(&path(state, root), &serde_json::to_vec_pretty(inventory)?)
}

fn note(message: &str) {
    eprintln!("{} discovery: {message}", Utc::now().to_rfc3339());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_lock_is_an_observed_session_from_its_process_start() {
        let lock = claude::parse_lock("claude session x (pid 42 start Sat Oct  3 09:05:07 2026)");
        let observed = session(&lock, false).unwrap();
        assert_eq!(observed.provider, "claude");
        assert_eq!(observed.session_id, "lock:x@42");
        assert_eq!(observed.source, "discovered");
        assert_eq!(
            observed.started_at.to_rfc3339(),
            "2026-10-03T09:05:07+00:00"
        );
        assert_eq!((observed.ended_at, observed.end_reason), (None, None));
        let before = Utc::now();
        let ended = session(&lock, true).unwrap();
        assert!(ended.ended_at.is_some_and(|at| at >= before));
        assert_eq!(ended.end_reason.as_deref(), Some("process_gone"));
        assert!(session(&Lock::Other("moving it".into()), false).is_err());
        let unreadable = Lock::Session {
            name: "x".into(),
            pid: 42,
            start: "yesterday".into(),
        };
        assert!(session(&unreadable, true).is_err());
    }
}
