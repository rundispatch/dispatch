## Dispatch 0.5.0: Dispatch coordinates

In 0.4.18 a live agent heard, in a note, that a landing changed a declaration its Work uses,
and nothing asked it to act: an idle session heard nothing until its next tool call, and the
stale Work waited for you at accept as REFRESH. Several ready results needed one `dispatch
accept <id>` each, in an order you chose, each after the first with its own run of the
merged checks, and `dispatch accept` without an ID refused them all. A result whose patch
no longer applied only because lines around its edit had changed was REFRESH
`patch_conflict`, and was re-applied by hand or redone. In 0.5.0 a live Claude Code or Hermes agent whose Work a
landing broke is asked once, in its own session, to merge what landed; `dispatch accept`
lands the ready results that can land together, a change before what uses it, behind one
approval and one run of the checks; and Git's 3-way merge lands a result whose context
moved, once the merged checks pass on exactly what would land.

### Catch-up in place

- **What asks.** When Work lands, a live agent whose Work uses a declaration the landing
  changed the signature of or removed, or changes a declaration the landing changed, is
  asked once to merge it. A whole-file or textual interaction never asks. The ask is fixed
  text and names the landed Work, each declaration with its old and new signature when
  Dispatch has them, and the recorded commit. Packet S of this release got this one, as its
  turn ended, after packet P2h landed:

  ```text
  Dispatch: Work 01M75EBG (Claude Code) landed changes this Work relies on.
  - tests::a_refresh_step_follows_what_the_work_can_do (src/orchestrator.rs), which this Work also changes: 01M75EBG changed it
  - next_step (src/orchestrator.rs), which this Work uses: 01M75EBG changed its signature, pub(crate) fn next_step( run: &RunRecord, decision: Decision, source: Option<&s0::Recorded>, ) -> (String, Option<String>) → pub(crate) fn next_step( run: &RunRecord, decision: Decision, source: Option<&s0::Recorded>, asked: Option<&catchup::Asked>, ) -> (String, Option<String>)
  These changes are in refs/dispatch/source (commit 5e86363d).
  To catch up: commit your work, run git merge 5e86363d7b31ffeffe31f4a1fa84256b7994badf, update what uses them, and run the project's checks. Dispatch asks once per landing.
  ```

  The command names the commit in full: Claude Code 2.1.296's worktree isolation refuses
  any command that names `refs/dispatch/source`.
- **Claude Code.** The ask arrives as a turn ends: the `Stop` hook keeps the turn going
  with it, and Claude Code shows it as `Stop hook error: Dispatch: …`. An idle session is
  woken by a second, background `Stop` hook, the waiter, and Claude Code shows that as `Stop
  hook feedback`. Each turn's end starts a waiter. It looks for an ask once a second and
  ends with its session, when the next turn's waiter replaces it, or after 8 hours, so a
  session idle longer gets the ask when its next turn ends.
  - On this build's own work, packet P2h was idle when the batch it built on landed. Its
    ask was written 1 s after the landing, and the waiter woke the session 0.9 s later.
  - Packet S had been idle for 16.7 hours when P2h landed, so no waiter was left. Its ask
    came by `Stop` when the integrator next gave it a turn, 2 min 8 s after it was written.
- **Hermes.** At the end of a turn that edited files, the new `pre_verify` hook keeps the
  turn going with the ask, on that turn's first verify only, so it uses none of the nudges
  `agent.max_verify_nudges` allows. Otherwise the ask comes with the next turn, after any
  notes, through `pre_llm_call`. Hermes cannot be woken while idle, so an ask waits for its
  next turn. This build's one Hermes packet ran wrapped, which is never asked, so Hermes
  catch-up is shown by stand-in tests only.
- **Codex and Cursor get none:** they have no hook that can carry it.
- **Once, and nothing more.** Symbol-level only, once per landing and Work, fixed text.
  Landings before a hook takes the ask are one ask that names each. Dispatch never
  relaunches the agent, merges or refreshes for it, or asks twice about one landing. Only
  Work whose S0 a merge can move is asked, so wrapped Work (`dispatch attach -- <agent>`)
  is not. An ask expires when the Work's S0 already moved past its landings, or the Work is
  no longer in progress.
- **What follows.** When the agent merges the commit, S0 follows it as in 0.4.18, the
  owner checks the Work again, and the next verdict is taken against the new S0.
- **Where it shows.** `dispatch status <id>` gains a `Catch-up` section, `asked to catch up
  with <landed Work> at <time>`, and `dispatch watch` shows the same line under a REFRESH,
  until S0 moves past those landings. The REFRESH next step for such Work reads `its agent
  was asked to merge <commit>; once its S0 moves, Dispatch checks it again`.
- **Recorded** as `catchup.written`, `catchup.delivered {via: stop | wake | verify | turn}`
  and `catchup.expired {why: s0_moved | left_working}`.

0.4.18 told an agent in a note and asked nothing of it.

### Landing is one decision

`dispatch accept` with no ID, in a Git project where two or more results wait for review
and were frozen with the same checks, execution and authority:

- **The proposal.** The results in the order they would land, each with its Δ digest and
  why it comes where it does; those left out, with the verdict and reasons; those that can
  only be accepted on their own; and the batch digest. A change comes before what uses it,
  and otherwise the result that finished first comes first. Each result is judged against
  the source with every result before it, so one that a change before it breaks is left
  out, instead of landing first and being broken by it. The last batch of this release:

  ```text
  3 results can land together, in this order:
    1. 01M75EBGPC4ARYSM3B6Z51WHK7  Δ 945b2aa032c5  claude session in 0500-p2h
         by finish time
    2. 01M766VKVJY334G9HQS2BWK5W1  Δ e3bdaeab6614  claude session in 0500-u
         by finish time
    3. 01M76R0RRK5CCXQ6XV05SCN7GZ  Δ 36bdf374d014  claude session in 0500-v
         by finish time
  Batch 50bcf2398ace. The project's checks run once on all of them together; if they fail, only the results before the first one that fails land.
  error: nothing was decided: to land 3 results together, run dispatch accept --expect 50bcf2398ace
  ```

  On a stand-in project where one result changes `validate`'s signature and another
  calls it, the changer comes first and the caller is left out:

  ```text
    2. 01M7AS4AKN3RTDCFNSSKBQ2AMF  Δ 720be956c2a1  attached work in wt-changer
         before 01M7AS48: this Work changes the signature of validate (src/auth.rs), which it uses
  Left out:
    01M7AS484ZM3BC5WK8YQ5H88DF  attached work in wt-user
         REFRESH: fact_broken: pub fn validate(token: &str) -> bool => pub fn validate(token: &str, strict: bool) -> bool
  ```

- **One approval.** In a terminal it asks `Land 3 results together?` with Cancel focused,
  so Enter decides nothing. Without a terminal it decides nothing and prints the `dispatch
  accept --expect <batch digest>` to run. This replaces 0.4.18's refusal, `3 results wait
  for review for <source>; nothing was decided. Name the one you mean:`.
- **`accept --expect <batch digest>`** judges the batch again under the source lock, and
  refuses with nothing applied if the results that can land changed. The merged checks run
  once on the combined tree. If they fail, Dispatch checks shorter prefixes, each tree once,
  to find the first result they fail with, lands the results before it, names that result
  with its logs, and exits 1. If they fail with the first result alone, nothing lands, and
  the message says the source's own checks may be failing.
- **One patch.** What lands is applied as one patch, under the same world-digest fence as a
  single accept, and makes one `refs/dispatch/source` commit. The batch above left the
  reflog line `dispatch: 3 results landed together (batch 50bcf2398ace)`. Each result is
  then recorded as a single accept records it, naming the batch.
- A result's own Δ digest given without its ID is refused, with the command that names it.
  One waiting result, `accept <id>` and plain directories work as before.
- **Recorded** as `batch.proposed`, `batch.verified {tree, passed, runs, lands, failed}` and
  `batch.landed`, on each result.

### 3-way landing

- **What changes.** A result whose patch no longer applies as it is, because the source
  changed lines around its edit but none it edits, is merged 3-way from S0 (`git apply
  --3way` in a temporary index; the source's files, index and HEAD are untouched). A clean
  merge is CONTINUE with the new reason `merged_three_way` (`applied 3-way from S0`), and
  the file and symbol checks then run against the source as usual. 0.4.18 refused it as
  `patch_conflict`; 0.4.17's 4 hand re-applies and 0.4.18's 1 were all of that kind.
- **Only with merged checks.** They run on the merged tree, and accept lands exactly that
  tree, from a patch it keeps beside the run. When they cannot run (none configured, turned
  off, no permission to run commands, `coherence.accept: strict`), the verdict is REFRESH.
  On a stand-in project:

  ```text
  Applied Candidate A to ~/project (1 file(s) changed).
  Merged 3-way from S0; what landed: ~/.dispatch/runs/01M7AS4X8Q79MCFA04E0HDEZR3/delta-landed.patch
  ```

- An overlapping edit stays REFRESH `patch_conflict`, which now says what was tried: `the
  patch does not apply as it is to src/lib.rs, and a 3-way merge from its S0 conflicts`.
- Δ never changes, and `--expect` still names Δ's digest. `result.applied` records Δ's
  digest and the landed patch's.
- Not for plain directories, SHA-256 repositories, sources with an untracked nested
  repository, or files whose bytes Git converts. A file without symbol support that the
  source also changed stays REFRESH `fact_broken`, merged or not, so this helps Rust and
  Python files.

### REFRESH next steps

- **One step, everywhere.** `check`, `finish`, a result's summary, `dispatch watch` and
  accept's refusal give the same REFRESH next step.
  - Watch now reads `It can't be accepted as is: <the step>. Or r reject it.`, where 0.4.18
    said `It can't be accepted as is. r reject it, then run the agent again from the
    current source.`.
  - Accept's refusal of a REFRESH now ends `Next: <the step> (or dispatch reject <id>).`.
    For a native run that is `Next: dispatch refresh <id> (or dispatch reject <id>).`,
    where 0.4.18 said `Run 'dispatch refresh <id>' to redo the work on the current source,
    or 'dispatch reject <id>'.`. For attached Work, 0.4.18 said `Run your agent again on
    the current source and attach it, or run 'dispatch reject <id>'.`. The rest of the
    refusal, and a STOP's, are unchanged.
  - Watch's notice after a refused accept now reads `<name>: not applied: stale (REFRESH).
    See its Next line.`, where 0.4.18 said `… The source is unchanged. Next: r reject it.`.
    A step can be longer than the notice's two lines; the Work's Next line shows it whole.
- **Resume where that works.** A finished result whose session can be resumed in its
  worktree (registered by its runtime's hook, every session ended, the worktree still
  there), with a source recorded since its S0, now reads: `resume its session in
  <worktree>, then commit there and run git merge <commit>; the session goes on as new Work
  from this Work's S0, and this Work closes as superseded; Dispatch then moves that S0 and
  checks it again`. The resumed Work keeps its S0's time, the earlier Work's start when
  that S0 never moved, so a landing made before the resume still counts as newer than that
  S0, and the merge moves it.
- **`patch_conflict` says what 3-way tried** (above).

### Smaller items

- `dispatch status` with no Work lists the worktrees found, not tracked, and `status --json`
  then prints `{"found_not_tracked": [...]}`. A command that only reads and prints ends
  quietly when its reader goes away (`dispatch history | head -1`), where it panicked with
  `failed printing to stdout: Broken pipe`.
- Stopping a run's agent (a hangup, Ctrl-C, the deadline) no longer leaves behind a process
  it was starting at that moment: Dispatch kills its process group again while anything is
  left in it. Seen on macOS.
- A resumed session is no longer told that its new Work and the Work it reopens interact.
- `dispatch setup --checks` no longer replaces configured checks on two Enters: its menu
  opens on letting Work run them, and changing them keeps each one selected until you clear
  it. `/checks` in a session works the same way.
- A landing note ends `The source with this change is commit <40 hex>.`, without
  `(refs/dispatch/source)`, so an agent in a Claude Code worktree can merge it as written.
- A Claude Code worktree that a busy machine left unlocked for more than 3 seconds was
  listed as not tracked for good; it now becomes Work once the session that made it locks
  it.
- An undone merge no longer records a move of S0 to itself on every tick.
- Test and race fixes: the owner checks Work again when its S0 moves, even when its Δ is
  byte for byte the same; and four tests that asserted a step too early or measured the CI
  host are fixed.

### Measured, honestly

Counts from this build's own work, from the documented queries on its state
(`docs/queries/notes-measured.sql`, `catchup-measured.sql`, `batches-measured.sql`). The
first wave of packets ran under the pinned 0.4.18, the second under a release build of this
branch. "Followed by" means recorded later, not caused by, and agreement is only with
Dispatch's own later verdict, never a human judgment.

- **Notes:** 26 delivered (12 by 0.4.18, 14 by the release build), 0 unsettled. The next
  verdict was REFRESH for 7, all 7 counting as agreement; CONTINUE for 7; none yet for 12.
- **Catch-up:** 2 asks naming 3 landings; 0 asked twice; 11 declaration targets and 0
  file-level. 1 delivered by waking an idle session, 1 by `Stop`. Both agents merged:
  S0 moved 271 s and 96 s after delivery, and the next verdict was CONTINUE both times. No
  relaunch.
- **Batches:** 2 landed, of 7 and 3 results, each with one merged-check run and no
  bisection. 15 Works landed in all, 10 of them in those batches, behind 7 approvals.
- **Suite runs:** 20 for 15 landed Works, 1.33 per landed Work (0.4.18: 1.71).
- **Proposals:** computing one took 8.8 to 22.2 s on this repository (4 proposals of 2 to 7
  results).
- **3-way:** no landing on this build needed it. This one is read from the run events (no
  event carries `merged_three_way`), not from a documented query.
- **Coordination conformance:** [[C: the four-harness coordination cells: notes, catch-up, S0 following the merge, batch landing, for Claude Code, Codex, Cursor and Hermes]]
- **Landing a verified result:** [[C: p50 of 10 accepts of an already verified result on the release build, with the load]]
- **Native start:** [[C: native start on the release build, median and load]]
- **Hook cost:** [[C: Stop hook p95 over 1,000 calls with an ask pending, on the release build, with the load]]
- **Out of the box:** [[C: install line, setup and accept on a temporary HOME with the release build: commands and flags]]

Nothing here says tokens, minutes or money were saved.

### Upgrading to 0.5.0

- No migration; the schema stays at 24. A run whose verdict carries `merged_three_way`
  cannot be read by 0.4.18 (``error: invalid committed run projection: unknown variant
  `merged_three_way`, …``), so roll back only by restoring a backup of the whole state
  directory.
- `dispatch setup` offers Claude Code's hooks once as an Update: `Stop` gets the waiter as a
  second entry (`hook claude-wait`, `asyncRewake`, timeout 28800). Until then an ask still
  arrives as a turn ends, but an idle session is not woken.
- Hermes's block gains `pre_verify` (5 s), offered once as an Update, and Hermes asks you
  once more, for that hook, at its next start in a terminal.
- One sleeping waiter per idle Claude Code session in a watched project with Work. It
  looks for an ask once a second.
- "Upgrading to 0.5.0" in `docs/release-install.md` has the details. As since 0.4.17, the
  next command after an upgrade in place restarts the owner.

### Not in 0.5.0

- Splitting a task among agents.
- Relaunching an agent, or merging or refreshing on its behalf.
- Catch-up for Codex and Cursor, and waking an idle Hermes session.
- An ask at a resumed session's start: it waits for that session's next turn.
- Batches for plain directories or from `dispatch watch`'s keys; landing results after a
  failing one; automatic retries; a commit on landing.
- Seeding warm build output into workspaces.

Production code under `src/`, excluding `#[cfg(test)]` items, grew by 2,603 non-blank lines
since 0.4.18 (2,188 without comment lines): batch landing is 962 of them and catch-up 618.

Site impact: `dispatch accept` without an ID now proposes a batch and, with `--expect <batch digest>`, lands it (the review guide's and commands page's "acts only when exactly one result waits", the agent guide's same line, and `--expect`'s description); accept's output gains the batch proposal, the menu, `Merged checks on the source with N results`, `Applied N results … as one patch` and `Merged 3-way from S0; what landed:`; a new CONTINUE reason `merged_three_way` and new `patch_conflict` text for the verdicts page; watch's REFRESH Next (the `watch-landed` capture) and the resume step; accept's REFRESH refusal now ends with the same step as `check`, `Next: <step> (or dispatch reject <id>).` (native: `Next: dispatch refresh <id> (or dispatch reject <id>).`), where concepts' video transcript, the examples page's merged-check example, the `accept-refused` and `merged-check` captures and the explainer that the videos and GIFs are rendered from show 0.4.18's `Run your agent again …` or `Run 'dispatch refresh <id>' to redo the work …`; watch's notice after a refused accept now reads `See its Next line.`; the landing note's last line (the Claude Code guide, the agent guide and concepts' transcript show `… (refs/dispatch/source).`); `git merge refs/dispatch/source` in concepts, several agents, the review guide and the agent guide, which Claude Code refuses, should name the commit; the hooks: Claude Code's eighth (the waiter) and Hermes's `pre_verify`, with setup's two new consent sentences; the claims "A note asks nothing. Dispatch never wakes an idle session or holds a turn." (Claude Code guide), "Notes inform; they ask nothing." (limits) and `product.ts`'s `addsLittleWhen` ("It says what changed, and tells a live Claude Code or Hermes session"), which now ask a live agent once to catch up; `dispatch status` gains a Catch-up section, and with no Work lists worktrees found, not tracked.
