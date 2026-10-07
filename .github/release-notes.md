## Dispatch 0.4.10 — No silent gaps

Building 0.4.9 with Dispatch itself, four agents working at once, exposed two
gaps on a busy machine:
- a Claude Code session's start hook was killed while Dispatch was still
  registering it, leaving the session untracked and half-made run state behind;
- `dispatch attach` failed when a build tool deleted a file while Dispatch was
  reading it.

0.4.10 closes both.

- **A session is tracked, pending, or told it isn't.** For a session in its own
  worktree of a watched project, Dispatch finishes registering within 40 s, well
  inside the hook's 60 s timeout. The session is told which of three outcomes it
  got:
  - **tracked:** its Work exists before the hook returns;
  - **pending:** its starting state is saved, and the watching owner creates the
    Work moments later;
  - **not tracked:** its starting state could not be captured in time, or the
    worktree kept changing while it was captured.

  Work is assembled outside `runs/` and moved in only when complete, so a
  killed hook never leaves a half-made run behind. A repeated session start
  lands on the same Work.
- **Registration ignores build output.** Attaching Work no longer reads the
  integration root's whole tree. Outside strict mode nothing used that walk,
  and it was what made registration slow and fragile: Claude Code worktrees,
  each with its own build directory, live inside the root. Where a walk remains
  (strict mode), a file that disappears mid-walk counts as a difference, not a
  failure.
- **A moving workspace never gives a mixed starting state.** If the worktree
  changes while its starting state is captured, Dispatch captures it once more,
  and otherwise refuses rather than record a baseline mixing two moments.
- **Running Dispatch on Dispatch.** `docs/self-hosting.md` is the setup that
  built this release:
  - a pinned, verified release binary;
  - a dedicated state directory outside `/tmp`;
  - a dedicated integration clone, with project-local hooks;
  - one isolated workspace per worker;
  - landing one contribution at a time.

  It also sets the rule for build caches in checks: a cache may be reused only
  when one workspace's build can never stand in for another's.

**Fixed.**
- **Notices name the full Work ID.** Sessions that started together all read
  "Work 01M4BRE4" in their notice; Work created in the same quarter second
  shares its first 8 characters.

**Evidence.** A stress trial on a clone of Dispatch, with three cargo builds
putting 1.5 GB of churning build output under the root, every core busy, a
disk write loop, and load averages of 19–25:
- **Six Claude Code sessions started at once:** all six were registered by their
  own hooks in 8.0–8.3 s, with exactly one Work each.
- **Hooks killed at 0.05–4 s:** those killed before their starting state was
  saved left no Work, and the owner removed their leftovers. Those killed after
  it was saved were published by the owner.
- **The owner killed with SIGKILL while a registration was pending:** the
  restarted owner published it.
- **A repeated session start:** one Work, the same starting state, both sessions
  recorded.
- **`dispatch attach` while build output churned:** 10 of 10 succeeded, and no
  build output entered any starting state or patch.
- **Two files rewritten continuously during capture:** all 6 sessions were told
  they were not tracked. No mixed starting state was accepted.

Building 0.4.10 itself, four workers under Dispatch 0.4.9 all registered by
their hooks. Dispatch reported no interactions between them, which was
correct.

An audit of names the facts layer could not resolve covered about 1,000 names
over six real runs. It found none that hid a meaningful interaction, so binding
is unchanged.

**Not built.**
- **Strict mode** still walks the root's whole tree when attaching.
- **Partial run directories left by older versions** are not cleaned up.
- **The hook budget** assumes the 60 s timeout `dispatch setup` installs.

**Upgrading.** No migration; the schema stays at 24. Run `dispatch stop &&
dispatch start` so the project owner is 0.4.10.
