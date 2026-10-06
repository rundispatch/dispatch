## Dispatch 0.4.9 — Interactions, measured

0.4.8 began reporting where pieces of unintegrated Work touch each other. 0.4.9
records, from real use, how often those reports turn out right, so that nothing
is built on them before that is known.

- **Every landing records what was known.** When Work is applied, by `accept`
  or by auto-apply, Dispatch records the interactions its watching owner had
  reported between it and every other piece of Work in progress
  (`interaction.landed`). Pieces with no reported interaction are listed too,
  so misses can be counted.
- **The owner records what happened next.** The watching owner records its
  next verdict on each of those other pieces (`interaction.outcome`): CONTINUE
  or not, interaction or not.
  - An outcome is scored only when nothing else could explain it. The landing
    must have been observed, and the other piece analysed and valid before. If
    another change reached the project, or the piece itself changed before it
    was evaluated, the outcome is kept but labelled with why it cannot be
    scored.
  - Each outcome is recorded once, across owner restarts.
- **Documented queries read it.** `docs/queries/interactions-measured.sql`
  gives, from your own state directory:
  - landings by status;
  - outcomes by class;
  - precision (invalidated among predicted) and misses, with their counts;
  - the same by rule;
  - landings whose outcomes are missing.

  `docs/coherence-validation.md` says what the numbers can support.
  "Invalidated" means Dispatch's own later verdict, not a confirmed conflict.
  Nothing leaves your machine.
- **Live patches are published atomically.** A watcher or the owner could read
  a work-in-progress patch while it was being rewritten. Patches are now
  written to a temporary file and renamed.
- **No policy changes.** Interactions stay advisory: no verdict, accept or
  auto-apply decision depends on them.

**Fixed.** A landing auto-applied by an owner on its first tick after a restart
would have seen no interaction view. The owner now compares Work before it
auto-applies.

**Evidence.** A real-agent trial on a small Python project ran five Claude Code
sessions at once:
- A changed the signature of `auth.validate`;
- B added a caller of it;
- C changed its body;
- D changed an unrelated module;
- E changed another function in the same file.

Before anything landed, Dispatch reported A against B (B uses the signature A
changes) and A against C (both change `validate`), and nothing else.

D landed first. Its landing was observed, and within 5 s the owner recorded
four outcomes: all scorable, none predicted, all CONTINUE. Then A landed:
- B: predicted, and REFRESH (`fact_broken`);
- C: predicted, and REFRESH (`patch_conflict`);
- E: not predicted, and CONTINUE.

The documented queries over that state report 2 landings and 7 scorable
outcomes. Precision was 2 of 2 predicted, with 0 misses among 5 that were not
predicted. That is one small trial, not a rate.

**How it was built.** This release was built by four Claude Code sessions
working in parallel, each on one packet in its own worktree. Dispatch 0.4.8
watched them, and every packet was integrated through `dispatch accept` after
review. Dispatch reported no interactions between the packets, which was
correct: none of them shared a file.

The process found two 0.4.8 defects, both still open:
- a `SessionStart` hook can be killed by Claude Code's 60 s timeout while
  Dispatch registers the Work under heavy load, leaving the session untracked;
- `dispatch attach --workspace` walks ignored build output, and can fail on a
  file that disappears mid-walk.

**Upgrading.** No migration; the schema stays at 24. Run `dispatch stop &&
dispatch start` so the project owner is 0.4.9.
