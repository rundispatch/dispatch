//! Mid-run coherence watcher for allocation runs.
//!
//! While an agent works, this task notices that the source moved, evaluates
//! the work so far (L0 and L1, no integration checks) and reports verdicts to
//! the run's owning loop over a channel. The watcher never touches the
//! database or the run record: the owner applies each message and persists it,
//! so there is exactly one writer of the run's state revision.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::Result;
use sha2::{Digest, Sha256};
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{
    Decision, ReasonCode, SourceKind, Validity,
    coherence::{
        WorkView, evaluate,
        world::{Signal, observe, signal},
    },
    executor::CancellationToken,
    source::{snapshot_delta, snapshot_delta_if_changed_indexed},
};

/// The fewest seconds between two messages from one watcher, so a person
/// editing continuously cannot make the run's revision churn.
const MIN_MESSAGE_GAP: Duration = Duration::from_secs(60);

/// A verdict for the owning loop to apply to the run.
#[derive(Debug)]
pub struct WatchMsg {
    pub validity: Validity,
}

/// Everything the watcher needs about one running attempt.
#[derive(Debug, Clone)]
pub struct WatchSpec {
    pub source: PathBuf,
    pub kind: SourceKind,
    pub baseline: PathBuf,
    pub baseline_commit: String,
    /// The live candidate workspace whose work so far is evaluated.
    pub workspace: PathBuf,
    /// Overwritten at each evaluation; must live outside `workspace`.
    pub delta_patch: PathBuf,
    pub poll: Duration,
    /// The verdicts that stop this attempt (`CoherenceConfig::stopping`).
    pub stopping: Vec<Decision>,
}

/// A running watcher. It stops when `finish` is awaited or when it is dropped,
/// so it cannot outlive the attempt it was started for.
pub struct Watcher {
    pub rx: mpsc::UnboundedReceiver<WatchMsg>,
    stop: CancellationToken,
    task: JoinHandle<()>,
}

impl Watcher {
    pub fn spawn(spec: WatchSpec) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let stop = CancellationToken::new();
        let task = tokio::spawn(watch(spec, tx, stop.clone()));
        Self { rx, stop, task }
    }

    /// Stop the watcher and wait for its task, including any evaluation that
    /// is in flight, to end.
    pub async fn finish(&mut self) {
        self.stop.cancel();
        let _ = (&mut self.task).await;
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.cancel();
        self.task.abort();
    }
}

async fn watch(spec: WatchSpec, tx: mpsc::UnboundedSender<WatchMsg>, stop: CancellationToken) {
    let mut policy = Policy {
        stopping: spec.stopping.clone(),
        ..Policy::default()
    };
    let mut last: Option<Signals> = None;
    // The work's index, kept between checks so Git re-reads only what
    // changed; without one, every check stages the whole workspace.
    let index_dir = tempfile::Builder::new()
        .prefix("dispatch-watch-")
        .tempdir()
        .ok();
    let index = index_dir.as_ref().map(|dir| dir.path().join("index"));
    let mut ticker = tokio::time::interval(spec.poll);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = stop.cancelled() => return,
        }
        let blocking_spec = spec.clone();
        let blocking_last = last.clone();
        let blocking_index = index.clone();
        let checked = tokio::task::spawn_blocking(move || {
            check(&blocking_spec, blocking_index.as_deref(), blocking_last)
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|result| result);
        let evaluated = match checked {
            Ok((now, evaluated)) => {
                last = Some(now);
                evaluated
            }
            Err(error) => {
                // A tree that is mid-edit can fail to read; try again at the
                // next tick. Never fail the run because of the watcher.
                tracing::debug!("coherence watcher check failed: {error:#}");
                None
            }
        };
        if let Some(validity) = policy.step(Instant::now(), evaluated)
            && tx.send(WatchMsg { validity }).is_err()
        {
            return;
        }
    }
}

/// The world's signal and the work's: a verdict depends on both, so either
/// moving is a reason to evaluate again.
type Signals = (Signal, Signal);

/// Cheap signals first; only when the world or the work so far moved, observe
/// the world and evaluate. The work's signal is a hash of the work-in-progress
/// patch, taken with the trusted baseline repository, so it honors the ignore
/// rules (build output never counts) and never runs the workspace's own Git
/// configuration. Following the work too means a change that landed before
/// the agent touched the same code is still seen once the agent touches it.
/// With `index`, the patch is taken again only when the workspace changed
/// since that index, as the project owner follows Work.
fn check(
    spec: &WatchSpec,
    index: Option<&Path>,
    previous: Option<Signals>,
) -> Result<(Signals, Option<Validity>)> {
    let world_signal = signal(&spec.source, &spec.kind)?;
    match index {
        Some(index) => {
            snapshot_delta_if_changed_indexed(
                &spec.baseline,
                &spec.workspace,
                &spec.delta_patch,
                index,
                &spec.delta_patch,
            )?;
        }
        None => snapshot_delta(&spec.baseline, &spec.workspace, &spec.delta_patch)?,
    }
    let work_signal = Signal(hex::encode(Sha256::digest(fs::read(&spec.delta_patch)?)));
    let now = (world_signal, work_signal);
    if previous.as_ref() == Some(&now) {
        return Ok((now, None));
    }
    let world = observe(
        &spec.source,
        &spec.baseline,
        &spec.baseline_commit,
        &spec.kind,
    )?;
    let validity = evaluate(
        &world,
        &WorkView {
            source: &spec.source,
            delta_patch: &spec.delta_patch,
            baseline: &spec.baseline,
            baseline_commit: &spec.baseline_commit,
        },
    )?;
    Ok((now, Some(validity)))
}

/// Parse errors while a person is mid-edit are normal, so uncertainty must not
/// invalidate work mid-run. Drop `AnalysisUncertain` reasons; a refresh that
/// rested only on them is a continue.
pub(crate) fn settle(mut validity: Validity) -> Validity {
    validity
        .reasons
        .retain(|reason| reason.code != ReasonCode::AnalysisUncertain);
    if validity.decision == Decision::Refresh && validity.reasons.is_empty() {
        validity.decision = Decision::Continue;
    }
    validity
}

/// Whether `validity` says something `last` did not: a run is taken to be
/// valid until a verdict says otherwise, so a verdict is worth recording when
/// its decision differs from the last one, or when it is not `Continue` and
/// the world it describes is not the world the last one described. Shared by
/// the watcher's messages and the project owner's stored verdicts.
pub(crate) fn worth_recording(last: Option<&Validity>, validity: &Validity) -> bool {
    let last_decision = last.map_or(Decision::Continue, |v| v.decision);
    validity.decision != last_decision
        || (validity.decision != Decision::Continue
            && last.is_none_or(|last| last.world_digest != validity.world_digest))
}

/// Decides which verdicts become messages to a running agent: those
/// `worth_recording` against the last message sent. At most one message goes
/// out per `MIN_MESSAGE_GAP`; a verdict held back by that limit is sent later
/// unless a newer evaluation supersedes it. A verdict that stops the attempt
/// is never held back: it ends the attempt, so it cannot churn the run.
#[derive(Default)]
pub(crate) struct Policy {
    /// The decisions that stop the attempt.
    stopping: Vec<Decision>,
    sent: Option<Validity>,
    sent_at: Option<Instant>,
    pending: Option<Validity>,
}

impl Policy {
    /// One tick: `evaluated` is the verdict computed at `now`, or `None` when
    /// nothing was evaluated. Returns the message to send, if any.
    pub(crate) fn step(&mut self, now: Instant, evaluated: Option<Validity>) -> Option<Validity> {
        if let Some(validity) = evaluated {
            let validity = settle(validity);
            self.pending = self.worth_sending(&validity).then_some(validity);
        }
        if let Some(pending) = &self.pending
            && (self.stopping.contains(&pending.decision)
                || self
                    .sent_at
                    .is_none_or(|at| now.saturating_duration_since(at) >= MIN_MESSAGE_GAP))
        {
            self.sent_at = Some(now);
            self.sent = self.pending.clone();
            return self.pending.take();
        }
        None
    }

    fn worth_sending(&self, validity: &Validity) -> bool {
        worth_recording(self.sent.as_ref(), validity)
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::{
        AnalysisLevel, Reason,
        source::{create_candidate_workspace, create_snapshot},
    };

    #[cfg(unix)]
    #[test]
    fn the_work_is_captured_again_only_when_the_workspace_changed() {
        use std::os::unix::fs::MetadataExt;
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("a.txt"), "one\n").unwrap();
        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        let workspace =
            create_candidate_workspace(&snapshot.baseline_path, &temp.path().join("work")).unwrap();
        let spec = WatchSpec {
            source: fs::canonicalize(&source).unwrap(),
            kind: snapshot.kind.clone(),
            baseline: snapshot.baseline_path.clone(),
            baseline_commit: snapshot.baseline_commit.clone(),
            workspace: workspace.clone(),
            delta_patch: temp.path().join("delta-live.patch"),
            poll: Duration::from_secs(1),
            stopping: Vec::new(),
        };
        let index = temp.path().join("index");
        fs::write(workspace.join("a.txt"), "two\n").unwrap();
        let (first, evaluated) = check(&spec, Some(&index), None).unwrap();
        assert!(evaluated.is_some());
        let captured = fs::metadata(&spec.delta_patch).unwrap().ino();

        let (again, evaluated) = check(&spec, Some(&index), Some(first.clone())).unwrap();
        assert_eq!(again, first);
        assert!(evaluated.is_none());
        assert_eq!(
            fs::metadata(&spec.delta_patch).unwrap().ino(),
            captured,
            "an unchanged workspace is not captured again"
        );

        fs::write(workspace.join("b.txt"), "new\n").unwrap();
        let (moved, evaluated) = check(&spec, Some(&index), Some(again)).unwrap();
        assert_ne!(moved.1, first.1);
        assert!(evaluated.is_some());
        let patch = fs::read_to_string(&spec.delta_patch).unwrap();
        assert!(patch.contains("+two") && patch.contains("+new"), "{patch}");
    }

    fn reason(code: ReasonCode) -> Reason {
        Reason {
            code,
            fact_id: None,
            path: None,
            detail: format!("{code:?}"),
        }
    }

    fn verdict(decision: Decision, digest: &str, codes: &[ReasonCode]) -> Validity {
        Validity {
            decision,
            evaluated_at: Utc::now(),
            world_digest: digest.into(),
            world_changed: true,
            changed_files: 1,
            reasons: codes.iter().copied().map(reason).collect(),
            analysis: AnalysisLevel::FilesOnly,
        }
    }

    fn refresh(digest: &str) -> Validity {
        verdict(Decision::Refresh, digest, &[ReasonCode::PatchConflict])
    }

    fn cont(digest: &str) -> Validity {
        verdict(Decision::Continue, digest, &[])
    }

    fn at(base: Instant, seconds: u64) -> Instant {
        base + Duration::from_secs(seconds)
    }

    #[test]
    fn continue_while_nothing_was_sent_stays_silent() {
        let base = Instant::now();
        let mut policy = Policy::default();
        assert!(policy.step(at(base, 0), Some(cont("w1"))).is_none());
        assert!(policy.step(at(base, 10), None).is_none());
        assert!(policy.step(at(base, 20), Some(cont("w2"))).is_none());
    }

    #[test]
    fn first_invalid_verdict_is_sent_immediately_once() {
        let base = Instant::now();
        let mut policy = Policy::default();
        let sent = policy.step(at(base, 0), Some(refresh("w1"))).unwrap();
        assert_eq!(sent.decision, Decision::Refresh);
        // The same world is not announced again, however often it is checked.
        assert!(policy.step(at(base, 100), Some(refresh("w1"))).is_none());
        assert!(policy.step(at(base, 200), None).is_none());
    }

    #[test]
    fn a_new_world_that_is_still_invalid_is_sent_again() {
        let base = Instant::now();
        let mut policy = Policy::default();
        assert!(policy.step(at(base, 0), Some(refresh("w1"))).is_some());
        assert!(policy.step(at(base, 61), Some(refresh("w2"))).is_some());
    }

    #[test]
    fn returning_to_continue_is_sent_after_the_gap() {
        let base = Instant::now();
        let mut policy = Policy::default();
        assert!(policy.step(at(base, 0), Some(refresh("w1"))).is_some());
        let back = policy.step(at(base, 61), Some(cont("w2"))).unwrap();
        assert_eq!(back.decision, Decision::Continue);
        assert!(policy.step(at(base, 200), Some(cont("w3"))).is_none());
    }

    #[test]
    fn messages_are_at_most_one_per_minute_and_held_verdicts_are_sent_later() {
        let base = Instant::now();
        let mut policy = Policy::default();
        assert!(policy.step(at(base, 0), Some(refresh("w1"))).is_some());
        // Back to continue within the minute: held back, not lost.
        assert!(policy.step(at(base, 10), Some(cont("w2"))).is_none());
        assert!(policy.step(at(base, 59), None).is_none());
        let held = policy.step(at(base, 60), None).unwrap();
        assert_eq!(held.decision, Decision::Continue);
        assert!(policy.step(at(base, 200), None).is_none());
    }

    #[test]
    fn a_held_verdict_that_no_longer_differs_is_dropped() {
        let base = Instant::now();
        let mut policy = Policy::default();
        assert!(policy.step(at(base, 0), Some(refresh("w1"))).is_some());
        assert!(policy.step(at(base, 10), Some(cont("w2"))).is_none());
        // The edit was reverted to the state already reported: nothing to say.
        assert!(policy.step(at(base, 20), Some(refresh("w1"))).is_none());
        assert!(policy.step(at(base, 200), None).is_none());
    }

    #[test]
    fn uncertain_reasons_never_turn_continue_into_refresh() {
        let base = Instant::now();
        let mut policy = Policy::default();
        let uncertain = verdict(
            Decision::Refresh,
            "w1",
            &[ReasonCode::AnalysisUncertain, ReasonCode::AnalysisUncertain],
        );
        assert!(policy.step(at(base, 0), Some(uncertain)).is_none());
        // Real reasons survive the filter; the uncertain ones are removed.
        let mixed = verdict(
            Decision::Refresh,
            "w2",
            &[ReasonCode::AnalysisUncertain, ReasonCode::FactBroken],
        );
        let sent = policy.step(at(base, 1), Some(mixed)).unwrap();
        assert_eq!(sent.decision, Decision::Refresh);
        assert_eq!(sent.reasons.len(), 1);
        assert_eq!(sent.reasons[0].code, ReasonCode::FactBroken);
    }

    fn unreadable_spec(root: &std::path::Path) -> WatchSpec {
        WatchSpec {
            source: root.join("missing-source"),
            kind: SourceKind::Directory,
            baseline: root.join("missing-baseline"),
            baseline_commit: "0".repeat(40),
            workspace: root.join("missing-workspace"),
            delta_patch: root.join("delta-live.patch"),
            poll: Duration::from_millis(20),
            stopping: Vec::new(),
        }
    }

    #[tokio::test]
    async fn finish_ends_the_task_and_closes_the_channel() {
        let root = tempfile::tempdir().unwrap();
        let mut watcher = Watcher::spawn(unreadable_spec(root.path()));
        // Failed evaluations are skipped: the watcher keeps ticking, silently.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(watcher.rx.try_recv().is_err());
        watcher.finish().await;
        assert!(watcher.task.is_finished());
        // The sender died with the task, so nothing is left to send.
        assert!(watcher.rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn dropping_the_watcher_aborts_its_task() {
        let root = tempfile::tempdir().unwrap();
        let watcher = Watcher::spawn(unreadable_spec(root.path()));
        let task = watcher.task.abort_handle();
        drop(watcher);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(task.is_finished());
    }

    #[test]
    fn stop_after_refresh_is_a_decision_change() {
        let base = Instant::now();
        let mut policy = Policy::default();
        assert!(policy.step(at(base, 0), Some(refresh("w1"))).is_some());
        let stop = verdict(Decision::Stop, "w1", &[ReasonCode::AlreadyApplied]);
        assert!(policy.step(at(base, 30), Some(stop.clone())).is_none());
        let sent = policy.step(at(base, 60), None).unwrap();
        assert_eq!(sent.decision, Decision::Stop);
    }

    /// A source file read while it is being written can give a refresh that
    /// does not stop the attempt; the stop that follows once the write ends
    /// is sent at once, not a minute later.
    #[test]
    fn a_verdict_that_stops_the_attempt_is_never_held_back() {
        let base = Instant::now();
        let mut policy = Policy {
            stopping: vec![Decision::Stop],
            ..Policy::default()
        };
        assert!(policy.step(at(base, 0), Some(refresh("w1"))).is_some());
        let stop = verdict(Decision::Stop, "w2", &[ReasonCode::AlreadyApplied]);
        let sent = policy.step(at(base, 1), Some(stop)).unwrap();
        assert_eq!(sent.decision, Decision::Stop);
        // What does not stop it still waits for the gap.
        assert!(policy.step(at(base, 2), Some(cont("w3"))).is_none());
        assert!(policy.step(at(base, 61), None).is_some());
    }
}
