# Plan: Dispatch 0.4.9, interactions measured

## Context

0.4.8 shows where two pieces of unintegrated Work touch each other. Nobody yet
knows how often that is right. 0.4.9 records evidence from real use:
- what a piece of Work's interactions were when it landed;
- what Dispatch's own next evaluation of every other piece of Work said.

From these, documented queries give precision and misses. No policy changes:
interactions stay advisory, and no verdict, apply or auto-apply behaviour
changes.

This release is also the first built by several agents coordinated through
Dispatch 0.4.8 itself. One integrator (the release architect) owns the
contract, review, integration and release; workers implement packets.

## Packets

| Packet | Owner | Objective | Files |
|---|---|---|---|
| P0 | integrator | this contract, stubs and fixtures | `src/coherence/measure.rs`, stubs, `tests/fixtures/measurement/`, this plan |
| A | worker (Cursor) | atomic publication of live patches | `src/source.rs` |
| B | worker (Claude Code) | landing observation at a successful apply | `src/coherence/measure/landing.rs`, `src/orchestrator/apply.rs`, `tests/measurement_landing.rs` |
| C | worker (Claude Code) | outcome classification, pure | `src/coherence/measure/outcome.rs` |
| D | worker (Cursor) | documented queries, claims guidance, query tests | `docs/queries/interactions-measured.sql`, `docs/coherence-validation.md` (new section), `tests/measurement_queries.rs` |
| W | integrator | the owner resolves and records outcomes | `src/orchestrator/serve.rs`, `tests/interactions.rs` |
| R | integrator | docs, claims, version, full checks, measurement trial | README, `docs/coherence.md`, `docs/attach.md`, `AGENTS.md`, release notes |

```
P0 ─┬─ A
    ├─ B ─┐
    ├─ C ─┼─ W ── R
    └─ D ─┘
```

- No file is edited by two packets. So no interactions between packets are
  expected; any that Dispatch reports point to a packet outside its files.
- Integration order: A, C, B, D, then W and R.
- Contract changes are the integrator's to decide. A worker reports them; it
  does not make them.

## Decisions

1. **Both measurement events are on the landed run.** It is finished, so
   writing to it never touches a live native or wrapped run, which keep a
   single writer.
2. **The owner evaluates each counterpart once, read-only, for measurement.**
   Native counterparts are included. It uses the Δ the owner already snapshots
   for interactions (0.4.8). It writes only to the landed run.
3. **The first evaluation after a landing is recorded, whatever it says:**
   CONTINUE, a failure, or unscorable. The owner looks at landings from the
   last 24 h. A landing that no owner saw afterwards gets no outcome, and the
   queries say so.
4. **No causation is claimed.** Before and after world digests and the
   counterpart's Δ digest are kept. Confounded cases are classified, not
   scored.
5. **Measurement does not inherit coherence-event suppression.** Outcomes are
   their own events, so an unchanged verdict is recorded too.
6. **Safety.** The landing is captured under the apply locks before applying,
   and recorded only after `result.applied` is committed. A measurement error
   is printed and swallowed. It never changes the apply's result, its events or
   its outcome, and a failed apply records no landing.

## 3. The contract (`src/coherence/measure.rs`)

### 3.1 Identities

- **Work:** `run_id`.
- **S0:** the run's `baseline_commit`. It is `None` only when a counterpart's
  run could not be read.
- **Δ:** `delta_sha256`, the SHA-256 of the exact patch bytes:
  - the landed run: the patch being applied (`candidates[0].diff_path`);
  - a counterpart at the landing: the view's `participants[].delta_sha256`;
  - a counterpart at the outcome: the Δ the owner evaluated.
- **World:** the coherence world digest, SHA-256 over the non-ignored tree, as
  in a `Validity`.
- **Direction:** the 0.4.8 `Interaction`, with `Side::A` meaning the landed
  run. So `writer: "a"` means the landed Work changes what the counterpart
  relies on.

### 3.2 `interaction.landed`: one per landed run, after `result.applied`

The payload is `Landing`: `version`, `status`, `detail`, `landed` (identity
and `applied_by`), `world_before`, `world_after`, `projection_computed_at`,
and `counterparts`. Each counterpart has its identity, `analysis`,
`unresolved`, `prior` (its stored decision and world digest, or `null`), and
`interactions`.

`landing::capture(landed, applied_by, patch, world_before, view, counterparts)`
is pure. The application path gathers its inputs before applying:
- `view` comes from `background::watcher` and `background::interactions`;
- `counterparts` are the runs the view lists;
- `world_before` is the gate's `validity.world_digest`, when the gate produced
  a validity.

After applying, the application path sets `world_after` (a fresh
`world::observe` digest, or `None`) and persists the event.

The status is the first of these that applies:
1. `OwnerView::Unwatched`: `unwatched`, with no counterparts.
2. `OwnerView::Unavailable(why)`: `unavailable`, with `detail` = why and no
   counterparts.
3. The landed run is not a participant, or its participant `delta_sha256`
   differs from the SHA-256 of `patch`: `stale`, with `detail` and the
   counterparts still listed.
4. Otherwise: `observed`.

`counterparts` lists every other participant, with or without interactions,
in the view's order. `interactions` comes from `Projection::of(landed)`.

### 3.3 `interaction.outcome`: one per (landed run, counterpart), on the landed run

The payload is `Outcome`: `version`, `classifier_version`, `landed_run_id`,
`counterpart_run_id`, `evaluated_at`, `world_evaluated`,
`counterpart_delta_sha256`, `decision`, `reasons` (codes), `error`,
`predicted`, and `class`. These come from an `Evaluation`:
- `Gone`;
- `Failed { error, world_digest?, delta_sha256? }`;
- `Evaluated { world_digest, delta_sha256, decision, reasons }`.

`predicted` is true exactly when the counterpart has at least one interaction.

`class` is the first rule that applies:
1. `landing_unobserved`: the landing's status is not `observed`.
2. `counterpart_gone`: the evaluation is `Gone`.
3. `evaluation_failed`: the evaluation is `Failed`.
4. `counterpart_unanalyzed`: the counterpart's `analysis` was `pending`.
5. `already_invalid`: `prior.decision` was `refresh` or `stop`.
6. `world_moved_on`: `world_after` is `None`, or differs from the evaluated
   world digest.
7. `counterpart_moved`: the evaluated `delta_sha256` differs from the
   counterpart's at the landing.
8. `scorable`.

A `last_seen` counterpart is still scorable; queries can filter on
`analysis`.

### 3.4 Duplicates and restarts

- The key is (landed run, counterpart). The owner reads the landed run's
  events under that run's lock and writes only missing keys. If the lock is
  busy, it tries again on the next tick.
- There is one owner per root.
- Outcomes recorded after a restart come out `world_moved_on` or
  `counterpart_moved` rather than being guessed.

### 3.5 Fixtures (`tests/fixtures/measurement/`, read-only for workers)

- `landed-*.json`: one landing per status, plus an observed landing with no
  `world_after`.
- `cases.json`: 16 cases of (landing file, counterpart index, evaluation) with
  the expected class and `predicted`. Together they cover every class and the
  precedence boundaries.
- `events.jsonl`: an event log of 6 landings and 8 outcomes for the query
  tests.

`measure::tests::the_fixtures_are_valid_contract_values` keeps every fixture a
valid contract value.

## How it runs

- **Supervision:** a pinned Dispatch 0.4.8 at
  `~/dispatch-dev/dispatch-0.4.8/dispatch` (verified from the release),
  supervision state `~/dispatch-dev/state-049`, and a dedicated integration
  clone `~/dispatch-dev/integration` on `release-0.4.9`, watched with
  `dispatch start`.
- **Excluded files:** `dispatch.yml` (checks: fmt, clippy and the lib tests,
  with a shared target directory) and `.claude/settings.local.json`
  (project-local hooks and `worktree.baseRef: head`). Both are excluded from
  git, so they are never part of S0 or Δ.
- **Workers:**
  - Claude Code: `claude --worktree 049-<x>`, which becomes discovered Work.
  - Cursor: `dispatch attach --allow-unsafe-local -- cursor-agent …`, in a
    workspace Dispatch makes.
  - No `--auto-apply`, and no Codex.
- **Integration**, one contribution at a time:
  1. Read its status, coherence and interactions.
  2. The integrator reviews the diff and runs fmt, clippy and the full suite
     independently on HEAD plus the patch.
  3. Recommend accept, revise or reject. The human approves.
  4. `dispatch accept`, which runs the merged-tree checks, then a commit in the
     integration clone.
  5. Recheck the remaining Work.
- **Stale Work:** reject it, then start a new workspace from HEAD with the old
  patch and the reasons. Attached Work has no `refresh`.

## Evaluation (per contribution)

These are kept apart:
- correctness and maintainability;
- corrective work after review;
- duration from Dispatch timestamps, and usage only as reported (otherwise
  unknown);
- Dispatch's interactions, invalidations, misses and the quality of its
  explanations.

Packets are not a controlled model comparison, and agent review is not human
preference data.

## Progress log

- 2026-10-06: setup.
  - Pinned 0.4.8 verified against `SHA256SUMS`.
  - Integration clone on `release-0.4.9` from `29563c6`.
  - Excluded `dispatch.yml` and project-local hooks; the owner started; a
    session in the checkout got the notice and no Work.
- 2026-10-06: P0.
  - The contract module, stubs for B and C (`unimplemented!`, never called),
    the fixtures and this plan.
  - `cargo fmt`, clippy and the fixture test pass.
