//! S0 follows integration. After each landing the project's source is
//! recorded as a commit (`source::record_source`), named by
//! `refs/dispatch/source` and listed in `projects/<key>/sources.json`. When an
//! attached Work's workspace merges or rebases onto a source newer than its
//! S0 (recorded after it, or descending from it) or onto the root's HEAD
//! while nothing is uncommitted there, its S0 moves to it: the workspace's
//! HEAD descending from it is the proof. Without
//! that proof (a pull of something never recorded, a cherry-pick, a squash)
//! S0 stays, and the verdict is what it was. When the HEAD no longer has a
//! moved S0 (a merge reset or rebased away, or a checkout of a commit
//! without it), S0 moves back to the newest earlier one it has, or to where
//! the Work began, at the latest when Δ is frozen, so Δ never shows the
//! undone merge as a revert.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::persist_event;
use crate::{
    AttachmentRecord, EarlierBaseline, EventRecord, LifecycleState, RunMode, RunRecord, SourceKind,
    db::Database,
    lock::OperationLock,
    process::{IdentityState, identity_state},
    source,
    state::State,
};

/// The source after one landing: the commit `source::record_source` made of
/// it, when, and the Work that landed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Recorded {
    pub commit: String,
    pub at: DateTime<Utc>,
    pub landed_run: String,
}

fn sources_path(state: &State, root: &Path) -> PathBuf {
    let key = hex::encode(Sha256::digest(root.to_string_lossy().as_bytes()));
    state.root.join("projects").join(key).join("sources.json")
}

/// Every source recorded at `root`, oldest first.
pub(crate) fn recorded(state: &State, root: &Path) -> Result<Vec<Recorded>> {
    let path = sources_path(state, root);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("unreadable record of the source {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

/// Record the source of `run`, which just landed, under the source lock that
/// every landing holds, so no two records race.
pub(crate) fn record(state: &State, run: &RunRecord) -> Result<()> {
    record_together(state, &[run], &format!("dispatch: Work {} landed", run.id))
}

/// `record` for Work that landed together in one batch: one commit, and an
/// entry naming it for each Work.
pub(crate) fn record_together(state: &State, runs: &[&RunRecord], message: &str) -> Result<()> {
    let Some(first) = runs.first() else {
        return Ok(());
    };
    if !matches!(first.source_kind, SourceKind::Git | SourceKind::GitWorktree) {
        return Ok(());
    }
    let commit = source::record_source(&first.source_path, message)?;
    let mut sources = recorded(state, &first.source_path)?;
    let at = Utc::now();
    sources.extend(runs.iter().map(|run| Recorded {
        commit: commit.clone(),
        at,
        landed_run: run.id.clone(),
    }));
    crate::state::write_durably(
        &sources_path(state, &first.source_path),
        &serde_json::to_vec_pretty(&sources)?,
    )
}

/// Attached Git Work still in progress, unowned by a live wrapper (whose
/// watcher keeps the S0 it began with), and with a commit for S0.
pub(super) fn movable(run: &RunRecord) -> Option<&AttachmentRecord> {
    let attachment = run.attachment.as_ref()?;
    let wrapped = attachment
        .owner
        .as_ref()
        .is_some_and(|owner| identity_state(owner) == IdentityState::ExactLive);
    (run.mode == RunMode::Attached
        && run.outcome.lifecycle == LifecycleState::Working
        && run.outcome.closed.is_none()
        && matches!(run.source_kind, SourceKind::Git | SourceKind::GitWorktree)
        && run.s0_commit().is_some()
        && attachment.workspace_removed.is_none()
        && !wrapped)
        .then_some(attachment)
}

/// Move the S0 of each Work in `runs` whose workspace integrated a newer
/// source, replacing its record with the moved one, or back when its HEAD
/// no longer has it. `looked` holds each Work's workspace HEAD when it was
/// last looked at (`workspace_head`); only a HEAD that moved is looked at
/// again. Whether any S0 moved, and what failed.
pub(crate) fn follow(
    state: &State,
    root: &Path,
    runs: &mut [RunRecord],
    looked: &mut HashMap<String, (String, bool)>,
) -> (bool, Vec<anyhow::Error>) {
    let mut errors = Vec::new();
    let mut followed = HashSet::new();
    let mut heads = Vec::new();
    for (index, run) in runs.iter().enumerate() {
        let Some(attachment) = movable(run) else {
            continue;
        };
        followed.insert(run.id.clone());
        match workspace_head(attachment) {
            // Mid-merge, mid-rebase or mid-cherry-pick: no proof now, and
            // the HEAD it ends on is looked at then.
            Ok(None) => {}
            Ok(Some(head)) if looked.get(&run.id) == Some(&head) => {}
            Ok(Some(head)) => heads.push((index, head)),
            Err(error) => errors.push(error),
        }
    }
    looked.retain(|id, _| followed.contains(id));
    if heads.is_empty() {
        return (false, errors);
    }
    let (records, root_head) = match recorded(state, root).and_then(|records| {
        let mut command = source::git_command(root);
        command.args(["rev-parse", "--verify", "--quiet", "HEAD"]);
        let output = source::run_git(command, None, "failed to read the root's HEAD")?;
        let head = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        Ok((records, output.status.success().then_some(head)))
    }) {
        Ok(found) => found,
        Err(error) => {
            errors.push(error);
            return (false, errors);
        }
    };
    let mut clean = None;
    let mut moved = false;
    for (index, (head, forward)) in heads {
        let run = &mut runs[index];
        let newer = if forward {
            newer_source(root, run, &head, &records, root_head.as_deref(), &mut clean)
        } else {
            Ok(None)
        };
        let outcome = match newer.map(|newer| newer.or_else(|| earlier_s0(root, run, &head))) {
            Ok(Some(target)) => move_s0(state, root, &run.id, &head, target),
            Ok(None) => Ok(Move::Stayed),
            Err(error) => Err(error),
        };
        match outcome {
            Ok(Move::Busy) => continue,
            Ok(Move::Stayed) => {}
            // Looked at again, against the new S0.
            Ok(Move::Moved(now)) => {
                *run = *now;
                moved = true;
                continue;
            }
            Err(error) => {
                errors.push(error);
                continue;
            }
        }
        looked.insert(run.id.clone(), (head, forward));
    }
    (moved, errors)
}

/// The HEAD commit of `attachment`'s workspace, and whether it may prove a
/// move forward; `None` while a merge, rebase or cherry-pick is in progress.
/// A detached HEAD in a workspace that began on a branch is a checkout to
/// look at a commit, not the branch that integrated it, so it proves no
/// move forward. In a workspace that began detached, the detached HEAD is
/// where its agent commits; a look there moves S0 to what it checked out,
/// and back once it leaves. Any of these may prove a move back, which needs
/// less: it asks only that the HEAD lack S0, and it goes to an S0 the Work
/// had before, so all it can take out of Δ is a revert of what the HEAD lacks.
fn workspace_head(attachment: &AttachmentRecord) -> Result<Option<(String, bool)>> {
    let mut command = source::git_command(&attachment.workspace);
    command.args([
        "rev-parse",
        "--absolute-git-dir",
        "HEAD",
        "--symbolic-full-name",
        "HEAD",
    ]);
    let output = source::checked_output(command, "failed to read the workspace's HEAD")?;
    let output = String::from_utf8_lossy(&output.stdout);
    let [git_dir, head, branch] = output.lines().collect::<Vec<_>>()[..] else {
        bail!("Git described the workspace's HEAD unexpectedly: {output}");
    };
    // Not `REBASE_HEAD`: Git leaves it behind when a stopped rebase is done,
    // so a rebase's own directories say whether it is still going.
    let busy = [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "rebase-merge",
        "rebase-apply",
    ]
    .iter()
    .any(|name| Path::new(git_dir).join(name).exists());
    let forward = attachment.began_detached || branch.starts_with("refs/heads/");
    Ok((!busy).then(|| (head.to_owned(), forward)))
}

/// Whether `workspace`'s HEAD is detached, as Work beginning there records
/// (`AttachmentRecord::began_detached`).
pub(super) fn detached(workspace: &Path) -> Result<bool> {
    let mut command = source::git_command(workspace);
    command.args(["symbolic-ref", "--quiet", "HEAD"]);
    let output = source::run_git(command, None, "failed to read the workspace's HEAD")?;
    match output.status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => bail!(
            "failed to read the workspace's HEAD: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

/// Before `run`'s Δ is frozen, move S0 back if the workspace's HEAD no
/// longer has it, as the owner's next tick would: after a merge reset away,
/// or a look at a recorded source that the agent has left, Δ against that
/// S0 reverts what it holds, and accepting it would land the revert. A HEAD
/// mid-merge, mid-rebase or mid-cherry-pick, or one that cannot be read,
/// leaves S0 where it is. The caller holds the run's lock.
pub(super) fn back_before_freeze(state: &State, run: &mut RunRecord) -> Result<()> {
    let Some(Ok(Some((head, _)))) = movable(run).map(workspace_head) else {
        return Ok(());
    };
    let root = run.source_path.clone();
    match earlier_s0(&root, run, &head) {
        Some(target) => moved(state, &root, run, &head, target),
        None => Ok(()),
    }
}

/// What S0 moves to, and what the move records of it. `kept` is the
/// baseline already kept for an earlier S0 it moves back to.
struct Target {
    commit: String,
    recorded_at: DateTime<Utc>,
    landed_run: Option<String>,
    kept: Option<(PathBuf, String)>,
}

/// The newest source `head` descends from that is newer than `run`'s S0:
/// the root's HEAD when it is the whole source now and S0 is not already
/// past it, or else the newest source recorded after S0 was. `clean` keeps
/// whether the root has nothing uncommitted, once it was looked at.
fn newer_source(
    root: &Path,
    run: &RunRecord,
    head: &str,
    records: &[Recorded],
    root_head: Option<&str>,
    clean: &mut Option<bool>,
) -> Result<Option<Target>> {
    let (s0, since) = run.s0_commit().context("S0 has no commit")?;
    if let Some(root_head) = root_head
        && is_ancestor(root, root_head, head)?
        // S0's own commit may be gone once nothing referred to it; then
        // nothing shows the root's HEAD is past it.
        && is_ancestor(root, root_head, s0).is_ok_and(|behind| !behind)
    {
        if clean.is_none() {
            *clean = Some(is_clean(root, root_head)?);
        }
        if *clean == Some(true) {
            return Ok(Some(Target {
                commit: root_head.to_owned(),
                recorded_at: Utc::now(),
                landed_run: None,
                kept: None,
            }));
        }
    }
    // A record made before S0 is newer still when it descends from S0: Work
    // begun from the root's HEAD while a landing was uncommitted there.
    let mut past_s0 = HashSet::new();
    if records.iter().any(|record| record.at <= since) {
        let mut command = source::git_command(root);
        command.args(["rev-list", "--ancestry-path", &format!("{s0}..{head}")]);
        // S0's commit may be gone: then nothing shows a record is past it.
        if let Ok(output) = source::checked_output(command, "failed to list commits past S0") {
            past_s0.extend(
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(str::to_owned),
            );
        }
    }
    for record in records.iter().rev() {
        // A record Git no longer has was never merged: what is merged stays.
        let newer = if record.at > since {
            is_ancestor(root, &record.commit, head).unwrap_or(false)
        } else {
            past_s0.contains(&record.commit)
        };
        if newer {
            return Ok(Some(Target {
                commit: record.commit.clone(),
                recorded_at: record.at,
                landed_run: Some(record.landed_run.clone()),
                kept: None,
            }));
        }
    }
    Ok(None)
}

/// When `head` no longer descends from `run`'s moved S0, the newest
/// earlier S0 it does descend from, or else the S0 the Work began from (a
/// world commit no HEAD descends from, or a commit reset away too): the
/// baseline kept for it, and what Δ would be had S0 never moved. `None`
/// while it descends from S0, and once S0 is back where the Work began.
fn earlier_s0(root: &Path, run: &RunRecord, head: &str) -> Option<Target> {
    let (s0, _) = run.s0_commit()?;
    let history = &run.baseline_history;
    if history.is_empty() || is_ancestor(root, s0, head).unwrap_or(true) {
        return None;
    }
    let index = (1..history.len())
        .rev()
        .find(|&index| {
            let earlier = &history[index].commit;
            earlier != s0 && is_ancestor(root, earlier, head).unwrap_or(false)
        })
        .or((history[0].commit != s0).then_some(0))?;
    // It was S0 since the move to it, or as the S0 the Work began from.
    let (recorded_at, landed_run) = match index.checked_sub(1).map(|before| &history[before]) {
        Some(before) => (before.recorded_at, before.landed_run.clone()),
        None => (run.first_s0_since(), None),
    };
    let earlier = &history[index];
    Some(Target {
        commit: earlier.commit.clone(),
        recorded_at,
        landed_run,
        kept: Some((
            earlier.baseline_path.clone(),
            earlier.baseline_commit.clone(),
        )),
    })
}

/// Whether the root's world is exactly `head`'s tree: with nothing
/// uncommitted, its HEAD is the source.
fn is_clean(root: &Path, head: &str) -> Result<bool> {
    let world = source::world_commit(root)?;
    let mut trees = source::git_command(root);
    trees.args([
        "rev-parse",
        &format!("{head}^{{tree}}"),
        &format!("{world}^{{tree}}"),
    ]);
    let trees = source::checked_output(trees, "failed to compare the root with its HEAD")?;
    let trees = String::from_utf8_lossy(&trees.stdout);
    let mut trees = trees.lines();
    Ok(trees.next() == trees.next())
}

/// Whether `ancestor` is `commit` or one of its ancestors.
fn is_ancestor(root: &Path, ancestor: &str, commit: &str) -> Result<bool> {
    let mut command = source::git_command(root);
    command.args(["merge-base", "--is-ancestor", ancestor, commit]);
    let output = source::run_git(command, None, "failed to compare commits")?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "failed to compare {ancestor} with {commit}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

enum Move {
    /// Another process holds the run.
    Busy,
    /// No newer source, or the Work stopped being movable meanwhile.
    Stayed,
    Moved(Box<RunRecord>),
}

/// `moved`, under the run's lock, for Work still movable once it is held.
fn move_s0(state: &State, root: &Path, run_id: &str, head: &str, target: Target) -> Result<Move> {
    let Ok(_lock) = OperationLock::acquire(
        &state.run_dir(run_id).join(".operation.lock"),
        "run has a foreground owner",
    ) else {
        return Ok(Move::Busy);
    };
    let mut run = state.load_run(run_id)?;
    if movable(&run).is_none() {
        return Ok(Move::Stayed);
    }
    moved(state, root, &mut run, head, target)?;
    Ok(Move::Moved(Box::new(run)))
}

/// Materialize `target` as `run`'s new baseline beside the old one, which
/// stays, and record the move with its proof. The caller holds the run's
/// lock.
fn moved(
    state: &State,
    root: &Path,
    run: &mut RunRecord,
    head: &str,
    target: Target,
) -> Result<()> {
    let run_dir = state.run_dir(&run.id);
    let workspace = run
        .attachment
        .as_ref()
        .context("not attached Work")?
        .workspace
        .clone();
    let (from, _) = run.s0_commit().context("S0 has no commit")?;
    let from = from.to_owned();
    let (baseline_path, baseline_commit) = match target.kept.clone() {
        Some(kept) => kept,
        None => {
            let dir = run_dir
                .join("s0")
                .join((run.baseline_history.len() + 1).to_string());
            // Left by a move that was never recorded: nothing refers to it.
            if dir.exists() {
                fs::remove_dir_all(&dir)?;
            }
            let baseline = source::materialize_baseline_from_commit(root, &target.commit, &dir)?;
            (baseline.baseline_path, baseline.baseline_commit)
        }
    };
    let scratch = tempfile::Builder::new().prefix("dispatch-s0-").tempdir()?;
    let digest = |baseline: &Path, name: &str| -> Result<(String, Vec<u8>)> {
        let patch = scratch.path().join(name);
        source::snapshot_delta(baseline, &workspace, &patch)?;
        let bytes = fs::read(&patch)?;
        Ok((hex::encode(Sha256::digest(&bytes)), bytes))
    };
    let (before, _) = digest(&run.baseline_path, "before.patch")?;
    let (after, delta) = digest(&baseline_path, "after.patch")?;
    let back = target.kept.is_some();
    run.baseline_history.push(EarlierBaseline {
        baseline_path: std::mem::replace(&mut run.baseline_path, baseline_path),
        baseline_commit: std::mem::replace(&mut run.baseline_commit, baseline_commit),
        commit: from.clone(),
        moved_to: target.commit.clone(),
        moved_at: Utc::now(),
        recorded_at: target.recorded_at,
        landed_run: target.landed_run.clone(),
        back,
    });
    let database = Database::open(state.db_path())?;
    persist_event(
        state,
        &database,
        EventRecord {
            run_id: run.id.clone(),
            candidate_label: run.candidates.first().map(|c| c.label.clone()),
            event_type: "attach.s0_moved".into(),
            timestamp: Utc::now(),
            payload: serde_json::json!({
                "from": from,
                "to": target.commit,
                "workspace_head": head,
                "proof": if back {
                    "workspace HEAD no longer descends from S0"
                } else {
                    "ancestor of workspace HEAD"
                },
                "recorded_at": target.recorded_at,
                "landed_run": target.landed_run,
                "delta_before_sha256": before,
                "delta_after_sha256": after,
            }),
            ..EventRecord::default()
        },
        run,
    )?;
    // The live Δ is against the new S0 from now on.
    crate::state::write_atomically(&run_dir.join("delta-live.patch"), &delta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BaselineProvenance, CloseReason, Decision, RuntimeSession,
        coherence::{self, WorkView, world},
        orchestrator::{
            ReviewDecision, accept_or_reject_latest,
            attach::{self, AttachRequest},
            registration,
            serve::Owner,
        },
    };

    struct Project {
        _temp: tempfile::TempDir,
        root: PathBuf,
        state: State,
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = source::git_command(dir).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    impl Project {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("project");
            fs::create_dir(&root).unwrap();
            let root = root.canonicalize().unwrap();
            for name in ["a", "b", "c"] {
                fs::write(root.join(format!("{name}.txt")), format!("{name}\n")).unwrap();
            }
            fs::write(root.join("dispatch.yml"), "checks:\n  verify: ['true']\n").unwrap();
            git(&root, &["init", "--quiet", "-b", "main"]);
            git(&root, &["config", "user.name", "Test"]);
            git(&root, &["config", "user.email", "test@example.invalid"]);
            git(&root, &["config", "core.editor", "true"]);
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

        fn worktree(&self, name: &str) -> PathBuf {
            let path = self.root.with_file_name(name);
            git(
                &self.root,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "-b",
                    name,
                    path.to_str().unwrap(),
                ],
            );
            path
        }

        fn attach(&self, workspace: &Path) -> RunRecord {
            attach::create(
                &self.state,
                AttachRequest {
                    workspace: workspace.to_owned(),
                    root: Some(self.root.clone()),
                    task: None,
                    agent: None,
                    pid: None,
                    command: None,
                    allow_unsafe_local: true,
                    auto_apply: false,
                    runtime: None,
                },
            )
            .unwrap()
        }

        /// A worktree whose HEAD is detached from the start, as `git
        /// worktree add --detach` makes it, registered as the owner's
        /// discovery registers a runtime's session there.
        fn detached(&self, name: &str) -> (PathBuf, String) {
            let path = self.root.with_file_name(name);
            git(
                &self.root,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "--detach",
                    path.to_str().unwrap(),
                ],
            );
            let session = RuntimeSession {
                provider: "codex".into(),
                source: "discovered".into(),
                ..session(name)
            };
            let found = registration::discovered(&self.state, &self.root, &path, session, None);
            (path, found.unwrap().unwrap())
        }

        /// Attach as a runtime's session start does: S0 is the workspace's
        /// world commit, which its HEAD never descends from.
        fn attach_session(&self, workspace: &Path) -> RunRecord {
            attach::create(
                &self.state,
                AttachRequest {
                    workspace: workspace.to_owned(),
                    root: Some(self.root.clone()),
                    task: None,
                    agent: None,
                    pid: None,
                    command: None,
                    allow_unsafe_local: true,
                    auto_apply: false,
                    runtime: Some(attach::RuntimeStart {
                        session: session("s1"),
                        resumed: false,
                        commit: source::world_commit(workspace).unwrap(),
                        discovered: false,
                        reopens: None,
                    }),
                },
            )
            .unwrap()
        }

        /// Land `text` in `file` through another Work, accepted as a person
        /// accepts it: the change is applied and never committed.
        async fn land(&self, name: &str, file: &str, text: &str) -> String {
            let workspace = self.worktree(name);
            let run = self.attach(&workspace);
            fs::write(workspace.join(file), text).unwrap();
            self.accept(&run.id).await;
            run.id
        }

        async fn accept(&self, id: &str) {
            attach::finish(&self.state, id, true).await.unwrap();
            accept_or_reject_latest(
                &self.state,
                Some(id),
                &self.root,
                ReviewDecision::Accept {
                    despite_refresh: false,
                },
                Vec::new(),
                None,
                None,
            )
            .unwrap();
        }

        fn run(&self, id: &str) -> RunRecord {
            self.state.load_run(id).unwrap()
        }

        fn events(&self, id: &str) -> Vec<EventRecord> {
            Database::open_read_only(self.state.db_path())
                .unwrap()
                .events_for_run(id)
                .unwrap()
                .into_iter()
                .filter(|event| event.event_type == "attach.s0_moved")
                .collect()
        }

        fn recorded(&self) -> Vec<Recorded> {
            recorded(&self.state, &self.root).unwrap()
        }

        fn owner(&self) -> Owner {
            Owner::new(self.root.clone(), SourceKind::Git).unwrap()
        }
    }

    fn session(id: &str) -> RuntimeSession {
        RuntimeSession {
            provider: "claude".into(),
            session_id: id.into(),
            source: "startup".into(),
            started_at: Utc::now(),
            ended_at: None,
            end_reason: None,
            model: None,
        }
    }

    /// Commit `text` in `file` as the agent in `workspace` would.
    fn commit(workspace: &Path, file: &str, text: &str) {
        fs::write(workspace.join(file), text).unwrap();
        git(
            workspace,
            &["commit", "--quiet", "-am", &format!("edit {file}")],
        );
    }

    /// The Δ of `run`'s workspace against its S0 now.
    fn delta(run: &RunRecord) -> Vec<u8> {
        let patch = tempfile::NamedTempFile::new().unwrap();
        source::snapshot_delta(
            &run.baseline_path,
            &run.candidates[0].workspace_path,
            patch.path(),
        )
        .unwrap();
        fs::read(patch.path()).unwrap()
    }

    fn paths(patch: &[u8]) -> Vec<String> {
        coherence::facts::changed_paths(patch)
            .into_iter()
            .map(|(path, _)| path)
            .collect()
    }

    /// The verdict `check` would give `run` against the source now.
    fn verdict(run: &RunRecord) -> Decision {
        let patch = tempfile::NamedTempFile::new().unwrap();
        fs::write(patch.path(), delta(run)).unwrap();
        let world = world::observe(
            &run.source_path,
            &run.baseline_path,
            &run.baseline_commit,
            &run.source_kind,
        )
        .unwrap();
        coherence::evaluate(
            &world,
            &WorkView {
                source: &run.source_path,
                delta_patch: patch.path(),
                baseline: &run.baseline_path,
                baseline_commit: &run.baseline_commit,
            },
        )
        .unwrap()
        .decision
    }

    fn merge_base(run: &RunRecord) -> String {
        match &run.attachment.as_ref().unwrap().provenance {
            BaselineProvenance::GitMergeBase { commit } => commit.clone(),
            other => panic!("not a merge base: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_landing_records_the_source_and_touches_nothing_of_the_users() {
        let project = Project::new();
        let root = &project.root;
        // The person's own uncommitted and untracked work is part of the source.
        fs::write(root.join("c.txt"), "the person's own\n").unwrap();
        fs::write(root.join("notes.txt"), "untracked\n").unwrap();
        let workspace = project.worktree("landing");
        let landing = project.attach(&workspace);
        fs::write(workspace.join("b.txt"), "landed\n").unwrap();
        let users = || {
            (
                git(root, &["rev-parse", "HEAD"]),
                git(root, &["symbolic-ref", "HEAD"]),
                git(root, &["for-each-ref", "refs/heads"]),
                fs::read(root.join(".git/index")).unwrap(),
            )
        };
        let before = users();

        project.accept(&landing.id).await;

        assert_eq!(users(), before);
        assert_eq!(git(root, &["ls-files", "notes.txt"]), "");
        let source = git(root, &["rev-parse", source::SOURCE_REF]);
        assert_eq!(git(root, &["show", &format!("{source}:b.txt")]), "landed");
        assert_eq!(
            git(root, &["show", &format!("{source}:c.txt")]),
            "the person's own"
        );
        assert_eq!(
            git(root, &["show", &format!("{source}:notes.txt")]),
            "untracked"
        );
        assert_eq!(git(root, &["rev-parse", &format!("{source}^")]), before.0);
        assert_eq!(
            git(
                root,
                &["reflog", "show", "--format=%gs", source::SOURCE_REF]
            ),
            format!("dispatch: Work {} landed", landing.id)
        );
        let recorded = project.recorded();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].commit, source);
        assert_eq!(recorded[0].landed_run, landing.id);
    }

    #[tokio::test]
    async fn a_source_that_cannot_be_recorded_does_not_fail_the_landing() {
        let project = Project::new();
        // `refs/dispatch` as a file: no ref can be made under it.
        fs::write(project.root.join(".git/refs/dispatch"), "").unwrap();

        let landed = project.land("landing", "b.txt", "landed\n").await;

        assert_eq!(
            project.run(&landed).outcome.application,
            crate::ApplicationState::Applied
        );
        assert_eq!(
            fs::read_to_string(project.root.join("b.txt")).unwrap(),
            "landed\n"
        );
        assert!(project.recorded().is_empty());
    }

    #[tokio::test]
    async fn what_accept_would_verify_names_the_s0_the_work_moved_to() {
        let project = Project::new();
        let workspace = project.worktree("agent");
        let work = project.attach(&workspace);
        commit(&workspace, "a.txt", "agent\n");
        let mut owner = project.owner();
        owner.tick(&project.state);
        // The line finish and check print under "At accept".
        let scope = |run: &RunRecord| {
            let validity = run.coherence.as_ref().and_then(|c| c.validity.as_ref());
            crate::orchestrator::accept_scope(&project.state, run, validity.unwrap())
        };
        let unmoved = "merged checks would not run now — the source has not moved since";
        let own = "so the merged result is the Work's own tree";
        assert_eq!(
            scope(&project.run(&work.id)),
            format!("{unmoved} the Work began, {own}")
        );

        project.land("landing", "b.txt", "landed\n").await;
        git(
            &workspace,
            &["merge", "--quiet", "--no-edit", source::SOURCE_REF],
        );
        owner.tick(&project.state);

        let run = project.run(&work.id);
        let moved_to = &run.baseline_history[0].moved_to[..8];
        assert_eq!(
            scope(&run),
            format!("{unmoved} the Work's S0 moved to {moved_to}, {own}")
        );
    }

    #[tokio::test]
    async fn merging_the_recorded_source_moves_s0_so_the_delta_is_the_agents_own() {
        let project = Project::new();
        let workspace = project.worktree("agent");
        let work = project.attach(&workspace);
        commit(&workspace, "a.txt", "agent\n");
        let mut owner = project.owner();
        // The owner followed the Work against its first S0.
        let first = owner.tick(&project.state);
        assert!(first.error.is_none(), "{:?}", first.error);
        let landed = project.land("landing", "b.txt", "landed\n").await;
        git(
            &workspace,
            &["merge", "--quiet", "--no-edit", source::SOURCE_REF],
        );
        let before = project.run(&work.id);
        assert_eq!(paths(&delta(&before)), ["a.txt", "b.txt"]);
        assert_eq!(verdict(&before), Decision::Refresh);

        let tick = owner.tick(&project.state);

        assert!(tick.error.is_none(), "{:?}", tick.error);
        let run = project.run(&work.id);
        let recorded = project.recorded();
        let moved = &run.baseline_history[..];
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].baseline_path, before.baseline_path);
        assert_eq!(moved[0].baseline_commit, before.baseline_commit);
        assert_eq!(moved[0].commit, merge_base(&work));
        assert_eq!(moved[0].moved_to, recorded[0].commit);
        assert_eq!(moved[0].recorded_at, recorded[0].at);
        assert_eq!(moved[0].landed_run.as_deref(), Some(landed.as_str()));
        assert!(before.baseline_path.join(".git").is_dir());
        let live = fs::read(project.state.run_dir(&work.id).join("delta-live.patch")).unwrap();
        assert_eq!(live, delta(&run));
        assert_eq!(paths(&live), ["a.txt"]);
        assert_eq!(verdict(&run), Decision::Continue);
        let stored = run.coherence.as_ref().and_then(|c| c.validity.as_ref());
        assert_eq!(stored.map(|v| v.decision), Some(Decision::Continue));

        let events = project.events(&work.id);
        assert_eq!(events.len(), 1);
        let payload = &events[0].payload;
        assert_eq!(payload["proof"], "ancestor of workspace HEAD");
        assert_eq!(payload["from"], merge_base(&work));
        assert_eq!(payload["to"], recorded[0].commit);
        assert_eq!(payload["landed_run"], landed);
        assert_eq!(
            payload["workspace_head"],
            git(&workspace, &["rev-parse", "HEAD"])
        );
        assert_eq!(
            payload["delta_before_sha256"],
            hex::encode(Sha256::digest(delta(&before)))
        );
        assert_eq!(
            payload["delta_after_sha256"],
            hex::encode(Sha256::digest(&live))
        );

        assert_eq!(
            crate::orchestrator::s0_lines(&run, run.attachment.as_ref().unwrap()),
            [
                format!(
                    "S0: commit {} (moved from {} at {}: the workspace merged the recorded source)",
                    recorded[0].commit,
                    merge_base(&work),
                    moved[0].moved_at.format("%Y-%m-%d %H:%M UTC")
                ),
                format!(
                    "S0 before: commit {} (merge base with the root)",
                    merge_base(&work)
                ),
            ]
        );

        // Looked at again against its new S0, it stays.
        owner.tick(&project.state);
        assert_eq!(project.run(&work.id).baseline_history.len(), 1);
        // And it lands as only the agent's change.
        project.accept(&work.id).await;
        let root = &project.root;
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "agent\n");
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "landed\n");
    }

    /// Work found stale against a landing, which it then merges, is checked
    /// again against its new S0, though its Δ is the same bytes against
    /// either and the source has not moved since.
    #[tokio::test]
    async fn stale_work_that_merges_the_landing_continues_though_its_delta_is_unchanged() {
        let project = Project::new();
        let lib = "pub fn scale(n: i32) -> i32 {\n    n * 2\n}\n";
        fs::write(project.root.join("lib.rs"), lib).unwrap();
        git(&project.root, &["add", "lib.rs"]);
        git(&project.root, &["commit", "--quiet", "-m", "lib"]);
        let workspace = project.worktree("agent");
        let work = project.attach(&workspace);
        let report = "pub fn report() -> i32 {\n    crate::scale(3)\n}\n";
        fs::write(workspace.join("report.rs"), report).unwrap();
        git(&workspace, &["add", "report.rs"]);
        git(&workspace, &["commit", "--quiet", "-m", "report"]);
        let changed = lib.replace("(n: i32)", "(n: i32, by: i32)");
        project.land("landing", "lib.rs", &changed).await;
        let stored = || {
            let run = project.run(&work.id);
            run.coherence.and_then(|c| c.validity).map(|v| v.decision)
        };
        let mut owner = project.owner();
        owner.tick(&project.state);
        assert_eq!(stored(), Some(Decision::Refresh));
        let before = delta(&project.run(&work.id));

        git(
            &workspace,
            &["merge", "--quiet", "--no-edit", source::SOURCE_REF],
        );
        let tick = owner.tick(&project.state);

        assert!(tick.error.is_none(), "{:?}", tick.error);
        let run = project.run(&work.id);
        assert_eq!(run.baseline_history.len(), 1);
        assert_eq!(delta(&run), before);
        assert_eq!(stored(), Some(Decision::Continue));
    }

    #[tokio::test]
    async fn a_merge_in_progress_does_not_move_s0_and_finishing_it_does() {
        let project = Project::new();
        let workspace = project.worktree("agent");
        let work = project.attach(&workspace);
        commit(&workspace, "a.txt", "agent\n");
        commit(&workspace, "c.txt", "agent's c\n");
        git(&workspace, &["branch", "side", "main"]);
        git(&workspace, &["checkout", "--quiet", "side"]);
        commit(&workspace, "c.txt", "side's c\n");
        git(&workspace, &["checkout", "--quiet", "agent"]);
        project.land("landing", "b.txt", "landed\n").await;
        git(
            &workspace,
            &["merge", "--quiet", "--no-edit", source::SOURCE_REF],
        );
        // HEAD descends from the recorded source, but another merge stopped.
        let conflicted = source::git_command(&workspace)
            .args(["merge", "--no-edit", "side"])
            .output()
            .unwrap();
        assert!(!conflicted.status.success());
        let mut owner = project.owner();

        owner.tick(&project.state);
        assert!(project.run(&work.id).baseline_history.is_empty());

        fs::write(workspace.join("c.txt"), "resolved\n").unwrap();
        git(&workspace, &["commit", "--quiet", "-am", "merge side"]);
        owner.tick(&project.state);
        let run = project.run(&work.id);
        assert_eq!(run.baseline_history.len(), 1);
        assert_eq!(paths(&delta(&run)), ["a.txt", "c.txt"]);
    }

    #[tokio::test]
    async fn a_cherry_pick_of_the_landed_change_does_not_move_s0() {
        let project = Project::new();
        let workspace = project.worktree("agent");
        let work = project.attach(&workspace);
        commit(&workspace, "a.txt", "agent\n");
        project.land("landing", "b.txt", "landed\n").await;
        git(&workspace, &["cherry-pick", source::SOURCE_REF]);

        project.owner().tick(&project.state);

        let run = project.run(&work.id);
        assert!(run.baseline_history.is_empty());
        assert!(project.events(&work.id).is_empty());
        assert_eq!(verdict(&run), Decision::Refresh);
    }

    #[tokio::test]
    async fn a_rebase_onto_the_recorded_source_moves_s0_once_it_is_done() {
        let project = Project::new();
        let workspace = project.worktree("agent");
        let work = project.attach(&workspace);
        commit(&workspace, "a.txt", "agent\n");
        commit(&workspace, "b.txt", "agent's b\n");
        project.land("landing", "b.txt", "landed\n").await;
        // Stops at the second commit: HEAD is the recorded source and the
        // first, detached, with the rebase in progress.
        let stopped = source::git_command(&workspace)
            .args(["rebase", source::SOURCE_REF])
            .output()
            .unwrap();
        assert!(!stopped.status.success());
        let mut owner = project.owner();

        owner.tick(&project.state);
        assert!(project.run(&work.id).baseline_history.is_empty());

        fs::write(workspace.join("b.txt"), "agent's b\n").unwrap();
        git(&workspace, &["add", "b.txt"]);
        git(&workspace, &["rebase", "--continue"]);
        // Git leaves `REBASE_HEAD` behind.
        let rebase_head = git(&workspace, &["rev-parse", "--git-path", "REBASE_HEAD"]);
        assert!(workspace.join(rebase_head).exists());
        owner.tick(&project.state);
        let run = project.run(&work.id);
        assert_eq!(run.baseline_history.len(), 1);
        assert_eq!(
            run.baseline_history[0].moved_to,
            project.recorded()[0].commit
        );
        assert_eq!(paths(&delta(&run)), ["a.txt", "b.txt"]);
        assert_eq!(verdict(&run), Decision::Continue);
    }

    #[tokio::test]
    async fn a_landed_change_the_agent_reverts_after_merging_is_in_the_new_delta() {
        let project = Project::new();
        let workspace = project.worktree("agent");
        let work = project.attach(&workspace);
        commit(&workspace, "a.txt", "agent\n");
        project.land("landing", "b.txt", "landed\n").await;
        git(
            &workspace,
            &["merge", "--quiet", "--no-edit", source::SOURCE_REF],
        );
        let mut owner = project.owner();
        owner.tick(&project.state);
        assert_eq!(project.run(&work.id).baseline_history.len(), 1);

        // A revert is a commit on top: HEAD still has S0.
        git(&workspace, &["revert", "--no-edit", "-m", "1", "HEAD"]);
        owner.tick(&project.state);

        let run = project.run(&work.id);
        assert_eq!(run.baseline_history.len(), 1);
        assert_eq!(project.events(&work.id).len(), 1);
        let delta = String::from_utf8(delta(&run)).unwrap();
        assert_eq!(paths(delta.as_bytes()), ["a.txt", "b.txt"]);
        assert!(delta.contains("-landed\n+b\n"), "{delta}");
    }

    #[tokio::test]
    async fn a_merge_reset_away_moves_s0_back_so_the_delta_reverts_nothing() {
        let project = Project::new();
        let workspace = project.worktree("agent");
        let work = project.attach(&workspace);
        commit(&workspace, "a.txt", "agent\n");
        let landed = project.land("landing", "b.txt", "landed\n").await;
        git(
            &workspace,
            &["merge", "--quiet", "--no-edit", source::SOURCE_REF],
        );
        let mut owner = project.owner();
        owner.tick(&project.state);
        let forward = project.run(&work.id);
        assert_eq!(forward.baseline_history.len(), 1);
        let record = project.recorded()[0].clone();

        git(&workspace, &["reset", "--quiet", "--hard", "HEAD^1"]);
        owner.tick(&project.state);

        let run = project.run(&work.id);
        let back = &run.baseline_history[1];
        assert_eq!(run.baseline_history.len(), 2);
        assert!(back.back);
        assert_eq!(back.commit, record.commit);
        assert_eq!(back.moved_to, merge_base(&work));
        assert_eq!(back.baseline_path, forward.baseline_path);
        assert_eq!(run.baseline_path, work.baseline_path);
        assert_eq!(run.baseline_commit, work.baseline_commit);
        assert_eq!(
            run.s0_commit(),
            Some((merge_base(&work).as_str(), work.created_at))
        );
        let live = fs::read(project.state.run_dir(&work.id).join("delta-live.patch")).unwrap();
        assert_eq!(live, delta(&run));
        assert_eq!(paths(&live), ["a.txt"]);
        let events = project.events(&work.id);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[1].payload["proof"],
            "workspace HEAD no longer descends from S0"
        );
        assert_eq!(events[1].payload["from"], record.commit);
        assert_eq!(events[1].payload["to"], merge_base(&work));
        assert!(
            crate::orchestrator::s0_lines(&run, run.attachment.as_ref().unwrap())[0]
                .ends_with(": the workspace's HEAD no longer has it)")
        );

        // Merged again, it moves forward again, to a baseline of its own.
        git(
            &workspace,
            &["merge", "--quiet", "--no-edit", source::SOURCE_REF],
        );
        owner.tick(&project.state);
        let run = project.run(&work.id);
        assert_eq!(run.baseline_history.len(), 3);
        assert!(!run.baseline_history[2].back);
        assert_eq!(run.s0_commit(), Some((record.commit.as_str(), record.at)));
        assert_eq!(
            run.baseline_history[2].landed_run.as_deref(),
            Some(landed.as_str())
        );
        assert_ne!(run.baseline_path, forward.baseline_path);
        assert_eq!(paths(&delta(&run)), ["a.txt"]);

        // Reset away again, it lands as only the agent's change.
        git(&workspace, &["reset", "--quiet", "--hard", "HEAD^1"]);
        owner.tick(&project.state);
        assert_eq!(project.run(&work.id).baseline_history.len(), 4);
        project.accept(&work.id).await;
        let root = &project.root;
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "agent\n");
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "landed\n");
    }

    #[tokio::test]
    async fn a_session_whose_head_has_no_earlier_s0_goes_back_to_where_it_began() {
        let project = Project::new();
        let workspace = project.worktree("agent");
        // Uncommitted at the session's start: the HEAD never has its S0.
        fs::write(workspace.join("c.txt"), "the session's own\n").unwrap();
        let work = project.attach_session(&workspace);
        commit(&workspace, "a.txt", "agent\n");
        project.land("landing", "b.txt", "landed\n").await;
        git(
            &workspace,
            &["merge", "--quiet", "--no-edit", source::SOURCE_REF],
        );
        let mut owner = project.owner();
        owner.tick(&project.state);
        assert_eq!(project.run(&work.id).baseline_history.len(), 1);

        git(&workspace, &["reset", "--quiet", "--hard", "HEAD^1"]);
        owner.tick(&project.state);

        let run = project.run(&work.id);
        assert_eq!(run.baseline_history.len(), 2);
        assert_eq!(run.s0_commit(), work.s0_commit());
        assert_eq!(run.baseline_path, work.baseline_path);
        assert_eq!(paths(&delta(&run)), ["a.txt"]);
        project.accept(&work.id).await;
        let root = &project.root;
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "agent\n");
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "landed\n");
    }

    #[tokio::test]
    async fn s0_back_where_the_session_began_stays_there_until_a_merge_moves_it() {
        let project = Project::new();
        let workspace = project.worktree("agent");
        fs::write(workspace.join("c.txt"), "the session's own\n").unwrap();
        let work = project.attach_session(&workspace);
        commit(&workspace, "a.txt", "agent\n");
        let landed = project.land("landing", "b.txt", "landed\n").await;
        git(
            &workspace,
            &["merge", "--quiet", "--no-edit", source::SOURCE_REF],
        );
        let mut owner = project.owner();
        owner.tick(&project.state);
        git(&workspace, &["reset", "--quiet", "--hard", "HEAD^1"]);
        owner.tick(&project.state);
        assert_eq!(project.events(&work.id).len(), 2);

        // Its HEAD never has that world commit, and S0 is it already: no
        // move to itself, on later ticks or after a commit of its own.
        owner.tick(&project.state);
        owner.tick(&project.state);
        commit(&workspace, "a.txt", "agent, again\n");
        owner.tick(&project.state);
        let run = project.run(&work.id);
        assert_eq!(project.events(&work.id).len(), 2);
        assert_eq!(run.baseline_history.len(), 2);
        assert_eq!(run.s0_commit(), work.s0_commit());

        // Merged again, it moves forward.
        git(
            &workspace,
            &["merge", "--quiet", "--no-edit", source::SOURCE_REF],
        );
        owner.tick(&project.state);
        let run = project.run(&work.id);
        assert_eq!(run.baseline_history.len(), 3);
        assert!(!run.baseline_history[2].back);
        assert_eq!(
            run.baseline_history[2].landed_run.as_deref(),
            Some(landed.as_str())
        );
    }

    #[tokio::test]
    async fn s0_moves_back_to_the_newest_earlier_s0_the_head_still_has() {
        let project = Project::new();
        let workspace = project.worktree("agent");
        let work = project.attach(&workspace);
        commit(&workspace, "a.txt", "agent\n");
        let mut owner = project.owner();
        project.land("first", "b.txt", "landed\n").await;
        git(
            &workspace,
            &["merge", "--quiet", "--no-edit", source::SOURCE_REF],
        );
        owner.tick(&project.state);
        let first = project.run(&work.id);
        project.land("second", "c.txt", "landed\n").await;
        git(
            &workspace,
            &["merge", "--quiet", "--no-edit", source::SOURCE_REF],
        );
        owner.tick(&project.state);
        assert_eq!(project.run(&work.id).baseline_history.len(), 2);

        git(&workspace, &["reset", "--quiet", "--hard", "HEAD^1"]);
        owner.tick(&project.state);

        let run = project.run(&work.id);
        assert_eq!(run.baseline_history.len(), 3);
        assert_eq!(run.s0_commit(), first.s0_commit());
        assert_eq!(run.baseline_path, first.baseline_path);
        assert_eq!(
            run.baseline_history[2].landed_run,
            first.baseline_history[0].landed_run
        );
        assert_eq!(paths(&delta(&run)), ["a.txt"]);
    }

    #[tokio::test]
    async fn work_begun_after_an_uncommitted_landing_moves_when_it_merges_the_record() {
        let project = Project::new();
        let landed = project.land("landing", "b.txt", "landed\n").await;
        // Begun from the root's HEAD, which lacks the uncommitted landing.
        let workspace = project.worktree("agent");
        let work = project.attach(&workspace);
        commit(&workspace, "a.txt", "agent\n");
        git(
            &workspace,
            &["merge", "--quiet", "--no-edit", source::SOURCE_REF],
        );
        let record = project.recorded()[0].clone();
        assert!(record.at < work.created_at);
        assert_eq!(paths(&delta(&work)), ["a.txt", "b.txt"]);

        project.owner().tick(&project.state);

        let run = project.run(&work.id);
        assert_eq!(run.baseline_history.len(), 1);
        assert_eq!(run.baseline_history[0].moved_to, record.commit);
        assert_eq!(
            run.baseline_history[0].landed_run.as_deref(),
            Some(landed.as_str())
        );
        assert_eq!(paths(&delta(&run)), ["a.txt"]);
        assert_eq!(verdict(&run), Decision::Continue);
    }

    #[tokio::test]
    async fn an_older_record_that_is_not_past_s0_does_not_move_it() {
        let project = Project::new();
        let workspace = project.worktree("agent");
        let work = project.attach(&workspace);
        commit(&workspace, "a.txt", "agent\n");
        project.land("first", "b.txt", "landed\n").await;
        project.land("second", "c.txt", "landed\n").await;
        let [first, second] = &project.recorded()[..] else {
            panic!("two records");
        };
        git(
            &workspace,
            &["merge", "--quiet", "--no-edit", &second.commit],
        );
        let mut owner = project.owner();
        owner.tick(&project.state);
        assert_eq!(project.run(&work.id).baseline_history.len(), 1);

        // Older, and a sibling of S0 rather than past it.
        git(
            &workspace,
            &["merge", "--quiet", "--no-edit", &first.commit],
        );
        owner.tick(&project.state);

        let run = project.run(&work.id);
        assert_eq!(run.baseline_history.len(), 1);
        assert_eq!(run.s0_commit(), Some((second.commit.as_str(), second.at)));
    }

    #[tokio::test]
    async fn a_detached_checkout_of_the_recorded_source_does_not_move_s0() {
        let project = Project::new();
        let workspace = project.worktree("agent");
        let work = project.attach(&workspace);
        commit(&workspace, "a.txt", "agent\n");
        project.land("landing", "b.txt", "landed\n").await;
        let mut owner = project.owner();

        git(
            &workspace,
            &["checkout", "--quiet", "--detach", source::SOURCE_REF],
        );
        owner.tick(&project.state);
        git(&workspace, &["checkout", "--quiet", "agent"]);
        owner.tick(&project.state);

        assert!(project.run(&work.id).baseline_history.is_empty());
    }

    #[tokio::test]
    async fn a_workspace_detached_from_the_start_moves_s0_when_its_agent_merges() {
        let project = Project::new();
        let (workspace, work) = project.detached("agent");
        let began = project.run(&work);
        commit(&workspace, "a.txt", "agent\n");
        let mut owner = project.owner();
        owner.tick(&project.state);
        let landed = project.land("landing", "b.txt", "landed\n").await;
        let record = project.recorded()[0].clone();
        // As landing notes and asks name it: by its commit.
        git(
            &workspace,
            &["merge", "--quiet", "--no-edit", &record.commit],
        );
        let head = git(&workspace, &["rev-parse", "HEAD"]);
        assert_eq!(
            git(&workspace, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "HEAD"
        );

        let tick = owner.tick(&project.state);

        assert!(tick.error.is_none(), "{:?}", tick.error);
        let run = project.run(&work);
        assert_eq!(run.baseline_history.len(), 1);
        assert_eq!(run.baseline_history[0].commit, began.s0_commit().unwrap().0);
        assert_eq!(run.s0_commit(), Some((record.commit.as_str(), record.at)));
        let events = project.events(&work);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload["proof"], "ancestor of workspace HEAD");
        assert_eq!(events[0].payload["workspace_head"], head);
        assert_eq!(events[0].payload["landed_run"], landed);
        assert_eq!(paths(&delta(&run)), ["a.txt"]);
        assert_eq!(verdict(&run), Decision::Continue);
        let stored = run.coherence.as_ref().and_then(|c| c.validity.as_ref());
        assert_eq!(stored.map(|v| v.decision), Some(Decision::Continue));
    }

    #[tokio::test]
    async fn a_workspace_detached_from_the_start_does_not_move_mid_merge() {
        let project = Project::new();
        let side = project.worktree("side");
        commit(&side, "c.txt", "side's c\n");
        let (workspace, work) = project.detached("agent");
        commit(&workspace, "a.txt", "agent\n");
        commit(&workspace, "c.txt", "agent's c\n");
        project.land("landing", "b.txt", "landed\n").await;
        git(
            &workspace,
            &["merge", "--quiet", "--no-edit", source::SOURCE_REF],
        );
        // HEAD descends from the recorded source, but another merge stopped.
        let conflicted = source::git_command(&workspace)
            .args(["merge", "--no-edit", "side"])
            .output()
            .unwrap();
        assert!(!conflicted.status.success());
        let mut owner = project.owner();

        owner.tick(&project.state);
        assert!(project.run(&work).baseline_history.is_empty());

        fs::write(workspace.join("c.txt"), "resolved\n").unwrap();
        git(&workspace, &["commit", "--quiet", "-am", "merge side"]);
        owner.tick(&project.state);
        let run = project.run(&work);
        assert_eq!(run.baseline_history.len(), 1);
        assert_eq!(paths(&delta(&run)), ["a.txt", "c.txt"]);
    }

    /// A look in a workspace detached from the start, `git checkout --detach`
    /// of the recorded source, is a HEAD that has it: S0 moves there, and
    /// back once the agent checks its own commit out again. Finished before
    /// the owner sees it leave, S0 moves back first, or Δ would revert the
    /// landing and land as CONTINUE.
    #[tokio::test]
    async fn a_look_at_the_recorded_source_from_a_detached_workspace_lands_no_revert() {
        let project = Project::new();
        let (workspace, work) = project.detached("agent");
        let began = project.run(&work);
        commit(&workspace, "a.txt", "agent\n");
        let own = git(&workspace, &["rev-parse", "HEAD"]);
        project.land("landing", "b.txt", "landed\n").await;
        let record = project.recorded()[0].clone();
        let mut owner = project.owner();
        let look = |commit: &str| {
            git(&workspace, &["checkout", "--quiet", "--detach", commit]);
        };

        look(&record.commit);
        owner.tick(&project.state);
        let looking = project.run(&work);
        assert_eq!(
            looking.s0_commit(),
            Some((record.commit.as_str(), record.at))
        );
        assert!(delta(&looking).is_empty());

        look(&own);
        owner.tick(&project.state);
        let left = project.run(&work);
        assert_eq!(left.baseline_history.len(), 2);
        assert!(left.baseline_history[1].back);
        assert_eq!(left.s0_commit(), began.s0_commit());
        assert_eq!(paths(&delta(&left)), ["a.txt"]);

        look(&record.commit);
        owner.tick(&project.state);
        assert_eq!(project.run(&work).baseline_history.len(), 3);
        look(&own);
        project.accept(&work).await;

        let run = project.run(&work);
        assert_eq!(run.baseline_history.len(), 4);
        assert!(run.baseline_history[3].back);
        assert_eq!(run.s0_commit(), began.s0_commit());
        let root = &project.root;
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "agent\n");
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "landed\n");
    }

    #[tokio::test]
    async fn a_merge_reset_away_just_before_finish_lands_no_revert() {
        let project = Project::new();
        let workspace = project.worktree("agent");
        let work = project.attach(&workspace);
        commit(&workspace, "a.txt", "agent\n");
        project.land("landing", "b.txt", "landed\n").await;
        git(
            &workspace,
            &["merge", "--quiet", "--no-edit", source::SOURCE_REF],
        );
        project.owner().tick(&project.state);
        assert_eq!(project.run(&work.id).baseline_history.len(), 1);

        // Finished before the owner looks again.
        git(&workspace, &["reset", "--quiet", "--hard", "HEAD^1"]);
        project.accept(&work.id).await;

        let run = project.run(&work.id);
        assert_eq!(run.baseline_history.len(), 2);
        assert_eq!(
            run.s0_commit(),
            Some((merge_base(&work).as_str(), work.created_at))
        );
        let root = &project.root;
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "agent\n");
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "landed\n");
    }

    /// A branch workspace whose S0 moved to R, checked out detached at a
    /// commit without R, with an uncommitted change Git carries over.
    async fn detached_without_the_moved_s0(project: &Project) -> (PathBuf, RunRecord, Recorded) {
        let workspace = project.worktree("agent");
        let work = project.attach(&workspace);
        commit(&workspace, "a.txt", "agent\n");
        project.land("landing", "b.txt", "landed\n").await;
        git(
            &workspace,
            &["merge", "--quiet", "--no-edit", source::SOURCE_REF],
        );
        project.owner().tick(&project.state);
        assert_eq!(project.run(&work.id).baseline_history.len(), 1);
        fs::write(workspace.join("c.txt"), "agent's c\n").unwrap();
        git(&workspace, &["checkout", "--quiet", "--detach", "main"]);
        (workspace, work, project.recorded()[0].clone())
    }

    /// The move back, as `detached_without_the_moved_s0` leaves it.
    fn assert_moved_back(project: &Project, workspace: &Path, work: &RunRecord, record: &Recorded) {
        let run = project.run(&work.id);
        assert_eq!(run.baseline_history.len(), 2);
        assert!(run.baseline_history[1].back);
        assert_eq!(
            run.s0_commit(),
            Some((merge_base(work).as_str(), work.created_at))
        );
        let events = project.events(&work.id);
        assert_eq!(events.len(), 2);
        let payload = &events[1].payload;
        assert_eq!(
            payload["proof"],
            "workspace HEAD no longer descends from S0"
        );
        assert_eq!(payload["from"], record.commit);
        assert_eq!(payload["to"], merge_base(work));
        assert_eq!(
            payload["workspace_head"],
            git(workspace, &["rev-parse", "HEAD"])
        );
    }

    #[tokio::test]
    async fn a_detached_checkout_without_the_moved_s0_moves_it_back_on_a_tick() {
        let project = Project::new();
        let (workspace, work, record) = detached_without_the_moved_s0(&project).await;

        project.owner().tick(&project.state);

        assert_moved_back(&project, &workspace, &work, &record);
        assert_eq!(paths(&delta(&project.run(&work.id))), ["c.txt"]);
        // Its branch again, and S0 moves forward again.
        git(&workspace, &["checkout", "--quiet", "agent"]);
        project.owner().tick(&project.state);
        let run = project.run(&work.id);
        assert_eq!(run.baseline_history.len(), 3);
        assert_eq!(run.s0_commit(), Some((record.commit.as_str(), record.at)));
        assert_eq!(paths(&delta(&run)), ["a.txt", "c.txt"]);
    }

    #[tokio::test]
    async fn a_detached_checkout_without_the_moved_s0_just_before_finish_lands_no_revert() {
        let project = Project::new();
        let (workspace, work, record) = detached_without_the_moved_s0(&project).await;

        project.accept(&work.id).await;

        assert_moved_back(&project, &workspace, &work, &record);
        let root = &project.root;
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "landed\n");
        assert_eq!(
            fs::read_to_string(root.join("c.txt")).unwrap(),
            "agent's c\n"
        );
    }

    /// A commit looked at detached, in a workspace that began on a branch,
    /// proves no move forward; on the branch, it is looked at again.
    #[tokio::test]
    async fn a_commit_first_seen_detached_is_looked_at_again_on_the_branch() {
        let project = Project::new();
        let workspace = project.worktree("agent");
        let work = project.attach(&workspace);
        project.land("landing", "b.txt", "landed\n").await;
        let mut owner = project.owner();
        git(
            &workspace,
            &["checkout", "--quiet", "--detach", source::SOURCE_REF],
        );
        owner.tick(&project.state);
        assert!(project.run(&work.id).baseline_history.is_empty());

        git(&workspace, &["checkout", "--quiet", "agent"]);
        git(
            &workspace,
            &["merge", "--quiet", "--ff-only", source::SOURCE_REF],
        );
        owner.tick(&project.state);

        let run = project.run(&work.id);
        assert_eq!(run.baseline_history.len(), 1);
        assert_eq!(run.s0_commit().unwrap().0, project.recorded()[0].commit);
    }

    #[tokio::test]
    async fn reopened_work_keeps_whether_its_workspace_began_detached() {
        let project = Project::new();
        let workspace = project.worktree("agent");
        let discover = |id: &str| {
            let found = registration::discovered(
                &project.state,
                &project.root,
                &workspace,
                session(id),
                None,
            );
            found.unwrap().unwrap()
        };
        let first = discover("s1");
        assert!(!project.run(&first).attachment.unwrap().began_detached);
        commit(&workspace, "a.txt", "agent\n");
        attach::finish(&project.state, &first, true).await.unwrap();
        git(&workspace, &["checkout", "--quiet", "--detach"]);

        let reopened = discover("s2");

        let run = project.run(&reopened);
        assert_eq!(run.s0_commit(), project.run(&first).s0_commit());
        assert!(!run.attachment.unwrap().began_detached);
    }

    #[tokio::test]
    async fn merging_the_roots_head_moves_s0_only_with_nothing_uncommitted_there() {
        let project = Project::new();
        let root = &project.root;
        let workspace = project.worktree("agent");
        let work = project.attach(&workspace);
        commit(&workspace, "a.txt", "agent\n");
        project.land("landing", "b.txt", "landed\n").await;
        git(root, &["commit", "--quiet", "-am", "the landing"]);
        let head = git(root, &["rev-parse", "HEAD"]);
        git(&workspace, &["merge", "--quiet", "--no-edit", "main"]);

        fs::write(root.join("c.txt"), "uncommitted\n").unwrap();
        project.owner().tick(&project.state);
        assert!(project.run(&work.id).baseline_history.is_empty());

        fs::write(root.join("c.txt"), "c\n").unwrap();
        project.owner().tick(&project.state);
        let run = project.run(&work.id);
        assert_eq!(run.baseline_history.len(), 1);
        assert_eq!(run.baseline_history[0].moved_to, head);
        assert_eq!(run.baseline_history[0].landed_run, None);
        assert_eq!(
            project.events(&work.id)[0].payload["landed_run"],
            serde_json::Value::Null
        );
        assert_eq!(paths(&delta(&run)), ["a.txt"]);
        assert_eq!(verdict(&run), Decision::Continue);
        assert!(
            crate::orchestrator::s0_lines(&run, run.attachment.as_ref().unwrap())[0]
                .ends_with(": the workspace merged the root's HEAD)")
        );
    }

    #[tokio::test]
    async fn finished_reopened_and_superseded_work_keep_their_history() {
        let project = Project::new();
        let workspace = project.worktree("agent");
        let first = registration::discovered(
            &project.state,
            &project.root,
            &workspace,
            session("s1"),
            None,
        )
        .unwrap()
        .unwrap();
        commit(&workspace, "a.txt", "agent\n");
        project.land("landing", "b.txt", "landed\n").await;
        git(
            &workspace,
            &["merge", "--quiet", "--no-edit", source::SOURCE_REF],
        );
        let mut owner = project.owner();
        owner.tick(&project.state);
        let history = project.run(&first).baseline_history;
        assert_eq!(history.len(), 1);

        // Finished Work never moves again.
        attach::finish(&project.state, &first, true).await.unwrap();
        project.land("second", "c.txt", "landed too\n").await;
        git(
            &workspace,
            &["merge", "--quiet", "--no-edit", source::SOURCE_REF],
        );
        owner.tick(&project.state);
        assert_eq!(project.run(&first).baseline_history, history);

        // A resumed session goes on from the moved S0, with its history;
        // the Work it reopens is superseded and keeps its own.
        let reopened = registration::discovered(
            &project.state,
            &project.root,
            &workspace,
            session("s2"),
            None,
        )
        .unwrap()
        .unwrap();
        let old = project.run(&first);
        assert_eq!(
            old.outcome.closed.as_ref().map(|closed| closed.reason),
            Some(CloseReason::Superseded)
        );
        assert_eq!(old.baseline_history, history);
        let run = project.run(&reopened);
        assert_eq!(run.baseline_history, history);
        assert_eq!(
            fs::read_to_string(run.baseline_path.join("b.txt")).unwrap(),
            "landed\n"
        );

        // It moves on from there, to the second landing it merged.
        owner.tick(&project.state);
        let run = project.run(&reopened);
        assert_eq!(run.baseline_history.len(), 2);
        assert_eq!(run.baseline_history[1].commit, history[0].moved_to);
        assert_eq!(
            run.baseline_history[1].moved_to,
            project.recorded()[1].commit
        );
        assert_eq!(paths(&delta(&run)), ["a.txt"]);
    }
}
