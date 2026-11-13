//! Catch-up in place. When Work lands a change that live Work relies on at
//! the declaration level, that Work's agent is asked, through its runtime's
//! hooks, to bring the change into its workspace: merge the recorded source
//! (`refs/dispatch/source`), update what uses it, and run the project's
//! checks. The ask is fixed text, made once per landing and Work, and nothing
//! is done on the agent's behalf; its merge then moves S0 as any other does
//! (`s0::follow`).
//!
//! - **The trigger**, each tick of the project owner: a landing recorded at
//!   the project (`s0::recorded`) whose `interaction.landed` was observed with
//!   live Claude Code or Hermes Work as a counterpart that uses a declaration
//!   whose signature the landing changed or which it removed, or that changes
//!   the same declaration. A whole-file or textual interaction never asks, nor
//!   is Work whose S0 a merge would not move: a live wrapper keeps its S0.
//! - **The record**, `catchup/work/<key>.json`, kept while the Work is in
//!   progress: the landings it was asked about. A landing is put there before
//!   its ask is written, so a crash asks at most once. That the record exists
//!   tells a waiting hook the workspace has Work.
//! - **The ask**, `catchup/<key>.json`, at most one per Work: every landing
//!   since the last ask a hook delivered. A hook delivers it by renaming it
//!   into `catchup/delivered/` (`claim`), and the owner records
//!   `catchup.delivered`. It expires when the Work's S0 already moved to a
//!   source recorded at or after its landings, or when the Work is no longer
//!   in progress.
//!
//! `<key>` is the inbox's, the SHA-256 of the workspace's canonical path.
//! Asks are kept apart from notes (`inbox`), which other hook events deliver.

use std::{
    collections::{BTreeSet, HashMap},
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ulid::Ulid;

use super::{inbox, notify, s0};
use crate::{
    EventRecord, LifecycleState, RunRecord,
    coherence::{
        interactions::{Change, Interaction, Rule, Side, Target},
        measure,
    },
    db::Database,
    lock::OperationLock,
    state::{State, write_atomically, write_durably},
};

pub const WRITTEN: &str = "catchup.written";
pub const DELIVERED: &str = "catchup.delivered";
pub const EXPIRED: &str = "catchup.expired";

/// Under the state root.
const DIRECTORY: &str = "catchup";

/// The runtimes whose hooks deliver an ask, as sessions name them.
const ASKED: [&str; 2] = ["claude", "hermes"];

/// The most targets an ask lists; the rest are counted.
const TARGETS: usize = 4;
/// The most characters of a signature an ask shows.
const SIGNATURE: usize = 200;

/// The landings the Work in one workspace was asked about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    version: u32,
    run_id: String,
    asked: BTreeSet<String>,
}

/// An ask, pending or delivered.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Ask {
    version: u32,
    run_id: String,
    landings: Vec<Landed>,
    text: String,
}

/// One landing an ask names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Landed {
    run_id: String,
    /// The runtime it ran in, as the person knows it.
    runtime: String,
    /// The recorded source that includes it, and when that was recorded.
    commit: String,
    at: DateTime<Utc>,
    /// What it changed that the Work relies on, seen from the landed run's
    /// side, each with the old and new signature when they are known.
    targets: Vec<(Interaction, Option<(String, String)>)>,
}

/// What the owner remembers for this step between ticks: each landed run's
/// `interaction.landed`, read once, with the signatures it changed; `None`
/// when it recorded none.
#[derive(Default)]
pub(crate) struct Landings(HashMap<String, Option<(measure::Landing, notify::Signatures)>>);

/// The owner's step, each tick, for the project at `root`, whose Work is
/// `runs`: record the asks hooks delivered, expire those no longer needed,
/// and ask live Work about landings since it began or last moved its S0.
/// Busy Work is left for a later tick.
pub(crate) fn step(
    state: &State,
    root: &Path,
    runs: &[RunRecord],
    landings: &mut Landings,
) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    let mut collect = |result: Result<()>| {
        if let Err(error) = result {
            errors.push(error);
        }
    };
    let directory = state.root.join(DIRECTORY);
    for path in inbox::files(&directory.join("delivered"), "json") {
        collect(delivered(state, &path, runs));
    }
    for path in inbox::files(&directory, "json")
        .into_iter()
        .chain(inbox::files(&directory, "merging"))
    {
        collect(left(state, &path, runs));
    }
    let mut records = records(state, runs, &mut errors);
    let live: Vec<&RunRecord> = runs.iter().filter(|run| live(run)).collect();
    let oldest = live.iter().filter_map(|run| run.s0_commit()).map(|s0| s0.1);
    let recorded = match oldest
        .min()
        .map(|oldest| (oldest, s0::recorded(state, root)))
    {
        Some((oldest, Ok(recorded))) => recorded.into_iter().filter(|r| r.at > oldest).collect(),
        Some((_, Err(error))) => {
            errors.push(error);
            Vec::new()
        }
        None => Vec::new(),
    };
    landings
        .0
        .retain(|run, _| recorded.iter().any(|r| r.landed_run == *run));
    for source in &recorded {
        if !landings.0.contains_key(&source.landed_run) {
            match notify::landing(state, runs, &source.landed_run) {
                Ok(found) => {
                    landings.0.insert(source.landed_run.clone(), found);
                }
                Err(error) => errors.push(error),
            }
        }
    }
    for run in live {
        let workspace = &run.attachment.as_ref().expect("live Work").workspace;
        let kept = records.remove(&record_path(state, workspace));
        if let Err(error) = ask(state, run, runs, &recorded, landings, kept) {
            errors.push(error);
        }
    }
    errors
}

/// Ask `run` about the landings in `recorded` that break what it relies on
/// and that it was not asked about, merged into its ask still pending, if
/// any. `kept` is the record of its workspace; one is written for it first.
fn ask(
    state: &State,
    run: &RunRecord,
    runs: &[RunRecord],
    recorded: &[s0::Recorded],
    landings: &Landings,
    kept: Option<Record>,
) -> Result<()> {
    let workspace = &run.attachment.as_ref().expect("live Work").workspace;
    let path = record_path(state, workspace);
    let mut record = match kept {
        Some(record) if record.run_id == run.id => record,
        _ => {
            let record = Record {
                version: 1,
                run_id: run.id.clone(),
                asked: BTreeSet::new(),
            };
            write_durably(&path, &serde_json::to_vec(&record)?)?;
            record
        }
    };
    let Some((_, since)) = run.s0_commit() else {
        return Ok(());
    };
    let new: Vec<Landed> = recorded
        .iter()
        .filter(|source| source.at > since && !record.asked.contains(&source.landed_run))
        .filter_map(|source| {
            let found = landings.0.get(&source.landed_run)?.as_ref()?;
            breaking(source, found, run, runs)
        })
        .collect();
    if new.is_empty() {
        return Ok(());
    }
    let Ok(_lock) = OperationLock::acquire(
        &inbox::run_lock(state, &run.id),
        "run has a foreground owner",
    ) else {
        return Ok(());
    };
    let mut run = state.load_run(&run.id)?;
    if !in_progress(&run) {
        return Ok(());
    }
    // Asked before the ask is written: a crash between the two asks nothing,
    // and never twice.
    record
        .asked
        .extend(new.iter().map(|landed| landed.run_id.clone()));
    write_durably(&path, &serde_json::to_vec(&record)?)?;
    let ask = put(state, workspace, &run.id, new.clone())?;
    let targets: Vec<&Target> = new
        .iter()
        .flat_map(|landed| landed.targets.iter().map(|(i, _)| &i.target))
        .collect();
    let payload = serde_json::json!({
        "landings": ids(&new), "targets": targets, "text": ask.text,
    });
    let db = Database::open(state.db_path())?;
    inbox::record(state, &db, &mut run, WRITTEN, Utc::now(), payload)
}

/// What the landing recorded at `source` changed that `run` relies on
/// (`breaks`), as its `interaction.landed` recorded it. `None` when nothing,
/// or when the owner's view did not see the landing as it was applied.
fn breaking(
    source: &s0::Recorded,
    (landing, signatures): &(measure::Landing, notify::Signatures),
    run: &RunRecord,
    runs: &[RunRecord],
) -> Option<Landed> {
    if landing.status != measure::LandingStatus::Observed {
        return None;
    }
    let counterpart = landing
        .counterparts
        .iter()
        .find(|c| c.identity.run_id == run.id)?;
    let targets: Vec<_> = counterpart
        .interactions
        .iter()
        .filter(|interaction| breaks(interaction))
        .map(|i| (i.clone(), signatures.get(&i.target).cloned()))
        .collect();
    let runtime = runs
        .iter()
        .find(|landed| landed.id == source.landed_run)
        .and_then(notify::runtime)
        .unwrap_or("unknown runtime");
    (!targets.is_empty()).then(|| Landed {
        run_id: source.landed_run.clone(),
        runtime: runtime.to_owned(),
        commit: source.commit.clone(),
        at: source.at,
        targets,
    })
}

/// Whether `interaction`, seen from the landed run's side (`Side::A`), asks
/// its counterpart to catch up: the landed run changed the signature of, or
/// removed, a declaration the counterpart uses, or both changed one
/// declaration. A whole file, text alone, or a declaration the counterpart
/// changes and the landed run only uses never does.
fn breaks(interaction: &Interaction) -> bool {
    matches!(interaction.target, Target::Symbol { .. })
        && match interaction.rule {
            Rule::Uses => {
                interaction.writer == Some(Side::A)
                    && matches!(
                        interaction.change,
                        Some(Change::Signature | Change::Removes)
                    )
            }
            Rule::SameDeclaration => true,
            Rule::File | Rule::TextualOverlap => false,
        }
}

/// What an ask says: which Work landed, in which runtime, what it changed
/// that this Work relies on, where those changes are, and what would bring
/// them in. Fixed text, at most eight lines, that informs and asks.
fn text(landings: &[Landed]) -> String {
    let short = crate::state::short_ids(&ids(landings));
    let works: Vec<String> = landings
        .iter()
        .map(|l| format!("Work {} ({})", short[l.run_id.as_str()], l.runtime))
        .collect();
    let items: Vec<String> = landings
        .iter()
        .flat_map(|landed| {
            let by = short[landed.run_id.as_str()];
            landed
                .targets
                .iter()
                .map(move |(i, old_new)| item(by, i, old_new))
        })
        .collect();
    let mut text = format!(
        "Dispatch: {} landed {} this Work relies on.",
        works.join(" and "),
        if items.len() == 1 {
            "a change"
        } else {
            "changes"
        }
    );
    for item in items.iter().take(TARGETS) {
        text += "\n- ";
        text += item;
    }
    if items.len() > TARGETS {
        text += &format!("\n- and {} more.", items.len() - TARGETS);
    }
    // The newest landing's source holds the earlier ones too. The command
    // names its commit, not the ref: Claude Code's worktree isolation refuses
    // any command naming `refs/dispatch/source` (2.1.296).
    let commit = landings
        .iter()
        .max_by_key(|landed| landed.at)
        .map_or("", |newest| newest.commit.as_str());
    text += &format!(
        "\nThese changes are in refs/dispatch/source (commit {}).\nTo catch up: commit your \
         work, run git merge {commit}, update what uses them, and run the project's checks. \
         Dispatch asks once per landing.",
        commit.get(..8).unwrap_or(commit)
    );
    text
}

/// One target of an ask, which `by` changed.
fn item(by: &str, interaction: &Interaction, old_new: &Option<(String, String)>) -> String {
    let what = match &interaction.target {
        Target::Symbol { path, name } => format!("{name} ({path})"),
        Target::File { path } => path.clone(),
    };
    match (interaction.rule, interaction.change, old_new) {
        (Rule::Uses, Some(Change::Removes), _) => {
            format!("{what}, which this Work uses: {by} removed it")
        }
        (Rule::Uses, _, Some((old, new))) => format!(
            "{what}, which this Work uses: {by} changed its signature, {} → {}",
            cap(old),
            cap(new)
        ),
        (Rule::Uses, _, None) => {
            format!("{what}, which this Work uses: {by} changed its signature")
        }
        _ => format!("{what}, which this Work also changes: {by} changed it"),
    }
}

/// A signature on one line, cut at `SIGNATURE` characters with "…".
fn cap(value: &str) -> String {
    let value = value.replace(['\n', '\r'], " ");
    match value.char_indices().nth(SIGNATURE) {
        Some((end, _)) => format!("{}…", &value[..end]),
        None => value,
    }
}

fn ids(landings: &[Landed]) -> Vec<&str> {
    landings
        .iter()
        .map(|landed| landed.run_id.as_str())
        .collect()
}

/// Merge `new` into the ask pending for Work `run_id` in `workspace`, or
/// write one, and return it. The caller holds the run's lock.
fn put(state: &State, workspace: &Path, run_id: &str, new: Vec<Landed>) -> Result<Ask> {
    let pending = pending(state, workspace);
    fs::create_dir_all(pending.with_file_name("delivered"))?;
    let aside = pending.with_extension("merging");
    let mut ask = take(&pending)?.unwrap_or_else(|| Ask {
        version: 1,
        run_id: run_id.to_owned(),
        landings: Vec::new(),
        text: String::new(),
    });
    if ask.run_id != run_id {
        fs::rename(&aside, &pending)?;
        anyhow::bail!(
            "the ask pending for {} is Work {}'s",
            workspace.display(),
            ask.run_id
        );
    }
    ask.landings.extend(new);
    ask.text = text(&ask.landings);
    write_atomically(&aside, &serde_json::to_vec(&ask)?)?;
    fs::rename(&aside, &pending)?;
    Ok(ask)
}

/// Move the ask at `pending` aside, where no hook takes it, and read it. As
/// in the inbox, the ask is pending, aside, or both only when a merge was
/// interrupted after putting its result back, which then holds all of it.
fn take(pending: &Path) -> Result<Option<Ask>> {
    let aside = pending.with_extension("merging");
    match fs::rename(pending, &aside) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    match fs::read(&aside) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// An ask a hook took at `path`: recorded as delivered.
fn delivered(state: &State, path: &Path, runs: &[RunRecord]) -> Result<()> {
    let name = path.file_stem().unwrap_or_default().to_string_lossy();
    let (id, via) = name.split_once('.').unwrap_or((&name, ""));
    let at = Ulid::from_string(id).map_or_else(|_| Utc::now(), |id| id.datetime().into());
    let ask: Ask = serde_json::from_slice(&fs::read(path)?)
        .with_context(|| format!("unreadable delivered ask {}", path.display()))?;
    let Some(run) = runs.iter().find(|run| run.id == ask.run_id) else {
        return Ok(());
    };
    let Ok(_lock) = OperationLock::acquire(
        &inbox::run_lock(state, &run.id),
        "run has a foreground owner",
    ) else {
        return Ok(());
    };
    let mut run = state.load_run(&run.id)?;
    let db = Database::open(state.db_path())?;
    let payload = serde_json::json!({"via": via, "landings": ids(&ask.landings)});
    inbox::record(state, &db, &mut run, DELIVERED, at, payload)?;
    fs::remove_file(path)?;
    Ok(())
}

/// The ask at `path`, pending or left aside by a merge the owner never
/// finished: expired when its Work no longer needs it, and put back where a
/// merge left it.
fn left(state: &State, path: &Path, runs: &[RunRecord]) -> Result<()> {
    let bytes = match fs::read(path) {
        // Taken by a hook, or already settled with the ask beside it.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        bytes => bytes?,
    };
    let ask: Ask = serde_json::from_slice(&bytes)
        .with_context(|| format!("unreadable ask {}", path.display()))?;
    let Some(run) = runs.iter().find(|run| run.id == ask.run_id) else {
        return Ok(());
    };
    let merging = path.extension().is_some_and(|found| found == "merging");
    if !merging && expiry(run, &ask).is_none() {
        return Ok(());
    }
    let Ok(_lock) = OperationLock::acquire(
        &inbox::run_lock(state, &run.id),
        "run has a foreground owner",
    ) else {
        return Ok(());
    };
    let mut run = state.load_run(&run.id)?;
    let pending = path.with_extension("json");
    let Some(ask) = take(&pending)? else {
        return Ok(());
    };
    let aside = pending.with_extension("merging");
    let Some(why) = expiry(&run, &ask) else {
        fs::rename(&aside, &pending)?;
        return Ok(());
    };
    let db = Database::open(state.db_path())?;
    let payload = serde_json::json!({"why": why, "landings": ids(&ask.landings)});
    inbox::record(state, &db, &mut run, EXPIRED, Utc::now(), payload)?;
    fs::remove_file(aside)?;
    Ok(())
}

/// Why `run` no longer needs `ask`: it is no longer in progress, or its S0
/// already moved to a source recorded at or after every landing it names.
fn expiry(run: &RunRecord, ask: &Ask) -> Option<&'static str> {
    if !in_progress(run) {
        return Some("left_working");
    }
    let (_, since) = run.s0_commit()?;
    let moved = ask.landings.iter().all(|landed| landed.at <= since);
    moved.then_some("s0_moved")
}

/// The records kept, by path, once those of Work in `runs` no longer in
/// progress are removed: it is asked nothing more.
fn records(
    state: &State,
    runs: &[RunRecord],
    errors: &mut Vec<anyhow::Error>,
) -> HashMap<PathBuf, Record> {
    let mut kept = HashMap::new();
    for path in inbox::files(&state.root.join(DIRECTORY).join("work"), "json") {
        let record = fs::read(&path)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| Ok(serde_json::from_slice::<Record>(&bytes)?));
        let record = match record {
            Ok(record) => record,
            Err(error) => {
                errors.push(error.context(format!("unreadable record {}", path.display())));
                continue;
            }
        };
        let left = runs
            .iter()
            .any(|run| run.id == record.run_id && !in_progress(run));
        if !left {
            kept.insert(path, record);
        } else if let Err(error) = fs::remove_file(&path) {
            errors.push(error.into());
        }
    }
    kept
}

fn in_progress(run: &RunRecord) -> bool {
    run.outcome.lifecycle == LifecycleState::Working && run.outcome.closed.is_none()
}

/// Work whose S0 follows the merge it would be asked to make (`s0::movable`:
/// in progress, and no live wrapper keeps the S0 it began with), in a
/// workspace still there, where a session of a runtime whose hooks deliver an
/// ask has not ended.
fn live(run: &RunRecord) -> bool {
    s0::movable(run).is_some_and(|attachment| {
        attachment.workspace.is_dir()
            && attachment.sessions.iter().any(|session| {
                ASKED.contains(&session.provider.as_str()) && session.ended_at.is_none()
            })
    })
}

/// What the agent of Work in progress was asked to catch up with that its S0
/// has not followed yet: the landings delivered asks named that were recorded
/// after its S0, the newest one's commit, and when the last ask naming them
/// was delivered.
pub(crate) struct Asked {
    landings: Vec<String>,
    pub(crate) commit: String,
    at: DateTime<Utc>,
}

impl Asked {
    /// As status and watch show it.
    pub(crate) fn line(&self) -> String {
        let ids: Vec<&str> = self.landings.iter().map(String::as_str).collect();
        let short = crate::state::short_ids(&ids);
        let named: Vec<&str> = ids.iter().map(|id| short[id]).collect();
        format!(
            "asked to catch up with {} at {}",
            named.join(" and "),
            self.at.format("%Y-%m-%d %H:%M UTC")
        )
    }
}

/// `run`'s ask, from its `catchup.delivered` events and the sources recorded
/// at its project; `None` when there is none, or it cannot be read.
pub(crate) fn asked(state: &State, run: &RunRecord) -> Option<Asked> {
    if !in_progress(run) {
        return None;
    }
    let (_, since) = run.s0_commit()?;
    let db = Database::open_read_only(state.db_path()).ok()?;
    let events = db.events_for_run(&run.id).ok()?;
    let delivered: Vec<_> = events
        .iter()
        .filter(|event| event.event_type == DELIVERED)
        .collect();
    if delivered.is_empty() {
        return None;
    }
    let names = |event: &EventRecord, id: &str| {
        let landings = event.payload["landings"].as_array();
        landings.is_some_and(|ids| ids.iter().any(|named| named.as_str() == Some(id)))
    };
    // Oldest first, so the last holds the others.
    let open: Vec<s0::Recorded> = s0::recorded(state, &run.source_path)
        .ok()?
        .into_iter()
        .filter(|source| source.at > since)
        .filter(|source| {
            delivered
                .iter()
                .any(|event| names(event, &source.landed_run))
        })
        .collect();
    let at = delivered
        .iter()
        .filter(|event| open.iter().any(|source| names(event, &source.landed_run)))
        .map(|event| event.timestamp)
        .max()?;
    Some(Asked {
        commit: open.last()?.commit.clone(),
        landings: open.into_iter().map(|source| source.landed_run).collect(),
        at,
    })
}

/// The hook's side: take the ask pending for `workspace`, a canonical path,
/// for delivery through `via` (`stop`, `wake`, `verify` or `turn`), and
/// return its text; `None` when there is none. The rename is the claim, so an
/// ask reaches the agent once however many hooks race for it.
pub(crate) fn claim(state: &State, workspace: &Path, via: &str) -> Option<String> {
    let pending = pending(state, workspace);
    let taken = pending
        .with_file_name("delivered")
        .join(format!("{}.{via}.json", Ulid::new()));
    fs::rename(&pending, &taken).ok()?;
    let ask: Ask = serde_json::from_slice(&fs::read(&taken).ok()?).ok()?;
    Some(ask.text)
}

/// Where an ask for `workspace`, a canonical path, waits.
pub(crate) fn pending(state: &State, workspace: &Path) -> PathBuf {
    state
        .root
        .join(DIRECTORY)
        .join(format!("{}.json", key(workspace)))
}

/// Whether the owner keeps a record for Work in `workspace`, a canonical
/// path: whether it may be asked to catch up.
pub(crate) fn followed(state: &State, workspace: &Path) -> bool {
    record_path(state, workspace).exists()
}

fn record_path(state: &State, workspace: &Path) -> PathBuf {
    state
        .root
        .join(DIRECTORY)
        .join("work")
        .join(format!("{}.json", key(workspace)))
}

/// The inbox's key for `workspace`.
fn key(workspace: &Path) -> String {
    hex::encode(Sha256::digest(workspace.as_os_str().as_encoded_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CloseReason, RuntimeSession,
        coherence::measure::LANDED,
        orchestrator::{
            attach::{self, AttachRequest, RuntimeStart},
            registration,
        },
        source,
    };

    const LIB: &str =
        "pub fn total(n: i32) -> i32 {\n    n\n}\n\npub fn scale(n: i32) -> i32 {\n    n * 2\n}\n";

    fn git(dir: &Path, args: &[&str]) {
        let mut git = source::git_command(dir);
        git.args(["-c", "user.name=T", "-c", "user.email=t@example.invalid"]);
        assert!(git.args(args).status().unwrap().success(), "git {args:?}");
    }

    struct Project {
        _temp: tempfile::TempDir,
        root: PathBuf,
        state: State,
        landings: Landings,
    }

    impl Project {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = fs::canonicalize(temp.path()).unwrap().join("project");
            fs::create_dir_all(root.join("src")).unwrap();
            fs::write(root.join("src/lib.rs"), LIB).unwrap();
            git(&root, &["init", "--quiet"]);
            git(&root, &["add", "."]);
            git(&root, &["commit", "--quiet", "-m", "initial"]);
            let state = State {
                root: temp.path().join("state"),
            };
            state.initialize().unwrap();
            Self {
                _temp: temp,
                root,
                state,
                landings: Landings::default(),
            }
        }

        /// Work in a new worktree, registered by a session of `provider`.
        fn work(&self, name: &str, provider: &str) -> RunRecord {
            let workspace = self.root.with_file_name(name);
            let path = workspace.to_str().unwrap();
            git(
                &self.root,
                &["worktree", "add", "--quiet", "-b", name, path],
            );
            let session = RuntimeSession {
                provider: provider.into(),
                session_id: name.into(),
                source: "startup".into(),
                started_at: Utc::now(),
                ended_at: None,
                end_reason: None,
                model: None,
            };
            attach::create(
                &self.state,
                AttachRequest {
                    workspace: workspace.clone(),
                    root: Some(self.root.clone()),
                    task: None,
                    agent: Some(provider.into()),
                    pid: None,
                    command: None,
                    allow_unsafe_local: true,
                    auto_apply: false,
                    runtime: Some(RuntimeStart {
                        session,
                        resumed: false,
                        commit: source::world_commit(&workspace).unwrap(),
                        discovered: false,
                        reopens: None,
                    }),
                },
            )
            .unwrap()
        }

        /// Hermes Work in `.worktrees/hermes-<id>`, as the owner's discovery
        /// registers a `hermes chat --worktree` session.
        fn hermes(&self, id: &str) -> RunRecord {
            let name = format!("hermes-{id}");
            let workspace = self.root.join(".worktrees").join(&name);
            let path = workspace.to_str().unwrap();
            let branch = format!("hermes/{name}");
            git(
                &self.root,
                &["worktree", "add", "--quiet", "-b", &branch, path],
            );
            let session = RuntimeSession {
                provider: "hermes".into(),
                session_id: name,
                source: "discovered".into(),
                started_at: Utc::now(),
                ended_at: None,
                end_reason: None,
                model: None,
            };
            let found =
                registration::discovered(&self.state, &self.root, &workspace, session, None);
            self.state.load_run(&found.unwrap().unwrap()).unwrap()
        }

        /// Work a wrapped agent runs in a new worktree: `dispatch attach --
        /// <agent>`, whose wrapper is this process.
        fn wrapped(&self, name: &str, agent: &str) -> RunRecord {
            let workspace = self.root.with_file_name(name);
            let path = workspace.to_str().unwrap();
            git(
                &self.root,
                &["worktree", "add", "--quiet", "-b", name, path],
            );
            attach::create(
                &self.state,
                AttachRequest {
                    workspace,
                    root: Some(self.root.clone()),
                    task: None,
                    agent: Some(agent.into()),
                    pid: None,
                    command: Some(vec![agent.into(), "chat".into()]),
                    allow_unsafe_local: true,
                    auto_apply: false,
                    runtime: None,
                },
            )
            .unwrap()
        }

        fn runs(&self) -> Vec<RunRecord> {
            self.state.load_source_runs(&self.root).unwrap()
        }

        /// The owner's step, which must not fail.
        fn step(&mut self) {
            let runs = self.runs();
            let errors = step(&self.state, &self.root, &runs, &mut self.landings);
            assert!(errors.is_empty(), "{errors:?}");
        }

        /// `run` lands with `lib` as its `src/lib.rs`, as an accept records
        /// it: applied to the root, its source recorded, and its
        /// `interaction.landed` with `status`, naming each counterpart's
        /// interactions, seen from `run`'s side.
        async fn land(
            &self,
            run: &RunRecord,
            lib: &str,
            status: &str,
            counterparts: &[(&RunRecord, Vec<Interaction>)],
        ) {
            fs::write(workspace(run).join("src/lib.rs"), lib).unwrap();
            let finished = attach::finish_quietly(&self.state, &run.id, true).await;
            let mut run = finished.unwrap();
            fs::write(self.root.join("src/lib.rs"), lib).unwrap();
            s0::record(&self.state, &run).unwrap();
            let counterparts: Vec<serde_json::Value> = counterparts
                .iter()
                .map(|(other, interactions)| {
                    serde_json::json!({
                        "run_id": other.id, "s0": null, "delta_sha256": "", "analysis": "analyzed",
                        "unresolved": 0, "prior": null, "interactions": interactions,
                    })
                })
                .collect();
            let landing = serde_json::json!({
                "version": 1, "status": status, "detail": null,
                "landed": {"run_id": run.id, "s0": null, "delta_sha256": "", "applied_by": "human"},
                "world_before": null, "world_after": null, "projection_computed_at": null,
                "counterparts": counterparts,
            });
            let db = Database::open(self.state.db_path()).unwrap();
            inbox::record(&self.state, &db, &mut run, LANDED, Utc::now(), landing).unwrap();
        }

        fn events(&self, run: &RunRecord, kind: &str) -> Vec<serde_json::Value> {
            Database::open(self.state.db_path())
                .unwrap()
                .events_for_run(&run.id)
                .unwrap()
                .into_iter()
                .filter(|event| event.event_type == kind)
                .map(|event| event.payload)
                .collect()
        }

        /// What a `Stop` hook in `run`'s workspace is given now.
        fn hook(&self, run: &RunRecord) -> Option<String> {
            claim(&self.state, workspace(run), "stop")
        }
    }

    fn workspace(run: &RunRecord) -> &Path {
        &run.attachment.as_ref().unwrap().workspace
    }

    fn symbol(name: &str) -> Target {
        Target::Symbol {
            path: "src/lib.rs".into(),
            name: name.into(),
        }
    }

    fn interaction(
        rule: Rule,
        writer: Option<Side>,
        target: Target,
        change: Option<Change>,
    ) -> Interaction {
        Interaction {
            rule,
            writer,
            target,
            change,
            lines: None,
        }
    }

    /// The landed run (`Side::A`) changed the signature of `name`, which its
    /// counterpart uses.
    fn breaks_use_of(name: &str) -> Interaction {
        let change = Some(Change::Signature);
        interaction(Rule::Uses, Some(Side::A), symbol(name), change)
    }

    #[test]
    fn only_a_landed_change_to_a_declaration_the_work_relies_on_asks() {
        let file = || Target::File {
            path: "src/lib.rs".into(),
        };
        let (a, b) = (Some(Side::A), Some(Side::B));
        let cases = [
            (breaks_use_of("f"), true),
            (
                interaction(Rule::Uses, a, symbol("f"), Some(Change::Removes)),
                true,
            ),
            (
                interaction(Rule::SameDeclaration, None, symbol("f"), None),
                true,
            ),
            (
                interaction(Rule::SameDeclaration, None, symbol("f"), Some(Change::Adds)),
                true,
            ),
            // The other way round: the Work changes what the landed run uses.
            (
                interaction(Rule::Uses, b, symbol("f"), Some(Change::Signature)),
                false,
            ),
            (
                interaction(Rule::Uses, b, symbol("f"), Some(Change::Removes)),
                false,
            ),
            // A body alone, or an addition, breaks nothing the Work uses.
            (
                interaction(Rule::Uses, a, symbol("f"), Some(Change::Changes)),
                false,
            ),
            (
                interaction(Rule::Uses, a, symbol("f"), Some(Change::Adds)),
                false,
            ),
            // Whole files and text never ask.
            (
                interaction(Rule::Uses, a, file(), Some(Change::Signature)),
                false,
            ),
            (
                interaction(Rule::SameDeclaration, None, file(), None),
                false,
            ),
            (
                interaction(Rule::File, a, file(), Some(Change::Changes)),
                false,
            ),
            (
                interaction(Rule::File, b, file(), Some(Change::Changes)),
                false,
            ),
            (interaction(Rule::File, None, file(), None), false),
            (
                Interaction {
                    lines: Some((1, 3)),
                    ..interaction(Rule::TextualOverlap, None, file(), None)
                },
                false,
            ),
        ];
        for (interaction, asks) in cases {
            assert_eq!(breaks(&interaction), asks, "{interaction:?}");
        }
    }

    fn landed(
        run_id: &str,
        at: i64,
        targets: Vec<(Interaction, Option<(String, String)>)>,
    ) -> Landed {
        Landed {
            run_id: run_id.into(),
            runtime: "Claude Code".into(),
            commit: format!("{at:0>8}{}", "c".repeat(32)),
            at: DateTime::from_timestamp(1_700_000_000 + at, 0).unwrap(),
            targets,
        }
    }

    #[test]
    fn an_ask_names_the_landed_work_each_target_and_the_recorded_source() {
        let old_new = (
            "pub fn f() -> i32".into(),
            "pub fn f(strict: bool) -> i32".into(),
        );
        let one = landed("01AAAAAAAAAA", 1, vec![(breaks_use_of("f"), Some(old_new))]);
        // The command names the whole commit, never the ref, which Claude
        // Code refuses to run in a worktree.
        let commit = format!("00000001{}", "c".repeat(32));
        assert_eq!(
            text(std::slice::from_ref(&one)),
            format!(
                "Dispatch: Work 01AAAAAA (Claude Code) landed a change this Work relies on.\n\
                 - f (src/lib.rs), which this Work uses: 01AAAAAA changed its signature, \
                 pub fn f() -> i32 → pub fn f(strict: bool) -> i32\n\
                 These changes are in refs/dispatch/source (commit 00000001).\n\
                 To catch up: commit your work, run git merge {commit}, update what uses them, \
                 and run the project's checks. Dispatch asks once per landing."
            )
        );
        let removes = Some(Change::Removes);
        let two = Landed {
            runtime: "Hermes".into(),
            ..landed(
                "01BBBBBBBBBB",
                2,
                vec![
                    (
                        interaction(Rule::Uses, Some(Side::A), symbol("g"), removes),
                        None,
                    ),
                    (
                        interaction(Rule::SameDeclaration, None, symbol("h"), None),
                        None,
                    ),
                    (breaks_use_of("k"), None),
                ],
            )
        };
        let both = text(&[one, two]);
        let lines: Vec<&str> = both.lines().collect();
        assert_eq!(
            lines[..5],
            [
                "Dispatch: Work 01AAAAAA (Claude Code) and Work 01BBBBBB (Hermes) landed changes \
                 this Work relies on.",
                "- f (src/lib.rs), which this Work uses: 01AAAAAA changed its signature, \
                 pub fn f() -> i32 → pub fn f(strict: bool) -> i32",
                "- g (src/lib.rs), which this Work uses: 01BBBBBB removed it",
                "- h (src/lib.rs), which this Work also changes: 01BBBBBB changed it",
                "- k (src/lib.rs), which this Work uses: 01BBBBBB changed its signature",
            ]
        );
        // The newest landing's source holds the earlier ones too.
        assert_eq!(
            lines[5],
            "These changes are in refs/dispatch/source (commit 00000002)."
        );
        let merge = format!("run git merge 00000002{}, ", "c".repeat(32));
        assert!(lines[6].contains(&merge), "{}", lines[6]);
    }

    #[test]
    fn an_ask_stays_within_eight_lines_and_never_insists() {
        let long = "x".repeat(1_000);
        let targets = (0..9)
            .map(|i| {
                let old_new = (long.clone(), format!("{long}\n{long}"));
                (breaks_use_of(&format!("f{i}")), Some(old_new))
            })
            .collect();
        let text = text(&[landed("01AAAAAAAAAA", 1, targets)]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 8, "{text}");
        assert_eq!(lines[5], "- and 5 more.");
        assert!(lines.iter().all(|line| line.chars().count() <= 600));
        assert!(lines[0].starts_with("Dispatch: "));
        assert!(!text.to_lowercase().contains("must"), "{text}");
    }

    #[tokio::test]
    async fn two_landings_before_a_hook_takes_the_ask_are_one_ask() {
        let mut p = Project::new();
        let (a, b) = (p.work("a", "claude"), p.work("b", "codex"));
        let c = p.work("c", "claude");
        p.step();
        assert!(followed(&p.state, workspace(&c)), "a record before any ask");
        assert!(
            !followed(&p.state, workspace(&b)),
            "Codex has no hook to ask through"
        );
        let changed = LIB.replace("total(n: i32)", "total(n: i32, by: i32)");
        let breaks = [(&c, vec![breaks_use_of("total")])];
        p.land(&a, &changed, "observed", &breaks).await;
        p.step();
        let changed = changed.replace("scale(n: i32)", "scale(n: i64)");
        let breaks = [(&c, vec![breaks_use_of("scale")])];
        p.land(&b, &changed, "observed", &breaks).await;
        p.step();

        let written = p.events(&c, WRITTEN);
        assert_eq!(written.len(), 2, "{written:?}");
        assert_eq!(written[0]["landings"], serde_json::json!([a.id]));
        assert_eq!(written[1]["landings"], serde_json::json!([b.id]));
        assert_eq!(
            written[1]["targets"],
            serde_json::json!([{"kind": "symbol", "path": "src/lib.rs", "name": "scale"}])
        );
        let text = p.hook(&c).expect("one ask");
        assert_eq!(text, written[1]["text"]);
        assert!(text.contains(" (Claude Code) and Work "), "{text}");
        assert!(text.contains(" (Codex) landed changes "), "{text}");
        let signature = "pub fn total(n: i32) -> i32 → pub fn total(n: i32, by: i32) -> i32";
        assert!(
            text.contains("- total (src/lib.rs), which this Work uses: ")
                && text.contains(signature)
                && text.contains("- scale (src/lib.rs), which this Work uses: "),
            "{text}"
        );
        assert_eq!(p.hook(&c), None);
        p.step();
        let delivered = p.events(&c, DELIVERED);
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0]["via"], "stop");
        assert_eq!(delivered[0]["landings"], serde_json::json!([a.id, b.id]));
    }

    #[tokio::test]
    async fn an_ask_reaches_the_agent_once_however_many_hooks_race_and_never_again() {
        let mut p = Project::new();
        let (a, b) = (p.work("a", "claude"), p.work("b", "claude"));
        let changed = LIB.replace("total(n: i32)", "total(n: i32, by: i32)");
        let breaks = [(&b, vec![breaks_use_of("total")])];
        p.land(&a, &changed, "observed", &breaks).await;
        p.step();
        let claims: Vec<Option<String>> = std::thread::scope(|scope| {
            let hooks: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| claim(&p.state, workspace(&b), "wake")))
                .collect();
            hooks.into_iter().map(|hook| hook.join().unwrap()).collect()
        });
        assert_eq!(claims.iter().flatten().count(), 1, "{claims:?}");
        for _ in 0..3 {
            p.step();
        }
        assert_eq!(p.hook(&b), None, "asked once per landing");
        assert_eq!(p.events(&b, WRITTEN).len(), 1);
        assert_eq!(p.events(&b, DELIVERED)[0]["via"], "wake");

        // An owner that starts again knows what was asked.
        p.landings = Landings::default();
        p.step();
        assert_eq!(p.hook(&b), None);
        assert_eq!(p.events(&b, WRITTEN).len(), 1);
    }

    #[tokio::test]
    async fn only_live_hooked_work_an_observed_landing_breaks_is_asked() {
        let mut p = Project::new();
        let a = p.work("a", "claude");
        let reader = p.work("reader", "claude");
        let writer = p.work("writer", "claude");
        let hermes = p.hermes("0a1b2c3d");
        let ended = p.work("ended", "claude");
        attach::record_session_end(
            &p.state,
            &ended.id,
            "claude",
            "ended",
            "prompt_input_exit",
            Utc::now(),
            std::time::Duration::ZERO,
        )
        .unwrap();
        // Hermes's session ended and its worktree kept, or the worktree
        // removed with the session.
        let (shut, removed) = (p.hermes("1b2c3d4e"), p.hermes("2c3d4e5f"));
        attach::record_session_end(
            &p.state,
            &shut.id,
            "hermes",
            "hermes-1b2c3d4e",
            "shutdown",
            Utc::now(),
            std::time::Duration::ZERO,
        )
        .unwrap();
        fs::remove_dir_all(workspace(&removed)).unwrap();
        let changed = LIB.replace("total(n: i32)", "total(n: i32, by: i32)");
        let theirs = Some(Change::Signature);
        let reverse = interaction(Rule::Uses, Some(Side::B), symbol("total"), theirs);
        let file = Target::File {
            path: "src/lib.rs".into(),
        };
        let whole = interaction(Rule::File, Some(Side::A), file, Some(Change::Changes));
        let breaks = [
            (&reader, vec![breaks_use_of("total")]),
            (&writer, vec![reverse, whole]),
            (&hermes, vec![breaks_use_of("total")]),
            (&ended, vec![breaks_use_of("total")]),
            (&shut, vec![breaks_use_of("total")]),
            (&removed, vec![breaks_use_of("total")]),
        ];
        p.land(&a, &changed, "observed", &breaks).await;
        p.step();
        assert!(p.hook(&reader).is_some());
        assert!(followed(&p.state, workspace(&hermes)));
        let ask = claim(&p.state, workspace(&hermes), "verify").expect("Hermes is asked");
        assert!(
            ask.contains("- total (src/lib.rs), which this Work uses: "),
            "{ask}"
        );
        for run in [&writer, &ended, &shut, &removed] {
            assert_eq!(p.hook(run), None, "{}", workspace(run).display());
            assert!(p.events(run, WRITTEN).is_empty());
        }
        p.step();
        let delivered = p.events(&hermes, DELIVERED);
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0]["via"], "verify");
        assert_eq!(delivered[0]["landings"], serde_json::json!([a.id]));

        // A landing the owner's view did not see as it was applied asks
        // nothing.
        let (other, late) = (p.work("other", "claude"), p.work("late", "claude"));
        let breaks = [(&other, vec![breaks_use_of("scale")])];
        p.land(&late, &changed.replace("n * 2", "n * 3"), "stale", &breaks)
            .await;
        p.step();
        assert_eq!(p.hook(&other), None);
    }

    #[tokio::test]
    async fn an_ask_expires_when_s0_already_moved_past_its_landings() {
        let mut p = Project::new();
        let (a, b) = (p.work("a", "claude"), p.work("b", "claude"));
        let changed = LIB.replace("total(n: i32)", "total(n: i32, by: i32)");
        let breaks = [(&b, vec![breaks_use_of("total")])];
        p.land(&a, &changed, "observed", &breaks).await;
        p.step();
        assert!(pending(&p.state, workspace(&b)).exists());

        // B merges the recorded source by itself before any hook took the ask.
        let commit = s0::recorded(&p.state, &p.root).unwrap()[0].commit.clone();
        git(workspace(&b), &["merge", "--quiet", "--no-edit", &commit]);
        let mut runs = p.runs();
        let (moved, errors) = s0::follow(&p.state, &p.root, &mut runs, &mut HashMap::new());
        assert!(moved && errors.is_empty(), "{errors:?}");
        p.step();
        assert_eq!(p.hook(&b), None);
        let expired = p.events(&b, EXPIRED);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0]["why"], "s0_moved");
        assert_eq!(expired[0]["landings"], serde_json::json!([a.id]));
        assert!(followed(&p.state, workspace(&b)), "still in progress");
    }

    #[tokio::test]
    async fn work_whose_wrapper_keeps_its_s0_is_never_asked() {
        let mut p = Project::new();
        let (a, hooked) = (p.work("a", "claude"), p.work("hooked", "claude"));
        // `dispatch attach -- claude`: Claude Code's SessionStart joins the
        // wrapped Work, whose wrapper is this process.
        let wrapped = p.wrapped("wrapped", "claude");
        let session = RuntimeSession {
            provider: "claude".into(),
            session_id: "wrapped".into(),
            source: "startup".into(),
            started_at: Utc::now(),
            ended_at: None,
            end_reason: None,
            model: None,
        };
        let wait = std::time::Duration::ZERO;
        assert!(attach::record_session_start(&p.state, &wrapped.id, session, wait).unwrap());
        // No Hermes hook gives wrapped Work a session at all.
        let hermes = p.wrapped("hermes", "hermes");
        let changed = LIB.replace("total(n: i32)", "total(n: i32, by: i32)");
        let breaks = [
            (&hooked, vec![breaks_use_of("total")]),
            (&wrapped, vec![breaks_use_of("total")]),
            (&hermes, vec![breaks_use_of("total")]),
        ];
        p.land(&a, &changed, "observed", &breaks).await;
        p.step();
        assert!(p.hook(&hooked).is_some());
        // Its merge would fold the landing into its Δ, not move its S0.
        for run in [&wrapped, &hermes] {
            assert!(!followed(&p.state, workspace(run)), "no waiter waits there");
            assert_eq!(p.hook(run), None, "{}", workspace(run).display());
            assert!(p.events(run, WRITTEN).is_empty());
        }
    }

    #[tokio::test]
    async fn work_reads_as_asked_from_its_delivery_until_its_s0_follows() {
        let mut p = Project::new();
        let (a, b) = (p.work("a", "claude"), p.hermes("0a1b2c3d"));
        let changed = LIB.replace("total(n: i32)", "total(n: i32, by: i32)");
        let breaks = [(&b, vec![breaks_use_of("total")])];
        p.land(&a, &changed, "observed", &breaks).await;
        p.step();
        let b = p.state.load_run(&b.id).unwrap();
        assert!(asked(&p.state, &b).is_none(), "written, not delivered");
        claim(&p.state, workspace(&b), "turn").unwrap();
        p.step();
        let delivered = Database::open(p.state.db_path())
            .unwrap()
            .events_for_run(&b.id)
            .unwrap()
            .into_iter()
            .find(|event| event.event_type == DELIVERED)
            .unwrap();
        let recorded = s0::recorded(&p.state, &p.root).unwrap()[0].commit.clone();
        let b = p.state.load_run(&b.id).unwrap();
        let found = asked(&p.state, &b).expect("asked");
        assert_eq!(
            found.line(),
            format!(
                "asked to catch up with {} at {}",
                &a.id[..8],
                delivered.timestamp.format("%Y-%m-%d %H:%M UTC")
            )
        );
        assert_eq!(found.commit, recorded);
        // Its REFRESH step waits for its S0, which Dispatch follows.
        let step = |asked| super::super::next_step(&b, crate::Decision::Refresh, None, asked);
        let waits = format!(
            "its agent was asked to merge {recorded}; once its S0 moves, Dispatch checks it again"
        );
        assert_eq!(step(Some(&found)), (waits, None));
        assert!(step(None).0.starts_with("run your agent again"));
        // Finished, it has nothing left to follow.
        let mut finished = b.clone();
        finished.outcome.lifecycle = LifecycleState::Finished;
        assert!(asked(&p.state, &finished).is_none());

        git(workspace(&b), &["merge", "--quiet", "--no-edit", &recorded]);
        let mut runs = p.runs();
        let (moved, errors) = s0::follow(&p.state, &p.root, &mut runs, &mut HashMap::new());
        assert!(moved && errors.is_empty(), "{errors:?}");
        let b = p.state.load_run(&b.id).unwrap();
        assert!(asked(&p.state, &b).is_none(), "its S0 followed");
    }

    #[tokio::test]
    async fn an_ask_expires_and_its_record_goes_when_the_work_leaves_working() {
        let mut p = Project::new();
        let (a, b) = (p.work("a", "claude"), p.work("b", "claude"));
        let changed = LIB.replace("total(n: i32)", "total(n: i32, by: i32)");
        let breaks = [(&b, vec![breaks_use_of("total")])];
        p.land(&a, &changed, "observed", &breaks).await;
        p.step();
        attach::close(
            &p.state,
            attach::CloseRequest {
                run_id: b.id.clone(),
                reason: CloseReason::Unfinished,
                replacement: None,
                explanation: None,
            },
        )
        .unwrap();
        p.step();
        assert_eq!(p.hook(&b), None);
        let expired = p.events(&b, EXPIRED);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0]["why"], "left_working");
        assert!(!followed(&p.state, workspace(&b)));
        assert!(!followed(&p.state, workspace(&a)), "A landed");
    }

    #[tokio::test]
    async fn an_ask_left_aside_by_an_interrupted_merge_is_put_back() {
        let mut p = Project::new();
        let (a, b) = (p.work("a", "claude"), p.work("b", "claude"));
        let changed = LIB.replace("total(n: i32)", "total(n: i32, by: i32)");
        let breaks = [(&b, vec![breaks_use_of("total")])];
        p.land(&a, &changed, "observed", &breaks).await;
        p.step();
        let pending = pending(&p.state, workspace(&b));
        fs::rename(&pending, pending.with_extension("merging")).unwrap();
        assert_eq!(p.hook(&b), None);
        p.step();
        assert!(p.hook(&b).is_some());
        assert!(p.events(&b, EXPIRED).is_empty());
    }
}
