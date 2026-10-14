## Dispatch 0.4.13 — Work that appears and ends by itself

Run `dispatch start` in a project and use Claude Code the way you already do. Before 0.4.13, a
Claude Code worktree session became Work only if Claude Code's hooks were installed, and a
`claude -p` session never said it had ended. Now, with no hooks installed, the session's worktree
becomes Work. The Work says while its agent runs, and says when the agent is gone. Dispatch
records only what it can observe, and it still never finishes Work because a session ended.

- **Discovery without hooks.** On each tick the watching owner lists the project's Git worktrees
  (`git worktree list`). It never walks a directory. A worktree Claude Code made (under
  `.claude/worktrees/`, with Claude's `CLAUDE_BASE` file) that appears after watching began and
  is locked by a `claude session …` becomes Work:
  - S0 is the worktree's merge base with the project, with full confidence: everything in a
    worktree that appeared while you watched is its session's. `watch` shows
    `S0 merge-base c45fd977 (full; found by Dispatch)`, and the origin is `discovered`.
  - This works for `claude --worktree` and `claude -p --worktree`. A `claude -p` run that
    starts and ends between two ticks still becomes Work, with its session shown as ended.
  - With hooks installed too, whichever sees the worktree first creates the Work, and the other
    joins it. There is one Work per worktree.
  - If a hook could not register a session in time and told it so, Dispatch may still track its
    worktree when it finds it later.
- **Found, not tracked.** Claude worktrees that were already there when watching began are not
  turned into Work, because Dispatch cannot know what they held before. Neither are new ones
  without a `claude session` lock, such as subagent or background worktrees, whose lock has not
  been observed yet. They are listed with the command that attaches them:

  ```text
  01M4VJ2H · discovered claude · S0 merge-base c45fd977 (full; found by Dispatch) · unmoved · working · checks not run
  found, not tracked · dispatch attach --workspace ~/src/project/.claude/worktrees/old
  ```

  `watch` lists them under the Work, `status` in a "Found, not tracked" section, and
  `watch --json` in the `inventory` field of the `watcher` record. The list belongs to the
  watch: it survives an owner restarted after a crash, and `dispatch stop` clears it. Worktrees
  you made yourself (`git worktree add`) are never listed.
- **Liveness from Claude's worktree lock.** Claude Code locks a worktree it made with
  `claude session <name> (pid <pid> start <time>)`. While that exact process runs, the Work
  shows `agent running (pid N)`. A pid reused by another process does not count. Dispatch
  records the end of the session once the process is gone and its lock is left, as `claude -p`
  leaves it (`process_gone`), or once the lock is released (`lock_released`). For either, it
  must first have seen the process running. The Work then reads `idle`, with 0.4.12's
  "session ended — finish, resume or close". Nothing is finished, applied, closed or signalled.
  An unreadable process start, or a lock never seen running, proves nothing, and nothing is
  recorded. A hook's end and a lock's end give one end. Only worktrees the runtime made are
  read; a person's `attach`, with or without `--pid`, is never touched.
- **Live progress, with no fake zeros.** Working attached Work shows the files it has changed so
  far, as the owner last counted them, and how long Dispatch has tracked it:

  ```text
  Candidate A (claude)
    Status          running
    Verification    not configured
    Runtime         unknown
    Tracked         for 4s
    Cost            unknown
    Task tokens     unknown
    Token basis     unknown
    Files changed   2 now
  ```

  Until the owner has counted, and whenever no owner watches the project, files read `unknown`.
  Before, `status` printed `Runtime 0.0s`, `Files changed 0` and `Diff +0 / -0` for every
  working attached Work. In `watch` the selected Work's details have a Progress line, such as
  `files: 2 changed · tracked for 12m · agent running (pid 4242)`, or `files: none changed yet`
  for a count of zero. A finished result keeps the numbers recorded when it was delivered.
- **First use without a terminal ceremony.**
  - `dispatch start` in a project without checks, on a terminal, shows the checks Dispatch
    detected, with "Continue without checks" focused, so Enter alone never writes to the
    project:

    ```text
    This project has no checks. Dispatch found:
      sh ./verify.sh
      make test
    Saving them writes checks.verify in dispatch.yml; it does not let Work run them by themselves.
    ```

    "Save these checks" writes them to `dispatch.yml`. Esc starts nothing.
  - Without a terminal (a script, CI, an agent's shell), `start` watches at once, detects and
    runs nothing, and says:
    `No checks are configured, so Work here is not verified. Add them: dispatch setup --checks, or dispatch setup --checks --verify '<command>'`.
  - `dispatch setup --checks --verify 'cargo test' --verify 'make lint'` saves exactly those
    commands, in that order, and keeps the rest of `dispatch.yml`. On a terminal, `--verify`
    pre-fills the same menu.
  - Consent is never granted without a terminal. Letting Work run checks by itself still needs
    `dispatch setup --checks` in a terminal, or `--allow-unsafe-local` on a single `finish`.
    Saving other commands ends consent given for the old ones. The consent screen now says that
    checks run project code, including code an agent wrote, on this host with your permissions,
    not in a sandbox.
- **Watching no longer takes Git's `index.lock`.** The owner runs `git status` in your
  repository on every tick. Git refreshes the index under `index.lock` when it can, so your own
  `git commit` could fail while Dispatch watched. That has been true since 0.4.5. Dispatch's Git
  commands now set `GIT_OPTIONAL_LOCKS=0`. The race showed up while building this release, as a
  test's own `git commit` failing under load.

Not in 0.4.13: other agents' worktree layouts are not recognized; for them,
`dispatch attach -- <agent>` gives the agent a workspace of its own. There is no process
scanning, and nothing is finished, closed or cleaned up automatically. A lock is read only in a
watched project.

No migration: the schema stays at 24. JSON changes are additive: the `discovered` provenance,
`via: "discovery"`, the `runtime.agent_running` and `runtime.agent_gone` events, the
`process_gone` and `lock_released` end reasons, and `watch --json`'s `inventory`. Once 0.4.13
has found Work, 0.4.12 cannot read that state directory. Roll back only from a backup.
"Upgrading to 0.4.13" in `docs/release-install.md` has the details; restart the owner after
upgrading.

Built with Dispatch: four code packets ran at the same time, each an interactive Claude Code
session that a pinned Dispatch 0.4.12 started in its own workspace
(`dispatch attach --root <clone> -- claude`), then a docs packet. A human reviewed and accepted
them one at a time. Checking each packet against what had already landed, by an independent
suite and by the merged checks at accept, caught three integration problems before release.
A real-agent trial with no hooks caught a fourth: a `claude -p` worktree session that
discovery found while it ran, and that ended before Dispatch looked again, kept its session
open; discovery now records the running session's process as the Work's agent, so its end is
seen however soon it comes.
