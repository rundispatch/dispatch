//! Worktrees a runtime made, found in a watched project without hooks
//! (`docs/plan-0.4.13.md` §3 B, `docs/plan-0.4.14.md` §3 C). The fast owner loop
//! lists the root's Git worktrees, never walking a directory, and asks the
//! runtime's adapter which ones it made. Those already there when the watch
//! began go into the `Inventory` and are only listed. One Claude Code made
//! that appears later with a `claude session` lock becomes Work through the
//! same registration and publication path as a hook's, its session already
//! ended when that process is gone; one with any other lock, or none, is
//! listed. One Hermes made is the same with its `hermes pid=` lock. One that a
//! runtime in `UNOBSERVED` made becomes Work as soon as it appears, its
//! session open for good: nothing shows when it ends.
//!
//! A worktree that has Work is the Work's: it is neither listed nor
//! registered again, unless that Work finished after its worktree was
//! removed, so a worktree at that path now is another one. A new session's
//! lock on it reopens its Work when that Work awaits review or is being
//! finished: the session goes on from the same S0.

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
    AttachmentRecord, BaselineProvenance, LifecycleState, RunRecord, RuntimeSession,
    process::ProcessIdentity,
    runtime::{
        claude::{self, Liveness, Lock, WorktreeSignals},
        codex, cursor, hermes,
    },
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

/// A runtime whose worktree names its session but nothing that shows when
/// the session ends: its liveness is always `Unknown`, so Dispatch never says
/// its agent runs and never records its end.
pub(crate) struct Unobserved {
    /// The agent, as its Work records it.
    pub agent: &'static str,
    /// The name a person knows it by.
    pub name: &'static str,
    /// The session the worktree names if this runtime made it, from the
    /// project root, the worktree and its branch as Git lists them, and the
    /// user's home.
    session: fn(&Path, &Path, Option<&str>, Option<&Path>) -> Option<String>,
}

/// The runtimes recognized besides Claude Code, each by the one reliable sign
/// it leaves (`runtime::codex`, `runtime::cursor`).
pub(crate) static UNOBSERVED: [Unobserved; 2] = [
    Unobserved {
        agent: "codex",
        name: "Codex",
        session: |_, worktree, _, _| codex::thread(worktree),
    },
    Unobserved {
        agent: "cursor",
        name: "Cursor Agent",
        session: cursor::session,
    },
];

/// Runtimes that can remove their own worktrees, taking uncommitted work with
/// them. Their attached Work needs a fresh last-seen patch while it exists.
pub(crate) const VANISHING_WORKTREES: &[&str] = &["hermes"];

pub(crate) fn can_vanish(agent: Option<&str>) -> bool {
    agent.is_some_and(|agent| VANISHING_WORKTREES.contains(&agent))
}

/// The runtime of discovered Work whose session's end is not observable.
pub(crate) fn unobserved(attachment: &AttachmentRecord) -> Option<&'static Unobserved> {
    if !matches!(attachment.provenance, BaselineProvenance::Discovered { .. }) {
        return None;
    }
    let agent = attachment.agent.as_deref()?;
    UNOBSERVED.iter().find(|runtime| runtime.agent == agent)
}

/// A linked worktree of the root that a runtime made.
struct Found {
    path: PathBuf,
    made: Made,
    /// Its lock reason as Git lists it.
    lock: Option<String>,
}

/// Who made a worktree, by the sign its runtime left.
enum Made {
    Claude(WorktreeSignals),
    /// Hermes, and the worktree's name, which is its session.
    Hermes(String),
    /// A runtime in `UNOBSERVED`, and the session the worktree names.
    Unobserved {
        runtime: &'static Unobserved,
        session_id: String,
    },
}

/// One fast owner pass for `root`, with the runs the owner loaded.
pub(crate) fn scan(state: &State, root: &Path, runs: &[RunRecord]) -> Result<()> {
    // A plain directory has no worktrees.
    if !root.join(".git").exists() {
        return Ok(());
    }
    let found = runtime_worktrees(root)?;
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
        base: match &found.made {
            Made::Claude(signals) => signals.base.clone(),
            Made::Hermes(_) | Made::Unobserved { .. } => None,
        },
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
                "{} worktree(s) an agent's runtime made were already here; listed, not tracked",
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
        if has_work(&found.path) {
            // A known worktree that a new session locked: a resume goes on
            // from its Work if that Work is a result awaiting review or being
            // finished (`registration::discovered` decides, under the lock).
            if let (Some(run), Made::Claude(signals)) =
                (latest.get(found.path.as_path()), &found.made)
                && let Some(lock) = &signals.lock
                && relocked(run, lock)
            {
                let session = session(lock, false);
                register(
                    state,
                    root,
                    &found.path,
                    "Claude Code",
                    session,
                    lock_pid(lock),
                );
            }
            continue;
        }
        if inventory.entries.iter().any(|e| e.path == found.path) {
            continue;
        }
        // The process a session's lock names, and whether it runs.
        let (name, locked) = match &found.made {
            Made::Claude(signals) => match &signals.lock {
                Some(lock @ Lock::Session { pid, .. }) => {
                    ("Claude Code", Some((*pid, claude::liveness(lock))))
                }
                _ => ("Claude Code", None),
            },
            Made::Hermes(_) => {
                let pid = found.lock.as_deref().and_then(hermes::lock_pid);
                (
                    "Hermes",
                    pid.map(|pid| (pid, hermes::liveness(pid, &found.path))),
                )
            }
            // It appeared after watching began; its liveness is Unknown.
            Made::Unobserved {
                runtime,
                session_id,
            } => {
                let session = unobserved_session(runtime.agent, session_id, &found.path);
                register(state, root, &found.path, runtime.name, Ok(session), None);
                continue;
            }
        };
        if let Some((pid, liveness)) = locked {
            match liveness {
                // A running session's process is recorded as the Work's
                // agent, so that liveness sees it end even if it ends before
                // liveness first looks.
                Liveness::Running => {
                    let session = locked_session(found, false);
                    register(state, root, &found.path, name, session, Some(pid));
                }
                // It appeared after watching began and its session already
                // ended: a `claude -p` run between two ticks leaves its lock.
                Liveness::Gone => {
                    let session = locked_session(found, true);
                    register(state, root, &found.path, name, session, None);
                }
                // Unknown proves nothing: look again next tick.
                Liveness::Unknown => {}
            }
            continue;
        }
        if young(&found.path) {
            continue;
        }
        note(&format!(
            "{} was made by {name} without a session's lock ({}); listed, not \
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

/// Whether `lock` names a running Claude session that `run`, the latest Work
/// of its worktree, has not seen, while that Work awaits review or its
/// sessions have all ended: then a session started there again.
fn relocked(run: &RunRecord, lock: &Lock) -> bool {
    let Some(attachment) = run.attachment.as_ref() else {
        return false;
    };
    let Lock::Session { name, pid, .. } = lock else {
        return false;
    };
    let over = super::awaits_review(run)
        || (run.outcome.lifecycle == LifecycleState::Working
            && attachment.sessions.iter().all(|s| s.ended_at.is_some()));
    let seen = attachment.agent_process.as_ref().map(|agent| agent.pid) == Some(*pid)
        || attachment
            .sessions
            .iter()
            .any(|s| s.session_id == format!("lock:{name}@{pid}"));
    over && run.outcome.closed.is_none() && !seen && claude::liveness(lock) == Liveness::Running
}

fn lock_pid(lock: &Lock) -> Option<u32> {
    match lock {
        Lock::Session { pid, .. } => Some(*pid),
        Lock::Other(_) => None,
    }
}

/// Register `session` of the runtime a person knows as `name` as Work in
/// `workspace`, with its running `agent` process, noting a failure once.
fn register(
    state: &State,
    root: &Path,
    workspace: &Path,
    name: &str,
    session: Result<RuntimeSession>,
    agent: Option<u32>,
) {
    let ended = session.as_ref().is_ok_and(|s| s.ended_at.is_some());
    let observable = session
        .as_ref()
        .is_ok_and(|s| !UNOBSERVED.iter().any(|r| r.agent == s.provider));
    let result = session.and_then(|session| {
        super::registration::discovered(state, root, workspace, session, agent)
    });
    let mut failed = FAILED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match result {
        Ok(Some(run_id)) => {
            failed.remove(workspace);
            note(&format!(
                "found a{} {name} session in {}; tracking it as Work {run_id}{}",
                if ended { "n ended" } else { "" },
                workspace.display(),
                if observable {
                    String::new()
                } else {
                    format!(
                        "; its end is not observable: when it is done, dispatch finish \
                         {run_id}, or dispatch close {run_id} --reason unfinished"
                    )
                }
            ));
        }
        // Someone else is registering it, or has: the next tick sees which.
        Ok(None) => {}
        Err(error) => {
            if failed.insert(workspace.to_owned()) {
                note(&format!(
                    "could not track the {name} session in {}: {error:#}",
                    workspace.display()
                ));
            }
        }
    }
}

/// The observed session of a worktree whose lock names its process; `ended`
/// now, when that process was seen gone.
fn locked_session(found: &Found, ended: bool) -> Result<RuntimeSession> {
    match &found.made {
        Made::Claude(WorktreeSignals {
            lock: Some(lock), ..
        }) => session(lock, ended),
        // Since Git made the worktree, as Hermes's lock records no start.
        Made::Hermes(name) => {
            let mut session = unobserved_session("hermes", name, &found.path);
            session.ended_at = ended.then(Utc::now);
            session.end_reason = ended.then(|| "process_gone".into());
            Ok(session)
        }
        _ => anyhow::bail!("not a session lock"),
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

/// The session a runtime in `UNOBSERVED` began in `worktree`: since when is
/// not recorded, so since Git made the worktree, its `.git` file written
/// then; open, since its end is not observable.
fn unobserved_session(agent: &str, session_id: &str, worktree: &Path) -> RuntimeSession {
    let made = fs::metadata(worktree.join(".git")).and_then(|metadata| metadata.modified());
    RuntimeSession {
        provider: agent.into(),
        session_id: session_id.into(),
        source: "discovered".into(),
        started_at: made.map_or_else(|_| Utc::now(), DateTime::<Utc>::from),
        ended_at: None,
        end_reason: None,
        model: None,
    }
}

/// The root's linked worktrees that a runtime made, from `git worktree
/// list`; missing and prunable ones are skipped. Each is tested only at the
/// path Git gives, wherever it is.
fn runtime_worktrees(root: &Path) -> Result<Vec<Found>> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut list = source::git_command(root);
    list.args(["worktree", "list", "--porcelain"]);
    let output = source::checked_output(list, "failed to list the project's worktrees")?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut found = Vec::new();
    for block in text.split("\n\n") {
        let (mut path, mut branch, mut lock, mut prunable) = (None, None, None, false);
        for line in block.lines() {
            if let Some(listed) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(listed));
            } else if let Some(listed) = line.strip_prefix("branch ") {
                branch = Some(listed);
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
        let made = if claude::made_by_claude(root, &path, &signals) {
            Made::Claude(signals)
        } else if let Some(name) = hermes::session(root, &path, branch) {
            Made::Hermes(name)
        } else {
            let Some((runtime, session_id)) = UNOBSERVED.iter().find_map(|runtime| {
                let session = (runtime.session)(root, &path, branch, home.as_deref())?;
                Some((runtime, session))
            }) else {
                continue;
            };
            Made::Unobserved {
                runtime,
                session_id,
            }
        };
        found.push(Found {
            path,
            made,
            lock: lock.filter(|reason| !reason.is_empty()),
        });
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
