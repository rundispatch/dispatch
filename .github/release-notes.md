## Dispatch 0.4.18: Agents that know about each other

In 0.4.17 a live agent heard nothing from Dispatch. It did not learn that other Work changed a
declaration it uses, or that a landing had made its result stale: you found out at accept, as
REFRESH, and fixed it by hand. An agent that did merge what landed gained nothing, because S0
stayed where the Work began, so its Δ still carried the landed change. In 0.4.18 a Claude Code
or Hermes session hears, through its hooks, what overlaps its work and what landed under it,
and S0 follows an agent that merges or rebases onto a recorded source, with proof. Hooks no
longer wait on a lock or open the database.

### Notes for live agents

- **What a note says.** Fixed text, never written by a model, one line per item: the other
  Work by short ID and runtime, the declaration and file, and the change. Two stand-in Claude
  Code sessions in their own worktrees, one changing the signature of `validate` and the other
  adding a call to it, each heard after their next tool call, for example:

  ```text
  Dispatch: other Work touches this Work (01M6T2KV), your changes in this workspace.
  01M6T2KT (Claude Code): it changes the signature of validate (src/lib.rs), which this Work uses
  ```

  After the first Work landed, the second heard:

  ```text
  Dispatch: other Work touches this Work (01M6T2KV), your changes in this workspace.
  01M6T2KT (Claude Code), landed: it changes the signature of validate (src/lib.rs), which this Work uses
  Signature: pub fn validate(input: &str) -> bool → pub fn validate(input: &str, strict: bool) -> bool
  The source with this change is commit b768cb1b (refs/dispatch/source).
  ```

  - Only interactions at a declaration or at lines make a note: the same declaration, a
    declaration one Work changes and the other uses, or overlapping lines. A file-level
    interaction never does.
  - A note is information, never an instruction: it says what changed and where, and asks
    nothing of the agent. Dispatch does not wake an idle session or hold a turn.
- **When it arrives.** In Claude Code, after the session's next tool call (a new `PostToolUse`
  hook) or when a session starts, as context Claude Code adds beside the tool's result. In
  Hermes, with its next turn, through a new `pre_llm_call` hook.
- **How often.** Each item once, for each other Work or landing, target and change. At most
  one note per Work per ten minutes: what is found in between waits and is merged into the
  next note, and a note longer than one delivery ends `And <n> more in the next note.`
- **The landing note names the recorded source.** After each landing on a Git project,
  Dispatch records the project's source, the landed change included, as a commit, and points
  `refs/dispatch/source` at it with a reflog entry `dispatch: Work <id> landed`. No branch,
  HEAD, index or file of yours changes. A landing in a plain directory gets no landing note.
- **Recorded.** `note.written` when the owner puts a note in the Work's inbox,
  `note.delivered` when a hook took it, `note.expired` when the Work finished or closed first.
- **Codex and Cursor get none:** Dispatch has no hook in them that can carry context.

0.4.17 told a live agent nothing; the first sign was REFRESH at accept.

### Hooks never wait

- **Session end and needs-you are left as files.** Claude Code's `SessionEnd`, `Notification`
  and `Stop`, and Hermes's end when it keeps its worktree, write a small file and return. The
  project owner records it on its next tick, or the next owner does if none runs, with the
  time the hook saw it. The hook opens no database and takes no lock of the Work's.
  `dispatch watch` shows a wait one tick later than before. 0.4.17's hooks waited up to 3 s
  for the Work's lock and then opened SQLite inside a 5 s timeout; while a typed `finish` held
  the Work they failed with `Dispatch could not track this session: attached work has a
  foreground owner: still busy after waiting 3s`.
- **Delivery is a rename.** The hook walks up from its directory to the worktree's top, takes
  the inbox by renaming it, and prints it, with no Git command. Its cost is under "Measured,
  honestly".
- **The hook update.** Claude Code's set gains `PostToolUse` (every tool, 5 s), seven hooks
  in all. Hermes's block gains `pre_llm_call` (5 s) beside `on_session_finalize`. Setup now
  says: `Hermes asks you once for each of these hooks, at its next start in a terminal, and
  they don't run until you allow them (hermes hooks list shows which are allowed); a run
  without a terminal needs hermes --accept-hooks.` A Hermes `config.yaml` that `hermes config
  set` rewrote is still recognized as holding Dispatch's hook; setup shows the lines to change
  there by hand and writes nothing. 0.4.17 did not recognize its hook there.

### S0 follows integration

- **A merge or rebase of the recorded source moves S0.** For attached Git Work in progress,
  on the first tick after the workspace's HEAD moved, the project owner moves S0 to the newest
  recorded source (or the root's HEAD, with nothing uncommitted there) that the HEAD descends
  from, checked with `git merge-base --is-ancestor`. The Δ shrinks to the agent's own change.
  In the fixture above, the second Work merged `refs/dispatch/source` and updated its call;
  its frozen patch then held only its own two files, and accept was CONTINUE.
  - The move is recorded as `attach.s0_moved {from, to, workspace_head, proof, recorded_at,
    landed_run, delta_before_sha256, delta_after_sha256}`. The old baseline is kept and listed
    in the run's `baseline_history`, so every earlier (S0, Δ) stays recomputable.
  - `explain` shows it:

    ```text
      S0: commit b768cb1b9f76b262d153548f872a5cb6fdafc50b (moved from 47a3d33e4885c2f6835c99a6bf77dbbb0a5ce1ba at 2026-11-07 04:00 UTC: the workspace merged the recorded source)
      S0 before: the workspace as it was when the work began (47a3d33e4885c2f6835c99a6bf77dbbb0a5ce1ba)
    ```

    `watch` names it too: `S0 commit b768cb1b (moved from 47a3d33e)`.
- **What does not move it.** A merge, cherry-pick or rebase still in progress, a detached
  HEAD, a cherry-pick or squash of the landed change, a pull of something Dispatch never
  recorded, and a merge of a source recorded before the Work began that does not descend from
  its S0. Native runs, plain directories, wrapped Work while its wrapper lives, and finished,
  closed or applied Work keep their S0. Without the proof the verdict is what it was in
  0.4.17.
- **Undoing the merge moves it back.** A `reset` that drops the merge moves S0 back to the
  newest earlier S0 the HEAD has, or to where the Work began. A `git revert` on top is not an
  undo: S0 stays, and the revert is in Δ, so accepting would land it.
- **The downside.** A rebase that copies the recorded source's commit instead of keeping it as
  an ancestor (for example one that flattens the merge) leaves a HEAD that no longer descends
  from the moved S0. S0 goes back, the landed change is in Δ again, and the Work is REFRESH
  where it could have been CONTINUE: the verdict 0.4.17 gave, never a revert landed. `git
  merge refs/dispatch/source`, or a rebase onto it, keeps the record as an ancestor.
- **The recorded source is your whole working tree,** uncommitted and untracked files
  included. An agent that merges it takes those into its branch, where they are part of S0,
  not Δ, so they never land through that Work.

### Native start

On macOS, a native run's workspace now clones a clean directory, such as an ignored `target/`,
whole with one `clonefile(2)`, where 0.4.17 cloned it file by file. A directory is cloned whole
only when a walk that reads no file finds nothing Dispatch leaves out: no nested `.git`, no
Dispatch state, no special file. Otherwise, and on any error, it is copied per file as before.
The fingerprints and the snapshot's checks are unchanged, and Linux is unchanged.

On the 0.4.17 fixture (a Git repository with a 3.6 GB ignored `target/`, 9,650 files), from
invocation to the agent's start, on one Mac: this build's median was 3.3 s over 5 runs (3.0 to
3.9 s, 1-minute load 6 to 6.5), and the pinned 0.4.17's, run right after, 9.9 s over 3 runs
(9.8 to 11.7 s, load 6 to 13). 0.4.17's median on the same fixture at release was 10.8 s.

### Smaller items

- `f` in `dispatch watch` finishes at once when the project's standing check consent covers
  exactly the Work's checks. 0.4.17 asked first, for Work that carried no authority of its own.
- The answer to a key in `dispatch watch` (a refusal, an accept, a finish) now always shows for
  its 10 seconds. On a heavily loaded machine it could expire before it was first drawn.
- A second `SessionStart` in a worktree another session is registering waits for it until the
  hook's own deadline, instead of failing after 10 s with `another session is registering
  this workspace`.
- Hermes's `pre_llm_call` payload carries the whole conversation. The hook reads only its
  start, takes the rest of its input unread, and works with a 1 MB payload.
- For Work whose S0 moved, `finish`, `check` and `accept` say what they compare against:
  `Merged checks: not run — the source has not moved since the Work's S0 moved to d1b75cdf, so
  the merged result is the Work's own tree`. Work whose S0 never moved keeps `…since the Work
  began…`.

### Measured, honestly

Notes are new, and unmeasured on real work: nothing here says agents act on them, agree with
them or save anything. `docs/queries/notes-measured.sql` counts, for each note delivered,
whether the Work's Δ later changed the note's target (`note.touched`), whether its S0 moved
afterwards, and the Work's next verdict. A note "agrees" when that verdict is REFRESH or STOP,
which is agreement with Dispatch's own later verdict, not a human judgment, quoted as counts.
It also counts notes neither delivered nor expired, which must be 0 for finished Work.

- **Notes on this build's own work: none.** Its workers were supervised by the pinned 0.4.17,
  which writes no notes, so there is no agreement count from real work yet.
- **A stand-in check.** On a release build with stand-in Claude Code sessions, the query
  counted 3 notes delivered and 0 unsettled. That shows the recording works, not that notes
  help.
- **Hook cost.** A release build's `PostToolUse` over 1,000 runs each, at a 1-minute load of
  11 to 12: 4.65 ms at the 95th percentile in a project nobody watches, 4.47 ms watched with
  no inbox, and 5.61 ms with an inbox to deliver on every call.
- **Native start:** the 3.3 s and 9.9 s medians above, with their loads.

### Not in 0.4.18

- Asking an agent to act: no blocking `Stop` reply, no waking an idle session (0.5.0's
  catch-up).
- Notes for Codex and Cursor.
- Merging or refreshing on an agent's behalf.
- 3-way or batch accept (0.5.0).
- Seeding warm build output into workspaces.

No migration: the schema stays at 24. Runs gain `baseline_history`. New events:
`attach.s0_moved`, `note.written`, `note.delivered`, `note.expired`, `note.touched`.
"Upgrading to 0.4.18" in `docs/release-install.md` has the details: Claude Code's hooks are
offered once as an Update, and Hermes asks once for its new hook. As since 0.4.17, the next
command after an upgrade in place restarts the owner.

Site impact: new claims, notes to live Claude Code and Hermes sessions through their hooks, and S0 following a merge or rebase of the recorded source (`refs/dispatch/source`, written after each landing); the Claude Code guide's "Optional hooks" section and the agents page's hooks line (hooks now also deliver notes); `product.ts`'s `addsLittleWhen` ("it tells you to redo the Work") and `comparison` ("blocks `dispatch accept` until the Work is redone"), since an agent can now merge the recorded source instead of redoing the Work. The site's links to the Core README for the command reference (`reference/commands.md`, `agents.md`'s "read the README at that version's tag" and its Core README link) should point to `docs/reference.md`, where the reference moved before this release.
