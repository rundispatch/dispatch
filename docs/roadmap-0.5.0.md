# Roadmap: Dispatch 0.5.0, agents that coordinate

**Approved 2026-10-17.** Each release still gets its own plan (`docs/plan-0.4.x.md`); this
page says where they lead and when 0.5.0 is earned.

## What 0.5.0 means

0.4.15 completed the 0.4 line: Dispatch sees every agent's work and gates it. 0.5.0 changes
what Dispatch is, from a gate that checks agents' work to a coordinator they work through.
For a user: run several agents on one repository at once, and Dispatch keeps them out of each
other's way.

0.5.0 is tagged only when all six hold; otherwise another 0.4.x comes first:

1. **Agents know about each other.** A live agent is told when other work overlaps its own or
   lands under it. Whether those notes were right is measured as agreement with Dispatch's own
   later verdicts, quoted as counts.
2. **Stale work catches up in place.** When a landed change breaks a symbol a live agent's
   work uses, that agent updates its work in its own session. Dispatch never relaunches it.
3. **Landing is one decision.** Several ready results land in a safe order behind one
   approval, and their combined result is verified once.
4. **Out of the box.** From install to a first landed change takes two or three commands and
   no flags.
5. **Fast.** No tree is verified twice; a verified result lands in seconds; agents start at
   once.
6. **Proven on itself.** 0.5.0 is built by agents coordinating through Dispatch, with packets
   allowed to overlap, and its execution log shows the numbers. The four-harness conformance
   trials (run with the user's approval) pass, including new coordination cells.

## Where the six conditions stand (2026-11-15)

Measured on 0.5.0's own build: its first wave of packets ran under the pinned 0.4.18, and
its second under a release build of the branch. The counts come from the documented
queries on that build's state (`notes-measured.sql`, `catchup-measured.sql`,
`batches-measured.sql`), run again after its last landing (packet E, 2026-11-15 01:24 UTC).
The timings and the coordination trials are on the release build of `96fd98c`, on one Mac
(arm64), on 2026-11-15, with the 1-minute load where it was recorded.

1. **Agents know about each other.** 26 notes delivered on real work, 0 unsettled. Their
   next verdicts: REFRESH 7 (all 7 count as agreement with Dispatch's own later verdict),
   CONTINUE 7, none yet 12. Counts only, with no target, as the plan asks.
2. **Stale work catches up in place.** 2 asks on real work, naming 3 landings, with 0
   asked twice and 0 file-level targets. One woke an idle Claude Code session, one came by
   `Stop`. Each was followed by the agent's merge and an S0 move (271 s and 96 s after
   delivery), then CONTINUE, with no relaunch. Hermes: this build's one Hermes packet ran
   wrapped, which is never asked. In the coordination trial, a real Hermes 0.18.2 session,
   idle when its ask was written, got it with its next turn; the agent merged, and S0
   followed its merge.
3. **Landing is one decision.** Each landing of 2 or more ready results in the second
   wave was one `dispatch accept` proposal and one approval: 7 results, then 3, then 2,
   each with one merged-check run and no bisection. 20 Works landed behind 11 approvals in
   all. No result was re-applied by hand: every landing was a `dispatch accept`. Manual
   steps are not recorded by any event.
4. **Out of the box.** On a temporary HOME, the install line (from a local release
   packaged by `scripts/package-local.py`), then `dispatch setup` (one pass, in a terminal)
   and `dispatch accept`: 2 Dispatch commands to a first landed change, and no flags (gate:
   3 commands). The owner finished the stand-in Claude Code session's Work by consent, and
   accept reused its merged checks and landed it.
5. **Fast.** 29 suite runs for 20 landed Works, 1.45 per landed Work (gate: at most 1.75).
   No passing tree was verified twice: `coherence.merged_checks` ran 4 times on this
   build's state, on 4 different trees, all passing. That is counted from those events by
   `tree_id`, not by a documented query. An already verified result landed in 0.33 s at
   the median of 10 accepts (0.30 to 0.43 s, load 3.4 to 4.2), on a clone of this
   repository at `96fd98c` (gate: 10 s). A native run reached its agent in 3.0 s at the
   median of 3 (2.8, 3.0 and 3.5 s, load 3.7 to 6.5), on the 3.6 GB measurement
   repository 0.4.17 and 0.4.18 used (0.4.18: 3.3 s; gate: 10 s).
6. **Proven on itself.** The second wave was supervised by a release build of the branch,
   and recorded symbol-level interactions between live Works, for example packet P2h
   changing the signature of `next_step`, which packet S used; P2h's landing then asked S
   to catch up. The coordination trials pass on the release build
   ([harness-conformance.md](harness-conformance.md), "Coordination cells"): Claude Code
   2.1.296 and Hermes 0.18.2 on all four cells (notes, catch-up, S0 follows the merge,
   batch landing), Codex 0.155.1 and Cursor Agent 2026.11.13 on S0 follows the merge and
   batch landing, with notes and catch-up "none, no hook". The first run, on `e8067c3`,
   failed Codex's S0 cell: every Codex worktree is detached, and S0 took no proof from a
   detached HEAD. Packet Y fixed it, and the rerun passed.

Also measured for the release: the `Stop` hook with an ask pending, over 1,000 calls at a
load of 3.4, took 5.16 ms at the median, 8.78 ms at the 95th percentile (gate: 15 ms) and
21.98 ms at most, and every call returned the ask as a block. An idle waiter used 0.00 s of
user and of system CPU over a 30 s wait, at 9.6 MB resident (load 3.1).

## The releases

| Release | Theme | What it adds | Gate to the next |
|---|---|---|---|
| 0.4.16 | Nothing runs twice | verification memo; warm check workspace; native runs start at once; a faster owner and hooks | suite runs per landed Work ≤ 1.75 |
| 0.4.17 | Out of the box | install script and an owner that restarts itself; one setup pass; finish when a session ends, reopen when it resumes; "waiting on you" signals; mentions are not dependencies (REFRESH precision) | ≤ 3 commands to a first landing; zero overrides |
| 0.4.18 | Agents that know about each other | notes delivered through Claude Code and Hermes hooks; what a note says; S0 follows the agent's integration of the source; queries for notes and their outcomes | every note recorded; no S0 move without proof |
| 0.5.0 | Dispatch coordinates | `dispatch accept` proposes a batch in dependency order; batch verification with bisection; catch-up once per landing; coordination conformance | the six conditions above |

## Decisions behind it (user, 2026-10-17)

- **Coordination** means agents aware of each other, a safe landing order, and catch-up in
  place. Splitting a task among agents is not part of 0.5.0.
- **Landing** is one human approval per batch.
- **What Dispatch may do by itself,** each recorded and bounded:
  - speak to live agents through their runtimes' hooks;
  - keep a stale agent working, only for a symbol-level break from a landed change, once per
    landing, never relaunching it;
  - finish Work when its session ends, with standing check consent, never applying it;
  - keep a warm check workspace per project.

  AGENTS.md gains each clause in the release that ships it.
- **Defaults:**
  - Dispatch does not commit when it lands.
  - A landing that edited the same declaration counts as breaking a symbol that Work uses.
  - Existing check consents are asked again once, for the wider scope.
  - Native launches get a separate standing consent, given in a terminal.

## How progress is measured

Each release is measured on its own build, from recorded events and logs, as counts with
their denominators. The baselines are 0.4.15's:
- **Suite runs per landed Work:** 2.43 (0.4.14: 2.50).
- **Manual steps per landing:** about 8.
- **Owner tick with 6 Works on Dispatch's repository:** about 0.4 s when nothing moved, about
  2.9 s when the source moved.
- **A hook with nothing to do:** 202 ms (median).

A reused result is counted as reused, never as time saved.

## After 0.5.0: candidates for 0.5.x

Each needs its gate met before it is planned into a release.

- **Warm, copy-on-write build output in the workspaces Dispatch makes.**
  - **The problem:** every agent workspace starts with empty build directories (`target/`,
    `node_modules`, `build/`, `.venv`). So each pays a cold build and holds its own copy on
    disk. Rust shows it worst, but it is not Rust-specific. On 0.4.17's own build, four
    leftover workspaces held 14 GB, the warm check workspace 6 GB, and every packet started
    with a cold build.
  - **What it would be:** seed a new workspace's ignored build directories from the checkout
    by cloning (`clonefile(2)` on APFS, reflinks on Linux), as native runs already do. Copies
    then share blocks until they diverge. This is filesystem-level and language-neutral: no
    index, no analysis.
  - **Caveats:**
    - Toolchains that fingerprint absolute paths (Cargo) may rebuild the project's own code
      and keep only dependencies warm.
    - Without clone support it falls back to a real copy, which saves no disk.
    - Worktrees a runtime made (Claude Code, Hermes) are the runtime's own; seeding them is a
      separate decision.
  - **Gate:** a measurement during 0.4.18's build, at no feature cost. Cold versus seeded
    first-build time, and disk per workspace, on Dispatch's repository and on one Node
    project, with load averages. Reported as measured, never as time saved.
