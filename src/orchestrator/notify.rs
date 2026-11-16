//! The owner's notes to live agents. Each tick, what `coherence::notes`
//! composes for Work in progress whose runtime has a hook that delivers
//! notes (Claude Code, Hermes) goes to that Work's inbox (`inbox::write`).
//! What each Work was sent is kept in `inbox/sent/<run id>.json` until the
//! Work finishes. So are the targets its notes named, so that the first
//! change its Δ makes there afterwards is recorded once, `note.touched`.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{inbox, persist_event, s0};
use crate::{
    EventRecord, LifecycleState, RunRecord,
    coherence::{
        facts,
        interactions::{Change, Projection, Target},
        measure,
        notes::{self, Sent},
        symbols,
    },
    db::Database,
    lock::OperationLock,
    state::{State, write_atomically},
};

const TOUCHED: &str = "note.touched";

/// The runtimes whose hooks deliver notes, as sessions name them.
const HOOKED: [&str; 2] = ["claude", "hermes"];

/// What one Work was sent, kept across ticks and owners.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
struct Kept {
    sent: Sent,
    /// Each item key a written note named, with that note, and what the
    /// Work's Δ did at its target then (`at`), until Δ does something else
    /// there.
    #[serde(default)]
    open: BTreeMap<String, (String, Option<String>)>,
}

/// What the owner remembers for this step between ticks.
#[derive(Default)]
pub(crate) struct Told {
    /// Each landed run's `interaction.landed`, read once, with the
    /// signatures it changed; `None` when it recorded none.
    landings: HashMap<String, Option<(measure::Landing, Signatures)>>,
    /// Each Work's Δ digest when its open targets were last looked at.
    looked: HashMap<String, String>,
}

pub(super) type Signatures = BTreeMap<Target, (String, String)>;

/// Leave each live Work in `runs` the note `compose` gives it at `now`, from
/// `projection` and the landings recorded at `root` since the oldest of them
/// began, and record what changed at the targets of notes already written.
/// Busy Work keeps what it was sent and is tried again on a later tick.
pub(crate) fn tell(
    state: &State,
    root: &Path,
    runs: &[RunRecord],
    projection: &Projection,
    told: &mut Told,
    now: DateTime<Utc>,
) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    forget(state, runs, &mut errors);
    let hooked: Vec<&RunRecord> = runs.iter().filter(|run| hooked(run)).collect();
    let Some(since) = hooked.iter().map(|run| run.created_at).min() else {
        return errors;
    };
    let runtimes: BTreeMap<String, String> = runs
        .iter()
        .filter_map(|run| Some((run.id.clone(), runtime(run)?.to_owned())))
        .collect();
    let recorded = match s0::recorded(state, root) {
        Ok(recorded) => recorded,
        Err(error) => {
            errors.push(error);
            Vec::new()
        }
    };
    let recorded: Vec<s0::Recorded> = recorded.into_iter().filter(|r| r.at >= since).collect();
    told.landings
        .retain(|run, _| recorded.iter().any(|r| r.landed_run == *run));
    for source in &recorded {
        if !told.landings.contains_key(&source.landed_run) {
            match landing(state, runs, &source.landed_run) {
                Ok(found) => {
                    told.landings.insert(source.landed_run.clone(), found);
                }
                Err(error) => errors.push(error),
            }
        }
    }
    for &run in &hooked {
        let landings: Vec<notes::Landing> = recorded
            .iter()
            .filter_map(|source| {
                let (landing, signatures) = told.landings.get(&source.landed_run)?.as_ref()?;
                let counterpart = landing
                    .counterparts
                    .iter()
                    .find(|c| c.identity.run_id == run.id)?;
                Some(notes::Landing {
                    run_id: source.landed_run.clone(),
                    runtime: runtimes
                        .get(&source.landed_run)
                        .map_or("unknown runtime", String::as_str)
                        .to_owned(),
                    commit: source.commit.clone(),
                    interactions: counterpart.interactions.clone(),
                    signatures: signatures.clone(),
                })
            })
            .collect();
        let kept = match one(state, run, projection, &runtimes, &landings, now) {
            Ok(Some(kept)) => kept,
            Ok(None) => continue,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        // A target can only have changed with the Δ.
        let digest = projection.participants.iter().find(|p| p.run_id == run.id);
        if let Some(digest) = digest.map(|p| &p.delta_sha256)
            && told.looked.get(&run.id) != Some(digest)
        {
            match touched(state, run, kept) {
                Ok(true) => {
                    told.looked.insert(run.id.clone(), digest.clone());
                }
                Ok(false) => {}
                Err(error) => errors.push(error),
            }
        }
    }
    told.looked
        .retain(|id, _| hooked.iter().any(|run| run.id == *id));
    errors
}

/// Write `run`'s note, if `compose` gives one, and keep what was sent once
/// the inbox has it. What is kept now; `None` while the Work is busy.
fn one(
    state: &State,
    run: &RunRecord,
    projection: &Projection,
    runtimes: &BTreeMap<String, String>,
    landings: &[notes::Landing],
    now: DateTime<Utc>,
) -> Result<Option<Kept>> {
    let mut kept = load(state, &run.id)?;
    let (notes, sent) = notes::compose(&run.id, projection, runtimes, landings, &kept.sent, now);
    let written: Vec<(String, String)> = notes
        .iter()
        .map(|note| (note.id.clone(), note.text.clone()))
        .collect();
    if !written.is_empty() && !inbox::write(state, &run.id, &written)? {
        return Ok(None);
    }
    let unchanged = sent == kept.sent;
    kept.sent = sent;
    for note in &notes {
        for key in &note.keys {
            if let Some(at) = target(key).and_then(|target| at(run, &target)) {
                kept.open.insert(key.clone(), (note.id.clone(), at));
            }
        }
    }
    if !unchanged {
        save(state, &run.id, &kept)?;
    }
    Ok(Some(kept))
}

/// Record `note.touched` for each open target where `run`'s Δ now does
/// something else than when its note was written, and close it. `false`
/// while the Work is busy. A target that does not parse is looked at again
/// when Δ next changes.
fn touched(state: &State, run: &RunRecord, mut kept: Kept) -> Result<bool> {
    let mut changed = Vec::new();
    for (key, (note, then)) in &kept.open {
        let now = target(key).and_then(|target| at(run, &target));
        if now.is_some_and(|now| now != *then) {
            changed.push((key.clone(), note.clone()));
        }
    }
    if changed.is_empty() {
        return Ok(true);
    }
    let Ok(_lock) = OperationLock::acquire(
        &state.run_dir(&run.id).join(".operation.lock"),
        "run has a foreground owner",
    ) else {
        return Ok(false);
    };
    let mut record = state.load_run(&run.id)?;
    let db = Database::open(state.db_path())?;
    for (key, note) in changed {
        let event = EventRecord {
            run_id: run.id.clone(),
            event_type: TOUCHED.into(),
            timestamp: Utc::now(),
            payload: serde_json::json!({"note": note, "key": key}),
            ..EventRecord::default()
        };
        persist_event(state, &db, event, &mut record)?;
        kept.open.remove(&key);
    }
    save(state, &run.id, &kept)?;
    Ok(true)
}

/// What `run`'s Δ does at `target` now: `None` inside when the Work's text
/// there is its S0's, else a digest of it. `None` while a file there does
/// not parse.
fn at(run: &RunRecord, target: &Target) -> Option<Option<String>> {
    let path = target.path();
    let text = |dir: &Path| -> Option<Option<String>> {
        let Ok(bytes) = fs::read(dir.join(path)) else {
            return Some(None);
        };
        let Target::Symbol { name, .. } = target else {
            return Some(Some(hex::encode(Sha256::digest(&bytes))));
        };
        let found = symbols::extract(symbols::lang_for_path(path)?, &bytes).ok()?;
        (!found.has_error).then(|| {
            let decl = found.decls.into_iter().find(|decl| decl.name == *name);
            decl.map(|decl| decl.full_fp)
        })
    };
    let workspace = &run.attachment.as_ref()?.workspace;
    let (s0, now) = (text(&run.baseline_path)?, text(workspace)?);
    Some((s0 != now).then(|| now.unwrap_or_default()))
}

/// The target an item key names: `[kind, other, path, name, change]`.
fn target(key: &str) -> Option<Target> {
    let (_, _, path, name, _): (String, String, String, Option<String>, serde_json::Value) =
        serde_json::from_str(key).ok()?;
    Some(match name {
        Some(name) => Target::Symbol { path, name },
        None => Target::File { path },
    })
}

/// `landed`'s `interaction.landed`, and the signatures it changed, as its S0
/// and Δ show them.
pub(super) fn landing(
    state: &State,
    runs: &[RunRecord],
    landed: &str,
) -> Result<Option<(measure::Landing, Signatures)>> {
    let events = Database::open_read_only(state.db_path())?.events_for_run(landed)?;
    let Some(event) = events.iter().find(|e| e.event_type == measure::LANDED) else {
        return Ok(None);
    };
    let landing: measure::Landing = serde_json::from_value(event.payload.clone())?;
    let signatures = runs
        .iter()
        .find(|run| run.id == landed)
        .map(|run| signatures(run, &landing))
        .unwrap_or_default();
    Ok(Some((landing, signatures)))
}

/// Old and new signature text of each declaration whose signature `run`
/// changed for one of `landing`'s counterparts: its S0 against S0 with its Δ.
fn signatures(run: &RunRecord, landing: &measure::Landing) -> Signatures {
    let Some(patch) = run
        .candidates
        .first()
        .and_then(|c| fs::read(&c.diff_path).ok())
    else {
        return Signatures::new();
    };
    let files = facts::parse_patch(&patch);
    let changed = landing
        .counterparts
        .iter()
        .flat_map(|c| &c.interactions)
        .filter(|i| i.change == Some(Change::Signature));
    changed
        .filter_map(|interaction| {
            let Target::Symbol { path, name } = &interaction.target else {
                return None;
            };
            let lang = symbols::lang_for_path(path)?;
            let display = |bytes: &[u8]| {
                let decls = symbols::extract(lang, bytes).ok()?.decls;
                decls
                    .into_iter()
                    .find(|d| d.name == *name)
                    .map(|d| d.display)
            };
            let old = fs::read(run.baseline_path.join(path)).ok()?;
            let new = files.iter().find(|f| f.path == *path)?.post_image(&old)?;
            Some((interaction.target.clone(), (display(&old)?, display(&new)?)))
        })
        .collect()
}

/// Work in progress in a workspace where a runtime that delivers notes
/// reported a session.
fn hooked(run: &RunRecord) -> bool {
    run.outcome.lifecycle == LifecycleState::Working
        && run.outcome.closed.is_none()
        && run.attachment.as_ref().is_some_and(|attachment| {
            attachment.workspace_removed.is_none()
                && attachment
                    .sessions
                    .iter()
                    .any(|session| HOOKED.contains(&session.provider.as_str()))
        })
}

/// `run`'s runtime as the person knows it, from what the run recorded.
pub(super) fn runtime(run: &RunRecord) -> Option<&str> {
    let id = match &run.attachment {
        Some(attachment) => attachment.agent.as_deref()?,
        None => &run.candidates.first()?.harness_id,
    };
    Some(super::harness_name(id))
}

fn path(state: &State, run_id: &str) -> PathBuf {
    state.root.join("inbox/sent").join(format!("{run_id}.json"))
}

fn load(state: &State, run_id: &str) -> Result<Kept> {
    match fs::read(path(state, run_id)) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Kept::default()),
        Err(error) => Err(error.into()),
    }
}

fn save(state: &State, run_id: &str, kept: &Kept) -> Result<()> {
    write_atomically(&path(state, run_id), &serde_json::to_vec(kept)?)
}

/// Remove what was kept for Work in `runs` that finished: it is told
/// nothing more.
fn forget(state: &State, runs: &[RunRecord], errors: &mut Vec<anyhow::Error>) {
    // A run never leaves `runs`: nothing removes its metadata, and its root is fixed.
    let Ok(entries) = fs::read_dir(state.root.join("inbox/sent")) else {
        return;
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
            continue;
        };
        let left = runs.iter().any(|run| {
            run.id == id
                && (run.outcome.lifecycle == LifecycleState::Finished
                    || run.outcome.closed.is_some())
        });
        if left && let Err(error) = fs::remove_file(entry.path()) {
            errors.push(error.into());
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::{
        RuntimeSession,
        coherence::interactions::{Analysis, Edge, Interaction, Participant, Rule, Side},
        orchestrator::attach::{self, AttachRequest, RuntimeStart},
        source,
    };

    const LIB: &str = "pub fn f() -> i32 {\n    1\n}\n\npub fn g() -> i32 {\n    2\n}\n";

    struct Project {
        _temp: tempfile::TempDir,
        root: PathBuf,
        state: State,
    }

    fn git(dir: &Path, args: &[&str]) {
        let mut git = source::git_command(dir);
        git.args(["-c", "user.name=T", "-c", "user.email=t@example.invalid"]);
        assert!(git.args(args).status().unwrap().success(), "git {args:?}");
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

        fn runs(&self) -> Vec<RunRecord> {
            self.state.load_source_runs(&self.root).unwrap()
        }

        /// The owner's step at `now`, which must not fail.
        fn tell(&self, view: &Projection, told: &mut Told, now: DateTime<Utc>) {
            let errors = tell(&self.state, &self.root, &self.runs(), view, told, now);
            assert!(errors.is_empty(), "{errors:?}");
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

        /// What a hook in `run`'s workspace is given now.
        fn hook(&self, run: &RunRecord) -> Option<String> {
            let workspace = &run.attachment.as_ref().unwrap().workspace;
            inbox::claim(&self.state, workspace, "PostToolUse")
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

    /// `a` and `b` both change `name`.
    fn both_change(name: &str) -> Interaction {
        Interaction {
            rule: Rule::SameDeclaration,
            writer: Some(Side::B),
            target: symbol(name),
            change: None,
            lines: None,
        }
    }

    /// The view of `a` and `b` interacting by `interactions`, with `a`'s Δ
    /// digest `digest`.
    fn view(
        a: &RunRecord,
        b: &RunRecord,
        interactions: Vec<Interaction>,
        digest: &str,
    ) -> Projection {
        Projection {
            version: 1,
            computed_at: Utc::now(),
            participants: [(a, digest), (b, "b")]
                .map(|(run, digest)| Participant {
                    run_id: run.id.clone(),
                    delta_sha256: digest.into(),
                    analysis: Analysis::Analyzed,
                    unresolved: 0,
                })
                .into(),
            edges: vec![Edge {
                a: a.id.clone(),
                b: b.id.clone(),
                interactions,
            }],
        }
    }

    #[test]
    fn a_work_hears_at_most_once_per_ten_minutes_through_its_hook() {
        let p = Project::new();
        let (a, b) = (p.work("a", "claude"), p.work("b", "hermes"));
        let mut told = Told::default();
        let t0 = Utc::now();
        p.tell(&view(&a, &b, vec![both_change("f")], "a"), &mut told, t0);
        let heard = p.hook(&a).unwrap();
        assert!(
            heard.contains("(Hermes): both change f (src/lib.rs)"),
            "{heard}"
        );
        assert!(p.hook(&b).unwrap().contains("(Claude Code): both change f"));
        assert_eq!(p.events(&a, inbox::WRITTEN).len(), 1);

        let more = view(&a, &b, vec![both_change("f"), both_change("g")], "a");
        for minutes in [0, 1, 9] {
            p.tell(&more, &mut told, t0 + Duration::minutes(minutes));
        }
        assert_eq!(p.hook(&a), None, "nothing within ten minutes");
        assert_eq!(p.events(&a, inbox::WRITTEN).len(), 1);
        p.tell(&more, &mut told, t0 + Duration::minutes(10));
        let heard = p.hook(&a).unwrap();
        assert!(heard.contains("both change g") && !heard.contains("both change f"));
        assert_eq!(p.events(&a, inbox::WRITTEN).len(), 2);
    }

    #[test]
    fn busy_work_keeps_what_it_was_sent_and_a_later_tick_sends() {
        let p = Project::new();
        let (a, b) = (p.work("a", "claude"), p.work("b", "claude"));
        let view = view(&a, &b, vec![both_change("f")], "a");
        let lock = OperationLock::acquire(
            &p.state.run_dir(&a.id).join(".operation.lock"),
            "test holds the run",
        )
        .unwrap();
        let mut told = Told::default();
        let t0 = Utc::now();
        p.tell(&view, &mut told, t0);
        assert!(p.events(&a, inbox::WRITTEN).is_empty());
        assert!(
            !path(&p.state, &a.id).exists(),
            "what was sent stays as it was"
        );
        assert!(p.hook(&b).is_some(), "other Work is not held back");
        drop(lock);
        p.tell(&view, &mut told, t0 + Duration::seconds(5));
        assert!(p.hook(&a).unwrap().contains("both change f"));
        assert_eq!(
            load(&p.state, &a.id).unwrap().sent.last_note_at,
            Some(t0 + Duration::seconds(5))
        );
    }

    #[tokio::test]
    async fn what_was_sent_is_gone_when_the_work_leaves() {
        let p = Project::new();
        let (a, b) = (p.work("a", "claude"), p.work("b", "claude"));
        let mut told = Told::default();
        p.tell(
            &view(&a, &b, vec![both_change("f")], "a"),
            &mut told,
            Utc::now(),
        );
        assert!(path(&p.state, &a.id).exists() && path(&p.state, &b.id).exists());
        fs::write(workspace(&a).join("src/lib.rs"), LIB.replace("1", "10")).unwrap();
        attach::finish(&p.state, &a.id, true).await.unwrap();
        p.tell(
            &view(&a, &b, vec![both_change("f")], "a"),
            &mut told,
            Utc::now(),
        );
        assert!(!path(&p.state, &a.id).exists());
        assert!(
            path(&p.state, &b.id).exists(),
            "Work still in progress keeps it"
        );
    }

    #[test]
    fn work_whose_runtime_has_no_hook_hears_nothing() {
        let p = Project::new();
        let (a, b) = (p.work("a", "codex"), p.work("b", "cursor"));
        let mut told = Told::default();
        p.tell(
            &view(&a, &b, vec![both_change("f")], "a"),
            &mut told,
            Utc::now(),
        );
        for run in [&a, &b] {
            assert_eq!(p.hook(run), None);
            assert!(p.events(run, inbox::WRITTEN).is_empty());
            assert!(!path(&p.state, &run.id).exists());
        }
    }

    #[test]
    fn the_first_change_at_a_target_after_its_note_is_recorded_once() {
        let p = Project::new();
        let (a, b) = (p.work("a", "claude"), p.work("b", "claude"));
        let file = workspace(&a).join("src/lib.rs");
        fs::write(&file, LIB.replace("    1\n", "    11\n")).unwrap();
        let mut told = Told::default();
        let t0 = Utc::now();
        p.tell(&view(&a, &b, vec![both_change("f")], "a1"), &mut told, t0);
        let note = p.events(&a, inbox::WRITTEN)[0]["note"].clone();

        // g changes, f does not; then the file stops parsing: neither is f
        // touched.
        fs::write(
            &file,
            LIB.replace("    1\n", "    11\n")
                .replace("    2\n", "    22\n"),
        )
        .unwrap();
        p.tell(&view(&a, &b, vec![both_change("f")], "a2"), &mut told, t0);
        fs::write(&file, "pub fn f() -> i32 {\n").unwrap();
        p.tell(&view(&a, &b, vec![both_change("f")], "a3"), &mut told, t0);
        assert!(p.events(&a, TOUCHED).is_empty());

        fs::write(&file, LIB.replace("    1\n", "    12\n")).unwrap();
        p.tell(&view(&a, &b, vec![both_change("f")], "a4"), &mut told, t0);
        fs::write(&file, LIB.replace("    1\n", "    13\n")).unwrap();
        p.tell(&view(&a, &b, vec![both_change("f")], "a5"), &mut told, t0);
        let touched = p.events(&a, TOUCHED);
        assert_eq!(touched.len(), 1, "{touched:?}");
        assert_eq!(touched[0]["note"], note);
        assert_eq!(
            target(touched[0]["key"].as_str().unwrap()),
            Some(symbol("f"))
        );
    }

    #[tokio::test]
    async fn a_landed_signature_is_read_from_its_s0_and_delta() {
        let p = Project::new();
        let a = p.work("a", "claude");
        let changed = LIB.replace("pub fn f() -> i32", "pub fn f(strict: bool) -> i32");
        fs::write(workspace(&a).join("src/lib.rs"), changed).unwrap();
        let a = attach::finish(&p.state, &a.id, true).await.unwrap();
        let landing: measure::Landing = serde_json::from_value(serde_json::json!({
            "version": 1, "status": "observed", "detail": null,
            "landed": {"run_id": a.id, "s0": null, "delta_sha256": "", "applied_by": "human"},
            "world_before": null, "world_after": null, "projection_computed_at": null,
            "counterparts": [{
                "run_id": "other", "s0": null, "delta_sha256": "", "analysis": "analyzed",
                "unresolved": 0, "prior": null,
                "interactions": [{
                    "rule": "uses", "writer": "a", "change": "signature", "lines": null,
                    "target": {"kind": "symbol", "path": "src/lib.rs", "name": "f"}
                }]
            }]
        }))
        .unwrap();
        assert_eq!(
            signatures(&a, &landing),
            BTreeMap::from([(
                symbol("f"),
                (
                    "pub fn f() -> i32".into(),
                    "pub fn f(strict: bool) -> i32".into()
                )
            )])
        );
    }
}
