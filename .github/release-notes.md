## Dispatch 0.4.16 — Nothing runs twice

Nothing Dispatch does should run twice, and nothing it does itself should be slow. In 0.4.15,
`dispatch check --merged` re-tested exactly the tree `finish` had just tested, merged checks
built from nothing in a new scratch copy every time, a native agent waited for a whole suite
on S0 before it started, and an idle project owner re-read every worktree each tick. 0.4.16
keeps a record of passed check runs, runs merged checks in one warm workspace per project,
starts native agents without a suite, and makes the owner and the hooks cheap.

- **A tree that passed is not checked again.** A pass of `checks.verify` is recorded for 24
  hours under `<state>/memo/`, keyed by:
  - the exact tree that was checked: its Git tree id, ignored files left out;
  - the ordered commands;
  - an execution fingerprint: backend and Docker image id, the environment a check gets
    (`PATH`, `HOME`, `TMPDIR`, `LANG`), where each command's program resolves, and the
    Dispatch version.

  `finish`, `check --merged` and accept consult it first. For a tree with a recorded pass
  they run nothing, record `checks.reused` naming the run whose pass and logs they reuse,
  and say so:

  ```text
  Merged checks: passed, reused from 01M57NW03FEE0CRK45KD2NSSM1 at 2026-10-18 14:15:23 UTC; logs: <state>/runs/01M57NW03FEE0CRK45KD2NSSM1/coherence-checks/01M57NW0SDTHTAV7ZCSKNGNXQV
  ```

  `status` lists each reuse (`check: checks: passed, reused from …`), and `status --json`
  gains `checks_reused`. Failures are never recorded, and a reused finish shows 0.0 s for
  its checks, because nothing ran.
  - **When no key is made, the checks run as before:** a plain directory; a Docker image
    Dispatch cannot identify; a source or workspace where Git converts file contents
    (`core.autocrlf`, `core.eol`, or a `text`, `eol`, `crlf`, `filter`, `ident` or
    `working-tree-encoding` attribute on any file), so an LF pass is never reused for a CRLF
    source; and a tree holding an untracked nested repository.
  - **`check --merged` ends** `Nothing was applied and no verdict changed. accept checks again
    only if the source moves; a tree with a recorded pass is not checked again.`
  - **`check --merged` applies accept's rule.** For finished Work whose source has not moved
    since the Work began, and no recorded pass to reuse, it runs nothing and says `Merged
    checks: not run — the source has not moved since the Work began, so the merged result is
    the Work's own tree`.
  - **Attached `finish` checks what would land when the source moved:** the source as it is
    now with the Work's Δ, in the check workspace below, exactly as accept would. It prints
    `finish: checked on the current source with this Δ, as accept checks it`, and accept then
    reuses that pass unless the source moves again. Only if that fails, or Δ no longer
    applies, does it also check Δ on its own start (S0), to show whose change broke it; it
    reports both. When the source has not moved, finish checks once, in the Work's own
    workspace, as before.
- **Merged checks run warm.** One internal Git repository per project, under
  `<state>/check-workspaces/`, replaces the scratch copy. It is never a worktree of your
  repository, so nothing that lists your worktrees sees it.
  - Each run stages the source's current non-ignored files in a kept index, applies Δ there,
    and updates the checkout. Ignored build output, such as `target/`, and the times of
    unchanged files survive between runs, so builds are incremental. Files a check left
    behind are removed, and the checked-out tree is verified against the staged one.
  - `coherence.merged_checks` records the checked `tree_id`, `warm: true` and the load
    average when the checks started. `warm` names the persistent workspace, not a filled
    cache: a project's first run in it builds from nothing.
  - Merged checks hold the project's source lock while they run. A second one waits, saying
    `Merged checks: waiting for the source lock on <path>`, instead of failing. Accept and
    auto-apply hold that lock from their evaluation to the apply, as in 0.4.15, and run their
    merged checks under it, so one never evaluates a source another is writing. An accept
    that finds the lock held now waits for it, saying `Apply: waiting for the source lock on
    <path>`, where 0.4.15 refused at once; a waiting auto-apply says `Auto-apply: waiting for
    the source lock on <path>`.
  - `dispatch clean` lists each check workspace (`<path> · internal check workspace`) and the
    memo (`<state>/memo · recorded passes (N)`), and removes them only after you confirm,
    never on `--dry-run` or when it refuses. It refuses a check workspace whose source lock is
    held.
  - Clippy on Dispatch's own repository, in the check workspace after a one-line edit: 21.8 s
    cold, 4.2 s warm (one pair on one machine, load averages 53.3 and 48.7).
- **Native agents start without a suite.** No check runs before the agent:
  - each check command's program is resolved first, and a missing one still stops the run
    before any model call;
  - after the agent's own checks, S0 is checked only for the commands it failed, so a
    failure is still told apart as already broken or broken by the agent;
  - without `checks.baseline`, `Running baseline checks...` is gone, and `baseline.finished`
    records `programs_resolved` and `checks_deferred` instead of a suite that did not run;
  - only the default is lazy. Commands you list in `checks.baseline` still run on S0 before
    the agent, as in 0.4.15 (`Running baseline checks...`), and serve as its S0 evidence;
  - a missing program says what happened: `` check program not found: no-such-tool-xyz (from
    `no-such-tool-xyz --check`); the agent was not started ``, then `Work stopped: the
    project's checks could not run on its starting point (S0), so the agent was not started`.
    0.4.15 said `baseline checks did not pass; no model escalation` and
    `Work stopped: Some(BaselineInfrastructure)`.

  For Git sources, the source fingerprint now covers tracked and non-ignored untracked files,
  and no longer hashes ignored build output. Strict mode (`coherence.accept: strict`) and
  plain directories keep the whole-tree fingerprint. From start to a stand-in agent, with a
  3.8 GB `target/` and one check: 517 s with 0.4.15, 121 s with 0.4.16 (one sample each, load
  about 39). The snapshot still copies ignored build output; how much of the 121 s that is was
  not measured.
- **The owner and the hooks cost milliseconds.** The owner looks at the project once per
  change of its signal and shares that look with every Work: file hashes are kept while a
  file's size, times, inode and mode are unchanged, and are taken again for a file changed
  within 2 s of the last look. Facts are derived once per Work and Δ, and a Work is followed
  only when its worktree changed. Nothing is persisted. `serve -v` tick lines end with
  `· total N ms`. With 6 Works on a clone of Dispatch's repository, measured in pairs on one
  machine against a build of the same code without these changes, with the same verdicts and
  events:

  | Tick | Before (3 runs) | 0.4.16 (3 runs) | Load average |
  |---|---|---|---|
  | Nothing moved (median of 7 per run) | 400, 414, 424 ms | 59, 64, 36 ms | 4.2 to 11.6 |
  | Source moved, recording 6 verdicts | 2612, 2659, 2945 ms | 334, 157, 137 ms | 4.2 to 11.6 |
  | Source moved again, nothing to record | 2688, 2667, 2748 ms | 112, 91, 86 ms | 4.2 to 11.6 |

  Before is the sum of the old tick line's parts, which leave a little out; 0.4.16 is the
  line's new `total`. A move that records verdicts took 311 to 427 ms in 6 of 15 samples,
  mostly at load averages above 10, with the extra time outside the parts the line measures.

  A hook in a project nobody watches decides that without Git or the database. `dispatch hook
  claude` with a `SessionStart` payload took 215.5 to 222.4 ms at the median before and 4.0 to
  4.4 ms now (5 rounds of 40 runs, load average 4.3 to 5.1), about what starting the binary
  costs (`dispatch --version`: 4.2 ms). At load average 30 the median was 8 to 20 ms.
- **Changed: a Git failure is an error, not a plain directory.** When Dispatch inspects a
  source (`run`, `attach`, `start`, hooks in watched projects), only Git's "not a git
  repository" now means a plain directory. Any other failure, such as a broken config or a
  repository owned by another user, stops with Git's message, where 0.4.15 snapshotted the
  source as a plain directory, and hooks said nothing.
- **Fixed: on Linux, a same-size edit could be captured with its old content** (since
  0.4.10). Dispatch reads a checkout's current files through a copy of its Git index. On
  Linux that copy got a new modification time, so Git trusted cached file data it should
  have checked again: a file rewritten at the same size in the second Git last wrote the
  index kept its old content in what Dispatch captured. An attached or registered Work's S0
  could miss such an edit, which then counted as part of the Work's Δ. The copy now keeps
  the index's time. On macOS the copy already kept it.

Not in 0.4.16:
- A pass is reused only from the same fingerprint. A check run from an agent's shell, which
  may put its own directories first on `PATH` (Codex does), does not match yours; neither do
  0.4.15 and 0.4.16, since the Dispatch version is in the key.
- The memo does not serve a native run's own checks or its S0 checks.
- One check workspace per project, so merged checks for one project run one at a time. There
  is no pool, and no test selection.
- A native run's snapshot still copies and walks ignored build output.
- No filesystem notifications; the owner still looks once per tick.
- Carried from 0.4.15: no Codex or Cursor hooks; file-level facts from paths named only in
  comments or configuration; `status` with no runs errors.

No migration: the schema stays at 24. Events and JSON are additive: `checks.reused`;
`coherence.merged_checks` gains `tree_id`, `warm`, `load_average` and the `by` value `finish`;
`baseline.finished` gains `programs_resolved` and `checks_deferred`; `status --json` gains
`checks_reused`. New state directories are `memo/` and `check-workspaces/`. "Upgrading to
0.4.16" in `docs/release-install.md` has the details; restart the owner after upgrading.

Measurements, counted from recorded events and logs:
- **0.4.15's own check runs, replayed** through a memo of passed runs, keyed by the tree each
  tested: 17 suite runs for 7 landed Works (2.43 per landed Work) would have been 12 (1.71).
  The duplicates were `check --merged` re-testing exactly the tree `finish` had tested.
- **This build's own suite runs:** 11 for 6 landed Works (1.83 per landed Work), and no tree
  was tested twice. Three of the 11 were the finishes of Works replaced after a conflict;
  without them, 8 for 6 (1.33). The build ran on 0.4.15, which has no memo, so nothing was
  reused. Replayed with 0.4.16's finish, which checks what would land, the same runs come to
  9 (1.50).

A reused result is counted as reused, never as time saved.

Built with Dispatch: each packet's worker ran in a workspace Dispatch 0.4.15 made, on three
harnesses. Claude Code built the memo (P1, re-applied on top of P2 as P1b), the owner and hook
costs (P4), the docs (R) and the fixes for two failures CI found on the release branch (P5);
Codex built the warm check workspace (P2); Hermes Agent built native start (P3). Every packet
landed with `dispatch accept --expect`, and no verdict was overridden. P1 conflicted with P2 in
three files and was re-applied on top of it in a fresh workspace; P4's accept was refused once
its patch no longer applied after P1b landed, and its frozen Δ was re-applied with a 3-way merge;
P5 came out REFRESH because R had edited the same function, and its frozen Δ was re-applied on
top of R.

Site impact: `check --merged`, accept and `status` can say `passed, reused from <run> at <time>; logs: …` instead of running checks; `check --merged` on finished Work with an unmoved source prints `Merged checks: not run — the source has not moved since the Work began, so the merged result is the Work's own tree`; attached `finish` on a moved source checks the current source with Δ and prints `finish: checked on the current source with this Δ, as accept checks it` (or that it did not pass and S0 was checked too); `status --json` gains `checks_reused`; native runs without `checks.baseline` no longer print `Running baseline checks...` and run no S0 suite before the agent (an explicit `checks.baseline` still runs first); a missing check program prints `` check program not found: <program> (from `<command>`); the agent was not started `` and `Work stopped: the project's checks could not run on its starting point (S0), so the agent was not started`; `check --merged` ends `accept checks again only if the source moves; a tree with a recorded pass is not checked again`; merged checks run in a per-project check workspace under the state directory instead of a scratch copy, can print `Merged checks: waiting for the source lock on <path>`, and `dispatch clean` lists `<path> · internal check workspace` and `<state>/memo · recorded passes (N)`, with its own confirmation text, and prints `Removed N recorded pass(es).`; accept and auto-apply hold the source lock from their evaluation to the apply, and a waiting one prints `Apply: waiting for the source lock on <path>` (accept no longer refuses at once) or `Auto-apply: waiting for the source lock on <path>`; the check consent text and `dispatch check --merged`'s help now name the check workspace; `serve -v` tick lines end with `· total N ms`; a source whose Git inspection fails for any reason but "not a git repository" is now an error.
