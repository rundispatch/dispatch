## Dispatch 0.4.7 — Work you can finish where you see it

0.4.6 picked up Claude Code sessions as Work by themselves. 0.4.7 lets that Work
be verified, reviewed and cleaned up without you copying run IDs between commands.

- **Checks can run by themselves, when you allow it.** `dispatch setup --checks`
  can let Work in a project run that project's checks by themselves.
  - When Claude Code removes a session's worktree, Dispatch keeps its exact
    changes, as before. Then it runs the checks in a workspace rebuilt from S0 and
    those changes. The Work becomes Ready, or shows its failed checks, and waits
    for your review.
  - The permission names the exact `checks.verify` commands you approved. If they
    change, it no longer holds until you approve the new ones; `dispatch status`
    and `dispatch watch` say which.
  - It is stored in Dispatch's private state, never in the repository, so a
    repository cannot grant itself the right to run commands on your machine.
  - It runs checks only. Nothing is applied without your review.
- **Act from `dispatch watch`.** On a terminal, select a row with ↑/↓, then:
  - `f` finishes attached Work, asking first before running checks nobody allowed;
  - `a` accepts, through the same gate as `dispatch accept`;
  - `r` rejects, after asking;
  - `d` or Enter opens the review.

  Each key is exactly the command you would type. `watch --json` and output that
  is not a terminal are unchanged.
- **`dispatch clean`.** Workspaces Dispatch made for `dispatch attach -- <agent>`
  are kept after a reject. `clean` lists the ones whose Work is over and removes
  them, with their branches, only after you confirm. `--dry-run` only lists them,
  and `--yes` confirms where there is no terminal. Nothing is ever deleted on a
  timer, and each run's record and patch stay.

**Fixed.**
- **Accept now runs the merged-tree checks for discovered Work you finished.**
  Discovered Work finished with `dispatch finish --allow-unsafe-local` was accepted
  without its checks running on the merged tree. The flag was not recorded on the
  run, so a change that only the merged tree catches could apply. The authority
  is now recorded, and accept runs the checks.
- **Rejecting Work whose worktree was removed now closes it.** `dispatch reject`
  refused it ("no delivered result to review"), so it could not be closed without
  finishing it. Reject now closes it and keeps its patch in the run.
- **A worktree with no changes that vanished without notice now closes.** It
  used to show as `lost`; it closes with no changes, since nothing was lost.
- **`finish` no longer fails on a busy project.** `finish` refused the run when the
  background owner was busy with it at that moment. It now waits up to 5 s. Accept
  or reject typed at the command line no longer fails as "stale" because the
  owner had just recorded a verdict.

**Evidence.** A real-agent trial with Claude Code and Cursor Agent:
- **With consent.** A `claude -p --worktree` session registered Work with local
  authority. Its worktree was removed, and the exact changes were kept before the
  hook returned. The owner then verified the Work by itself: Ready, checks
  passed, nothing applied. After a teammate's commit, it was accepted from
  `dispatch watch` with the merged-tree checks run.
- **Consent voided.** Changing the project's checks voided the consent. The next
  session's Work kept its changes on removal and waited. The trial found that
  rejecting it was refused, which is fixed above.
- **Clean.** `dispatch attach -- cursor-agent` from the checkout finished Ready and
  was rejected. `dispatch clean --dry-run` listed its workspace, and `dispatch
  clean` removed it only after confirmation.

**Not built.** There is no Codex lifecycle integration: Codex's hooks need a trust
review of every hook definition, and its session end also means "idle".
`dispatch attach -- codex` remains the supported way to isolate Codex. There is
also no automatic apply for discovered Work, no retention timer, and no start at
login.

**Upgrading.** No migration; the schema stays at 24. State is forward-only: 0.4.6
cannot read a run finished by consent. After upgrading, run `dispatch stop &&
dispatch start` so the project owner is 0.4.7.
