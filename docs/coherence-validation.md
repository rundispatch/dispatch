# Coherence validation: claims, metrics and falsifiers

This page records what Dispatch claims about work coherence today, what is
measured while the feature is used on real work, and what evidence would show
the thesis to be wrong. It exists so that the public story does not drift ahead
of the product. The technical reference is [coherence.md](coherence.md).

## What is claimed today

| Claim | Status |
|---|---|
| A finished result is validated against the source as it is now, and a stale result is never applied | Shipped; `tests/coherence_accept.rs`, `tests/coherence_cli.rs` |
| Verdicts carry reasons, including the old and new declaration for a broken symbol fact | Shipped for Rust and Python; file-level facts elsewhere |
| Zero false CONTINUE and zero false REFRESH on the fixture matrix | True for the 36 scenarios in `tests/coherence_matrix.rs`, each on a Git and a plain-directory source |
| Agents need no protocol; nothing is locked; no index | Shipped |
| Watching runs as one local background process per project (`dispatch start`, stopped by `dispatch stop`). It keeps verdicts current and applies only attached Work allowed to integrate; it never launches, refreshes or discovers agents. Since 0.4.7 there is a background process, so "no daemon" is no longer claimed | Shipped; `tests/background.rs` |
| Mid-run detection, and cancellation with `mid_run: stop` | Implemented for allocation runs; observe-only by default; no real run has been stopped |
| Tokens, minutes or money saved | Not claimed. Only wall-clock time after the first invalid verdict is recorded, and it is not cost |
| Works across languages | Not claimed. Symbol facts exist for `.rs` and `.py` only |
| Scales to many concurrent tasks | Not observed |
| Eligible results can be applied automatically under a session or invocation policy; the review stays not performed | Shipped; `tests/auto_apply.rs`, `tests/auto_apply_cli.rs` |
| Two runs finishing against one source serialize on the source lock and the second is re-judged against the new world; an edit during integration checks is fenced and re-validated once | Shipped; `tests/auto_apply_concurrency.rs` |
| Work done by an external agent in its own worktree is judged by the same coherence model as Dispatch-launched work; S0 is the Git merge base with full confidence, or a snapshot at attach with partial confidence | Shipped; `tests/attach_cli.rs`, `tests/attach_wrapped.rs`, `tests/attach_scenarios.rs`; real-agent attach with Claude Code (`claude-sonnet-5`) on 2026-09-22 (`248c09d`) |
| A repo-scoped foreground loop re-evaluates foreign attached work when the root moves and applies eligible work through the same auto-apply path | Shipped; `tests/serve.rs`, `tests/attach_scenarios.rs` |
| While a project is watched, Work not yet integrated is compared and interactions are reported: the same declaration or file, textual overlap, a contract one changes and the other uses, a file one changes and the other relies on. Advisory: no verdict changes and nothing is blocked | Shipped in 0.4.8; exact to the declaration for Rust and Python, to the file elsewhere; `tests/interactions.rs` |
| How often a reported interaction is followed by an invalid verdict is measured locally | Shipped in 0.4.9; `tests/measurement_landing.rs`, `tests/measurement_queries.rs`. No results from real use are published, and "invalidated" is Dispatch's own later verdict, not a confirmed conflict |
| Every eligible runtime session in a watched project ends registered, pending or told it is not tracked, with no half-created Work | Shipped in 0.4.10; `tests/runtime_registration.rs`, `tests/runtime_hooks.rs`; stress trial in `docs/plan-0.4.10.md` |
| The B/A/C scenario holds with real agents: the interaction is reported, accepting B applies it, A is REFRESH with B's broken `validate` fact, C stays CONTINUE | Runner shipped in 0.4.14 (`scripts/harness-trial.sh`, results in `docs/harness-conformance.md`). Local trial on 2026-10-15 with Dispatch 0.4.14: pass for Claude Code 2.1.296, Codex 0.155.1 and Cursor Agent 2026.10.14 on attach, native and each one's own worktree mode. Hermes Agent 0.18.2 passed attach and native; its own worktree mode failed, because Hermes removes its worktree at session end and Dispatch does not recognize it. For Codex and Cursor worktrees, the session's end is not observable by design; the runner accepts that only where Dispatch declares it. Pass or fail against Dispatch's contract only; agents are never compared |
| While a project is watched, Codex and Cursor Agent worktrees that appear become Work with S0 at the merge base, recognized only by Codex's `codex-thread.json` marker or Cursor's documented location and branch | Shipped in 0.4.14; `tests/discovery.rs`; recognized from P0's real Codex 0.155.1 and Cursor Agent 2026.10.14 worktrees on macOS. Their sessions' end is not observable, so their liveness stays unknown |
| `dispatch check --merged` runs the merged checks accept would run, before accept, and changes nothing; `accept --expect` lands only the reviewed Δ | Shipped in 0.4.14; `tests/merged_check.rs`. Used by the 0.4.14 integrator: `accept --expect` for every packet from P3 on, `check --merged` for P3, P2, P6, P6b and R |

| While a project is watched, live Claude Code and Hermes sessions get notes in fixed text, through their hooks, about other Work that changes the same declaration, a declaration they use, or the same lines, and about such Work that landed, naming the recorded source commit. At most one note per Work per ten minutes; each item once | Shipped in 0.4.18; `tests/owner_notes.rs`, `tests/hook_delivery.rs`, `tests/notes_contract.rs`. On 0.5.0's own build, `notes-measured.sql` counted 26 notes delivered and 0 unsettled; the next verdict was REFRESH for 7 (all 7 agree with it), CONTINUE for 7, none yet for 12. Nothing is claimed about whether agents act on notes |
| Attached Git Work's S0 follows the agent's merge or rebase of a recorded source, only with an ancestry proof, so Δ is the agent's own change; every earlier (S0, Δ) stays recomputable | Shipped in 0.4.18; the tests in `src/orchestrator/s0.rs` (merge, rebase, merge in progress, cherry-pick, revert, reset, detached HEAD). Since 0.5.0 a resumed session's Work keeps its S0's time, and the owner checks Work again when its S0 moves with the same Δ |
| When Work lands, a live Claude Code or Hermes agent whose Work uses a declaration the landing changed the signature of or removed, or changes a declaration the landing changed, is asked once, in fixed text, through its runtime's hooks, to merge the recorded commit. Claude Code: as a turn ends (`Stop`), or by waking an idle session within 8 hours of its last turn. Hermes: as an edit turn ends (`pre_verify`) or with the next turn. Never from a whole-file or textual interaction, never a relaunch, a merge on the agent's behalf or a second ask | Shipped in 0.5.0; `src/orchestrator/catchup.rs`, `tests/catchup.rs`, `tests/hook_delivery.rs`. On 0.5.0's own build (Claude Code only), `catchup-measured.sql` counted 2 asks naming 3 landings, 0 repeated, 0 file-level targets; 1 delivered by a wake, 1 by `Stop`; both followed by an S0 move (271 s and 96 s after delivery) and then CONTINUE. Hermes catch-up is shown by stand-in tests only. Four-harness cells: [[C: the coordination cells in harness-conformance.md]] |
| `dispatch accept` without an ID lands the ready results of a Git project that can land together, a change before what uses it, behind one approval; the merged checks run once on the combined tree, and if they fail, only the results before the first failing one land, as one patch | Shipped in 0.5.0; `tests/batch_landing.rs`. On 0.5.0's own build, `batches-measured.sql` counted 2 batches landed (7 and 3 results), each with one merged-check run and no bisection; 15 Works landed behind 7 approvals; 20 suite runs, 1.33 per landed Work |
| A Δ that no longer applies as it is, but merges 3-way from S0 without conflict, is CONTINUE `merged_three_way`, and lands only as the tree the merged checks passed on; an overlapping edit stays REFRESH | Shipped in 0.5.0; `tests/coherence_three_way.rs`, the `coherence::` and `source::` unit tests, on Git 2.39.5 (CI's Linux image) and macOS's Git. On 0.5.0's own build no landing needed it: no event carries `merged_three_way` and no `result.applied` a landed patch. That is read from the run events; no documented query counts it. Nothing is claimed about how often real work needs it |

Every public statement must fit this table. When a row changes, change the
README and the website in the same release.

## Observed while building 0.4.14

These are observations from one build, not rates.

- **Three human overrides of a file-level REFRESH.** Each had one reason: a
  file the patch named without depending on it had changed.
  - P2 and P4: `fact_broken: docs/plan-0.4.14.md changed since the work started`.
    Each patch named the plan only in code comments, and the plan had changed only
    in sections the packet did not rely on.
  - P6b: `fact_broken: Cargo.lock changed since the work started`. The patch named
    `Cargo.lock` only as a CI cache key, and the change was the release's version
    bump.

  A person accepted all three with `--despite-refresh`, so the merged checks had to
  pass first. All three are recorded as `coherence.overridden` with the person's
  explanation. The person judged them false REFRESH from a file mentioned by path:
  the cost of file-level facts that falsifier 4 below is about. No REFRESH in this
  build caught real stale work. Packets that edited the same file in different
  functions (P2 and P3 in `src/orchestrator.rs`) were correctly reported as not
  interacting.

## The question that decides the thesis

L0 is `git apply --check` against the current source. Anyone can run that. The
thesis is that the symbol layer (L1) and the integration checks (L2) add verdicts
that L0 alone cannot give. If, on real work, nearly every REFRESH comes from
`patch_conflict` and nearly every CONTINUE comes from an unchanged world, then
MustHold facts are a model over a trivial check and the wedge is thinner than it
looks. Every measurement below is designed to answer this.

## What to record during real use

All of these are derivable from the `events` table and the run projection; none
needs new machinery. Record them per run where the world moved between the
snapshot and the verdict.

- **Base rate of world movement.** Share of runs whose world moved before accept.
  Near zero means the feature has no pain at single-developer scale and the story
  needs users who run agents in parallel.
- **Verdict source.** For each real verdict, what strict mode, file overlap and L0
  alone would have said. This is the direct measure of whether L1 earns its keep.
- **Reason and analysis level.** Distribution of reason codes and of `symbols`,
  `files_only` and `integration`. The share of REFRESH from `FileFallback` facts
  measures the cost of the language gap.
- **Integration check cost.** Wall-clock per L2 run and how often it flips a
  verdict. From 0.4.16, L2 runs in a warm check workspace per project under the
  source lock, and a tree with a recorded pass is not checked again; count a reuse
  as a reuse, never as time saved.
- **Time to invalid versus attempt length.** From `first_invalid_at` and the
  attempt timestamps. If the world rarely moves during an attempt, mid-run
  detection has no economic value and the product is an accept gate.
- **Human agreement.** For each REFRESH or STOP, record agree or disagree at the
  moment it is acted on. A false CONTINUE is only observable when something breaks
  later; define it as "accepted with CONTINUE, then a failure attributed to the
  moved world" and label it when it happens. Human judgment stays the quality
  signal.
- **Refresh outcomes.** Whether a refreshed run was accepted, rejected or went
  stale again.
- **Bypasses.** Every use of `coherence.accept: strict`, every accept over a
  disagreed verdict (since 0.4.4 each is a `coherence.overridden` event carrying the
  overridden verdict and the human's explanation, which also measures false
  REFRESH), and every run on a path with no watcher (runs made before 0.4.1 by the legacy,
  routed, comparison or planned paths).
- **Auto-apply outcomes and post-hoc disagreement.** Every `auto_apply.skipped` and
  `auto_apply.blocked` reason, every `result.applied`/`application.failed` with
  `applied_by: auto_apply`, and every later `review.rejected` on a run that was
  already `applied_by: auto_apply` (a human disagreeing with a CONTINUE that was
  acted on automatically). Real-agent trials have exercised auto-apply only a few
  times (a Claude Code attach in 0.4.0); treat any auto-apply numbers as anecdotal
  until real use is logged.
- **Attached versus native Work.** The run's `mode` field (`attached` versus the
  native modes) separates work Dispatch launched from work it only observed, so every
  metric above can be split by the two. Record `attach.created`/`attach.finished`/
  `attach.adopted` events separately: how often a `serve`-adopted run (owner gone,
  `owner_state: adopted`) occurs versus a live wrapper finishing its own Work, and the
  S0 confidence (`full`/`partial`) attached results were judged under.

## Measuring interactions (0.4.9)

When Work lands, Dispatch records the interactions the owner had reported
between it and every other piece of unintegrated Work (`interaction.landed`).
It then records Dispatch's own next evaluation of each of those pieces
(`interaction.outcome`). Both are events on the landed run; the payloads are in
`src/coherence/measure.rs`. The documented queries read them, read-only:

```bash
sqlite3 -header -column <state>/dispatch.db < docs/queries/interactions-measured.sql
```

The file only reads: every statement is a `SELECT`. Do not add `-readonly`. The
database uses a write-ahead log, and when no process has it open, a read-only
connection cannot create the log's shared-memory file, so it fails with "unable
to open database file". To query a copy instead, make one with
`sqlite3 <state>/dispatch.db ".backup measure.db"`.

`<state>` is the state directory: `~/.dispatch`, or `--state-dir` or
`DISPATCH_HOME`. Each row starts with the label of its result set:

1. `landings_by_status`: landings by the status of the owner's view.
2. `outcomes_by_class`: outcomes by class.
3. `scorable_predicted_by_invalidated`: among scorable outcomes, predicted
   (at least one interaction) by invalidated, with precision (invalidated among
   predicted) and misses (invalidated, not predicted).
4. `scorable_predicted_by_rule`: the predicted outcomes of 3, by interaction
   rule. An outcome with interactions under two rules counts under both.
5. `landings_without_outcomes`: landings whose counterparts have no outcome.

`tests/measurement_queries.rs` checks every result set against counts derived
by hand from `tests/fixtures/measurement/events.jsonl`.

What these numbers can support, and what they cannot:

- **Invalidated is Dispatch's own later verdict.** It means the next
  evaluation of the counterpart said REFRESH or STOP. It is not a confirmed
  conflict and not a human judgment, so precision measures agreement between
  two parts of Dispatch, not whether the interaction was real. Human agreement
  stays the quality signal.
- **Only scorable outcomes count.** An outcome whose landing was not observed,
  whose counterpart was gone, unanalyzed or already invalid, whose evaluation
  failed, or whose world or Δ moved before it was evaluated, is reported by
  class in result set 2 and never scored.
- **Correlational, not causal.** An invalidation after a landing is not shown
  to come from that landing. The counterpart's prior verdict can predate other
  changes that reached the source before the landing; the landing keeps
  `prior.world_digest` and `world_before` so that this can be checked.
- **Small samples are reported as counts.** Quote precision with its numerator
  and denominator ("3 of 4 predicted"), and quote misses as a count. Do not
  turn a handful of outcomes into a percentage or a trend.
- **Unknown stays unknown.** A counterpart without an outcome (result set 5) is
  neither a hit nor a miss, and a precision with no predicted outcome is NULL,
  not zero. No composite score is computed.

## Measuring notes (0.4.18)

Notes to live agents are recorded as `note.written`, then `note.delivered` or
`note.expired`, and `note.touched` records the first change a Work's Δ made, after a
note, at a target the note named (the declaration, compared by its full text, or the
file for a textual overlap). The payloads are in [attach.md](attach.md#events). The
documented query reads them, read-only, with the same rules as the interaction queries
above:

```bash
sqlite3 -header -column <state>/dispatch.db < docs/queries/notes-measured.sql
```

`docs/queries/notes-measured.sql` lists each delivered note with whether the Work's Δ
later changed its target, whether S0 moved after it, and the Work's next verdict. It
counts them by that verdict; a note "agrees" when the verdict is REFRESH or STOP. Counts
only: what followed a note, not what it caused. `notes_unsettled` must be 0 for
finished Work.

1. `notes_delivered`: one row per note delivered, with `touched`, `s0_moved` and
   `next_verdict` (`none` when there is no later verdict yet).
2. `notes_by_next_verdict`: counts by that verdict, with `agreed`, `touched` and
   `s0_moved`.
3. `notes_unsettled`: notes written and neither delivered nor expired, as `unsettled`
   for Work no longer in progress and `pending` for Work still in progress.

`tests/measurement_queries.rs` checks each result set against counts derived by hand
from `tests/fixtures/measurement/notes.jsonl`. The same limits hold as for
interactions: the verdict is Dispatch's own, not a human judgment; a touch or an S0
move after a note is not shown to come from it; small samples are quoted as counts.
For a `Uses` note, an agent that fixes its own call sites has not touched the target,
so `touched` stays 0.

## Measuring catch-up and batches (0.5.0)

Asks to catch up are recorded on the Work asked, as `catchup.written`, then
`catchup.delivered` or `catchup.expired`; batches as `batch.proposed`, `batch.verified`
and `batch.landed` on each result they name. The payloads are in
[reference.md](reference.md#events). Two documented queries read them, read-only, with the
same rules as the queries above:

```bash
sqlite3 -header -column <state>/dispatch.db < docs/queries/catchup-measured.sql
sqlite3 -header -column <state>/dispatch.db < docs/queries/batches-measured.sql
```

`docs/queries/catchup-measured.sql` counts the asks to catch up written, delivered by each
hook (`via`) and expired (`why`). It also counts any (Work, landing) asked twice and any
file-level target, both of which must be 0. For each delivered ask it gives:

- when its Work's S0 then moved forward to a source holding every landing it named;
- how long after the delivery;
- Dispatch's next verdict on the Work.

It ends with the asks still unsettled. "Followed by" means recorded later, not caused by,
and the next verdict is Dispatch's own.

1. `catchup_counts`: asks written, delivered and expired, each with the landings they
   name; for writes, `repeated_landings`, `symbol_targets` and `file_targets`.
2. `catchup_s0`: one row per delivered ask, with `moved_at`, `seconds` and
   `next_verdict`. A move back never counts, and neither does a move to a source recorded
   before the ask's landings.
3. `catchup_unsettled`: each write whose landings no delivery or expiry has settled yet,
   with those landings. A landing written twice for one Work needs two settlements, so a
   repeated ask shows here too. Work still in progress may have a pending ask; finished
   Work should have none.

`docs/queries/batches-measured.sql` gives three sets:

1. `batch_members`: each batch's proposed, left-out and landed Work, once however many
   offers recorded it.
2. `batch_check_runs`: each verification's merged-check runs, on the whole batch
   (`full_runs`) and bisecting it (`bisection_runs`), with `passed`, `lands` and
   `failed`.
3. `landing_counts`: over all Work, the Work landed, how many in batches, the batches,
   approvals (one per batch, one per single accept), suite runs and reuses, counted as
   0.4.17 and 0.4.18 counted them.

An attached finish that checks in its workspace is counted from the `checks` table, so
only a Work's last checks are seen. No event records a manual step, so neither query
counts one.

`tests/measurement_queries.rs` checks every result set against counts derived by hand
from `tests/fixtures/measurement/catchup.jsonl` and `batches.jsonl`.

Where each of 0.5.0's measured conditions reads from:

- **Catch-up in place:** a `catchup_s0` row with `moved_at` and `next_verdict` `continue`,
  and `catchup_counts` with `repeated_landings` 0 and `file_targets` 0.
- **One decision:** `batch_check_runs` rows with `passed` 1 and `full_runs` equal to
  `verifications`; then `approvals` over `landed_works`.
- **Fast:** `suite_runs` over `landed_works` (at most 1.75), with `reuses` beside them.

## What would falsify the positioning

1. L1 adds no verdict beyond L0 on real repositories over a meaningful sample.
2. Most REFRESH verdicts are overridden, so the gate gets turned off.
3. The world almost never moves during an attempt, so the mid-run and cost story is empty.
4. File fallback on non-Rust, non-Python repositories is noisy enough that users disable it.
5. A harness or merge queue ships a rebase-and-reverify step and users find a bare signal sufficient without reasons.

Decision point: after thirty runs where the world moved, review the record
against these five. If none has triggered, invest in the next language and in
cost accounting. If one or two have, the wedge narrows to an accept gate and the
broader control-plane framing waits.

## Messaging rules

- Lead with the problem and the verdict, publicly: "Keep autonomous software
  work valid while the code moves."
- Agent selection, allocation, execution and verification are how Dispatch
  carries work. They are not separate product stories.
- The "control plane for autonomous software work" framing belongs in the founder
  narrative, not on the homepage or README, until the falsifiers above have been
  tested.
- Never print a savings number that the run's own timestamps cannot support.
