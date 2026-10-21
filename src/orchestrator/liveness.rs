//! Whether the agent of attached Work still runs, from what its runtime
//! records (`docs/plan-0.4.13.md` §3 C). A Claude worktree's session lock names
//! its process; Dispatch records that process once it sees it running, and
//! when it is gone, or its lock released, records the end of the sessions it
//! ran, through the same path a hook's `SessionEnd` takes. The Work then reads
//! `idle`. Nothing here finishes, applies, closes or signals anything.
//!
//! A Hermes worktree's lock, `hermes pid=<pid>`, names its process without a
//! start time: it is followed the same way, the process told apart from a
//! recycled pid by having started no later than the worktree was made.
//!
//! Removal does not stop this. Hermes removes its worktree as its process
//! exits, and Dispatch notes the removal, by its hook or by seeing the
//! worktree gone, usually before it sees the process gone. The lock went with
//! the worktree, so the recorded process is followed by its exact identity
//! alone: once it is gone, its sessions end as for any other gone process.
//!
//! Codex's and Cursor Agent's worktrees name no process (`discovery::
//! UNOBSERVED`): their liveness is `Unknown` for good, so their Work never
//! reads "agent running" and no end of their sessions is ever recorded. No
//! process is guessed at: Cursor's lingering `worker-server` is not the agent.

use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, NaiveDateTime, Utc};

use super::{attach::record_session_end, persist_event};
use crate::{
    EventRecord, LifecycleState, RunMode, RunRecord, WorkspaceOwner,
    db::Database,
    lock::OperationLock,
    process::{self, IdentityState, ProcessIdentity},
    runtime::{
        claude::{self, Liveness, Lock},
        hermes,
    },
    state::State,
};

/// One owner tick for `root`, with the runs the owner loaded: reads each
/// followed Claude worktree's lock, and at most one `ps` per lock. A run whose
/// lock is busy is left for the next tick.
pub(crate) fn observe(state: &State, root: &Path, runs: &[RunRecord]) -> Result<()> {
    let mut failed = Ok(());
    for run in runs {
        if let Err(error) = observe_run(state, root, run) {
            failed = Err(error);
        }
    }
    failed
}

fn observe_run(state: &State, root: &Path, run: &RunRecord) -> Result<()> {
    let Some(attachment) = run.attachment.as_ref() else {
        return Ok(());
    };
    // Only a workspace the runtime made: a person's attach, with or without
    // `--pid`, is theirs. Finished Work is followed while it still records its
    // agent: a session that outlives `finish`, even by a moment, still has its
    // end recorded.
    if run.mode != RunMode::Attached
        || (run.outcome.lifecycle == LifecycleState::Finished && attachment.agent_process.is_none())
        || attachment.workspace_owner != WorkspaceOwner::Runtime
    {
        return Ok(());
    }
    // A removed workspace has no lock left: only the recorded process is
    // followed, until that exact process is gone.
    if attachment.workspace_removed.is_some() {
        let Some(old) = attachment.agent_process.as_ref() else {
            return Ok(());
        };
        return match process::identity_state(old) {
            IdentityState::Gone | IdentityState::Reused => {
                end_sessions(state, run, None, "process_gone")?;
                set_agent_process(state, &run.id, Some(old), None)
            }
            IdentityState::ExactLive | IdentityState::Unknown => Ok(()),
        };
    }
    let workspace = attachment.workspace.as_path();
    let signals = claude::worktree_signals(workspace);
    // The lock's process, with its start when the lock records one.
    let session = if claude::made_by_claude(root, workspace, &signals) {
        match signals.lock {
            Some(Lock::Session { pid, start, .. }) => Some((pid, Some(start))),
            _ => None,
        }
    } else if hermes::made(root, workspace) {
        hermes::locked_pid(workspace).map(|pid| (pid, None))
    } else {
        return Ok(());
    };
    let recorded = attachment.agent_process.as_ref();
    match (recorded, session) {
        (None, None) => Ok(()),
        // Running: record it. Gone or Unknown, never seen running: nothing.
        (None, Some((pid, start))) => match running(workspace, pid, start.as_deref()) {
            Some(identity) => set_agent_process(state, &run.id, None, Some(identity)),
            None => Ok(()),
        },
        // The lock still names the recorded process: it ended if that exact
        // process is gone, as `claude -p` leaves it. Unknown proves nothing.
        (Some(old), Some((pid, start))) if pid == old.pid => {
            match lock_liveness(workspace, pid, start.as_deref()) {
                Liveness::Gone => {
                    end_sessions(state, run, None, "process_gone")?;
                    set_agent_process(state, &run.id, Some(old), None)
                }
                Liveness::Running | Liveness::Unknown => Ok(()),
            }
        }
        // Another session locked the worktree: the recorded process's lock was
        // released. Sessions that began before the new process stay its own.
        (Some(old), Some((pid, start))) => {
            if let Some(since) = start.as_deref().and_then(lock_start) {
                end_sessions(state, run, Some(since), "lock_released")?;
            }
            let new = running(workspace, pid, start.as_deref());
            set_agent_process(state, &run.id, Some(old), new)
        }
        (Some(old), None) => {
            end_sessions(state, run, None, "lock_released")?;
            set_agent_process(state, &run.id, Some(old), None)
        }
    }
}

/// Whether the process that ran `run`'s sessions is gone, by its worktree's
/// lock, for a worktree Claude Code or Hermes made in the project at `root`:
/// the lock names a process that no longer runs (a closed terminal leaves
/// the lock behind), or there is no lock, as the runtime releases it when it
/// exits. Either way, the process Dispatch recorded as the agent must not
/// run either. Anything that cannot be told, including a worktree no
/// runtime made, which has no lock to judge by, is not gone.
pub(crate) fn session_process_gone(root: &Path, run: &RunRecord) -> bool {
    let Some(attachment) = run.attachment.as_ref() else {
        return false;
    };
    let recorded_runs = attachment
        .agent_process
        .as_ref()
        .is_some_and(|agent| process::identity_state(agent) == IdentityState::ExactLive);
    let workspace = attachment.workspace.as_path();
    let signals = claude::worktree_signals(workspace);
    let lock_gone = if claude::made_by_claude(root, workspace, &signals) {
        match &signals.lock {
            None => true,
            Some(lock @ Lock::Session { .. }) => claude::liveness(lock) == Liveness::Gone,
            Some(Lock::Other(_)) => false,
        }
    } else if hermes::made(root, workspace) {
        match hermes::locked_pid(workspace) {
            Some(pid) => hermes::liveness(pid, workspace) == Liveness::Gone,
            None => signals.lock.is_none(),
        }
    } else {
        false
    };
    lock_gone && !recorded_runs
}

/// The exact identity of the process a session lock names, if it runs.
fn running(workspace: &Path, pid: u32, start: Option<&str>) -> Option<ProcessIdentity> {
    if lock_liveness(workspace, pid, start) != Liveness::Running {
        return None;
    }
    let identity = process::process_identity(pid);
    (identity.start.is_some() && identity.boot.is_some()).then_some(identity)
}

/// Whether the process a lock in `workspace` names runs: Claude's by the
/// start its lock records, Hermes's by when it made the worktree.
fn lock_liveness(workspace: &Path, pid: u32, start: Option<&str>) -> Liveness {
    match start {
        Some(start) => claude::liveness(&session_lock(pid, start)),
        None => hermes::liveness(pid, workspace),
    }
}

/// What `liveness` compares: the lock's pid and start.
fn session_lock(pid: u32, start: &str) -> Lock {
    Lock::Session {
        name: String::new(),
        pid,
        start: start.to_owned(),
    }
}

/// A lock's start, `ps -o lstart=` in UTC with single spaces.
fn lock_start(start: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(start, "%a %b %e %H:%M:%S %Y")
        .ok()
        .map(|start| start.and_utc())
}

/// End every open session that began before `before` (all of them for `None`),
/// as a hook's end would. A session a hook ended already stays as it was.
fn end_sessions(
    state: &State,
    run: &RunRecord,
    before: Option<DateTime<Utc>>,
    reason: &str,
) -> Result<()> {
    let sessions = run.attachment.iter().flat_map(|a| &a.sessions);
    for session in sessions.filter(|session| {
        session.ended_at.is_none() && before.is_none_or(|before| session.started_at < before)
    }) {
        record_session_end(
            state,
            &run.id,
            &session.provider,
            &session.session_id,
            reason,
        )?;
    }
    Ok(())
}

/// Replace the recorded agent process `old` with `new`, under the run's lock,
/// only if the record still holds `old`, with an event that keeps both.
fn set_agent_process(
    state: &State,
    run_id: &str,
    old: Option<&ProcessIdentity>,
    new: Option<ProcessIdentity>,
) -> Result<()> {
    let Ok(_lock) = OperationLock::acquire(
        &state.run_dir(run_id).join(".operation.lock"),
        "attached work is busy",
    ) else {
        return Ok(()); // busy: the next tick sees it again
    };
    let mut run = state.load_run(run_id)?;
    let Some(attachment) = run.attachment.as_mut() else {
        return Ok(());
    };
    // Finished Work takes on no new agent; it only lets go of the one it had.
    let new = new.filter(|_| run.outcome.lifecycle != LifecycleState::Finished);
    if attachment.agent_process.as_ref() != old || attachment.agent_process == new {
        return Ok(());
    }
    let event_type = if new.is_some() {
        "runtime.agent_running"
    } else {
        "runtime.agent_gone"
    };
    let payload = serde_json::json!({"agent_process": new, "previous": old});
    attachment.agent_process = new;
    let mut db = Database::open(state.db_path())?;
    db.sync_run(&run)?;
    persist_event(
        state,
        &db,
        EventRecord {
            run_id: run.id.clone(),
            candidate_label: None,
            event_type: event_type.into(),
            timestamp: Utc::now(),
            payload,
            ..EventRecord::default()
        },
        &mut run,
    )
}

#[cfg(all(test, unix))]
mod tests {
    use std::process::{Child, Command};

    use chrono::Duration;

    use super::*;
    use crate::{
        RuntimeSession,
        orchestrator::{
            attach::{self, AttachRequest, record_session_start},
            serve::load_source_runs,
            work_line,
        },
    };

    struct Project {
        _temp: tempfile::TempDir,
        state: State,
        root: std::path::PathBuf,
        locked: std::path::PathBuf,
        run_id: String,
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=T", "-c", "user.email=t@example.invalid"])
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// Attached Work in a worktree laid out as `claude --worktree` makes it,
    /// with one open session; its `locked` file is the test's to write.
    fn project() -> Project {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "--quiet", "-m", "initial"]);
        git(
            &root,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "w",
                ".claude/worktrees/w",
            ],
        );
        let root = std::fs::canonicalize(&root).unwrap();
        let gitdir = root.join(".git/worktrees/w");
        std::fs::write(gitdir.join("CLAUDE_BASE"), "base\n").unwrap();
        let state = State::discover(Some(temp.path().join("state"))).unwrap();
        state.initialize().unwrap();
        let run = attach::create(
            &state,
            AttachRequest {
                workspace: root.join(".claude/worktrees/w"),
                root: None,
                task: Some("t".into()),
                agent: Some("claude".into()),
                pid: None,
                command: None,
                allow_unsafe_local: false,
                auto_apply: false,
                runtime: None,
            },
        )
        .unwrap();
        let project = Project {
            _temp: temp,
            state,
            root,
            locked: gitdir.join("locked"),
            run_id: run.id,
        };
        // As a hook or discovery registers it: the runtime made the workspace.
        project.edit(|attachment| attachment.workspace_owner = WorkspaceOwner::Runtime);
        project.start_session("s1", Utc::now() - Duration::minutes(1));
        project
    }

    impl Project {
        fn start_session(&self, id: &str, started_at: DateTime<Utc>) {
            let session = RuntimeSession {
                provider: "claude".into(),
                session_id: id.into(),
                source: "startup".into(),
                started_at,
                ended_at: None,
                end_reason: None,
                model: None,
            };
            record_session_start(&self.state, &self.run_id, session).unwrap();
        }

        fn lock(&self, pid: u32, start: &str) {
            std::fs::write(
                &self.locked,
                format!("claude session w (pid {pid} start {start})"),
            )
            .unwrap();
        }

        fn lock_child(&self, child: &Child) {
            self.lock(child.id(), &utc_start(child.id()));
        }

        fn tick(&self) {
            let runs = load_source_runs(&self.state, &self.root).unwrap();
            observe(&self.state, &self.root, &runs).unwrap();
        }

        fn run(&self) -> RunRecord {
            self.state.load_run(&self.run_id).unwrap()
        }

        fn agent(&self) -> Option<ProcessIdentity> {
            self.run().attachment.unwrap().agent_process
        }

        /// As `finish` leaves it: finished, its recorded agent kept.
        fn finish(&self) {
            let mut run = self.run();
            run.outcome.lifecycle = LifecycleState::Finished;
            let mut db = Database::open(self.state.db_path()).unwrap();
            db.sync_run(&run).unwrap();
            self.state.save_run(&run).unwrap();
        }

        fn edit(&self, change: impl FnOnce(&mut crate::AttachmentRecord)) {
            let mut run = self.run();
            change(run.attachment.as_mut().unwrap());
            let mut db = Database::open(self.state.db_path()).unwrap();
            db.sync_run(&run).unwrap();
            self.state.save_run(&run).unwrap();
        }

        /// Each session's id and end reason, oldest first.
        fn sessions(&self) -> Vec<(String, Option<String>)> {
            let run = self.run();
            run.attachment
                .unwrap()
                .sessions
                .into_iter()
                .map(|s| {
                    assert_eq!(s.ended_at.is_some(), s.end_reason.is_some());
                    (s.session_id, s.end_reason)
                })
                .collect()
        }

        fn events(&self, kind: &str) -> usize {
            Database::open_read_only(self.state.db_path())
                .unwrap()
                .events_for_run(&self.run_id)
                .unwrap()
                .iter()
                .filter(|event| event.event_type == kind)
                .count()
        }
    }

    fn sleeper() -> Child {
        Command::new("sleep").arg("600").spawn().unwrap()
    }

    /// Kill and reap: a zombie still answers signal zero.
    fn stop(mut child: Child) {
        child.kill().unwrap();
        child.wait().unwrap();
    }

    fn utc_start(pid: u32) -> String {
        let output = Command::new("ps")
            .env("TZ", "UTC0")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn ended(reason: &str) -> Option<String> {
        Some(reason.into())
    }

    #[test]
    fn a_running_lock_records_the_agent_once() {
        let p = project();
        let child = sleeper();
        p.lock_child(&child);
        p.tick();
        p.tick();
        let agent = p.agent().expect("the running agent is recorded");
        assert_eq!(agent.pid, child.id());
        assert_eq!(
            process::identity_state(&agent),
            process::IdentityState::ExactLive
        );
        assert_eq!(p.events("runtime.agent_running"), 1);
        assert_eq!(p.sessions(), [("s1".into(), None)]);
        stop(child);
    }

    #[test]
    fn a_lock_left_with_a_dead_pid_ends_the_session_and_the_work_is_idle() {
        let p = project();
        let child = sleeper();
        p.lock_child(&child);
        p.tick();
        stop(child);
        p.tick();
        assert_eq!(p.sessions(), [("s1".into(), ended("process_gone"))]);
        assert_eq!(p.agent(), None);
        let run = p.run();
        assert_ne!(run.outcome.lifecycle, LifecycleState::Finished);
        assert_eq!(work_line(&run, None).state, "idle");
        assert_eq!(p.events("runtime.session_ended"), 1);
        // A session that starts later, without a lock, is not ended by the
        // process that went before it.
        p.start_session("s2", Utc::now());
        p.tick();
        assert_eq!(p.sessions()[1], ("s2".into(), None));
    }

    #[test]
    fn a_released_lock_after_an_exact_identity_ends_the_session() {
        let p = project();
        let child = sleeper();
        p.lock_child(&child);
        p.tick();
        stop(child);
        std::fs::remove_file(&p.locked).unwrap();
        p.tick();
        assert_eq!(p.sessions(), [("s1".into(), ended("lock_released"))]);
        assert_eq!(work_line(&p.run(), None).state, "idle");
    }

    #[test]
    fn never_seen_running_changes_nothing() {
        let p = project();
        // The lock names a process that already ended.
        let child = sleeper();
        let (pid, start) = (child.id(), utc_start(child.id()));
        stop(child);
        p.lock(pid, &start);
        p.tick();
        assert_eq!(p.agent(), None);
        assert_eq!(p.sessions(), [("s1".into(), None)]);
        std::fs::remove_file(&p.locked).unwrap();
        p.tick();
        assert_eq!(p.sessions(), [("s1".into(), None)]);
        assert_eq!(p.events("runtime.session_ended"), 0);
    }

    #[test]
    fn a_persons_attach_with_a_pid_is_never_touched() {
        let p = project();
        let child = sleeper();
        let identity = process::process_identity(child.id());
        p.edit(|attachment| {
            attachment.workspace_owner = WorkspaceOwner::User;
            attachment.agent_process = Some(identity.clone());
        });
        p.tick(); // no lock
        p.lock_child(&child);
        stop(child);
        p.tick(); // a lock naming it, gone
        assert_eq!(p.agent(), Some(identity));
        assert_eq!(p.sessions(), [("s1".into(), None)]);
    }

    #[test]
    fn a_pid_another_process_now_has_counts_as_gone() {
        let p = project();
        let child = sleeper();
        let Some(identity) = running(&p.locked, child.id(), Some(&utc_start(child.id()))) else {
            stop(child);
            return; // no exact identity on this platform
        };
        p.edit(|attachment| attachment.agent_process = Some(identity));
        // The lock's process had this pid, but started at another time.
        p.lock(child.id(), "Thu Jan 1 00:00:00 1970");
        p.tick();
        assert_eq!(p.sessions(), [("s1".into(), ended("process_gone"))]);
        assert_eq!(p.agent(), None);
        stop(child);
    }

    #[test]
    fn a_hook_end_before_or_after_gives_one_end() {
        // Before: the hook's end stands.
        let p = project();
        let child = sleeper();
        p.lock_child(&child);
        p.tick();
        record_session_end(&p.state, &p.run_id, "claude", "s1", "prompt_input_exit").unwrap();
        stop(child);
        p.tick();
        assert_eq!(p.sessions(), [("s1".into(), ended("prompt_input_exit"))]);
        assert_eq!(p.events("runtime.session_ended"), 1);

        // After: the observed end stands.
        let p = project();
        let child = sleeper();
        p.lock_child(&child);
        p.tick();
        stop(child);
        p.tick();
        record_session_end(&p.state, &p.run_id, "claude", "s1", "other").unwrap();
        assert_eq!(p.sessions(), [("s1".into(), ended("process_gone"))]);
        assert_eq!(p.events("runtime.session_ended"), 1);
    }

    #[test]
    fn a_resumed_sessions_new_lock_keeps_the_new_session_open() {
        let p = project();
        let first = sleeper();
        p.lock_child(&first);
        p.tick();
        stop(first);
        // The resume starts a new process, which locks the worktree again,
        // before the owner's next tick.
        let second = sleeper();
        p.lock_child(&second);
        p.start_session("s2", Utc::now());
        p.tick();
        assert_eq!(
            p.sessions(),
            [("s1".into(), ended("lock_released")), ("s2".into(), None)]
        );
        assert_eq!(p.agent().map(|agent| agent.pid), Some(second.id()));
        assert_eq!(work_line(&p.run(), None).state, "working");
        stop(second);
        p.tick();
        assert_eq!(p.sessions()[1], ("s2".into(), ended("process_gone")));
    }

    /// Found in the 0.4.14 trials: `dispatch finish` run moments after a
    /// discovered `claude -p` session exited, before liveness looked again,
    /// left that session open for good. Its end is recorded whichever comes
    /// first, the finish or the process's end.
    #[test]
    fn a_session_that_ends_after_or_just_before_finish_still_ends() {
        // Finished while the agent runs; it ends later.
        let p = project();
        let child = sleeper();
        p.lock_child(&child);
        p.tick();
        p.finish();
        p.tick();
        assert_eq!(p.sessions(), [("s1".into(), None)]);
        assert_eq!(p.agent().map(|agent| agent.pid), Some(child.id()));
        stop(child);
        p.tick();
        assert_eq!(p.sessions(), [("s1".into(), ended("process_gone"))]);
        assert_eq!(p.agent(), None);
        assert_eq!(p.events("runtime.session_ended"), 1);
        p.tick();
        assert_eq!(p.events("runtime.session_ended"), 1);

        // It ended, then was finished before liveness looked again.
        let p = project();
        let child = sleeper();
        p.lock_child(&child);
        p.tick();
        stop(child);
        p.finish();
        p.tick();
        assert_eq!(p.sessions(), [("s1".into(), ended("process_gone"))]);
        assert_eq!(p.agent(), None);
        assert_eq!(p.run().outcome.lifecycle, LifecycleState::Finished);
    }

    /// Finished Work never takes on a new agent: a later session's lock only
    /// ends the sessions that began before it.
    #[test]
    fn finished_work_takes_on_no_new_agent() {
        let p = project();
        let first = sleeper();
        p.lock_child(&first);
        p.tick();
        p.finish();
        let second = sleeper();
        p.lock_child(&second);
        p.tick();
        assert_eq!(p.sessions(), [("s1".into(), ended("lock_released"))]);
        assert_eq!(p.agent(), None);
        assert_eq!(p.events("runtime.agent_running"), 1);
        stop(first);
        stop(second);
    }

    #[test]
    fn a_lock_start_reads_as_utc() {
        assert_eq!(
            lock_start("Sat Oct 3 09:00:00 2026").unwrap().to_rfc3339(),
            "2026-10-03T09:00:00+00:00"
        );
        assert_eq!(
            lock_start("Tue Oct 13 14:08:28 2026").unwrap().to_rfc3339(),
            "2026-10-13T14:08:28+00:00"
        );
        assert_eq!(lock_start("not a time"), None);
    }
}
