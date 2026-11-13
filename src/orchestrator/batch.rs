//! Batch landing: `dispatch accept` with no id, when two or more results of a
//! Git project wait for review and can land together. They are ordered so a
//! change comes before what uses it, each is judged against the source with
//! every result before it, the project's checks run once on all of them, and
//! only a verified prefix lands, as one patch. Each landed result is then
//! recorded as a single accept records it, naming the batch.
//!
//! One approval covers the batch: `accept` proposes it and, in a terminal,
//! asks; confirming runs exactly what `accept --expect <batch digest>` runs.

use std::{
    cmp::Reverse,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::Utc;
use sha2::{Digest, Sha256};

use super::{
    ApplyAuthority, DIGEST_SHOWN, ReviewCommand, apply, check_line, delta_digest, digest_line,
    locked_delivery, one_line, reason_line, record_review, s0, sole_candidate, transition,
    waits_for_decision,
};
use crate::{
    AppliedBy, Decision, Reason, ReasonCode, RunRecord, RunStatus, SourceKind, VerificationState,
    coherence::{
        self, WorkView, facts,
        integration::{self, ChecksSkipped, MergedRun},
        interactions::{self, Interaction, Rule, Side},
        measure::{self, landing},
        memo, run_config,
        world::Worlds,
    },
    config::Config,
    db::Database,
    lock::OperationLock,
    presenter,
    source::{self, ApplyReport, ExpectedWorld, Patch, Stage, check_workspace_path},
    state::{self, State},
};

/// The results of one Git project that may land together: each finished,
/// ready, unapplied and waiting for review, its own checks passed or none
/// configured, and frozen with the same checks, execution and authority.
pub(super) struct Eligible {
    root: PathBuf,
    /// In id order, the order their locks are taken in.
    runs: Vec<RunRecord>,
    /// What else waits for review here, each with why it is not in the batch.
    others: Vec<(RunRecord, String)>,
    config: Config,
    /// Why the merged checks do not run on the batch, when they do not.
    skipped: Option<ChecksSkipped>,
}

/// The `Eligible` results at `source_path`, or `None` when fewer than two
/// are, as for a plain directory: an ID-less accept is then the single
/// accept it always was.
pub(super) fn eligible(state: &State, source_path: &Path) -> Result<Option<Eligible>> {
    let root = source::resolve_source(Some(source_path))?;
    let mut groups: Vec<(
        serde_json::Value,
        Config,
        Option<ChecksSkipped>,
        Vec<RunRecord>,
    )> = Vec::new();
    let mut others = Vec::new();
    for run in state.load_source_runs(&root)? {
        if !waits_for_decision(&run) {
            continue;
        }
        match frozen(state, &run)? {
            Err(why) => others.push((run, why)),
            Ok((config, skipped)) => {
                let key = serde_json::json!([
                    config.checks.verify,
                    config.execution,
                    skipped.as_ref().map(ToString::to_string),
                ]);
                match groups.iter_mut().find(|group| group.0 == key) {
                    Some(group) => group.3.push(run),
                    None => groups.push((key, config, skipped, vec![run])),
                }
            }
        }
    }
    // The largest group is the batch; of two as large, the one with the
    // result that finished first.
    groups.sort_by_cached_key(|(.., runs)| (Reverse(runs.len()), runs.iter().map(finished).min()));
    let mut groups = groups.into_iter();
    let Some((_, config, skipped, mut runs)) = groups.next().filter(|(.., runs)| runs.len() >= 2)
    else {
        return Ok(None);
    };
    for (.., rest) in groups {
        others.extend(rest.into_iter().map(|run| {
            let why = "it was frozen with other checks, execution or authority than these";
            (run, why.to_owned())
        }));
    }
    runs.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(Some(Eligible {
        root,
        runs,
        others,
        config,
        skipped,
    }))
}

/// The frozen configuration `run` lands with and why the merged checks would
/// not run on it, or why it cannot land with others.
fn frozen(
    state: &State,
    run: &RunRecord,
) -> Result<std::result::Result<(Config, Option<ChecksSkipped>), String>> {
    let run_dir = state.run_dir(&run.id);
    let candidate = sole_candidate(run)?;
    let why = match run.outcome.verification {
        _ if !matches!(run.source_kind, SourceKind::Git | SourceKind::GitWorktree) => {
            "its source was a plain directory when it began"
        }
        VerificationState::Failed => "its own checks failed",
        VerificationState::Inconclusive => "its own checks were inconclusive",
        VerificationState::NotRun => "its own checks did not run",
        _ if !matches!(
            run.status,
            RunStatus::ReadyForEvaluation | RunStatus::Evaluated
        ) || apply::was_planned(run) =>
        {
            "it cannot be applied"
        }
        _ if fs::metadata(&candidate.diff_path)?.len() == 0 => "it changes nothing",
        _ => match (
            run_config(&run_dir),
            integration::checks_skipped(run, &candidate.label, &run_dir, Some(true))?,
        ) {
            (None, _) | (_, Some(ChecksSkipped::ConfigUnreadable)) => {
                "its frozen configuration cannot be read"
            }
            (_, Some(ChecksSkipped::Strict)) => {
                "accept is strict for it (coherence.accept: strict)"
            }
            (Some(config), skipped) => return Ok(Ok((config, skipped))),
        },
    };
    Ok(Err(why.to_owned()))
}

/// When `run` finished, then its id: the order of results nothing else
/// orders.
fn finished(run: &RunRecord) -> (chrono::DateTime<Utc>, String) {
    (run.completed_at.unwrap_or(run.created_at), run.id.clone())
}

/// Every run's lock, in id order, then the source lock; the runs as they
/// are under them that still wait for a decision.
fn lock(
    state: &State,
    eligible: &Eligible,
) -> Result<(Vec<OperationLock>, OperationLock, Vec<RunRecord>)> {
    let mut locks = Vec::new();
    let mut runs = Vec::new();
    for run in &eligible.runs {
        let command = ReviewCommand {
            run_id: run.id.clone(),
            candidate_id: sole_candidate(run)?.id.clone(),
            revision: run.state_revision,
        };
        let (run, lock) = locked_delivery(state, &command, false, false)?;
        locks.push(lock);
        if waits_for_decision(&run) {
            runs.push(run);
        }
    }
    anyhow::ensure!(
        !runs.is_empty(),
        "no result waits for review here any more; nothing was decided"
    );
    let path = apply::source_lock_path(state, &eligible.runs[0]);
    if OperationLock::is_held(&path)? {
        eprintln!(
            "Accept: waiting for the source lock on {}",
            eligible.root.display()
        );
    }
    let wait = eligible.runs.iter().map(|run| run.environment.timeout_secs);
    let source_lock = OperationLock::acquire_wait(
        &path,
        "another operation is using this source",
        Duration::from_secs(wait.max().unwrap_or(5)),
    )?;
    Ok((locks, source_lock, runs))
}

/// A result in the batch, its Δ digest, and why it comes where it does.
struct Member {
    run: RunRecord,
    delta: String,
    why: String,
}

/// The batch as judged under the source lock.
struct Proposal {
    members: Vec<Member>,
    /// Results that cannot land with the others now, with the verdict and
    /// its reasons.
    left_out: Vec<(RunRecord, Decision, Vec<Reason>)>,
    /// The source as staged (T0), then with each member's Δ in turn.
    trees: Vec<String>,
    /// The source's world digest when it was staged: the landing's fence.
    world: String,
    /// SHA-256 over the ordered (run id, Δ digest) pairs.
    digest: String,
}

/// What a run is to the facts layer.
fn view<'a>(run: &'a RunRecord, source: &'a Path) -> Result<WorkView<'a>> {
    Ok(WorkView {
        source,
        delta_patch: &sole_candidate(run)?.diff_path,
        baseline: &run.baseline_path,
        baseline_commit: &run.baseline_commit,
    })
}

/// Order `runs`, then judge each against the source with every member before
/// it, in the warm check workspace (`workspace`), observed as a Git source of
/// its own. The verdicts are this proposal's only: nothing is stored. The
/// caller holds the source lock; `worlds` observes the root.
fn propose(
    eligible: &Eligible,
    runs: Vec<RunRecord>,
    workspace: &Path,
    worlds: &mut Worlds,
) -> Result<Proposal> {
    let root = &eligible.root;
    let memo = facts::Memo::default();
    let mut works = Vec::new();
    for run in &runs {
        let fresh = interactions::footprint(&view(run, root)?, &memo)?;
        works.push((run.id.clone(), interactions::settle(fresh, true, None).0));
    }
    let ordered = order(&runs, &interactions::edges(&works));
    let first = &runs[0];
    let world = worlds
        .observe(
            root,
            &first.baseline_path,
            &first.baseline_commit,
            &first.source_kind,
        )?
        .digest;
    let mut trees = vec![source::prepare_check_workspace(
        root,
        &first.source_kind,
        workspace,
        &[],
    )?];
    // As for any merged tree: 3-way only from a source whose tree is exact.
    let three_way = memo::exact(root)?;
    let git = || source::check_git_command(workspace, workspace);
    let mut staged = Worlds::default();
    let (mut members, mut left_out) = (Vec::new(), Vec::new());
    let mut runs: Vec<Option<RunRecord>> = runs.into_iter().map(Some).collect();
    for (at, why) in ordered {
        let run = runs[at].take().expect("each run is ordered once");
        let tree = trees.last().expect("the staged source");
        source::check_out(workspace, tree)?;
        staged.moved();
        let world = staged.observe(
            workspace,
            &run.baseline_path,
            &run.baseline_commit,
            &SourceKind::Git,
        )?;
        let work = WorkView {
            source: workspace,
            ..view(&run, root)?
        };
        let validity = coherence::evaluate_with(&world, &work, &memo)?;
        if validity.decision != Decision::Continue {
            left_out.push((run, validity.decision, validity.reasons));
            continue;
        }
        if let Some(why) = eligible
            .skipped
            .as_ref()
            .filter(|_| coherence::rests_on_merged_checks(&validity))
        {
            let reason = Reason {
                code: ReasonCode::AnalysisUncertain,
                fact_id: None,
                path: None,
                detail: format!("merged checks cannot run — {why}"),
            };
            left_out.push((run, Decision::Refresh, vec![reason]));
            continue;
        }
        let delta = &sole_candidate(&run)?.diff_path;
        let patch = Patch::new(delta, &run.baseline_path);
        match source::stage(&git, tree, &[patch], three_way)? {
            Stage::Staged { tree } => {
                trees.push(tree);
                let delta = delta_digest(delta)?;
                members.push(Member { run, delta, why });
            }
            Stage::Conflict { output } => {
                let reason = Reason {
                    code: ReasonCode::PatchConflict,
                    fact_id: None,
                    path: None,
                    detail: one_line(&output, 300),
                };
                left_out.push((run, Decision::Refresh, vec![reason]));
            }
        }
    }
    let mut digest = Sha256::new();
    for member in &members {
        digest.update(format!("{} {}\n", member.run.id, member.delta));
    }
    Ok(Proposal {
        members,
        left_out,
        trees,
        world,
        digest: hex::encode(digest.finalize()),
    })
}

/// Kahn's algorithm over the interactions among `runs` that have a direction:
/// a `Uses` writer, or the one writer of a whole `File`, before the other
/// side. Results that are ready together, symmetric interactions and cycles
/// go by finish time, then id. Each index comes with why it is where it is.
fn order(runs: &[RunRecord], edges: &[(&str, &str, Vec<Interaction>)]) -> Vec<(usize, String)> {
    let at = |id: &str| {
        runs.iter()
            .position(|run| run.id == id)
            .expect("an edge between these runs")
    };
    // (writer, reader, the interaction)
    let mut directed = Vec::new();
    for (a, b, found) in edges {
        for interaction in found {
            let (writer, reader) = match (interaction.rule, interaction.writer) {
                (Rule::Uses | Rule::File, Some(Side::A)) => (at(a), at(b)),
                (Rule::Uses | Rule::File, Some(Side::B)) => (at(b), at(a)),
                _ => continue,
            };
            directed.push((writer, reader, interaction));
        }
    }
    let mut left: Vec<usize> = (0..runs.len()).collect();
    left.sort_by_key(|&i| finished(&runs[i]));
    let mut placed = Vec::new();
    while !left.is_empty() {
        let ready = left.iter().position(|&i| {
            !directed
                .iter()
                .any(|(writer, reader, _)| *reader == i && left.contains(writer))
        });
        placed.push(left.remove(ready.unwrap_or(0)));
    }
    let ids: Vec<&str> = runs.iter().map(|run| run.id.as_str()).collect();
    let short = state::short_ids(&ids);
    // `explain` speaks from `Side::A`'s view of the other Work, "it".
    let said = |interaction: &Interaction, writer: Side| {
        interactions::explain(&Interaction {
            writer: Some(writer),
            ..interaction.clone()
        })
    };
    placed
        .iter()
        .enumerate()
        .map(|(position, &i)| {
            let (before, after) = (&placed[..position], &placed[position + 1..]);
            let why = if let Some((writer, _, interaction)) = directed
                .iter()
                .find(|(writer, reader, _)| *reader == i && before.contains(writer))
            {
                format!(
                    "after {}: {}",
                    short[ids[*writer]],
                    said(interaction, Side::B)
                )
            } else if let Some((_, reader, interaction)) = directed
                .iter()
                .find(|(writer, reader, _)| *writer == i && after.contains(reader))
            {
                format!(
                    "before {}: {}",
                    short[ids[*reader]],
                    said(interaction, Side::A)
                )
            } else {
                "by finish time".to_owned()
            };
            (i, why)
        })
        .collect()
}

/// Propose the batch: judged under the locks, recorded on every eligible
/// result as `batch.proposed`, and printed once the locks are let go. In a
/// terminal, a menu with Cancel focused gives the batch digest to accept,
/// or `None` when the person does not confirm. Without a terminal, or when
/// nothing can land, nothing is decided.
pub(super) async fn offer(
    state: &State,
    eligible: &Eligible,
    options: presenter::Options,
) -> Result<Option<String>> {
    let proposal = {
        let (_runs, _source, runs) = lock(state, eligible)?;
        let workspace = check_workspace_path(&state.root, &eligible.root);
        let mut proposal = propose(eligible, runs, &workspace, &mut Worlds::default())?;
        record_proposed(state, &mut proposal)?;
        proposal
    };
    print_proposal(eligible, &proposal);
    let count = proposal.members.len();
    anyhow::ensure!(
        count > 0,
        "none of these results can land now; nothing was decided"
    );
    anyhow::ensure!(
        presenter::suitable(),
        "nothing was decided: to land {} together, run dispatch accept --expect {}",
        results(count),
        &proposal.digest[..DIGEST_SHOWN]
    );
    let question = format!("Land {} together?", results(count));
    if presenter::confirm(&question, "Accept and land them", options).await? {
        Ok(Some(proposal.digest))
    } else {
        println!("Nothing was decided.");
        Ok(None)
    }
}

/// "1 result", "3 results".
fn results(count: usize) -> String {
    match count {
        1 => "1 result".to_owned(),
        count => format!("{count} results"),
    }
}

fn record_proposed(state: &State, proposal: &mut Proposal) -> Result<()> {
    let payload = serde_json::json!({
        "batch": proposal.digest,
        "members": proposal.members.iter().map(|member| serde_json::json!({
            "run_id": member.run.id,
            "delta_digest": member.delta,
            "why": member.why,
        })).collect::<Vec<_>>(),
        "left_out": proposal.left_out.iter().map(|(run, decision, reasons)| serde_json::json!({
            "run_id": run.id,
            "decision": decision,
            "reasons": reasons,
        })).collect::<Vec<_>>(),
    });
    let runs = proposal.members.iter_mut().map(|member| &mut member.run);
    let left_out = proposal.left_out.iter_mut().map(|(run, ..)| run);
    record_each(state, runs.chain(left_out), "batch.proposed", &payload)
}

/// Commit `kind` with `payload` on each of `runs`.
fn record_each<'a>(
    state: &State,
    runs: impl IntoIterator<Item = &'a mut RunRecord>,
    kind: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    let database = Database::open(state.db_path())?;
    for run in runs {
        transition(state, &database, run, kind, payload.clone())?;
    }
    Ok(())
}

fn print_proposal(eligible: &Eligible, proposal: &Proposal) {
    if proposal.members.is_empty() {
        println!("None of these results can land now:");
    } else {
        println!(
            "{} can land together, in this order:",
            results(proposal.members.len())
        );
    }
    for (position, member) in proposal.members.iter().enumerate() {
        println!(
            "  {}. {}  {}  {}",
            position + 1,
            member.run.id,
            digest_line(&member.delta),
            one_line(&member.run.task, 60)
        );
        println!("       {}", member.why);
    }
    let left_out = proposal.left_out.iter().map(|(run, decision, reasons)| {
        let verdict = coherence::verdict(*decision);
        let lines = reasons
            .iter()
            .take(3)
            .map(|reason| format!("{verdict}: {}", one_line(&reason_line(reason), 200)));
        (run, lines.collect::<Vec<_>>())
    });
    let others = eligible
        .others
        .iter()
        .map(|(run, why)| (run, vec![why.clone()]));
    for (heading, listed) in [
        ("Left out:", left_out.collect::<Vec<_>>()),
        (
            "Not in the batch; accept each on its own:",
            others.collect(),
        ),
    ] {
        if listed.is_empty() {
            continue;
        }
        println!("{heading}");
        for (run, lines) in listed {
            println!("  {}  {}", run.id, one_line(&run.task, 60));
            for line in lines {
                println!("       {line}");
            }
        }
    }
    if proposal.members.is_empty() {
        return;
    }
    let shown = &proposal.digest[..DIGEST_SHOWN];
    match &eligible.skipped {
        None => println!(
            "Batch {shown}. The project's checks run once on all of them together; if they fail, only the results before the first one that fails land."
        ),
        Some(why) => println!(
            "Batch {shown}. Merged checks: not run — {why}; the file and symbol checks above decide it."
        ),
    }
}

/// `dispatch accept --expect <prefix>` with no id, for `eligible`: judge the
/// batch again under the locks and refuse unless it is the batch `expected`
/// names. Then run the merged checks once on all of it, and land the verified
/// prefix as one patch under the fence of the source as it was judged.
pub(super) async fn land(
    state: &State,
    eligible: Eligible,
    expected: &str,
    reasons: Vec<String>,
    explanation: Option<String>,
) -> Result<()> {
    let waiting = eligible.others.iter().map(|(run, _)| run);
    for run in eligible.runs.iter().chain(waiting) {
        let delta = delta_digest(&sole_candidate(run)?.diff_path)?;
        anyhow::ensure!(
            !delta.starts_with(expected),
            "{expected} is the Δ of Work {id}, not a batch, and nothing was applied; to accept that Work alone, name it: dispatch accept {id} --expect {expected}",
            id = run.id
        );
    }
    let reasons = super::normalize_reasons(reasons)?;
    let (_runs, source_lock, runs) = lock(state, &eligible)?;
    let workspace = check_workspace_path(&state.root, &eligible.root);
    let mut worlds = Worlds::default();
    let mut proposal = propose(&eligible, runs, &workspace, &mut worlds)?;
    if proposal.members.is_empty() || !proposal.digest.starts_with(expected) {
        print_proposal(&eligible, &proposal);
        anyhow::bail!(
            "the results that can land together changed since batch {expected}, and nothing was applied; review them above, then accept again"
        );
    }
    let lands = verify(state, &eligible, &mut proposal, &workspace, &source_lock).await?;
    let failed = proposal
        .members
        .get(lands)
        .map(|member| member.run.id.clone());
    if lands == 0 {
        anyhow::bail!(
            "the merged checks fail on the source with {} alone, so nothing was applied; the source's own checks may be failing",
            proposal.members[0].run.id
        );
    }
    let first = &proposal.members[0].run;
    let (root, tree) = (&eligible.root, &proposal.trees[lands]);
    let patch = state
        .run_dir(&first.id)
        .join(format!("batch-{}.patch", &proposal.digest[..DIGEST_SHOWN]));
    diff(&workspace, &proposal.trees[0], tree, &patch)?;
    let touched = changed_paths(&workspace, &proposal.trees[0], tree)?;
    let landings = observe_landings(
        state,
        root,
        &proposal.members[..lands],
        &proposal.world,
        &proposal.digest,
    );
    let fence = ExpectedWorld::Digest(&proposal.world);
    let report = apply::land(state, first, None, &patch, fence, None).map_err(|error| {
        if apply::is_fence_failure(&format!("{error:#}")) {
            error.context("the source changed after accept checked it, so nothing was applied; accept again to check it as it is now")
        } else {
            error
        }
    })?;
    // What landed is what was verified: every path the patch touches holds
    // the verified tree's blob. Once applied it has landed, so a path that
    // does not, or all of them when that cannot be seen, is reported.
    worlds.moved();
    let mut observe = || -> Result<(Vec<String>, String)> {
        let differs = worlds
            .observe(root, &workspace, tree, &first.source_kind)?
            .changes
            .into_iter()
            .map(|change| change.path)
            .filter(|path| touched.contains(path))
            .collect();
        let after = worlds.observe(
            root,
            &first.baseline_path,
            &first.baseline_commit,
            &first.source_kind,
        )?;
        Ok((differs, after.digest))
    };
    let (unconfirmed, after) = match observe() {
        Ok((differs, after)) => (differs, Some(after)),
        Err(_) => (touched, None),
    };
    record_landed(
        state,
        &mut proposal,
        lands,
        &patch,
        after.as_deref(),
        &unconfirmed,
        landings,
        &reasons,
        explanation.as_deref(),
    )?;
    println!(
        "Applied {} to {} as one patch ({} file(s) changed):",
        results(lands),
        root.display(),
        report.files_changed
    );
    for member in &proposal.members[..lands] {
        println!("  {}  {}", member.run.id, one_line(&member.run.task, 60));
    }
    println!("Review recorded for each.");
    if !unconfirmed.is_empty() {
        eprintln!(
            "Warning: not confirmed at {}: it changed as the patch landed; look at it in the source",
            unconfirmed.join(", ")
        );
    }
    match failed {
        Some(id) => anyhow::bail!(
            "Work {id} and the results after it did not land: the merged checks fail with it"
        ),
        None => Ok(()),
    }
}

/// Run the merged checks once on the whole batch. When they fail, bisect the
/// prefixes for the boundary between the longest one that passes and the
/// first that fails, checking each tree at most once: the source itself is
/// taken to pass. Each run is recorded on the first member, with the batch.
/// Returns how many members land.
async fn verify(
    state: &State,
    eligible: &Eligible,
    proposal: &mut Proposal,
    workspace: &Path,
    source_lock: &OperationLock,
) -> Result<usize> {
    let count = proposal.members.len();
    let mut checked = Vec::new();
    let (mut good, mut bad) = (0, count);
    if let Some(why) = &eligible.skipped {
        println!("Merged checks: not run — {why}");
        good = count;
    } else {
        let scratch = tempfile::tempdir().context("failed to create a temporary directory")?;
        let mut prefix = count;
        loop {
            let ran = check(
                state,
                eligible,
                proposal,
                workspace,
                scratch.path(),
                prefix,
                source_lock,
            )
            .await?;
            if ran.passed() {
                good = prefix;
            } else {
                bad = prefix;
            }
            checked.push((prefix, ran));
            if good == count || bad - good <= 1 {
                break;
            }
            prefix = (good + bad) / 2;
        }
    }
    let failed = (good < count).then(|| &proposal.members[bad - 1].run);
    if let Some(run) = failed {
        let ran = checked
            .iter()
            .find(|(prefix, _)| *prefix == bad)
            .map(|(_, ran)| ran)
            .expect("the failing prefix was checked");
        println!(
            "The first result the merged checks fail with: {} ({}); logs: {}",
            run.id,
            one_line(&run.task, 60),
            ran.logs.display()
        );
    }
    let payload = serde_json::json!({
        "batch": proposal.digest,
        "tree": proposal.trees[count],
        "passed": eligible.skipped.is_none().then_some(good == count),
        "runs": checked.iter().filter(|(_, ran)| ran.reused.is_none()).count(),
        "lands": good,
        "failed": failed.map(|run| run.id.clone()),
    });
    let members = proposal.members.iter_mut().map(|member| &mut member.run);
    record_each(state, members, "batch.verified", &payload)?;
    let database = Database::open(state.db_path())?;
    let vouches = checked.into_iter().find(|(prefix, _)| *prefix == good);
    if let Some((_, ran)) = vouches.filter(|_| good > 0) {
        let first = proposal.members[0].run.id.clone();
        for member in &mut proposal.members[1..good] {
            let mut payload = match &ran.reused {
                Some(_) => ran.event("accept", &member.delta).1,
                None => serde_json::json!({
                    "by": "accept",
                    "reused_run": first,
                    "logs": ran.logs,
                    "passed_at": Utc::now(),
                    "key": null,
                    "tree": ran.tree_id,
                    "source": ran.source,
                    "delta_digest": member.delta,
                    "passed": true,
                }),
            };
            payload["batch"] = serde_json::json!(proposal.digest);
            transition(state, &database, &mut member.run, "checks.reused", payload)?;
        }
    }
    Ok(good)
}

/// Run the merged checks on the source with the first `prefix` members, or
/// reuse a recorded pass of that exact tree, and record it on the first
/// member with the batch. The tree is built as for any accept, from the
/// source and one patch, here the prefix's diff from T0, and must be the tree
/// the proposal staged.
#[allow(clippy::too_many_arguments)]
async fn check(
    state: &State,
    eligible: &Eligible,
    proposal: &mut Proposal,
    workspace: &Path,
    scratch: &Path,
    prefix: usize,
    source_lock: &OperationLock,
) -> Result<MergedRun> {
    let tree = &proposal.trees[prefix];
    let patch = scratch.join(format!("{prefix}.patch"));
    diff(workspace, &proposal.trees[0], tree, &patch)?;
    let first = &proposal.members[0];
    let run_dir = state.run_dir(&first.run.id);
    let ran = integration::run_or_reuse(
        &first.run,
        &patch,
        &eligible.config,
        &run_dir,
        &integration::new_logs(&run_dir),
        Some(source_lock),
    )
    .await?;
    anyhow::ensure!(
        ran.tree_id == *tree,
        "the source changed while the merged checks ran, so nothing was applied; accept again to check it as it is now"
    );
    let (kind, mut payload) = ran.event("accept", &first.delta);
    payload["batch"] = serde_json::json!(proposal.digest);
    payload["members"] = serde_json::json!(
        proposal.members[..prefix]
            .iter()
            .map(|member| &member.run.id)
            .collect::<Vec<_>>()
    );
    let database = Database::open(state.db_path())?;
    transition(
        state,
        &database,
        &mut proposal.members[0].run,
        kind,
        payload,
    )?;
    let count = proposal.members.len();
    let with = if prefix == count {
        results(count)
    } else {
        format!("the first {prefix} of {count} results")
    };
    let outcome = match &ran.reused {
        Some(entry) => entry.describe(),
        None if ran.passed() => "passed".to_owned(),
        None => "failed".to_owned(),
    };
    println!(
        "Merged checks on the source with {with}: {outcome}; logs: {}",
        ran.logs.display()
    );
    for result in &ran.results {
        println!("{}", check_line(result));
    }
    Ok(ran)
}

/// The landing observation of each of `landed`, about to land together in
/// `batch` on the world `before`: the owner's view of the Work in progress
/// is read once, and members that land together are not each other's
/// counterparts. As for a single accept, a failure is printed and nothing is
/// recorded for that member.
fn observe_landings(
    state: &State,
    root: &Path,
    landed: &[Member],
    before: &str,
    batch: &str,
) -> Vec<Option<measure::Landing>> {
    let ids: Vec<String> = landed.iter().map(|member| member.run.id.clone()).collect();
    let id_strs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let (view, counterparts) = match apply::owner_view(state, root, &id_strs) {
        Ok(view) => view,
        Err(error) => {
            eprintln!("Could not observe how this Work landed: {error:#}");
            return landed.iter().map(|_| None).collect();
        }
    };
    landed
        .iter()
        .map(|member| {
            let patch = fs::read(&sole_candidate(&member.run).ok()?.diff_path)
                .inspect_err(|error| eprintln!("Could not observe how this Work landed: {error:#}"))
                .ok()?;
            let landing = landing::capture(
                &member.run,
                AppliedBy::Human,
                &patch,
                Some(before),
                view.clone(),
                &counterparts,
            );
            Some(landing::in_batch(landing, batch, &ids))
        })
        .collect()
}

/// Record the first `lands` members as landed together: per member as a
/// single accept records it (`result.applied`, `interaction.landed` from
/// `landings`, the workspace release and `review.accepted`), each naming the
/// batch, with one commit in `refs/dispatch/source` for all of them. `after`
/// is the world right after the landing, when it could be observed, and
/// `unconfirmed` the paths not seen to hold what was verified.
#[allow(clippy::too_many_arguments)]
fn record_landed(
    state: &State,
    proposal: &mut Proposal,
    lands: usize,
    patch: &Path,
    after: Option<&str>,
    unconfirmed: &[String],
    landings: Vec<Option<measure::Landing>>,
    reasons: &[String],
    explanation: Option<&str>,
) -> Result<()> {
    let digest = proposal.digest.clone();
    let tree = proposal.trees[lands].clone();
    let landed = &mut proposal.members[..lands];
    // Like the measurement, the source record never fails the landing.
    if let Err(error) = s0::record_together(
        state,
        &landed.iter().map(|member| &member.run).collect::<Vec<_>>(),
        &format!(
            "dispatch: {} landed together (batch {})",
            results(lands),
            &digest[..DIGEST_SHOWN]
        ),
    ) {
        eprintln!("Could not record the source after this landing: {error:#}");
    }
    for (member, landing) in landed.iter_mut().zip(landings) {
        let candidate = sole_candidate(&member.run)?;
        let report = ApplyReport {
            candidate_label: candidate.label.clone(),
            files_changed: candidate.diff_stats.files_changed,
            landed_patch: None,
            unconfirmed: Vec::new(),
        };
        let landing = landing.map(|landing| measure::Landing {
            world_after: after.map(str::to_owned),
            ..landing
        });
        member.run = apply::persist_applied(
            state,
            member.run.clone(),
            &report.candidate_label,
            &report,
            None,
            ApplyAuthority::Human,
            landing,
            Some(&digest),
        )?;
        record_review(
            state,
            &mut member.run,
            true,
            reasons.to_vec(),
            explanation.map(str::to_owned),
            Some(&digest),
        )?;
    }
    let payload = serde_json::json!({
        "batch": digest,
        "members": landed.iter().map(|member| &member.run.id).collect::<Vec<_>>(),
        "tree": tree,
        "patch": patch,
        "patch_digest": delta_digest(patch)?,
        "world_before": proposal.world,
        "world_after": after,
        "confirmed": unconfirmed.is_empty(),
        "unconfirmed": unconfirmed,
    });
    let members = landed.iter_mut().map(|member| &mut member.run);
    record_each(state, members, "batch.landed", &payload)
}

/// The binary, full-index diff from `from` to `to` in the check workspace,
/// written to `out`. `stage` keeps every object either tree needs there.
fn diff(workspace: &Path, from: &str, to: &str, out: &Path) -> Result<()> {
    let mut diff = source::check_git_command(workspace, workspace);
    diff.args([
        "diff",
        "--binary",
        "--full-index",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        from,
        to,
        "--",
    ]);
    let patch = source::checked_output(diff, "failed to write the batch's patch")?.stdout;
    fs::write(out, patch).with_context(|| format!("failed to write {}", out.display()))
}

/// The paths that differ between `from` and `to` in the check workspace.
fn changed_paths(workspace: &Path, from: &str, to: &str) -> Result<Vec<String>> {
    let mut diff = source::check_git_command(workspace, workspace);
    diff.args(["diff", "--name-only", "--no-renames", "-z", from, to, "--"]);
    let listed = source::checked_output(diff, "failed to list the batch's paths")?.stdout;
    Ok(listed
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coherence::interactions::{Change, Target};

    fn run(id: &str, finished: &str) -> RunRecord {
        serde_json::from_value(serde_json::json!({
            "id": id, "task": "t", "exact_prompt": "t", "source_path": "/s",
            "source_kind": "git", "source_git_head": null, "source_fingerprint": "f",
            "baseline_path": "/b", "baseline_commit": "c", "status": "ready_for_evaluation",
            "created_at": "2026-11-10T00:00:00Z", "completed_at": finished,
            "environment": {"dispatch_version": "test", "os": "test", "architecture": "test",
                "execution_backend": "local", "timeout_secs": 30, "cpus": 1.0,
                "memory": "1g", "max_parallel": 1},
            "evaluation": null, "applied_candidate": null,
        }))
        .unwrap()
    }

    fn interaction(rule: Rule, writer: Option<Side>) -> Interaction {
        Interaction {
            rule,
            writer,
            target: Target::Symbol {
                path: "src/auth.rs".into(),
                name: "validate".into(),
            },
            change: writer.map(|_| Change::Signature),
            lines: None,
        }
    }

    /// A writer comes before what relies on it, whenever each finished; what
    /// nothing orders, symmetric interactions and cycles go by finish time.
    #[test]
    fn kahn_orders_writers_first_and_the_rest_by_finish_time() {
        let runs = [
            run("A", "2026-11-10T00:03:00Z"),
            run("B", "2026-11-10T00:01:00Z"),
            run("C", "2026-11-10T00:02:00Z"),
        ];
        let uses = interaction(Rule::Uses, Some(Side::A));
        let both = interaction(Rule::SameDeclaration, None);
        let edges = [("A", "C", vec![uses.clone()]), ("B", "C", vec![both])];
        let said = |text: &str| text.to_owned();
        assert_eq!(
            order(&runs, &edges),
            [
                (1, said("by finish time")),
                (
                    0,
                    said(
                        "before C: this Work changes the signature of validate (src/auth.rs), which it uses"
                    )
                ),
                (
                    2,
                    said(
                        "after A: it changes the signature of validate (src/auth.rs), which this Work uses"
                    )
                ),
            ]
        );

        let placed = |edges: &[(&str, &str, Vec<Interaction>)]| -> Vec<usize> {
            order(&runs, edges).into_iter().map(|(i, _)| i).collect()
        };
        // The one writer of a whole file comes first too; C, which nothing
        // orders and finished before A, is ready before either.
        let file = Interaction {
            target: Target::File {
                path: "data.json".into(),
            },
            ..interaction(Rule::File, Some(Side::A))
        };
        assert_eq!(placed(&[("A", "B", vec![file])]), [2, 0, 1]);
        // A cycle is broken by finish time: B, which finished first, leads.
        let back = interaction(Rule::Uses, Some(Side::B));
        assert_eq!(placed(&[("A", "B", vec![uses, back])]), [2, 1, 0]);
        assert_eq!(placed(&[]), [1, 2, 0]);
    }
}
