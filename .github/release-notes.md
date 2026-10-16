## Dispatch 0.4.15 — Worktrees caught while they exist

An agent's worktree should become Work within about a second of appearing, and the work in it
should outlive the agent removing that worktree. Hermes Agent removes its worktree when a
session ends, uncommitted changes and all, so in 0.4.14 Dispatch never saw that work. 0.4.15
looks for worktrees on a fast loop of its own, keeps a fresh copy of the changes in a worktree
that can vanish, lets a person finish Work from the changes it last saw, and recognizes Hermes's
worktrees, with a hook that keeps their exact changes before Hermes removes them.

- **Discovery on its own loop.** The project owner now lists the project's worktrees about once
  a second, for every recognized agent, instead of once per `coherence.poll_secs` tick (10 s by
  default). It uses the same recognizers and the same registration path as before; only the
  cadence moved. It keeps going while the owner runs a project's checks for Work finished by
  consent, which in 0.4.14 held discovery up. The coherence tick keeps `poll_secs` for verdicts,
  liveness, interactions and auto-apply. There is no new setting.
- **`dispatch start` returns after the first scan.** It waits until the owner has listed the
  worktrees already there, so a worktree made right after `start` returns is Work, not one
  listed as found before the watch began. "Already watching" waits for that scan too.
- **A fresh Δ for worktrees that can vanish.** Work whose agent can remove its own worktree
  (Hermes, today) is followed on the fast loop: after each pass that finds its tree changed, the
  owner rewrites `delta-live.patch`. Measured on one machine, medians of ten passes: an unchanged
  pass took about 15 ms and a changed one 47 ms on Dispatch's own repository (340 tracked files),
  and 12 ms and 39 ms on a one-file fixture. The loop then waits a second; it is a cadence, not a
  guarantee. Other agents' Work is followed on the coherence tick as before.
- **Work whose worktree vanished can be finished.** When the agent removes its worktree without
  a hook, Dispatch keeps the changes it last saw (`delta-last-seen.patch`), as 0.4.14 did. Now a
  person can finish that Work, where 0.4.14 could only close it: `dispatch finish <id>` rebuilds
  it from S0 and those changes and runs your checks as any finish does, and it waits for review
  like any other result. Accept runs its merged checks when the source moved, as usual, and
  `accept --expect` binds the review to that Δ.
  - It is labelled as last seen, with the time, in `status`, `watch`, `finish`, `diff`, `check`
    and the review screen:

    ```text
    removed 2026-10-15 23:27 UTC: only the changes last seen were kept; finish them yourself (dispatch finish <id>) or close it (dispatch close <id> --reason workspace-removed)
    Changes last seen 2026-10-15T23:27:17.198627622Z. Changes made after that time are not included; Dispatch cannot say whether there were any.
    ```

  - A person finishes it, with `dispatch finish` or `f` in `watch`. The project's check consent
    never finishes it; consent still finishes only Work whose exact changes were kept.
  - With no changes kept, it cannot be finished, and `finish` says to close it with
    `dispatch close <id> --reason workspace-removed`.
- **Hermes's worktrees become Work.** In a watched project, a worktree that `hermes chat
  --worktree` makes, `<repo>/.worktrees/hermes-<8 hex>` on branch `hermes/hermes-<same 8 hex>`,
  becomes Work as soon as discovery sees it, with S0 at its merge base. Its session is the
  worktree's name. Hermes locks the worktree with `hermes pid=<pid>`: the Work reads `agent
  running (pid N)` while that process runs and started no later than the worktree was made. Once
  it is gone, the session's end is recorded (`process_gone`), with or without the hook: after
  Hermes removed the worktree, and its lock with it, Dispatch follows the process it recorded
  by its exact identity. A Hermes worktree without that lock is listed as found, not tracked.
- **The Hermes hook keeps the exact Δ.** `dispatch setup` → Runtime integrations now has
  `Install the Hermes hook` (or `Remove the Hermes hook`) beside Claude Code's, and a `Hermes
  hook: installed | not installed` line.
  - Installing appends one `on_session_finalize` shell hook, running `dispatch hook hermes`
    with a 300 s timeout, to `~/.hermes/config.yaml` (or `$HERMES_HOME/config.yaml`). The consent
    screen shows the block and the path, with focus on Cancel. Every other byte of the file is
    kept, a backup is made beside it, and removing takes out exactly that block. A config that
    already has a `hooks:` key is left alone, and setup shows the lines to add by hand.
  - Hermes runs the hook before it removes the worktree. When Hermes is about to remove it, the
    hook keeps the exact Δ, as Claude Code's `WorktreeRemove` hook does. When Hermes keeps it
    (it holds commits on no remote-tracking ref), the hook records only the session's end.
  - Only the CLI's shutdown counts: an `on_session_finalize` with `reason: "shutdown"`, from the
    process that locked that worktree. `/new` and the TUI fire the same event while the worktree
    lives on, so they are ignored, and their Work falls back to the last-seen Δ.
  - Hermes asks you to allow the hook the first time it runs it; a run without a terminal needs
    `hermes --accept-hooks`. Dispatch never writes Hermes's allowlist.

Not in 0.4.15:
- Hermes's TUI and `/new` get no exact capture; their Work has the last-seen Δ.
- The Hermes hook registers no Work it has not seen. In a project that is not watched, or for a
  worktree removed before discovery saw it, nothing is kept.
- The end of a Codex or Cursor Agent session is still not observable, and there are no Codex or
  Cursor hooks.
- Carried from 0.4.14: file-level facts derived from a path named only in comments or
  configuration, which caused three false REFRESH verdicts; `status` with no runs errors; and
  `watch` and the CLI suggest different next steps for REFRESH Work.

There is still no process scanning and no filesystem notification, and nothing is finished,
closed or cleaned up automatically, except Work whose exact changes were kept, which the owner
finishes where you gave the project's check consent, as in 0.4.14.

No migration: the schema stays at 24. JSON and events are additive: `workspace_removed` (`at`,
`exact`, `last_seen_at`) on `status --json` results and on `check --json` when the workspace was
removed, `workspace_removed` on `attach.finished`, `last_seen_at` on `workspace.removed`, and
Discovered Work with agent `hermes`. "Upgrading to 0.4.15" in `docs/release-install.md` has the
details, including what `hermes config set` does to the hook block; restart the owner after
upgrading.

Conformance: only Hermes's own worktree mode was rerun, on 2026-10-16 with this release's
candidate and Hermes Agent 0.18.2, and it passed. It was one trial of three short sessions, without
the Hermes hook, so the last-seen path. For each task, S0 + Δ equalled the agent's tree as the
runner last saw it, the session's start and end were recorded, `dispatch finish` succeeded, and
the verdicts held (B applied, A REFRESH on `validate`'s signature, C CONTINUE). The runner now
keeps its own copy of a Hermes worktree's tree while the agent runs, because Hermes removes the
worktree before the runner can read it; `record.json` names where each task's tree came from
(`agent.tree_source`), and the table notes it. Every other cell keeps its 0.4.14 result, since
nothing it tests changed. The table and evidence are in
[`docs/harness-conformance.md`](docs/harness-conformance.md).

Built with Dispatch: each packet's worker ran in a workspace Dispatch 0.4.14 made, on three
harnesses. Codex built the fast loop (P1), Hermes Agent built finishing from the last-seen Δ (P2),
and Claude Code built Hermes's recognizer and hook (P3), the session-end and runner fix (P4) and
the docs (R). Every packet passed `dispatch check --merged` before review and landed with
`dispatch accept --expect`, with no override. P3 and P1 both changed the discovery module; P3's
accept ran the merged checks on top of P1 and P2, and they passed. Review sent back two things: a
capture lock in P1 that no observed case needed, and two cost figures in R taken from the wrong
column of P1's measurements.

Site impact: discovery runs about once a second instead of every `poll_secs` tick, and `dispatch start` returns only after its first scan; Hermes Agent worktrees (`hermes chat --worktree`) become Work in a watched project (`discovered hermes` origin, `agent running` from the `hermes pid=` lock, owner-log line `discovery: found a Hermes session in <path>`), so "only Claude Code's, Codex's and Cursor Agent's worktrees are recognized" and "Hermes's own worktrees are not recognized" no longer hold; Work whose worktree vanished can now be finished: new `status` line `removed …: only the changes last seen were kept; finish them yourself (dispatch finish <id>) or close it (…)`, a `Changes last seen <time>. …` line on `status`, `watch`, `finish`, `diff` and `check`, and `watch` offers `f finish` and `c close` for it instead of close only; `dispatch setup` → Runtime integrations gains a `Hermes hook:` line and `Install/Remove the Hermes hook`, and a hook that cannot be installed or removed now reads `Nothing changed: <reason>` with no provider-login advice; hidden `dispatch hook hermes`; the empty `watch` view now reads "A session in its own worktree of this project, from any agent Dispatch recognizes, or dispatch attach -- <agent>, appears here."; `status --json` and `check --json` gain `workspace_removed`.
