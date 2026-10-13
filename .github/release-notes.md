## Dispatch 0.4.12 — Review the Work you mean

While investigating field reports (a finish that said "Ready for review" while `check` said
REFRESH, Work that seemed to need full IDs, landed work recorded as rejected, `clean` leaving
worktrees behind), review found a worse problem. Two pieces of attached Work, A and B, finish one
after the other. A's finish summary ends with `dispatch accept` / `dispatch reject`, with no ID.
Following it accepts **B**: an ID-less accept takes the newest result waiting for that source,
applies it and records a human acceptance for it, while A stays pending. A fixture reproduced it
on 0.4.11. 0.4.12 fixes it, and the problems those reports raised.

- **Decisions name their Work, and ambiguity is refused.** Every finish and result summary
  prints `diff`, `accept`, `reject` or `close` with the Work's full ID. `dispatch accept` or
  `dispatch reject` without an ID acts only when exactly one result waits for review for the
  source. With two or more, it changes nothing and lists them:

  ```text
  $ dispatch accept
  error: 2 results wait for review for ~/src/project; nothing was decided. Name the one you mean:
    dispatch accept 01M4SNHQS8ZP2FEGVG5A92BNNC  (work B)
    dispatch accept 01M4SNHPQV8CY5GWW19BZ3RDSJ  (work A)
  ```

  Work still in progress is listed as not counted. Unique ID prefixes still resolve, and what is
  recorded names the full ID.
- **`dispatch close` retires Work without judging it.** `dispatch close <id> --reason <reason>`
  records one `work.closed` event and no review. It never applies, commits, deletes a workspace
  or signals an agent, and it refuses Work a live `attach --` wrapper still owns. The reasons:
  - `unfinished`: Work nobody will finish. Its changes are frozen into its patch first, without
    running checks.
  - `workspace-removed`: unfinished Work whose workspace is gone. Its kept changes stay.
  - `already-present`: a result whose whole patch is already in the source. Part of it is not
    enough.
  - `superseded --replacement <id>`: a result that other finished Work replaces. Both patches
    are kept.

  STOP now points to `dispatch close <id> --reason already-present` instead of `reject`, so
  changes that landed some other way are no longer recorded as a rejection. `reject` on
  unfinished Work refuses and names `close`. In `dispatch watch`, `c` closes the selected Work
  after asking for the reason and a confirmation; Enter alone or Esc records nothing. A
  session that ended says so: finish it, resume it, or close it.
- **Finish, check and accept say what holds, and what was verified.** `finish`, and a native
  run's result summary, now show the patch, a verdict computed against the source now, and what
  accept would verify:

  ```text
  Finished, but stale (REFRESH)
  …
  Coherence: REFRESH — 1 file changed underneath
    patch_conflict: the patch no longer applies to c.txt; Git's full output: <run dir>/patch-conflict.txt; …

  At accept
    accept would refuse it now (REFRESH), before any merged checks
  ```

  - The heading follows the verdict: `Ready for review`, `Finished, but stale (REFRESH)`,
    `Finished, already in the source (STOP)`, `Verification failed` or `Closed`.
  - REFRESH and STOP never offer accept.
  - A patch that no longer applies names its paths and keeps Git's full output in
    `patch-conflict.txt` in the run directory.
  - `check` adds the same `At accept:` line.
  - `accept` prints `Merged checks:` before it applies anything. Either they passed, with the
    log directory, or they did not run, and it says why: the source has not moved, no checks
    are configured, merged checks are turned off, the Work has no recorded permission to run
    checks on the host, the patch is empty, strict mode, or the configuration could not be
    read. `watch`'s accept notice and the review screen say the same.
  - If the source moves while merged checks run, accept now says nothing was applied because
    the source changed after it checked.
- **`clean` never removes a workspace live Work uses, and says what it keeps.** Before, a
  workspace Dispatch made could be listed for removal while new Work was attached to it, for
  example after repairing it and attaching it again. Now a workspace Dispatch made is cleanable
  only when no unfinished Work, or Work waiting for review, uses its path. Closed results are
  cleanable. Worktrees you or an agent's runtime made are listed as kept, with their Work IDs
  and how to inspect or remove them with Git, and are never removed. "Nothing to clean" no
  longer suggests that every agent worktree is gone.
- **Work that began from older source says so, once.** Accept does not commit, so a worktree
  made from HEAD, or from a remote branch, can miss accepted changes or local commits. For
  attached Work that Dispatch did not start from the current source (an existing worktree you
  attach, or a session a runtime reports), Dispatch compares the source with the Work's S0 once,
  right after it records the Work, in both directions. `attach`, `status`, `explain` and `watch`
  show the result:

  ```text
  Source     differed from its S0 at 1 path (observed 2026-10-13 03:40 UTC, source HEAD aab43dbb): a.txt (in applied Work 01M4SNJ9XR31N21H3CT04PN7XR)
             Work started with dispatch attach -- <agent> begins from the current source, including uncommitted changes.
  ```

  - The advice leads with `dispatch attach -- <agent>`, which works for any agent.
  - Only a Claude Code worktree also gets Claude-specific advice: local commits reach it only
    with `worktree.baseRef: "head"`, and neither that setting nor a commit copies uncommitted
    changes.
  - A path is named as another Work's only when that applied Work's patch contains it.
  - The observation is recorded once and never recomputed. Changes after the start are the
    verdict's business.
  - Runtime-registered Work that no 0.4.12 owner saw recorded, or whose observation
    failed, shows "not observed", and is never filled in later.
  - It is advisory: no verdict depends on it, and Dispatch changes no settings and commits or
    pushes nothing.

**Disclosure.** Before 0.4.12, an ID-less `dispatch accept` or `dispatch reject` with several
results pending for the same source acted on the newest one. It could record a human decision,
and for accept apply the patch, on Work nobody chose. Existing records are not rewritten.

Not in 0.4.12: it does not make `claude --worktree` inherit uncommitted source, and it does
not replace Claude Code's worktree creation. Discovering agent worktrees without hooks, session
liveness and project setup are planned for 0.4.13. Nothing is refreshed, retried or applied
automatically.

No migration: the schema stays at 24. JSON changes are additive: `outcome.closed`, the
attachment's `source_at_start`, the `attach.source_observed` event, more `work.closed`
reasons, and the state `closed` in `watch --json`, `serve --json` and `history`. Scripts
that call `accept` or `reject` without an ID should pass one. "Upgrading to 0.4.12" in
`docs/release-install.md` has the details; restart the owner after upgrading.

Built with Dispatch: four packets, three for the code and one for these docs, each an
interactive Claude Code session that a pinned Dispatch 0.4.11 started in its own workspace
(`dispatch attach --root <clone> -- claude`), reviewed and accepted by a human one at a time.
