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
| Agents need no protocol; nothing is locked; no daemon or index | Shipped |
| Mid-run detection, and cancellation with `mid_run: stop` | Implemented for allocation runs; observe-only by default; no real run has been stopped |
| Tokens, minutes or money saved | Not claimed. Only wall-clock time after the first invalid verdict is recorded, and it is not cost |
| Works across languages | Not claimed. Symbol facts exist for `.rs` and `.py` only |
| Scales to many concurrent tasks | Not observed |
| Eligible results can be applied automatically under a session or invocation policy; the review stays not performed | Shipped; `tests/auto_apply.rs`, `tests/auto_apply_cli.rs` |
| Two runs finishing against one source serialize on the source lock and the second is re-judged against the new world; an edit during integration checks is fenced and re-validated once | Shipped; `tests/auto_apply_concurrency.rs` |
| Work done by an external agent in its own worktree is judged by the same coherence model as Dispatch-launched work; S0 is the Git merge base with full confidence, or a snapshot at attach with partial confidence | Shipped; `tests/attach_cli.rs`, `tests/attach_wrapped.rs`, `tests/attach_scenarios.rs`; real-agent attach with Claude Code (`claude-sonnet-5`) on 2026-09-22 (`248c09d`) |
| A repo-scoped foreground loop re-evaluates foreign attached work when the root moves and applies eligible work through the same auto-apply path | Shipped; `tests/serve.rs`, `tests/attach_scenarios.rs` |

Every public statement must fit this table. When a row changes, change the
README and the website in the same release.

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
  verdict. L2 builds from scratch with no cache while holding the apply locks.
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
