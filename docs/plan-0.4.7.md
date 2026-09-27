# Plan: Dispatch 0.4.7, finish without a terminal

## Context

In 0.4.6, Work appears by itself (Claude Code sessions in their own worktree,
Dispatch-made workspaces), but closing it still takes typed commands, and
discovered Work stays open until someone gets to it. This release closes the loop
where you allowed it, from the place you already look (`dispatch watch`). It first
fixes a gap in 0.4.6 found while planning.

**Public promise.**
- Work can be finished, accepted or rejected from `dispatch watch`, without typing
  IDs.
- For a project whose checks you approved, discovered Work is verified by itself
  when its runtime removes the worktree.
- Nothing is ever deleted unless you ask (`dispatch clean`).

## Decisions (agreed 2026-09-25)

1. **Codex is deferred.** `dispatch attach -- codex` remains the supported
   isolation path. The runtime seam (`src/runtime.rs`) stays provider-neutral so an
   adapter can be added later. No lifecycle integration ships that cannot be
   real-agent trialled.
2. **Check consent is per project and bound to the exact commands.**
   - Consent to run a project's checks by themselves is stored in Dispatch's
     state, never in the repository: a repository must not be able to grant
     itself host execution.
   - It names the root and the exact effective `checks.verify` commands.
   - Any change to that command set invalidates it until the person approves the
     new commands.
3. **No automatic deletion.**
   - Rejected Dispatch-made workspaces are kept until the person asks.
   - `dispatch clean` is the explicit mechanism. It lists exactly what it would
     remove and removes it only on confirmation.
   - There is no retention timer.
4. **Review stays human.**
   - Consent authorizes running checks, never applying.
   - Auto-apply still needs `--auto-apply` given at attach, which discovered Work
     never has.
   - A session ending still never ends Work.
5. **`watch` owns nothing.** Every action taken there is exactly the command the
   person would type, run as them, under the same locks and authority.

## Verified current state

- **Checks config.** `checks.verify` lives in `dispatch.yml`, written by
  `setup --checks` (`setup::write_check`).
- **Authority to run checks on the host** is per run: `run.environment.unsafe_local`
  is set at creation from `--allow-unsafe-local`. `finish` accepts the flag for
  that call only.
- **Gap (0.4.6).** `coherence::integration::verify_integration` skips the
  merged-tree checks when a local run lacks `unsafe_local`. Runtime-registered Work
  is created with `unsafe_local: false`, and `finish --allow-unsafe-local` does not
  record authority on the run. So discovered Work accepted into a project that
  moved since its S0 gets no merged-tree check: it is gated on the file and symbol
  analysis alone.
- **`watch`** (`serve.rs`: `watch`, `View`) is passive: rows plus a header, no
  input.
- **The review screen** (`presenter/inspection.rs`) and the `Ui` primitives
  (`select`, `draw`, `next`) exist on `ratatui`/`crossterm`.
- **Dispatch-made workspaces** are released only after apply
  (`attach::release_workspace`) and are otherwise kept. The facts are on the run
  (`attachment.managed`).

## Design

**Stage 1: the merged-tree check for discovered Work.**
- A person's `dispatch finish --allow-unsafe-local` records that authority on the
  run (`environment.unsafe_local = true`, with an event), so accept's merged-tree
  checks run as for any other Work.
- Regression test: discovered Work, the world moves in a way only the merged tree
  catches, finish with the flag, then accept. Accept must refuse; it applies
  today.

**Consent (`<state>/projects/<root-key>.json`).**
- Contents: `{version, root, commands: [...], granted_at}`.
- Granted in `dispatch setup` → Checks: "Run these checks by themselves for Work
  in this project". The screen shows the exact commands, with focus on Cancel.
  Revoked there too.
- Valid only while `commands` equals the effective `checks.verify` of the root's
  configuration.
- `status` and `watch` show it for the project: granted for these commands, not
  granted, or no longer valid because the checks changed.
- **With valid consent:**
  - runtime-registered Work is created with local authority;
  - the owner and accept run the merged-tree checks;
  - removal-time verification (below) happens.
- The consent never reaches auto-apply.

**Removal-time verification.**
- With valid consent, once `WorktreeRemove` or `ExitWorktree` removal has kept the
  exact Δ, the checks run in the rebuilt workspace. The result is Ready (or
  checks failed), awaiting review. Without consent, the Work waits as in 0.4.6.
- The hook's durability invariant is unchanged: the Δ is kept before the hook
  returns.
- Verification runs after that, in the owner's next tick, not inside the hook, so
  the hook stays within its budget.
- **Also:** a workspace that vanishes with no hook but whose last-seen Δ was empty
  closes as "no changes" rather than "lost": nothing was lost.

**Interactive `watch` (TTY only).**
- The same rows gain a selection. ↑/↓ or j/k move.
- **Keys:**
  - `f` finish: active attached or discovered Work. It asks before running checks
    when there is no valid consent, which is the same as `--allow-unsafe-local`.
  - `a` accept: the same gate as `dispatch accept`; a refusal is shown verbatim.
  - `r` reject: after a confirmation with focus on Cancel.
  - `d` review: the existing diff and review screen.
  - `q` or Ctrl+C: leave.
- `--json` and non-TTY output stay passive and unchanged.
- It is built on the existing `Ui`; no new framework.

**`dispatch clean`.**
- It lists the Dispatch-made workspaces it may remove: those of rejected or closed
  Work, not yet removed, directly under `<state>/workspaces/`, and recorded at
  creation. For each it shows the path, branch, Work and when it was rejected.
- It removes them only on confirmation (focus on Cancel). Without a TTY it
  requires `--yes`. `--dry-run` lists only.
- It uses the same removal and path checks as the release after apply, and
  records `workspace.released { reason: cleaned }`.
- It never touches workspaces of active, Ready, unreviewed or applied-and-released
  Work, and never a runtime's or the user's workspace.

## Stages (one commit each on `release-0.4.7`)

| # | Stage |
|---|---|
| 0 | This plan |
| 1 | `finish --allow-unsafe-local` records authority; accept's merged-tree checks run for discovered Work; regression test |
| 2 | Project check consent: state file, setup screen, exact-command binding, shown in `status`/`watch` |
| 3 | Removal-time verification with consent (owner tick); vanished-but-empty closes as "no changes" |
| 4 | Interactive `watch`: selection and f/a/r/d/q; PTY journey tests |
| 5 | `dispatch clean`: list, confirm, `--dry-run`, `--yes` |
| 6 | Real-agent trial and fixes |
| 7 | Docs, release notes, version 0.4.7 |

## Real-agent trial (stage 6)

This uses Claude Code and Cursor only, `caffeinate -i`, and hooks in the trial
project's own `.claude/settings.local.json`.
- **Consent granted:** a `claude -p --worktree` session, then `ExitWorktree`
  removal. The Work is verified by itself (Ready, checks passed) and accepted from
  `dispatch watch` with no typed ID. After a teammate commit, accept runs the
  merged-tree checks.
- **Consent invalidated:** `dispatch.yml`'s checks are changed. Removal then keeps
  Δ but does not verify, and `watch` says why.
- **Reject and clean:** `dispatch attach -- cursor-agent ...` from the checkout,
  rejected from `watch`. `dispatch clean --dry-run` lists its workspace, and
  `dispatch clean` removes it only after confirmation.

## Tests

- **Stage 1:** accept refuses discovered Work whose merged tree fails its checks.
- **Consent:**
  - grant, revoke and invalidation on a command change;
  - a repository's `dispatch.yml` alone never grants it;
  - runtime Work created with and without consent;
  - consent never enables auto-apply.
- **Removal:**
  - with consent: verified and Ready after removal; without consent: waits;
  - checks failing: Ready with checks failed;
  - vanished and empty: closes as no changes.
- **Watch (PTY):** move, finish (the consent prompt), accept (success and a shown
  refusal), reject with confirmation, review opens, leaving, and `--json`
  unchanged.
- **Clean:**
  - lists only eligible workspaces; `--dry-run` removes nothing;
  - confirmation defaults to Cancel; non-TTY needs `--yes`;
  - never removes active, Ready or applied Work, or a runtime's workspace;
  - records the release.

## Risks

- **Consent is a new permission.** Mitigated: it is explicit, per project, bound
  to exact commands, stored outside the repository, never implies apply, and is
  tested against self-granting.
- **An interactive `watch` can race the owner.** Actions go through the same locks
  as the typed commands, which already coexist with the owner.
- **PTY tests are the most fragile part of the suite.** Plain-mode variants are
  used where live redraws split text, as in 0.4.6.

## Definition of done

- Discovered Work accepted into a moved project always runs its merged-tree checks
  when authority exists, and the silent skip is gone.
- With consent, a `claude --worktree` session's work is verified by itself when its
  worktree is removed, and can be accepted from `watch` without typing an ID.
- Changing `checks.verify` invalidates consent until it is re-approved.
- `dispatch clean` removes only what it listed and only on confirmation. Nothing is
  deleted otherwise.
- The trial is logged. fmt, clippy and the full suite are green.

## Deferred

- The Codex lifecycle adapter (until a real-agent trial is possible).
- Warnings about overlapping concurrent Work (0.4.8, "Work that notices Work").
- Tracked shared-checkout Work.
- Start at login.
- Process scanning.
- Any automatic apply for discovered Work.
- Time-based retention.

## Progress log

- 2026-09-25: Stage 0. Plan agreed: Codex deferred; consent per project and bound
  to the exact effective `checks.verify` commands; no automatic deletion, with an
  explicit `dispatch clean`.
- Stage 1. The merged-tree check for discovered Work, and a person's commands
  racing the owner.
  - **Reproduced:** discovered Work, finished with `--allow-unsafe-local`, was
    accepted into a project that had moved in a way only its checks catch (an
    added `forbidden.txt`): the merged-tree checks were skipped because the run
    had no recorded authority.
  - **Fix:** a person's `finish --allow-unsafe-local` records the authority on the
    run (`environment.unsafe_local`, event `attach.authorized`). Accept now runs
    the merged-tree checks and refuses, quoting the failing check.
  - **Found while testing:** a person's `finish` could fail at once ("attached
    work has a foreground owner") when the background owner briefly held the run
    lock to check the Work. `accept`/`reject` could likewise fail with "stale
    review" when the owner recorded a verdict in between.
    - `finish` and review now wait up to 5 s for the lock.
    - The command line's `accept`/`reject` name a run, not a revision, so they no
      longer compare revisions. The interactive review, which shows a particular
      result, still does.
  - Tests:
    - `accepting_discovered_work_runs_its_checks_on_the_merged_tree`;
    - `finish_waits_for_the_owner_to_let_go_of_the_run` (the test holds the lock
      for 1.5 s).

    Both fail on the old code.
  - Full suite: 486 passed.
- Stage 2. Project check consent.
  - `src/consent.rs` keeps `<state>/projects/<root-key>.json`: `{version, root,
    commands, granted_at}`, written durably in a 0700 directory. The root is the
    repository's main worktree, so every worktree shares one consent.
  - Consent is `Valid` only while the recorded commands equal the effective
    `checks.verify`. Otherwise it is `Changed { approved, now }`; any change,
    even an addition, needs re-approval.
  - `dispatch setup --checks` with checks chosen offers "Change checks…", "Let
    Work run these checks by themselves…" / "Stop letting Work run these checks
    by themselves" and "Back". Allowing shows the exact commands with focus on
    Cancel.
  - Runtime registration gives Work local authority only with valid consent, and
    records `attach.authorized { by: "project consent" }`.
  - `status` and `watch` project lines add "checks run by themselves" or "check
    consent no longer holds: the checks changed".
  - Tests:
    - 3 unit tests: exact-command binding, including an addition; the repository
      cannot grant; one project only; no checks means no consent;
    - a PTY journey: Enter cancels; Allow writes consent to the state, not the
      project; Stop revokes;
    - `consent_for_the_projects_checks_gives_discovered_work_its_authority`:
      authority and event, never auto-apply, and a checks change voids consent
      for new Work, with `status` saying why.
  - Full suite: 484 passed, 6 failed. The failures were all in `claude_profiles`
    ("Claude CLI version changed": the fake CLI's 5 s version probe timed out
    under machine load). All 10 pass alone.
