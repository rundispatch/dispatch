//! What hooks and the project owner pass each other as files, so that a hook
//! never opens the database, takes a lock or waits.
//!
//! - **Notes** for a live agent. The owner keeps at most one inbox per
//!   workspace, `inbox/<sha256 of its canonical path>.json`, with the notes
//!   still pending. A hook delivers them by renaming that file into
//!   `inbox/delivered/`: the rename is the claim, so each note reaches the
//!   agent once however many hooks race for it. The owner then records
//!   `note.delivered`, as it records `note.written` when it writes an inbox
//!   and `note.expired` when the Work stops being in progress first.
//! - **Marks**: a session's end, or a wait on the person, that a hook
//!   reports. Each is one file under `marks/`, recorded on its Work by the
//!   owner on its next tick, or by the next owner if none runs.

use std::{
    collections::HashSet,
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ulid::Ulid;

use super::{attach, persist_event, registration};
use crate::{
    EventRecord, LifecycleState, NeedsYou, RunRecord,
    db::Database,
    lock::OperationLock,
    state::{State, write_atomically, write_durably},
};

pub const WRITTEN: &str = "note.written";
pub const DELIVERED: &str = "note.delivered";
pub const EXPIRED: &str = "note.expired";

/// The most text one delivery carries, in characters: well under the 10,000
/// Claude Code takes in one `additionalContext`.
pub const MAX_TEXT: usize = 8_000;

/// Under the state root.
const INBOX: &str = "inbox";
const MARKS: &str = "marks";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub id: String,
    pub text: String,
}

#[derive(Serialize, Deserialize)]
struct Inbox {
    version: u32,
    run_id: String,
    notes: Vec<Note>,
}

/// The inbox of the workspace at `workspace`, a canonical path.
fn path(state: &State, workspace: &Path) -> PathBuf {
    let key = hex::encode(Sha256::digest(workspace.as_os_str().as_encoded_bytes()));
    state.root.join(INBOX).join(format!("{key}.json"))
}

/// The hook's side: take the inbox of `workspace` for delivery through
/// `via`, the runtime's event, and return its text, a line or more per note.
/// `None` when nothing is pending. Notes past `MAX_TEXT` are delivered, as
/// far as the agent knows, only later: the owner puts them back.
pub(crate) fn claim(state: &State, workspace: &Path, via: &str) -> Option<String> {
    let pending = path(state, workspace);
    // A name of its own, so a claim never replaces one the owner has not
    // recorded yet; its time is the delivery's.
    let taken = pending
        .with_file_name("delivered")
        .join(format!("{}.{via}.json", Ulid::new()));
    fs::rename(&pending, &taken).ok()?;
    let inbox: Inbox = serde_json::from_slice(&fs::read(&taken).ok()?).ok()?;
    let notes = &inbox.notes[..fitting(&inbox.notes)];
    let text: Vec<&str> = notes.iter().map(|note| note.text.as_str()).collect();
    (!text.is_empty()).then(|| text.join("\n"))
}

/// How many of `notes`, from the first, one delivery carries: as many as
/// keep its text within `MAX_TEXT`, and at least one.
fn fitting(notes: &[Note]) -> usize {
    let mut length = 0;
    let fit = notes
        .iter()
        .take_while(|note| {
            length += note.text.chars().count() + 1;
            length <= MAX_TEXT + 1
        })
        .count();
    fit.max(notes.len().min(1))
}

/// Add `notes`, as `(id, text)`, to the inbox of Work `run_id`'s workspace,
/// after any still pending there, and record `note.written` for each one not
/// pending already. `false`: the Work is busy, and nothing was written; the
/// owner tries again on a later tick.
pub fn write(state: &State, run_id: &str, notes: &[(String, String)]) -> Result<bool> {
    for (id, text) in notes {
        anyhow::ensure!(
            text.chars().count() <= MAX_TEXT,
            "note {id} is longer than {MAX_TEXT} characters"
        );
    }
    let Ok(_lock) = OperationLock::acquire(&run_lock(state, run_id), "run has a foreground owner")
    else {
        return Ok(false);
    };
    let mut run = state.load_run(run_id)?;
    let notes = notes
        .iter()
        .map(|(id, text)| Note {
            id: id.clone(),
            text: text.clone(),
        })
        .collect();
    let added = put(state, workspace(&run)?, run_id, notes)?;
    if !added.is_empty() {
        let db = Database::open(state.db_path())?;
        for note in added {
            let payload = serde_json::json!({"note": note.id, "text": note.text});
            record(state, &db, &mut run, WRITTEN, Utc::now(), payload)?;
        }
    }
    Ok(true)
}

/// Merge `notes` into the inbox of `workspace`, Work `run_id`'s, and return
/// those it did not hold yet. The caller holds the run's lock.
fn put(state: &State, workspace: &Path, run_id: &str, notes: Vec<Note>) -> Result<Vec<Note>> {
    let pending = path(state, workspace);
    fs::create_dir_all(pending.with_file_name("delivered"))?;
    let held = pending.with_extension("merging");
    let mut inbox = take(&pending)?.unwrap_or_else(|| Inbox {
        version: 1,
        run_id: run_id.to_owned(),
        notes: Vec::new(),
    });
    if inbox.run_id != run_id {
        fs::rename(&held, &pending)?;
        anyhow::bail!(
            "the inbox of {} holds Work {}'s notes",
            workspace.display(),
            inbox.run_id
        );
    }
    let mut added = Vec::new();
    for note in notes {
        if !inbox.notes.iter().any(|known| known.id == note.id) {
            inbox.notes.push(note.clone());
            added.push(note);
        }
    }
    if inbox.notes.is_empty() {
        let _ = fs::remove_file(&held);
        return Ok(added);
    }
    write_atomically(&held, &serde_json::to_vec(&inbox)?)?;
    fs::rename(&held, &pending)?;
    Ok(added)
}

/// Move the inbox at `pending` aside, where no hook takes it, and read it.
/// The notes are in one place at every moment: pending, aside, or both
/// only when a merge was interrupted after putting its result back, which
/// then holds everything the one aside does.
fn take(pending: &Path) -> Result<Option<Inbox>> {
    let held = pending.with_extension("merging");
    match fs::rename(pending, &held) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    match fs::read(&held) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// A session's end, or its wait on the person, as a hook reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Mark {
    pub root: PathBuf,
    pub workspace: PathBuf,
    pub provider: String,
    pub session_id: String,
    pub at: DateTime<Utc>,
    pub what: Marked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Marked {
    SessionEnd { reason: String },
    NeedsYou(NeedsYou),
}

/// The hook's side: leave `mark` for the project owner.
pub(crate) fn mark(state: &State, mark: &Mark) -> Result<()> {
    let path = state.root.join(MARKS).join(format!("{}.json", Ulid::new()));
    write_durably(&path, &serde_json::to_vec(mark)?)
}

/// The owner's step, each tick, for the project at `root`, whose Work is
/// `runs`: record the marks hooks left, the notes they delivered, and the
/// notes whose Work is no longer in progress. Whatever cannot be done now
/// (its Work busy) stays for a later tick; each error goes to `errors`.
/// Returns whether anything was recorded.
pub(crate) fn settle(
    state: &State,
    root: &Path,
    runs: &[RunRecord],
    errors: &mut Vec<anyhow::Error>,
) -> bool {
    let mut recorded = false;
    let mut collect = |result: Result<bool>| match result {
        Ok(changed) => recorded |= changed,
        Err(error) => errors.push(error),
    };
    // Marks in the order they were made; one that waits holds back the
    // later ones of its workspace.
    let mut waiting = HashSet::new();
    for path in files(&state.root.join(MARKS), "json") {
        let mark = fs::read(&path)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| Ok(serde_json::from_slice::<Mark>(&bytes)?));
        let mark = match mark {
            Ok(mark) if mark.root != root || waiting.contains(&mark.workspace) => continue,
            Ok(mark) => mark,
            Err(error) => {
                let _ = fs::remove_file(&path);
                collect(Err(
                    error.context(format!("unreadable mark {}", path.display()))
                ));
                continue;
            }
        };
        let applied = apply(state, &mark).and_then(|applied| {
            if applied.is_some() {
                fs::remove_file(&path)?;
            }
            Ok(applied)
        });
        if !matches!(applied, Ok(Some(_))) {
            waiting.insert(mark.workspace.clone());
        }
        collect(applied.map(|applied| applied == Some(true)));
    }
    let inbox = state.root.join(INBOX);
    for path in files(&inbox.join("delivered"), "json") {
        collect(delivered(state, &path, runs));
    }
    for path in files(&inbox, "json")
        .into_iter()
        .chain(files(&inbox, "merging"))
    {
        collect(left(state, &path, runs));
    }
    recorded
}

/// The files in `directory` with `extension`, by name; never a temporary one.
fn files(directory: &Path, extension: &str) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|path| {
            path.extension().is_some_and(|found| found == extension)
                && !path
                    .file_name()
                    .is_some_and(|name| name.as_encoded_bytes().starts_with(b"."))
        })
        .collect();
    found.sort();
    found
}

/// Record `mark` on the Work it belongs to, if any is active. The Work is
/// found by its workspace or, failing that, by the session: Claude Code
/// reports a worktree session's end from the main checkout. A registration
/// whose S0 is durable is published first; one being registered right now
/// leaves the mark for a later tick, as does Work whose lock is held
/// (`None`): the owner never waits on a typed command. `Some(false)`: no
/// Work to record it on.
fn apply(state: &State, mark: &Mark) -> Result<Option<bool>> {
    let (provider, session) = (mark.provider.as_str(), mark.session_id.as_str());
    registration::publish_first(state, &mark.workspace, Duration::ZERO)?;
    let mut run_id = attach::find_active_attachment(state, &mark.workspace)?;
    if run_id.is_none() {
        registration::publish_session_first(state, &mark.root, provider, session, Duration::ZERO)?;
        run_id = attach::find_session_attachment(state, &mark.root, provider, session)?;
    }
    let Some(run_id) = run_id else {
        return Ok(Some(false));
    };
    let recorded = match &mark.what {
        Marked::SessionEnd { reason } => {
            let at = mark.at;
            attach::record_session_end(
                state,
                &run_id,
                provider,
                session,
                reason,
                at,
                Duration::ZERO,
            )?
        }
        Marked::NeedsYou(waiting) => attach::record_needs_you(state, &run_id, waiting.clone())?,
    };
    Ok(recorded.then_some(true))
}

/// An inbox a hook took at `path`: its notes delivered, and any past what
/// the hook delivered put back for the next one.
fn delivered(state: &State, path: &Path, runs: &[RunRecord]) -> Result<bool> {
    let name = path.file_stem().unwrap_or_default().to_string_lossy();
    let (id, via) = name.split_once('.').unwrap_or((&name, ""));
    let at = Ulid::from_string(id).map_or_else(|_| Utc::now(), |id| id.datetime().into());
    let inbox: Inbox = serde_json::from_slice(&fs::read(path)?)
        .with_context(|| format!("unreadable delivered notes {}", path.display()))?;
    let Some(run) = runs.iter().find(|run| run.id == inbox.run_id) else {
        return Ok(false);
    };
    let Ok(_lock) = OperationLock::acquire(&run_lock(state, &run.id), "run has a foreground owner")
    else {
        return Ok(false);
    };
    let mut run = state.load_run(&run.id)?;
    let db = Database::open(state.db_path())?;
    let (sent, rest) = inbox.notes.split_at(fitting(&inbox.notes));
    for note in sent {
        let payload = serde_json::json!({"note": note.id, "via": via});
        record(state, &db, &mut run, DELIVERED, at, payload)?;
    }
    if run.outcome.lifecycle == LifecycleState::Working {
        put(state, workspace(&run)?, &run.id, rest.to_vec())?;
    } else {
        expire(state, &db, &mut run, rest)?;
    }
    fs::remove_file(path)?;
    Ok(true)
}

/// A pending inbox at `path`: expired once its Work is no longer in
/// progress, and put back where a merge left it aside.
fn left(state: &State, path: &Path, runs: &[RunRecord]) -> Result<bool> {
    let bytes = match fs::read(path) {
        // Expiring the inbox beside it took this one too.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        bytes => bytes?,
    };
    let inbox: Inbox = serde_json::from_slice(&bytes)
        .with_context(|| format!("unreadable inbox {}", path.display()))?;
    let Some(run) = runs.iter().find(|run| run.id == inbox.run_id) else {
        return Ok(false);
    };
    let working = run.outcome.lifecycle == LifecycleState::Working;
    if working && path.extension().is_some_and(|found| found == "json") {
        return Ok(false);
    }
    let Ok(_lock) = OperationLock::acquire(&run_lock(state, &run.id), "run has a foreground owner")
    else {
        return Ok(false);
    };
    let mut run = state.load_run(&run.id)?;
    let pending = path.with_extension("json");
    if run.outcome.lifecycle == LifecycleState::Working {
        put(state, workspace(&run)?, &run.id, Vec::new())?;
        return Ok(false);
    }
    let Some(inbox) = take(&pending)? else {
        return Ok(false);
    };
    let db = Database::open(state.db_path())?;
    expire(state, &db, &mut run, &inbox.notes)?;
    fs::remove_file(pending.with_extension("merging"))?;
    Ok(true)
}

fn expire(state: &State, db: &Database, run: &mut RunRecord, notes: &[Note]) -> Result<()> {
    for note in notes {
        let payload = serde_json::json!({"note": note.id, "lifecycle": run.outcome.lifecycle});
        record(state, db, run, EXPIRED, Utc::now(), payload)?;
    }
    Ok(())
}

fn record(
    state: &State,
    db: &Database,
    run: &mut RunRecord,
    kind: &str,
    at: DateTime<Utc>,
    payload: serde_json::Value,
) -> Result<()> {
    let event = EventRecord {
        run_id: run.id.clone(),
        candidate_label: None,
        event_type: kind.into(),
        timestamp: at,
        payload,
        ..EventRecord::default()
    };
    persist_event(state, db, event, run)
}

fn workspace(run: &RunRecord) -> Result<&Path> {
    Ok(&run
        .attachment
        .as_ref()
        .context("only attached Work has an inbox")?
        .workspace)
}

fn run_lock(state: &State, run_id: &str) -> PathBuf {
    state.run_dir(run_id).join(".operation.lock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NeedsYouKind, source};

    /// Attached Work in a linked worktree of a fresh project.
    fn attached() -> (tempfile::TempDir, State, RunRecord) {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap().join("project");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("file.txt"), "before\n").unwrap();
        let git = |args: &[&str]| {
            let mut git = source::git_command(&root);
            git.args(["-c", "user.name=T", "-c", "user.email=t@example.invalid"]);
            assert!(git.args(args).status().unwrap().success());
        };
        git(&["init", "--quiet"]);
        git(&["add", "."]);
        git(&["commit", "--quiet", "-m", "initial"]);
        let workspace = root.with_file_name("worktree");
        git(&[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "w",
            workspace.to_str().unwrap(),
        ]);
        let state = State {
            root: temp.path().join("state"),
        };
        state.initialize().unwrap();
        let run = attach::create(
            &state,
            attach::AttachRequest {
                workspace,
                root: Some(root),
                task: None,
                agent: None,
                pid: None,
                command: None,
                allow_unsafe_local: false,
                auto_apply: false,
                runtime: None,
            },
        )
        .unwrap();
        (temp, state, run)
    }

    fn notes(texts: &[(&str, &str)]) -> Vec<(String, String)> {
        texts
            .iter()
            .map(|(id, text)| (id.to_string(), text.to_string()))
            .collect()
    }

    fn events(state: &State, run: &RunRecord, kind: &str) -> Vec<serde_json::Value> {
        let db = Database::open(state.db_path()).unwrap();
        db.events_for_run(&run.id)
            .unwrap()
            .into_iter()
            .filter(|event| event.event_type == kind)
            .map(|event| event.payload)
            .collect()
    }

    /// The owner's step, which must not fail, over the project's Work now.
    fn tick(state: &State, run: &RunRecord) -> bool {
        let mut errors = Vec::new();
        let runs = [state.load_run(&run.id).unwrap()];
        let recorded = settle(state, &run.source_path, &runs, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        recorded
    }

    fn workspace_of(run: &RunRecord) -> PathBuf {
        run.attachment.as_ref().unwrap().workspace.clone()
    }

    #[test]
    fn notes_are_written_once_merged_with_those_pending_and_delivered_once() {
        let (_temp, state, run) = attached();
        let workspace = workspace_of(&run);
        assert!(write(&state, &run.id, &notes(&[("n1", "one"), ("n2", "two")])).unwrap());
        assert!(write(&state, &run.id, &notes(&[("n2", "two"), ("n3", "three")])).unwrap());
        let written: Vec<_> = events(&state, &run, WRITTEN)
            .iter()
            .map(|event| event["note"].clone())
            .collect();
        assert_eq!(written, ["n1", "n2", "n3"]);

        assert_eq!(
            claim(&state, &workspace, "PostToolUse").as_deref(),
            Some("one\ntwo\nthree")
        );
        assert_eq!(claim(&state, &workspace, "PostToolUse"), None);
        assert!(tick(&state, &run));
        let delivered = events(&state, &run, DELIVERED);
        assert_eq!(delivered.len(), 3);
        assert!(delivered.iter().all(|event| event["via"] == "PostToolUse"));
        assert!(files(&state.root.join("inbox/delivered"), "json").is_empty());
        assert!(!tick(&state, &run), "nothing more to record");
        // Busy Work is written later: nothing now.
        let _busy = OperationLock::acquire(&run_lock(&state, &run.id), "busy").unwrap();
        assert!(!write(&state, &run.id, &notes(&[("n4", "four")])).unwrap());
        assert_eq!(claim(&state, &workspace, "PostToolUse"), None);
    }

    #[test]
    fn notes_past_the_limit_wait_for_the_next_delivery() {
        let (_temp, state, run) = attached();
        let workspace = workspace_of(&run);
        let long = |c: &str| c.repeat(3_000);
        let (a, b, c) = (long("a"), long("b"), long("c"));
        write(
            &state,
            &run.id,
            &notes(&[("n1", &a), ("n2", &b), ("n3", &c)]),
        )
        .unwrap();
        assert!(
            write(
                &state,
                &run.id,
                &notes(&[("n4", &"d".repeat(MAX_TEXT + 1))])
            )
            .is_err()
        );
        let first = claim(&state, &workspace, "SessionStart").unwrap();
        assert_eq!(first, format!("{a}\n{b}"));
        assert!(first.chars().count() <= MAX_TEXT);
        tick(&state, &run);
        assert_eq!(events(&state, &run, DELIVERED).len(), 2);
        assert_eq!(claim(&state, &workspace, "PostToolUse"), Some(c));
        tick(&state, &run);
        let delivered: Vec<_> = events(&state, &run, DELIVERED)
            .iter()
            .map(|event| (event["note"].clone(), event["via"].clone()))
            .collect();
        assert_eq!(
            delivered,
            [
                ("n1".into(), "SessionStart".into()),
                ("n2".into(), "SessionStart".into()),
                ("n3".into(), "PostToolUse".into())
            ]
        );
        assert_eq!(events(&state, &run, WRITTEN).len(), 3, "written once each");
    }

    #[test]
    fn notes_left_for_work_no_longer_in_progress_expire() {
        let (_temp, state, run) = attached();
        let workspace = workspace_of(&run);
        write(
            &state,
            &run.id,
            &notes(&[("n1", &"a".repeat(5_000)), ("n2", &"b".repeat(5_000))]),
        )
        .unwrap();
        // Taken, but its Work is closed before the owner sees it: what the
        // hook delivered was delivered, and the rest expires.
        assert!(claim(&state, &workspace, "PostToolUse").is_some());
        write(&state, &run.id, &notes(&[("n3", "three")])).unwrap();
        attach::close(
            &state,
            attach::CloseRequest {
                run_id: run.id.clone(),
                reason: crate::CloseReason::Unfinished,
                replacement: None,
                explanation: None,
            },
        )
        .unwrap();
        assert!(tick(&state, &run));
        let ids = |kind| -> Vec<serde_json::Value> {
            events(&state, &run, kind)
                .iter()
                .map(|event| event["note"].clone())
                .collect()
        };
        assert_eq!(ids(DELIVERED), ["n1"]);
        let mut expired = ids(EXPIRED);
        expired.sort_by_key(|id| id.to_string());
        assert_eq!(expired, ["n2", "n3"]);
        assert!(files(&state.root.join(INBOX), "json").is_empty());
        assert_eq!(claim(&state, &workspace, "PostToolUse"), None);
    }

    #[test]
    fn a_merge_the_owner_never_finished_is_put_back() {
        let (_temp, state, run) = attached();
        let workspace = workspace_of(&run);
        write(&state, &run.id, &notes(&[("n1", "one")])).unwrap();
        let pending = path(&state, &workspace);
        fs::rename(&pending, pending.with_extension("merging")).unwrap();
        assert_eq!(claim(&state, &workspace, "PostToolUse"), None);
        tick(&state, &run);
        assert_eq!(
            claim(&state, &workspace, "PostToolUse").as_deref(),
            Some("one")
        );
        // Interrupted after putting its result back: the one aside is a
        // part of it.
        write(&state, &run.id, &notes(&[("n2", "two")])).unwrap();
        fs::copy(&pending, pending.with_extension("merging")).unwrap();
        write(&state, &run.id, &notes(&[("n3", "three")])).unwrap();
        assert_eq!(
            claim(&state, &workspace, "PostToolUse").as_deref(),
            Some("two\nthree")
        );
    }

    #[test]
    fn marks_are_recorded_on_their_work_and_left_for_their_own_projects_owner() {
        let (temp, state, run) = attached();
        let at = Utc::now() - chrono::Duration::minutes(5);
        let waiting = |workspace: PathBuf, root: &Path| Mark {
            root: root.to_owned(),
            workspace,
            provider: "claude".into(),
            session_id: "s1".into(),
            at,
            what: Marked::NeedsYou(NeedsYou {
                kind: NeedsYouKind::Permission,
                message: "Claude needs your permission".into(),
                at,
                transcript: None,
                provider: "claude".into(),
                session_id: "s1".into(),
            }),
        };
        let other = temp.path().join("other");
        mark(&state, &waiting(workspace_of(&run), &other)).unwrap();
        mark(&state, &waiting(temp.path().join("gone"), &run.source_path)).unwrap();
        mark(&state, &waiting(workspace_of(&run), &run.source_path)).unwrap();
        assert!(tick(&state, &run));
        let recorded = state.load_run(&run.id).unwrap();
        let needs_you = recorded.attachment.unwrap().needs_you.unwrap();
        assert_eq!(
            (needs_you.kind, needs_you.at),
            (NeedsYouKind::Permission, at)
        );
        assert_eq!(events(&state, &run, "runtime.needs_you").len(), 1);
        // Only the other project's mark is left, for its owner.
        let left = files(&state.root.join(MARKS), "json");
        assert_eq!(left.len(), 1);
        let mark: Mark = serde_json::from_slice(&fs::read(&left[0]).unwrap()).unwrap();
        assert_eq!(mark.root, other);
    }

    #[test]
    fn a_mark_on_busy_work_waits_for_a_later_tick_without_stalling_this_one() {
        let (_temp, state, run) = attached();
        let at = Utc::now();
        let what = Marked::NeedsYou(NeedsYou {
            kind: NeedsYouKind::Permission,
            message: "Claude needs your permission".into(),
            at,
            transcript: None,
            provider: "claude".into(),
            session_id: "s1".into(),
        });
        mark(
            &state,
            &Mark {
                root: run.source_path.clone(),
                workspace: workspace_of(&run),
                provider: "claude".into(),
                session_id: "s1".into(),
                at,
                what,
            },
        )
        .unwrap();
        // A typed command holds the run's lock, as `finish` does through its checks.
        let busy = OperationLock::acquire(&run_lock(&state, &run.id), "busy").unwrap();
        let started = std::time::Instant::now();
        assert!(!tick(&state, &run));
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "the tick waited {:?} for a busy run",
            started.elapsed()
        );
        assert_eq!(files(&state.root.join(MARKS), "json").len(), 1);
        assert!(events(&state, &run, "runtime.needs_you").is_empty());
        drop(busy);
        assert!(tick(&state, &run));
        assert_eq!(events(&state, &run, "runtime.needs_you").len(), 1);
        assert!(files(&state.root.join(MARKS), "json").is_empty());
    }
}
