//! Applying a finished candidate to the source: the human path (`accept`,
//! the review view, `dispatch apply`) and the policy path (`auto_apply`).
//!
//! Both go through `apply_locked`, which holds the per-source lock, asks
//! `coherence::gate` whether the work is still valid against the source as it
//! is now, and applies through the digest or fingerprint fence. What differs
//! is the authority: a human decision records acceptance, a policy never does.

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Result;
use chrono::Utc;
use sha2::{Digest, Sha256};

use super::{persist_event, sole_candidate, transition};
use crate::{
    AnalysisLevel, ApplicationState, AppliedBy, CoherenceRecord, Decision, EventRecord,
    LifecycleState, ReviewState, RunPhase, RunRecord, RunStatus, Validity, VerificationState,
    WorkResult,
    coherence::{AcceptGate, CoherenceBlocked, run_config},
    db::Database,
    lock::OperationLock,
    source::{self, ApplyReport},
    state::State,
};

/// Who is applying. This is threaded into the persisted outcome and events so
/// that an application by policy is never recorded as human acceptance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyAuthority {
    /// A person accepted the result (or typed `dispatch apply`).
    Human,
    /// The auto-apply policy of the owning session or invocation.
    AutoApply,
}

impl ApplyAuthority {
    pub fn applied_by(self) -> AppliedBy {
        match self {
            Self::Human => AppliedBy::Human,
            Self::AutoApply => AppliedBy::AutoApply,
        }
    }
}

/// What an automatic application attempt did. Every variant has already been
/// persisted (events and outcome) by the time it is returned; the caller only
/// presents it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The source was patched; `validity` is present when the world had moved.
    Applied {
        report: ApplyReport,
        validity: Option<Validity>,
    },
    /// The gate, or the authorization predicate on top of it, refused. The
    /// source is unchanged and the run remains reviewable.
    Blocked {
        reason: String,
        validity: Option<Validity>,
    },
    /// The run was not eligible; nothing external ran.
    Skipped { reason: String },
    /// The apply itself failed after authorization (for example Git).
    Failed { error: String },
}

impl ApplyOutcome {
    /// Project this outcome into `RunResult.auto_apply` (part 5.4 and 5.6 of
    /// the plan): the CLI's JSON/JSONL surface. Every variant already
    /// persisted what happened; this only reshapes it for presentation.
    pub fn summary(&self) -> crate::AutoApplySummary {
        let coherence = |validity: &Validity| crate::CoherenceSummary {
            decision: validity.decision,
            reasons: validity.reasons.iter().take(5).cloned().collect(),
            changed_files: validity.changed_files,
            analysis: validity.analysis,
        };
        match self {
            Self::Applied { report, validity } => crate::AutoApplySummary {
                outcome: "applied".into(),
                reason: None,
                coherence: validity.as_ref().map(coherence),
                files_changed: report.files_changed,
            },
            Self::Blocked { reason, validity } => crate::AutoApplySummary {
                outcome: "blocked".into(),
                reason: Some(reason.clone()),
                coherence: validity.as_ref().map(coherence),
                files_changed: 0,
            },
            Self::Skipped { reason } => crate::AutoApplySummary {
                outcome: "skipped".into(),
                reason: Some(reason.clone()),
                coherence: None,
                files_changed: 0,
            },
            Self::Failed { error } => crate::AutoApplySummary {
                outcome: "failed".into(),
                reason: Some(error.clone()),
                coherence: None,
                files_changed: 0,
            },
        }
    }
}

/// Store the latest validity on the run, remembering when work first became
/// invalid so that later evidence can show how long it ran on a false premise.
pub(super) fn remember_validity(run: &mut RunRecord, validity: &Validity) {
    let mut record = run.coherence.take().unwrap_or(CoherenceRecord {
        version: 1,
        refreshed_from: None,
        validity: None,
        first_invalid_at: None,
        overridden: None,
    });
    if validity.decision != Decision::Continue && record.first_invalid_at.is_none() {
        record.first_invalid_at = Some(validity.evaluated_at);
    }
    record.validity = Some(validity.clone());
    run.coherence = Some(record);
}

/// Remember a watcher's verdict and commit it as `coherence.checked` (a
/// `Continue` decision) or `coherence.invalidated` (anything else), returning
/// which kind was committed. Shared by the allocation-run watcher
/// (`native::apply_watch`) and the attach owner loop / `serve` (part 14.5);
/// none of them implement `mid_run: stop` themselves.
pub(super) fn persist_verdict(
    state: &State,
    db: &Database,
    run: &mut RunRecord,
    validity: &Validity,
) -> Result<&'static str> {
    remember_validity(run, validity);
    let kind = if validity.decision == Decision::Continue {
        "coherence.checked"
    } else {
        "coherence.invalidated"
    };
    transition(
        state,
        db,
        run,
        kind,
        serde_json::json!({"coherence": validity}),
    )?;
    Ok(kind)
}

/// The per-source apply lock path, keyed by the run's source path. Shared by
/// every caller that serializes against concurrent application of that
/// source (`apply_locked`, `auto_apply`) so the key expression exists once.
fn source_lock_path(state: &State, run: &RunRecord) -> PathBuf {
    let source_key = hex::encode(Sha256::digest(run.source_path.to_string_lossy().as_bytes()));
    state
        .root
        .join("locks")
        .join(format!("source-{source_key}.lock"))
}

/// Set the shared "applied" outcome fields and commit `result.applied`.
/// Shared by the human path (`apply_locked`) and the policy path
/// (`auto_apply`); only the authority, and whether a validity is present,
/// differ. `applied_by` is added to the payload only for a policy
/// application, so the human path's event stays byte-for-byte what it was
/// before auto-apply existed.
fn persist_applied(
    state: &State,
    mut run: RunRecord,
    candidate_label: &str,
    report: &ApplyReport,
    validity: Option<&Validity>,
    authority: ApplyAuthority,
) -> Result<RunRecord> {
    if let Some(validity) = validity {
        remember_validity(&mut run, validity);
    }
    run.applied_candidate = Some(candidate_label.to_owned());
    run.status = RunStatus::Applied;
    if authority == ApplyAuthority::Human {
        run.outcome.review = ReviewState::Accepted;
    }
    run.outcome.application = ApplicationState::Applied;
    run.outcome.applied_by = Some(authority.applied_by());
    run.outcome.phase = RunPhase::Finished;
    let mut db = Database::open(state.db_path())?;
    db.sync_run(&run)?;
    let mut payload = serde_json::json!({
        "files_changed": report.files_changed,
        "coherence": validity.filter(|validity| validity.world_changed),
    });
    if authority == ApplyAuthority::AutoApply {
        payload["applied_by"] = serde_json::json!("auto_apply");
    }
    persist_event(
        state,
        &db,
        EventRecord {
            run_id: run.id.clone(),
            candidate_label: Some(candidate_label.to_owned()),
            event_type: "result.applied".into(),
            timestamp: Utc::now(),
            payload,
            ..EventRecord::default()
        },
        &mut run,
    )?;
    // Applied, so a workspace Dispatch made for this Work holds nothing the
    // checkout lacks. Keeping it when removal fails loses nothing.
    if let Err(error) = super::attach::release_workspace(state, &mut run, "applied") {
        eprintln!("Kept the workspace Dispatch made for this work: {error:#}");
    }
    Ok(run)
}

/// Set `application` and commit `application.failed` with `payload`. Shared
/// mechanics only: the payload shape differs between the human path (always
/// carries a `coherence` key, `null` when there is none) and the policy path
/// (carries `applied_by` instead), so each caller builds its own payload.
fn persist_application_failed(
    state: &State,
    mut run: RunRecord,
    candidate_label: &str,
    application: ApplicationState,
    payload: serde_json::Value,
) -> Result<RunRecord> {
    run.outcome.application = application;
    run.outcome.phase = RunPhase::Finished;
    let database = Database::open(state.db_path())?;
    persist_event(
        state,
        &database,
        EventRecord {
            run_id: run.id.clone(),
            candidate_label: Some(candidate_label.to_owned()),
            event_type: "application.failed".into(),
            timestamp: Utc::now(),
            payload,
            ..EventRecord::default()
        },
        &mut run,
    )?;
    Ok(run)
}

/// The applied run on the same source whose patch is `run`'s patch, when a
/// STOP finds the work already present: which Work landed it. Reads the other
/// runs' stored projections only; `index` lines are ignored because they name
/// blobs, not changes.
pub(super) fn landed_by(state: &State, run: &RunRecord) -> Option<String> {
    let patch = |run: &RunRecord| -> Option<String> {
        let bytes = std::fs::read(&sole_candidate(run).ok()?.diff_path).ok()?;
        Some(
            String::from_utf8_lossy(&bytes)
                .lines()
                .filter(|line| !line.starts_with("index "))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    };
    let mine = patch(run)?;
    state
        .list_metadata_paths()
        .ok()?
        .into_iter()
        .filter_map(|path| serde_json::from_slice::<RunRecord>(&std::fs::read(path).ok()?).ok())
        .find(|other| {
            other.id != run.id
                && other.source_path == run.source_path
                && other.outcome.application == ApplicationState::Applied
                && patch(other).as_ref() == Some(&mine)
        })
        .map(|other| other.id)
}

/// Apply the run's result onto the source under the per-source lock.
/// The caller holds the run's operation lock. `authority` names who is
/// applying: only a human decision marks the review as accepted; a policy
/// application leaves the review exactly as it was.
pub(super) fn apply_locked(
    state: &State,
    mut run: RunRecord,
    quiet: bool,
    authority: ApplyAuthority,
    despite_refresh: bool,
) -> Result<Option<Validity>> {
    anyhow::ensure!(
        matches!(
            run.status,
            RunStatus::ReadyForEvaluation | RunStatus::Evaluated
        ),
        "run {} is {}; apply requires a completed, unapplied run",
        run.id,
        run.status.as_str()
    );
    anyhow::ensure!(
        !was_planned(&run),
        "run {} is a planned goal from an earlier Dispatch whose delivery can no longer be verified; refresh it instead",
        run.id
    );
    let normalized_label = sole_candidate(&run)?.label.clone();
    let _source_lock = OperationLock::acquire(
        &source_lock_path(state, &run),
        "another apply operation is already modifying this source",
    )?;
    let applied = crate::coherence::gate(
        &run,
        &normalized_label,
        &state.run_dir(&run.id),
        despite_refresh,
    )
    .and_then(|gate| match gate {
        AcceptGate::Legacy => Ok((source::safe_apply(&run, &normalized_label)?, None, None)),
        AcceptGate::Compatible(validity) => Ok((
            source::apply_validated(&run, &normalized_label, &validity.world_digest)?,
            Some(validity),
            None,
        )),
        AcceptGate::Overridden {
            overridden,
            verified,
        } => Ok((
            source::apply_validated(&run, &normalized_label, &verified.world_digest)?,
            Some(verified),
            Some(overridden),
        )),
        AcceptGate::Blocked(validity) => Err(CoherenceBlocked {
            run_id: run.id.clone(),
            landed_by: (validity.decision == Decision::Stop)
                .then(|| landed_by(state, &run))
                .flatten(),
            validity,
            attached: run.mode == crate::RunMode::Attached,
            despite_refresh,
        }
        .into()),
    });
    let (report, validity, overridden) = match applied {
        Ok(applied) => applied,
        Err(error) => {
            let message = format!("{error:#}");
            let blocked = error.downcast_ref::<CoherenceBlocked>();
            let application = if blocked.is_some()
                || message.contains("source changed")
                || message.contains("source has changed")
                || message.contains("source drift")
            {
                ApplicationState::BlockedBySourceDrift
            } else {
                ApplicationState::Failed
            };
            if let Some(blocked) = blocked {
                remember_validity(&mut run, &blocked.validity);
            }
            let payload = serde_json::json!({
                "application": application,
                "error": message,
                "coherence": blocked.map(|blocked| &blocked.validity),
            });
            persist_application_failed(state, run, &normalized_label, application, payload)?;
            return Err(error);
        }
    };
    if let Some(overridden) = &overridden {
        run.coherence
            .get_or_insert(CoherenceRecord {
                version: 1,
                refreshed_from: None,
                validity: None,
                first_invalid_at: None,
                overridden: None,
            })
            .overridden = Some(overridden.clone());
    }
    let run = persist_applied(
        state,
        run,
        &normalized_label,
        &report,
        validity.as_ref(),
        authority,
    )?;
    if !quiet {
        println!(
            "Applied Candidate {} to {} ({} file(s) changed).",
            normalized_label,
            run.source_path.display(),
            report.files_changed
        );
    }
    Ok(overridden)
}

/// Commit `auto_apply.skipped {reason}`. Nothing about the outcome changes:
/// the run stays exactly as ready and unapplied as it was.
fn persist_skip(state: &State, mut run: RunRecord, reason: &str) -> Result<ApplyOutcome> {
    let database = Database::open(state.db_path())?;
    persist_event(
        state,
        &database,
        EventRecord {
            run_id: run.id.clone(),
            candidate_label: run
                .candidates
                .first()
                .map(|candidate| candidate.label.clone()),
            event_type: "auto_apply.skipped".into(),
            timestamp: Utc::now(),
            payload: serde_json::json!({"reason": reason}),
            ..EventRecord::default()
        },
        &mut run,
    )?;
    Ok(ApplyOutcome::Skipped {
        reason: reason.into(),
    })
}

/// Commit `auto_apply.blocked {reason, coherence}`: the gate, or the
/// authorization predicate on top of it, refused. The source stays
/// untouched and the run remains reviewable.
fn persist_blocked(
    state: &State,
    mut run: RunRecord,
    reason: String,
    validity: Option<Validity>,
) -> Result<ApplyOutcome> {
    if let Some(validity) = &validity {
        remember_validity(&mut run, validity);
    }
    run.outcome.application = ApplicationState::BlockedBySourceDrift;
    run.outcome.phase = RunPhase::Finished;
    let database = Database::open(state.db_path())?;
    persist_event(
        state,
        &database,
        EventRecord {
            run_id: run.id.clone(),
            candidate_label: run
                .candidates
                .first()
                .map(|candidate| candidate.label.clone()),
            event_type: "auto_apply.blocked".into(),
            timestamp: Utc::now(),
            payload: serde_json::json!({
                "reason": reason.clone(),
                "coherence": validity.clone(),
            }),
            ..EventRecord::default()
        },
        &mut run,
    )?;
    Ok(ApplyOutcome::Blocked { reason, validity })
}

/// Commit `application.failed {application, error, applied_by}` for the
/// policy path: an apply that was authorized still failed (for example Git).
fn persist_apply_failed(
    state: &State,
    run: RunRecord,
    candidate_label: &str,
    error: String,
) -> Result<ApplyOutcome> {
    let payload = serde_json::json!({
        "application": ApplicationState::Failed,
        "error": error.clone(),
        "applied_by": "auto_apply",
    });
    persist_application_failed(
        state,
        run,
        candidate_label,
        ApplicationState::Failed,
        payload,
    )?;
    Ok(ApplyOutcome::Failed { error })
}

/// What the gate authorizes, ahead of the actual `git apply`. Kept separate
/// from the apply call itself so a fence failure during the apply can be
/// re-decided against a fresh gate rather than reusing a stale digest.
enum Authorization {
    /// Byte-identical world: the fingerprint fence applies.
    Fingerprint,
    /// A moved world whose evidence is complete for this run's exact digest.
    Digest(Validity),
}

/// Stage 2: `coherence::gate` plus the stricter authorization predicate for
/// automatic application (part 5.2 of the plan). Runs no external work beyond
/// what `gate` itself does (L2 only when the world moved and L0/L1 pass).
fn decide(
    run: &RunRecord,
    candidate_label: &str,
    run_dir: &Path,
) -> Result<Result<Authorization, (String, Option<Validity>)>> {
    Ok(
        // A policy never overrides a verdict.
        match crate::coherence::gate(run, candidate_label, run_dir, false)? {
            // `config_unreadable` was already ruled out in stage 1: a config
            // that was readable there and is unreadable here would be an
            // extraordinary race, not a case this predicate distinguishes.
            AcceptGate::Legacy => {
                if source::fingerprint_tree(&run.source_path)? == run.source_fingerprint {
                    Ok(Authorization::Fingerprint)
                } else {
                    Err(("strict_mode_drift".into(), None))
                }
            }
            AcceptGate::Compatible(validity) => {
                if !validity.world_changed || validity.analysis == AnalysisLevel::Integration {
                    Ok(Authorization::Digest(validity))
                } else {
                    Err(("integration_evidence_missing".into(), Some(validity)))
                }
            }
            AcceptGate::Blocked(validity)
            | AcceptGate::Overridden {
                overridden: validity,
                ..
            } => {
                let reason = if validity.decision == Decision::Stop {
                    "verdict_stop"
                } else {
                    "verdict_refresh"
                };
                Err((reason.into(), Some(validity)))
            }
        },
    )
}

/// The typed fence texts `source::apply_checked` raises when the world moves
/// between authorization and the real `git apply`. Matched by text because
/// the fence is inside `anyhow::ensure!`, same as `apply_locked` already does
/// for the human path.
/// Planned goals (removed in 0.4.1) delivered through an integrity chain this
/// version no longer verifies, so their results are never applied.
fn was_planned(run: &RunRecord) -> bool {
    run.execution
        .as_ref()
        .is_some_and(|goal| goal.provenance == "planned_policy_chain")
}

fn is_fence_failure(message: &str) -> bool {
    message.contains("source changed during apply validation")
        || message.contains("source has changed since this run was created")
}

/// Apply `run_id`'s sole candidate under the auto-apply policy, if and only if
/// the evidence is complete for the exact world being modified (see
/// `docs/plan-0.3-auto-apply-and-attach.md`, part 5.2). Every returned
/// variant has already been persisted; the caller only presents it.
pub fn auto_apply(state: &State, run_id: &str) -> Result<ApplyOutcome> {
    let resolved_run_id = state.resolve_run_id(run_id)?;
    let run_lock = OperationLock::acquire_wait(
        &state.run_dir(&resolved_run_id).join(".operation.lock"),
        "run has a foreground owner",
        Duration::from_secs(5),
    );
    // A human review may hold this lock; writing anything here would bump
    // the run's revision under them, so a busy run lock persists nothing.
    let Ok(_run_lock) = run_lock else {
        return Ok(ApplyOutcome::Skipped {
            reason: "run_busy".into(),
        });
    };

    let run = state.load_run(&resolved_run_id)?;
    let run_dir = state.run_dir(&run.id);

    // A run that is not a pending, unapplied delivery may still be owned by a
    // live loop that holds no operation lock (comparison and planned runs),
    // or may already be applied. Writing an event would bump its revision
    // under that owner, so this case persists nothing, like `run_busy`.
    if !(run.outcome.lifecycle == LifecycleState::Finished
        && run.outcome.work_result == WorkResult::Ready
        && run.status == RunStatus::ReadyForEvaluation
        && run.outcome.review == ReviewState::Pending
        && run.outcome.application == ApplicationState::NotApplied)
    {
        return Ok(ApplyOutcome::Skipped {
            reason: "not_ready".into(),
        });
    }
    if run.candidates.len() != 1 {
        return persist_skip(state, run, "not_sole_candidate");
    }
    if was_planned(&run) {
        return persist_skip(state, run, "delivery_unverifiable");
    }
    let Some(config) = run_config(&run_dir) else {
        return persist_skip(state, run, "config_unreadable");
    };
    if config.checks.verify.is_empty() {
        return persist_skip(state, run, "verification_not_configured");
    }
    match run.outcome.verification {
        VerificationState::Passed => {}
        VerificationState::Failed => return persist_skip(state, run, "verification_failed"),
        VerificationState::Inconclusive => {
            return persist_skip(state, run, "verification_inconclusive");
        }
        VerificationState::NotRun => return persist_skip(state, run, "verification_not_run"),
        VerificationState::NotConfigured => {
            return persist_skip(state, run, "verification_not_configured");
        }
    }
    if config.execution.backend == "local" && !run.environment.unsafe_local {
        return persist_skip(state, run, "integration_checks_unavailable");
    }
    // Nothing to apply is not an application: an agent that produced no
    // change (or an attached workspace with no edits) stays reviewable
    // instead of being recorded as applied with zero files.
    let (candidate_label, empty_delta) = {
        let candidate = sole_candidate(&run)?;
        (
            candidate.label.clone(),
            fs::metadata(&candidate.diff_path).is_ok_and(|metadata| metadata.len() == 0),
        )
    };
    if empty_delta {
        return persist_skip(state, run, "empty_delta");
    }
    let source_lock = OperationLock::acquire_wait(
        &source_lock_path(state, &run),
        "another apply operation is already modifying this source",
        Duration::from_secs(run.environment.timeout_secs),
    );
    let Ok(_source_lock) = source_lock else {
        return persist_skip(state, run, "source_busy");
    };

    let mut retried = false;
    loop {
        let authorization = match decide(&run, &candidate_label, &run_dir)? {
            Ok(authorization) => authorization,
            Err((reason, validity)) => return persist_blocked(state, run, reason, validity),
        };
        let validity = match &authorization {
            Authorization::Fingerprint => None,
            Authorization::Digest(validity) => Some(validity.clone()),
        };
        let applied = match &authorization {
            Authorization::Fingerprint => source::safe_apply(&run, &candidate_label),
            Authorization::Digest(validity) => {
                source::apply_validated(&run, &candidate_label, &validity.world_digest)
            }
        };
        match applied {
            Ok(report) => {
                persist_applied(
                    state,
                    run,
                    &candidate_label,
                    &report,
                    validity.as_ref(),
                    ApplyAuthority::AutoApply,
                )?;
                return Ok(ApplyOutcome::Applied { report, validity });
            }
            Err(error) => {
                let message = format!("{error:#}");
                if is_fence_failure(&message) {
                    if !retried {
                        retried = true;
                        continue;
                    }
                    return persist_blocked(
                        state,
                        run,
                        "world_moved_during_validation".into(),
                        validity,
                    );
                }
                return persist_apply_failed(state, run, &candidate_label, message);
            }
        }
    }
}
