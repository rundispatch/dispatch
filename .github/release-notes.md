## Dispatch 0.5.1: Subagents are Work

In 0.5.0 a Claude Code subagent started with `isolation: worktree` was invisible to
Dispatch. Its worktree was listed as found, not tracked, so it had no S0, heard no note,
was never asked to catch up and never joined a batch. On a stand-in project where one
parent session started four such subagents, 0.5.0's `dispatch status` read:

```text
No Work in ~/project yet.

Project: watched in the background since 09:11 (pid 11634)

Found, not tracked
  dispatch attach --workspace ~/project/.claude/worktrees/agent-a85d8d5dd371b34d4
  dispatch attach --workspace ~/project/.claude/worktrees/agent-a2e9ec256a5f3d587
  dispatch attach --workspace ~/project/.claude/worktrees/agent-a2f2fb9991cd71185
  dispatch attach --workspace ~/project/.claude/worktrees/agent-a3af667cafd45715f
```

In 0.5.1 each of them is Work from its first moment, with no new step: S0 at spawn, notes,
one ask to catch up at the end of a response, its exact Δ kept at each stop, a finish by
consent when its session ends, and a place in `dispatch accept`'s batch. Menus also set
their question, facts and choices apart: a bold question, a blank line before the
choices, and the focused choice the only colored one.

### Subagents are Work

- **Which ones.** A subagent that Claude Code starts with the Agent tool and `isolation:
  worktree`, in a project watched with `dispatch start`, with the updated hooks. Claude
  Code 2.1.296 makes its worktree at `.claude/worktrees/agent-<id>`, on the branch
  `worktree-agent-<id>`, from the checkout's HEAD.
- **S0 at spawn.** The new `SubagentStart` hook registers it before its first tool call,
  as `SessionStart` registers a session, and replies, as for a session, `Dispatch is
  tracking this worktree as Work 01M7WVQ96A2D0VMPX3NT0GA6R3; see it with dispatch watch.`
  Its Work is named `claude subagent general-purpose in agent-a85d8d5d`, its type and the
  first 8 characters of its ID.
- **Its own session.** Every hook inside a subagent carries its parent's session ID, so the
  Work's session is `<parent session>_agent-<id>`, and its record has a `subagent` field
  (parent session, agent ID and type).
- **Notes** reach it at `SubagentStart` and after each tool call. On the stand-in project,
  the subagent that calls `scale` heard, after its next tool call:

  ```text
  Dispatch: other Work touches this Work (01M7WVQAN), your changes in this workspace.
  01M7WVQ96 (Claude Code): it changes the signature of scale (src/lib.rs), which this Work uses
  ```

- **Catch-up at the end of a response.** The new `SubagentStop` hook comes at the end of
  each of the subagent's responses. When an ask waits, it keeps the subagent going with it,
  once, as `Stop` does for a session. The same subagent, still running when the change to
  `scale` landed, got this at its next stop:

  ```text
  Dispatch: Work 01M7WVQ96 (Claude Code) landed a change this Work relies on.
  - scale (src/lib.rs), which this Work uses: 01M7WVQ96 changed its signature, pub fn scale(n: i32) -> i32 → pub fn scale(n: i32, by: i32) -> i32
  These changes are in refs/dispatch/source (commit bf7573a3).
  To catch up: commit your work, run git merge bf7573a363b69f53c6d4338ba2066440ce207dd2, update what uses them, and run the project's checks. Dispatch asks once per landing.
  ```

  The stand-in then merged as the ask says, its S0 followed the merge, and it finished
  CONTINUE on the new S0. A subagent is asked only while it runs: nothing wakes one that
  has stopped.
- **Its exact Δ at each stop.** Every `SubagentStop` that gives no ask keeps the Work's
  exact Δ, because Claude Code removes an unchanged subagent worktree right after the stop
  and runs no `WorktreeRemove`. When Claude Code removes it, the Work closes with no
  changes.
- **How its session ends.** Claude Code locks a subagent's worktree with a lock that names
  the parent's process, and removes the lock shortly after the subagent's first stop,
  unless it resumes the subagent by itself, after a background shell for example. The
  session ends when that lock goes, or when the parent's process is gone. A run resumed
  with `SendMessage` has no lock, and its session ends at its stop. Dispatch never records
  the parent's process as the subagent's agent.
- **Finish and land.** With the project's check consent, the owner finishes the Work once
  its session has ended, from the worktree, or, once the worktree is gone, from the Δ kept
  at the last stop if S0 has not moved since. The result joins `dispatch accept`'s batch,
  a change before what uses it. On the stand-in project:

  ```text
  2 results can land together, in this order:
    1. 01M7WVQ9NGSA35Y6CFNET8AZDY  Δ 4e9e94dea885  claude subagent general-purpose in agent-a2e9ec25
         by finish time
    2. 01M7WVQ96A2D0VMPX3NT0GA6R3  Δ feb2e6743a98  claude subagent general-purpose in agent-a85d8d5d
         before 01M7WVQA: this Work changes the signature of scale (src/lib.rs), which it uses
  Left out:
    01M7WVQA3MW5J5ZJNBS9AZ8TZQ  claude subagent general-purpose in agent-a2f2fb99
         REFRESH: fact_broken: pub fn scale(n: i32) -> i32 => pub fn scale(n: i32, by: i32) -> i32
  Batch 70b484fe4111. The project's checks run once on all of them together; if they fail, only the results before the first one that fails land.
  error: nothing was decided: to land 2 results together, run dispatch accept --expect 70b484fe4111
  ```

- **What the parent's own end does.** The parent's `Stop` and `SessionEnd` name the
  parent's session, so they never mark or end a subagent's Work. The parent exiting ends
  the sessions of its subagents' runs that started under their locks, and with consent
  those Works are then finished. It does not end a run resumed with `SendMessage` that has
  not stopped yet: that Work waits for you to finish or close it.
- **Run again.** When the parent sends a subagent another message, the subagent goes on in
  the same Work, or, once that Work is a result waiting for review, as new Work from the
  same S0, and the result closes as superseded.
- **Going on yourself.** A subagent's REFRESH next step and its "session ended" line say
  `start Claude Code in <worktree>`, where other Work says `resume its session in
  <worktree>`: nobody can resume a subagent's session by hand.
- **Where it began.** When the source differed from a subagent's S0 at its start, the
  "source at start" lines of `status` and `watch` no longer give it the advice about
  `worktree.baseRef` and `dispatch attach -- <agent>`, which does not apply to a subagent.
  After the line that names the differing paths, they say: `Claude Code: a subagent's
  worktree starts from the checkout's HEAD; uncommitted changes are not copied.`
- **Found without the new hooks.** Discovery also reads a subagent's lock now. A subagent
  worktree that appears while the project is watched becomes Work even with 0.5.0's hooks,
  from its merge base, with no type (`claude subagent in agent-<id prefix>`), notes after
  its tool calls, and no ask and no Δ kept at its stops.
- **Not tracked:** a subagent without isolation, and Claude Code's own agents (prompt
  suggestions, `/btw`). They work in the parent's directory, and their changes belong to
  whatever holds it. In 0.5.0, when that directory was a Work's worktree, they took that
  Work's notes after their tool calls; now they take none of its notes or asks.
- **Recorded** as `subagent.stopped {agent_id, sessions, files_changed}` at each stop that
  keeps Δ, `catchup.delivered {via: subagent_stop}`, `note.delivered {via:
  SubagentStart}`, and `runtime.session_ended {reason: subagent_stop}` for a run that ends
  at its stop. `docs/attach.md` ("Subagents" and "Known limits") has the details, including
  what happens when the parent merges a subagent's branch: the change lands once, never
  twice.

### Menus

Menus show their question in bold in the terminal's own foreground color, a blank line
before the choices, and the focused choice in bold in the focus color, the only colored
choice. Without color (`--no-color`, `NO_COLOR`), the bold, the blank line and the `›`
marker remain. Line mode (`--plain`) also gains the blank line before its numbered
choices:

```text
Project checks
  true
Work here asks you before running them.

1) Change checks…
2) Let Work run these checks by themselves…
3) Back
[2] >
```

In 0.5.0 a menu drew its question, its facts and its choices in one style, with nothing
between them; only the `›` marker and the hint row stood apart. The words, the initial
focus and the keys are unchanged.

### Fixed

- A live Claude Code or Hermes agent that merged a landing within ten minutes of its last
  note was later sent a note about that landing, which its S0 already held. Notes now name
  only landings after the Work's S0, as catch-up does, and a landing item still waiting
  for the window is dropped once the Work's S0 holds it. Found in the subagent trial
  ("Measured, honestly").
- `docs/queries/batches-measured.sql` counted one merged-check run of a batch more than
  once when its members' states differed, and now counts each verification once. Its
  suite runs also left out an attached finish that checked in its workspace whenever a
  later accept reused that finish's own pass: on 0.5.1's own state, 9 were counted where
  10 ran. Such a finish now counts, and a reuse of another run's pass is left out as
  before. 0.5.0's published figures are unaffected by either.

### Measured, honestly

From this build's own work, in the integrator's record. Every packet ran under the pinned
0.5.0, which watched the project. Six packets landed, in three of four `dispatch accept`s:

- U and A, as one batch, with the merged checks run once on the combined tree (`cargo
  test` 14 min 28 s).
- F and R, as one batch: the merged checks failed in
  `native_runs::phase4_pty_intent_answer_recovery_review_and_restoration`, its hangup
  scenario, at a load of 25 to 27 while they ran, so F landed alone and R did not.
- R alone: the merged checks failed in
  `attach_scenarios::simultaneous_integration_serializes_attached_and_native`, at a load
  of 22 while they ran, and nothing landed.
- R, N and G, as one batch, with the merged checks run once (`cargo test` 13 min 2 s).

Both failures were on the Mac that ran the checks, loaded by other work. Packet G found the
cause of neither in the code. It made both tests say what happened when they fail, so that
the next failure names its path, and changed no product code. The same tests passed in CI,
including 10 of 10 runs of the suite on a macOS runner under load, a job this release adds
to `stability.yml`. Packet A was found by discovery 29 minutes before the project's check
consent was given, and the consent covers only Work registered while it holds, so the owner
never finished it: it waited about 28.5 hours, until a typed `dispatch finish`.
`docs/attach.md` now says so where it describes the consent. Every other packet was
finished by consent.

Counts from the documented queries on this build's state after those landings
(`docs/queries/notes-measured.sql`, `catchup-measured.sql`, and `batches-measured.sql` as
corrected, "Fixed"). Since 0.5.0 watched the project, they count 0.5.0's notes and asks
on this release's work. "Followed by" means recorded later, not caused by, and agreement is
only with Dispatch's own later verdict, never a human judgment.

- **Notes:** none delivered, 0 unsettled.
- **Catch-up:** no asks.
- **Batches:** 3 landed: 2 results of 2, 1 of 2 and 3 of 3, each with one merged-check run
  and no bisection. 6 Works landed in all, all 6 in those batches, behind 3 approvals.
- **Suite runs:** 10 for 6 landed Works, 1.67 per landed Work (0.5.0: 1.45), and 1 reuse
  of a recorded pass.

The rest was measured on one Mac (arm64, 10 CPUs), with the 1-minute load where it was
recorded.

- **Subagent conformance** (`docs/harness-conformance.md`, "Subagent cells"), on
  2026-11-20, on a release build of `f35e1d6`, before packet N's fix. One Claude Code
  2.1.296 session in the fixture's checkout started B, A and C at once with the Agent tool
  and `isolation: worktree`. Every cell passed.
  - All 3 were Work, with S0 the fixture's tree, and 0 were "Found, not tracked". B's Work
    was made by discovery from its lock 0.46 s before its `SubagentStart` arrived, and the
    hook joined it.
  - A note about A and B reached A by `PostToolUse`. B's expired unread, since B had
    finished.
  - One ask, naming B's landing, reached A at its next `SubagentStop`, 68 s after it was
    written, and no second one. A committed and merged, and its S0 followed.
  - B and C were finished by consent once their locks went, and landed as one batch: one
    proposal, by finish time, one `--expect`, one merged-check run and one
    `refs/dispatch/source` commit. A was finished by consent when the parent exited, and
    landed later on its own.
  - The parent's `SessionEnd` ended no subagent Work, no waiter ran for the trial's state,
    and no Work was left lost: all three finished and landed.
  - Two findings. Claude Code resumed A after each of its background shells, and A kept its
    lock after its last stop, so its session stayed open until the parent exited 18
    minutes later, and its finish waited until then ("Not in 0.5.1"). And a note about B's
    landing was written to A 8 minutes after A's S0 already held it. It was never
    delivered, as A was done, and packet N fixed the cause after the trial ("Fixed").
- **Hook cost,** on the release build of `1a331ff`, on 2026-11-21, 1,000 calls each:
  - `SubagentStart` with a note pending: 6.02 ms at the median, 9.9 ms at the 95th
    percentile and 17.07 ms at most (load 2.9). Every call delivered the note.
  - `SubagentStop` with an ask pending: 5.43 ms, 8.77 ms and 24.42 ms (load 2.9). Every
    call returned the ask as a block.
  - The bar this release set is for these two paths, where nothing is registered: 15 ms
    at the 95th percentile. Both are under it. With nothing pending, `SubagentStart` took
    4.99 ms at the 95th percentile (load 3.0).
  - Timed call by call in turn, so that each saw the same load (3.7 to 4.4): at the 95th
    percentile, 0.5.0's `Stop` with an ask took 7.57 ms and this build's 7.49 ms,
    `SubagentStop` with an ask 7.74 ms and `SubagentStart` with a note 7.91 ms; at most,
    17.51 ms, 14.2 ms, 200.83 ms and 22.52 ms.
  - In a watched project, `SubagentStart` for a subagent whose Work is already registered
    joins that Work, as `SessionStart` does for a session: 57.16 ms at the median, 64.44
    ms at the 95th percentile and 94.73 ms at most (load 4.0), within its 60 s timeout.
    `SessionStart` joining a session's Work took 73.89 ms at the 95th percentile with 0.5.0
    (load 3.2) and 75.85 ms with this build (load 4.0), so a subagent's join costs no more
    than a session's.
- **Keeping Δ at a stop:** `SubagentStop` in a watched project, with a file changed before
  each call, keeping the Work's exact Δ: 71.52 ms at the median, 107.4 ms at the 95th
  percentile and 125.0 ms at most, over 200 calls (load 4.4), against its 60 s timeout.

Nothing here says tokens, minutes or money were saved.

### Upgrading to 0.5.1

- No migration; the schema stays at 24. 0.5.0's `history` and `status` read a state
  directory with subagent Work (checked on the stand-in project); nothing else was tried.
- `dispatch setup` offers Claude Code's hooks once as an Update. It adds two hooks,
  `SubagentStart` and `SubagentStop`, 60 s each, and the consent screen gains one sentence:
  `With SubagentStart and SubagentStop, each subagent in its own worktree is Work of its
  own.` After it, setup says `Claude Code hooks: already running this Dispatch`. Until
  then, discovery still finds a subagent's worktree (above).
- `SubagentStop` keeps a subagent's Δ under its Work's lock in a watched project, within a
  40 s budget in its 60 s timeout. In a project nobody watches it decides from the files
  alone, as every hook does there.
- Nothing else asks again: the check consent, Hermes's hooks and the agent profiles are
  unchanged.
- "Upgrading to 0.5.1" in `docs/release-install.md` has the details, including going back
  to 0.5.0 with the new hooks in place. As since 0.4.17, the next command after an upgrade
  in place restarts the owner.

### Not in 0.5.1

- Subagents of other harnesses. Hermes's `delegate_task` children work in their parent's
  own directory and are part of its Work.
- A subagent without isolation as Work of its own.
- Waking a subagent that has stopped, or a waiter for one.
- Putting a parent's Work first when it merged a subagent's branch, so that the subagent's
  is left out as already applied.
- Ending a run resumed with `SendMessage` whose parent exits before its stop: it has no
  lock, and its Work waits for you.
- Ending, before its parent exits, the session of a subagent that Claude Code resumed by
  itself, after a background shell for example: it can keep its lock until then, and its
  finish by consent waits. Its Δ was kept at each stop.
- Keeping a parent's own hooks, sent from inside a subagent's worktree, away from that
  Work's notes and asks.
- An orchestrator's own launches and messages: that is `dispatch mcp`, planned for 0.6.0.

Production code under `src/`, excluding `#[cfg(test)]` items, grew by 477 non-blank lines
since 0.5.0 (362 without comment lines): subagents are 424 of them (319), menus 38 (34),
and the notes fix 15 (9).

Site impact: Claude Code subagents started with `isolation: worktree` become Work, with two new Claude Code hooks (`SubagentStart`, `SubagentStop`, 60 s each) that setup offers once as an Update; the Claude Code guide's "Worktrees that were already there when watching began, or that have no session lock yet, are listed as *found, not tracked*" no longer covers a subagent's worktree, which has an agent lock and becomes Work, its "The hooks take a few milliseconds: they open no database and take no lock" no longer holds for `SubagentStop`, which keeps a subagent's Δ under its Work's lock, and its "With them:" list gains subagents (S0 at spawn, notes, an ask at `SubagentStop`, its Δ at each stop, finish by consent, the batch; not tracked: no isolation, Claude Code's own agents), and its "Starting from your current code" (`status` and `watch` "recommend `dispatch attach -- claude`") does not hold for a subagent's Work, whose start line reads `Claude Code: a subagent's worktree starts from the checkout's HEAD; uncommitted changes are not copied.`; limits' "Found by itself: Claude Code, Codex, Cursor Agent and Hermes Agent sessions in their own worktrees" and the agent guide's "Agents in their own worktrees" gain Claude Code subagents; a subagent's REFRESH next step reads `start Claude Code in <worktree>`.
