//! Work coherence: is a run's finished work still valid against a source tree
//! that may have moved since the run's baseline was taken?
//!
//! This is the pure evaluator: L0 (file and patch state) then L1 (symbol and
//! file facts, see `facts`). It reads the source tree, the candidate patch and
//! the run's baseline, and runs `git apply` only with `--check`, so it never
//! mutates anything, and it never touches the database or the run record.

use std::{fmt, fs, future::Future, path::Path};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;

use crate::{
    AnalysisLevel, ApplicationState, CandidateRecord, CoherenceRecord, Decision, LifecycleState,
    Reason, ReasonCode, RunRecord, RunStatus, Validity, WorkResult,
    coherence::world::WorldObservation,
    config::{AcceptMode, Config},
    source::{fingerprint_tree, git_command, run_git},
};

pub mod facts;
pub mod integration;
pub mod interactions;
pub mod measure;
pub mod symbols;
pub mod watch;
pub mod world;

const MAX_REASONS: usize = 20;
const MAX_DETAIL_CHARS: usize = 300;

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
pub fn evaluate_run(run: &RunRecord, candidate_label: &str) -> Result<Validity> {
    let candidate = find_candidate(run, candidate_label)?;
    let world = world::observe(
        &run.source_path,
        &run.baseline_path,
        &run.baseline_commit,
        &run.source_kind,
    )?;
    evaluate(&world, &WorkView::of(run, candidate))
}

/// The verdict `check`, `status`, `explain` and `refresh` show: a fresh file
/// and symbol evaluation, unless accept already refused this result on the
/// merged tree of this same world. Only accept runs the integration checks,
/// so a fresh evaluation of an unmoved world must not hide that refusal. Once
/// the world moves, the fresh verdict stands. The accept gate itself always
/// evaluates afresh, so a retry runs the checks again.
pub fn shown_validity(run: &RunRecord, candidate_label: &str) -> Result<Validity> {
    let live = evaluate_run(run, candidate_label)?;
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
    if !is_ready_unapplied(run) {
        return None;
    }
    let [candidate] = run.candidates.as_slice() else {
        return None;
    };
    match shown_validity(run, &candidate.label) {
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
    let mut shown = run.clone();
    if let Some(validity) = live_validity(run) {
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
        if self.validity.decision == Decision::Stop {
            write!(f, "Run 'dispatch reject {}'.", self.run_id)?;
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

/// What acceptance may do about a possibly moved source.
pub enum AcceptGate {
    /// Strict mode, or the whole tree is byte-identical to the snapshot: the
    /// existing all-or-nothing fingerprint rules apply unchanged.
    Legacy,
    /// The world moved but the work is still valid; apply against that world.
    Compatible(Validity),
    /// The work is stale; nothing may be accepted or applied.
    Blocked(Validity),
    /// A human overrode an analysis-only REFRESH (`--despite-refresh`) and the
    /// merged tree's checks passed: apply against the verified world.
    Overridden {
        overridden: Validity,
        verified: Validity,
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
    // An unreadable snapshot means strict, so the oracle is never reached.
    let Some(config) = run_config(run_dir) else {
        return Ok(AcceptGate::Legacy);
    };
    // For a run Dispatch launched, the whole-tree fingerprint was taken from
    // the same tree S0 was copied from, so an equal fingerprint means an
    // unmoved world. Attached work's S0 is a merge-base commit while its
    // fingerprint is the root's working tree at attach time, which may already
    // differ from S0; the shortcut would skip evaluation and the integration
    // checks, so attached runs always evaluate.
    if config.coherence.accept == AcceptMode::Strict
        || (run.mode != crate::RunMode::Attached
            && fingerprint_tree(&run.source_path)? == run.source_fingerprint)
    {
        return Ok(AcceptGate::Legacy);
    }
    let mut validity = evaluate_run(run, candidate_label)?;
    if validity.decision == Decision::Continue {
        validity = block_on(integration::verify_integration(
            run,
            candidate_label,
            validity,
            &config,
            run_dir,
        ))??;
    } else if despite_refresh && overridable(&validity) {
        // The override sets aside the analysis, never the checks: they must
        // run on the merged tree and pass.
        let verified = block_on(integration::verify_integration(
            run,
            candidate_label,
            Validity {
                decision: Decision::Continue,
                reasons: Vec::new(),
                ..validity.clone()
            },
            &config,
            run_dir,
        ))??;
        return Ok(match verified.decision {
            Decision::Continue if verified.analysis == AnalysisLevel::Integration => {
                AcceptGate::Overridden {
                    overridden: validity,
                    verified,
                }
            }
            Decision::Continue => {
                validity.reasons.push(Reason {
                    code: ReasonCode::AnalysisUncertain,
                    fact_id: None,
                    path: None,
                    detail: "an override needs checks.verify to run and pass on the merged tree, and none ran".into(),
                });
                AcceptGate::Blocked(validity)
            }
            _ => {
                validity.reasons.extend(verified.reasons);
                validity.reasons.truncate(MAX_REASONS);
                AcceptGate::Blocked(validity)
            }
        });
    }
    Ok(match validity.decision {
        Decision::Continue => AcceptGate::Compatible(validity),
        Decision::Refresh | Decision::Stop => AcceptGate::Blocked(validity),
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
    let world_changed = !world.changes.is_empty();
    let mut analysis = AnalysisLevel::FilesOnly;
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
        let detail = if stderr.is_empty() {
            "the patch does not apply to the current source".to_owned()
        } else {
            stderr.chars().take(MAX_DETAIL_CHARS).collect()
        };
        let reason = Reason {
            code: ReasonCode::PatchConflict,
            fact_id: None,
            path: None,
            detail,
        };
        (Decision::Refresh, vec![reason])
    } else {
        let (found, level) = facts::check(work, world)?;
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
    Ok(Validity {
        decision,
        evaluated_at: Utc::now(),
        world_digest: world.digest.clone(),
        world_changed,
        changed_files: u32::try_from(world.changes.len()).unwrap_or(u32::MAX),
        reasons,
        analysis,
    })
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
        assert!(detail.contains("a.txt"), "{detail}");
        assert!(detail.chars().count() <= MAX_DETAIL_CHARS);
        assert_eq!(validity.changed_files, 1);
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
        let validity = evaluate_run(&run, "A").unwrap();
        assert_eq!(validity.decision, Decision::Stop);
        assert!(evaluate_run(&run, "B").is_err());
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

    #[test]
    fn blocked_message_names_the_next_commands() {
        let fixture = Fixture::new(true, &base_files());
        let run = fixture.run(fixture.patch(PATCH_A));
        fixture.write("a.txt", "one\nTWO\nthree\nfour\nfive\n");
        let validity = evaluate_run(&run, "A").unwrap();
        let text = CoherenceBlocked {
            run_id: "RUN1".into(),
            validity: validity.clone(),
            attached: false,
            despite_refresh: false,
            landed_by: None,
        }
        .to_string();
        assert!(text.contains("STOP") && text.contains("Run 'dispatch reject RUN1'"));
        let refresh = CoherenceBlocked {
            run_id: "RUN1".into(),
            validity: Validity {
                decision: Decision::Refresh,
                ..validity.clone()
            },
            attached: false,
            despite_refresh: false,
            landed_by: None,
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
            AcceptGate::Legacy
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
