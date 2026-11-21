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
  the parent's process, and removes the lock shortly after the subagent's first stop. The
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
  the sessions of its subagents' first runs, through their locks, and with consent those
  Works are then finished. It does not end a resumed run that has not stopped yet: that
  Work waits for you to finish or close it.
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

### Measured, honestly

From this build's own work, in the integrator's record. Two packets landed through one
`dispatch accept --expect`, with the merged checks run once on the combined tree (`cargo
test` 14 min 28 s). Packet A was found by discovery 29 minutes before the project's check
consent was given, and the consent covers only Work registered while it holds, so the owner
never finished it: it waited about 28.5 hours, until a typed `dispatch finish`.
`docs/attach.md` now says so where it describes the consent. The other packet was finished
by consent.

[[C: this build's own notes, asks and batches, from the documented queries on its state, as 0.5.0's notes give them]]

The rest waits on the conformance trial and a quiet machine:

- **Subagent conformance** (`docs/harness-conformance.md`, "Subagent cells"): [[C: an orchestrating Claude Code session starts B, A and C as worktree subagents on the release build: how many were Work at spawn and how many "Found, not tracked"; a note and a catch-up delivered to a subagent, each once; finish by consent; the batch, its order and its one merged-check run; whether the parent's end ended any subagent Work, and whether any waiter process belonged to a subagent; and whether any subagent Work was left lost]].
- **Hook cost:** [[C: `SubagentStart` with a note pending, and `SubagentStop` with an ask pending, over 1,000 calls each on a quiet machine: median, 95th percentile and most, with the load]].
- **Keeping Δ at a stop:** [[C: `SubagentStop` keeping Δ in a watched project with Work: median, 95th percentile and most, against its 60 s timeout, with the load]].

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
- Ending a resumed subagent run whose parent exits before its stop.
- Keeping a parent's own hooks, sent from inside a subagent's worktree, away from that
  Work's notes and asks.
- An orchestrator's own launches and messages: that is `dispatch mcp`, planned for 0.6.0.

Production code under `src/`, excluding `#[cfg(test)]` items, grew by 454 non-blank lines
since 0.5.0 (349 without comment lines): subagents are 416 of them (315), and menus 38
(34).

Site impact: Claude Code subagents started with `isolation: worktree` become Work, with two new Claude Code hooks (`SubagentStart`, `SubagentStop`, 60 s each) that setup offers once as an Update; the Claude Code guide's "Worktrees that were already there when watching began, or that have no session lock yet, are listed as *found, not tracked*" no longer covers a subagent's worktree, which has an agent lock and becomes Work, its "The hooks take a few milliseconds: they open no database and take no lock" no longer holds for `SubagentStop`, which keeps a subagent's Δ under its Work's lock, and its "With them:" list gains subagents (S0 at spawn, notes, an ask at `SubagentStop`, its Δ at each stop, finish by consent, the batch; not tracked: no isolation, Claude Code's own agents), and its "Starting from your current code" (`status` and `watch` "recommend `dispatch attach -- claude`") does not hold for a subagent's Work, whose start line reads `Claude Code: a subagent's worktree starts from the checkout's HEAD; uncommitted changes are not copied.`; limits' "Found by itself: Claude Code, Codex, Cursor Agent and Hermes Agent sessions in their own worktrees" and the agent guide's "Agents in their own worktrees" gain Claude Code subagents; a subagent's REFRESH next step reads `start Claude Code in <worktree>`.
