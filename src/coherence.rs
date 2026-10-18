//! Work coherence: is a run's finished work still valid against a source tree
//! that may have moved since the run's baseline was taken?
//!
//! This is the pure evaluator: L0 (file and patch state) then L1 (symbol and
//! file facts, see `facts`). It reads the source tree, the candidate patch and
//! the run's baseline, and runs `git apply` only with `--check`, so it never
//! mutates the source, and it never touches the database or the run record.
//! The one file it may write is Git's complaint about a conflicting patch,
//! beside the run, when finish or accept asks (`evaluate_run`).

use std::{fmt, fs, future::Future, path::Path};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;

use crate::{
    AnalysisLevel, ApplicationState, CandidateRecord, CoherenceRecord, Decision, LifecycleState,
    Reason, ReasonCode, RunRecord, RunStatus, Validity, WorkResult,
    coherence::{
        integration::{ChecksSkipped, MergedChecks},
        world::WorldObservation,
    },
    config::Config,
    source::{git_command, run_git},
};

pub mod facts;
pub mod integration;
pub mod interactions;
pub mod measure;
pub mod memo;
pub mod symbols;
pub mod watch;
pub mod world;

const MAX_REASONS: usize = 20;
const MAX_DETAIL_CHARS: usize = 300;
/// Beside the run: Git's whole complaint about a patch that no longer
/// applies, as finish or accept last found it.
pub const CONFLICT_OUTPUT: &str = "patch-conflict.txt";

/// What the evaluator needs to know about the work: where the source lives,
/// the patch (the work's delta against its baseline), and the run's private
/// baseline repository and commit (S0).
pub struct WorkView<'a> {
    pub source: &'a Path,
    pub delta_patch: &'a Path,
    pub baseline: &'a Path,
    pub baseline_commit: &'a str,
}

impl<'a> WorkView<'a> {
    fn of(run: &'a RunRecord, candidate: &'a CandidateRecord) -> Self {
        Self {
            source: &run.source_path,
            delta_patch: &candidate.diff_path,
            baseline: &run.baseline_path,
            baseline_commit: &run.baseline_commit,
        }
    }
}

/// Observe the current source tree and evaluate one candidate's patch of `run`.
/// With `keep` (the run's directory, for finish and accept), a patch that no
/// longer applies leaves Git's whole complaint there (`CONFLICT_OUTPUT`) and
/// its reason says so; without it nothing is written.
pub fn evaluate_run(
    run: &RunRecord,
    candidate_label: &str,
    keep: Option<&Path>,
) -> Result<Validity> {
    evaluate_run_in(
        run,
        candidate_label,
        keep,
        &mut world::Worlds::default(),
        &facts::Memo::default(),
    )
}

/// `evaluate_run`, observing through `worlds` and deriving through `memo`.
fn evaluate_run_in(
    run: &RunRecord,
    candidate_label: &str,
    keep: Option<&Path>,
    worlds: &mut world::Worlds,
    memo: &facts::Memo,
) -> Result<Validity> {
    let candidate = find_candidate(run, candidate_label)?;
    let world = worlds.observe(
        &run.source_path,
        &run.baseline_path,
        &run.baseline_commit,
        &run.source_kind,
    )?;
    let kept = keep.map(|run_dir| run_dir.join(CONFLICT_OUTPUT));
    let (validity, conflict) =
        assess(&world, &WorkView::of(run, candidate), kept.as_deref(), memo)?;
    if let (Some(path), Some(output)) = (&kept, conflict) {
        fs::write(path, output).with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(validity)
}

/// The verdict `check`, `status`, `explain` and `refresh` show: a fresh file
/// and symbol evaluation, unless accept already refused this result on the
/// merged tree of this same world. Only accept runs the integration checks,
/// so a fresh evaluation of an unmoved world must not hide that refusal. Once
/// the world moves, the fresh verdict stands. The accept gate itself always
/// evaluates afresh, so a retry runs the checks again. `keep` as for
/// `evaluate_run`.
pub fn shown_validity(
    run: &RunRecord,
    candidate_label: &str,
    keep: Option<&Path>,
) -> Result<Validity> {
    shown_validity_in(
        run,
        candidate_label,
        keep,
        &mut world::Worlds::default(),
        &facts::Memo::default(),
    )
}

fn shown_validity_in(
    run: &RunRecord,
    candidate_label: &str,
    keep: Option<&Path>,
    worlds: &mut world::Worlds,
    memo: &facts::Memo,
) -> Result<Validity> {
    let live = evaluate_run_in(run, candidate_label, keep, worlds, memo)?;
    Ok(
        match run.coherence.as_ref().and_then(|c| c.validity.as_ref()) {
            Some(stored)
                if stored.decision != Decision::Continue
                    && stored.world_digest == live.world_digest
                    && stored
                        .reasons
                        .iter()
                        .any(integration::is_integration_reason) =>
            {
                stored.clone()
            }
            _ => live,
        },
    )
}

/// A finished, delivered result that has not been applied. Runs created before
/// the outcome model existed migrate with `work_result: pending`, so a
/// `ready_for_evaluation` status (what `apply` itself requires) also counts.
pub fn is_ready_unapplied(run: &RunRecord) -> bool {
    let delivered = (run.outcome.lifecycle == LifecycleState::Finished
        && run.outcome.work_result == WorkResult::Ready)
        || (matches!(
            run.status,
            RunStatus::ReadyForEvaluation | RunStatus::Evaluated
        ) && run.outcome.work_result == WorkResult::Pending);
    delivered
        && run.outcome.application != ApplicationState::Applied
        && run.status != RunStatus::Applied
}

/// The shown verdict (`shown_validity`) for a finished result that has not
/// been applied and has exactly one candidate; `None` for any other run, or
/// when evaluation fails. Reads only; nothing is written.
pub fn live_validity(run: &RunRecord) -> Option<Validity> {
    live(
        run,
        None,
        &mut world::Worlds::default(),
        &facts::Memo::default(),
    )
}

/// `live_validity`, observing through `worlds` and deriving through `memo`:
/// the project owner's, shared by every Work it evaluates.
pub fn live_validity_in(
    run: &RunRecord,
    worlds: &mut world::Worlds,
    memo: &facts::Memo,
) -> Option<Validity> {
    live(run, None, worlds, memo)
}

/// `live_validity`, with `keep` as for `evaluate_run`.
fn live(
    run: &RunRecord,
    keep: Option<&Path>,
    worlds: &mut world::Worlds,
    memo: &facts::Memo,
) -> Option<Validity> {
    if !is_ready_unapplied(run) {
        return None;
    }
    let [candidate] = run.candidates.as_slice() else {
        return None;
    };
    match shown_validity_in(run, &candidate.label, keep, worlds, memo) {
        Ok(validity) => Some(validity),
        Err(error) => {
            tracing::debug!("coherence evaluation of run {} failed: {error:#}", run.id);
            None
        }
    }
}

/// A copy of `run` whose stored validity is replaced by the live one when there
/// is one, for pure renderers. The copy is for display and is never persisted.
pub fn with_live_validity(run: &RunRecord) -> RunRecord {
    shown_with(run, live_validity(run))
}

/// `with_live_validity` for finish and the end of a native run: a patch that
/// no longer applies leaves Git's whole complaint beside the run (`run_dir`).
pub fn with_kept_validity(run: &RunRecord, run_dir: &Path) -> RunRecord {
    let validity = live(
        run,
        Some(run_dir),
        &mut world::Worlds::default(),
        &facts::Memo::default(),
    );
    shown_with(run, validity)
}

fn shown_with(run: &RunRecord, live: Option<Validity>) -> RunRecord {
    let mut shown = run.clone();
    if let Some(validity) = live {
        let record = shown.coherence.get_or_insert(CoherenceRecord {
            version: 1,
            refreshed_from: None,
            validity: None,
            first_invalid_at: None,
            overridden: None,
        });
        record.validity = Some(validity);
    }
    shown
}

fn find_candidate<'a>(run: &'a RunRecord, candidate_label: &str) -> Result<&'a CandidateRecord> {
    let matches = run
        .candidates
        .iter()
        .filter(|candidate| candidate.label == candidate_label)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [candidate] => Ok(*candidate),
        [] => bail!("candidate not found: {candidate_label}"),
        _ => bail!("candidate label is ambiguous: {candidate_label}"),
    }
}

/// The word for a decision in every human-facing verdict.
pub fn verdict(decision: Decision) -> &'static str {
    match decision {
        Decision::Continue => "CONTINUE",
        Decision::Refresh => "REFRESH",
        Decision::Stop => "STOP",
    }
}

/// The typed refusal returned when accepted work is no longer valid against the
/// moved source. It replaces matching on error text to recognize drift.
#[derive(Debug)]
pub struct CoherenceBlocked {
    pub run_id: String,
    pub validity: Validity,
    /// Attached work has no Dispatch task to refresh.
    pub attached: bool,
    /// The human asked to override (`--despite-refresh`).
    pub despite_refresh: bool,
    /// For a STOP, the applied run that already landed the same patch.
    pub landed_by: Option<String>,
    /// What became of the merged checks; `None` when the verdict refused the
    /// work before them.
    pub checks: Option<MergedChecks>,
}

impl fmt::Display for CoherenceBlocked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let verdict = match self.validity.decision {
            Decision::Stop => "STOP",
            _ => "REFRESH",
        };
        write!(f, "source has changed; this work is stale ({verdict})")?;
        for reason in &self.validity.reasons {
            write!(f, ": {}", reason.detail)?;
        }
        if let Some(id) = &self.landed_by {
            write!(f, " (landed by run {id})")?;
        }
        write!(f, ". The source was left unchanged. ")?;
        match &self.checks {
            // The reasons name the failing check and its logs.
            Some(MergedChecks::Failed(_)) => {}
            Some(checks) => write!(f, "Merged checks: {checks}. ")?,
            None => write!(
                f,
                "Merged checks: not run — accept refuses {verdict} work before them. "
            )?,
        }
        if self.validity.decision == Decision::Stop {
            write!(
                f,
                "Run 'dispatch close {} --reason already-present'.",
                self.run_id
            )?;
        } else if self.attached {
            write!(
                f,
                "Run your agent again on the current source and attach it, or run 'dispatch reject {}'.",
                self.run_id
            )?;
        } else {
            write!(
                f,
                "Run 'dispatch refresh {id}' to redo the work on the current source, or 'dispatch reject {id}'.",
                id = self.run_id
            )?;
        }
        if self.despite_refresh {
            if self
                .validity
                .reasons
                .iter()
                .any(integration::is_integration_reason)
            {
                write!(
                    f,
                    " The override was refused: it needs the checks to pass on the merged tree, and they did not."
                )?;
            } else if !overridable(&self.validity) {
                write!(
                    f,
                    " --despite-refresh covers only a REFRESH whose every reason comes from the analysis (fact_broken, fact_missing, same_symbol_edited, analysis_uncertain); this one cannot be overridden."
                )?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for CoherenceBlocked {}

/// What acceptance may do about a possibly moved source, and what became of
/// the merged checks.
pub enum AcceptGate {
    /// Strict mode, or the whole tree is byte-identical to the snapshot: the
    /// existing all-or-nothing fingerprint rules apply unchanged, and the
    /// merged checks do not run, for this reason.
    Legacy(ChecksSkipped),
    /// The work is still valid against the world as it is; apply against it.
    Compatible {
        validity: Validity,
        checks: MergedChecks,
    },
    /// The work is stale; nothing may be accepted or applied. `checks` is
    /// `None` when the verdict refused it before them.
    Blocked {
        validity: Validity,
        checks: Option<MergedChecks>,
    },
    /// A human overrode an analysis-only REFRESH (`--despite-refresh`) and the
    /// merged tree's checks passed: apply against the verified world.
    Overridden {
        overridden: Validity,
        verified: Validity,
        checks: MergedChecks,
    },
}

/// Whether a human may override `validity`: a REFRESH whose every reason
/// comes from the file and symbol analysis. STOP (already applied), a patch
/// that no longer applies, and a check that failed on the merged tree are
/// facts no human judgment can change.
pub fn overridable(validity: &Validity) -> bool {
    validity.decision == Decision::Refresh
        && !validity.reasons.is_empty()
        && validity.reasons.iter().all(|reason| {
            matches!(
                reason.code,
                ReasonCode::FactBroken
                    | ReasonCode::FactMissing
                    | ReasonCode::SameSymbolEdited
                    | ReasonCode::AnalysisUncertain
            ) && !integration::is_integration_reason(reason)
        })
}

/// The configuration frozen with the run, or `None` when the snapshot cannot be
/// read. Callers treat `None` as strict accept so that a missing file can only
/// make acceptance more conservative.
pub fn run_config(run_dir: &Path) -> Option<Config> {
    fs::read_to_string(run_dir.join("config.snapshot.yml"))
        .ok()
        .and_then(|text| serde_yaml::from_str::<Config>(&text).ok())
}

/// Decide whether `candidate_label` of `run` may be accepted onto the source as
/// it is now. Runs no external work when the tree is unchanged. When the world
/// moved but L0 says the work is still valid, the run's own verification checks
/// are run against the merged result before anything is applied.
///
/// The caller holds the per-run and per-source operation locks and has no
/// database transaction open; the checks (each bounded by the run's execution
/// timeout) hold only those locks.
/// `despite_refresh` is a human's explicit override (`dispatch accept
/// --despite-refresh`); policy applications always pass `false`.
pub fn gate(
    run: &RunRecord,
    candidate_label: &str,
    run_dir: &Path,
    despite_refresh: bool,
) -> Result<AcceptGate> {
    // Strict accept, or a launched run's unmoved tree: the fingerprint rules
    // apply, and nothing is evaluated.
    if let Some(
        skipped
        @ (ChecksSkipped::ConfigUnreadable | ChecksSkipped::Strict | ChecksSkipped::Unmoved),
    ) = integration::checks_skipped(run, candidate_label, run_dir, None)?
    {
        return Ok(AcceptGate::Legacy(skipped));
    }
    let mut validity = evaluate_run(run, candidate_label, Some(run_dir))?;
    if validity.decision == Decision::Continue {
        let (validity, checks) = block_on(integration::verify_integration(
            run,
            candidate_label,
            validity,
            run_dir,
        ))??;
        return Ok(match validity.decision {
            Decision::Continue => AcceptGate::Compatible { validity, checks },
            Decision::Refresh | Decision::Stop => AcceptGate::Blocked {
                validity,
                checks: Some(checks),
            },
        });
    }
    if despite_refresh && overridable(&validity) {
        // The override sets aside the analysis, never the checks: they must
        // run on the merged tree and pass.
        let (verified, checks) = block_on(integration::verify_integration(
            run,
            candidate_label,
            Validity {
                decision: Decision::Continue,
                reasons: Vec::new(),
                ..validity.clone()
            },
            run_dir,
        ))??;
        return Ok(match (verified.decision, checks) {
            (Decision::Continue, checks @ MergedChecks::Passed(_)) => AcceptGate::Overridden {
                overridden: validity,
                verified,
                checks,
            },
            (Decision::Continue, checks) => {
                validity.reasons.push(Reason {
                    code: ReasonCode::AnalysisUncertain,
                    fact_id: None,
                    path: None,
                    detail: "an override needs checks.verify to run and pass on the merged tree, and none ran".into(),
                });
                AcceptGate::Blocked {
                    validity,
                    checks: Some(checks),
                }
            }
            (_, checks) => {
                validity.reasons.extend(verified.reasons);
                validity.reasons.truncate(MAX_REASONS);
                AcceptGate::Blocked {
                    validity,
                    checks: Some(checks),
                }
            }
        });
    }
    Ok(AcceptGate::Blocked {
        validity,
        checks: None,
    })
}

/// Drive a future to completion from synchronous code that may be running on a
/// tokio worker. A multi-thread runtime lends the worker out; anything else (no
/// runtime, or a current-thread one that cannot be blocked) gets a temporary
/// runtime on its own thread.
fn block_on<T: Send>(future: impl Future<Output = T> + Send) -> Result<T> {
    use tokio::runtime::{Builder, Handle, RuntimeFlavor};
    if let Ok(handle) = Handle::try_current()
        && handle.runtime_flavor() == RuntimeFlavor::MultiThread
    {
        return Ok(tokio::task::block_in_place(|| handle.block_on(future)));
    }
    std::thread::scope(|scope| {
        scope
            .spawn(|| -> Result<T> {
                let runtime = Builder::new_current_thread().enable_all().build()?;
                Ok(runtime.block_on(future))
            })
            .join()
            .map_err(|_| anyhow!("integration check thread panicked"))?
    })
}

/// L0 then L1. L0 is the verdict from file and patch state alone. Order
/// matters: an unchanged world needs no analysis, an empty patch cannot
/// conflict, a patch that already reverses cleanly is present in the source,
/// and a patch that does not apply cleanly needs a refresh. Only when L0
/// continues and the world moved does L1 check the symbols and files the patch
/// assumes; any reason it finds means refresh.
pub fn evaluate(world: &WorldObservation, work: &WorkView) -> Result<Validity> {
    evaluate_with(world, work, &facts::Memo::default())
}

/// `evaluate`, with facts already derived for the same (S0, Δ) taken from
/// `memo`.
pub fn evaluate_with(
    world: &WorldObservation,
    work: &WorkView,
    memo: &facts::Memo,
) -> Result<Validity> {
    Ok(assess(world, work, None, memo)?.0)
}

/// `evaluate`, also returning Git's whole complaint when the patch no longer
/// applies; the bounded reason names the paths it mentions, and `kept`, where
/// the caller keeps the complaint.
fn assess(
    world: &WorldObservation,
    work: &WorkView,
    kept: Option<&Path>,
    memo: &facts::Memo,
) -> Result<(Validity, Option<String>)> {
    let world_changed = !world.changes.is_empty();
    let mut analysis = AnalysisLevel::FilesOnly;
    let mut conflict = None;
    // An unchanged world is checked first so that it costs no file or Git work.
    let (decision, mut reasons) = if !world_changed || patch_is_empty(work.delta_patch)? {
        (Decision::Continue, Vec::new())
    } else if apply_check(work, true)?.is_none() {
        let reason = Reason {
            code: ReasonCode::AlreadyApplied,
            fact_id: None,
            path: None,
            detail: "the patch is already present in the source".into(),
        };
        (Decision::Stop, vec![reason])
    } else if let Some(stderr) = apply_check(work, false)? {
        let paths = conflicting_paths(&stderr);
        let reason = Reason {
            code: ReasonCode::PatchConflict,
            fact_id: None,
            path: paths.first().cloned(),
            detail: conflict_detail(&stderr, &paths, kept),
        };
        conflict = Some(stderr);
        (Decision::Refresh, vec![reason])
    } else {
        let (found, level) = facts::check(work, world, memo)?;
        analysis = level;
        (
            if found.is_empty() {
                Decision::Continue
            } else {
                Decision::Refresh
            },
            found,
        )
    };
    reasons.truncate(MAX_REASONS);
    let validity = Validity {
        decision,
        evaluated_at: Utc::now(),
        world_digest: world.digest.clone(),
        world_changed,
        changed_files: u32::try_from(world.changes.len()).unwrap_or(u32::MAX),
        reasons,
        analysis,
    };
    Ok((validity, conflict))
}

/// The paths `git apply --check` names in `stderr`, once each, in order:
/// `error: patch failed: <path>:<line>` and `error: <path>: <problem>`.
fn conflicting_paths(stderr: &str) -> Vec<String> {
    let mut paths = Vec::<String>::new();
    for line in stderr.lines() {
        let Some(rest) = line.strip_prefix("error: ") else {
            continue;
        };
        let path = match rest.strip_prefix("patch failed: ") {
            Some(at) => at.rsplit_once(':').map_or(at, |(path, _)| path),
            None => match rest.split_once(": ") {
                Some((path, _)) => path,
                None => continue,
            },
        };
        if !paths.iter().any(|known| known == path) {
            paths.push(path.to_owned());
        }
    }
    paths
}

/// A patch conflict in one bounded line: the paths, where Git's whole
/// complaint is kept, and as much of it as fits.
fn conflict_detail(stderr: &str, paths: &[String], kept: Option<&Path>) -> String {
    let mut detail = match paths {
        [] => "the patch does not apply to the current source".to_owned(),
        [one] => format!("the patch no longer applies to {one}"),
        [first, second] => format!("the patch no longer applies to {first} and {second}"),
        [first, second, rest @ ..] => format!(
            "the patch no longer applies to {first}, {second} and {} more",
            rest.len()
        ),
    };
    if let Some(path) = kept {
        detail.push_str(&format!("; Git's full output: {}", path.display()));
    }
    if !stderr.is_empty() {
        let said = stderr.split_whitespace().collect::<Vec<_>>().join(" ");
        detail.push_str(&format!("; git: {said}"));
    }
    detail.chars().take(MAX_DETAIL_CHARS).collect()
}

/// Whether the whole of a candidate's retained patch is in the source now:
/// the reverse-apply check behind STOP's `already_applied`. Partial presence
/// is not presence, and an empty patch is never present. `Some` carries the
/// digest of the world it was found in, for the record.
pub fn patch_present(run: &RunRecord, candidate_label: &str) -> Result<Option<String>> {
    let work = WorkView::of(run, find_candidate(run, candidate_label)?);
    if patch_is_empty(work.delta_patch)? || apply_check(&work, true)?.is_some() {
        return Ok(None);
    }
    let world = world::observe(
        &run.source_path,
        &run.baseline_path,
        &run.baseline_commit,
        &run.source_kind,
    )?;
    Ok(Some(world.digest))
}

fn patch_is_empty(patch: &Path) -> Result<bool> {
    let metadata = fs::metadata(patch)
        .with_context(|| format!("failed to inspect delta {}", patch.display()))?;
    Ok(metadata.len() == 0)
}

/// Run `git apply --check` (optionally `--reverse`) in the source. `None`
/// means the patch would apply; `Some(stderr)` is Git's trimmed complaint.
fn apply_check(work: &WorkView, reverse: bool) -> Result<Option<String>> {
    let mut command = git_command(work.source);
    command.args(["apply", "--check"]);
    if reverse {
        command.arg("--reverse");
    }
    command
        .args(["--binary", "--whitespace=nowarn", "--"])
        .arg(work.delta_patch);
    let output = run_git(command, None, "failed to check the candidate patch")?;
    if output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, process::Command};

    use tempfile::TempDir;

    use super::*;
    use crate::{
        CandidateRecord, CandidateStatus, CoherenceRecord, DiffStats, SourceKind,
        source::{SourceSnapshot, create_snapshot},
    };

    struct Fixture {
        root: TempDir,
        source: PathBuf,
        snapshot: SourceSnapshot,
    }

    impl Fixture {
        fn new(git: bool, files: &[(&str, &str)]) -> Self {
            let root = TempDir::new().unwrap();
            let source = root.path().join("source");
            fs::create_dir_all(&source).unwrap();
            let source = fs::canonicalize(source).unwrap();
            for (path, contents) in files {
                write(&source.join(path), contents);
            }
            if git {
                git_in(&source, &["init", "--quiet"]);
                git_in(&source, &["add", "-A"]);
                git_in(&source, &["commit", "--quiet", "-m", "initial"]);
            }
            let snapshot = create_snapshot(&source, &root.path().join("run")).unwrap();
            Self {
                root,
                source,
                snapshot,
            }
        }

        fn write(&self, path: &str, contents: &str) {
            write(&self.source.join(path), contents);
        }

        fn patch(&self, contents: &str) -> PathBuf {
            let path = self.root.path().join("delta.patch");
            fs::write(&path, contents).unwrap();
            path
        }

        fn world(&self) -> WorldObservation {
            world::observe(
                &self.source,
                &self.snapshot.baseline_path,
                &self.snapshot.baseline_commit,
                &self.snapshot.kind,
            )
            .unwrap()
        }

        fn evaluate(&self, patch: &Path) -> Validity {
            evaluate(
                &self.world(),
                &WorkView {
                    source: &self.source,
                    delta_patch: patch,
                    baseline: &self.snapshot.baseline_path,
                    baseline_commit: &self.snapshot.baseline_commit,
                },
            )
            .unwrap()
        }

        fn run(&self, patch: PathBuf) -> RunRecord {
            let candidate = CandidateRecord {
                id: "c1".into(),
                label: "A".into(),
                harness_id: "codex".into(),
                harness_version: None,
                model: None,
                status: CandidateStatus::Completed,
                workspace_path: self.root.path().join("workspace"),
                prompt_path: PathBuf::new(),
                stdout_path: PathBuf::new(),
                stderr_path: PathBuf::new(),
                diff_path: patch,
                duration_ms: 0,
                exit_code: Some(0),
                timed_out: false,
                tokens: None,
                token_semantics: None,
                cost_usd: None,
                error: None,
                diff_stats: DiffStats::default(),
                checks: Vec::new(),
            };
            let mut run = old_run_record();
            run.source_path = self.source.clone();
            run.source_kind = self.snapshot.kind.clone();
            run.baseline_path = self.snapshot.baseline_path.clone();
            run.baseline_commit = self.snapshot.baseline_commit.clone();
            run.candidates = vec![candidate];
            run
        }
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn git_in(path: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
            ])
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// The JSON shape of a run.json written before `coherence` existed.
    const OLD_RUN_JSON: &str = r#"{
        "id":"01TEST", "task":"Fix cache", "exact_prompt":"Fix cache",
        "source_path":"/source", "source_kind":"directory", "source_git_head":null,
        "source_fingerprint":"baseline", "baseline_path":"/baseline", "baseline_commit":"abc",
        "status":"running", "created_at":"2026-09-17T00:00:00Z", "completed_at":null,
        "environment":{"dispatch_version":"test","os":"test","architecture":"test","execution_backend":"local","timeout_secs":30,"cpus":1.0,"memory":"1g","max_parallel":1},
        "evaluation":null,"applied_candidate":null
    }"#;

    fn old_run_record() -> RunRecord {
        serde_json::from_str(OLD_RUN_JSON).unwrap()
    }

    const A_TXT: &str = "one\ntwo\nthree\nfour\nfive\n";
    const PATCH_A: &str =
        "--- a/a.txt\n+++ b/a.txt\n@@ -1,5 +1,5 @@\n one\n-two\n+TWO\n three\n four\n five\n";

    fn base_files() -> Vec<(&'static str, &'static str)> {
        vec![
            (".gitignore", "target/\n"),
            ("a.txt", A_TXT),
            ("b.txt", "b\n"),
        ]
    }

    fn reason_codes(validity: &Validity) -> Vec<ReasonCode> {
        validity.reasons.iter().map(|reason| reason.code).collect()
    }

    /// Mutates the source after the baseline snapshot.
    type Mutate = fn(&Fixture);

    struct Case {
        name: &'static str,
        git: bool,
        mutate: Mutate,
        patch: &'static str,
        decision: Decision,
        reasons: &'static [ReasonCode],
        world_changed: bool,
    }

    #[test]
    fn l0_verdict_table() {
        let cases = [
            Case {
                name: "unrelated file change continues",
                git: true,
                mutate: |f| f.write("b.txt", "b edited\n"),
                patch: PATCH_A,
                decision: Decision::Continue,
                reasons: &[],
                world_changed: true,
            },
            Case {
                name: "untracked new file continues",
                git: true,
                mutate: |f| f.write("new.txt", "new\n"),
                patch: PATCH_A,
                decision: Decision::Continue,
                reasons: &[],
                world_changed: true,
            },
            Case {
                name: "ignored build directory change is not drift",
                git: true,
                mutate: |f| f.write("target/out.bin", "built\n"),
                patch: PATCH_A,
                decision: Decision::Continue,
                reasons: &[],
                world_changed: false,
            },
            Case {
                name: "conflicting hunk refreshes",
                git: true,
                mutate: |f| f.write("a.txt", "one\ntwo changed elsewhere\nthree\nfour\nfive\n"),
                patch: PATCH_A,
                decision: Decision::Refresh,
                reasons: &[ReasonCode::PatchConflict],
                world_changed: true,
            },
            Case {
                name: "identical patch already applied stops",
                git: true,
                mutate: |f| f.write("a.txt", "one\nTWO\nthree\nfour\nfive\n"),
                patch: PATCH_A,
                decision: Decision::Stop,
                reasons: &[ReasonCode::AlreadyApplied],
                world_changed: true,
            },
            Case {
                name: "empty patch continues",
                git: true,
                mutate: |f| f.write("a.txt", "one\ntwo changed elsewhere\nthree\nfour\nfive\n"),
                patch: "",
                decision: Decision::Continue,
                reasons: &[],
                world_changed: true,
            },
            Case {
                name: "deleted target file refreshes",
                git: true,
                mutate: |f| fs::remove_file(f.source.join("a.txt")).unwrap(),
                patch: PATCH_A,
                decision: Decision::Refresh,
                reasons: &[ReasonCode::PatchConflict],
                world_changed: true,
            },
            Case {
                name: "directory source unrelated change continues",
                git: false,
                mutate: |f| f.write("b.txt", "b edited\n"),
                patch: PATCH_A,
                decision: Decision::Continue,
                reasons: &[],
                world_changed: true,
            },
            Case {
                name: "directory source conflict refreshes",
                git: false,
                mutate: |f| f.write("a.txt", "one\ntwo changed elsewhere\nthree\nfour\nfive\n"),
                patch: PATCH_A,
                decision: Decision::Refresh,
                reasons: &[ReasonCode::PatchConflict],
                world_changed: true,
            },
            Case {
                name: "directory source already applied stops",
                git: false,
                mutate: |f| f.write("a.txt", "one\nTWO\nthree\nfour\nfive\n"),
                patch: PATCH_A,
                decision: Decision::Stop,
                reasons: &[ReasonCode::AlreadyApplied],
                world_changed: true,
            },
        ];
        for case in cases {
            let fixture = Fixture::new(case.git, &base_files());
            let patch = fixture.patch(case.patch);
            (case.mutate)(&fixture);
            let validity = fixture.evaluate(&patch);
            assert_eq!(validity.decision, case.decision, "{}", case.name);
            assert_eq!(reason_codes(&validity), case.reasons, "{}", case.name);
            assert_eq!(validity.world_changed, case.world_changed, "{}", case.name);
            assert_eq!(validity.analysis, AnalysisLevel::FilesOnly, "{}", case.name);
        }
    }

    #[test]
    fn unchanged_world_continues_without_running_git_apply() {
        for git in [true, false] {
            let fixture = Fixture::new(git, &base_files());
            // Git apply on a missing patch would fail, so any call would show.
            let missing = fixture.root.path().join("missing.patch");
            let validity = fixture.evaluate(&missing);
            assert_eq!(validity.decision, Decision::Continue);
            assert!(!validity.world_changed);
            assert_eq!(validity.changed_files, 0);
            assert!(validity.reasons.is_empty());
            assert_eq!(validity.world_digest, fixture.world().digest);
        }
    }

    #[test]
    fn conflict_reason_carries_bounded_git_detail() {
        let fixture = Fixture::new(true, &base_files());
        let patch = fixture.patch(PATCH_A);
        fixture.write("a.txt", "one\ntwo changed elsewhere\nthree\nfour\nfive\n");
        let validity = fixture.evaluate(&patch);
        let detail = &validity.reasons[0].detail;
        assert!(
            detail.starts_with(
                "the patch no longer applies to a.txt; git: error: patch failed: a.txt:1"
            ),
            "{detail}"
        );
        assert_eq!(validity.reasons[0].path.as_deref(), Some("a.txt"));
        assert!(detail.chars().count() <= MAX_DETAIL_CHARS);
        assert_eq!(validity.changed_files, 1);
    }

    /// Finish and accept keep Git's whole complaint beside the run and say
    /// where; a reading evaluation writes nothing.
    #[test]
    fn a_conflict_keeps_git_output_only_when_asked() {
        let fixture = Fixture::new(true, &base_files());
        let run = fixture.run(fixture.patch(PATCH_A));
        fixture.write("a.txt", "one\ntwo changed elsewhere\nthree\nfour\nfive\n");
        let run_dir = fixture.root.path().join("run");
        let kept = run_dir.join(CONFLICT_OUTPUT);
        assert!(
            !evaluate_run(&run, "A", None).unwrap().reasons[0]
                .detail
                .contains(CONFLICT_OUTPUT)
        );
        assert!(!kept.exists());
        let validity = evaluate_run(&run, "A", Some(&run_dir)).unwrap();
        assert!(
            validity.reasons[0]
                .detail
                .contains(&format!("Git's full output: {}", kept.display())),
            "{:?}",
            validity.reasons
        );
        let output = fs::read_to_string(&kept).unwrap();
        assert!(output.contains("error: patch failed: a.txt:1"), "{output}");
        assert!(
            output.contains("error: a.txt: patch does not apply"),
            "{output}"
        );
    }

    #[test]
    fn conflicting_paths_come_from_git_complaint_once_each() {
        let stderr = "error: patch failed: src/a.rs:12\nerror: src/a.rs: patch does not apply\n\
                      error: b.txt: already exists in working directory\n\
                      error: c d.txt: does not exist in index\nerror: corrupt patch at line 9";
        assert_eq!(conflicting_paths(stderr), ["src/a.rs", "b.txt", "c d.txt"]);
        let paths = conflicting_paths(stderr);
        let detail = conflict_detail(stderr, &paths, Some(Path::new("/r/patch-conflict.txt")));
        assert!(
            detail.starts_with(
                "the patch no longer applies to src/a.rs, b.txt and 1 more; Git's full output: /r/patch-conflict.txt; git: error: patch failed: src/a.rs:12 error:"
            ),
            "{detail}"
        );
        assert!(detail.chars().count() <= MAX_DETAIL_CHARS);
        assert_eq!(
            conflict_detail("", &[], None),
            "the patch does not apply to the current source"
        );
    }

    #[test]
    fn evaluation_never_mutates_the_source() {
        let fixture = Fixture::new(true, &base_files());
        let patch = fixture.patch(PATCH_A);
        fixture.write("b.txt", "b edited\n");
        let before = crate::source::fingerprint_tree(&fixture.source).unwrap();
        assert_eq!(fixture.evaluate(&patch).decision, Decision::Continue);
        assert_eq!(
            crate::source::fingerprint_tree(&fixture.source).unwrap(),
            before
        );
        assert_eq!(
            fs::read_to_string(fixture.source.join("a.txt")).unwrap(),
            A_TXT
        );
    }

    #[test]
    fn evaluate_run_observes_and_selects_the_candidate() {
        let fixture = Fixture::new(true, &base_files());
        let patch = fixture.patch(PATCH_A);
        let run = fixture.run(patch);
        fixture.write("a.txt", "one\nTWO\nthree\nfour\nfive\n");
        let validity = evaluate_run(&run, "A", None).unwrap();
        assert_eq!(validity.decision, Decision::Stop);
        assert!(evaluate_run(&run, "B", None).is_err());
        assert!(matches!(run.source_kind, SourceKind::Git));
    }

    #[test]
    fn live_validity_covers_only_finished_unapplied_sole_candidates() {
        use crate::{LifecycleState, RunStatus, WorkResult};
        let fixture = Fixture::new(true, &base_files());
        let mut run = fixture.run(fixture.patch(PATCH_A));
        // Not finished, not ready: nothing to say.
        assert!(live_validity(&run).is_none());
        run.outcome.lifecycle = LifecycleState::Finished;
        run.outcome.work_result = WorkResult::Ready;
        let unchanged = live_validity(&run).unwrap();
        assert_eq!(unchanged.decision, Decision::Continue);
        assert!(!unchanged.world_changed);
        fixture.write("a.txt", "one\nTWO\nthree\nfour\nfive\n");
        assert_eq!(live_validity(&run).unwrap().decision, Decision::Stop);
        // The stored record is never touched; only the display copy carries it.
        assert!(run.coherence.is_none());
        let shown = with_live_validity(&run);
        assert_eq!(
            shown.coherence.unwrap().validity.unwrap().decision,
            Decision::Stop
        );
        run.status = RunStatus::Applied;
        assert!(live_validity(&run).is_none());
        run.status = RunStatus::ReadyForEvaluation;
        run.outcome.application = ApplicationState::Applied;
        assert!(live_validity(&run).is_none());
        run.outcome.application = ApplicationState::NotApplied;
        run.candidates.push(run.candidates[0].clone());
        assert!(live_validity(&run).is_none());
        run.candidates.truncate(1);
        // An error (missing patch) is swallowed.
        run.candidates[0].diff_path = PathBuf::from("/nonexistent/delta.patch");
        fixture.write("b.txt", "b edited\n");
        assert!(live_validity(&run).is_none());
    }

    /// `dispatch close --reason already-present` takes only the whole patch:
    /// one of its two hunks in the source is not presence.
    #[test]
    fn a_patch_is_present_only_when_all_of_it_is_in_the_source() {
        let fixture = Fixture::new(true, &base_files());
        let both = "--- a/a.txt\n+++ b/a.txt\n@@ -1,5 +1,5 @@\n one\n-two\n+TWO\n three\n four\n five\n\
                    --- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-b\n+B\n";
        let run = fixture.run(fixture.patch(both));
        assert_eq!(patch_present(&run, "A").unwrap(), None, "unmoved");
        fixture.write("a.txt", "one\nTWO\nthree\nfour\nfive\n");
        assert_eq!(patch_present(&run, "A").unwrap(), None, "partial");
        fixture.write("b.txt", "B\n");
        let digest = patch_present(&run, "A")
            .unwrap()
            .expect("whole patch present");
        assert_eq!(digest, fixture.world().digest);
        let empty = fixture.run(fixture.patch(""));
        assert_eq!(patch_present(&empty, "A").unwrap(), None, "empty");
    }

    #[test]
    fn blocked_message_names_the_next_commands() {
        let fixture = Fixture::new(true, &base_files());
        let run = fixture.run(fixture.patch(PATCH_A));
        fixture.write("a.txt", "one\nTWO\nthree\nfour\nfive\n");
        let validity = evaluate_run(&run, "A", None).unwrap();
        let text = CoherenceBlocked {
            run_id: "RUN1".into(),
            validity: validity.clone(),
            attached: false,
            despite_refresh: false,
            landed_by: None,
            checks: None,
        }
        .to_string();
        assert!(
            text.contains("STOP")
                && text.contains("Run 'dispatch close RUN1 --reason already-present'")
        );
        let refresh = CoherenceBlocked {
            run_id: "RUN1".into(),
            validity: Validity {
                decision: Decision::Refresh,
                ..validity.clone()
            },
            attached: false,
            despite_refresh: false,
            landed_by: None,
            checks: None,
        }
        .to_string();
        assert!(refresh.contains(
            "Run 'dispatch refresh RUN1' to redo the work on the current source, or 'dispatch reject RUN1'."
        ));
        // Attached work has no Dispatch task to refresh.
        let attached = CoherenceBlocked {
            run_id: "RUN1".into(),
            validity: Validity {
                decision: Decision::Refresh,
                ..validity.clone()
            },
            attached: true,
            despite_refresh: false,
            landed_by: None,
            checks: None,
        }
        .to_string();
        assert!(!attached.contains("dispatch refresh"), "{attached}");
        assert!(attached.contains("Run your agent again on the current source"));
        // A STOP is never overridable, and the refusal says so.
        let stop = CoherenceBlocked {
            run_id: "RUN1".into(),
            validity,
            attached: false,
            despite_refresh: true,
            landed_by: None,
            checks: None,
        }
        .to_string();
        assert!(stop.contains("cannot be overridden"), "{stop}");
    }

    #[test]
    fn gate_without_a_readable_config_snapshot_is_legacy_and_skips_the_oracle() {
        let fixture = Fixture::new(true, &base_files());
        let run = fixture.run(fixture.patch(PATCH_A));
        fixture.write("b.txt", "b edited\n");
        let run_dir = fixture.root.path().join("run");
        assert!(run_config(&run_dir).is_none());
        assert!(matches!(
            gate(&run, "A", &run_dir, false).unwrap(),
            AcceptGate::Legacy(ChecksSkipped::ConfigUnreadable)
        ));
        fs::write(run_dir.join("config.snapshot.yml"), "coherence: [not a map").unwrap();
        assert!(run_config(&run_dir).is_none());
    }

    #[test]
    fn block_on_works_without_a_runtime_and_on_either_runtime_flavor() {
        assert_eq!(block_on(async { 1 }).unwrap(), 1);
        let current = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert_eq!(
            current.block_on(async { block_on(async { 2 }) }).unwrap(),
            2
        );
        let multi = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        assert_eq!(multi.block_on(async { block_on(async { 3 }) }).unwrap(), 3);
        // A spawned task runs on a worker, where `block_in_place` is allowed.
        let spawned = multi.block_on(async { tokio::spawn(async { block_on(async { 4 }) }).await });
        assert_eq!(spawned.unwrap().unwrap(), 4);
    }

    #[test]
    fn old_run_json_deserializes_without_coherence() {
        assert!(old_run_record().coherence.is_none());
    }

    #[test]
    fn coherence_record_round_trips() {
        let fixture = Fixture::new(true, &base_files());
        let patch = fixture.patch(PATCH_A);
        fixture.write("a.txt", "one\ntwo changed elsewhere\nthree\nfour\nfive\n");
        let record = CoherenceRecord {
            version: 1,
            refreshed_from: Some("01OLD".into()),
            validity: Some(fixture.evaluate(&patch)),
            first_invalid_at: Some(Utc::now()),
            overridden: None,
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("\"decision\":\"refresh\""));
        assert!(json.contains("\"patch_conflict\""));
        assert!(json.contains("\"files_only\""));
        assert_eq!(
            serde_json::from_str::<CoherenceRecord>(&json).unwrap(),
            record
        );

        let mut run = old_run_record();
        run.coherence = Some(record.clone());
        let back: RunRecord = serde_json::from_str(&serde_json::to_string(&run).unwrap()).unwrap();
        assert_eq!(back.coherence, Some(record));

        let defaulted: CoherenceRecord = serde_json::from_str("{}").unwrap();
        assert_eq!(defaulted.version, 1);
        assert!(defaulted.validity.is_none());
    }
}
