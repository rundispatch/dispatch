# Plan: Dispatch 0.4.18, agents that know about each other

**Approved, 2026-11-04.** Base: `fb4ef53` (Dispatch 0.4.17 and PR #28). Third and last 0.4.x step
on the road to 0.5.0 (`docs/roadmap-0.5.0.md`). Built as 0.4.17 was: workers wrapped by the pinned
released Dispatch, here 0.4.17. Packets are landed with `accept --expect`, and CI validates each
landing on a draft PR.

## Context

This is the third and last 0.4.x step to 0.5.0 (`docs/roadmap-0.5.0.md`, approved 2026-10-17). Today a
live agent learns nothing from Dispatch. It doesn't hear when another Work changes what it uses, or when a
landing makes its result stale. The person finds out at accept (REFRESH) and fixes it by hand.

In 0.4.17's build, none of the 15 Claude worker sessions of 0.4.16 and 0.4.17 ran a `git merge`, `rebase`
or `pull`. Every stale result was re-applied 3-way by the integrator, four times (P2b, P2c, P5b, Rb).
0.4.18 does two things:
- it tells live agents what overlaps their work, and what landed under it;
- it lets S0 follow an agent that merges what landed, with proof.

These give 0.5.0's condition 1 ("agents know about each other") its first measurement.

**Decisions (user, 2026-11-04):**
- P0 probes on real Claude Code and Hermes sessions: approved.
- A landing note gives the facts and names the recorded source commit to merge. It is information,
  never an instruction.
- Carried items included: hook waits and the second SessionStart; EARLIER as a list (required); native
  start's last 0.8 s; the warm-workspace measurement (0.5.x gate); `f` in watch under consent.
- Workers: Claude Code, Codex and Hermes. Every non-Claude packet is reviewed before it lands, and its
  fixes are made by Claude Opus 5.5.

**Left out:** the capture/removal overlap guard and same-second ties in `freshest_patch`, since neither
has failed in real use. The `ubuntu-22.04` runner remains a decision for the user.

## Evidence (code at main `fb4ef53`)

- **Delivery paths (docs and source read 2026-11-04).**
  - Claude Code 2.1.296: `hookSpecificOutput.additionalContext` is wrapped as a system reminder. It
    lands next to the tool result on PreToolUse and PostToolUse, and before the first prompt on
    SessionStart. Each string is capped at 10,000 characters. `cwd` follows Claude into worktrees
    and after `cd`.
  - `asyncRewake` can wake an idle session. It is not used: waking an agent is 0.5.0's catch-up.
  - Hermes 0.18.2: a shell `pre_llm_call` hook (config `hooks:`) may print `{"context": "..."}`. The
    text is appended to that turn's user message, once per turn (`agent/shell_hooks.py`
    `_parse_response`). The payload carries `cwd`. New shell hooks go through Hermes's allowlist,
    `~/.hermes/shell-hooks-allowlist.json`.
- **Hooks today.**
  - Claude's hook set is `INSTALLED`/`EARLIER` in `runtime/claude.rs:187-212`; EARLIER holds one
    earlier set.
  - Every handled event goes through `runtime::ingest` (`runtime.rs:152`), which calls Git and the
    owner record.
  - `session_work` waits 3 s to publish registrations (`runtime.rs:236`).
  - `record_session_end` and `record_needs_you` wait 3 s for the run's operation lock, then open
    SQLite (`attach.rs:1238`, `1285`), inside a 5 s hook timeout.
  - A PostToolUse hook on every tool call can afford none of this.
- **Interactions.**
  - The owner computes footprints and edges each tick (`serve.rs` around 1240-1330) and writes
    `watchers/<key>.interactions.json` (`background::write_interactions`).
  - The rules are `SameDeclaration`, `Uses`, `File` and `TextualOverlap` (`coherence/interactions.rs:219`).
  - `interaction.landed` and `interaction.outcome` already measure them (`coherence/measure.rs`).
- **S0.**
  - Attached Git Work's S0 is the merge base at attach or discovery, materialized under
    `runs/<id>/baseline` (`docs/attach.md` "S0 and confidence").
  - `source::world_commit` (`source.rs:405`) records a checkout, uncommitted changes included, as a
    commit in its own repository.
  - `materialize_baseline_from_commit` (`source.rs:353`) makes a baseline from such a commit.
  - Landings go through `source::apply_validated` (`apply.rs:455`, `810`).
- **Native start.** 10.8 s median against a 10 s target. P6's profile (0.4.17) puts the rest in the
  `open()` calls the fingerprints still make, under endpoint security.

## Packets

### P0 Evidence (integrator; `~/dispatch-dev/p0-0418/`)

1. **The roadmap commit.** Cherry-pick `5b52553` (the 0.5.x candidate that missed PR #28) as the
   release branch's first commit.
2. **Probes (approved).** Run them in a scratch repository, with probe hooks only in its local
   settings, which log every payload and return a marked context string. Payloads are saved as
   fixtures.
   - **Claude:** in a `claude --worktree` session, check that the SessionStart and PostToolUse
     `additionalContext` reach the model, which acknowledges the marker. Also record how often
     PostToolUse fires in a short task, and whether `cwd` is a subdirectory after `cd`.
   - **Hermes:** in a `hermes -w` session, check that `pre_llm_call` context arrives. Also record
     whether adding a second hook re-asks the allowlist, and whether 0.4.15's `on_session_finalize`
     block still matches.
3. **Hook cost baseline.** Time today's no-op hook path, p50/p95 at ordinary load, as the base for the
   15 ms gate.
4. **Re-profile native start** on the 0.4.17 fixture (3.6 GB `target/`), split by phase. This sets
   P5's approach.
5. **The second-SessionStart race (P9's question 2):** reproduce it as a failing test, or record that
   it doesn't reproduce.

### P1 Delivery and hook reliability (Claude Code, Opus 5.5)

- **Inbox.**
  - The owner writes at most one inbox file per workspace,
    `state/inbox/<sha256 of canonical workspace path>.json`, by rename. The file holds the pending
    notes and their ids.
  - The hook's fast path walks up from `cwd` to the worktree top (a `.git` file), with no Git
    command. It renames the inbox to `inbox/delivered/<id>.json`, which is atomic, so a note is
    delivered once, and prints the notes.
  - The hook never opens SQLite, takes no lock and makes no network call.
- **Claude:**
  - `additionalContext` on SessionStart (merged into today's start reply) and on PostToolUse (all
    tools, timeout 5 s);
  - the new set goes into `INSTALLED`, and `EARLIER` becomes a list of every earlier set, so
    installs from 0.4.16 and 0.4.17 are recognized and updated.
- **Hermes:** a `pre_llm_call` shell hook returning `{"context"}`. The exact-match block in
  `runtime/hermes.rs` extends to two hooks. Removal still recognizes 0.4.15's one-hook block, and
  setup says the allowlist will ask once.
- **Events:**
  - `note.written` is recorded by the owner when it writes an inbox.
  - `note.delivered` is recorded by the owner when it sees the delivered file, so the hook never
    writes events.
  - An undelivered inbox for Work that finished is `note.expired`.
- **Hook waits.**
  - SessionEnd and Notification/Stop record their mark as a file the owner applies (the same
    rename pattern), instead of waiting 3 s for the run lock and opening SQLite in the hook.
  - Registration publishing stays within budget. The second-SessionStart race is fixed per P0.5.
- **`f` in watch** finishes without asking when standing consent covers it.
- **Tests:**
  - one delivery per note under concurrent hooks;
  - a hook in a subdirectory finds its worktree;
  - nothing is printed when the inbox is empty;
  - an unwatched project costs nothing;
  - EARLIER upgrades from both earlier sets;
  - the Hermes block with one or two hooks;
  - session-end and needs-you marks applied by the owner, none lost when the owner is down (applied
    when it comes back);
  - `f` under consent;
  - the payload fixtures from P0.

### P2 What a note says (Codex; reviewed, see "Review gate")

- **A pure function,** `coherence/notes.rs` `compose(projection, landings, sent) -> Vec<Note>`, with
  no I/O. The owner calls it each tick after interactions and passes the result to P1's inbox.
- **What gets a note:** only `SameDeclaration`, `Uses` and `TextualOverlap` interactions, and
  landings that changed a declaration this Work uses or edits. File-level interactions never do.
- **Templates:**
  - fixed text, at most 3 lines, no model-written text;
  - each note names the other Work by id and runtime, the target (path and declaration), and the
    change;
  - a landing note also gives old → new for a signature, and the line "The source with this change
    is commit `<sha>` (`refs/dispatch/source`)". It never says "you must".
- **Sent once** per (other Work or landing, target, change), rate-limited per Work. The gate is a
  median of at most 1 note per Work per 10 minutes, so a burst is merged into one inbox.
- **Tests:**
  - template fixtures for each rule;
  - no note from file-level facts;
  - deduplication;
  - the rate limit;
  - a landing note names the recorded commit.

### P3 S0 follows integration (Claude Code, Opus 5.5)

- **Recording the source.**
  - After each landing (`apply.rs`, after `apply_validated`), record the root with
    `source::world_commit` under `refs/dispatch/source`.
  - Keep the history as `refs/dispatch/source` reflog entries plus a list on disk:
    `state/projects/<key>/sources.json`, each entry with its commit and time.
- **When S0 moves.** On each tick, for attached or discovered Git Work whose workspace HEAD moved,
  S0 moves to the newest recorded commit (or root HEAD commit) that:
  - is newer than the current S0;
  - is an ancestor of the workspace HEAD (`git merge-base --is-ancestor`);
  - and while no `MERGE_HEAD`, `REBASE_HEAD` or `CHERRY_PICK_HEAD` exists.
- **How it moves.**
  - The new baseline is materialized in a new directory with `materialize_baseline_from_commit`,
    and the old one is kept.
  - The run gains `baseline_history`, and an `attach.s0_moved` event records from, to, head, proof
    and Δ digests before and after.
  - Every earlier verdict's (S0, Δ) stays recomputable.
- **No proof, no move:** a remote integration, a cherry-pick, a squash or a merge of an unrecorded
  state. These REFRESH as today.
- **Native runs and plain directories** keep a fixed S0.
- **Tests:**
  - a merge of the recorded commit moves S0, the Δ shrinks to the agent's own change, and the
    verdict is CONTINUE;
  - a merge in progress doesn't move it;
  - a cherry-pick doesn't move it;
  - an agent reverting a landed change shows the revert in the new Δ;
  - a rebase onto the recorded commit moves it;
  - finished, resumed and superseded Work keep their history;
  - `explain` shows the S0 line with its move.

### P4 Queries (Claude Code, Opus 5.5, small)

- **`docs/queries/notes-measured.sql`:** per note delivered, whether it was followed by:
  - the Work's Δ touching the target;
  - an S0 move;
  - its next verdict.

  Agreement is with Dispatch's own later verdict, counts only, as in `interactions-measured.sql`.
- **Tests:** the query against a fixture database.

### P5 Native start, part 3 (Hermes; reviewed)

- **The approach comes from P0.4.** The expected one: the copied fingerprint reuses the source
  fingerprint's per-file identity (size, mtime, inode) for files cloned by path, instead of opening
  them again. Fidelity stays proven by `snapshot_is_exact_private_and_excludes_state`.
- **Target:** 10 s median at ordinary load on the 0.4.17 fixture.

### M Warm-workspace measurement (integrator; no product code)

- **The 0.5.x gate** (`docs/roadmap-0.5.0.md` "After 0.5.0"):
  - measure cold against seeded first build (`cp -c -R` of the checkout's ignored build directories
    into a fresh workspace) and disk per workspace (`du`, and APFS free-space delta);
  - on Dispatch's repository and one Node project;
  - three runs each, with load averages recorded.
- **Recorded as measured** in the execution log, never as time saved. Its results decide whether the
  candidate gets planned.

### R Docs, AGENTS.md, version, release notes (Claude Code, Opus 5.5, last)

- **Version** 0.4.18, with release notes "Agents that know about each other" ending in
  `Site impact:`. Notes, the new hooks, the S0 line, and the docs that moved to `docs/reference.md`
  with PR #28.
- **AGENTS.md, under "What Dispatch does by itself":**
  - Dispatch speaks to live agents only through their runtimes' hooks, with fixed text, and asks
    nothing of them;
  - S0 moves only with an ancestry proof against a recorded source.
- **Docs:** `attach.md` (notes, inbox, S0 moves), `coherence.md` (S0 history), `reference.md`
  (events), and "Upgrading to 0.4.18" (hooks re-offered once, Hermes allowlist).

## Review gate for non-Claude packets (P2, P5)

The user, 2026-11-04: "I don't want subpar work to get merged into everything that we've built so far."

1. **Before landing, never after:**
   - the integrator (Opus 5.5) reviews the packet's diff line by line for correctness, simplicity,
     idiom, comment density, test quality, needless machinery and AGENTS.md's "keep Dispatch small";
   - findings are written to `notes-0418/review-non-claude.md`.
2. **Contract tests.** A separate Claude Code worker pinned to `--model claude-opus-5-5` writes tests
   against the packet's contract in this plan, not against its code. This catches what reading
   misses, as 0.4.17's consent gap showed.
3. **Fixes by Opus 5.5 only:**
   - a Claude worker (pinned) makes them as a "b" packet on top of the original;
   - the non-Claude packet lands only together with its "b" packet, never alone;
   - nothing is sent back to the model that wrote it.
4. **Models proven:**
   - every Claude worker's transcript is checked to show only `claude-opus-5-5` before the release
     says so;
   - Codex's and Hermes's models are recorded as their harnesses report them.
5. **The execution log** lists each finding, its fix and the commit.

Safety-critical code stays with Opus 5.5 workers: the hook fast path, consent, S0 moves and the
source record.

## Order, workers, landing

- **Setup:**
  - supervisor: the pinned 0.4.17 (`~/dispatch-dev/dispatch-0.4.17/`), state
    `~/dispatch-dev/state-0418`;
  - clone: `~/dispatch-dev/integration-0418` on `release-0.4.18` from `fb4ef53`;
  - `launch.sh` pins `--model claude-opus-5-5` for Claude workers;
  - a draft PR for CI, asked for first.
- **Waves:**
  1. P1, P2, P3 and P5 in parallel, with M beside them.
  2. P4 after P1 and P3.
  3. R last.

  The Claude workers in waves 2 and 3 are the first sessions to receive notes. That is 0.4.18's
  own measurement.
- **Landing:**
  - exit the worker; its finish under 0.4.17 records a pass;
  - for P2 and P5, the review gate comes first;
  - the user approves, then `accept --expect`;
  - a stale result is merged by its agent from `refs/dispatch/source` where possible, or else
    re-applied 3-way as a "b" packet;
  - CI must be green on all three jobs.

## Exit criteria (measured on this build and its fixtures)

- **Notes recorded:** every note has `note.written` and either `note.delivered` or `note.expired`.
- **Note volume:** median at most 1 note per Work per 10 minutes.
- **Note grounds:** no note rests only on file-level facts.
- **No S0 move without its proof:** the merge-in-progress, cherry-pick, revert and rebase tests pass,
  and every `attach.s0_moved` event carries its proof.
- **Hook cost:** PostToolUse p95 at most 15 ms with an inbox, at ordinary load. No hook waits on a
  lock or opens SQLite.
- **Agreement:** notes delivered on this build, and how many agreed with Dispatch's later verdict.
  Quoted as counts; this is the first measurement, with no target.
- **Re-applies by hand:** counted against 0.4.17's 4. Reported, not gated.
- **Native start:** at most 10 s median.
- **Suite runs per landed Work:** at most 1.75, with reuses counted.
- **Orphans:** no orphaned owners after a full test run.
- **Review:** every non-Claude packet reviewed before landing, findings fixed by Opus 5.5, models
  verified from transcripts.

## Not built

- Asking agents to act: no Stop `decision:"block"`, no `asyncRewake`, no waking idle sessions (0.5.0
  catch-up).
- Notes for Codex and Cursor, which have no context-injecting hook Dispatch installs.
- Batch or 3-way accept (0.5.0).
- Automatic refresh or merge on the agent's behalf.
- Seeding warm build output in workspaces. Only M measures it.

## Verification

- **Each packet:**
  - `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`;
  - finish under 0.4.17, then `accept --expect`;
  - CI on the draft PR (Linux, macOS, stress).
- **End to end, on a release build in a temp HOME:**
  1. Two Claude worktree sessions touch the same declaration; both receive one note.
  2. One lands; the other gets a landing note naming the commit.
  3. A `git merge` of that commit moves S0 (`attach.s0_moved`), and accept is CONTINUE.

  This uses a stand-in agent. A real Claude and Hermes run happens only with approval.
- **Hook timing:** p50/p95 over 1,000 PostToolUse invocations, with and without an inbox.
- **Queries:** `notes-measured.sql` on `state-0418` at the end of the build.

## Execution log

To be written as the build runs.
